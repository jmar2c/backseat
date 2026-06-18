use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use crate::audio::{AudioCapture, AudioPlayer, AudioSource};
use crate::sticker_layer::{
    AssembleResult, HostSticker, HostStickerEntry, StickerReassembler,
    ViewerSticker, ViewerStickerLayer, MAX_STICKERS_PER_VIEWER,
};

// cpal::Stream is !Send, so AudioCapture and AudioPlayer cannot be moved into
// tokio tasks or Send types.  Since audio runs for the lifetime of the process
// (no "stop hosting" transition exists), Box::leak is the right approach.
fn keepalive<T: 'static>(v: T) {
    Box::leak(Box::new(v));
}
use crate::capture::ScreenCapture;
use crate::cursor::CursorState;
use crate::decoder::Vp9Decoder;
use crate::draw_layer::DrawLayer;
use crate::encoder::Vp9Encoder;
use crate::transport::{Packet, Reassembler, RoomCode, Transport};
use crate::types::{AnnotMsg, NormPoint, UserColor, UserId, UserInfo};

// ── Tool state ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum PenType { Pen, Marker }

#[derive(Clone, Copy, PartialEq)]
enum ActiveTool { Draw, Eraser, Select }

struct ToolState {
    active:    ActiveTool,
    pen_type:  PenType,
    color_idx: usize,
    size:      f32,
}

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
        Self { active: ActiveTool::Draw, pen_type: PenType::Pen, color_idx: 4, size: 3.0 }
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

// ── Quality / FPS settings ────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Default)]
enum QualityPreset { Low, Medium, High, #[default] Auto }

impl QualityPreset {
    fn bitrate_kbps(self) -> u32 {
        match self { Self::Low => 2_000, Self::Medium => 4_000, Self::High => 8_000, Self::Auto => 4_000 }
    }
    fn label(self) -> &'static str {
        match self { Self::Low => "Low", Self::Medium => "Medium", Self::High => "High", Self::Auto => "Auto (ABR)" }
    }
}

// ── Internal types ────────────────────────────────────────────────────────────

struct RgbaFrame { width: u32, height: u32, data: Vec<u8> }

/// Encoded VP8 frame plus the 90 kHz RTP timestamp used to encode it.
struct EncodedFrame {
    data:     Vec<u8>,
    pts:      u32,
    keyframe: bool,
}

struct PeerInfo {
    last_seen:   Instant,
    viewer_id:   Uuid,
    last_kf_req: Instant,
}

struct HostReady {
    transport:      Arc<Transport>,
    room_code:      String,
    local_code:     String,
    annot_rx:       mpsc::Receiver<(Uuid, AnnotMsg)>,
    disconnect_rx:  mpsc::UnboundedReceiver<Uuid>,
    sticker_rx:     mpsc::UnboundedReceiver<(Uuid, u64, HostSticker)>,
    sticker_counts: Arc<Mutex<HashMap<Uuid, usize>>>,
    cursors:        Arc<Mutex<CursorState>>,
    draws:          Arc<Mutex<DrawLayer>>,
    capture_ok:     Arc<AtomicBool>,
    live_code:      Arc<Mutex<String>>,
}

struct HostCtx {
    room_code:      String,
    local_code:     String,
    _transport:     Arc<Transport>,
    annot_rx:       mpsc::Receiver<(Uuid, AnnotMsg)>,
    disconnect_rx:  mpsc::UnboundedReceiver<Uuid>,
    sticker_rx:     mpsc::UnboundedReceiver<(Uuid, u64, HostSticker)>,
    stickers:       std::collections::HashMap<u64, HostStickerEntry>,
    sticker_counts: Arc<Mutex<HashMap<Uuid, usize>>>,
    cursors:        Arc<Mutex<CursorState>>,
    draws:          Arc<Mutex<DrawLayer>>,
    tray:           crate::tray::HostTray,
    capture_ok:     Arc<AtomicBool>,
    live_code:      Arc<Mutex<String>>,
}

#[derive(Default)]
struct ConnectionStats {
    rx_bps:   f32,
    tx_bps:   f32,
    loss_pct: f32,
    ping_ms:  Option<f32>,
}

struct JoinReady {
    transport:   Arc<Transport>,
    rgba_rx:     mpsc::UnboundedReceiver<RgbaFrame>,
    annot_out:   mpsc::UnboundedSender<String>,
    image_out:   mpsc::UnboundedSender<(u64, Vec<u8>)>,
    viewer_id:   Uuid,
    host_addr:   SocketAddr,
    nat_warning: Option<String>,
    stats:       Arc<Mutex<ConnectionStats>>,
}

/// Carries image bytes + sticker metadata from the async upload task back to the egui thread.
struct UploadedImage {
    sticker_id: u64,
    bytes:      Vec<u8>,
    pos:        crate::types::NormPoint,
    size:       crate::types::NormPoint,
}

struct JoinCtx {
    _transport:      Arc<Transport>,
    rgba_rx:         mpsc::UnboundedReceiver<RgbaFrame>,
    annot_out:       mpsc::UnboundedSender<String>,
    image_out:       mpsc::UnboundedSender<(u64, Vec<u8>)>,
    cursors:         Arc<Mutex<CursorState>>,
    draws:           Arc<Mutex<DrawLayer>>,
    stickers:        ViewerStickerLayer,
    sticker_textures: std::collections::HashMap<u64, egui::TextureHandle>,
    selected_sticker:        Option<u64>,
    sticker_dragging_resize: bool,
    upload_rx:               Option<tokio::sync::oneshot::Receiver<Option<UploadedImage>>>,
    texture:         Option<egui::TextureHandle>,
    viewer_id:       Uuid,
    host_addr:       SocketAddr,
    active_stroke:   Option<Uuid>,
    nat_warning:     Option<String>,
    tool:            ToolState,
    name:            String,
    show_stats:      bool,
    stats:           Arc<Mutex<ConnectionStats>>,
}

// ── State machine ─────────────────────────────────────────────────────────────

enum State {
    ChoosingMode,
    ConfiguringHost {
        audio_source: AudioSource,
        has_monitor:  bool,
        devices:      Vec<crate::audio::AudioDeviceInfo>,
        fps:          u32,
        quality:      QualityPreset,
    },
    Discovering  { rx: tokio::sync::oneshot::Receiver<HostReady> },
    Hosting      (HostCtx),
    EnteringCode { name: String, input: String, error: Option<String>, connect_rx: Option<tokio::sync::oneshot::Receiver<JoinReady>> },
    Joining      (JoinCtx),
}

// ── App ───────────────────────────────────────────────────────────────────────

pub struct OverlayApp {
    state: State,
    rt:    tokio::runtime::Runtime,
    #[cfg(target_os = "linux")]
    x11_window_id: Option<u32>,
}

impl OverlayApp {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        #[cfg(target_os = "linux")]
        let x11_window_id = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match cc.window_handle().ok().map(|h| h.as_raw()) {
                Some(RawWindowHandle::Xcb(h))  => Some(h.window.get()),
                Some(RawWindowHandle::Xlib(h)) => Some(h.window as u32),
                _ => None,
            }
        };

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        Self {
            state: State::ChoosingMode,
            rt,
            #[cfg(target_os = "linux")]
            x11_window_id,
        }
    }

    fn step(&mut self, ctx: &egui::Context, state: State) -> State {
        match state {

            // ── Choose host or join ───────────────────────────────────────────
            State::ChoosingMode => {
                let mut clicked_host = false;
                let mut clicked_join = false;
                let mut close = false;
                egui::CentralPanel::default().show(ctx, |ui| {
                    let title_row = ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("backseat").strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            close = ui.small_button("x").clicked();
                        });
                    });
                    let drag = ui.interact(
                        title_row.response.rect,
                        ui.id().with("titlebar_drag"),
                        egui::Sense::drag(),
                    );
                    if drag.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    ui.separator();
                    let remaining = ui.available_height();
                    ui.add_space((remaining - 24.0).max(0.0) / 2.0);
                    ui.vertical_centered(|ui| {
                        ui.horizontal(|ui| {
                            clicked_host = ui.button("  Host  ").clicked();
                            clicked_join = ui.button("  Join  ").clicked();
                        });
                    });
                });
                if close {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if clicked_host {
                    let (has_monitor, devices) = crate::audio::probe_devices();
                    return State::ConfiguringHost {
                        audio_source: AudioSource::None,
                        has_monitor,
                        devices,
                        fps:     30,
                        quality: QualityPreset::Auto,
                    };
                }
                if clicked_join {
                    return State::EnteringCode {
                        name: String::new(), input: String::new(),
                        error: None, connect_rx: None,
                    };
                }
                State::ChoosingMode
            }

            // ── Host settings ─────────────────────────────────────────────────
            State::ConfiguringHost { mut audio_source, has_monitor, devices, mut fps, mut quality } => {
                let mut go_back  = false;
                let mut go_start = false;

                egui::CentralPanel::default().show(ctx, |ui| {
                    let title_row = ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("backseat — host settings").strong());
                    });
                    let drag = ui.interact(
                        title_row.response.rect,
                        ui.id().with("titlebar_drag"),
                        egui::Sense::drag(),
                    );
                    if drag.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    ui.separator();
                    ui.label(egui::RichText::new("Audio source").strong());
                    ui.add_space(4.0);
                    ui.radio_value(&mut audio_source, AudioSource::None, "None");

                    // Microphone group: default mic radio + device picker combobox.
                    let mic_active = matches!(audio_source, AudioSource::Microphone | AudioSource::NamedDevice(_));
                    ui.horizontal(|ui| {
                        if ui.radio(mic_active, "Microphone").clicked() {
                            audio_source = AudioSource::Microphone;
                        }
                        ui.add_enabled_ui(mic_active, |ui| {
                            let selected_label = match &audio_source {
                                AudioSource::NamedDevice(n) => n.as_str(),
                                _                           => "Default",
                            };
                            egui::ComboBox::from_id_source("mic_device")
                                .selected_text(selected_label)
                                .show_ui(ui, |ui| {
                                    if ui.selectable_label(
                                        matches!(audio_source, AudioSource::Microphone),
                                        "Default",
                                    ).clicked() {
                                        audio_source = AudioSource::Microphone;
                                    }
                                    for dev in devices.iter().filter(|d| !d.is_monitor) {
                                        let sel = matches!(&audio_source, AudioSource::NamedDevice(n) if n == &dev.name);
                                        if ui.selectable_label(sel, &dev.name).clicked() {
                                            audio_source = AudioSource::NamedDevice(dev.name.clone());
                                        }
                                    }
                                });
                        });
                    });

                    ui.add_enabled_ui(has_monitor, |ui| {
                        ui.radio_value(&mut audio_source, AudioSource::Desktop, "Desktop audio");
                    });
                    if !has_monitor {
                        ui.label(
                            egui::RichText::new("(desktop audio not available — no monitor source found)")
                                .small()
                                .color(egui::Color32::GRAY),
                        );
                    }
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Frame rate").strong());
                    ui.horizontal(|ui| {
                        for &(label, f) in &[("15 fps", 15u32), ("24 fps", 24), ("30 fps", 30), ("60 fps", 60)] {
                            ui.radio_value(&mut fps, f, label);
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Stream quality").strong());
                    ui.horizontal(|ui| {
                        for &q in &[QualityPreset::Low, QualityPreset::Medium, QualityPreset::High, QualityPreset::Auto] {
                            ui.radio_value(&mut quality, q, q.label());
                        }
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        go_back  = ui.button("Back").clicked();
                        go_start = ui.button("Start Hosting").clicked();
                    });
                });

                if go_back  { return State::ChoosingMode; }
                if go_start { return self.begin_host(audio_source, fps, quality); }
                State::ConfiguringHost { audio_source, has_monitor, devices, fps, quality }
            }

            // ── Waiting for STUN ──────────────────────────────────────────────
            State::Discovering { mut rx } => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let title_row = ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("backseat").strong());
                    });
                    let drag = ui.interact(
                        title_row.response.rect,
                        ui.id().with("titlebar_drag"),
                        egui::Sense::drag(),
                    );
                    if drag.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    ui.separator();
                    let remaining = ui.available_height();
                    ui.add_space((remaining - 20.0).max(0.0) / 2.0);
                    ui.vertical_centered(|ui| { ui.label("Discovering public address…"); });
                });
                match rx.try_recv() {
                    Ok(ready) => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
                        #[cfg(target_os = "linux")]
                        if let Some(wid) = self.x11_window_id {
                            x11_set_notification_type(wid);
                        }
                        {
                            let sz = ctx.input(|i| i.viewport().monitor_size)
                                .unwrap_or_else(|| ctx.screen_rect().size());
                            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(0.0, 0.0)));
                            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(sz));
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
                        let tray = crate::tray::HostTray::new(ready.room_code.clone());
                        State::Hosting(HostCtx {
                            room_code:      ready.room_code,
                            local_code:     ready.local_code,
                            _transport:     ready.transport,
                            annot_rx:       ready.annot_rx,
                            disconnect_rx:  ready.disconnect_rx,
                            sticker_rx:     ready.sticker_rx,
                            stickers:       std::collections::HashMap::new(),
                            sticker_counts: ready.sticker_counts,
                            cursors:        ready.cursors,
                            draws:          ready.draws,
                            tray,
                            capture_ok:     ready.capture_ok,
                            live_code:      ready.live_code,
                        })
                    }
                    Err(_) => State::Discovering { rx },
                }
            }

            // ── Hosting ───────────────────────────────────────────────────────
            State::Hosting(mut h) => {
                while let Ok((src_id, msg)) = h.annot_rx.try_recv() {
                    apply_annot(src_id, &msg, &h.cursors, &h.draws, &mut h.stickers, &h.sticker_counts, ctx);
                }
                while let Ok((owner, sticker_id, sticker)) = h.sticker_rx.try_recv() {
                    let img = egui::ColorImage::from_rgba_unmultiplied(
                        [1, 1], &[0, 0, 0, 0], // placeholder; real load below
                    );
                    // Load image bytes into an egui texture.
                    if let Ok(dyn_img) = image::load_from_memory(&sticker.image_bytes) {
                        let rgba = dyn_img.to_rgba8();
                        let ci = egui::ColorImage::from_rgba_unmultiplied(
                            [rgba.width() as usize, rgba.height() as usize],
                            rgba.as_raw(),
                        );
                        let tex = ctx.load_texture(
                            format!("sticker-{sticker_id}"),
                            ci,
                            egui::TextureOptions::LINEAR,
                        );
                        h.stickers.insert(sticker_id, HostStickerEntry {
                            texture: tex,
                            pos:     sticker.pos,
                            size:    sticker.size,
                            owner,
                        });
                    }
                    drop(img); // suppress unused warning
                }
                if h.tray.pop_copy_request() {
                    ctx.output_mut(|o| o.copied_text = h.room_code.clone());
                }
                if h.tray.pop_exit_request() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                while let Ok(id) = h.disconnect_rx.try_recv() {
                    h.cursors.lock().unwrap().remove_user(&UserId(id));
                    h.draws.lock().unwrap().remove_user_strokes(id);
                    h.stickers.retain(|_, s| s.owner != id);
                }

                egui::Window::new("backseat")
                    .anchor(egui::Align2::RIGHT_TOP, egui::Vec2::new(-12.0, 12.0))
                    .resizable(false).collapsible(false).title_bar(false)
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
                        let is_short = current_code.len() == 6
                            && current_code.chars().all(|c| c.is_ascii_uppercase());
                        if is_short {
                            ui.horizontal(|ui| {
                                ui.label("Code:");
                                ui.monospace(&current_code);
                            });
                        } else {
                            let loopback = format!(
                                "127.0.0.1:{}",
                                current_code.split(':').last().unwrap_or("?")
                            );
                            ui.horizontal(|ui| { ui.label("Same machine:"); ui.monospace(&loopback); });
                            ui.horizontal(|ui| { ui.label("LAN:"); ui.monospace(&h.local_code); });
                            ui.horizontal(|ui| { ui.label("WAN:"); ui.monospace(&current_code); });
                        }
                    });

                egui::CentralPanel::default()
                    .frame(egui::Frame::none())
                    .show(ctx, |ui| {
                        // Correct for any WM-imposed offset (e.g. taskbar struts pushing
                        // the window down). NormPoints are relative to the full monitor, so
                        // the drawing rect must also be expressed in window-local coords
                        // that span the full monitor — even if that extends above the window.
                        let win_min = ctx.input(|i| i.viewport().inner_rect.map(|r| r.min))
                            .unwrap_or(egui::Pos2::ZERO);
                        let monitor = ctx.input(|i| i.viewport().monitor_size)
                            .unwrap_or_else(|| ui.max_rect().size());
                        let rect = egui::Rect::from_min_size(
                            egui::pos2(-win_min.x, -win_min.y),
                            monitor,
                        );
                        crate::renderer::paint_stickers(ui.painter(), rect, &h.stickers);
                        crate::renderer::paint(ui.painter(), rect, &h.draws, &h.cursors);
                    });

                State::Hosting(h)
            }

            // ── Enter name + room code ────────────────────────────────────────
            State::EnteringCode { mut name, mut input, error, mut connect_rx } => {
                if let Some(ref mut rx) = connect_rx {
                    if let Ok(ready) = rx.try_recv() {
                        return self.finish_join(ctx, ready, name);
                    }
                }

                let mut go_back    = false;
                let mut go_connect = false;
                egui::CentralPanel::default().show(ctx, |ui| {
                    let title_row = ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("backseat — join").strong());
                    });
                    let drag = ui.interact(
                        title_row.response.rect,
                        ui.id().with("titlebar_drag"),
                        egui::Sense::drag(),
                    );
                    if drag.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    ui.separator();
                    ui.label("Your name:");
                    ui.text_edit_singleline(&mut name);
                    ui.add_space(4.0);
                    ui.label("Room code:");
                    let resp = ui.text_edit_singleline(&mut input);
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
                            name, input,
                            error: Some("Invalid room code (expected 6-letter code or IP:port)".into()),
                            connect_rx: None,
                        },
                    }
                }
                State::EnteringCode { name, input, error, connect_rx }
            }

            // ── Joined ────────────────────────────────────────────────────────
            State::Joining(mut j) => {
                if ctx.input(|i| i.viewport().close_requested()) {
                    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
                        let _ = sock.send_to(&[0x04u8], j.host_addr);
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }

                while let Ok(frame) = j.rgba_rx.try_recv() {
                    let img = egui::ColorImage::from_rgba_unmultiplied(
                        [frame.width as usize, frame.height as usize],
                        &frame.data,
                    );
                    match &mut j.texture {
                        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                        None    => {
                            tracing::debug!("first video texture {}x{}", frame.width, frame.height);
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

                // ── Poll async upload result ──────────────────────────────────
                if let Some(ref mut rx) = j.upload_rx {
                    if let Ok(Some(uploaded)) = rx.try_recv() {
                        j.upload_rx = None;
                        // Load texture locally for optimistic rendering.
                        if let Ok(dyn_img) = image::load_from_memory(&uploaded.bytes) {
                            let rgba = dyn_img.to_rgba8();
                            let ci = egui::ColorImage::from_rgba_unmultiplied(
                                [rgba.width() as usize, rgba.height() as usize],
                                rgba.as_raw(),
                            );
                            let tex = ctx.load_texture(
                                format!("sticker-local-{}", uploaded.sticker_id),
                                ci,
                                egui::TextureOptions::LINEAR,
                            );
                            j.sticker_textures.insert(uploaded.sticker_id, tex);
                        }
                        j.stickers.add(ViewerSticker {
                            sticker_id: uploaded.sticker_id,
                            pos:  uploaded.pos,
                            size: uploaded.size,
                        });
                        // Send manifest+chunks to host.
                        let _ = j.image_out.send((uploaded.sticker_id, uploaded.bytes));
                        // Notify host of initial position via AnnotMsg.
                        let msg = AnnotMsg::StickerPlace {
                            sticker_id: uploaded.sticker_id,
                            pos:  uploaded.pos,
                            size: uploaded.size,
                        };
                        let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                    } else if let Ok(None) = rx.try_recv() {
                        j.upload_rx = None; // user cancelled
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

                            if ui.selectable_label(j.tool.active == ActiveTool::Draw && j.tool.pen_type == PenType::Pen, "✏ Pen").clicked() {
                                j.tool.active   = ActiveTool::Draw;
                                j.tool.pen_type = PenType::Pen;
                            }
                            if ui.selectable_label(j.tool.active == ActiveTool::Draw && j.tool.pen_type == PenType::Marker, "🖍 Marker").clicked() {
                                j.tool.active   = ActiveTool::Draw;
                                j.tool.pen_type = PenType::Marker;
                            }
                            ui.separator();

                            for (i, &(hex, label)) in PALETTE.iter().enumerate() {
                                let color    = crate::renderer::hex_to_color32(hex);
                                let selected = j.tool.color_idx == i && j.tool.active == ActiveTool::Draw;
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
                                    j.tool.active    = ActiveTool::Draw;
                                }
                            }
                            ui.separator();

                            for &(label, sz) in &[("S", 2.0f32), ("M", 4.0), ("L", 8.0)] {
                                if ui.selectable_label(j.tool.active == ActiveTool::Draw && j.tool.size == sz, label).clicked() {
                                    j.tool.size   = sz;
                                    j.tool.active = ActiveTool::Draw;
                                }
                            }
                            ui.separator();

                            if ui.selectable_label(j.tool.active == ActiveTool::Eraser, "⌫ Eraser").clicked() {
                                j.tool.active = if j.tool.active == ActiveTool::Eraser {
                                    ActiveTool::Draw
                                } else {
                                    ActiveTool::Eraser
                                };
                            }
                            if ui.selectable_label(j.tool.active == ActiveTool::Select, "↖ Select").clicked() {
                                j.tool.active = if j.tool.active == ActiveTool::Select {
                                    ActiveTool::Draw
                                } else {
                                    ActiveTool::Select
                                };
                            }
                            ui.separator();

                            let can_upload = j.stickers.count() < MAX_STICKERS_PER_VIEWER && j.upload_rx.is_none();
                            if ui.add_enabled(can_upload, egui::Button::new("🖼 Sticker")).clicked() {
                                let (tx, rx) = tokio::sync::oneshot::channel();
                                j.upload_rx = Some(rx);
                                self.rt.spawn(async move {
                                    let result = pick_and_process_image().await;
                                    let _ = tx.send(result);
                                });
                            }

                            if ui.button("🗑 Clear").clicked() {
                                j.draws.lock().unwrap().remove_user_strokes(j.viewer_id);
                                let msg = AnnotMsg::ClearAll;
                                let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                            }

                            ui.separator();
                            ui.menu_button("⚙", |ui| {
                                ui.checkbox(&mut j.show_stats, "Show stats");
                            });
                        });
                    });

                // ── Canvas ────────────────────────────────────────────────────
                egui::CentralPanel::default()
                    .frame(egui::Frame::none().fill(egui::Color32::BLACK))
                    .show(ctx, |ui| {
                        let rect     = ui.max_rect();
                        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
                        let painter  = ui.painter();
                        let to_norm = |p: egui::Pos2| NormPoint {
                            x: ((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0),
                            y: ((p.y - rect.min.y) / rect.height()).clamp(0.0, 1.0),
                        };

                        if let Some(pos) = response.hover_pos() {
                            let norm = to_norm(pos);
                            j.cursors.lock().unwrap().update(UserId(j.viewer_id), norm);
                            let msg = AnnotMsg::CursorMove { pos: norm };
                            let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                        }

                        match j.tool.active {
                            ActiveTool::Eraser => {
                                if let Some(sid) = j.active_stroke.take() {
                                    j.draws.lock().unwrap().end_stroke(sid);
                                    let msg = AnnotMsg::StrokeEnd { stroke_id: sid };
                                    let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                                }
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
                                            let msg = AnnotMsg::EraseStroke { stroke_id };
                                            let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                                        }
                                    }
                                }
                                if let Some(pos) = response.hover_pos() {
                                    const ERASE_R: f32 = 0.03;
                                    let r = ERASE_R * rect.width().min(rect.height());
                                    painter.circle_stroke(pos, r, egui::Stroke::new(2.0, egui::Color32::WHITE));
                                }
                            }

                            ActiveTool::Select => {
                                if let Some(sid) = j.active_stroke.take() {
                                    j.draws.lock().unwrap().end_stroke(sid);
                                    let msg = AnnotMsg::StrokeEnd { stroke_id: sid };
                                    let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                                }
                                // Capture drag mode (move vs resize) once at drag start.
                                if response.drag_started() {
                                    if let Some(pos) = response.interact_pointer_pos() {
                                        let norm = to_norm(pos);
                                        j.sticker_dragging_resize = false;
                                        // Check resize handle on the already-selected sticker FIRST —
                                        // the handle extends outside sticker bounds so hit_test would miss it.
                                        let started_resize = j.selected_sticker.and_then(|sel_id| {
                                            j.stickers.stickers.iter().find(|s| s.sticker_id == sel_id)
                                        }).map_or(false, |s| {
                                            let corner = crate::types::NormPoint {
                                                x: s.pos.x + s.size.x,
                                                y: s.pos.y + s.size.y,
                                            };
                                            let dx = norm.x - corner.x;
                                            let dy = norm.y - corner.y;
                                            dx * dx + dy * dy < 0.025 * 0.025
                                        });
                                        if started_resize {
                                            j.sticker_dragging_resize = true;
                                        } else {
                                            j.selected_sticker = j.stickers.hit_test(norm);
                                        }
                                    }
                                }
                                if response.dragged() {
                                    if let Some(sel_id) = j.selected_sticker {
                                        let delta = response.drag_delta();
                                        let dnx = delta.x / rect.width();
                                        let dny = delta.y / rect.height();
                                        if let Some(s) = j.stickers.get_mut(sel_id) {
                                            if j.sticker_dragging_resize {
                                                s.size.x = (s.size.x + dnx).max(0.05);
                                                s.size.y = (s.size.y + dny).max(0.05);
                                            } else {
                                                s.pos.x += dnx;
                                                s.pos.y += dny;
                                            }
                                            let msg = AnnotMsg::StickerMove {
                                                sticker_id: sel_id,
                                                pos:  s.pos,
                                                size: s.size,
                                            };
                                            let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                                        }
                                    }
                                }
                                // Check for X-button click on the selected sticker.
                                if response.clicked() {
                                    if let Some(sel_id) = j.selected_sticker {
                                        if let Some(pos) = response.interact_pointer_pos() {
                                            let norm = to_norm(pos);
                                            if let Some(s) = j.stickers.stickers.iter().find(|s| s.sticker_id == sel_id) {
                                                let x_norm = crate::types::NormPoint {
                                                    x: s.pos.x + s.size.x,
                                                    y: s.pos.y,
                                                };
                                                let dx = norm.x - x_norm.x;
                                                let dy = norm.y - x_norm.y;
                                                if dx * dx + dy * dy < 0.018 * 0.018 {
                                                    let msg = AnnotMsg::StickerRemove { sticker_id: sel_id };
                                                    let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                                                    j.stickers.remove(sel_id);
                                                    j.sticker_textures.remove(&sel_id);
                                                    j.selected_sticker = None;
                                                } else {
                                                    j.selected_sticker = j.stickers.hit_test(norm);
                                                }
                                            }
                                        }
                                    } else if let Some(pos) = response.interact_pointer_pos() {
                                        let norm = to_norm(pos);
                                        j.selected_sticker = j.stickers.hit_test(norm);
                                    }
                                }

                                // Cursor and hover state for handle proximity.
                                if let Some(hover_pos) = response.hover_pos() {
                                    let hn = to_norm(hover_pos);
                                    if let Some(sel_id) = j.selected_sticker {
                                        if let Some(s) = j.stickers.stickers.iter().find(|s| s.sticker_id == sel_id) {
                                            let corner = crate::types::NormPoint {
                                                x: s.pos.x + s.size.x, y: s.pos.y + s.size.y,
                                            };
                                            let cdx = hn.x - corner.x;
                                            let cdy = hn.y - corner.y;
                                            let x_pt = crate::types::NormPoint {
                                                x: s.pos.x + s.size.x, y: s.pos.y,
                                            };
                                            let xdx = hn.x - x_pt.x;
                                            let xdy = hn.y - x_pt.y;
                                            if cdx * cdx + cdy * cdy < 0.025 * 0.025 {
                                                ctx.set_cursor_icon(egui::CursorIcon::ResizeSouthEast);
                                            } else if xdx * xdx + xdy * xdy < 0.018 * 0.018 {
                                                ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                                            }
                                        }
                                    }
                                }
                            }

                            ActiveTool::Draw => {
                                if response.drag_started() {
                                    let sid = Uuid::new_v4();
                                    j.active_stroke = Some(sid);
                                    if let Some(pos) = response.interact_pointer_pos() {
                                        let norm  = to_norm(pos);
                                        let width = j.tool.stroke_width();
                                        let color = j.tool.stroke_color().to_string();
                                        let alpha = j.tool.stroke_alpha();
                                        j.draws.lock().unwrap().begin_stroke(UserId(j.viewer_id), sid, norm, width, color.clone(), alpha);
                                        let msg = AnnotMsg::StrokeBegin { stroke_id: sid, pos: norm, width, color, alpha };
                                        let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                                    }
                                }
                                if let Some(sid) = j.active_stroke {
                                    if response.dragged() {
                                        if let Some(pos) = response.interact_pointer_pos() {
                                            let norm = to_norm(pos);
                                            j.draws.lock().unwrap().add_point(UserId(j.viewer_id), sid, norm);
                                            let msg = AnnotMsg::StrokePoint { stroke_id: sid, pos: norm };
                                            let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                                        }
                                    }
                                    if response.drag_stopped() {
                                        j.draws.lock().unwrap().end_stroke(sid);
                                        let msg = AnnotMsg::StrokeEnd { stroke_id: sid };
                                        let _ = j.annot_out.send(serde_json::to_string(&msg).unwrap());
                                        j.active_stroke = None;
                                    }
                                }
                            }
                        }

                        if let Some(tex) = &j.texture {
                            painter.image(
                                tex.id(),
                                rect,
                                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                egui::Color32::WHITE,
                            );
                        }

                        // Stickers always painted after the video frame.
                        // selected_sticker is only Some when Select tool is active.
                        let hover_norm = response.hover_pos().map(|p| to_norm(p));
                        crate::renderer::paint_viewer_stickers(
                            painter, rect, &j.stickers, &j.sticker_textures,
                            j.selected_sticker, hover_norm,
                        );

                        crate::renderer::paint(painter, rect, &j.draws, &j.cursors);

                        if j.show_stats {
                            if let Ok(s) = j.stats.try_lock() {
                                let ping_str = s.ping_ms
                                    .map_or("—".to_string(), |p| format!("{:.0}ms", p));
                                let line1 = format!(
                                    "RX {:.2} MB/s   TX {:.2} MB/s",
                                    s.rx_bps / 1_000_000.0,
                                    s.tx_bps / 1_000_000.0,
                                );
                                let line2 = format!("Loss {:.1}%   Ping {}", s.loss_pct, ping_str);
                                let font   = egui::FontId::monospace(12.0);
                                let origin = rect.min + egui::vec2(8.0, 8.0);
                                painter.rect_filled(
                                    egui::Rect::from_min_size(
                                        origin - egui::vec2(4.0, 2.0),
                                        egui::vec2(240.0, 38.0),
                                    ),
                                    4.0,
                                    egui::Color32::from_black_alpha(160),
                                );
                                painter.text(origin,                          egui::Align2::LEFT_TOP, &line1, font.clone(), egui::Color32::WHITE);
                                painter.text(origin + egui::vec2(0.0, 18.0), egui::Align2::LEFT_TOP, &line2, font,         egui::Color32::WHITE);
                            }
                        }
                    });

                State::Joining(j)
            }
        }
    }

    // ── Background task launchers ─────────────────────────────────────────────

    fn begin_host(&mut self, audio_source: AudioSource, fps: u32, quality: QualityPreset) -> State {
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

            let cursors      = Arc::new(Mutex::new(CursorState::default()));
            let draws        = Arc::new(Mutex::new(DrawLayer::default()));
            let capture_ok   = Arc::new(AtomicBool::new(true));
            let keyframe_req = Arc::new(AtomicBool::new(false));
            let viewer_loss  = Arc::new(Mutex::new(0.0f32));

            let kf_secs = std::env::var("BACKSEAT_KF_SECS")
                .ok()
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(2.0)
                .max(0.1);
            let kf_frames       = (kf_secs * fps as f32).round() as u64;
            let initial_bitrate = quality.bitrate_kbps();
            let is_auto         = quality == QualityPreset::Auto;
            tracing::debug!("stream config: {fps}fps kf_every={kf_frames} ({kf_secs:.1}s) {initial_bitrate}kbps auto={is_auto}");
            let (annot_tx, annot_rx)         = mpsc::channel::<(Uuid, AnnotMsg)>(1_024);
            let (sticker_tx, sticker_rx)     = mpsc::unbounded_channel::<(Uuid, u64, HostSticker)>();
            let (frame_tx, _dummy)           = broadcast::channel::<Arc<EncodedFrame>>(4);
            let (peer_hint_tx, peer_hint_rx) = mpsc::unbounded_channel::<SocketAddr>();

            // Audio capture (optional).
            // AudioCapture is !Send (cpal::Stream), so we keep it alive on a dedicated
            // std::thread and only pass the Send channel into the tokio task.
            let audio_rx: Option<mpsc::UnboundedReceiver<(u32, Vec<u8>)>> =
                if audio_source != AudioSource::None {
                    match AudioCapture::start(audio_source) {
                        Ok((cap, rx)) => { keepalive(cap); Some(rx) }
                        Err(e) => { tracing::warn!("audio capture unavailable: {e}"); None }
                    }
                } else {
                    None
                };

            // Screen capture + VP8 encode thread.
            {
                let tx           = frame_tx.clone();
                let capture_ok   = Arc::clone(&capture_ok);
                let keyframe_req = Arc::clone(&keyframe_req);
                let viewer_loss  = Arc::clone(&viewer_loss);
                std::thread::spawn(move || {
                    let mut cap = match ScreenCapture::new() {
                        Ok(c)  => c,
                        Err(e) => {
                            tracing::error!("screen capture unavailable: {e}");
                            capture_ok.store(false, Ordering::Relaxed);
                            return;
                        }
                    };
                    let mut enc = match Vp9Encoder::new(cap.width as u32, cap.height as u32, initial_bitrate, fps, kf_frames) {
                        Ok(e)  => e,
                        Err(e) => { tracing::warn!("encoder init failed: {e}"); return; }
                    };
                    tracing::debug!("capture thread started {}x{}", cap.width, cap.height);
                    let mut n               = 0u64;
                    let mut consec_errors   = 0u32;
                    let mut enc_w           = cap.width;
                    let mut enc_h           = cap.height;
                    let mut current_bitrate = initial_bitrate;
                    let mut last_abr        = Instant::now();
                    let frame_dur           = Duration::from_nanos(1_000_000_000 / fps as u64);
                    loop {
                        let t = std::time::Instant::now();
                        match cap.capture() {
                            Ok(Some(bgra)) => {
                                if consec_errors > 0 {
                                    tracing::info!("capture recovered after {consec_errors} errors");
                                    capture_ok.store(true, Ordering::Relaxed);
                                    consec_errors = 0;
                                }
                                let keyframe = n % kf_frames == 0
                                    || keyframe_req.swap(false, Ordering::Relaxed);
                                if let Some((data, pts)) = enc.encode(&bgra, keyframe) {
                                    if n == 0 { tracing::debug!("first encoded frame {} bytes", data.len()); }
                                    if keyframe { tracing::trace!("encode keyframe {n} pts={pts} → {} bytes", data.len()); }
                                    let _ = tx.send(Arc::new(EncodedFrame { data, pts, keyframe }));
                                } else if keyframe {
                                    tracing::warn!("encode returned None at frame {n}");
                                }
                                n += 1;

                                // ABR: adjust bitrate every 3 s based on viewer-reported loss.
                                if is_auto && last_abr.elapsed() >= Duration::from_secs(3) {
                                    let loss = *viewer_loss.lock().unwrap();
                                    let new_bitrate = if loss > 5.0 {
                                        (current_bitrate * 3 / 4).max(500)
                                    } else if loss < 1.0 {
                                        (current_bitrate * 13 / 10).min(15_000)
                                    } else {
                                        current_bitrate
                                    };
                                    if new_bitrate != current_bitrate {
                                        enc.set_bitrate(new_bitrate);
                                        current_bitrate = new_bitrate;
                                    }
                                    last_abr = Instant::now();
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                consec_errors += 1;
                                if consec_errors == 1 { tracing::warn!("capture error: {e}"); }
                                if consec_errors == 30 {
                                    tracing::debug!("reinitialising capturer after {consec_errors} errors");
                                    match ScreenCapture::new() {
                                        Ok(new_cap) => {
                                            let (nw, nh) = (new_cap.width, new_cap.height);
                                            cap = new_cap;
                                            if nw != enc_w || nh != enc_h {
                                                tracing::info!("resolution changed {enc_w}x{enc_h} → {nw}x{nh}, rebuilding encoder");
                                                match Vp9Encoder::new(nw as u32, nh as u32, current_bitrate, fps, kf_frames) {
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
                                            consec_errors = 0;
                                        }
                                    }
                                }
                            }
                        }
                        let elapsed = t.elapsed();
                        if elapsed < frame_dur { std::thread::sleep(frame_dur - elapsed); }
                    }
                });
            }

            // Transport task: send encoded frames and audio to peers; receive annotations.
            let (disconnect_tx, disconnect_rx) = mpsc::unbounded_channel::<Uuid>();
            let sticker_counts: Arc<Mutex<HashMap<Uuid, usize>>> = Arc::new(Mutex::new(HashMap::new()));
            {
                let transport       = Arc::clone(&transport);
                let annot_tx        = annot_tx;
                let disconnect_tx   = disconnect_tx;
                let mut frame_rx    = frame_tx.subscribe();
                let mut peer_hint_rx = peer_hint_rx;
                let mut audio_rx    = audio_rx;
                let keyframe_req    = Arc::clone(&keyframe_req);
                let sticker_tx      = sticker_tx;
                let sticker_counts  = Arc::clone(&sticker_counts);
                let viewer_loss     = Arc::clone(&viewer_loss);
                tokio::spawn(async move {
                    let mut peers:          HashMap<SocketAddr, PeerInfo> = HashMap::new();
                    let mut buf                                             = vec![0u8; 65_536];
                    let mut hint_done                                       = false;
                    let mut last_video_pts: u32                             = 0;
                    let mut last_audio_pts: u32                             = 0;
                    let mut reassembler    = StickerReassembler::default();
                    let mut sync_tick = tokio::time::interval(Duration::from_secs(1));
                    let mut nack_tick = tokio::time::interval(Duration::from_secs(2));
                    sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    nack_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                    loop {
                        tokio::select! {
                            hint = peer_hint_rx.recv(), if !hint_done => {
                                match hint {
                                    None => hint_done = true,
                                    Some(addr) => {
                                        tracing::debug!("signaling: viewer STUN addr {addr}, punching");
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
                                if let Some((src, pkt, _n)) = res {
                                    match pkt {
                                        Packet::Punch => {
                                            let is_new = !peers.contains_key(&src);
                                            if is_new && peers.len() >= MAX_PEERS {
                                                tracing::warn!("peer limit reached ({MAX_PEERS}), dropping punch from {src}");
                                            } else {
                                                let long_ago = Instant::now() - Duration::from_secs(10);
                                                let new_id = {
                                                    let entry = peers.entry(src).or_insert(PeerInfo {
                                                        last_seen:   Instant::now(),
                                                        viewer_id:   Uuid::new_v4(),
                                                        last_kf_req: long_ago,
                                                    });
                                                    entry.last_seen = Instant::now();
                                                    entry.viewer_id
                                                };
                                                if is_new {
                                                    tracing::info!("new peer {src} id={new_id} (total: {})", peers.len());
                                                }
                                                // Request a keyframe on the first punch and again
                                                // every 3 s until the viewer has one to decode.
                                                let peer = peers.get_mut(&src).unwrap();
                                                if peer.last_kf_req.elapsed() >= Duration::from_secs(3) {
                                                    keyframe_req.store(true, Ordering::Relaxed);
                                                    peer.last_kf_req = Instant::now();
                                                }
                                                let _ = transport.send_punch(src).await;
                                            }
                                        }
                                        Packet::Annot(json) => {
                                            if let Some(info) = peers.get_mut(&src) {
                                                info.last_seen = Instant::now();
                                            }
                                            if let Ok(msg) = serde_json::from_str::<AnnotMsg>(&json) {
                                                if let Some(peer) = peers.get(&src) {
                                                    if annot_tx.try_send((peer.viewer_id, msg)).is_err() {
                                                        tracing::warn!("annot channel full, dropping from {src}");
                                                    }
                                                }
                                            }
                                        }
                                        Packet::Disconnect => {
                                            if let Some(info) = peers.remove(&src) {
                                                tracing::info!("viewer {} disconnected cleanly", info.viewer_id);
                                                sticker_counts.lock().unwrap().remove(&info.viewer_id);
                                                reassembler.remove_by_owner(info.viewer_id);
                                                let _ = disconnect_tx.send(info.viewer_id);
                                            }
                                        }
                                        Packet::ImageChunk { sticker_id, total, idx, crc32, data } => {
                                            if let Some(peer) = peers.get(&src) {
                                                let viewer_id = peer.viewer_id;
                                                let result = reassembler.push_chunk(sticker_id, total, idx, crc32, data);
                                                handle_assemble(result, sticker_id, viewer_id, &sticker_tx, &mut sticker_counts.lock().unwrap());
                                            }
                                        }
                                        Packet::ImageManifest { sticker_id, total_chunks, pos_x, pos_y, size_w, size_h, sha256 } => {
                                            if let Some(peer) = peers.get(&src) {
                                                let viewer_id = peer.viewer_id;
                                                let count = sticker_counts.lock().unwrap().get(&viewer_id).copied().unwrap_or(0);
                                                if count >= MAX_STICKERS_PER_VIEWER {
                                                    tracing::warn!("viewer {viewer_id} at sticker limit, dropping sticker {sticker_id}");
                                                } else {
                                                    let result = reassembler.push_manifest(
                                                        sticker_id, total_chunks,
                                                        pos_x, pos_y, size_w, size_h,
                                                        sha256, viewer_id,
                                                    );
                                                    handle_assemble(result, sticker_id, viewer_id, &sticker_tx, &mut sticker_counts.lock().unwrap());
                                                }
                                            }
                                        }
                                        Packet::Ping { sent_ms } => {
                                            if peers.contains_key(&src) {
                                                let _ = transport.send_pong(src, sent_ms).await;
                                            }
                                        }
                                        Packet::Stats { loss_pct, ping_ms: _ } => {
                                            if peers.contains_key(&src) {
                                                *viewer_loss.lock().unwrap() = loss_pct;
                                                tracing::trace!("abr: viewer {src} loss={loss_pct:.1}%");
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }

                            res = frame_rx.recv() => {
                                match res {
                                    Ok(frame) => {
                                        let now    = Instant::now();
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
                                            for &addr in peers.keys() {
                                                let _ = transport.send_video(addr, frame.pts, &frame.data, frame.keyframe).await;
                                            }
                                            last_video_pts = frame.pts;
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                                    Err(broadcast::error::RecvError::Closed)    => break,
                                }
                            }

                            audio = recv_audio(&mut audio_rx) => {
                                if let Some((rtp_ts, data)) = audio {
                                    last_audio_pts = rtp_ts;
                                    for &addr in peers.keys() {
                                        let _ = transport.send_audio(addr, rtp_ts, &data).await;
                                    }
                                }
                            }

                            _ = sync_tick.tick() => {
                                if !peers.is_empty() {
                                    let ntp_ms = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis() as u64)
                                        .unwrap_or(0);
                                    for &addr in peers.keys() {
                                        let _ = transport.send_sync(addr, last_video_pts, last_audio_pts, ntp_ms).await;
                                    }
                                }
                            }

                            _ = nack_tick.tick() => {
                                for (sticker_id, owner, missing) in reassembler.collect_nacks() {
                                    if let Some((&addr, _)) = peers.iter().find(|(_, p)| p.viewer_id == owner) {
                                        let _ = transport.send_image_nack(addr, sticker_id, &missing).await;
                                    }
                                }
                            }
                        }
                    }
                });
            }

            let initial_code = room_code.clone();
            let live_code    = Arc::new(Mutex::new(room_code.clone()));
            let _ = tx.send(HostReady {
                transport, room_code, local_code, annot_rx, disconnect_rx,
                sticker_rx, sticker_counts, cursors, draws, capture_ok,
                live_code: Arc::clone(&live_code),
            });

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
                            }
                            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
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
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                });
            }
        });
        State::Discovering { rx }
    }

    fn begin_join(&mut self, code: RoomCode) -> tokio::sync::oneshot::Receiver<JoinReady> {
        let (tx, rx) = tokio::sync::oneshot::channel::<JoinReady>();
        self.rt.spawn(async move {
            let transport = match Transport::bind_ephemeral().await {
                Ok(t)  => Arc::new(t),
                Err(e) => { tracing::error!("UDP bind: {e}"); return; }
            };

            let mut my_stun:     Option<SocketAddr> = None;
            let mut nat_warning: Option<String>     = None;
            let host_addr = match code {
                RoomCode::Direct(addr) => addr,
                RoomCode::Signaling(short_code) => {
                    let my_udp = transport.public_addr().await.unwrap_or_else(|| {
                        let ip = crate::transport::discover_lan_ip()
                            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
                        SocketAddr::new(ip, transport.socket.local_addr().map(|a| a.port()).unwrap_or(0))
                    });
                    my_stun = Some(my_udp);
                    nat_warning = crate::transport::diagnose_nat(&transport.socket).await;
                    if let Some(ref w) = nat_warning { tracing::warn!("NAT diagnosis: {w}"); }
                    let body   = serde_json::json!({ "udp": my_udp.to_string() });
                    let server = match server_url() {
                        Some(s) => s,
                        None    => { tracing::error!("BACKSEAT_SERVER not set"); return; }
                    };
                    match reqwest::Client::new()
                        .post(format!("{server}/room/{short_code}/join"))
                        .json(&body)
                        .send().await
                    {
                        Ok(resp) if resp.status().is_success() => {
                            if let Ok(v) = resp.json::<serde_json::Value>().await {
                                match v["host"].as_str().and_then(|s| s.parse::<SocketAddr>().ok()) {
                                    Some(addr) => { tracing::info!("signaling: host is at {addr}"); addr }
                                    None       => { tracing::error!("bad host addr from server"); return; }
                                }
                            } else { tracing::error!("bad json from server"); return; }
                        }
                        Ok(resp) => { tracing::error!("server returned {}", resp.status()); return; }
                        Err(e)   => { tracing::error!("signaling request failed: {e}"); return; }
                    }
                }
            };

            tracing::debug!("viewer local={:?} STUN={my_stun:?} host={host_addr}",
                transport.socket.local_addr());

            for i in 0..5 {
                match transport.send_punch(host_addr).await {
                    Ok(()) => tracing::trace!("punch {i} → {host_addr} ok"),
                    Err(e) => tracing::warn!("punch {i} → {host_addr} failed: {e}"),
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            let (frame_sync_tx, frame_sync_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
            let (rgba_tx, rgba_rx)             = mpsc::unbounded_channel::<RgbaFrame>();
            let (annot_out_tx, mut annot_out_rx) = mpsc::unbounded_channel::<String>();
            // (sticker_id, encoded_image_bytes) — viewer queues uploads here
            let (image_out_tx, mut image_out_rx) = mpsc::unbounded_channel::<(u64, Vec<u8>)>();
            let viewer_id = Uuid::new_v4();

            // Audio player (best-effort — carry on without audio on failure).
            // AudioPlayer is !Send (cpal::Stream), so keep it alive on a dedicated
            // std::thread and only pass the Send channel into the async task.
            let audio_pkt_tx: Option<mpsc::UnboundedSender<(u32, Vec<u8>)>> =
                match AudioPlayer::new() {
                    Ok((player, sender)) => { keepalive(player); Some(sender) }
                    Err(e) => { tracing::warn!("audio player unavailable: {e}"); None }
                };

            // VP9 decode thread.
            std::thread::spawn(move || {
                let mut dec = match Vp9Decoder::new() {
                    Ok(d)  => d,
                    Err(e) => { tracing::error!("decoder init: {e}"); return; }
                };
                tracing::debug!("decode thread started");
                let mut decoded_count = 0u64;
                while let Ok(data) = frame_sync_rx.recv() {
                    tracing::trace!("decode thread got {} bytes", data.len());
                    if let Some((w, h, pixels)) = dec.decode(&data) {
                        if decoded_count == 0 { tracing::debug!("first decoded frame {w}x{h}"); }
                        decoded_count += 1;
                        let _ = rgba_tx.send(RgbaFrame { width: w, height: h, data: pixels });
                    } else {
                        tracing::warn!("decode returned None for {} bytes", data.len());
                    }
                }
                tracing::debug!("decode thread exiting");
            });

            // Transport task.
            let stats: Arc<Mutex<ConnectionStats>> = Arc::new(Mutex::new(ConnectionStats::default()));
            {
                let transport = Arc::clone(&transport);
                let stats     = Arc::clone(&stats);
                tokio::spawn(async move {
                    let mut reassembler    = Reassembler::new();
                    let mut buf            = vec![0u8; 65_536];
                    let mut actual_host:   Option<std::net::SocketAddr> = None;
                    let mut pending_images: HashMap<u64, Vec<u8>>       = HashMap::new();
                    let mut punch_ticker     = tokio::time::interval(Duration::from_millis(500));
                    let mut stats_tick       = tokio::time::interval(Duration::from_secs(1));
                    let mut ping_tick        = tokio::time::interval(Duration::from_secs(2));
                    let mut stats_send_tick  = tokio::time::interval(Duration::from_secs(2));
                    punch_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    stats_send_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    let mut last_video_seq:   Option<u16>              = None;
                    let mut rx_bytes_window:  u64                      = 0;
                    let mut tx_bytes_window:  u64                      = 0;
                    let mut rx_frags_window:  u64                      = 0;
                    let mut lost_frags_window: u64                     = 0;
                    let mut ping_sent_at: Option<std::time::Instant>   = None;
                    let mut got_keyframe = false;

                    loop {
                        tokio::select! {
                            _ = punch_ticker.tick() => {
                                match transport.send_punch(host_addr).await {
                                    Ok(()) => tracing::trace!("periodic punch → {host_addr}"),
                                    Err(e) => tracing::warn!("periodic punch failed: {e}"),
                                }
                                tx_bytes_window += 1;
                            }

                            _ = ping_tick.tick() => {
                                let target = actual_host.unwrap_or(host_addr);
                                let sent_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);
                                let _ = transport.send_ping(target, sent_ms).await;
                                tx_bytes_window += 9;
                                ping_sent_at = Some(std::time::Instant::now());
                            }

                            _ = stats_send_tick.tick() => {
                                let target = actual_host.unwrap_or(host_addr);
                                let (loss, ping) = if let Ok(s) = stats.lock() {
                                    (s.loss_pct, s.ping_ms.unwrap_or(0.0))
                                } else {
                                    (0.0, 0.0)
                                };
                                let _ = transport.send_stats(target, loss, ping).await;
                                tx_bytes_window += 9;
                            }

                            _ = stats_tick.tick() => {
                                let total_frags = rx_frags_window + lost_frags_window;
                                if let Ok(mut s) = stats.lock() {
                                    s.rx_bps     = rx_bytes_window as f32;
                                    s.tx_bps     = tx_bytes_window as f32;
                                    s.loss_pct   = if total_frags > 0 {
                                        lost_frags_window as f32 / total_frags as f32 * 100.0
                                    } else {
                                        0.0
                                    };
                                }
                                rx_bytes_window   = 0;
                                tx_bytes_window   = 0;
                                rx_frags_window   = 0;
                                lost_frags_window = 0;
                            }

                            res = transport.recv(&mut buf) => {
                                if let Some((src, pkt, n)) = res {
                                    rx_bytes_window += n as u64;
                                    let from_host = match actual_host {
                                        Some(h) => src == h,
                                        None    => src == host_addr,
                                    };
                                    match pkt {
                                        Packet::Punch => {
                                            if actual_host.is_none() {
                                                tracing::debug!("punch-back from {src} — locking as actual_host");
                                                actual_host = Some(src);
                                            }
                                        }
                                        Packet::VideoFrag { rtp_ts, seq, frag_idx, frag_total, keyframe, data } => {
                                            if !from_host {
                                                tracing::warn!("video from unexpected {src} (expected {host_addr} / {actual_host:?}) — dropping");
                                            } else {
                                                if actual_host.is_none() {
                                                    tracing::debug!("host video from {src} — locking as actual_host");
                                                    actual_host = Some(src);
                                                }
                                                if let Some(last) = last_video_seq {
                                                    let gap = seq.wrapping_sub(last).saturating_sub(1);
                                                    lost_frags_window += gap as u64;
                                                    rx_frags_window   += 1 + gap as u64;
                                                }
                                                last_video_seq = Some(seq);
                                                tracing::trace!("rx frag rtp_ts={rtp_ts} {frag_idx}/{frag_total} kf={keyframe}");
                                                if let Some((frame, is_kf)) = reassembler.push(rtp_ts, frag_idx, frag_total, keyframe, data) {
                                                    if is_kf { got_keyframe = true; }
                                                    if got_keyframe {
                                                        tracing::trace!("reassembled frame rtp_ts={rtp_ts} ({} bytes)", frame.len());
                                                        if frame_sync_tx.try_send(frame).is_err() {
                                                            tracing::debug!("decode thread busy — dropped frame rtp_ts={rtp_ts}");
                                                        }
                                                    } else {
                                                        tracing::debug!("dropping P-frame rtp_ts={rtp_ts} before first keyframe");
                                                    }
                                                }
                                            }
                                        }
                                        Packet::Audio { rtp_ts, data, .. } => {
                                            if from_host {
                                                if let Some(ref tx) = audio_pkt_tx {
                                                    let _ = tx.send((rtp_ts, data));
                                                }
                                            }
                                        }
                                        Packet::Sync { video_ts, audio_ts, ntp_ms } => {
                                            if from_host {
                                                tracing::trace!(
                                                    "a/v sync anchor: video_ts={video_ts} audio_ts={audio_ts} ntp_ms={ntp_ms}"
                                                );
                                            }
                                        }
                                        Packet::Pong { sent_ms } => {
                                            if from_host {
                                                if let Some(sent) = ping_sent_at.take() {
                                                    let rtt_ms = sent.elapsed().as_secs_f32() * 1000.0;
                                                    tracing::trace!("pong sent_ms={sent_ms} rtt={rtt_ms:.1}ms");
                                                    if let Ok(mut s) = stats.lock() {
                                                        s.ping_ms = Some(rtt_ms);
                                                    }
                                                }
                                            }
                                        }
                                        Packet::ImageNack { sticker_id, missing } => {
                                            if from_host {
                                                if let Some(bytes) = pending_images.get(&sticker_id) {
                                                    let target = actual_host.unwrap_or(host_addr);
                                                    let chunks: Vec<&[u8]> = bytes.chunks(1_200).collect();
                                                    for idx in missing {
                                                        if let Some(chunk) = chunks.get(idx as usize) {
                                                            let crc = crc32fast::hash(chunk);
                                                            let mut pkt = Vec::with_capacity(17 + chunk.len());
                                                            pkt.push(0x07u8);
                                                            pkt.extend_from_slice(&sticker_id.to_be_bytes());
                                                            pkt.extend_from_slice(&(chunks.len() as u16).to_be_bytes());
                                                            pkt.extend_from_slice(&idx.to_be_bytes());
                                                            pkt.extend_from_slice(&crc.to_be_bytes());
                                                            pkt.extend_from_slice(chunk);
                                                            let _ = transport.socket.send_to(&pkt, target).await;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }

                            Some(json) = annot_out_rx.recv() => {
                                let target = actual_host.unwrap_or(host_addr);
                                tx_bytes_window += 1 + json.len() as u64;
                                let _ = transport.send_annot(target, &json).await;
                            }

                            Some((sticker_id, bytes)) = image_out_rx.recv() => {
                                let target = actual_host.unwrap_or(host_addr);
                                // Store bytes for possible retransmit on NACK.
                                pending_images.insert(sticker_id, bytes.clone());
                                let chunks: Vec<&[u8]> = bytes.chunks(1_200).collect();
                                let total = chunks.len() as u16;
                                use sha2::{Sha256, Digest};
                                let sha256: [u8; 32] = {
                                    let mut h = Sha256::new(); h.update(&bytes); h.finalize().into()
                                };
                                // 59 bytes for manifest + approx payload for chunks
                                tx_bytes_window += 59 + bytes.len() as u64;
                                // Initial placement: centre of screen, quarter width.
                                let _ = transport.send_image_manifest(
                                    target, sticker_id, total,
                                    0.375, 0.375, 0.25, 0.25, &sha256,
                                ).await;
                                let _ = transport.send_image_chunks(target, sticker_id, &bytes).await;
                            }
                        }
                    }
                });
            }

            let _ = tx.send(JoinReady {
                transport, rgba_rx,
                annot_out: annot_out_tx,
                image_out: image_out_tx,
                viewer_id, host_addr, nat_warning,
                stats,
            });
        });
        rx
    }

    fn finish_join(&self, ctx: &egui::Context, ready: JoinReady, name: String) -> State {
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));

        let display_name = if name.trim().is_empty() {
            format!("viewer-{}", &ready.viewer_id.to_string()[..4])
        } else {
            name.trim().to_string()
        };

        let register = AnnotMsg::Register { name: display_name.clone() };
        let _ = ready.annot_out.send(serde_json::to_string(&register).unwrap());

        let cursors = Arc::new(Mutex::new(CursorState::default()));
        let draws   = Arc::new(Mutex::new(DrawLayer::default()));
        cursors.lock().unwrap().add_user(UserInfo {
            id:    UserId(ready.viewer_id),
            name:  display_name.clone(),
            color: UserColor("#5c9ee0".into()),
        });

        State::Joining(JoinCtx {
            _transport:       ready.transport,
            rgba_rx:          ready.rgba_rx,
            annot_out:        ready.annot_out,
            image_out:        ready.image_out,
            cursors,
            draws,
            stickers:                ViewerStickerLayer::default(),
            sticker_textures:        std::collections::HashMap::new(),
            selected_sticker:        None,
            sticker_dragging_resize: false,
            upload_rx:               None,
            texture:          None,
            viewer_id:        ready.viewer_id,
            host_addr:        ready.host_addr,
            active_stroke:    None,
            nat_warning:      ready.nat_warning,
            tool:             ToolState::default(),
            name:             display_name,
            show_stats:       false,
            stats:            ready.stats,
        })
    }
}

// ── Sticker assembly helper (called from transport task) ──────────────────────

fn handle_assemble(
    result: AssembleResult,
    sticker_id: u64,
    viewer_id: Uuid,
    sticker_tx: &mpsc::UnboundedSender<(Uuid, u64, HostSticker)>,
    sticker_counts: &mut HashMap<Uuid, usize>,
) {
    match result {
        AssembleResult::Complete(sticker) => {
            *sticker_counts.entry(viewer_id).or_insert(0) += 1;
            let _ = sticker_tx.send((viewer_id, sticker_id, sticker));
        }
        AssembleResult::Corrupt => {
            tracing::warn!("sticker {sticker_id} from {viewer_id} corrupt after assembly");
        }
        AssembleResult::Rejected => {
            tracing::warn!("sticker {sticker_id} from {viewer_id} rejected");
        }
        AssembleResult::Pending => {}
    }
}

// ── Annotation application (host side) ───────────────────────────────────────

fn apply_annot(
    src_id: Uuid, msg: &AnnotMsg,
    cursors: &Arc<Mutex<CursorState>>, draws: &Arc<Mutex<DrawLayer>>,
    stickers: &mut std::collections::HashMap<u64, HostStickerEntry>,
    sticker_counts: &Arc<Mutex<HashMap<Uuid, usize>>>,
    _ctx: &egui::Context,
) {
    match msg {
        AnnotMsg::Register { name } => {
            let name = name.chars().take(64).collect::<String>();
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
        AnnotMsg::CursorMove { pos } => {
            let mut c = cursors.lock().unwrap();
            if !c.users.contains_key(&src_id) {
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
        AnnotMsg::StrokeBegin { stroke_id, pos, width, color, alpha } => {
            let color = color.chars().take(7).collect::<String>();
            let width = width.clamp(0.5, 50.0);
            draws.lock().unwrap().begin_stroke(UserId(src_id), *stroke_id, *pos, width, color, *alpha);
        }
        AnnotMsg::StrokePoint { stroke_id, pos } => {
            draws.lock().unwrap().add_point(UserId(src_id), *stroke_id, *pos);
        }
        AnnotMsg::StrokeEnd { stroke_id } => {
            draws.lock().unwrap().end_stroke(*stroke_id);
        }
        AnnotMsg::EraseStroke { stroke_id } => {
            draws.lock().unwrap().remove_stroke_if_owned(*stroke_id, src_id);
        }
        AnnotMsg::ClearAll => {
            draws.lock().unwrap().remove_user_strokes(src_id);
        }
        AnnotMsg::StickerPlace { sticker_id, pos, size } => {
            if let Some(entry) = stickers.get_mut(sticker_id) {
                if entry.owner == src_id {
                    entry.pos  = *pos;
                    entry.size = *size;
                }
            }
        }
        AnnotMsg::StickerMove { sticker_id, pos, size } => {
            if let Some(entry) = stickers.get_mut(sticker_id) {
                if entry.owner == src_id {
                    entry.pos  = *pos;
                    entry.size = *size;
                }
            }
        }
        AnnotMsg::StickerRemove { sticker_id } => {
            if stickers.get(sticker_id).map_or(false, |e| e.owner == src_id) {
                stickers.remove(sticker_id);
                let mut counts = sticker_counts.lock().unwrap();
                let n = counts.entry(src_id).or_insert(0);
                *n = n.saturating_sub(1);
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

const SERVER_URL: &str = "https://backseat.fly.dev";

fn server_url() -> Option<String> {
    let url = std::env::var("BACKSEAT_SERVER").unwrap_or_else(|_| SERVER_URL.to_string());
    if url.is_empty() { None } else { Some(url) }
}

/// Open a native file picker, decode the chosen image, resize to 960×540 ceiling,
/// encode as PNG (has alpha) or JPEG (opaque), and return the upload payload.
async fn pick_and_process_image() -> Option<UploadedImage> {
    use image::imageops::FilterType;

    let handle = rfd::AsyncFileDialog::new()
        .add_filter("Image", &["png", "jpg", "jpeg", "webp", "bmp", "gif"])
        .set_title("Choose a sticker image")
        .pick_file()
        .await?;

    let raw = handle.read().await;
    let mut img = image::load_from_memory(&raw).ok()?;

    // Resize to fit within 960×540 if needed.
    if img.width() > 960 || img.height() > 540 {
        let scale = (960.0 / img.width() as f32).min(540.0 / img.height() as f32);
        let nw = (img.width()  as f32 * scale) as u32;
        let nh = (img.height() as f32 * scale) as u32;
        img = img.resize(nw, nh, FilterType::Lanczos3);
    }

    let mut buf = Vec::new();
    let has_alpha = img.color().has_alpha();
    if has_alpha {
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png).ok()?;
    } else {
        {
            let mut cursor = std::io::Cursor::new(&mut buf);
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 85);
            enc.encode_image(&img).ok()?;
        }
    }

    let sticker_id: u64 = rand_u64();
    // Initial placement: centred, 25% of screen width.
    let pos  = crate::types::NormPoint { x: 0.375, y: 0.375 };
    let size = crate::types::NormPoint { x: 0.25,  y: 0.25  };
    Some(UploadedImage { sticker_id, bytes: buf, pos, size })
}

fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    // XorShift-based one-shot to avoid pulling in a rand crate.
    let mut x = t as u64 ^ 0xDEAD_BEEF_CAFE_1234;
    x ^= x << 13; x ^= x >> 7; x ^= x << 17; x
}

/// Drive an optional audio receiver without spinning: returns `None` forever when absent.
async fn recv_audio(
    rx: &mut Option<mpsc::UnboundedReceiver<(u32, Vec<u8>)>>,
) -> Option<(u32, Vec<u8>)> {
    match rx {
        Some(rx) => rx.recv().await,
        None     => std::future::pending().await,
    }
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

#[cfg(target_os = "linux")]
fn x11_set_notification_type(window_id: u32) {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;

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
        Ok(c)  => c,
        Err(e) => { tracing::warn!("x11 overlay type: change_property failed: {e}"); return; }
    };
    if let Err(e) = cookie.check() {
        tracing::warn!("x11 overlay type: change_property check failed: {e}");
        return;
    }

    let _ = conn.flush();
    tracing::debug!("x11 overlay type: set _NET_WM_WINDOW_TYPE_NOTIFICATION on window {window_id}");
}
