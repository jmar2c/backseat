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

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn main() {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "overlay=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_transparent(true)
            .with_always_on_top()
            .with_decorations(false)
            .with_fullscreen(true)
            .with_mouse_passthrough(false), // interactive until host mode activated
        ..Default::default()
    };

    eframe::run_native(
        "backseat",
        native_options,
        Box::new(|cc| Box::new(app::OverlayApp::new(cc))),
    )
    .unwrap();
}
