//! Backseat rendezvous server — matches peers for UDP hole-punching.
//!
//! Three endpoints:
//!
//! ```text
//! POST /host              {"udp":"1.2.3.4:47474"}   → {"code":"KXPQMZ"}
//! GET  /room/KXPQMZ/await                           → {"peer":"5.6.7.8:47474"}  (blocks until viewer joins, max 60 s)
//! POST /room/KXPQMZ/join  {"udp":"5.6.7.8:47474"}   → {"host":"1.2.3.4:47474"}
//! ```
//!
//! No video passes through here — only the two UDP addresses.
//! Rooms expire automatically after 5 minutes if no viewer joins.
//!
//! Configuration:
//!   PORT   — TCP port to listen on (default 3000)

use std::collections::HashMap;
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
use tokio::sync::oneshot;

// ── Room state ────────────────────────────────────────────────────────────────

struct Room {
    host_udp: String,
    /// Triggered by /join to wake up the blocked /await handler.
    peer_tx:  Option<oneshot::Sender<String>>,
    /// Held by the /await handler; taken out of the Mutex before awaiting.
    peer_rx:  Option<oneshot::Receiver<String>>,
    created:  Instant,
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
) -> Json<HostResp> {
    let (tx, rx) = oneshot::channel::<String>();
    let code = {
        let mut rooms = s.rooms.lock().unwrap();
        // Evict stale rooms on every registration (cheap, no background task needed).
        rooms.retain(|_, r| r.created.elapsed() < Duration::from_secs(300));
        let code = loop {
            let c = gen_code();
            if !rooms.contains_key(&c) { break c; }
        };
        rooms.insert(code.clone(), Room {
            host_udp: body.udp,
            peer_tx: Some(tx),
            peer_rx: Some(rx),
            created: Instant::now(),
        });
        code
    };
    tracing::info!("host registered room {code}");
    Json(HostResp { code })
}

/// Host long-polls here — blocks until a viewer joins (or 60 s timeout).
async fn handle_await(
    State(s): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<AwaitResp>, StatusCode> {
    // Take the receiver out of the room so we can await it without holding the lock.
    let rx = {
        let mut rooms = s.rooms.lock().unwrap();
        let room = rooms.get_mut(&code).ok_or(StatusCode::NOT_FOUND)?;
        room.peer_rx.take().ok_or(StatusCode::CONFLICT)? // CONFLICT = already waiting
    };

    match tokio::time::timeout(Duration::from_secs(60), rx).await {
        Ok(Ok(peer_addr)) => {
            s.rooms.lock().unwrap().remove(&code);
            tracing::info!("room {code}: peer joined, both parties notified");
            Ok(Json(AwaitResp { peer: peer_addr }))
        }
        _ => {
            s.rooms.lock().unwrap().remove(&code);
            tracing::info!("room {code}: timed out waiting for viewer");
            Err(StatusCode::REQUEST_TIMEOUT)
        }
    }
}

/// Viewer submits the room code and its own public UDP address.
/// The host's blocked /await call unblocks simultaneously.
async fn handle_join(
    State(s): State<AppState>,
    Path(code): Path<String>,
    Json(body): Json<JoinBody>,
) -> Result<Json<JoinResp>, StatusCode> {
    let (host_udp, tx) = {
        let mut rooms = s.rooms.lock().unwrap();
        let room = rooms.get_mut(&code).ok_or(StatusCode::NOT_FOUND)?;
        let tx = room.peer_tx.take().ok_or(StatusCode::CONFLICT)?;
        (room.host_udp.clone(), tx)
    };
    let _ = tx.send(body.udp); // wakes up the host's /await
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

    let app = Router::new()
        .route("/host",               post(handle_host))
        .route("/room/:code/await",   get(handle_await))
        .route("/room/:code/join",    post(handle_join))
        .route("/health",             get(|| async { "ok" }))
        .with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let addr = format!("0.0.0.0:{port}");
    tracing::info!("backseat-server listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
