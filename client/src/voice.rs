//! Voice engine: LiveKit (Phase 2).
//!
//! Use VoiceCmd::Start with livekit_url + livekit_token; spawn_voice_engine spawns
//! a tokio task that runs the LiveKit session (see crate::voice_livekit).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

// ─── VideoFrame / shared video output ────────────────────────────────────────

/// A single decoded video frame in RGBA format.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// Packed RGBA, row-major, `width * height * 4` bytes.
    /// Empty when `shared_handle` is Some (GPU zero-copy path is used).
    pub rgba: Vec<u8>,
    /// Phase 3.5: Win32 HANDLE from IDXGIResource::GetSharedHandle (as usize).
    /// When Some, `rgba` is empty — use WGL_NV_DX_interop2 path for zero CPU readback.
    pub shared_handle: Option<usize>,
}

/// Shared map: key → latest decoded video frame.
/// Key: positive = user_id (camera), negative = -(user_id + 1) (screen stream).
pub type VideoFrames = Arc<Mutex<HashMap<i64, VideoFrame>>>;

/// Key for camera: user_id. Key for screen stream: -(user_id + 1).
#[inline]
pub fn video_frame_key(user_id: i64, is_screen: bool) -> i64 {
    if is_screen {
        -(user_id + 1)
    } else {
        user_id
    }
}

pub fn video_preview_frame_key(user_id: i64) -> i64 {
    i64::MIN + user_id
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StreamSourceTarget {
    Monitor { index: usize },
    Window { window_id: u32, process_id: u32 },
}

#[derive(Clone, Debug)]
pub struct StreamWindowInfo {
    pub window_id: u32,
    pub process_id: u32,
    pub app_name: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

// ─── Screen share quality presets ─────────────────────────────────────────────

/// Quality preset for screen sharing. Encodes resolution and framerate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenPreset {
    P720F30,
    P720F60,
    P720F120,
    P1080F30,
    P1080F60,
    P1080F120,
    P1440F30,
    P1440F60,
    P1440F90,
}

impl ScreenPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::P720F30   => "720p  30 к/с",
            Self::P720F60   => "720p  60 к/с",
            Self::P720F120  => "720p  120 к/с",
            Self::P1080F30  => "1080p 30 к/с",
            Self::P1080F60  => "1080p 60 к/с",
            Self::P1080F120 => "1080p 120 к/с",
            Self::P1440F30  => "1440p 30 к/с",
            Self::P1440F60  => "1440p 60 к/с",
            Self::P1440F90  => "1440p 90 к/с",
        }
    }

    /// (width, height, max_fps, max_bitrate_bps)
    ///
    /// Bitrates sized for hardware encoders (NVENC/AMF) with rate control (CBR/LowDelayVBR).
    /// Lower than OpenH264 Baseline — hardware High profile is more efficient.
    /// Too-tight budget makes the RC skip frames, causing stutter.
    pub fn params(self) -> (u32, u32, f64, u64) {
        match self {
            Self::P720F30   => (1280, 720,  30.0, 5_000_000),
            Self::P720F60   => (1280, 720,  60.0, 8_000_000),
            Self::P720F120  => (1280, 720,  120.0, 16_000_000),
            Self::P1080F30  => (1920, 1080, 30.0, 8_000_000),
            Self::P1080F60  => (1920, 1080, 60.0, 12_000_000),
            Self::P1080F120 => (1920, 1080, 120.0, 20_000_000),
            Self::P1440F30  => (2560, 1440, 30.0, 12_000_000),
            Self::P1440F60  => (2560, 1440, 60.0, 18_000_000),
            // 24 Mbps is too tight for 1440p90, but 60 Mbps creates heavy
            // bursts on periodic IDRs and can destabilize the subscriber path.
            // Keep this roomy, but below the point where receiver jitter grows.
            Self::P1440F90  => (2560, 1440, 90.0, 35_000_000),
        }
    }

    /// Effective (width, height, fps, bitrate) when using CPU encoder.
    /// CPU path caps 60 fps presets to 30 fps to avoid overloading OpenH264;
    /// 90/120 fps presets cap to 60 fps. Bitrates are still generous for Baseline profile.
    pub fn effective_params_for_cpu(self) -> (u32, u32, f64, u64) {
        match self {
            Self::P720F60   => (1280, 720,  30.0, 10_000_000),
            Self::P1080F60  => (1920, 1080, 30.0, 20_000_000),
            Self::P720F120  => (1280, 720,  60.0, 20_000_000),
            Self::P1080F120 => (1920, 1080, 60.0, 35_000_000),
            Self::P1440F90  => (2560, 1440, 60.0, 50_000_000),
            _ => self.params(),
        }
    }

    /// Whether this preset would use simulcast (two layers: full + low-res fallback).
    /// Currently unused: screen share has simulcast disabled for stability (single-layer H264).
    /// Re-enable only after tuning layer bitrates so LOW layer is not 250 kbps.
    #[allow(dead_code)]
    pub fn use_simulcast(self) -> bool {
        matches!(
            self,
            Self::P720F60 | Self::P720F120 | Self::P1080F60 | Self::P1080F120
                | Self::P1440F60 | Self::P1440F90
        )
    }

    pub const ALL: &'static [Self] = &[
        Self::P720F30,
        Self::P720F60,
        Self::P720F120,
        Self::P1080F30,
        Self::P1080F60,
        Self::P1080F120,
        Self::P1440F30,
        Self::P1440F60,
        Self::P1440F90,
    ];
}

impl Default for ScreenPreset {
    fn default() -> Self {
        Self::P1080F30
    }
}

// ─── Session statistics (shared with UI; updated by engine) ─────────────────────

/// Encoding path for outgoing screen share (Phase 5).
#[derive(Debug, Clone)]
pub enum EncodingPath {
    /// GPU, direct D3D11 + NVIDIA NVENC path.
    NvencD3d11 { adapter: String },
    /// GPU, hardware MFT (e.g. NVIDIA NVENC, AMD AMF).
    MftHardware { adapter: String },
    /// CPU, software MFT (e.g. H264 Encoder MFT).
    MftSoftware,
    /// OpenH264 (I420 → libwebrtc). gpu_capture = D3D11 RGBA→I420 on GPU.
    OpenH264 { threads: u32, gpu_capture: bool },
}

impl EncodingPath {
    pub fn to_display_string(&self) -> String {
        match self {
            Self::NvencD3d11 { adapter } => format!("NVENC D3D11 ({}, hardware)", adapter.trim_end_matches('\0')),
            Self::MftHardware { adapter } => format!("MFT GPU ({}, hardware)", adapter.trim_end_matches('\0')),
            Self::MftSoftware => "MFT software".into(),
            Self::OpenH264 { threads, gpu_capture } => {
                let capture = if *gpu_capture { "GPU capture" } else { "CPU capture" };
                format!("OpenH264 ({} threads, {})", threads, capture)
            }
        }
    }
}

/// Statistics for the current voice/screen session (publisher = who started the stream).
/// Written by the voice engine, read by the UI in the statistics window.
#[derive(Debug, Clone, Default)]
pub struct VoiceSessionStats {
    /// Round-trip time, ms (when available from LiveKit/WebRTC).
    pub latency_rtt_ms: Option<f32>,
    /// Actual stream FPS (frames published per second).
    pub stream_fps: Option<f32>,
    /// Published resolution (width, height).
    pub resolution: Option<(u32, u32)>,
    /// Frames per second (same as stream_fps for screen share).
    pub frames_per_second: Option<f32>,
    /// Upload speed (отдача), Mbit/s. Target bitrate when real stats unavailable.
    pub connection_speed_mbps: Option<f32>,
    /// Incoming stream speed (приём) when watching someone's stream, Mbit/s. Estimated from received frames.
    pub incoming_speed_mbps: Option<f32>,
    /// Encoding path for outgoing video: "CPU" or "GPU" (screen share). None when not streaming.
    pub encoding_path: Option<String>,
    /// Decoding path for incoming video: "CPU" or "GPU". None when no video received yet.
    pub decoding_path: Option<String>,
    /// Number of H.264 encoder threads (CPU path, Windows only). None when GPU or not streaming.
    pub encoder_threads: Option<u32>,
    /// Number of H.264 decoder threads (viewer, CPU path, Windows only). None when GPU or no video.
    pub decoder_threads: Option<u32>,
}

/// Returns the number of H.264 decoder threads used by libwebrtc for the given resolution.
/// Matches the logic from h264_decoder_multithread_windows.patch (Windows only).
#[cfg(target_os = "windows")]
pub fn decoder_threads_for_resolution(width: u32, height: u32) -> u32 {
    let pixels = (width as u64) * (height as u64);
    let cores = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    if pixels >= 1920 * 1080 && cores >= 6 {
        4
    } else if pixels > 1280 * 720 && cores >= 4 {
        2
    } else {
        1
    }
}

#[cfg(not(target_os = "windows"))]
pub fn decoder_threads_for_resolution(_width: u32, _height: u32) -> u32 {
    1
}

/// Returns the number of H.264 encoder threads used by libwebrtc for the given resolution.
/// Matches the logic from h264_multithread_windows.patch (Windows only).
/// Uses physical cores: even logical cores >= 4 assumed hyperthreaded (e.g. 8c/16t -> 8).
#[cfg(target_os = "windows")]
pub fn encoder_threads_for_resolution(width: u32, height: u32) -> u32 {
    let pixels = (width as u64) * (height as u64);
    let logical = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    let cores = if logical >= 4 && logical % 2 == 0 {
        logical / 2
    } else {
        logical
    };
    if pixels >= 1920 * 1080 && cores > 8 {
        8
    } else if pixels >= 1920 * 1080 && cores >= 6 {
        6
    } else if pixels > 1280 * 960 && cores >= 4 {
        4
    } else if pixels > 640 * 480 && cores >= 3 {
        2
    } else {
        1
    }
}

#[cfg(not(target_os = "windows"))]
pub fn encoder_threads_for_resolution(_width: u32, _height: u32) -> u32 {
    1
}

// ─── Public command type ──────────────────────────────────────────────────────

pub enum VoiceCmd {
    /// Start a new LiveKit voice session (Phase 2).
    Start {
        /// LiveKit WebSocket URL (e.g. ws://localhost:7880).
        livekit_url: String,
        /// LiveKit JWT access token (from POST /voice/join).
        livekit_token: String,
        channel_id: i64,
        server_id: i64,
        api_base: String,
        /// Own user_id — used to track local speaking activity.
        my_user_id: i64,
        /// Shared map: user_id → currently speaking. Updated by the engine, read by UI.
        speaking: Arc<Mutex<HashMap<i64, bool>>>,
        /// Session stats (resolution, FPS, bitrate) — updated by engine, read by UI.
        session_stats: Arc<Mutex<VoiceSessionStats>>,
        /// Receiver + GUI telemetry (render, gui_draw). UI updates gui_draw, stream task updates render.
        receiver_telemetry: Option<std::sync::Arc<crate::telemetry::PipelineTelemetry>>,
    },
    /// Tear down the current session.
    Stop,
    /// Mute/unmute local microphone (audio still sent as silence when muted).
    SetMicMuted(bool),
    /// Mute/unmute all incoming audio locally.
    SetOutputMuted(bool),
    /// Override the playback volume for a remote user (0.0 – 3.0).
    SetUserVolume(i64, f32),
    /// Override the playback volume for a remote stream audio track (0.0 - 4.0).
    SetStreamVolume(i64, f32),
    /// Subscribe/unsubscribe to a remote screen stream (+ its audio track).
    SetStreamSubscription { user_id: i64, subscribed: bool },
    /// Set global input (microphone) volume multiplier (0.0 – 4.0).
    SetInputVolume(f32),
    /// Set global output (speaker) volume multiplier (0.0 – 4.0).
    SetOutputVolume(f32),
    /// Enable camera video (Phase 2.4).
    StartCamera,
    /// Disable camera video (Phase 2.4).
    StopCamera,
    /// Start screen sharing. `screen_index`: which monitor (None = 0). `preset`: quality preset.
    StartScreen { source: StreamSourceTarget, preset: ScreenPreset },
    /// Mute/unmute outgoing screen/application audio.
    SetScreenAudioMuted(bool),
    /// Stop screen sharing.
    StopScreen,
}

// ─── Spawn helper ─────────────────────────────────────────────────────────────

/// Start the voice engine (LiveKit). Spawns a tokio task that receives commands.
/// Returns `(cmd_sender, video_frames)` — the caller stores both.
pub fn spawn_voice_engine(
    rt: tokio::runtime::Handle,
) -> (
    tokio::sync::mpsc::UnboundedSender<VoiceCmd>,
    VideoFrames,
    std::sync::mpsc::Receiver<()>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let video_frames: VideoFrames = Arc::new(Mutex::new(HashMap::new()));
    let vf = Arc::clone(&video_frames);
    rt.spawn(async move {
        crate::voice_livekit::run_engine(rx, vf).await;
        let _ = done_tx.send(());
    });
    (tx, video_frames, done_rx)
}
