#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod opus;
mod types;
mod capture;
mod cursor;
mod decoder;
mod draw_layer;
mod sticker_layer;
mod encoder;
mod renderer;
mod transport;
mod tray;
mod vpx;

fn main() {
    dotenvy::dotenv().ok();

    let _guard = init_logging();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_transparent(true)
            .with_decorations(false)
            .with_mouse_passthrough(false) // interactive until host mode activated
            .with_inner_size(egui::vec2(320.0, 240.0)),
        ..Default::default()
    };

    eframe::run_native(
        "backseat",
        native_options,
        Box::new(|cc| Box::new(app::OverlayApp::new(cc))),
    )
    .unwrap();
}

#[cfg(debug_assertions)]
fn init_logging() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "overlay=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}

#[cfg(not(debug_assertions))]
fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    let log_dir = release_log_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let appender = tracing_appender::rolling::never(&log_dir, "backseat.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "overlay=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(writer).with_ansi(false))
        .init();
    guard
}

#[cfg(not(debug_assertions))]
fn release_log_dir() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .map(|p| std::path::PathBuf::from(p).join("backseat"))
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOME")
            .map(|p| std::path::PathBuf::from(p).join(".local/share/backseat"))
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
    }
}
