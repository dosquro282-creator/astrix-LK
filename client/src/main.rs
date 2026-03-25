use astrix_client::app;
use eframe::egui;

#[tokio::main]
async fn main() -> eframe::Result<()> {
    // ASTRIX_VSYNC=0 disables vertical sync, allowing render FPS above monitor refresh rate.
    // Default: vsync OFF for streaming (avoids capping render FPS at 60-72 Hz).
    // ASTRIX_VSYNC=1 re-enables vsync (lower CPU/GPU usage, prevents tearing).
    let vsync = std::env::var("ASTRIX_VSYNC")
        .map(|v| v == "1")
        .unwrap_or(false);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size(egui::vec2(1200.0, 800.0)),
        vsync,
        ..Default::default()
    };

    eframe::run_native(
        "Astrix",
        native_options,
        Box::new(|cc| Ok(Box::new(app::AstrixApp::new(cc)))),
    )
}

