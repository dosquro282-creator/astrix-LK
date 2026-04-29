use astrix_client::app;
use astrix_client::deep_links;
use eframe::egui;

#[cfg(target_os = "windows")]
#[cfg(not(debug_assertions))]
fn hide_console_window() {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, ShowWindow, SW_HIDE};

    unsafe {
        let window_name: Vec<u16> = "ConsoleWindowClass\0".encode_utf16().collect();
        if let Ok(hwnd) = FindWindowW(None, windows::core::PCWSTR(window_name.as_ptr())) {
            if hwnd != HWND::default() {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

#[cfg(target_os = "windows")]
#[cfg(debug_assertions)]
fn hide_console_window() {
    // No-op in debug builds
}

#[tokio::main]
async fn main() -> eframe::Result<()> {
    // Initialize console logging
    astrix_client::console_panel::log("Astrix client starting...");

    // Hide console window in release builds on Windows
    #[cfg(target_os = "windows")]
    hide_console_window();

    astrix_client::console_panel::log("Registering protocol handler...");
    deep_links::register_protocol_handler();
    let invite_token = deep_links::extract_invite_token_from_args();

    // ASTRIX_VSYNC=0 disables vertical sync, allowing render FPS above monitor refresh rate.
    // Default: vsync OFF for streaming (avoids capping render FPS at 60-72 Hz).
    // ASTRIX_VSYNC=1 re-enables vsync (lower CPU/GPU usage, prevents tearing).
    let vsync = std::env::var("ASTRIX_VSYNC")
        .map(|v| v == "1")
        .unwrap_or(false);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(egui::vec2(1200.0, 800.0))
            .with_min_inner_size(egui::vec2(800.0, 600.0)),
        vsync,
        ..Default::default()
    };

    eframe::run_native(
        "Astrix",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::AstrixApp::new(cc, invite_token.clone())))),
    )
}
