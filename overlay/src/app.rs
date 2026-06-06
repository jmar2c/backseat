use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

// ── Tool state ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum PenType { Pen, Marker }

/// The viewer's current annotation tool selection.
struct ToolState {
    pen_type:  PenType,
    color_idx: usize,
    size:      f32,
    eraser:    bool,
}

/// Hex colours shown in the toolbar palette, paired with a tooltip label.
const MAX_PEERS: usize = 50;

const PALETTE: &[(&str, &str)] = &[
    ("#e05c5c", "Red"),
    ("#e0903c", "Orange"),
    ("#e0c25c", "Yellow"),
    ("#5ce07a", "Green"),
    ("#5c9ee0", "Blue"),
    ("#b05ce0", "Purple"),
    ("#5ce0d4", "Teal"),
    ("#ffffff", "White"),
];

impl Default for ToolState {
    fn default() -> Self {
        Self { pen_type: PenType::Pen, color_idx: 4, size: 3.0, eraser: false }
    }
}

impl ToolState {
    fn stroke_color(&self) -> &str { PALETTE[self.color_idx].0 }
    fn stroke_width(&self) -> f32 {
        match self.pen_type {
            PenType::Pen    => self.size,
            PenType::Marker => self.size * 4.0,
        }
    }
    fn stroke_alpha(&self) -> u8 {
        match self.pen_type {
            PenType::Pen    => 255,
            PenType::Marker => 110,
        }
    }
}

// ── Internal types ────────────────────────────────────────────────────────────

/// Decoded video frame handed from the decode thread to the egui paint loop.
struct RgbaFrame { width: u32, height: u32, data: Vec<u8> }

/// Per-peer state tracked in the host's transport task.
struct PeerInfo {
    last_seen: Instant,
    viewer_id: Uuid, // assigned by host at first contact; never taken from client-supplied JSON
}

/// Payload sent from the async host setup task back to the egui thread once ready.
struct HostReady {
    transport:     Arc<Transport>,
    room_code:     String,  // WAN / STUN-discovered address
    local_code:    String,  // LAN IP address (same port)
    annot_rx:      mpsc::UnboundedReceiver<(Uuid, AnnotMsg)>,
    disconnect_rx: mpsc::UnboundedReceiver<Uuid>,
    cursors:       Arc<Mutex<CursorState>>,
    draws:         Arc<Mutex<DrawLayer>>,
    capture_ok:    Arc<AtomicBool>,
    /// Updated in-place by the signaling task when the room code is refreshed.
    live_code:     Arc<Mutex<String>>,
}

/// Live state held while in host mode.
struct HostCtx {
    room_code:     String,
    local_code:    String,
    _transport:    Arc<Transport>, // keeps the socket alive for the duration of hosting
    annot_rx:      mpsc::UnboundedReceiver<(Uuid, AnnotMsg)>,
    disconnect_rx: mpsc::UnboundedReceiver<Uuid>,
    cursors:       Arc<Mutex<CursorState>>,
    draws:         Arc<Mutex<DrawLayer>>,
    tray:          crate::tray::HostTray,
    capture_ok:    Arc<AtomicBool>,
    live_code:     Arc<Mutex<String>>,
}

/// Payload sent from the async join setup task back to the egui thread once ready.
struct JoinReady {
    transport:   Arc<Transport>,
    rgba_rx:     mpsc::UnboundedReceiver<RgbaFrame>,
    annot_out:   mpsc::UnboundedSender<String>,
    viewer_id:   Uuid,
    host_addr:   SocketAddr,
    nat_warning: Option<String>,
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
    host_addr:     SocketAddr,
    active_stroke: Option<Uuid>,
    nat_warning:   Option<String>,
    tool:          ToolState,
    name:          String,
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
    /// Viewer entering their name and a room code (or waiting for the connection task to finish).
    EnteringCode { name: String, input: String, error: Option<String>, connect_rx: Option<tokio::sync::oneshot::Receiver<JoinReady>> },
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
    pub fn new(cc: &eframe::CreationContext) -> Self {
        #[cfg(target_os = "linux")]
        x11_set_notification_type(cc);

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
                if clicked_join { return State::EnteringCode { name: String::new(), input: String::new(), error: None, connect_rx: None }; }
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
                        // On Linux, _NET_WM_STATE_FULLSCREEN causes GNOME/Mutter to lower
                        // the window when it loses focus. Exit fullscreen and manually cover
                        // the screen so _NET_WM_STATE_ABOVE keeps us on top while
                        // mouse clicks pass through.
                        #[cfg(target_os = "linux")]
                        {
                            let sz = ctx.screen_rect().size();
                            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(0.0, 0.0)));
                            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(sz));
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
                        let tray = crate::tray::HostTray::new(ready.room_code.clone());
                        State::Hosting(HostCtx {
                            room_code:     ready.room_code,
                            local_code:    ready.local_code,
                            _transport:    ready.transport,
                            annot_rx:      ready.annot_rx,
                            disconnect_rx: ready.disconnect_rx,
                            cursors:       ready.cursors,
                            draws:         ready.draws,
                            tray,
                            capture_ok:    ready.capture_ok,
                            live_code:     ready.live_code,
                        })
                    }
                    Err(_) => State::Discovering { rx },
                }
            }

            // ── Hosting — transparent overlay with annotation rendering ────────
            State::Hosting(mut h) => {
                while let Ok((src_id, msg)) = h.annot_rx.try_recv() {
                    apply_annot(src_id, &msg, &h.cursors, &h.draws);
                }

                // Propagate clipboard copy requested via the tray icon menu.
                if h.tray.pop_copy_request() {
                    ctx.output_mut(|o| o.copied_text = h.room_code.clone());
                }

                if h.tray.pop_exit_request() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }

                while let Ok(id) = h.disconnect_rx.try_recv() {
                    h.cursors.lock().unwrap().remove_user(&UserId(id));
                    h.draws.lock().unwrap().remove_user_strokes(id);
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

            // ── Enter name + room code ────────────────────────────────────────
            State::EnteringCode { mut name, mut input, error, mut connect_rx } => {
                // Check if the connection task finished.
                if let Some(ref mut rx) = connect_rx {
                    if let Ok(ready) = rx.try_recv() {
                        return self.finish_join(ctx, ready, name);
                    }
                }

                let mut go_back     = false;
                let mut go_connect  = false;
                egui::Window::new("backseat")
                    .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                    .resizable(false).collapsible(false)
                    .show(ctx, |ui| {
                        ui.label("Your name:");
                        ui.text_edit_singleline(&mut name);
                        ui.add_space(4.0);
                        ui.label("Room code:");
                        let resp = ui.text_edit_singleline(&mut input);
                        // Auto-connect on Enter key.
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            go_connect = true;
                        }
                        if let Some(ref e) = error {
                            ui.colored_label(egui::Color32::RED, e);
                        }
                        ui.add_space(4.0);
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
                            return State::EnteringCode { name, input, error: None, connect_rx: Some(rx) };
                        }
                        None => return State::EnteringCode {
                            name,
                            input,
                            error: Some("Invalid room code (expected 6-letter code or IP:port)".into()),
                            connect_rx: None,
                        },
                    }
                }
                State::EnteringCode { name, input, error, connect_rx }
            }

            // ── Joined — video + annotation canvas ────────────────────────────
            State::Joining(mut j) => {
                // Notify the host when the viewer closes cleanly.
                if ctx.input(|i| i.viewport().close_requested()) {
                    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
                        let _ = sock.send_to(&[0x04u8], j.host_addr);
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }

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

                if j.texture.is_none() {
                    if let Some(ref w) = j.nat_warning {
                        egui::Window::new("⚠ Connection warning")
                            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                            .resizable(false).collapsible(false)
                            .show(ctx, |ui| { ui.label(w); });
                    }
                }

                // ── Toolbar ───────────────────────────────────────────────────
                egui::Window::new("toolbar")
                    .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -12.0))
                    .resizable(false).collapsible(false).title_bar(false)
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&j.name).strong());
                            ui.separator();

                            // Pen type
                            if ui.selectable_label(j.tool.pen_type == PenType::Pen && !j.tool.eraser, "✏ Pen").clicked() {
                                j.tool.pen_type = PenType::Pen;
                                j.tool.eraser   = false;
                            }
                            if ui.selectable_label(j.tool.pen_type == PenType::Marker && !j.tool.eraser, "🖍 Marker").clicked() {
                                j.tool.pen_type = PenType::Marker;
                                j.tool.eraser   = false;
                            }
                            ui.separator();

                            // Color palette
                            for (i, &(hex, label)) in PALETTE.iter().enumerate() {
                                let color    = crate::renderer::hex_to_color32(hex);
                                let selected = j.tool.color_idx == i && !j.tool.eraser;
                                let stroke   = if selected {
                                    egui::Stroke::new(2.0, egui::Color32::WHITE)
                                } else {
                                    egui::Stroke::NONE
                                };
                                let btn = egui::Button::new("  ")
                                    .fill(color)
                                    .stroke(stroke)
                                    .min_size(egui::vec2(22.0, 22.0));
                                if ui.add(btn).on_hover_text(label).clicked() {
                                    j.tool.color_idx = i;
                                    j.tool.eraser    = false;
                                }
                            }
                            ui.separator();

                            // Brush size
                            for &(label, sz) in &[("S", 2.0f32), ("M", 4.0), ("L", 8.0)] {
                                if ui.selectable_label(j.tool.size == sz && !j.tool.eraser, label).clicked() {
                                    j.tool.size   = sz;
                                    j.tool.eraser = false;
                                }
                            }
                            ui.separator();

                            // Eraser
                            if ui.selectable_label(j.tool.eraser, "⌫ Eraser").clicked() {
                                j.tool.eraser = !j.tool.eraser;
                            }

                            // Clear all
                            if ui.button("🗑 Clear").clicked() {
                                j.draws.lock().unwrap().remove_user_strokes(j.viewer_id);
                                let msg = AnnotMsg::ClearAll;
                                let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                            }
                        });
                    });

                // ── Canvas ────────────────────────────────────────────────────
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

                        // Cursor tracking (always active).
                        if let Some(pos) = response.hover_pos() {
                            let norm = to_norm(pos);
                            j.cursors.lock().unwrap().update(UserId(j.viewer_id), norm);
                            let msg = AnnotMsg::CursorMove { viewer_id: j.viewer_id, pos: norm };
                            let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                        }

                        if j.tool.eraser {
                            // Finalize any in-progress stroke if tool was switched mid-drag.
                            if let Some(sid) = j.active_stroke.take() {
                                j.draws.lock().unwrap().end_stroke(sid);
                                let msg = AnnotMsg::StrokeEnd { viewer_id: j.viewer_id, stroke_id: sid };
                                let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                            }
                            // Erase strokes near the cursor while dragging.
                            if response.dragged() || response.drag_started() {
                                if let Some(pos) = response.interact_pointer_pos() {
                                    let norm = to_norm(pos);
                                    const ERASE_R: f32 = 0.03;
                                    let to_erase: Vec<Uuid> = {
                                        let layer = j.draws.lock().unwrap();
                                        layer.strokes.iter()
                                            .filter_map(|(&id, s)| {
                                                s.points.iter().any(|p| {
                                                    let dx = p.x - norm.x;
                                                    let dy = p.y - norm.y;
                                                    dx * dx + dy * dy < ERASE_R * ERASE_R
                                                }).then_some(id)
                                            })
                                            .collect()
                                    };
                                    for stroke_id in to_erase {
                                        j.draws.lock().unwrap().remove_stroke(stroke_id);
                                        let msg = AnnotMsg::EraseStroke { viewer_id: j.viewer_id, stroke_id };
                                        let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                                    }
                                }
                            }
                            // Show eraser circle cursor.
                            if let Some(pos) = response.hover_pos() {
                                const ERASE_R: f32 = 0.03;
                                let r = ERASE_R * rect.width().min(rect.height());
                                painter.circle_stroke(pos, r, egui::Stroke::new(2.0, egui::Color32::WHITE));
                            }
                        } else {
                            // ── Draw mode ─────────────────────────────────────
                            if response.drag_started() {
                                let sid = Uuid::new_v4();
                                j.active_stroke = Some(sid);
                                if let Some(pos) = response.interact_pointer_pos() {
                                    let norm  = to_norm(pos);
                                    let width = j.tool.stroke_width();
                                    let color = j.tool.stroke_color().to_string();
                                    let alpha = j.tool.stroke_alpha();
                                    j.draws.lock().unwrap().begin_stroke(UserId(j.viewer_id), sid, norm, width, color.clone(), alpha);
                                    let msg = AnnotMsg::StrokeBegin { viewer_id: j.viewer_id, stroke_id: sid, pos: norm, width, color, alpha };
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
            let (annot_tx, annot_rx) = mpsc::unbounded_channel::<(Uuid, AnnotMsg)>();
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
            let (disconnect_tx, disconnect_rx) = mpsc::unbounded_channel::<Uuid>();
            {
                let transport    = Arc::clone(&transport);
                let annot_tx     = annot_tx;
                let disconnect_tx = disconnect_tx;
                let mut frame_rx = frame_tx.subscribe();
                let mut peer_hint_rx = peer_hint_rx;
                tokio::spawn(async move {
                    let mut peers:    HashMap<SocketAddr, PeerInfo> = HashMap::new();
                    let mut frame_id: u32                           = 0;
                    let mut buf                                      = vec![0u8; 65_536];
                    // Once the signaling task exits it drops peer_hint_tx, closing the channel.
                    // A closed UnboundedReceiver::recv() returns None immediately on every poll,
                    // which would starve the transport.recv() and frame_rx arms.  Guard the arm
                    // with a flag so it is disabled once the channel is drained.
                    let mut hint_done = false;
                    loop {
                        tokio::select! {
                            // Signaling server resolved the viewer's address — punch proactively.
                            hint = peer_hint_rx.recv(), if !hint_done => {
                                match hint {
                                    None => hint_done = true, // channel closed; disable this arm
                                    Some(addr) => {
                                        // addr is the viewer's STUN-reported address — it may be the
                                        // wrong external port if the viewer is behind symmetric NAT.
                                        // Punch it to help open the hole; the punch handler below
                                        // registers the real source port when the viewer's packet arrives.
                                        tracing::info!("signaling: viewer STUN addr {addr}, punching");
                                        let t = Arc::clone(&transport);
                                        tokio::spawn(async move {
                                            for _ in 0..10 {
                                                let _ = t.send_punch(addr).await;
                                                tokio::time::sleep(Duration::from_millis(50)).await;
                                            }
                                        });
                                    }
                                }
                            }
                            res = transport.recv(&mut buf) => {
                                if let Some((src, pkt)) = res {
                                    match pkt {
                                        Packet::Punch => {
                                            let is_new = !peers.contains_key(&src);
                                            if is_new && peers.len() >= MAX_PEERS {
                                                tracing::warn!("peer limit reached ({MAX_PEERS}), dropping punch from {src}");
                                            } else {
                                                let new_id = {
                                                    let entry = peers.entry(src).or_insert(PeerInfo { last_seen: Instant::now(), viewer_id: Uuid::new_v4() });
                                                    entry.last_seen = Instant::now();
                                                    entry.viewer_id
                                                };
                                                if is_new { tracing::info!("new peer {src} id={new_id} (total: {})", peers.len()); }
                                                let _ = transport.send_punch(src).await;
                                            }
                                        }
                                        Packet::Annot(json) => {
                                            if let Some(info) = peers.get_mut(&src) {
                                                info.last_seen = Instant::now();
                                            }
                                            if let Ok(msg) = serde_json::from_str::<AnnotMsg>(&json) {
                                                if let Some(peer) = peers.get(&src) {
                                                    let _ = annot_tx.send((peer.viewer_id, msg));
                                                }
                                            }
                                        }
                                        Packet::Disconnect => {
                                            if let Some(info) = peers.remove(&src) {
                                                tracing::info!("viewer {} disconnected cleanly", info.viewer_id);
                                                let _ = disconnect_tx.send(info.viewer_id);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            res = frame_rx.recv() => {
                                match res {
                                    Ok(frame) => {
                                        // Prune peers that have gone silent for > 30 s.
                                        let now = Instant::now();
                                        let before = peers.len();
                                        peers.retain(|_, info| {
                                            if now.duration_since(info.last_seen) >= Duration::from_secs(30) {
                                                let _ = disconnect_tx.send(info.viewer_id);
                                                false
                                            } else {
                                                true
                                            }
                                        });
                                        if peers.len() < before {
                                            tracing::info!("pruned {} stale peer(s), {} remaining", before - peers.len(), peers.len());
                                        }

                                        if !peers.is_empty() {
                                            let kf = frame_id % 150 == 0;
                                            if frame_id % 150 == 0 { tracing::debug!("host tx frame {frame_id} → {} peer(s)", peers.len()); }
                                            for &addr in peers.keys() {
                                                let _ = transport.send_video(addr, frame_id, &frame, kf).await;
                                            }
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


            let initial_code = room_code.clone();
            let live_code = Arc::new(Mutex::new(room_code.clone()));
            let _ = tx.send(HostReady { transport, room_code, local_code, annot_rx, disconnect_rx, cursors, draws, capture_ok, live_code: Arc::clone(&live_code) });

            // Long-poll /await to learn when viewers join, then loop for the next one.
            // The code stays constant for the session; we only re-register on 404
            // (server evicted the room after 10 min of inactivity).
            if use_signaling {
                tokio::spawn(async move {
                    let mut current_code = initial_code;
                    loop {
                        let server = match server_url() { Some(s) => s, None => return };
                        tracing::debug!("signaling: awaiting viewer on {current_code}");
                        match reqwest::Client::new()
                            .get(format!("{server}/room/{current_code}/await"))
                            .timeout(Duration::from_secs(305))
                            .send().await
                        {
                            Ok(resp) if resp.status().is_success() => {
                                if let Ok(v) = resp.json::<serde_json::Value>().await {
                                    if let Some(addr_str) = v["peer"].as_str() {
                                        if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                                            tracing::info!("signaling: viewer at {addr}");
                                            let _ = peer_hint_tx.send(addr);
                                        }
                                    }
                                }
                                // Loop immediately — more viewers may join on the same code.
                            }
                            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                                // Room was evicted — re-register to get a new code.
                                tracing::info!("signaling: room {current_code} expired, re-registering");
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                match reqwest::Client::new()
                                    .post(format!("{server}/host"))
                                    .json(&serde_json::json!({ "udp": wan_addr.to_string() }))
                                    .send().await
                                {
                                    Ok(r) if r.status().is_success() => {
                                        if let Ok(v) = r.json::<serde_json::Value>().await {
                                            if let Some(code) = v["code"].as_str() {
                                                current_code = code.to_string();
                                                *live_code.lock().unwrap() = current_code.clone();
                                                tracing::info!("signaling: new code {current_code}");
                                            }
                                        }
                                    }
                                    _ => { tokio::time::sleep(Duration::from_secs(5)).await; }
                                }
                            }
                            Ok(_) | Err(_) => {
                                // 408 (no viewer this window) or transient network error — retry same code.
                                tokio::time::sleep(Duration::from_millis(100)).await;
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
            let transport = match Transport::bind_ephemeral().await {
                Ok(t)  => Arc::new(t),
                Err(e) => { tracing::error!("UDP bind: {e}"); return; }
            };

            // Resolve the host address — either directly from the room code or via signaling.
            let mut my_stun:     Option<SocketAddr> = None;
            let mut nat_warning: Option<String>     = None;
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
                    my_stun = Some(my_udp);
                    nat_warning = crate::transport::diagnose_nat(&transport.socket).await;
                    if let Some(ref w) = nat_warning {
                        tracing::warn!("NAT diagnosis: {w}");
                    }
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

            tracing::info!("viewer local={:?} STUN={my_stun:?} host={host_addr}",
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
                                            if actual_host.is_none() {
                                                tracing::info!("punch-back from {src} — locking as actual_host");
                                                actual_host = Some(src);
                                            }
                                        }
                                        Packet::VideoFrag { frame_id, frag_idx, frag_total, keyframe, data } => {
                                            let accepted = match actual_host {
                                                Some(h) => src == h,
                                                None    => src == host_addr || src.port() == host_addr.port(),
                                            };
                                            if !accepted {
                                                tracing::warn!("video from unexpected {src} (expected {host_addr} / {actual_host:?}) — dropping");
                                            } else {
                                                if actual_host.is_none() {
                                                    tracing::info!("host video from {src} — locking as actual_host");
                                                    actual_host = Some(src);
                                                }
                                                tracing::debug!("rx frag frame={frame_id} {frag_idx}/{frag_total} kf={keyframe}");
                                                if let Some((frame, _)) = reassembler.push(frame_id, frag_idx, frag_total, keyframe, data) {
                                                    tracing::debug!("reassembled frame {frame_id} ({} bytes)", frame.len());
                                                    let _ = frame_sync_tx.try_send(frame);
                                                }
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

            let _ = tx.send(JoinReady { transport, rgba_rx, annot_out: annot_out_tx, viewer_id, host_addr, nat_warning });
        });
        rx
    }

    /// Called once `JoinReady` arrives.  Restores a normal windowed viewport
    /// (the host runs fullscreen + passthrough; the viewer needs a regular interactive window).
    fn finish_join(&self, ctx: &egui::Context, ready: JoinReady, name: String) -> State {
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));

        let display_name = if name.trim().is_empty() {
            format!("viewer-{}", &ready.viewer_id.to_string()[..4])
        } else {
            name.trim().to_string()
        };

        // Tell the host our chosen name immediately.
        let register = AnnotMsg::Register { viewer_id: ready.viewer_id, name: display_name.clone() };
        let _ = ready.annot_out.send(serde_json::to_string(&register).unwrap());

        let cursors = Arc::new(Mutex::new(CursorState::default()));
        let draws   = Arc::new(Mutex::new(DrawLayer::default()));
        cursors.lock().unwrap().add_user(UserInfo {
            id:    UserId(ready.viewer_id),
            name:  display_name.clone(),
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
            host_addr:     ready.host_addr,
            active_stroke: None,
            nat_warning:   ready.nat_warning,
            tool:          ToolState::default(),
            name:          display_name,
        })
    }
}

// ── Annotation application (host side) ───────────────────────────────────────

/// Apply one incoming annotation message to the shared host state.
/// Auto-registers unknown viewers on first `CursorMove` using a deterministic colour
/// derived from the viewer UUID so the same viewer always gets the same colour.
fn apply_annot(src_id: Uuid, msg: &AnnotMsg, cursors: &Arc<Mutex<CursorState>>, draws: &Arc<Mutex<DrawLayer>>) {
    match msg {
        AnnotMsg::Register { name, .. } => {
            let mut c = cursors.lock().unwrap();
            if let Some(user) = c.users.get_mut(&src_id) {
                user.name = name.clone();
            } else {
                let palette = ["#e05c5c","#5c9ee0","#5ce07a","#e0c25c","#b05ce0","#5ce0d4"];
                let color   = palette[(src_id.as_u128() % palette.len() as u128) as usize];
                c.add_user(UserInfo {
                    id:    UserId(src_id),
                    name:  name.clone(),
                    color: UserColor(color.into()),
                });
            }
        }
        AnnotMsg::CursorMove { pos, .. } => {
            let mut c = cursors.lock().unwrap();
            if !c.users.contains_key(&src_id) {
                // Fallback: Register wasn't received first (UDP reorder).
                let palette = ["#e05c5c","#5c9ee0","#5ce07a","#e0c25c","#b05ce0","#5ce0d4"];
                let color   = palette[(src_id.as_u128() % palette.len() as u128) as usize];
                c.add_user(UserInfo {
                    id:    UserId(src_id),
                    name:  format!("viewer-{}", &src_id.to_string()[..4]),
                    color: UserColor(color.into()),
                });
            }
            c.update(UserId(src_id), *pos);
        }
        AnnotMsg::StrokeBegin { stroke_id, pos, width, color, alpha, .. } => {
            draws.lock().unwrap().begin_stroke(UserId(src_id), *stroke_id, *pos, *width, color.clone(), *alpha);
        }
        AnnotMsg::StrokePoint { stroke_id, pos, .. } => {
            draws.lock().unwrap().add_point(UserId(src_id), *stroke_id, *pos);
        }
        AnnotMsg::StrokeEnd { stroke_id, .. } => {
            draws.lock().unwrap().end_stroke(*stroke_id);
        }
        AnnotMsg::EraseStroke { stroke_id, .. } => {
            draws.lock().unwrap().remove_stroke(*stroke_id);
        }
        AnnotMsg::ClearAll => {
            draws.lock().unwrap().remove_user_strokes(src_id);
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

/// Set `_NET_WM_WINDOW_TYPE_NOTIFICATION` on the overlay window.
///
/// GNOME/Mutter places notification-type windows above all normal windows,
/// independent of focus changes — unlike `_NET_WM_STATE_ABOVE`, which Mutter
/// overrides when another window is raised.  The type is set here, immediately
/// after eframe creates the window, via a fresh RustConnection so we don't
/// interfere with winit's own xcb state.
#[cfg(target_os = "linux")]
fn x11_set_notification_type(cc: &eframe::CreationContext) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;

    let window_id: u32 = match cc.window_handle().ok().map(|h| h.as_raw()) {
        Some(RawWindowHandle::Xcb(h))  => h.window.get(),
        Some(RawWindowHandle::Xlib(h)) => h.window as u32,
        _ => {
            tracing::warn!("x11 overlay type: unrecognised window handle — skipping");
            return;
        }
    };

    let conn = match RustConnection::connect(None) {
        Ok((c, _)) => c,
        Err(e) => { tracing::warn!("x11 overlay type: X11 connect failed: {e}"); return; }
    };

    let wm_type = match conn.intern_atom(false, b"_NET_WM_WINDOW_TYPE").ok()
        .and_then(|c| c.reply().ok()).map(|r| r.atom)
    {
        Some(a) => a,
        None => { tracing::warn!("x11 overlay type: intern _NET_WM_WINDOW_TYPE failed"); return; }
    };

    let notification = match conn.intern_atom(false, b"_NET_WM_WINDOW_TYPE_NOTIFICATION").ok()
        .and_then(|c| c.reply().ok()).map(|r| r.atom)
    {
        Some(a) => a,
        None => { tracing::warn!("x11 overlay type: intern _NET_WM_WINDOW_TYPE_NOTIFICATION failed"); return; }
    };

    let cookie = match conn.change_property32(PropMode::REPLACE, window_id, wm_type,
                                              AtomEnum::ATOM, &[notification])
    {
        Ok(c) => c,
        Err(e) => { tracing::warn!("x11 overlay type: change_property failed: {e}"); return; }
    };
    if let Err(e) = cookie.check() {
        tracing::warn!("x11 overlay type: change_property check failed: {e}");
        return;
    }

    let _ = conn.flush();
    tracing::info!("x11 overlay type: set _NET_WM_WINDOW_TYPE_NOTIFICATION on window {window_id}");
}
