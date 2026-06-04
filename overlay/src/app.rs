use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use crate::capture::ScreenCapture;
use crate::cursor::CursorState;
use crate::decoder::Vp8Decoder;
use crate::draw_layer::DrawLayer;
use crate::encoder::Vp8Encoder;
use crate::transport::{Packet, Reassembler, RoomCode, Transport};
use crate::types::{AnnotMsg, NormPoint, UserColor, UserId, UserInfo};

// ── Internal types ────────────────────────────────────────────────────────────

/// Decoded video frame handed from the decode thread to the egui paint loop.
struct RgbaFrame { width: u32, height: u32, data: Vec<u8> }

/// Payload sent from the async host setup task back to the egui thread once ready.
struct HostReady {
    transport:   Arc<Transport>,
    room_code:   String,  // WAN / STUN-discovered address
    local_code:  String,  // LAN IP address (same port)
    annot_rx:    mpsc::UnboundedReceiver<AnnotMsg>,
    cursors:     Arc<Mutex<CursorState>>,
    draws:       Arc<Mutex<DrawLayer>>,
    capture_ok:  Arc<AtomicBool>,
    /// Updated in-place by the signaling task when the room code is refreshed.
    live_code:   Arc<Mutex<String>>,
}

/// Live state held while in host mode.
struct HostCtx {
    room_code:   String,
    local_code:  String,
    _transport:  Arc<Transport>, // keeps the socket alive for the duration of hosting
    annot_rx:    mpsc::UnboundedReceiver<AnnotMsg>,
    cursors:     Arc<Mutex<CursorState>>,
    draws:       Arc<Mutex<DrawLayer>>,
    tray:        crate::tray::HostTray,
    capture_ok:  Arc<AtomicBool>,
    live_code:   Arc<Mutex<String>>,
}

/// Payload sent from the async join setup task back to the egui thread once ready.
struct JoinReady {
    transport: Arc<Transport>,
    rgba_rx:   mpsc::UnboundedReceiver<RgbaFrame>,
    annot_out: mpsc::UnboundedSender<String>,
    viewer_id: Uuid,
}

/// Live state held while in viewer (joined) mode.
struct JoinCtx {
    _transport:    Arc<Transport>, // keeps the socket alive for the duration of the session
    rgba_rx:       mpsc::UnboundedReceiver<RgbaFrame>,
    annot_out:     mpsc::UnboundedSender<String>,
    cursors:       Arc<Mutex<CursorState>>,
    draws:         Arc<Mutex<DrawLayer>>,
    texture:       Option<egui::TextureHandle>,
    viewer_id:     Uuid,
    active_stroke: Option<Uuid>,
}

// ── State machine ─────────────────────────────────────────────────────────────

/// The top-level application state.  Transitions happen inside [`OverlayApp::step`].
enum State {
    /// Initial screen — user picks host or join.
    ChoosingMode,
    /// Host mode starting up: STUN query in progress, waiting for [`HostReady`].
    Discovering  { rx: tokio::sync::oneshot::Receiver<HostReady> },
    /// Host mode active: transparent overlay, tray icon showing room codes.
    Hosting      (HostCtx),
    /// Viewer entering a room code (or waiting for the connection task to finish).
    EnteringCode { input: String, error: Option<String>, connect_rx: Option<tokio::sync::oneshot::Receiver<JoinReady>> },
    /// Viewer connected: displaying video stream with annotation canvas on top.
    Joining      (JoinCtx),
}

// ── App ───────────────────────────────────────────────────────────────────────

/// Root eframe application.  Owns the Tokio runtime and drives the state machine
/// each egui frame via [`eframe::App::update`].
pub struct OverlayApp {
    state: State,
    rt:    tokio::runtime::Runtime,
}

impl OverlayApp {
    pub fn new(_cc: &eframe::CreationContext) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        Self { state: State::ChoosingMode, rt }
    }

    /// Drive one frame of the state machine.  Takes ownership of `state` so each
    /// arm can consume it and return the next state without fighting the borrow checker.
    fn step(&mut self, ctx: &egui::Context, state: State) -> State {
        match state {

            // ── Choose host or join ───────────────────────────────────────────
            State::ChoosingMode => {
                let mut clicked_host = false;
                let mut clicked_join = false;
                egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |_| {});
                egui::Window::new("backseat")
                    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                    .resizable(false).collapsible(false)
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            clicked_host = ui.button("  Host  ").clicked();
                            clicked_join = ui.button("  Join  ").clicked();
                        });
                    });
                if clicked_host { return self.begin_host(); }
                if clicked_join { return State::EnteringCode { input: String::new(), error: None, connect_rx: None }; }
                State::ChoosingMode
            }

            // ── Waiting for STUN ──────────────────────────────────────────────
            State::Discovering { mut rx } => {
                egui::Window::new("backseat")
                    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                    .resizable(false).collapsible(false)
                    .show(ctx, |ui| { ui.label("Discovering public address…"); });
                match rx.try_recv() {
                    Ok(ready) => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
                        let tray = crate::tray::HostTray::new(ready.room_code.clone());
                        State::Hosting(HostCtx {
                            room_code:   ready.room_code,
                            local_code:  ready.local_code,
                            _transport:  ready.transport,
                            annot_rx:    ready.annot_rx,
                            cursors:     ready.cursors,
                            draws:       ready.draws,
                            tray,
                            capture_ok:  ready.capture_ok,
                            live_code:   ready.live_code,
                        })
                    }
                    Err(_) => State::Discovering { rx },
                }
            }

            // ── Hosting — transparent overlay with annotation rendering ────────
            State::Hosting(mut h) => {
                while let Ok(msg) = h.annot_rx.try_recv() {
                    apply_annot(&msg, &h.cursors, &h.draws);
                }

                // Propagate clipboard copy requested via the tray icon menu.
                if h.tray.pop_copy_request() {
                    ctx.output_mut(|o| o.copied_text = h.room_code.clone());
                }

                // Room-code HUD — always visible so the user knows what to enter on the viewer.
                // Mouse passthrough is on, so this is read-only display only.
                egui::Window::new("backseat")
                    .anchor(egui::Align2::RIGHT_TOP, egui::Vec2::new(-12.0, 12.0))
                    .resizable(false)
                    .collapsible(false)
                    .title_bar(false)
                    .show(ctx, |ui| {
                        ui.label(egui::RichText::new("backseat — hosting").strong());
                        if !h.capture_ok.load(Ordering::Relaxed) {
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 120, 60),
                                "⚠ screen capture failing\nRun in physical desktop session\n(not via remote desktop)",
                            );
                        }
                        ui.separator();
                        let current_code = h.live_code.lock().unwrap().clone();
                        let is_short = current_code.len() == 6 && current_code.chars().all(|c| c.is_ascii_uppercase());
                        if is_short {
                            ui.horizontal(|ui| {
                                ui.label("Code:");
                                ui.monospace(&current_code);
                            });
                        } else {
                            let loopback = format!("127.0.0.1:{}", current_code.split(':').last().unwrap_or("?"));
                            ui.horizontal(|ui| { ui.label("Same machine:"); ui.monospace(&loopback); });
                            ui.horizontal(|ui| { ui.label("LAN:"); ui.monospace(&h.local_code); });
                            ui.horizontal(|ui| { ui.label("WAN:"); ui.monospace(&current_code); });
                        }
                    });

                egui::CentralPanel::default()
                    .frame(egui::Frame::none())
                    .show(ctx, |ui| {
                        crate::renderer::paint(ui.painter(), ui.max_rect(), &h.draws, &h.cursors);
                    });

                State::Hosting(h)
            }

            // ── Enter room code ───────────────────────────────────────────────
            State::EnteringCode { mut input, error, mut connect_rx } => {
                // Check if the connection task finished.
                if let Some(ref mut rx) = connect_rx {
                    if let Ok(ready) = rx.try_recv() {
                        return self.finish_join(ctx, ready);
                    }
                }

                let mut go_back     = false;
                let mut go_connect  = false;
                egui::Window::new("backseat")
                    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                    .resizable(false).collapsible(false)
                    .show(ctx, |ui| {
                        ui.label("Room code:");
                        let resp = ui.text_edit_singleline(&mut input);
                        // Auto-connect on Enter key.
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            go_connect = true;
                        }
                        if let Some(ref e) = error {
                            ui.colored_label(egui::Color32::RED, e);
                        }
                        ui.horizontal(|ui| {
                            if connect_rx.is_some() {
                                ui.label("Connecting…");
                            } else {
                                go_connect |= ui.button("Connect").clicked();
                            }
                            go_back = ui.button("Back").clicked();
                        });
                    });

                if go_back { return State::ChoosingMode; }
                if go_connect && connect_rx.is_none() {
                    match Transport::parse_room_code(&input) {
                        Some(code) => {
                            let rx = self.begin_join(code);
                            return State::EnteringCode { input, error: None, connect_rx: Some(rx) };
                        }
                        None => return State::EnteringCode {
                            input,
                            error: Some("Invalid room code (expected 6-letter code or IP:port)".into()),
                            connect_rx: None,
                        },
                    }
                }
                State::EnteringCode { input, error, connect_rx }
            }

            // ── Joined — video + annotation canvas ────────────────────────────
            State::Joining(mut j) => {
                // Drain decoded frames, update the egui texture.
                while let Ok(frame) = j.rgba_rx.try_recv() {
                    let img = egui::ColorImage::from_rgba_unmultiplied(
                        [frame.width as usize, frame.height as usize],
                        &frame.data,
                    );
                    match &mut j.texture {
                        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                        None    => {
                            tracing::info!("first video texture {}x{}", frame.width, frame.height);
                            j.texture = Some(ctx.load_texture("video", img, egui::TextureOptions::LINEAR));
                        }
                    }
                }

                egui::CentralPanel::default()
                    .frame(egui::Frame::none().fill(egui::Color32::BLACK))
                    .show(ctx, |ui| {
                        let rect = ui.max_rect();

                        // Allocate input sensing before borrowing painter.
                        let response = ui.allocate_rect(rect, egui::Sense::drag());
                        let painter  = ui.painter();
                        let to_norm = |p: egui::Pos2| NormPoint {
                            x: ((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0),
                            y: ((p.y - rect.min.y) / rect.height()).clamp(0.0, 1.0),
                        };

                        if let Some(pos) = response.hover_pos() {
                            let norm = to_norm(pos);
                            j.cursors.lock().unwrap().update(UserId(j.viewer_id), norm);
                            let msg = AnnotMsg::CursorMove { viewer_id: j.viewer_id, pos: norm };
                            let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                        }
                        if response.drag_started() {
                            let sid = Uuid::new_v4();
                            j.active_stroke = Some(sid);
                            if let Some(pos) = response.interact_pointer_pos() {
                                let norm = to_norm(pos);
                                j.draws.lock().unwrap().add_point(UserId(j.viewer_id), sid, norm);
                                let msg = AnnotMsg::StrokeBegin { viewer_id: j.viewer_id, stroke_id: sid, pos: norm };
                                let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                            }
                        }
                        if let Some(sid) = j.active_stroke {
                            if response.dragged() {
                                if let Some(pos) = response.interact_pointer_pos() {
                                    let norm = to_norm(pos);
                                    j.draws.lock().unwrap().add_point(UserId(j.viewer_id), sid, norm);
                                    let msg = AnnotMsg::StrokePoint { viewer_id: j.viewer_id, stroke_id: sid, pos: norm };
                                    let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                                }
                            }
                            if response.drag_stopped() {
                                j.draws.lock().unwrap().end_stroke(sid);
                                let msg = AnnotMsg::StrokeEnd { viewer_id: j.viewer_id, stroke_id: sid };
                                let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                                j.active_stroke = None;
                            }
                        }

                        // Draw the video frame before annotations.
                        if let Some(tex) = &j.texture {
                            painter.image(
                                tex.id(),
                                rect,
                                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                egui::Color32::WHITE,
                            );
                        }

                        crate::renderer::paint(painter, rect, &j.draws, &j.cursors);
                    });

                State::Joining(j)
            }
        }
    }

    // ── Background task launchers ─────────────────────────────────────────────

    /// Kick off the host pipeline on the Tokio runtime and return `State::Discovering`.
    ///
    /// Threading model:
    /// - OS thread: `ScreenCapture` + `Vp8Encoder` → `broadcast::Sender<Arc<Vec<u8>>>`
    /// - Tokio task: reads from broadcast, fragments frames to UDP; receives punch/annot packets
    /// - egui thread: drains `annot_rx` each frame, paints annotations
    fn begin_host(&mut self) -> State {
        let (tx, rx) = tokio::sync::oneshot::channel::<HostReady>();
        self.rt.spawn(async move {
            let transport = match Transport::bind().await {
                Ok(t)  => Arc::new(t),
                Err(e) => { tracing::error!("UDP bind: {e}"); return; }
            };

            let public_addr = transport.public_addr().await;
            let local_port  = transport.socket.local_addr().map(|a| a.port()).unwrap_or(0);
            let wan_addr    = public_addr.unwrap_or_else(|| {
                let ip = crate::transport::discover_lan_ip()
                    .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
                SocketAddr::new(ip, local_port)
            });
            let lan_ip = crate::transport::discover_lan_ip()
                .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
            let local_code = Transport::room_code(std::net::SocketAddr::new(lan_ip, local_port));

            // Try to register with the rendezvous server for a short room code.
            // Falls back to the WAN IP:port if the server is unavailable.
            let (room_code, use_signaling) = match server_url() {
                Some(server) => {
                    let body = serde_json::json!({ "udp": wan_addr.to_string() });
                    match reqwest::Client::new()
                        .post(format!("{server}/host"))
                        .json(&body)
                        .send().await
                        .and_then(|r| Ok(r))
                    {
                        Ok(resp) if resp.status().is_success() => {
                            if let Ok(v) = resp.json::<serde_json::Value>().await {
                                if let Some(code) = v["code"].as_str() {
                                    tracing::info!("signaling room code: {code}");
                                    (code.to_string(), true)
                                } else {
                                    (Transport::room_code(wan_addr), false)
                                }
                            } else {
                                (Transport::room_code(wan_addr), false)
                            }
                        }
                        _ => {
                            tracing::warn!("rendezvous server unreachable, falling back to IP:port");
                            (Transport::room_code(wan_addr), false)
                        }
                    }
                }
                None => (Transport::room_code(wan_addr), false),
            };

            tracing::info!("room code (WAN): {room_code}");
            tracing::info!("room code (LAN): {local_code}");

            let cursors     = Arc::new(Mutex::new(CursorState::default()));
            let draws       = Arc::new(Mutex::new(DrawLayer::default()));
            let capture_ok  = Arc::new(AtomicBool::new(true));
            let (annot_tx, annot_rx) = mpsc::unbounded_channel::<AnnotMsg>();
            // _dummy holds the initial broadcast receiver so the channel isn't
            // immediately considered closed before the transport task subscribes.
            let (frame_tx, _dummy)   = broadcast::channel::<Arc<Vec<u8>>>(4);
            // Signaling task → transport task: viewer's UDP address when resolved.
            let (peer_hint_tx, peer_hint_rx) = mpsc::unbounded_channel::<SocketAddr>();

            // Screen capture + VP8 encode thread.
            {
                let tx          = frame_tx.clone();
                let capture_ok  = Arc::clone(&capture_ok);
                std::thread::spawn(move || {
                    let mut cap = match ScreenCapture::new() {
                        Ok(c)  => c,
                        Err(e) => {
                            tracing::error!("screen capture unavailable: {e}");
                            capture_ok.store(false, Ordering::Relaxed);
                            return;
                        }
                    };
                    let mut enc = match Vp8Encoder::new(cap.width as u32, cap.height as u32, 4_000) {
                        Ok(e)  => e,
                        Err(e) => { tracing::warn!("encoder init failed: {e}"); return; }
                    };
                    tracing::info!("capture thread started {}x{}", cap.width, cap.height);
                    let mut n              = 0u64;
                    let mut consec_errors  = 0u32;
                    let mut enc_w          = cap.width;
                    let mut enc_h          = cap.height;
                    loop {
                        let t = std::time::Instant::now();
                        match cap.capture() {
                            Ok(Some(bgra)) => {
                                if consec_errors > 0 {
                                    tracing::info!("capture recovered after {consec_errors} errors");
                                    capture_ok.store(true, Ordering::Relaxed);
                                    consec_errors = 0;
                                }
                                if let Some(encoded) = enc.encode(&bgra, n % 150 == 0) {
                                    if n == 0 { tracing::info!("first encoded frame {} bytes", encoded.len()); }
                                    if n % 150 == 0 { tracing::debug!("encode frame {n} → {} bytes", encoded.len()); }
                                    let _ = tx.send(Arc::new(encoded));
                                } else if n % 150 == 0 {
                                    tracing::warn!("encode returned None at frame {n}");
                                }
                                n += 1;
                            }
                            Ok(None) => {} // WouldBlock — no new frame yet
                            Err(e) => {
                                consec_errors += 1;
                                if consec_errors == 1 {
                                    tracing::warn!("capture error: {e}");
                                }
                                // After ~1 s of consecutive errors, try reinitialising the capturer.
                                // This recovers from display resolution changes (e.g. Chrome Remote
                                // Desktop resizing the screen when a client connects).
                                if consec_errors == 30 {
                                    tracing::info!("reinitialising capturer after {consec_errors} errors");
                                    match ScreenCapture::new() {
                                        Ok(new_cap) => {
                                            let (nw, nh) = (new_cap.width, new_cap.height);
                                            cap = new_cap;
                                            // Rebuild encoder if resolution changed.
                                            if nw != enc_w || nh != enc_h {
                                                tracing::info!("resolution changed {enc_w}x{enc_h} → {nw}x{nh}, rebuilding encoder");
                                                match Vp8Encoder::new(nw as u32, nh as u32, 4_000) {
                                                    Ok(new_enc) => { enc = new_enc; enc_w = nw; enc_h = nh; n = 0; }
                                                    Err(e) => tracing::warn!("encoder rebuild failed: {e}"),
                                                }
                                            }
                                            consec_errors = 0;
                                            capture_ok.store(true, Ordering::Relaxed);
                                        }
                                        Err(e) => {
                                            tracing::warn!("capturer reinit failed: {e}");
                                            capture_ok.store(false, Ordering::Relaxed);
                                            consec_errors = 0; // reset so we retry again in ~1 s
                                        }
                                    }
                                }
                            }
                        }
                        let elapsed = t.elapsed();
                        let target  = Duration::from_millis(33);
                        if elapsed < target { std::thread::sleep(target - elapsed); }
                    }
                });
            }

            // Transport task: send encoded frames to peer; receive annotations.
            {
                let transport    = Arc::clone(&transport);
                let annot_tx     = annot_tx;
                let mut frame_rx = frame_tx.subscribe();
                let mut peer_hint_rx = peer_hint_rx;
                tokio::spawn(async move {
                    let mut peer:     Option<SocketAddr> = None;
                    let mut frame_id: u32                = 0;
                    let mut buf                          = vec![0u8; 65_536];
                    loop {
                        tokio::select! {
                            // Signaling server resolved the viewer's address — punch proactively.
                            hint = peer_hint_rx.recv() => {
                                if let Some(addr) = hint {
                                    tracing::info!("signaling: viewer at {addr}, punching");
                                    peer = Some(addr);
                                    for _ in 0..10 {
                                        let _ = transport.send_punch(addr).await;
                                        tokio::time::sleep(Duration::from_millis(50)).await;
                                    }
                                }
                            }
                            res = transport.recv(&mut buf) => {
                                if let Some((src, pkt)) = res {
                                    match pkt {
                                        Packet::Punch => {
                                            if peer != Some(src) {
                                                tracing::info!("viewer connected from {src}");
                                            }
                                            peer = Some(src);
                                            let _ = transport.send_punch(src).await;
                                        }
                                        Packet::Annot(json) => {
                                            if let Ok(msg) = serde_json::from_str::<AnnotMsg>(&json) {
                                                let _ = annot_tx.send(msg);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            res = frame_rx.recv() => {
                                match res {
                                    Ok(frame) => {
                                        if let Some(addr) = peer {
                                            let kf = frame_id % 150 == 0;
                                            if frame_id == 0 { tracing::info!("sending first video frame to {addr} ({} bytes)", frame.len()); }
                                            if frame_id % 150 == 0 { tracing::debug!("host tx frame {frame_id} → {addr}"); }
                                            let _ = transport.send_video(addr, frame_id, &frame, kf).await;
                                            frame_id = frame_id.wrapping_add(1);
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                                    Err(broadcast::error::RecvError::Closed)    => break,
                                }
                            }
                        }
                    }
                });
            }


            let live_code = Arc::new(Mutex::new(room_code.clone()));
            let _ = tx.send(HostReady { transport, room_code, local_code, annot_rx, cursors, draws, capture_ok, live_code: Arc::clone(&live_code) });

            // Keep re-registering with the signaling server after each timeout so the
            // host always has a valid room code without requiring a restart.
            if use_signaling {
                tokio::spawn(async move {
                    loop {
                        // Re-register to get a fresh code (previous room expired).
                        if let Some(server) = server_url() {
                            let body = serde_json::json!({ "udp": wan_addr.to_string() });
                            match reqwest::Client::new()
                                .post(format!("{server}/host"))
                                .json(&body)
                                .send().await
                            {
                                Ok(resp) if resp.status().is_success() => {
                                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                                        if let Some(code) = v["code"].as_str() {
                                            tracing::info!("signaling new room code: {code}");
                                            *live_code.lock().unwrap() = code.to_string();

                                            // Wait for a viewer on this new code.
                                            match reqwest::Client::new()
                                                .get(format!("{server}/room/{code}/await"))
                                                .timeout(Duration::from_secs(305))
                                                .send().await
                                            {
                                                Ok(resp) if resp.status().is_success() => {
                                                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                                                        if let Some(addr_str) = v["peer"].as_str() {
                                                            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                                                                let _ = peer_hint_tx.send(addr);
                                                                return; // connected, done
                                                            }
                                                        }
                                                    }
                                                }
                                                _ => {} // timeout or error — loop and re-register
                                            }
                                        }
                                    }
                                }
                                _ => { tokio::time::sleep(Duration::from_secs(5)).await; }
                            }
                        }
                    }
                });
            }
        });
        State::Discovering { rx }
    }

    /// Kick off the viewer pipeline and return a receiver that resolves to `State::Joining`.
    ///
    /// Threading model:
    /// - Tokio task: receives `PKT_VIDEO` fragments → `Reassembler` → `sync_channel(1)`;
    ///   sends `PKT_PUNCH` every 500 ms to keep the NAT hole open
    /// - OS thread: `Vp8Decoder` reads from `sync_channel`, sends RGBA frames via mpsc
    /// - egui thread: drains `rgba_rx` each frame, uploads to GPU texture
    ///
    /// `sync_channel(1)` is intentional: if the decoder falls behind, newer frames
    /// overwrite the pending one so the viewer always shows the latest image.
    fn begin_join(&mut self, code: RoomCode) -> tokio::sync::oneshot::Receiver<JoinReady> {
        let (tx, rx) = tokio::sync::oneshot::channel::<JoinReady>();
        self.rt.spawn(async move {
            let transport = match Transport::bind().await {
                Ok(t)  => Arc::new(t),
                Err(e) => { tracing::error!("UDP bind: {e}"); return; }
            };

            // Resolve the host address — either directly from the room code or via signaling.
            let host_addr = match code {
                RoomCode::Direct(addr) => addr,
                RoomCode::Signaling(short_code) => {
                    // STUN to learn our own public UDP address so the host can punch us back.
                    let my_udp = transport.public_addr().await.unwrap_or_else(|| {
                        let ip = crate::transport::discover_lan_ip()
                            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
                        SocketAddr::new(ip, transport.socket.local_addr()
                            .map(|a| a.port()).unwrap_or(0))
                    });
                    let body = serde_json::json!({ "udp": my_udp.to_string() });
                    let server = match server_url() {
                        Some(s) => s,
                        None => { tracing::error!("BACKSEAT_SERVER not set"); return; }
                    };
                    match reqwest::Client::new()
                        .post(format!("{server}/room/{short_code}/join"))
                        .json(&body)
                        .send().await
                    {
                        Ok(resp) if resp.status().is_success() => {
                            if let Ok(v) = resp.json::<serde_json::Value>().await {
                                match v["host"].as_str().and_then(|s| s.parse::<SocketAddr>().ok()) {
                                    Some(addr) => {
                                        tracing::info!("signaling: host is at {addr}");
                                        addr
                                    }
                                    None => { tracing::error!("bad host addr from server"); return; }
                                }
                            } else { tracing::error!("bad json from server"); return; }
                        }
                        Ok(resp) => {
                            tracing::error!("server returned {}", resp.status()); return;
                        }
                        Err(e) => { tracing::error!("signaling request failed: {e}"); return; }
                    }
                }
            };

            tracing::info!("viewer bound on {:?}, punching {host_addr}",
                transport.socket.local_addr());

            // Punch through NAT — send several packets to open the hole.
            for i in 0..5 {
                match transport.send_punch(host_addr).await {
                    Ok(()) => tracing::debug!("punch {i} → {host_addr} ok"),
                    Err(e) => tracing::warn!("punch {i} → {host_addr} failed: {e}"),
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            let (frame_sync_tx, frame_sync_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
            let (rgba_tx, rgba_rx)             = mpsc::unbounded_channel::<RgbaFrame>();
            let (annot_out_tx, mut annot_out_rx) = mpsc::unbounded_channel::<String>();
            let viewer_id = Uuid::new_v4();

            // VP8 decode thread → RGBA.
            std::thread::spawn(move || {
                let mut dec = match Vp8Decoder::new() {
                    Ok(d)  => d,
                    Err(e) => { tracing::error!("decoder init: {e}"); return; }
                };
                tracing::info!("decode thread started");
                let mut decoded_count = 0u64;
                while let Ok(data) = frame_sync_rx.recv() {
                    tracing::debug!("decode thread got {} bytes", data.len());
                    if let Some((w, h, pixels)) = dec.decode(&data) {
                        if decoded_count == 0 { tracing::info!("first decoded frame {w}x{h}"); }
                        decoded_count += 1;
                        let _ = rgba_tx.send(RgbaFrame { width: w, height: h, data: pixels });
                    } else {
                        tracing::warn!("decode returned None for {} bytes", data.len());
                    }
                }
                tracing::warn!("decode thread exiting");
            });

            // Transport task: recv video fragments + send annotations.
            {
                let transport = Arc::clone(&transport);
                tokio::spawn(async move {
                    let mut reassembler = Reassembler::new();
                    let mut buf = vec![0u8; 65_536];
                    // Track the actual address the host's packets arrive from — may differ from
                    // host_addr (the room code) when both peers are on the same machine or behind
                    // NAT that doesn't do hairpinning.
                    let mut actual_host: Option<std::net::SocketAddr> = None;
                    let mut punch_ticker = tokio::time::interval(Duration::from_millis(500));
                    punch_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                    loop {
                        tokio::select! {
                            _ = punch_ticker.tick() => {
                                match transport.send_punch(host_addr).await {
                                    Ok(()) => tracing::debug!("periodic punch → {host_addr}"),
                                    Err(e) => tracing::warn!("periodic punch failed: {e}"),
                                }
                            }
                            res = transport.recv(&mut buf) => {
                                if let Some((src, pkt)) = res {
                                    match pkt {
                                        Packet::Punch => {
                                            // Host sent a punch-back; learn its real address.
                                            if actual_host != Some(src) {
                                                tracing::info!("host confirmed at {src}");
                                                actual_host = Some(src);
                                            }
                                        }
                                        Packet::VideoFrag { frame_id, frag_idx, frag_total, keyframe, data } => {
                                            // Accept from the confirmed peer, or — before we have
                                            // one — from any source sharing the room-code port
                                            // (handles same-machine / no-hairpin-NAT cases).
                                            let accepted = match actual_host {
                                                Some(h) => src == h,
                                                None    => src == host_addr
                                                            || src.port() == host_addr.port(),
                                            };
                                            if !accepted {
                                                tracing::warn!("dropping frag from {src} (expected {host_addr} or port {})", host_addr.port());
                                                continue;
                                            }
                                            if actual_host.is_none() {
                                                tracing::info!("host video confirmed at {src}");
                                                actual_host = Some(src);
                                            }
                                            tracing::debug!("rx frag frame={frame_id} {frag_idx}/{frag_total} kf={keyframe}");
                                            if let Some((frame, _)) = reassembler.push(frame_id, frag_idx, frag_total, keyframe, data) {
                                                tracing::debug!("reassembled frame {frame_id} ({} bytes)", frame.len());
                                                let _ = frame_sync_tx.try_send(frame);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            Some(json) = annot_out_rx.recv() => {
                                let target = actual_host.unwrap_or(host_addr);
                                let _ = transport.send_annot(target, &json).await;
                            }
                        }
                    }
                });
            }

            let _ = tx.send(JoinReady { transport, rgba_rx, annot_out: annot_out_tx, viewer_id });
        });
        rx
    }

    /// Called once `JoinReady` arrives.  Restores a normal windowed viewport
    /// (the host runs fullscreen + passthrough; the viewer needs a regular interactive window).
    fn finish_join(&self, ctx: &egui::Context, ready: JoinReady) -> State {
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));

        let cursors = Arc::new(Mutex::new(CursorState::default()));
        let draws   = Arc::new(Mutex::new(DrawLayer::default()));
        cursors.lock().unwrap().add_user(UserInfo {
            id:    UserId(ready.viewer_id),
            name:  "you".into(),
            color: UserColor("#5c9ee0".into()),
        });

        State::Joining(JoinCtx {
            _transport:    ready.transport,
            rgba_rx:       ready.rgba_rx,
            annot_out:     ready.annot_out,
            cursors,
            draws,
            texture:       None,
            viewer_id:     ready.viewer_id,
            active_stroke: None,
        })
    }
}

// ── Annotation application (host side) ───────────────────────────────────────

/// Apply one incoming annotation message to the shared host state.
/// Auto-registers unknown viewers on first `CursorMove` using a deterministic colour
/// derived from the viewer UUID so the same viewer always gets the same colour.
fn apply_annot(msg: &AnnotMsg, cursors: &Arc<Mutex<CursorState>>, draws: &Arc<Mutex<DrawLayer>>) {
    match msg {
        AnnotMsg::CursorMove { viewer_id, pos } => {
            let mut c = cursors.lock().unwrap();
            if !c.users.contains_key(viewer_id) {
                let palette = ["#e05c5c","#5c9ee0","#5ce07a","#e0c25c","#b05ce0","#5ce0d4"];
                let color   = palette[(viewer_id.as_u128() % palette.len() as u128) as usize];
                c.add_user(UserInfo {
                    id:    UserId(*viewer_id),
                    name:  format!("viewer-{}", &viewer_id.to_string()[..4]),
                    color: UserColor(color.into()),
                });
            }
            c.update(UserId(*viewer_id), *pos);
        }
        AnnotMsg::StrokeBegin { viewer_id, stroke_id, pos } |
        AnnotMsg::StrokePoint { viewer_id, stroke_id, pos } => {
            draws.lock().unwrap().add_point(UserId(*viewer_id), *stroke_id, *pos);
        }
        AnnotMsg::StrokeEnd { stroke_id, .. } => {
            draws.lock().unwrap().end_stroke(*stroke_id);
        }
        AnnotMsg::ClearAll => {
            draws.lock().unwrap().clear();
            cursors.lock().unwrap().clear_positions();
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Base URL of the rendezvous server.  Override at runtime with `BACKSEAT_SERVER`
/// (useful for self-hosting or local dev).  Update this constant before shipping
/// a release build once the server is deployed.
const SERVER_URL: &str = "https://backseat.fly.dev";

fn server_url() -> Option<String> {
    let url = std::env::var("BACKSEAT_SERVER").unwrap_or_else(|_| SERVER_URL.to_string());
    if url.is_empty() { None } else { Some(url) }
}

// ── eframe integration ────────────────────────────────────────────────────────

impl eframe::App for OverlayApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let state = std::mem::replace(&mut self.state, State::ChoosingMode);
        self.state = self.step(ctx, state);
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}
