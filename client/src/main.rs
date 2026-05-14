use astrix_client::app;
use astrix_client::deep_links;
use astrix_client::voice::{spawn_voice_engine, ScreenPreset, StreamSourceTarget, VoiceCmd};
use eframe::egui;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

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

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on"))
        .unwrap_or(false)
}

fn parse_headless_preset() -> ScreenPreset {
    match std::env::var("ASTRIX_SCREEN_PRESET")
        .unwrap_or_else(|_| "P1080F60".to_string())
        .to_ascii_uppercase()
        .as_str()
    {
        "P720F30" | "720P30" => ScreenPreset::P720F30,
        "P720F60" | "720P60" => ScreenPreset::P720F60,
        "P720F120" | "720P120" => ScreenPreset::P720F120,
        "P1080F30" | "1080P30" => ScreenPreset::P1080F30,
        "P1080F120" | "1080P120" => ScreenPreset::P1080F120,
        "P1440F30" | "1440P30" => ScreenPreset::P1440F30,
        "P1440F60" | "1440P60" => ScreenPreset::P1440F60,
        "P1440F90" | "1440P90" => ScreenPreset::P1440F90,
        _ => ScreenPreset::P1080F60,
    }
}

fn parse_headless_source() -> StreamSourceTarget {
    let raw = std::env::var("ASTRIX_SCREEN_SOURCE").unwrap_or_else(|_| "monitor:0".to_string());
    let parts: Vec<&str> = raw.split(':').collect();
    match parts.as_slice() {
        ["window", hwnd, pid] => StreamSourceTarget::Window {
            window_id: hwnd.parse().unwrap_or(0),
            process_id: pid.parse().unwrap_or(0),
        },
        ["monitor", index] => StreamSourceTarget::Monitor {
            index: index.parse().unwrap_or(0),
        },
        [index] => StreamSourceTarget::Monitor {
            index: index.parse().unwrap_or(0),
        },
        _ => StreamSourceTarget::Monitor { index: 0 },
    }
}

async fn run_headless_screen_share_from_env() -> bool {
    if !env_truthy("ASTRIX_HEADLESS_SCREEN_SHARE") {
        return false;
    }

    let livekit_url = match std::env::var("ASTRIX_LIVEKIT_URL") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("[headless] ASTRIX_LIVEKIT_URL is required");
            return true;
        }
    };
    let livekit_token = match std::env::var("ASTRIX_LIVEKIT_TOKEN") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("[headless] ASTRIX_LIVEKIT_TOKEN is required");
            return true;
        }
    };
    let my_user_id = std::env::var("ASTRIX_USER_ID")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let source = parse_headless_source();
    let preset = parse_headless_preset();
    eprintln!(
        "[headless] starting screen share source={:?} preset={:?}; heartbeat log via ASTRIX_SCREEN_HEARTBEAT_LOG",
        source, preset
    );

    let (tx, _frames, done) = spawn_voice_engine(tokio::runtime::Handle::current());
    let speaking = Arc::new(Mutex::new(HashMap::new()));
    let session_stats = Arc::new(Mutex::new(
        astrix_client::voice::VoiceSessionStats::default(),
    ));
    let _ = tx.send(VoiceCmd::Start {
        livekit_url,
        livekit_token,
        channel_id: 0,
        server_id: 0,
        api_base: String::new(),
        my_user_id,
        speaking,
        session_stats,
        receiver_telemetry: None,
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let _ = tx.send(VoiceCmd::StartScreen { source, preset });
    eprintln!("[headless] screen share running; press Ctrl+C to stop");
    let _ = tokio::signal::ctrl_c().await;
    let _ = tx.send(VoiceCmd::StopScreen);
    let _ = tx.send(VoiceCmd::Stop);
    let _ = done.recv_timeout(std::time::Duration::from_secs(3));
    true
}

#[tokio::main]
async fn main() -> eframe::Result<()> {
    // Initialize console logging
    astrix_client::console_panel::init();
    astrix_client::console_panel::log("Astrix client starting...");

    if run_headless_screen_share_from_env().await {
        return Ok(());
    }

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
