//! Backseat rendezvous server — matches peers for UDP hole-punching.
//!
//! Three endpoints:
//!
//! ```text
//! POST /host              {"udp":"1.2.3.4:47474"}   → {"code":"KXPQMZ"}
//! GET  /room/KXPQMZ/await                           → {"peer":"5.6.7.8:47474"}  (blocks until viewer joins, max 300 s)
//! POST /room/KXPQMZ/join  {"udp":"5.6.7.8:47474"}   → {"host":"1.2.3.4:47474"}
//! ```
//!
//! Multiple viewers may join the same room.  Each `/join` enqueues the viewer's
//! address; each `/await` poll dequeues one entry (or blocks until one arrives).
//! The host loops on `/await` indefinitely with the same code; on 408 it retries,
//! on 404 it re-registers (room truly expired).
//!
//! Rooms expire after 10 minutes of host inactivity (no `/await` calls).
//!
//! Configuration:
//!   PORT   — TCP port to listen on (default 3000)

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};

// Maximum number of pending viewer addresses queued per room.
const MAX_PENDING: usize = 50;

// ── Room state ────────────────────────────────────────────────────────────────

struct Room {
    host_udp:   String,
    /// Viewers that have joined but whose address hasn't been delivered yet.
    pending:    Arc<Mutex<VecDeque<String>>>,
    /// Wakes up a blocked `/await` handler whenever a viewer is enqueued.
    notify:     Arc<Notify>,
    /// Updated each time `/await` is called; used to detect abandoned rooms.
    last_await: Arc<Mutex<Instant>>,
}

impl Room {
    fn new(host_udp: String) -> Self {
        Self {
            host_udp,
            pending:    Arc::new(Mutex::new(VecDeque::new())),
            notify:     Arc::new(Notify::new()),
            last_await: Arc::new(Mutex::new(Instant::now())),
        }
    }
}

#[derive(Clone)]
struct AppState {
    rooms: Arc<Mutex<HashMap<String, Room>>>,
}

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Deserialize)] struct HostBody  { udp: String }
#[derive(Serialize)]   struct HostResp  { code: String }

#[derive(Deserialize)] struct JoinBody  { udp: String }
#[derive(Serialize)]   struct JoinResp  { host: String }

#[derive(Serialize)]   struct AwaitResp { peer: String }

// ── Handlers ──────────────────────────────────────────────────────────────────

/// Host registers its public UDP address and receives a short room code.
async fn handle_host(
    State(s): State<AppState>,
    Json(body): Json<HostBody>,
) -> Result<Json<HostResp>, StatusCode> {
    let host_addr: SocketAddr = body.udp.parse().map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    let code = {
        let mut rooms = s.rooms.lock().unwrap_or_else(|p| p.into_inner());
        // Evict rooms whose host has stopped polling (no /await in 10 minutes).
        rooms.retain(|_, r| {
            r.last_await.lock().unwrap_or_else(|p| p.into_inner()).elapsed() < Duration::from_secs(600)
        });
        let code = loop {
            let c = gen_code();
            if !rooms.contains_key(&c) { break c; }
        };
        rooms.insert(code.clone(), Room::new(host_addr.to_string()));
        code
    };
    tracing::info!("host registered room {code} addr={host_addr}");
    Ok(Json(HostResp { code }))
}

/// Host long-polls here — blocks until a viewer joins (or 300 s timeout).
/// Returns 408 when no viewer arrived; the host should retry with the same code.
/// Returns 404 only if the room has been evicted (host was idle > 10 min).
/// The host calls this in a loop to learn about successive viewers.
async fn handle_await(
    State(s): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<AwaitResp>, StatusCode> {
    // Clone the Arcs so we can release the rooms lock before awaiting.
    let (pending, notify) = {
        let rooms = s.rooms.lock().unwrap_or_else(|p| p.into_inner());
        let room = rooms.get(&code).ok_or(StatusCode::NOT_FOUND)?;
        // Refresh liveness so the room isn't evicted while the host is healthy.
        *room.last_await.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
        (Arc::clone(&room.pending), Arc::clone(&room.notify))
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        if let Some(peer) = pending.lock().unwrap_or_else(|p| p.into_inner()).pop_front() {
            tracing::info!("room {code}: delivering peer {peer}");
            return Ok(Json(AwaitResp { peer }));
        }
        tokio::select! {
            _ = notify.notified() => {}  // viewer joined; re-check the queue
            _ = tokio::time::sleep_until(deadline) => {
                // No viewer in this window — tell the host to retry (same code).
                tracing::debug!("room {code}: await timeout, host should retry");
                return Err(StatusCode::REQUEST_TIMEOUT);
            }
        }
    }
}

/// Viewer submits the room code and its own public UDP address.
/// Enqueues the viewer's address and wakes up the host's blocked `/await`.
async fn handle_join(
    State(s): State<AppState>,
    Path(code): Path<String>,
    Json(body): Json<JoinBody>,
) -> Result<Json<JoinResp>, StatusCode> {
    let viewer_addr: SocketAddr = body.udp.parse().map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    let (host_udp, pending, notify) = {
        let rooms = s.rooms.lock().unwrap_or_else(|p| p.into_inner());
        let room = rooms.get(&code).ok_or(StatusCode::NOT_FOUND)?;
        (room.host_udp.clone(), Arc::clone(&room.pending), Arc::clone(&room.notify))
    };
    {
        let mut q = pending.lock().unwrap_or_else(|p| p.into_inner());
        if q.len() >= MAX_PENDING {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        q.push_back(viewer_addr.to_string());
    }
    tracing::info!("room {code}: viewer joined from {viewer_addr}");
    notify.notify_one();
    Ok(Json(JoinResp { host: host_udp }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn gen_code() -> String {
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(rand::distributions::Uniform::new_inclusive(b'A', b'Z'))
        .take(6)
        .map(|b| b as char)
        .collect()
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = AppState {
        rooms: Arc::new(Mutex::new(HashMap::new())),
    };

    // 10 req/s sustained, burst of 30 — transparent to legitimate clients,
    // blocks flood attacks. SmartIpKeyExtractor reads X-Forwarded-For first
    // (set by Fly's proxy) then falls back to ConnectInfo.
    let governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(10)
            .burst_size(30)
            .finish()
            .unwrap(),
    );

    let app = Router::new()
        .route("/host",               post(handle_host))
        .route("/room/:code/await",   get(handle_await))
        .route("/room/:code/join",    post(handle_join))
        .route("/health",             get(|| async { "ok" }))
        .with_state(state)
        .layer(GovernorLayer { config: governor_conf });

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let addr = format!("0.0.0.0:{port}");
    tracing::info!("backseat-server listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    // into_make_service_with_connect_info exposes the peer IP to GovernorLayer.
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
}
