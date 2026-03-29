//! LiveKit-based voice session (Phase 2).
//!
//! Connects to a room via URL + token, publishes local audio (mic), subscribes to remote
//! tracks, and updates speaking state from Room events (ActiveSpeakersChanged).
//! Phase 2.4: camera and screen video tracks (publish + subscribe), voice-grid video.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::denoise::{microphone_denoise_enabled, AudioDenoiser};
use futures_util::StreamExt;
use image::codecs::jpeg::JpegEncoder;
use image::{ColorType, DynamicImage};
use livekit::options::{TrackPublishOptions, VideoCodec};
use livekit::prelude::*;
use livekit::track::{LocalAudioTrack, LocalTrack, LocalVideoTrack, RemoteTrack};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::prelude::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::video_frame::{BoxVideoFrame, VideoFormatType, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::{NativeEncodedVideoSource, NativeVideoSource};
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use livekit::webrtc::video_stream::native::NativeVideoStream;
use parking_lot::Mutex;
use std::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use xcap::{Monitor, Window};

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use crate::d3d11_i420::{D3d11RgbaToI420, D3d11RgbaToI420Scaled, GpuConvertTiming, I420Planes};
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use crate::d3d11_nv12::D3d11BgraToNv12;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use crate::d3d11_rgba::{
    decode_gpu_failed, mark_decode_gpu_failed, D3d11I420ToRgba, D3d11Nv12ToRgba,
};
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use crate::dxgi_duplication::{DxgiDuplicationCapture, DxgiDuplicationError};
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use crate::encoded_h264::{EncodedBackendKind, EncodedH264Encoder, EncodedH264EncoderError};
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use crate::nvenc_d11::NvencD3d11Error;
use crate::screen_encoder::box_scale_rgba;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use crate::screen_encoder::{select_screen_encoder, EncoderOutput, RawFrame};
use crate::telemetry::{is_telemetry_enabled, PipelineTelemetry};
use crate::voice::{
    decoder_threads_for_resolution, encoder_threads_for_resolution, video_frame_key,
    video_preview_frame_key, EncodingPath, ScreenPreset, StreamSourceTarget, StreamWindowInfo,
    VideoFrames, VoiceCmd, VoiceSessionStats,
};
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use crate::windows_loopback::{
    capture_loopback_to_ring, sibling_current_image_process_ids, LoopbackTarget,
};
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use webrtc_sys::video_frame_buffer::ffi::{
    i420_to_yuv8, video_frame_buffer_get_d3d11_subresource, video_frame_buffer_get_d3d11_texture,
    video_frame_buffer_is_d3d11, yuv8_to_yuv, VideoFrameBuffer,
};

const SAMPLE_RATE: u32 = 48_000;
const SAMPLES_PER_10MS: usize = 480; // 10 ms at 48 kHz mono
const STREAM_PREVIEW_TOPIC: &str = "astrix.stream-preview";
const STREAM_KEYFRAME_REQUEST_TOPIC: &str = "astrix.stream-keyframe";
const STREAM_PREVIEW_INTERVAL: Duration = Duration::from_secs(30);
const STREAM_PREVIEW_MAX_DIM: u32 = 640;
const STREAM_PREVIEW_JPEG_QUALITY: u8 = 55;
const LOCAL_AUDIO_QUEUE_MS: u32 = 40;
const SCREEN_AUDIO_QUEUE_MS: u32 = 500;
const SCREEN_AUDIO_SIGNAL_THRESHOLD: i16 = 32;
const MIC_RING_CAPTURE_MAX_FRAMES: usize = 12;
const MIC_RING_TRIM_TRIGGER_FRAMES: usize = 6;
const MIC_RING_TARGET_FRAMES: usize = 3;
const LOCAL_SPEAKING_RMS_THRESHOLD: f32 = 0.0035;
const LOCAL_SPEAKING_PEAK_THRESHOLD: f32 = 0.0120;
const LOCAL_SPEAKING_HOLD_FRAMES: usize = 18;
const REMOTE_SPEAKING_RMS_THRESHOLD: f32 = 0.0022;
const REMOTE_SPEAKING_PEAK_THRESHOLD: f32 = 0.0080;
const REMOTE_SPEAKING_HOLD_FRAMES: usize = 22;
const DEFAULT_INPUT_SENSITIVITY: f32 = 0.55;
const MIC_GATE_FLOOR_MIN_DBFS: f32 = -78.0;
const MIC_GATE_FLOOR_MAX_DBFS: f32 = -38.0;
const MIC_GATE_OPEN_MARGIN_DB_MAX: f32 = 30.0;
const MIC_GATE_OPEN_MARGIN_DB_MIN: f32 = 4.5;
const MIC_GATE_CLOSE_HYSTERESIS_DB: f32 = 6.0;
const MIC_GATE_OPEN_PEAK_MAX: f32 = 0.22;
const MIC_GATE_OPEN_PEAK_MIN: f32 = 0.0075;
const MIC_GATE_SENSITIVITY_CURVE_EXP: f32 = 0.65;
const MIC_GATE_ATTACK: f32 = 0.88;
const MIC_GATE_RELEASE: f32 = 0.24;
const MIC_GATE_HOLD_FRAMES: usize = 18;
const MIC_GATE_MIN_GAIN: f32 = 0.001;

fn extract_rtt_ms_from_stats(stats: &[livekit::webrtc::stats::RtcStats]) -> Option<f32> {
    let candidate_pair_rtt_ms = stats
        .iter()
        .filter_map(|stat| match stat {
            livekit::webrtc::stats::RtcStats::CandidatePair(pair)
                if pair.candidate_pair.current_round_trip_time.is_finite()
                    && pair.candidate_pair.current_round_trip_time > 0.0
                    && (pair.candidate_pair.nominated
                        || matches!(
                            pair.candidate_pair.state,
                            Some(livekit::webrtc::stats::IceCandidatePairState::Succeeded)
                        )) =>
            {
                Some(pair.candidate_pair.current_round_trip_time as f32 * 1000.0)
            }
            _ => None,
        })
        .min_by(|left, right| left.total_cmp(right));
    if candidate_pair_rtt_ms.is_some() {
        return candidate_pair_rtt_ms;
    }

    stats
        .iter()
        .filter_map(|stat| match stat {
            livekit::webrtc::stats::RtcStats::RemoteInboundRtp(remote)
                if remote.remote_inbound.round_trip_time_measurements > 0
                    && remote.remote_inbound.round_trip_time.is_finite()
                    && remote.remote_inbound.round_trip_time > 0.0 =>
            {
                Some(remote.remote_inbound.round_trip_time as f32 * 1000.0)
            }
            livekit::webrtc::stats::RtcStats::RemoteOutboundRtp(remote)
                if remote.remote_outbound.round_trip_time_measurements > 0
                    && remote.remote_outbound.round_trip_time.is_finite()
                    && remote.remote_outbound.round_trip_time > 0.0 =>
            {
                Some(remote.remote_outbound.round_trip_time as f32 * 1000.0)
            }
            _ => None,
        })
        .min_by(|left, right| left.total_cmp(right))
}

fn extract_session_rtt_ms(
    publisher_stats: &[livekit::webrtc::stats::RtcStats],
    subscriber_stats: &[livekit::webrtc::stats::RtcStats],
) -> Option<f32> {
    match (
        extract_rtt_ms_from_stats(publisher_stats),
        extract_rtt_ms_from_stats(subscriber_stats),
    ) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

/// Resample mono i16 to target_len (linear interpolation). Used when device rate != 48 kHz.
fn resample_linear(samples: &[i16], target_len: usize) -> Vec<i16> {
    if samples.is_empty() {
        return vec![0i16; target_len];
    }
    if target_len == 0 {
        return Vec::new();
    }
    let n = samples.len();
    (0..target_len)
        .map(|i| {
            let src_f = (i as f64) * (n - 1) as f64 / (target_len - 1).max(1) as f64;
            let idx = src_f as usize;
            let frac = src_f - idx as f64;
            let a = samples.get(idx).copied().unwrap_or(0) as f64;
            let b = samples.get(idx + 1).copied().unwrap_or(0) as f64;
            (a + frac * (b - a)).clamp(-32768.0, 32767.0) as i16
        })
        .collect()
}

fn frame_rms_peak(samples: &[i16]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut energy = 0.0f32;
    let mut peak = 0.0f32;
    for &sample in samples {
        let value = sample as f32 / 32768.0;
        energy += value * value;
        peak = peak.max(value.abs());
    }
    ((energy / samples.len() as f32).sqrt(), peak)
}

fn lerp_f32(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

fn rms_to_dbfs(rms: f32) -> f32 {
    (20.0 * rms.max(1.0e-6).log10()).clamp(-96.0, 0.0)
}

fn apply_gain_ramp(samples: &mut [i16], start_gain: f32, end_gain: f32) {
    if samples.is_empty() {
        return;
    }
    if start_gain <= MIC_GATE_MIN_GAIN && end_gain <= MIC_GATE_MIN_GAIN {
        samples.fill(0);
        return;
    }
    let len = samples.len().max(1) as f32;
    for (idx, sample) in samples.iter_mut().enumerate() {
        let t = idx as f32 / len;
        let gain = lerp_f32(start_gain, end_gain, t);
        *sample = ((*sample as f32) * gain).clamp(-32768.0, 32767.0) as i16;
    }
}

struct MicSensitivityGate {
    noise_floor_dbfs: f32,
    current_gain: f32,
    hold_frames: usize,
}

impl Default for MicSensitivityGate {
    fn default() -> Self {
        Self {
            noise_floor_dbfs: -66.0,
            current_gain: 0.0,
            hold_frames: 0,
        }
    }
}

impl MicSensitivityGate {
    fn process(&mut self, samples: &mut [i16], sensitivity: f32) {
        if samples.is_empty() {
            return;
        }

        let sensitivity = sensitivity.clamp(0.0, 1.0);
        if sensitivity <= 0.001 {
            let prev_gain = self.current_gain;
            apply_gain_ramp(samples, prev_gain, 0.0);
            self.current_gain = 0.0;
            self.hold_frames = 0;
            return;
        }
        let curve = sensitivity.powf(MIC_GATE_SENSITIVITY_CURVE_EXP);
        let (rms, peak) = frame_rms_peak(samples);
        let frame_dbfs = rms_to_dbfs(rms);
        let open_margin_db = lerp_f32(
            MIC_GATE_OPEN_MARGIN_DB_MAX,
            MIC_GATE_OPEN_MARGIN_DB_MIN,
            curve,
        );
        let close_margin_db = (open_margin_db - MIC_GATE_CLOSE_HYSTERESIS_DB).max(1.0);
        let open_peak = lerp_f32(MIC_GATE_OPEN_PEAK_MAX, MIC_GATE_OPEN_PEAK_MIN, curve);
        let close_peak = open_peak * 0.72;

        if self.current_gain < 0.2 && peak < close_peak * 1.4 {
            let coeff = if frame_dbfs > self.noise_floor_dbfs {
                0.18
            } else {
                0.02
            };
            self.noise_floor_dbfs = lerp_f32(self.noise_floor_dbfs, frame_dbfs, coeff)
                .clamp(MIC_GATE_FLOOR_MIN_DBFS, MIC_GATE_FLOOR_MAX_DBFS);
        }

        let open_threshold_dbfs = (self.noise_floor_dbfs + open_margin_db).min(-4.0);
        let close_threshold_dbfs =
            (self.noise_floor_dbfs + close_margin_db).min(open_threshold_dbfs - 1.0);
        let open_now = frame_dbfs >= open_threshold_dbfs || peak >= open_peak;
        if open_now {
            self.hold_frames = MIC_GATE_HOLD_FRAMES;
        } else {
            self.hold_frames = self.hold_frames.saturating_sub(1);
        }

        let keep_open =
            self.hold_frames > 0 || frame_dbfs >= close_threshold_dbfs || peak >= close_peak;
        let target_open = if self.current_gain > 0.2 {
            keep_open
        } else {
            open_now || self.hold_frames > 0
        };

        let prev_gain = self.current_gain;
        let target_gain = if target_open { 1.0 } else { 0.0 };
        let coeff = if target_open {
            MIC_GATE_ATTACK
        } else {
            MIC_GATE_RELEASE
        };
        let next_gain = lerp_f32(prev_gain, target_gain, coeff);
        apply_gain_ramp(samples, prev_gain, next_gain);
        self.current_gain = if next_gain <= MIC_GATE_MIN_GAIN {
            0.0
        } else {
            next_gain
        };
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct MixerRoute {
    user_id: i64,
    stream: bool,
}

enum MixerFrameMsg {
    Frame(MixerRoute, Vec<i16>),
    Remove(MixerRoute),
}

fn track_source_label(source: TrackSource) -> &'static str {
    if source == TrackSource::Screenshare {
        "screenshare"
    } else if source == TrackSource::ScreenshareAudio {
        "screenshare-audio"
    } else if source == TrackSource::Camera {
        "camera"
    } else {
        "audio"
    }
}

fn parse_remote_user_id(identity: &str) -> Option<i64> {
    identity.parse::<i64>().ok()
}

fn apply_publication_subscription(publication: &RemoteTrackPublication, subscribed: bool) {
    if publication.is_desired() != subscribed || (subscribed && !publication.is_subscribed()) {
        publication.set_subscribed(subscribed);
    }
}

fn handle_publication_preference(
    participant: &RemoteParticipant,
    publication: &RemoteTrackPublication,
    desired_stream_subscriptions: &HashSet<i64>,
    stream_video_publications: &mut HashMap<i64, RemoteTrackPublication>,
    stream_audio_publications: &mut HashMap<i64, RemoteTrackPublication>,
) {
    let Some(user_id) = parse_remote_user_id(&participant.identity().to_string()) else {
        return;
    };
    let source = publication.source();
    if source == TrackSource::Screenshare {
        stream_video_publications.insert(user_id, publication.clone());
        apply_publication_subscription(
            publication,
            desired_stream_subscriptions.contains(&user_id),
        );
    } else if source == TrackSource::ScreenshareAudio {
        stream_audio_publications.insert(user_id, publication.clone());
        apply_publication_subscription(
            publication,
            desired_stream_subscriptions.contains(&user_id),
        );
    } else {
        apply_publication_subscription(publication, true);
    }
}

pub fn enumerate_stream_windows() -> Vec<StreamWindowInfo> {
    let mut windows: Vec<_> = Window::all()
        .unwrap_or_default()
        .into_iter()
        .filter(|window| !window.is_minimized())
        .filter(|window| window.width() > 64 && window.height() > 64)
        .filter(|window| {
            let title = window.title().trim();
            let app = window.app_name().trim();
            !(title.is_empty() && app.is_empty())
        })
        .map(|window| {
            #[cfg(target_os = "windows")]
            let process_id = window.process_id();
            #[cfg(not(target_os = "windows"))]
            let process_id = 0;

            StreamWindowInfo {
                window_id: window.id(),
                process_id,
                app_name: window.app_name().to_string(),
                title: window.title().to_string(),
                width: window.width(),
                height: window.height(),
            }
        })
        .collect();
    windows.sort_by(|a, b| {
        a.app_name
            .cmp(&b.app_name)
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.window_id.cmp(&b.window_id))
    });
    windows
}

fn find_stream_window(window_id: u32, process_id: u32) -> Option<Window> {
    Window::all()
        .unwrap_or_default()
        .into_iter()
        .find(|window| {
            if window.id() != window_id {
                return false;
            }
            #[cfg(target_os = "windows")]
            {
                process_id == 0 || window.process_id() == process_id
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = process_id;
                true
            }
        })
}

fn capture_source_image(source: &StreamSourceTarget) -> Option<image::RgbaImage> {
    match source {
        StreamSourceTarget::Monitor { index } => {
            let monitors = enumerate_unique_screens();
            let monitor = monitors.into_iter().nth(*index)?;
            monitor.capture_image().ok()
        }
        StreamSourceTarget::Window {
            window_id,
            process_id,
        } => find_stream_window(*window_id, *process_id)?
            .capture_image()
            .ok(),
    }
}

fn encode_stream_preview(source: &StreamSourceTarget) -> Option<Vec<u8>> {
    let image = capture_source_image(source)?;
    let preview = DynamicImage::ImageRgba8(image)
        .thumbnail(STREAM_PREVIEW_MAX_DIM, STREAM_PREVIEW_MAX_DIM)
        .to_rgb8();
    let mut encoded = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut encoded, STREAM_PREVIEW_JPEG_QUALITY);
    encoder
        .encode(
            preview.as_raw(),
            preview.width(),
            preview.height(),
            ColorType::Rgb8.into(),
        )
        .ok()?;
    Some(encoded)
}

fn spawn_stream_preview_publisher(
    participant: LocalParticipant,
    source: StreamSourceTarget,
    stop_flag: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(STREAM_PREVIEW_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            let preview = {
                let source = source.clone();
                tokio::task::spawn_blocking(move || encode_stream_preview(&source)).await
            };
            let Ok(Some(payload)) = preview else {
                continue;
            };
            let packet = DataPacket {
                payload,
                topic: Some(STREAM_PREVIEW_TOPIC.to_string()),
                reliable: false,
                destination_identities: Vec::new(),
            };
            if participant.publish_data(packet).await.is_err() {
                break;
            }
        }
    })
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn start_stream_audio_capture(
    source: &StreamSourceTarget,
    stop_flag: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
) -> Option<(LocalAudioTrack, tokio::task::JoinHandle<()>)> {
    let target = match source {
        StreamSourceTarget::Monitor { .. } => LoopbackTarget::ExcludeCurrentProcessTree,
        StreamSourceTarget::Window { process_id, .. } if *process_id != 0 => {
            LoopbackTarget::IncludeProcessTree(*process_id)
        }
        StreamSourceTarget::Window { .. } => return None,
    };

    let source_options = AudioSourceOptions {
        echo_cancellation: false,
        noise_suppression: false,
        auto_gain_control: false,
    };
    let audio_source =
        NativeAudioSource::new(source_options, SAMPLE_RATE, 1, SCREEN_AUDIO_QUEUE_MS);
    let track = LocalAudioTrack::create_audio_track(
        "screen-audio",
        RtcAudioSource::Native(audio_source.clone()),
    );
    let ring: Arc<Mutex<VecDeque<i16>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(SAMPLES_PER_10MS * 50)));
    let ring_for_capture = Arc::clone(&ring);
    let stop_for_capture = Arc::clone(&stop_flag);
    std::thread::Builder::new()
        .name("livekit-screen-audio-capture".into())
        .spawn(move || {
            if let Err(err) = capture_loopback_to_ring(target, stop_for_capture, ring_for_capture) {
                eprintln!("[voice][screen][audio] capture error: {err}");
            }
        })
        .ok()?;

    let mut sibling_rings: Vec<Arc<Mutex<VecDeque<i16>>>> = Vec::new();
    if matches!(source, StreamSourceTarget::Monitor { .. }) {
        let sibling_pids = sibling_current_image_process_ids();
        if !sibling_pids.is_empty() {
            eprintln!(
                "[voice][screen][audio] excluding sibling Astrix processes from monitor stream: {:?}",
                sibling_pids
            );
        }
        for pid in sibling_pids {
            let sibling_ring: Arc<Mutex<VecDeque<i16>>> =
                Arc::new(Mutex::new(VecDeque::with_capacity(SAMPLES_PER_10MS * 50)));
            let ring_for_capture = Arc::clone(&sibling_ring);
            let stop_for_capture = Arc::clone(&stop_flag);
            match std::thread::Builder::new()
                .name(format!("livekit-screen-audio-exclude-{pid}"))
                .spawn(move || {
                    if let Err(err) = capture_loopback_to_ring(
                        LoopbackTarget::IncludeProcessTree(pid),
                        stop_for_capture,
                        ring_for_capture,
                    ) {
                        eprintln!(
                            "[voice][screen][audio] sibling Astrix capture error for pid {}: {}",
                            pid, err
                        );
                    }
                }) {
                Ok(_) => sibling_rings.push(sibling_ring),
                Err(err) => eprintln!(
                    "[voice][screen][audio] failed to spawn sibling Astrix capture for pid {}: {}",
                    pid, err
                ),
            }
        }
    }

    let handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut logged_non_silent_capture = false;
        let mut logged_non_silent_sibling_exclusion = false;
        while !stop_flag.load(Ordering::Relaxed) {
            interval.tick().await;
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            let mut samples = Vec::with_capacity(SAMPLES_PER_10MS);
            {
                let mut guard = ring.lock();
                for _ in 0..SAMPLES_PER_10MS {
                    samples.push(guard.pop_front().unwrap_or(0));
                }
            }
            let mut subtracted_sibling_audio = false;
            for sibling_ring in &sibling_rings {
                let mut guard = sibling_ring.lock();
                for sample in &mut samples {
                    let sibling_sample = guard.pop_front().unwrap_or(0);
                    if sibling_sample.unsigned_abs() > SCREEN_AUDIO_SIGNAL_THRESHOLD as u16 {
                        subtracted_sibling_audio = true;
                    }
                    let mixed = (*sample as i32) - (sibling_sample as i32);
                    *sample = mixed.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                }
            }
            if muted.load(Ordering::Relaxed) {
                samples.fill(0);
            }
            if !logged_non_silent_sibling_exclusion && subtracted_sibling_audio {
                logged_non_silent_sibling_exclusion = true;
                eprintln!(
                    "[voice][screen][audio] subtracting sibling Astrix playback from monitor stream"
                );
            }
            if !logged_non_silent_capture
                && samples
                    .iter()
                    .any(|sample| sample.unsigned_abs() > SCREEN_AUDIO_SIGNAL_THRESHOLD as u16)
            {
                logged_non_silent_capture = true;
                eprintln!("[voice][screen][audio] captured first non-silent loopback chunk");
            }
            let frame = AudioFrame {
                data: samples.into(),
                sample_rate: SAMPLE_RATE,
                num_channels: 1,
                samples_per_channel: SAMPLES_PER_10MS as u32,
            };
            let _ = audio_source.capture_frame(&frame).await;
        }
    });

    Some((track, handle))
}

#[cfg(not(all(target_os = "windows", feature = "wgc-capture")))]
fn start_stream_audio_capture(
    _source: &StreamSourceTarget,
    _stop_flag: Arc<AtomicBool>,
    _muted: Arc<AtomicBool>,
) -> Option<(LocalAudioTrack, tokio::task::JoinHandle<()>)> {
    None
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
#[derive(Default)]
struct DxgiPushTraceStats {
    ticks: u64,
    slot_ready: u64,
    slot_missing: u64,
    pool_missing: u64,
    late_drop_ticks: u64,
    late_drop_frames: u64,
    need_init_ticks: u64,
    new_content_ticks: u64,
    reuse_content_ticks: u64,
    static_reencode_ticks: u64,
    static_reencode_ok: u64,
    empty_output_ticks: u64,
    convert_ok: u64,
    convert_fail: u64,
    convert_us_total: u64,
    flush_ok: u64,
    flush_fail: u64,
    flush_us_total: u64,
    ready_immediate: u64,
    ready_waited: u64,
    ready_timeout: u64,
    ready_wait_us_total: u64,
    submit_ok: u64,
    submit_fail: u64,
    submit_us_total: u64,
    collect_some: u64,
    collect_none: u64,
    collect_err: u64,
    collect_frames: u64,
    collect_us_total: u64,
    encode_ok: u64,
    encode_fail: u64,
    encode_us_total: u64,
    send_calls: u64,
    sent_frames: u64,
    send_us_total: u64,
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
impl DxgiPushTraceStats {
    fn rate(count: u64, secs: f32) -> f32 {
        count as f32 / secs.max(0.001)
    }

    fn avg_us(total: u64, count: u64) -> u64 {
        if count == 0 {
            0
        } else {
            total / count
        }
    }

    fn maybe_log(
        &mut self,
        window_start: &mut std::time::Instant,
        target_fps: f64,
        schedule_fps: f64,
        mode: &str,
    ) {
        let elapsed = window_start.elapsed();
        if elapsed < Duration::from_secs(1) || self.ticks == 0 {
            return;
        }
        let secs = elapsed.as_secs_f32().max(0.001);
        eprintln!(
            "[voice][screen][push][summary] tick={:.1} slot_ready={:.1} slot_miss={:.1} pool_miss={} late_ticks={} drop_frames={} need_init={} new_content={} reuse_content={} static_reuse={} empty={} sent={:.1} target={:.0} schedule={:.0} mode={}",
            Self::rate(self.ticks, secs),
            Self::rate(self.slot_ready, secs),
            Self::rate(self.slot_missing, secs),
            self.pool_missing,
            self.late_drop_ticks,
            self.late_drop_frames,
            self.need_init_ticks,
            self.new_content_ticks,
            self.reuse_content_ticks,
            self.static_reencode_ticks,
            self.empty_output_ticks,
            Self::rate(self.sent_frames, secs),
            target_fps,
            schedule_fps,
            mode,
        );
        eprintln!(
            "[voice][screen][push][timing] convert={}/{} avg={}us flush={}/{} avg={}us ready=imm:{} wait:{} timeout:{} avg={}us submit={}/{} avg={}us collect=some:{} none:{} err:{} frames={:.1} avg={}us encode={}/{} avg={}us send_calls={:.1} avg_send={}us",
            self.convert_ok,
            self.convert_fail,
            Self::avg_us(self.convert_us_total, self.convert_ok),
            self.flush_ok,
            self.flush_fail,
            Self::avg_us(self.flush_us_total, self.flush_ok),
            self.ready_immediate,
            self.ready_waited,
            self.ready_timeout,
            Self::avg_us(self.ready_wait_us_total, self.ready_waited),
            self.submit_ok,
            self.submit_fail,
            Self::avg_us(self.submit_us_total, self.submit_ok),
            self.collect_some,
            self.collect_none,
            self.collect_err,
            Self::rate(self.collect_frames, secs),
            Self::avg_us(self.collect_us_total, self.collect_some + self.collect_none),
            self.encode_ok,
            self.encode_fail,
            Self::avg_us(self.encode_us_total, self.encode_ok + self.encode_fail),
            Self::rate(self.send_calls, secs),
            Self::avg_us(self.send_us_total, self.send_calls),
        );
        *self = Self::default();
        *window_start = std::time::Instant::now();
    }
}

/// RTP clock for video tracks. WebRTC/H264 uses a 90 kHz timestamp base.
const VIDEO_RTP_CLOCK_HZ: u64 = 90_000;

/// Runs the LiveKit voice engine: receives commands, on Start connects to the room,
/// publishes mic, subscribes to remotes, updates speaking map. Exits on Stop or channel close.
pub async fn run_engine(mut rx: UnboundedReceiver<VoiceCmd>, video_frames: VideoFrames) {
    loop {
        match rx.recv().await {
            None => break,
            Some(VoiceCmd::Stop) => break,
            Some(VoiceCmd::Start {
                livekit_url,
                livekit_token,
                my_user_id,
                speaking,
                session_stats,
                receiver_telemetry,
                ..
            }) => {
                if let Err(e) = run_session(
                    livekit_url,
                    livekit_token,
                    my_user_id,
                    speaking,
                    video_frames.clone(),
                    session_stats,
                    receiver_telemetry,
                    &mut rx,
                )
                .await
                {
                    eprintln!("[voice][livekit] session error: {}", e);
                }
            }
            _ => {}
        }
    }
    eprintln!("[voice][livekit] engine stopped");
}

async fn run_session(
    url: String,
    token: String,
    my_user_id: i64,
    speaking: Arc<Mutex<HashMap<i64, bool>>>,
    video_frames: VideoFrames,
    session_stats: Arc<Mutex<VoiceSessionStats>>,
    receiver_telemetry: Option<Arc<PipelineTelemetry>>,
    rx: &mut UnboundedReceiver<VoiceCmd>,
) -> Result<(), String> {
    eprintln!("[voice][livekit] connecting to {} ...", url);

    let mut room_opts = RoomOptions::default();
    room_opts.adaptive_stream = false;
    room_opts.auto_subscribe = false;
    let (room, mut room_events) = Room::connect(&url, &token, room_opts)
        .await
        .map_err(|e| format!("Room::connect: {:?}", e))?;

    eprintln!("[voice][livekit] connected to room");

    // ── Mic input: determine device sample rate upfront for the timer task ──────────────────
    let (input_rate, _input_channels) = {
        use cpal::traits::HostTrait;
        let host = cpal::default_host();
        host.default_input_device()
            .and_then(|d| preferred_input_config_48k(&d).ok())
            .map(|c| (c.sample_rate.0, c.channels as usize))
            .unwrap_or((SAMPLE_RATE, 1))
    };
    // How many raw device samples equal 10 ms. The timer task drains this many every tick.
    let input_10ms_len = (input_rate / 100).max(1) as usize;
    let need_resample_mic = input_10ms_len != SAMPLES_PER_10MS;
    eprintln!(
        "[voice][livekit] mic: {} Hz{}",
        input_rate,
        if need_resample_mic {
            ", resampling to 48 kHz"
        } else {
            ""
        }
    );

    let output_sample_rate = get_output_sample_rate().unwrap_or(SAMPLE_RATE);
    eprintln!(
        "[voice][livekit] speaker output at {} Hz",
        output_sample_rate
    );

    // ── Publish local microphone ─────────────────────────────────────────────────────────────
    let source_options = AudioSourceOptions {
        echo_cancellation: false,
        noise_suppression: false,
        auto_gain_control: false,
    };
    // Keep the internal queue tight so sender-side DSP does not silently turn into
    // long-lived playout delay after a temporary stall.
    let livekit_source =
        NativeAudioSource::new(source_options, SAMPLE_RATE, 1, LOCAL_AUDIO_QUEUE_MS);
    let track = LocalAudioTrack::create_audio_track(
        "microphone",
        RtcAudioSource::Native(livekit_source.clone()),
    );
    let mic_publication = room
        .local_participant()
        .publish_track(LocalTrack::Audio(track), TrackPublishOptions::default())
        .await
        .map_err(|e| format!("publish_track: {:?}", e))?;

    // ── Mic ring buffer: cpal thread writes; timer task reads every 10 ms ───────────────────
    //
    // FIX (audio crackling over internet):
    // Old approach — event-driven channel: if capture_frame().await ever stalled under
    // network back-pressure, frames piled up and were delivered in a burst → crackling.
    // New approach — shared ring buffer + steady 10 ms timer: delivery rate is decoupled
    // from capture rate, so frames always arrive at a constant cadence regardless of
    // how busy the async runtime is.
    let mic_ring: Arc<Mutex<VecDeque<i16>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(input_10ms_len * 20)));
    let mic_stop = Arc::new(AtomicBool::new(false));

    // Input volume: scales mic samples before sending (SetInputVolume updates this).
    let input_volume: Arc<Mutex<f32>> = Arc::new(Mutex::new(1.0));
    let input_vol_for_mic = Arc::clone(&input_volume);
    let input_sensitivity: Arc<Mutex<f32>> = Arc::new(Mutex::new(DEFAULT_INPUT_SENSITIVITY));
    let input_sensitivity_for_mic = Arc::clone(&input_sensitivity);
    let mic_muted_flag = Arc::new(AtomicBool::new(false));
    let mic_muted_for_task = Arc::clone(&mic_muted_flag);
    let use_mic_denoise = microphone_denoise_enabled();

    // Incoming video stats (viewer): frame count and resolution for estimated bitrate.
    let incoming_frame_count: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let incoming_resolution: Arc<Mutex<(u32, u32)>> = Arc::new(Mutex::new((0, 0)));

    let ring_for_mic = Arc::clone(&mic_ring);
    let stop_for_mic = Arc::clone(&mic_stop);
    let mic_thread = std::thread::Builder::new()
        .name("livekit-mic-capture".into())
        .spawn(move || {
            if let Err(e) = capture_mic_to_ring(ring_for_mic, stop_for_mic) {
                eprintln!("[voice][livekit] mic capture error: {}", e);
            }
        })
        .map_err(|e| format!("spawn mic thread: {}", e))?;

    // Timer task: drains exactly input_10ms_len samples every 10 ms → 48 kHz to LiveKit.
    // Applies input_volume to scale mic level before sending.
    let mic_ring_for_task = Arc::clone(&mic_ring);
    let source_for_timer = livekit_source.clone();
    let speaking_for_mic = Arc::clone(&speaking);
    let my_user_id_for_mic = my_user_id;
    let mic_timer_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut mic_denoiser = if use_mic_denoise {
            Some(AudioDenoiser::new_sender_microphone())
        } else {
            None
        };
        let mut mic_gate = MicSensitivityGate::default();
        let mut local_speaking_hold = 0usize;
        loop {
            interval.tick().await;
            let chunk: Option<Vec<i16>> = {
                let mut ring = mic_ring_for_task.lock();
                let trim_threshold = input_10ms_len * MIC_RING_TRIM_TRIGGER_FRAMES;
                if ring.len() > trim_threshold {
                    let target_len = input_10ms_len * MIC_RING_TARGET_FRAMES;
                    let drop_count = ring.len().saturating_sub(target_len.max(input_10ms_len));
                    for _ in 0..drop_count {
                        ring.pop_front();
                    }
                }
                if ring.len() >= input_10ms_len {
                    Some(ring.drain(..input_10ms_len).collect())
                } else {
                    None
                }
            };
            if let Some(raw) = chunk {
                let mut samples: Vec<i16> = if need_resample_mic {
                    resample_linear(&raw, SAMPLES_PER_10MS)
                } else {
                    raw
                };
                let mic_muted = mic_muted_for_task.load(Ordering::Relaxed);
                if mic_muted {
                    local_speaking_hold = 0;
                    if my_user_id_for_mic > 0 {
                        speaking_for_mic.lock().remove(&my_user_id_for_mic);
                    }
                    samples.fill(0);
                } else {
                    if let Some(denoiser) = mic_denoiser.as_mut() {
                        denoiser.process_i16(&mut samples);
                    }
                    let sensitivity = *input_sensitivity_for_mic.lock();
                    mic_gate.process(&mut samples, sensitivity);
                    if my_user_id_for_mic > 0 {
                        let (rms, peak) = frame_rms_peak(&samples);
                        let speaking_now = rms >= LOCAL_SPEAKING_RMS_THRESHOLD
                            || peak >= LOCAL_SPEAKING_PEAK_THRESHOLD;
                        if speaking_now {
                            local_speaking_hold = LOCAL_SPEAKING_HOLD_FRAMES;
                        } else {
                            local_speaking_hold = local_speaking_hold.saturating_sub(1);
                        }
                        let mut map = speaking_for_mic.lock();
                        if local_speaking_hold > 0 {
                            map.insert(my_user_id_for_mic, true);
                        } else {
                            map.remove(&my_user_id_for_mic);
                        }
                    }
                }
                let vol = *input_vol_for_mic.lock();
                if (vol - 1.0).abs() > 0.01 {
                    for s in &mut samples {
                        *s = ((*s as f32) * vol).clamp(-32768.0, 32767.0) as i16;
                    }
                }
                let frame = AudioFrame {
                    data: samples.into(),
                    sample_rate: SAMPLE_RATE,
                    num_channels: 1,
                    samples_per_channel: SAMPLES_PER_10MS as u32,
                };
                let _ = source_for_timer.capture_frame(&frame).await;
            }
        }
    });

    // ── Remote audio: per-user mixer + cpal speaker output ──────────────────────────────────
    let user_volumes: Arc<Mutex<HashMap<i64, f32>>> = Arc::new(Mutex::new(HashMap::new()));
    let stream_volumes: Arc<Mutex<HashMap<i64, f32>>> = Arc::new(Mutex::new(HashMap::new()));
    let remote_voice_denoise_flags: Arc<Mutex<HashMap<i64, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let output_muted: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let output_volume: Arc<Mutex<f32>> = Arc::new(Mutex::new(1.0));
    let (mixer_tx, mut mixer_rx) = tokio::sync::mpsc::unbounded_channel::<MixerFrameMsg>();
    let output_10ms_len = (output_sample_rate / 100).max(1) as usize;
    let output_buffer: Arc<Mutex<VecDeque<i16>>> = {
        let mut q = VecDeque::new();
        // Pre-fill ~30 ms silence to avoid underrun crackling on first playback
        q.extend(std::iter::repeat(0i16).take(output_10ms_len * 3));
        Arc::new(Mutex::new(q))
    };
    let voice_volumes_for_mixer = Arc::clone(&user_volumes);
    let stream_volumes_for_mixer = Arc::clone(&stream_volumes);
    let muted_for_mixer = Arc::clone(&output_muted);
    let vol_for_mixer = Arc::clone(&output_volume);
    let out_buf = Arc::clone(&output_buffer);
    let mut mixer_interval = tokio::time::interval(Duration::from_millis(10));
    let mixer_task = tokio::spawn(async move {
        run_remote_audio_mixer(
            &mut mixer_rx,
            &mut mixer_interval,
            voice_volumes_for_mixer,
            stream_volumes_for_mixer,
            muted_for_mixer,
            vol_for_mixer,
            out_buf,
            output_sample_rate,
        )
        .await;
    });

    let speaker_stop = Arc::new(AtomicBool::new(false));
    let out_buf_play = Arc::clone(&output_buffer);
    let stop_for_speaker = Arc::clone(&speaker_stop);
    let speaker_thread = std::thread::Builder::new()
        .name("livekit-speaker".into())
        .spawn(move || {
            let _ = run_speaker_output(out_buf_play, stop_for_speaker);
        })
        .ok();

    // ── Video: camera + screen ───────────────────────────────────────────────────────────────
    let camera_running: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    // camera_sid / screen_sid only accessed from this async task — plain Option, no Arc<Mutex>.
    let mut camera_sid: Option<TrackSid> = None;
    let mut screen_sid: Option<TrackSid> = None;
    let mut screen_audio_sid: Option<TrackSid> = None;
    let mut screen_audio_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut screen_audio_stop_flag: Option<Arc<AtomicBool>> = None;
    let mut screen_audio_muted_flag: Option<Arc<AtomicBool>> = None;
    let mut screen_preview_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut screen_audio_muted = false;
    // Stop flag for the screen capture OS thread; replaced on each StartScreen.
    let mut screen_stop_flag: Option<Arc<AtomicBool>> = None;
    let mut screen_keyframe_requested: Option<Arc<AtomicBool>> = None;
    // Encoded-path -> CPU fallback: when the encoded H.264 backend hard-fails,
    // the encoder thread sends a Native track here.
    let mut screen_fallback_rx: Option<tokio::sync::oneshot::Receiver<LocalVideoTrack>> = None;
    // Publish options saved for potential fallback republish.
    let mut screen_publish_opts_saved: Option<(u64, f64)> = None; // (max_bitrate, max_framerate)
    let lp = room.local_participant();
    // Tracks the tokio task that drains each remote video stream.
    // On re-subscribe (stream restart) the old task is aborted before spawning a new one,
    // preventing multiple tasks from concurrently calling inc_recv_frame_count() on the
    // same telemetry object, which causes recv_fps to grow unboundedly across restarts.
    let mut video_stream_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut audio_stream_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut stream_video_publications: HashMap<i64, RemoteTrackPublication> = HashMap::new();
    let mut stream_audio_publications: HashMap<i64, RemoteTrackPublication> = HashMap::new();
    let mut desired_stream_subscriptions: HashSet<i64> = HashSet::new();
    let telemetry_enabled = is_telemetry_enabled();

    let mut incoming_stats_interval = tokio::time::interval(Duration::from_secs(1));
    incoming_stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // ── Main event loop ──────────────────────────────────────────────────────────────────────
    loop {
        tokio::select! {
            _ = incoming_stats_interval.tick() => {
                let c = incoming_frame_count.swap(0, Ordering::Relaxed);
                let (w, h) = *incoming_resolution.lock();
                if c > 0 && w > 0 && h > 0 {
                    // Approximate H.264 bitrate from (fps × resolution). Factor ~0.22 bpp matches
                    // OpenH264 Baseline-profile presets (e.g. 1440p60 @ 50 Mbit/s).
                    let mbps = (c as f32) * (w as f32) * (h as f32) * 0.22e-6;
                    if let Some(mut st) = session_stats.try_lock() {
                        st.incoming_speed_mbps = Some(mbps);
                    }
                } else {
                    if let Some(mut st) = session_stats.try_lock() {
                        st.incoming_speed_mbps = None;
                    }
                }
                if let Ok(stats) = room.get_stats().await {
                    if let Some(rtt_ms) =
                        extract_session_rtt_ms(&stats.publisher_stats, &stats.subscriber_stats)
                    {
                        if let Some(mut st) = session_stats.try_lock() {
                            st.latency_rtt_ms = Some(rtt_ms);
                        }
                    }
                }
            }
            ev = room_events.recv() => {
                match ev {
                    None => {
                        eprintln!("[voice][livekit] session: room_events channel closed (None)");
                        break;
                    }
                    Some(RoomEvent::ActiveSpeakersChanged { speakers }) => {
                        let mut map = speaking.lock();
                        let local_speaking = if my_user_id > 0 {
                            map.get(&my_user_id).copied().unwrap_or(false)
                        } else {
                            false
                        };
                        if my_user_id > 0 {
                            if local_speaking {
                                map.insert(my_user_id, true);
                            } else {
                                map.remove(&my_user_id);
                            }
                        }
                        for p in speakers {
                            if let Some(uid) = parse_remote_user_id(&p.identity().to_string()) {
                                map.insert(uid, true);
                            }
                        }
                    }
                    Some(RoomEvent::Connected { participants_with_tracks }) => {
                        for (participant, publications) in participants_with_tracks {
                            for publication in publications {
                                handle_publication_preference(
                                    &participant,
                                    &publication,
                                    &desired_stream_subscriptions,
                                    &mut stream_video_publications,
                                    &mut stream_audio_publications,
                                );
                            }
                        }
                    }
                    Some(RoomEvent::TrackPublished { publication, participant }) => {
                        handle_publication_preference(
                            &participant,
                            &publication,
                            &desired_stream_subscriptions,
                            &mut stream_video_publications,
                            &mut stream_audio_publications,
                        );
                    }
                    Some(RoomEvent::TrackUnpublished { publication, participant }) => {
                        let Some(uid) = parse_remote_user_id(&participant.identity().to_string()) else {
                            continue;
                        };
                        match publication.source() {
                            TrackSource::Screenshare => {
                                stream_video_publications.remove(&uid);
                                video_frames.lock().remove(&video_frame_key(uid, true));
                                video_frames.lock().remove(&video_preview_frame_key(uid));
                            }
                            TrackSource::ScreenshareAudio => {
                                stream_audio_publications.remove(&uid);
                                let _ = mixer_tx.send(MixerFrameMsg::Remove(MixerRoute {
                                    user_id: uid,
                                    stream: true,
                                }));
                            }
                            _ => {
                                speaking.lock().remove(&uid);
                                let _ = mixer_tx.send(MixerFrameMsg::Remove(MixerRoute {
                                    user_id: uid,
                                    stream: false,
                                }));
                            }
                        }
                    }
                    Some(RoomEvent::DataReceived {
                        payload,
                        topic,
                        participant,
                        ..
                    }) => {
                        if topic.as_deref() == Some(STREAM_KEYFRAME_REQUEST_TOPIC) {
                            if let Some(flag) = screen_keyframe_requested.as_ref() {
                                flag.store(true, Ordering::Relaxed);
                            }
                            continue;
                        }
                        if topic.as_deref() != Some(STREAM_PREVIEW_TOPIC) {
                            continue;
                        }
                        let Some(participant) = participant else {
                            continue;
                        };
                        let Some(uid) = parse_remote_user_id(&participant.identity().to_string()) else {
                            continue;
                        };
                        let currently_subscribed = stream_video_publications
                            .get(&uid)
                            .map(|publication| publication.is_subscribed())
                            .unwrap_or(false);
                        if desired_stream_subscriptions.contains(&uid) || currently_subscribed {
                            continue;
                        }
                        let Ok(image) = image::load_from_memory(payload.as_ref()) else {
                            continue;
                        };
                        let rgba = image.to_rgba8();
                        let (width, height) = rgba.dimensions();
                        video_frames.lock().insert(
                            video_preview_frame_key(uid),
                            crate::voice::VideoFrame {
                                width,
                                height,
                                rgba: rgba.into_raw(),
                                shared_handle: None,
                            },
                        );
                    }
                    Some(RoomEvent::TrackSubscribed { track, publication, participant }) => {
                        if let RemoteTrack::Audio(audio_track) = track.clone() {
                            let identity = participant.identity().to_string();
                            let Some(uid) = parse_remote_user_id(&identity) else {
                                continue;
                            };
                            let route = MixerRoute {
                                user_id: uid,
                                stream: publication.source() == TrackSource::ScreenshareAudio,
                            };
                            let task_key = format!(
                                "{}:{}:{}",
                                identity,
                                track_source_label(publication.source()),
                                publication.sid()
                            );
                            if let Some(old) = audio_stream_tasks.remove(&task_key) {
                                old.abort();
                                if !route.stream {
                                    speaking.lock().remove(&uid);
                                }
                            }
                            let tx = mixer_tx.clone();
                            let speaking_for_remote = Arc::clone(&speaking);
                            let denoise_enabled = remote_voice_denoise_flags
                                .lock()
                                .entry(uid)
                                .or_insert_with(|| Arc::new(AtomicBool::new(false)))
                                .clone();
                            let mut stream = livekit::webrtc::audio_stream::native::NativeAudioStream::new(
                                audio_track.rtc_track(),
                                SAMPLE_RATE as i32,
                                1,
                            );
                            let handle = tokio::spawn(async move {
                                let mut logged_non_silent_remote = false;
                                let mut denoiser: Option<AudioDenoiser> = None;
                                let mut remote_speaking_hold = 0usize;
                                while let Some(frame) = stream.next().await {
                                    let mut samples: Vec<i16> = frame.data.as_ref().to_vec();
                                    if !route.stream && denoise_enabled.load(Ordering::Relaxed) {
                                        let denoiser = denoiser.get_or_insert_with(|| {
                                            AudioDenoiser::new_receiver_voice(route.user_id)
                                        });
                                        denoiser.process_i16(&mut samples);
                                    }
                                    if !route.stream {
                                        let (rms, peak) = frame_rms_peak(&samples);
                                        let speaking_now = rms >= REMOTE_SPEAKING_RMS_THRESHOLD
                                            || peak >= REMOTE_SPEAKING_PEAK_THRESHOLD;
                                        if speaking_now {
                                            remote_speaking_hold = REMOTE_SPEAKING_HOLD_FRAMES;
                                        } else {
                                            remote_speaking_hold =
                                                remote_speaking_hold.saturating_sub(1);
                                        }
                                        let mut map = speaking_for_remote.lock();
                                        if remote_speaking_hold > 0 {
                                            map.insert(route.user_id, true);
                                        } else {
                                            map.remove(&route.user_id);
                                        }
                                    }
                                    if route.stream
                                        && !logged_non_silent_remote
                                        && samples.iter().any(|sample| {
                                            sample.unsigned_abs()
                                                > SCREEN_AUDIO_SIGNAL_THRESHOLD as u16
                                        })
                                    {
                                        logged_non_silent_remote = true;
                                        eprintln!(
                                            "[voice][screen][audio] received first non-silent remote chunk for user {}",
                                            route.user_id
                                        );
                                    }
                                    if samples.len() >= SAMPLES_PER_10MS {
                                        let _ = tx.send(MixerFrameMsg::Frame(route, samples));
                                    }
                                }
                                if !route.stream {
                                    speaking_for_remote.lock().remove(&route.user_id);
                                }
                            });
                            audio_stream_tasks.insert(task_key, handle);
                        }
                        if let RemoteTrack::Video(video_track) = track {
                            let identity = participant.identity().to_string();
                            let src = publication.source();
                            let src_str = if src == TrackSource::Screenshare {
                                "screenshare"
                            } else {
                                "camera"
                            };
                            let task_key = format!("{}:{}", identity, src_str);
                            // Abort any stale task from a previous stream for this participant.
                            // Without this, each restart spawns an additional task that keeps
                            // calling inc_recv_frame_count(), making recv_fps grow on every restart.
                            if let Some(old) = video_stream_tasks.remove(&task_key) {
                                old.abort();
                                eprintln!("[voice][screen][viewer] aborted stale stream task for {}", task_key);
                            }
                            eprintln!(
                                "[voice][screen][viewer] TrackSubscribed: {} from identity={}",
                                src_str, identity
                            );
                            let uid: i64 = match identity.parse() {
                                Ok(u) => u,
                                _ => {
                                    eprintln!("[voice][screen][viewer] cannot parse identity: {}", identity);
                                    continue;
                                }
                            };
                            let key = if src == TrackSource::Screenshare {
                                video_frame_key(uid, true)
                            } else {
                                video_frame_key(uid, false)
                            };

                            // Phase 7: split receive (tokio task) and convert (OS thread).
                            // stream.next() never blocked by GPU conversion → no backpressure → no SFU throttle.
                            let (frame_tx, frame_rx) = std::sync::mpsc::channel::<BoxVideoFrame>();

                            // Default OFF: wall-clock slot pacing causes visible speed-up/slow-down when
                            // delivery is bursty (SFU, decode variance). Set ASTRIX_RECV_PACING=1 to smooth.
                            let pacing_enabled = std::env::var("ASTRIX_RECV_PACING").map(|v| v == "1").unwrap_or(false);
                            static RECV_EXPECTED_US: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
                            let expected_us_fallback = *RECV_EXPECTED_US.get_or_init(|| {
                                std::env::var("ASTRIX_RECV_EXPECTED_US").ok()
                                    .and_then(|s| s.parse::<u64>().ok())
                                    .unwrap_or(11_111)
                            });
                            // Pacing slot (µs): follow sender timeline (timestamp_us delta), not delivered FPS —
                            // recv_fps swings with bursts/stalls and was retuning the slot every 1s → rubber timing.
                            let auto_interval_us = Arc::new(AtomicU64::new(expected_us_fallback));

                            // ── Converter OS thread: GPU/CPU conversion + pacing + insert into VideoFrames ──
                            let vf_conv = video_frames.clone();
                            let inc_count_conv = Arc::clone(&incoming_frame_count);
                            let inc_res_conv = Arc::clone(&incoming_resolution);
                            let stats_conv = Arc::clone(&session_stats);
                            let tel_conv = receiver_telemetry
                                .clone()
                                .unwrap_or_else(|| Arc::new(PipelineTelemetry::new()));
                            let conv_task_key = task_key.clone();
                            let auto_interval_conv = Arc::clone(&auto_interval_us);
                            std::thread::Builder::new()
                                .name(format!("astrix-conv-{}", conv_task_key))
                                .spawn(move || {
                                    #[cfg(target_os = "windows")]
                                    unsafe {
                                        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
                                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
                                    }
                                    if pacing_enabled {
                                        eprintln!("[voice][screen][viewer] converter thread started: pacing auto-adapt");
                                    } else {
                                        eprintln!("[voice][screen][viewer] converter thread started (no pacing)");
                                    }
                                    let mut first_frame_logged = false;
                                    let mut convert_count: u32 = 0;
                                    let mut last_playout = std::time::Instant::now();
                                    let mut convert_window_frames: u32 = 0;
                                    let mut convert_window_coalesced: u64 = 0;
                                    let mut convert_window_start = std::time::Instant::now();
                                    loop {
                                        let first = match frame_rx.recv() {
                                            Ok(f) => f,
                                            Err(_) => break,
                                        };
                                        let mut drained: u64 = 1;
                                        let mut frame = first;
                                        while let Ok(newer) = frame_rx.try_recv() {
                                            drained += 1;
                                            frame = newer;
                                        }
                                        convert_window_coalesced =
                                            convert_window_coalesced.saturating_add(drained.saturating_sub(1));
                                        if crate::telemetry::is_telemetry_enabled() {
                                            tel_conv.record_recv_coalesce(drained);
                                        }
                                        let iter_start = std::time::Instant::now();
                                        convert_count += 1;
                                        let decode_start = std::time::Instant::now();
                                        let rgba_result = video_frame_to_rgba(&frame);
                                        tel_conv.set_decode(decode_start.elapsed().as_micros() as u64);
                                        if rgba_result.is_none() && convert_count <= 3 {
                                            eprintln!(
                                                "[voice][screen][viewer] video_frame_to_rgba returned None for frame #{} ({}x{})",
                                                convert_count, frame.buffer.width(), frame.buffer.height()
                                            );
                                        }
                                        if let Some((w, h, rgba, used_gpu, shared_handle)) = rgba_result {
                                            if !first_frame_logged {
                                                let path_str = if shared_handle.is_some() { "GPU zero-copy (WGL)" } else if used_gpu { "GPU+CPU" } else { "CPU" };
                                                eprintln!("[voice][screen][viewer] first frame converted {}x{} path={}", w, h, path_str);
                                                first_frame_logged = true;
                                            }
                                            if pacing_enabled {
                                                let slot = std::time::Duration::from_micros(
                                                    auto_interval_conv.load(Ordering::Relaxed).max(1),
                                                );
                                                let now = std::time::Instant::now();
                                                // After a network burst, coalesce_pk>1: do not sleep between
                                                // iterations or we amplify lag (telemetry: high coalesced_drop +
                                                // pacing_sleep_sum while conv_iter_pk stays ~slot).
                                                if drained > 1 {
                                                    last_playout = now;
                                                } else {
                                                    let mut next_playout = last_playout + slot;
                                                    // Drop missed slots instead of snapping last_playout to `now`
                                                    // (snap caused long stretches with no sleep → visible speed-up).
                                                    while now >= next_playout {
                                                        last_playout = next_playout;
                                                        next_playout = last_playout + slot;
                                                    }
                                                    let sleep_dur = next_playout - now;
                                                    if crate::telemetry::is_telemetry_enabled() {
                                                        tel_conv.add_recv_pacing_sleep_us(
                                                            sleep_dur.as_micros() as u64,
                                                        );
                                                    }
                                                    std::thread::sleep(sleep_dur);
                                                    last_playout = next_playout;
                                                }
                                            }
                                            inc_count_conv.fetch_add(1, Ordering::Relaxed);
                                            *inc_res_conv.lock() = (w, h);
                                            if let Some(mut st) = stats_conv.try_lock() {
                                                let path = if shared_handle.is_some() { "GPU zero-copy" } else if used_gpu { "GPU" } else { "CPU" };
                                                st.decoding_path = Some(path.into());
                                                st.decoder_threads = Some(decoder_threads_for_resolution(w, h));
                                            }
                                            vf_conv.lock().insert(
                                                key,
                                                crate::voice::VideoFrame {
                                                    width: w,
                                                    height: h,
                                                    rgba,
                                                    shared_handle,
                                                },
                                            );
                                            convert_window_frames += 1;
                                            let elapsed = convert_window_start.elapsed();
                                            if elapsed >= Duration::from_secs(1) {
                                                let convert_fps =
                                                    convert_window_frames as f32 / elapsed.as_secs_f32().max(0.001);
                                                if crate::telemetry::is_telemetry_enabled() {
                                                    eprintln!(
                                                        "[voice][screen][viewer] convert rate: {:.1} fps (coalesced_drop={})",
                                                        convert_fps,
                                                        convert_window_coalesced
                                                    );
                                                }
                                                convert_window_frames = 0;
                                                convert_window_coalesced = 0;
                                                convert_window_start = std::time::Instant::now();
                                            }
                                        }
                                        if crate::telemetry::is_telemetry_enabled() {
                                            tel_conv.record_recv_conv_iter_peak(
                                                iter_start.elapsed().as_micros() as u64,
                                            );
                                        }
                                    }
                                    eprintln!("[voice][screen][viewer] converter thread ended after {} frames", convert_count);
                                })
                                .ok();

                            // ── Receiver tokio task: lean poll of stream.next(), never blocks on conversion ──
                            let tel_recv = receiver_telemetry
                                .clone()
                                .unwrap_or_else(|| Arc::new(PipelineTelemetry::new()));
                            let auto_interval_recv = Arc::clone(&auto_interval_us);
                            let mut stream = NativeVideoStream::new(video_track.rtc_track());
                            let handle = tokio::spawn(async move {
                                eprintln!("[voice][screen][viewer] receive task started, waiting for first frame...");
                                let mut raw_frame_count: u32 = 0;
                                let mut prev_frame_ts: i64 = 0;
                                let mut wait_start = std::time::Instant::now();
                                // Auto-detect sender FPS: count frames per second, derive expected interval.
                                let mut auto_expected_us: u64 = expected_us_fallback;
                                // EMA of capture timestamp step; matches sender FPS despite bursty network.
                                let mut pacing_slot_ema: u64 = expected_us_fallback;
                                let mut fps_window_frames: u32 = 0;
                                let mut fps_window_start = std::time::Instant::now();
                                while let Some(frame) = stream.next().await {
                                    let receive_wait_us = wait_start.elapsed().as_micros() as u64;
                                    raw_frame_count += 1;
                                    if raw_frame_count == 1 {
                                        eprintln!(
                                            "[voice][screen][viewer] first raw frame {}x{}",
                                            frame.buffer.width(), frame.buffer.height()
                                        );
                                    }
                                    // Update auto-detected expected interval every second.
                                    fps_window_frames += 1;
                                    let fps_elapsed = fps_window_start.elapsed();
                                    if fps_elapsed >= std::time::Duration::from_secs(1) {
                                        let fps = fps_window_frames as f64 / fps_elapsed.as_secs_f64();
                                        if fps > 1.0 {
                                            auto_expected_us = (1_000_000.0 / fps) as u64;
                                            // Do not push auto_expected_us into pacing slot — see pacing_slot_ema.
                                            if crate::telemetry::is_telemetry_enabled() {
                                                eprintln!(
                                                    "[voice][screen][viewer] recv rate: {:.1} fps (ts_step={} us)",
                                                    fps,
                                                    pacing_slot_ema
                                                );
                                            }
                                        }
                                        fps_window_frames = 0;
                                        fps_window_start = std::time::Instant::now();
                                    }
                                    tel_recv.set_recv_wait(receive_wait_us);
                                    tel_recv.inc_recv_frame_count();
                                    if raw_frame_count > 1 {
                                        let ts_delta = (frame.timestamp_us - prev_frame_ts).max(0) as u64;
                                        if pacing_enabled
                                            && ts_delta >= 4_000
                                            && ts_delta <= 50_000
                                        {
                                            // α=1/8: track sender step (~11.1ms @ 90fps); stable under recv_fps 58↔79.
                                            pacing_slot_ema =
                                                (pacing_slot_ema.saturating_mul(7) + ts_delta) / 8;
                                            auto_interval_recv.store(pacing_slot_ema, Ordering::Relaxed);
                                        }
                                        let expected_us = if ts_delta > 0 { ts_delta } else { auto_expected_us };
                                        let network_us = receive_wait_us.saturating_sub(expected_us);
                                        tel_recv.add_receiver_frame(network_us, receive_wait_us, expected_us);
                                        if raw_frame_count <= 5 && telemetry_enabled {
                                            eprintln!(
                                                "[voice][screen][viewer] frame #{} ts_us={} ts_delta={} recv_wait={} network={}",
                                                raw_frame_count, frame.timestamp_us, ts_delta, receive_wait_us, network_us
                                            );
                                        }
                                        let recv_wait_ms = receive_wait_us / 1000;
                                        if recv_wait_ms > 500 {
                                            eprintln!(
                                                "[voice][screen][viewer] LONG_STALL recv_wait_ms={} expected_us={} network_us={} ts_delta={}",
                                                recv_wait_ms, expected_us, network_us, ts_delta
                                            );
                                        } else if recv_wait_ms > 100 {
                                            eprintln!(
                                                "[voice][screen][viewer] STALL recv_wait_ms={} expected_us={} network_us={} ts_delta={}",
                                                recv_wait_ms, expected_us, network_us, ts_delta
                                            );
                                        }
                                    } else {
                                        tel_recv.set_network(0);
                                        if telemetry_enabled {
                                            eprintln!("[voice][screen][viewer] frame #1 ts_us={}", frame.timestamp_us);
                                        }
                                    }
                                    prev_frame_ts = frame.timestamp_us;
                                    if frame_tx.send(frame).is_err() {
                                        eprintln!("[voice][screen][viewer] converter thread gone, stopping receive task");
                                        break;
                                    }
                                    wait_start = std::time::Instant::now();
                                }
                                eprintln!(
                                    "[voice][screen][viewer] receive task ended after {} frames",
                                    raw_frame_count
                                );
                            });
                            video_stream_tasks.insert(task_key, handle);
                        }
                    }
                    Some(RoomEvent::TrackUnsubscribed { track, publication, participant }) => {
                        if track.kind() == TrackKind::Audio {
                            let identity = participant.identity().to_string();
                            if let Some(uid) = parse_remote_user_id(&identity) {
                                let _ = mixer_tx.send(MixerFrameMsg::Remove(MixerRoute {
                                    user_id: uid,
                                    stream: publication.source() == TrackSource::ScreenshareAudio,
                                }));
                            }
                            let task_key = format!(
                                "{}:{}:{}",
                                identity,
                                track_source_label(publication.source()),
                                publication.sid()
                            );
                            if let Some(old) = audio_stream_tasks.remove(&task_key) {
                                old.abort();
                            }
                        }
                        if track.kind() == TrackKind::Video {
                            let identity = participant.identity().to_string();
                            let src = publication.source();
                            let src_str = if src == TrackSource::Screenshare { "screenshare" } else { "camera" };
                            let task_key = format!("{}:{}", identity, src_str);
                            if let Some(old) = video_stream_tasks.remove(&task_key) {
                                old.abort();
                            }
                            if src != TrackSource::Screenshare {
                                let uid: i64 = if let Ok(u) = identity.parse() { u } else { continue };
                                video_frames.lock().remove(&video_frame_key(uid, false));
                            }
                        }
                    }
                    Some(RoomEvent::Disconnected { reason, .. }) => {
                        eprintln!("[voice][livekit] RoomEvent::Disconnected reason={:?}", reason);
                        break;
                    }
                    _ => {}
                }
            }
            cmd = rx.recv() => {
                match cmd {
                    None => {
                        eprintln!("[voice][livekit] session: cmd channel closed (None)");
                        break;
                    }
                    Some(VoiceCmd::Stop) => {
                        eprintln!("[voice][livekit] session: VoiceCmd::Stop received");
                        break;
                    }
                    Some(VoiceCmd::SetMicMuted(m)) => {
                        mic_muted_flag.store(m, Ordering::Relaxed);
                        if m && my_user_id > 0 {
                            speaking.lock().remove(&my_user_id);
                        }
                        if m {
                            mic_publication.mute();
                        } else {
                            mic_publication.unmute();
                        }
                    }
                    Some(VoiceCmd::StartCamera) => {
                        camera_running.store(true, Ordering::Relaxed);
                        let resolution = VideoResolution { width: 1280, height: 720 };
                        let source = NativeVideoSource::new(resolution.clone(), false);
                        let track = LocalVideoTrack::create_video_track("camera", RtcVideoSource::Native(source.clone()));
                        let mut opts = TrackPublishOptions::default();
                        opts.source = TrackSource::Camera;
                        if let Ok(pub_) = lp.publish_track(LocalTrack::Video(track), opts).await {
                            camera_sid = Some(pub_.sid());
                            let running = Arc::clone(&camera_running);
                            std::thread::Builder::new()
                                .name("livekit-camera-capture".into())
                                .spawn(move || {
                                    let _ = run_camera_capture(source, running);
                                })
                                .ok();
                        } else {
                            camera_running.store(false, Ordering::Relaxed);
                        }
                    }
                    Some(VoiceCmd::StopCamera) => {
                        camera_running.store(false, Ordering::Relaxed);
                        if let Some(sid) = camera_sid.take() {
                            let _ = lp.unpublish_track(&sid).await;
                        }
                    }
                    Some(VoiceCmd::StartScreen { source, preset }) => {
                        // Stop any existing capture thread and unpublish old track before starting a new one.
                        // Without unpublishing, viewers stay subscribed to the stale track and see a frozen frame.
                        if let Some(flag) = screen_stop_flag.take() {
                            flag.store(true, Ordering::Relaxed);
                            tokio::time::sleep(Duration::from_millis(80)).await;
                        }
                        if let Some(flag) = screen_audio_stop_flag.take() {
                            flag.store(true, Ordering::Relaxed);
                        }
                        if let Some(handle) = screen_preview_task.take() {
                            handle.abort();
                            let _ = handle.await;
                        }
                        if let Some(handle) = screen_audio_task.take() {
                            let _ = handle.await;
                        }
                        if let Some(sid) = screen_sid.take() {
                            let _ = lp.unpublish_track(&sid).await;
                        }
                        if let Some(sid) = screen_audio_sid.take() {
                            let _ = lp.unpublish_track(&sid).await;
                        }
                        screen_audio_muted_flag = None;
                        screen_fallback_rx = None;
                        let keyframe_requested = Arc::new(AtomicBool::new(false));
                        screen_keyframe_requested = Some(Arc::clone(&keyframe_requested));
                        let stop_flag = Arc::new(AtomicBool::new(false));
                        screen_stop_flag = Some(Arc::clone(&stop_flag));
                        let (width, height, fps, bitrate) = preset.params();
                        let (track_opt, fallback_rx) = start_screen_capture(
                            source.clone(),
                            preset,
                            Arc::clone(&stop_flag),
                            keyframe_requested,
                            Arc::clone(&session_stats),
                        );
                        screen_fallback_rx = fallback_rx;
                        screen_publish_opts_saved = Some((bitrate, fps));
                        if let Some(track) = track_opt {
                            // Update stats for the publisher (who started the stream).
                            {
                                let mut st = session_stats.lock();
                                st.resolution = Some((width, height));
                                st.stream_fps = Some(fps as f32);
                                st.frames_per_second = Some(fps as f32);
                                st.connection_speed_mbps = Some(bitrate as f32 / 1_000_000.0);
                            }
                            let mut opts = TrackPublishOptions::default();
                            opts.source = TrackSource::Screenshare;
                            opts.video_encoding = Some(livekit::options::VideoEncoding {
                                max_bitrate: bitrate,
                                max_framerate: fps,
                            });
                            opts.video_codec = VideoCodec::H264;
                            opts.simulcast = false;
                            if let Ok(pub_) = lp.publish_track(LocalTrack::Video(track), opts).await {
                                screen_sid = Some(pub_.sid());
                                screen_preview_task = Some(spawn_stream_preview_publisher(
                                    lp.clone(),
                                    source.clone(),
                                    Arc::clone(&stop_flag),
                                ));
                                let audio_stop_flag = Arc::new(AtomicBool::new(false));
                                let audio_muted_flag =
                                    Arc::new(AtomicBool::new(screen_audio_muted));
                                if let Some((audio_track, audio_handle)) = start_stream_audio_capture(
                                    &source,
                                    Arc::clone(&audio_stop_flag),
                                    Arc::clone(&audio_muted_flag),
                                ) {
                                    let mut audio_opts = TrackPublishOptions::default();
                                    audio_opts.audio_encoding =
                                        Some(livekit::options::audio::MUSIC_HIGH_QUALITY.encoding.clone());
                                    audio_opts.dtx = false;
                                    audio_opts.red = false;
                                    audio_opts.source = TrackSource::ScreenshareAudio;
                                    match lp
                                        .publish_track(LocalTrack::Audio(audio_track), audio_opts)
                                        .await
                                    {
                                        Ok(pub_) => {
                                            screen_audio_sid = Some(pub_.sid());
                                            screen_audio_task = Some(audio_handle);
                                            screen_audio_stop_flag = Some(audio_stop_flag);
                                            screen_audio_muted_flag = Some(audio_muted_flag);
                                        }
                                        Err(err) => {
                                            eprintln!(
                                                "[voice][screen][audio] publish_track failed: {:?}",
                                                err
                                            );
                                            audio_stop_flag.store(true, Ordering::Relaxed);
                                            let _ = audio_handle.await;
                                        }
                                    }
                                }
                            } else {
                                stop_flag.store(true, Ordering::Relaxed);
                                screen_stop_flag = None;
                                screen_keyframe_requested = None;
                            }
                        }
                    }
                    Some(VoiceCmd::StopScreen) => {
                        if let Some(flag) = screen_stop_flag.take() {
                            flag.store(true, Ordering::Relaxed);
                            tokio::time::sleep(Duration::from_millis(80)).await;
                        }
                        if let Some(flag) = screen_audio_stop_flag.take() {
                            flag.store(true, Ordering::Relaxed);
                        }
                        if let Some(handle) = screen_preview_task.take() {
                            handle.abort();
                            let _ = handle.await;
                        }
                        if let Some(handle) = screen_audio_task.take() {
                            let _ = handle.await;
                        }
                        if let Some(sid) = screen_sid.take() {
                            let _ = lp.unpublish_track(&sid).await;
                        }
                        if let Some(sid) = screen_audio_sid.take() {
                            let _ = lp.unpublish_track(&sid).await;
                        }
                        screen_audio_muted_flag = None;
                        screen_keyframe_requested = None;
                        // Keep last resolution/fps/bitrate visible; clear stream_fps so UI shows stream stopped.
                        {
                            let mut st = session_stats.lock();
                            st.stream_fps = None;
                            st.frames_per_second = None;
                            st.encoding_path = None;
                            st.encoder_threads = None;
                        }
                    }
                    Some(VoiceCmd::SetUserVolume(uid, vol)) => {
                        user_volumes.lock().insert(uid, vol.clamp(0.0, 3.0));
                    }
                    Some(VoiceCmd::SetRemoteVoiceDenoise { user_id, enabled }) => {
                        let flag = remote_voice_denoise_flags
                            .lock()
                            .entry(user_id)
                            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
                            .clone();
                        flag.store(enabled, Ordering::Relaxed);
                    }
                    Some(VoiceCmd::SetStreamVolume(uid, vol)) => {
                        stream_volumes.lock().insert(uid, vol.clamp(0.0, 4.0));
                    }
                    Some(VoiceCmd::SetStreamSubscription { user_id, subscribed }) => {
                        if subscribed {
                            desired_stream_subscriptions.insert(user_id);
                        } else {
                            desired_stream_subscriptions.remove(&user_id);
                        }
                        let mut video_publication = stream_video_publications.get(&user_id).cloned();
                        let mut audio_publication = stream_audio_publications.get(&user_id).cloned();
                        if video_publication.is_none() || audio_publication.is_none() {
                            if let Some(participant) = room
                                .remote_participants()
                                .into_values()
                                .find(|participant| {
                                    parse_remote_user_id(&participant.identity().to_string())
                                        == Some(user_id)
                                })
                            {
                                for publication in participant.track_publications().into_values() {
                                    match publication.source() {
                                        TrackSource::Screenshare => {
                                            stream_video_publications
                                                .insert(user_id, publication.clone());
                                            video_publication = Some(publication);
                                        }
                                        TrackSource::ScreenshareAudio => {
                                            stream_audio_publications
                                                .insert(user_id, publication.clone());
                                            audio_publication = Some(publication);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        if let Some(publication) = video_publication.as_ref() {
                            apply_publication_subscription(publication, subscribed);
                        }
                        if let Some(publication) = audio_publication.as_ref() {
                            apply_publication_subscription(publication, subscribed);
                        }
                        if subscribed {
                            let packet = DataPacket {
                                payload: vec![1],
                                topic: Some(STREAM_KEYFRAME_REQUEST_TOPIC.to_string()),
                                reliable: true,
                                destination_identities: vec![ParticipantIdentity(user_id.to_string())],
                            };
                            if let Err(err) = lp.publish_data(packet).await {
                                eprintln!(
                                    "[voice][stream] failed to request keyframe from {}: {:?}",
                                    user_id,
                                    err
                                );
                            }
                        }
                    }
                    Some(VoiceCmd::SetScreenAudioMuted(muted)) => {
                        screen_audio_muted = muted;
                        if let Some(flag) = screen_audio_muted_flag.as_ref() {
                            flag.store(muted, Ordering::Relaxed);
                        }
                    }
                    Some(VoiceCmd::SetOutputMuted(m)) => {
                        output_muted.store(m, Ordering::Relaxed);
                    }
                    Some(VoiceCmd::SetOutputVolume(vol)) => {
                        *output_volume.lock() = vol.clamp(0.0, 4.0);
                    }
                    Some(VoiceCmd::SetInputVolume(vol)) => {
                        *input_volume.lock() = vol.clamp(0.0, 4.0);
                    }
                    Some(VoiceCmd::SetInputSensitivity(value)) => {
                        *input_sensitivity.lock() = value.clamp(0.0, 1.0);
                    }
                    _ => {}
                }
            }
            // Encoded-path -> CPU fallback: encoder thread sent a Native track because
            // the encoded backend hard-failed.
            // Unpublish the Encoded track and republish the Native (I420) one.
            fallback_track = async {
                match screen_fallback_rx.as_mut() {
                    Some(rx) => rx.await.ok(),
                    None => {
                        // No fallback pending — park this branch forever so select! doesn't spin.
                        std::future::pending::<Option<LocalVideoTrack>>().await
                    }
                }
            }, if screen_fallback_rx.is_some() => {
                screen_fallback_rx = None;
                if let Some(track) = fallback_track {
                    eprintln!("[voice][screen] Republishing screen track as Native (encoded-path -> CPU fallback)");
                    if let Some(sid) = screen_sid.take() {
                        let _ = lp.unpublish_track(&sid).await;
                    }
                    if let Some((bitrate, fps)) = screen_publish_opts_saved {
                        let mut opts = TrackPublishOptions::default();
                        opts.source = TrackSource::Screenshare;
                        opts.video_encoding = Some(livekit::options::VideoEncoding {
                            max_bitrate: bitrate,
                            max_framerate: fps,
                        });
                        opts.video_codec = VideoCodec::H264;
                        opts.simulcast = false;
                        if let Ok(pub_) = lp.publish_track(LocalTrack::Video(track), opts).await {
                            screen_sid = Some(pub_.sid());
                            eprintln!("[voice][screen] Fallback Native track published OK");
                        }
                    }
                }
            }
        }
    }

    // ── Cleanup ──────────────────────────────────────────────────────────────────────────────
    mic_stop.store(true, Ordering::Relaxed);
    if let Some(flag) = screen_audio_stop_flag.take() {
        flag.store(true, Ordering::Relaxed);
    }
    if let Some(flag) = screen_stop_flag.take() {
        flag.store(true, Ordering::Relaxed);
        // Let the detached screen/encoder thread observe the stop flag and
        // release its EncodedVideoTrackSource clone before the next join.
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    if let Some(handle) = screen_preview_task.take() {
        handle.abort();
        let _ = handle.await;
    }
    if let Some(handle) = screen_audio_task.take() {
        let _ = handle.await;
    }
    if let Some(sid) = screen_audio_sid.take() {
        let _ = lp.unpublish_track(&sid).await;
    }
    if let Some(sid) = screen_sid.take() {
        let _ = lp.unpublish_track(&sid).await;
    }
    screen_audio_muted_flag = None;
    let mut room_stream_tasks = Vec::with_capacity(
        video_stream_tasks
            .len()
            .saturating_add(audio_stream_tasks.len()),
    );
    room_stream_tasks.extend(video_stream_tasks.drain().map(|(_, handle)| handle));
    room_stream_tasks.extend(audio_stream_tasks.drain().map(|(_, handle)| handle));
    for handle in &room_stream_tasks {
        handle.abort();
    }
    for handle in room_stream_tasks {
        let _ = handle.await;
    }
    mic_timer_task.abort();
    let _ = mic_timer_task.await;
    mixer_task.abort();
    let _ = mixer_task.await;
    speaker_stop.store(true, Ordering::Relaxed);
    if let Some(handle) = speaker_thread {
        handle.thread().unpark();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = handle.join();
        })
        .await;
    }
    let _ = tokio::task::spawn_blocking(move || {
        let _ = mic_thread.join();
    })
    .await;
    let _ = room.close().await;
    drop(room);
    Ok(())
}

/// Mixer: receives (user_id, samples) from remote tracks, applies volume, mixes every 10 ms.
async fn run_remote_audio_mixer(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<MixerFrameMsg>,
    interval: &mut tokio::time::Interval,
    voice_volumes: Arc<Mutex<HashMap<i64, f32>>>,
    stream_volumes: Arc<Mutex<HashMap<i64, f32>>>,
    output_muted: Arc<AtomicBool>,
    output_volume: Arc<Mutex<f32>>,
    output_buffer: Arc<Mutex<VecDeque<i16>>>,
    output_sample_rate: u32,
) {
    let output_10ms_len = (output_sample_rate / 100).max(1) as usize;
    let need_resample = output_10ms_len != SAMPLES_PER_10MS;
    let mut latest: HashMap<MixerRoute, Vec<i16>> = HashMap::new();
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    None => break,
                    Some(MixerFrameMsg::Frame(route, samples)) => {
                        if samples.len() >= SAMPLES_PER_10MS {
                            latest.insert(route, samples);
                        }
                    }
                    Some(MixerFrameMsg::Remove(route)) => {
                        latest.remove(&route);
                    }
                }
            }
            _ = interval.tick() => {
                let voice_map = voice_volumes.lock().clone();
                let stream_map = stream_volumes.lock().clone();
                let out_vol = *output_volume.lock();
                let muted = output_muted.load(Ordering::Relaxed);
                let mixed: Vec<i16> = if muted || latest.is_empty() {
                    vec![0i16; SAMPLES_PER_10MS]
                } else {
                    let mut out = vec![0i32; SAMPLES_PER_10MS];
                    for (route, samples) in &latest {
                        let gain = if route.stream {
                            stream_map.get(&route.user_id).copied().unwrap_or(1.0)
                        } else {
                            voice_map.get(&route.user_id).copied().unwrap_or(1.0)
                        } * out_vol;
                        for (i, &s) in samples.iter().take(SAMPLES_PER_10MS).enumerate() {
                            out[i] += (s as f32 * gain) as i32;
                        }
                    }
                    out.into_iter()
                        .map(|s| s.clamp(-32768, 32767) as i16)
                        .collect()
                };
                let to_push: Vec<i16> = if need_resample {
                    resample_linear(&mixed, output_10ms_len)
                } else {
                    mixed
                };
                let max_buffer_len = output_10ms_len * 24;
                let mut out = output_buffer.lock();
                out.extend(to_push);
                if out.len() > max_buffer_len {
                    let drop_count = out.len() - max_buffer_len;
                    for _ in 0..drop_count {
                        out.pop_front();
                    }
                }
            }
        }
    }
}

/// Speaker output: read mono i16 from output_buffer, play via cpal.
fn run_speaker_output(
    output_buffer: Arc<Mutex<VecDeque<i16>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no output device")?;
    let config = preferred_output_config_48k(&device)?;
    let channels = config.channels as usize;
    eprintln!(
        "[voice][livekit] speaker: {} Hz, {} channel(s)",
        config.sample_rate.0, channels
    );
    let err_fn = |e| eprintln!("[voice][livekit] speaker error: {}", e);
    let buf = Arc::clone(&output_buffer);
    let mut last_sample = 0.0f32;
    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut guard = buf.lock();
                let frames = data.len() / channels.max(1);
                for i in 0..frames {
                    let s = if let Some(sample) = guard.pop_front() {
                        last_sample = sample as f32 / 32768.0;
                        last_sample
                    } else {
                        last_sample *= 0.85;
                        if last_sample.abs() < 0.0005 {
                            last_sample = 0.0;
                        }
                        last_sample
                    };
                    for c in 0..channels {
                        data[i * channels + c] = s;
                    }
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    while !stop.load(Ordering::Relaxed) {
        std::thread::park_timeout(Duration::from_millis(100));
    }
    Ok(())
}

/// Prefer 48 kHz output config; fallback to default.
fn preferred_output_config_48k(device: &cpal::Device) -> Result<cpal::StreamConfig, String> {
    use cpal::traits::DeviceTrait;
    use cpal::SampleRate;
    for range in device
        .supported_output_configs()
        .map_err(|e| e.to_string())?
    {
        if let Some(supported) = range.try_with_sample_rate(SampleRate(SAMPLE_RATE)) {
            return Ok(supported.config());
        }
    }
    device
        .default_output_config()
        .map_err(|e| e.to_string())
        .map(|c| c.config())
}

fn get_output_sample_rate() -> Option<u32> {
    use cpal::traits::HostTrait;
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    let config = preferred_output_config_48k(&device).ok()?;
    Some(config.sample_rate.0)
}

/// Cache for D3D11 I420→RGBA converter (decode path). One per (width, height); recreated when size changes.
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
static DECODE_CONVERTER: parking_lot::Mutex<Option<(u32, u32, D3d11I420ToRgba)>> =
    parking_lot::Mutex::new(None);

/// Phase 3.4: Cache for D3D11 NV12→RGBA converter (MFT hardware decode path).
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
static DECODE_CONVERTER_NV12: parking_lot::Mutex<Option<D3d11Nv12ToRgba>> =
    parking_lot::Mutex::new(None);

/// BT.709 limited-range CPU fallback for D3D11TextureVideoFrameBuffer (MFT hardware decode path).
///
/// Hardware-decoded NV12 surfaces from MFT/DXVA arrive as limited-range YUV in practice, so the
/// viewer-side fallback must expand them before applying the BT.709 matrix.
///
/// Called when the GPU compute shader path fails or when decode_gpu_failed() is set.
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn d3d11_nv12_to_rgba_cpu(
    vfb_ptr: &cxx::UniquePtr<VideoFrameBuffer>,
    width: u32,
    height: u32,
    full_range: bool,
) -> Option<Vec<u8>> {
    // to_i420() → D3D11TextureVideoFrameBuffer::ToI420(): GPU→CPU staging copy of NV12,
    // then NV12ToI420Scale (deinterleaves UV without range conversion).
    // Result: limited-range I420 byte values (Y: 16-235, U/V: neutral=128).
    let i420 = unsafe { vfb_ptr.as_ref()?.to_i420() };
    if i420.is_null() {
        return None;
    }
    let i420_ref = i420.as_ref()?;

    let yuv8_ptr = unsafe { i420_to_yuv8(i420_ref as *const _) };
    let yuv_ptr = unsafe { yuv8_to_yuv(yuv8_ptr) };
    if yuv8_ptr.is_null() || yuv_ptr.is_null() {
        return None;
    }
    let yuv8 = unsafe { &*yuv8_ptr };
    let yuv = unsafe { &*yuv_ptr };

    let stride_y = yuv.stride_y() as usize;
    let stride_u = yuv.stride_u() as usize;
    let stride_v = yuv.stride_v() as usize;
    let h = height as usize;
    let w = width as usize;

    let y_data = unsafe { std::slice::from_raw_parts(yuv8.data_y(), stride_y * h) };
    let u_data = unsafe { std::slice::from_raw_parts(yuv8.data_u(), stride_u * ((h + 1) / 2)) };
    let v_data = unsafe { std::slice::from_raw_parts(yuv8.data_v(), stride_v * ((h + 1) / 2)) };

    let mut rgba = vec![0u8; w * h * 4];
    for py in 0..h {
        for px in 0..w {
            let y_raw = y_data[py * stride_y + px] as f32;
            let u_raw = u_data[(py / 2) * stride_u + px / 2] as f32;
            let v_raw = v_data[(py / 2) * stride_v + px / 2] as f32;
            let (y, u, v) = if full_range {
                (y_raw, u_raw - 128.0, v_raw - 128.0)
            } else {
                (
                    (y_raw - 16.0) * (255.0 / 219.0),
                    (u_raw - 128.0) * (255.0 / 224.0),
                    (v_raw - 128.0) * (255.0 / 224.0),
                )
            };
            // BT.709 matrix
            let r = (y + 1.5748 * v).clamp(0.0, 255.0) as u8;
            let g = (y - 0.1873 * u - 0.4681 * v).clamp(0.0, 255.0) as u8;
            let b = (y + 1.8556 * u).clamp(0.0, 255.0) as u8;
            let idx = (py * w + px) * 4;
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = 255;
        }
    }
    Some(rgba)
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn select_nv12_full_range(frame: &BoxVideoFrame) -> bool {
    if let Ok(val) = std::env::var("ASTRIX_VIDEO_NV12_FULL_RANGE") {
        return val == "1" || val.eq_ignore_ascii_case("true");
    }
    frame
        .color_space
        .as_ref()
        .map(|cs| cs.is_full_range())
        .unwrap_or(false)
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub(crate) fn wgl_zero_copy_enabled() -> bool {
    if let Ok(v) = std::env::var("ASTRIX_VIDEO_DISABLE_WGL_INTEROP") {
        return !(v == "1" || v.eq_ignore_ascii_case("true"));
    }
    if let Ok(v) = std::env::var("ASTRIX_VIDEO_ENABLE_WGL_INTEROP") {
        return v == "1" || v.eq_ignore_ascii_case("true");
    }
    true
}

/// Convert a LiveKit video frame to RGBA for egui. Tries D3D11 I420→RGBA when buffer is I420 and GPU is available; else CPU to_argb.
/// Returns (width, height, rgba, used_gpu, shared_handle) where:
///   - used_gpu: true when D3D11 compute shader was used
///   - shared_handle: Some(Win32 HANDLE) for GPU zero-copy path (Phase 3.5, WGL_NV_DX_interop2)
///
/// libyuv naming vs memory layout on little-endian:
///   ARGB → [B,G,R,A]   BGRA → [A,R,G,B]
///   RGBA → [A,B,G,R]   ABGR → [R,G,B,A]  ← egui wants [R,G,B,A]
fn video_frame_to_rgba(frame: &BoxVideoFrame) -> Option<(u32, u32, Vec<u8>, bool, Option<usize>)> {
    let buf = frame.buffer.as_ref();
    let width = buf.width();
    let height = buf.height();
    if width == 0 || height == 0 {
        return None;
    }

    // ASTRIX_DECODE_RGBA_CPU=1: force CPU path. Cached via OnceLock (was syscall per frame).
    static FORCE_CPU_RGBA: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let force_cpu_rgba = *FORCE_CPU_RGBA.get_or_init(|| {
        std::env::var("ASTRIX_DECODE_RGBA_CPU")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    });

    #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
    {
        let nv12_full_range = select_nv12_full_range(frame);
        static LOGGED_FRAME_COLORSPACE: AtomicBool = AtomicBool::new(false);
        if is_telemetry_enabled() && !LOGGED_FRAME_COLORSPACE.swap(true, Ordering::Relaxed) {
            eprintln!(
                "[d3d11_rgba] Frame color metadata: present={} matrix_id={:?} range_id={:?} selected_full_range={} wgl_zero_copy={}",
                frame.color_space.is_some(),
                frame.color_space.as_ref().map(|cs| cs.matrix_id),
                frame.color_space.as_ref().map(|cs| cs.range_id),
                nv12_full_range,
                wgl_zero_copy_enabled(),
            );
        }
        if !force_cpu_rgba && !decode_gpu_failed() {
            // Phase 3.4: D3D11 texture path (MFT hardware decoder → NV12 D3D11 texture)
            if let Some(native) = buf.as_native() {
                let vfb_ptr = native.video_frame_buffer_unique_ptr();
                if video_frame_buffer_is_d3d11(vfb_ptr) {
                    let tex_ptr = video_frame_buffer_get_d3d11_texture(vfb_ptr);
                    let subresource = video_frame_buffer_get_d3d11_subresource(vfb_ptr);
                    if tex_ptr != 0 {
                        use std::mem::ManuallyDrop;
                        use windows::core::Interface;
                        use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
                        let texture = unsafe {
                            ManuallyDrop::new(ID3D11Texture2D::from_raw(tex_ptr as *mut _))
                        };
                        // Phase 3.5: use GPU zero-copy path (WGL_NV_DX_interop2) when available,
                        // otherwise fall back to CPU readback (convert_to_rgba_bytes).
                        let use_zero_copy = wgl_zero_copy_enabled()
                            && crate::d3d11_gl_interop::GL_INTEROP_AVAILABLE
                                .load(std::sync::atomic::Ordering::Acquire);

                        let result = {
                            let mut conv_guard = DECODE_CONVERTER_NV12.lock();
                            if conv_guard
                                .as_ref()
                                .map(|conv| conv.full_range() != nv12_full_range)
                                .unwrap_or(false)
                            {
                                if is_telemetry_enabled() {
                                    eprintln!(
                                        "[voice][screen][viewer] NV12 color-space change: recreating converter full_range={} range_id={:?} matrix_id={:?}",
                                        nv12_full_range,
                                        frame.color_space.as_ref().map(|cs| cs.range_id),
                                        frame.color_space.as_ref().map(|cs| cs.matrix_id),
                                    );
                                }
                                *conv_guard = None;
                            }
                            if conv_guard.is_none() {
                                // Use the same D3D11 device as the MFT decoder so the NV12
                                // texture (produced by that device) can be CopySubresourceRegion'd
                                // directly without cross-device errors.
                                let init_result = crate::mft_device::get_shared_device()
                                    .ok_or_else(|| "shared device not initialized".to_string())
                                    .and_then(|dev| {
                                        D3d11Nv12ToRgba::new(&dev, nv12_full_range)
                                            .map_err(|e| format!("{:?}", e))
                                    });
                                match init_result {
                                    Ok(c) => *conv_guard = Some(c),
                                    Err(e) => {
                                        eprintln!("[voice][screen][viewer] D3D11 NV12→RGBA init failed: {}, using BT.709 CPU path", e);
                                        mark_decode_gpu_failed();
                                        // kNative D3D11 buffer: NEVER call video_frame_to_rgba_cpu —
                                        // its to_argb() calls ToI420() which may return nullptr on
                                        // staging failure, causing a null-deref crash.
                                        return d3d11_nv12_to_rgba_cpu(
                                            vfb_ptr,
                                            width,
                                            height,
                                            nv12_full_range,
                                        )
                                        .map(|v| (width, height, v, false, None));
                                    }
                                }
                            }
                            let conv = conv_guard.as_mut().unwrap();
                            if use_zero_copy {
                                // GPU zero-copy: compute NV12→RGBA on GPU, share via WGL interop.
                                match conv.convert(&texture, subresource, width, height) {
                                    Ok(_) => {
                                        let handle = conv.get_shared_handle();
                                        Ok((width, height, Vec::new(), handle))
                                    }
                                    Err(e) => Err(e),
                                }
                            } else {
                                // CPU readback: convert on GPU, Map to CPU for egui ColorImage.
                                conv.convert_to_rgba_bytes(&texture, subresource, width, height)
                                    .map(|(w, h, rgba)| (w, h, rgba, None))
                            }
                        };
                        match result {
                            Ok((w, h, rgba, shared_handle)) => {
                                return Some((w, h, rgba, true, shared_handle));
                            }
                            Err(e) => {
                                eprintln!("[voice][screen][viewer] D3D11 NV12→RGBA failed: {:?}, falling back to BT.709 CPU permanently", e);
                                mark_decode_gpu_failed();
                                // kNative D3D11 buffer: NEVER call video_frame_to_rgba_cpu —
                                // its to_argb() calls ToI420() which may return nullptr on
                                // staging failure, causing a null-deref crash.
                                return d3d11_nv12_to_rgba_cpu(
                                    vfb_ptr,
                                    width,
                                    height,
                                    nv12_full_range,
                                )
                                .map(|v| (width, height, v, false, None));
                            }
                        }
                    }
                }
            }

            if let Some(i420) = buf.as_i420() {
                let (y_plane, u_plane, v_plane) = i420.data();
                let result = {
                    let conv_guard = DECODE_CONVERTER.lock();
                    if conv_guard
                        .as_ref()
                        .map(|t| t.0 == width && t.1 == height)
                        .unwrap_or(false)
                    {
                        conv_guard
                            .as_ref()
                            .unwrap()
                            .2
                            .convert(y_plane, u_plane, v_plane)
                    } else {
                        drop(conv_guard);
                        let new_conv = match D3d11I420ToRgba::new(width, height) {
                            Ok(c) => c,
                            Err(e) => {
                                eprintln!("[voice][screen][viewer] D3D11 I420→RGBA init failed: {:?}, using CPU path", e);
                                mark_decode_gpu_failed();
                                return video_frame_to_rgba_cpu(buf, width, height)
                                    .map(|(w, h, rgba)| (w, h, rgba, false, None));
                            }
                        };
                        *DECODE_CONVERTER.lock() = Some((width, height, new_conv));
                        DECODE_CONVERTER
                            .lock()
                            .as_ref()
                            .unwrap()
                            .2
                            .convert(y_plane, u_plane, v_plane)
                    }
                };
                match result {
                    Ok((w, h, rgba)) => return Some((w, h, rgba, true, None)),
                    Err(e) => {
                        eprintln!("[voice][screen][viewer] D3D11 I420→RGBA failed: {:?}, falling back to CPU permanently", e);
                        mark_decode_gpu_failed();
                        return video_frame_to_rgba_cpu(buf, width, height)
                            .map(|(w, h, rgba)| (w, h, rgba, false, None));
                    }
                }
            }
        }
    }

    // D3D11 BT.709 CPU fallback for frames arriving after decode_gpu_failed() was set.
    // decode_gpu_failed() causes the GPU block above to be skipped entirely; without this
    // check, D3D11TextureVideoFrameBuffer (limited-range NV12) would go through
    // video_frame_to_rgba_cpu → libyuv I420ToABGR (full-range) → washed-out image.
    //
    // For kNative D3D11 buffers we MUST return here (Some or None) and never fall through
    // to video_frame_to_rgba_cpu: its to_argb() calls ToI420() which returns nullptr on
    // staging failure, and WebRTC then dereferences nullptr → crash.
    #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
    if !force_cpu_rgba {
        if let Some(native) = buf.as_native() {
            let vfb_ptr = native.video_frame_buffer_unique_ptr();
            if video_frame_buffer_is_d3d11(vfb_ptr) {
                return d3d11_nv12_to_rgba_cpu(
                    vfb_ptr,
                    width,
                    height,
                    select_nv12_full_range(frame),
                )
                .map(|rgba| (width, height, rgba, false, None));
            }
        }
    }

    video_frame_to_rgba_cpu(buf, width, height).map(|(w, h, rgba)| (w, h, rgba, false, None))
}

fn video_frame_to_rgba_cpu(
    buf: &dyn livekit::webrtc::video_frame::VideoBuffer,
    width: u32,
    height: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let dst_stride = width * 4;
    buf.to_argb(
        VideoFormatType::ABGR,
        &mut rgba,
        dst_stride,
        width as i32,
        height as i32,
    );
    Some((width, height, rgba))
}

/// Camera capture stub: grey gradient frames until a real camera source is wired up.
fn run_camera_capture(source: NativeVideoSource, running: Arc<AtomicBool>) -> Result<(), String> {
    use livekit::webrtc::video_frame::I420Buffer;
    let resolution = source.video_resolution();
    let w = resolution.width;
    let h = resolution.height;
    let mut frame_count: i64 = 0;
    while running.load(Ordering::Relaxed) {
        let mut i420 = I420Buffer::new(w, h);
        let (y, u, v) = i420.data_mut();
        for i in 0..(w * h) as usize {
            if i < y.len() {
                y[i] = (i % 256) as u8;
            }
        }
        let uv_len = ((w + 1) / 2) * ((h + 1) / 2);
        for i in 0..uv_len as usize {
            if i < u.len() {
                u[i] = 128;
            }
            if i < v.len() {
                v[i] = 128;
            }
        }
        frame_count += 1;
        let frame = VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us: frame_count * 16_667,
            color_space: None,
            buffer: i420,
        };
        source.capture_frame(&frame);
        std::thread::sleep(Duration::from_millis(16));
    }
    Ok(())
}

/// Enumerate physical monitors using xcap.
pub fn enumerate_unique_screens() -> Vec<Monitor> {
    Monitor::all().unwrap_or_default()
}

/// Reusable buffers for xcap → I420 path: scale-first (RGBA→target size) then convert to I420.
/// Avoids full-res I420 allocation; one I420 (target size) per frame when returning.
/// Compiled on all platforms so the benchmark test can run on Windows (with wgc).
struct XcapI420Buffers {
    dst_w: u32,
    dst_h: u32,
    scaled_rgba: Vec<u8>,
    scaled_i420: livekit::webrtc::video_frame::I420Buffer,
}

impl XcapI420Buffers {
    fn new(dst_w: u32, dst_h: u32) -> Self {
        use livekit::webrtc::video_frame::I420Buffer;
        let scaled_rgba = vec![0u8; (dst_w * dst_h * 4) as usize];
        Self {
            dst_w,
            dst_h,
            scaled_rgba,
            scaled_i420: I420Buffer::new(dst_w, dst_h),
        }
    }

    fn ensure_size(&mut self, dst_w: u32, dst_h: u32) {
        use livekit::webrtc::video_frame::I420Buffer;
        if self.dst_w != dst_w || self.dst_h != dst_h {
            self.dst_w = dst_w;
            self.dst_h = dst_h;
            self.scaled_rgba.resize((dst_w * dst_h * 4) as usize, 0);
            self.scaled_i420 = I420Buffer::new(dst_w, dst_h);
        }
    }

    /// Convert RGBA to I420 at target resolution using scale-first + buffer reuse.
    /// Returns a VideoFrame with I420; internal buffer is replaced for next call.
    fn rgba_to_i420_scaled_reuse(
        &mut self,
        rgba: &[u8],
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
        frame_count: i64,
    ) -> Option<livekit::webrtc::video_frame::VideoFrame<livekit::webrtc::video_frame::I420Buffer>>
    {
        use livekit::webrtc::native::yuv_helper;
        use livekit::webrtc::video_frame::I420Buffer;

        if rgba.len() < (src_w * src_h * 4) as usize {
            return None;
        }
        self.ensure_size(dst_w, dst_h);

        let (y, u, v) = self.scaled_i420.data_mut();
        let stride_y = dst_w;
        let stride_uv = (dst_w + 1) / 2;

        if src_w == dst_w && src_h == dst_h {
            yuv_helper::abgr_to_i420(
                rgba,
                src_w * 4,
                y,
                stride_y,
                u,
                stride_uv,
                v,
                stride_uv,
                dst_w as i32,
                dst_h as i32,
            );
        } else {
            box_scale_rgba(rgba, src_w, src_h, &mut self.scaled_rgba, dst_w, dst_h);
            yuv_helper::abgr_to_i420(
                &self.scaled_rgba,
                dst_w * 4,
                y,
                stride_y,
                u,
                stride_uv,
                v,
                stride_uv,
                dst_w as i32,
                dst_h as i32,
            );
        }

        let out = std::mem::replace(&mut self.scaled_i420, I420Buffer::new(dst_w, dst_h));
        let ts = frame_count * 16_667;
        Some(livekit::webrtc::video_frame::VideoFrame {
            rotation: VideoRotation::VideoRotation0,
            timestamp_us: ts,
            color_space: None,
            buffer: out,
        })
    }
}

// Lock-free ring buffer (size 3) for RawFrame: WGC pushes, encoder pops.
// Reduces skipped frames and FPS jitter when encoder briefly lags (~+16–33 ms latency).

/// Scale RGBA to target resolution using libyuv (SIMD). Pipeline: RGBA full-res
/// → abgr_to_i420 → I420 full-res → I420Buffer::scale (libyuv) → I420 target-res.
/// Avoids slow image::resize; keeps pipeline RGBA→I420→scale for 60 fps.
fn scale_rgba_to_target_libyuv(
    rgba: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Option<livekit::webrtc::video_frame::I420Buffer> {
    if rgba.len() < (src_w * src_h * 4) as usize {
        return None;
    }
    let mut i420_full = rgba_to_i420(rgba, src_w * 4, src_w, src_h)?;
    if src_w == dst_w && src_h == dst_h {
        Some(i420_full)
    } else {
        Some(i420_full.scale(dst_w as i32, dst_h as i32))
    }
}

/// Convert RGBA (ABGR byte order for libyuv) to I420 at the given size.
fn rgba_to_i420(
    rgba: &[u8],
    stride: u32,
    w: u32,
    h: u32,
) -> Option<livekit::webrtc::video_frame::I420Buffer> {
    use livekit::webrtc::native::yuv_helper;
    use livekit::webrtc::video_frame::I420Buffer;

    if rgba.len() < (stride * h) as usize {
        return None;
    }
    let mut i420 = I420Buffer::new(w, h);
    let (y, u, v) = i420.data_mut();
    yuv_helper::abgr_to_i420(
        rgba,
        stride,
        y,
        w,
        u,
        (w + 1) / 2,
        v,
        (w + 1) / 2,
        w as i32,
        h as i32,
    );
    Some(i420)
}

/// Full pipeline: scale (if needed) via libyuv then RGBA→I420. Used when caller does not need separate timing.
fn convert_rgba_to_i420(
    rgba: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    _scaled_buf: &mut Vec<u8>,
) -> Option<livekit::webrtc::video_frame::I420Buffer> {
    if src_w == dst_w && src_h == dst_h {
        rgba_to_i420(rgba, src_w * 4, src_w, src_h)
    } else {
        scale_rgba_to_target_libyuv(rgba, src_w, src_h, dst_w, dst_h)
    }
}

/// Start screen capture using Windows Graphics Capture API (GPU-accelerated, low latency).
///
/// Architecture (two-thread pipeline):
///   WGC callback  — GPU→CPU copy; atomically swaps new RawFrame into slot (lock-free).
///   Encoder thread — fires at preset FPS; steals latest frame; runs:
///                    ABGRScale (RGBA→RGBA at target res) → abgr_to_i420 → publish.
///
/// Pipeline order (CRITICAL for performance):
///   RGBA full-res → ABGRScale → RGBA target-res → abgr_to_i420 → I420 → publish
///   (Old order was RGBA→I420 at full-res, then I420::scale — 2–3× more CPU work)
///
/// Warmup: first 10 frames published at 15 fps to let LiveKit BWE stabilise before
///   full framerate, eliminating startup stutter.
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn start_screen_capture_xcap(
    source_target: StreamSourceTarget,
    preset: ScreenPreset,
    stop_flag: Arc<AtomicBool>,
    _keyframe_requested: Arc<AtomicBool>,
    session_stats: Arc<Mutex<VoiceSessionStats>>,
) -> (
    Option<LocalVideoTrack>,
    Option<tokio::sync::oneshot::Receiver<LocalVideoTrack>>,
) {
    let (video_width, video_height, max_fps, bitrate) = preset.params();
    {
        let mut st = session_stats.lock();
        st.resolution = Some((video_width, video_height));
        st.stream_fps = Some(max_fps as f32);
        st.frames_per_second = Some(max_fps as f32);
        st.connection_speed_mbps = Some(bitrate as f32 / 1_000_000.0);
        st.encoding_path = Some(
            EncodingPath::OpenH264 {
                threads: encoder_threads_for_resolution(video_width, video_height),
                gpu_capture: false,
            }
            .to_display_string(),
        );
        st.encoder_threads = Some(encoder_threads_for_resolution(video_width, video_height));
    }
    let frame_interval_ms = (1000.0 / max_fps).round() as u64;
    let resolution = VideoResolution {
        width: video_width,
        height: video_height,
    };
    let source = NativeVideoSource::new(resolution, true);
    let track =
        LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(source.clone()));

    if let StreamSourceTarget::Window {
        window_id,
        process_id,
    } = source_target.clone()
    {
        let stop_for_thread = Arc::clone(&stop_flag);
        std::thread::Builder::new()
            .name("livekit-window-xcap".into())
            .spawn(move || {
                let Some(window) = find_stream_window(window_id, process_id) else {
                    eprintln!(
                        "[voice][screen] xcap window not found: hwnd={} pid={}",
                        window_id, process_id
                    );
                    return;
                };
                eprintln!(
                    "[voice][screen] xcap capturing window '{}' [{}] ({}x{})",
                    window.title(),
                    window.app_name(),
                    window.width(),
                    window.height()
                );

                let mut frame_count: i64 = 0;
                let mut i420_buffers = XcapI420Buffers::new(video_width, video_height);
                let mut fps_window_frames: u32 = 0;
                let mut fps_window_start = std::time::Instant::now();
                while !stop_for_thread.load(Ordering::Relaxed) {
                    let t0 = std::time::Instant::now();
                    match window.capture_image() {
                        Ok(img) => {
                            let src_w = img.width();
                            let src_h = img.height();
                            let raw = img.as_raw();
                            if let Some(vf) = i420_buffers.rgba_to_i420_scaled_reuse(
                                raw,
                                src_w,
                                src_h,
                                video_width,
                                video_height,
                                frame_count,
                            ) {
                                source.capture_frame(&vf);
                                frame_count += 1;
                                fps_window_frames += 1;
                            }
                        }
                        Err(e) => eprintln!("[voice][screen] window capture error: {}", e),
                    }
                    let elapsed = fps_window_start.elapsed();
                    if elapsed >= Duration::from_secs(1) {
                        let actual_fps =
                            fps_window_frames as f32 / elapsed.as_secs_f32().max(0.001);
                        eprintln!(
                            "[voice][screen] xcap capture_frame rate: {:.1} fps (target {})",
                            actual_fps, max_fps
                        );
                        fps_window_frames = 0;
                        fps_window_start = std::time::Instant::now();
                    }
                    let elapsed_ms = t0.elapsed().as_millis() as u64;
                    let to_sleep = frame_interval_ms.saturating_sub(elapsed_ms);
                    if to_sleep > 0 {
                        std::thread::sleep(Duration::from_millis(to_sleep));
                    }
                }
                eprintln!("[voice][screen] xcap capture thread stopped");
            })
            .ok();

        return (Some(track), None);
    }

    let screen_index = match source_target {
        StreamSourceTarget::Monitor { index } => Some(index),
        StreamSourceTarget::Window { .. } => None,
    };

    let stop_for_thread = Arc::clone(&stop_flag);
    std::thread::Builder::new()
        .name("livekit-screen-xcap".into())
        .spawn(move || {
            let monitors = enumerate_unique_screens();
            if monitors.is_empty() {
                eprintln!("[voice][screen] no monitors found");
                return;
            }
            let idx = screen_index.unwrap_or(0).min(monitors.len() - 1);
            let monitor = monitors.into_iter().nth(idx).unwrap();
            eprintln!(
                "[voice][screen] xcap capturing monitor {} ({}×{})",
                idx,
                monitor.width(),
                monitor.height()
            );

            let mut frame_count: i64 = 0;
            let mut i420_buffers = XcapI420Buffers::new(video_width, video_height);
            let mut fps_window_frames: u32 = 0;
            let mut fps_window_start = std::time::Instant::now();
            while !stop_for_thread.load(Ordering::Relaxed) {
                let t0 = std::time::Instant::now();
                match monitor.capture_image() {
                    Ok(img) => {
                        let src_w = img.width();
                        let src_h = img.height();
                        let raw = img.as_raw();
                        if let Some(vf) = i420_buffers.rgba_to_i420_scaled_reuse(
                            raw,
                            src_w,
                            src_h,
                            video_width,
                            video_height,
                            frame_count,
                        ) {
                            source.capture_frame(&vf);
                            frame_count += 1;
                            fps_window_frames += 1;
                        }
                    }
                    Err(e) => eprintln!("[voice][screen] capture error: {}", e),
                }
                let elapsed = fps_window_start.elapsed();
                if elapsed >= Duration::from_secs(1) {
                    let actual_fps = fps_window_frames as f32 / elapsed.as_secs_f32().max(0.001);
                    eprintln!(
                        "[voice][screen] xcap capture_frame rate: {:.1} fps (target {})",
                        actual_fps, max_fps
                    );
                    fps_window_frames = 0;
                    fps_window_start = std::time::Instant::now();
                }
                let elapsed_ms = t0.elapsed().as_millis() as u64;
                let to_sleep = frame_interval_ms.saturating_sub(elapsed_ms);
                if to_sleep > 0 {
                    std::thread::sleep(Duration::from_millis(to_sleep));
                }
            }
            eprintln!("[voice][screen] xcap capture thread stopped");
        })
        .ok();

    (Some(track), None)
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn start_screen_capture(
    source_target: StreamSourceTarget,
    preset: ScreenPreset,
    stop_flag: Arc<AtomicBool>,
    keyframe_requested: Arc<AtomicBool>,
    session_stats: Arc<Mutex<VoiceSessionStats>>,
) -> (
    Option<LocalVideoTrack>,
    Option<tokio::sync::oneshot::Receiver<LocalVideoTrack>>,
) {
    let capture_backend =
        std::env::var("ASTRIX_SCREEN_CAPTURE_BACKEND").unwrap_or_else(|_| "wgc".to_string());
    if capture_backend.eq_ignore_ascii_case("xcap") {
        eprintln!("[voice][screen] capture backend override: xcap");
        return start_screen_capture_xcap(
            source_target,
            preset,
            stop_flag,
            keyframe_requested,
            session_stats,
        );
    }
    let use_dxgi = capture_backend.eq_ignore_ascii_case("dxgi")
        && matches!(&source_target, StreamSourceTarget::Monitor { .. });
    if capture_backend.eq_ignore_ascii_case("dxgi")
        && !matches!(&source_target, StreamSourceTarget::Monitor { .. })
    {
        eprintln!("[voice][screen] DXGI is unavailable for window capture, falling back to WGC");
    }
    if use_dxgi {
        eprintln!("[voice][screen] capture backend override: dxgi");
        eprintln!("[voice][screen] DXGI build marker: hardfail-fallback-v2");
    }
    let screen_index = match &source_target {
        StreamSourceTarget::Monitor { index } => Some(*index),
        StreamSourceTarget::Window { .. } => None,
    };
    use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering};
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows_capture::{
        capture::{Context, GraphicsCaptureApiHandler},
        frame::Frame,
        graphics_capture_api::InternalCaptureControl,
        monitor::Monitor as WgcMonitor,
        settings::{
            ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
            MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
        },
        window::Window as WgcWindow,
    };

    let (video_width, video_height, max_fps, bitrate_bps) = preset.params();

    /// OBS/Parsec style: encoder runs on fixed timer, reads single "latest" frame.
    /// Double buffer: WGC overwrites one slot, encoder reads the other.
    const LATEST_SLOT_COUNT: usize = 2;
    const LATEST_SLOT_NONE: u8 = 2; // Sentinel: no frame yet

    /// Phase 2: pool of our D3D11 textures (copy targets). Created on first frame from frame's device/desc.
    /// Two textures for double-buffering: WGC writes to one, encoder reads from the other.
    struct GpuTexturePool {
        #[allow(dead_code)]
        device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
        context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
        textures: [windows::Win32::Graphics::Direct3D11::ID3D11Texture2D; LATEST_SLOT_COUNT],
        width: u32,
        height: u32,
        /// Mutex protecting Immediate Context from concurrent use by WGC and encoder threads.
        /// D3D11 Immediate Context is NOT thread-safe; both threads must hold this lock
        /// before calling any context methods (CopyResource, Dispatch, etc.).
        /// The encoder releases the lock BEFORE GetData polling to avoid blocking WGC.
        context_mutex: Arc<parking_lot::Mutex<()>>,
    }

    /// Single latest slot: WGC overwrites, encoder reads on timer. OBS/Parsec style.
    /// Value 0 or 1 = valid slot index; LATEST_SLOT_NONE = no frame yet.
    struct LatestSlot {
        slot: AtomicU8,
    }
    impl LatestSlot {
        fn new() -> Self {
            Self {
                slot: AtomicU8::new(LATEST_SLOT_NONE),
            }
        }
        /// WGC: after copy to texture[write_slot], call this. Returns the slot we wrote to.
        fn store(&self, write_slot: u8) {
            self.slot.store(write_slot, Ordering::Release);
        }
        /// Encoder: read which slot has the latest frame. None if no frame yet.
        fn load(&self) -> Option<u8> {
            let s = self.slot.load(Ordering::Acquire);
            if s >= LATEST_SLOT_NONE {
                None
            } else {
                Some(s)
            }
        }
    }

    const RING_SIZE: usize = 6;

    // Warn early for demanding presets — software path may not sustain target fps.
    if matches!(
        preset,
        ScreenPreset::P1440F60
            | ScreenPreset::P720F120
            | ScreenPreset::P1080F120
            | ScreenPreset::P1440F90
    ) {
        eprintln!(
            "[voice][screen] WARNING: {:?} is demanding (high res/fps). If FPS drops, consider \
             lower preset (e.g. P1080F60 or P1440F30). Hardware encoder (NVENC/AMF) support is planned.",
            preset
        );
    }

    let resolution = VideoResolution {
        width: video_width,
        height: video_height,
    };

    // Encode path — mft (force MFT), cpu (OpenH264 I420), auto (prefer NVENC D3D11 on NVIDIA,
    // then fall back to MFT, then to OpenH264 if encoded backends hard-fail).
    // Phase 1.3-1.4 implemented: NativeEncodedVideoSource::push_frame now delivers H.264 to viewers
    // via ExternalH264Encoder → EncodedImageCallback → WebRTC RTP layer.
    // auto defaults to NVENC D3D11 → MFT hardware/software → CPU OpenH264 fallback chain.
    #[derive(Clone, Copy, PartialEq)]
    enum EncodePath {
        Mft,
        Cpu,
        Auto,
    }
    let encode_path: EncodePath = match std::env::var("ASTRIX_SCREEN_CAPTURE_PATH").as_deref() {
        Ok("mft") => EncodePath::Mft,
        Ok("cpu") => EncodePath::Cpu,
        Ok("auto") | _ => EncodePath::Auto,
    };

    // Encoded path uses NativeEncodedVideoSource; CPU path uses NativeVideoSource.
    // The encoded path always has a CPU fallback channel: if BGRA->NV12 or the encoded
    // H.264 backend hard-fails, the encoder thread creates a Native (I420) track and
    // sends it to the async context for republish.
    let source: Option<NativeVideoSource>;
    let encoded_source: Option<NativeEncodedVideoSource>;
    let track: LocalVideoTrack;
    let fallback_tx_opt: Option<tokio::sync::oneshot::Sender<LocalVideoTrack>>;
    let fallback_rx_opt: Option<tokio::sync::oneshot::Receiver<LocalVideoTrack>>;

    if encode_path == EncodePath::Mft || encode_path == EncodePath::Auto {
        let encoded = NativeEncodedVideoSource::new(resolution.clone(), true);
        track =
            LocalVideoTrack::create_video_track("screen", RtcVideoSource::Encoded(encoded.clone()));
        source = None;
        encoded_source = Some(encoded);
        let (tx, rx) = tokio::sync::oneshot::channel::<LocalVideoTrack>();
        fallback_tx_opt = Some(tx);
        fallback_rx_opt = Some(rx);
    } else {
        let native = NativeVideoSource::new(resolution.clone(), true);
        track =
            LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(native.clone()));
        source = Some(native);
        encoded_source = None;
        fallback_tx_opt = None;
        fallback_rx_opt = None;
    }

    /// SPSC ring buffer: RING_SIZE slots, WGC pushes (drops oldest when full), encoder pops.
    struct RawFrameRing {
        slots: [AtomicPtr<RawFrame>; RING_SIZE],
        write_idx: AtomicUsize,
        read_idx: AtomicUsize,
    }
    impl RawFrameRing {
        fn new() -> Self {
            Self {
                slots: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
                write_idx: AtomicUsize::new(0),
                read_idx: AtomicUsize::new(0),
            }
        }
        fn push(&self, ptr: *mut RawFrame) {
            let w = self.write_idx.load(Ordering::Acquire);
            let r = self.read_idx.load(Ordering::Acquire);
            if w.wrapping_sub(r) >= RING_SIZE {
                let old = self.slots[r % RING_SIZE].swap(std::ptr::null_mut(), Ordering::AcqRel);
                if !old.is_null() {
                    unsafe {
                        drop(Box::from_raw(old));
                    }
                }
                self.read_idx.store(r.wrapping_add(1), Ordering::Release);
            }
            let slot = w % RING_SIZE;
            let old = self.slots[slot].swap(ptr, Ordering::AcqRel);
            // Drop any previously unread frame (can happen when encoder lags at 60 fps).
            if !old.is_null() {
                unsafe {
                    drop(Box::from_raw(old));
                }
            }
            self.write_idx.store(w.wrapping_add(1), Ordering::Release);
        }
        fn pop(&self) -> Option<*mut RawFrame> {
            let r = self.read_idx.load(Ordering::Acquire);
            let w = self.write_idx.load(Ordering::Acquire);
            if r >= w {
                return None;
            }
            let ptr = self.slots[r % RING_SIZE].swap(std::ptr::null_mut(), Ordering::AcqRel);
            self.read_idx.store(r.wrapping_add(1), Ordering::Release);
            if ptr.is_null() {
                None
            } else {
                Some(ptr)
            }
        }
        fn drain_drop(&self) {
            while let Some(ptr) = self.pop() {
                if !ptr.is_null() {
                    unsafe {
                        drop(Box::from_raw(ptr));
                    }
                }
            }
        }
    }
    let ring: Arc<RawFrameRing> = Arc::new(RawFrameRing::new());
    let latest_slot: Arc<LatestSlot> = Arc::new(LatestSlot::new());
    let pool_ref: Arc<Mutex<Option<GpuTexturePool>>> = Arc::new(Mutex::new(None));
    /// When true, WGC callback skips CPU ring (expensive GPU→CPU RGBA copy).
    /// Set by encoder thread on first successful GPU convert; cleared on GPU failure.
    let gpu_encode_active: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // ── WGC callback struct ───────────────────────────────────────────────────
    /// Diagnostic:
    /// - frame_count_at_last_log
    /// - start of current 1s wall-clock window
    /// - WGC frame timestamp (100 ns) observed at the previous log point
    ///
    /// We intentionally track both callback cadence and compositor cadence. On heavy scenes,
    /// FrameArrived callbacks can bunch up or stall independently of the captured content's
    /// timestamp progression, and using callback cadence alone was causing false source-FPS
    /// drops and over-aggressive sender downshifts.
    let pool_creation_started: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let pool_creation_failed: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    struct WgcFlags {
        ring: Arc<RawFrameRing>,
        stop_flag: Arc<AtomicBool>,
        latest_slot: Arc<LatestSlot>,
        pool_ref: Arc<Mutex<Option<GpuTexturePool>>>,
        gpu_encode_active: Arc<AtomicBool>,
        pool_creation_started: Arc<AtomicBool>,
        pool_creation_failed: Arc<AtomicBool>,
        wgc_frame_count: Arc<AtomicU64>,
        wgc_source_fps_milli: Arc<AtomicU64>,
        wgc_log_state: Arc<Mutex<(u64, Option<std::time::Instant>, i64)>>,
        telemetry: Arc<PipelineTelemetry>,
    }
    struct ScreenHandler {
        ring: Arc<RawFrameRing>,
        stop_flag: Arc<AtomicBool>,
        latest_slot: Arc<LatestSlot>,
        pool_ref: Arc<Mutex<Option<GpuTexturePool>>>,
        gpu_encode_active: Arc<AtomicBool>,
        pool_creation_started: Arc<AtomicBool>,
        pool_creation_failed: Arc<AtomicBool>,
        wgc_frame_count: Arc<AtomicU64>,
        wgc_source_fps_milli: Arc<AtomicU64>,
        wgc_log_state: Arc<Mutex<(u64, Option<std::time::Instant>, i64)>>,
        telemetry: Arc<PipelineTelemetry>,
    }

    impl GraphicsCaptureApiHandler for ScreenHandler {
        type Flags = WgcFlags;
        type Error = Box<dyn std::error::Error + Send + Sync>;

        fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
            Ok(Self {
                ring: ctx.flags.ring,
                stop_flag: ctx.flags.stop_flag,
                latest_slot: ctx.flags.latest_slot,
                pool_ref: ctx.flags.pool_ref,
                gpu_encode_active: ctx.flags.gpu_encode_active,
                pool_creation_started: ctx.flags.pool_creation_started,
                pool_creation_failed: ctx.flags.pool_creation_failed,
                wgc_frame_count: ctx.flags.wgc_frame_count,
                wgc_source_fps_milli: ctx.flags.wgc_source_fps_milli,
                wgc_log_state: ctx.flags.wgc_log_state,
                telemetry: ctx.flags.telemetry,
            })
        }

        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            capture_control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            if self.stop_flag.load(Ordering::Relaxed) {
                capture_control.stop();
                return Ok(());
            }
            // Diagnostic: log both callback cadence and compositor cadence every second.
            // Use try_lock so we never block the WGC thread.
            let total = self.wgc_frame_count.fetch_add(1, Ordering::Relaxed) + 1;
            let now = std::time::Instant::now();
            let frame_timestamp_100ns = frame.timestamp().Duration;
            if let Some(mut guard) = self.wgc_log_state.try_lock() {
                let (last_count, last_t, last_frame_timestamp_100ns) = *guard;
                match last_t {
                    None => *guard = (total, Some(now), frame_timestamp_100ns),
                    Some(t0) if now.duration_since(t0) >= std::time::Duration::from_secs(1) => {
                        let elapsed_sec = now.duration_since(t0).as_secs_f32().max(0.001);
                        let callback_fps = (total - last_count) as f32 / elapsed_sec;
                        let frames = total.saturating_sub(last_count);
                        let compositor_fps = if last_frame_timestamp_100ns > 0
                            && frame_timestamp_100ns > last_frame_timestamp_100ns
                            && frames > 1
                        {
                            let compositor_elapsed_sec =
                                (frame_timestamp_100ns - last_frame_timestamp_100ns) as f32
                                    / 10_000_000.0;
                            (frames - 1) as f32 / compositor_elapsed_sec.max(0.001)
                        } else {
                            callback_fps
                        };
                        self.wgc_source_fps_milli
                            .store((compositor_fps * 1000.0) as u64, Ordering::Relaxed);
                        if is_telemetry_enabled() {
                            eprintln!(
                                "[voice][screen] WGC rate: callback {:.1} fps, compositor {:.1} fps",
                                callback_fps, compositor_fps
                            );
                        }
                        *guard = (total, Some(now), frame_timestamp_100ns);
                    }
                    _ => {}
                }
            }
            let w = frame.width();
            let h = frame.height();

            // Phase 1+2: get D3D11 texture, device, context, desc; create pool on first frame; copy to our texture.
            let texture: &ID3D11Texture2D = unsafe { frame.as_raw_texture() };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };

            if let Ok(device) = unsafe { texture.GetDevice() } {
                if let Ok(context) = unsafe { device.GetImmediateContext() } {
                    // Ensure pool exists (create on first frame). CRITICAL: CreateTexture2D x6 can
                    // take 50–100ms; blocking here triggers WGC half-rate (30 FPS) with no recovery.
                    // Defer pool creation to a background thread and return immediately.
                    {
                        let need_create = self.pool_ref.try_lock().map_or(false, |g| g.is_none())
                            && !self.pool_creation_started.swap(true, Ordering::Relaxed);
                        if need_create {
                            let device_clone = device.clone();
                            let context_clone = context.clone();
                            let pool_ref = Arc::clone(&self.pool_ref);
                            let pool_fail = Arc::clone(&self.pool_creation_failed);
                            let mut our_desc = desc;
                            our_desc.Usage = D3D11_USAGE_DEFAULT;
                            our_desc.BindFlags =
                                (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
                            our_desc.CPUAccessFlags = 0;
                            our_desc.MipLevels = 1;
                            our_desc.ArraySize = 1;
                            our_desc.MiscFlags = 0;
                            // Strip SRGB: VideoProcessor on NVIDIA drivers rejects UNORM_SRGB as input.
                            // BGRA_UNORM and BGRA_UNORM_SRGB are format-compatible (same bits),
                            // so CopyResource between them is valid per D3D11 spec.
                            {
                                use windows::Win32::Graphics::Dxgi::Common::{
                                    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
                                };
                                if our_desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB.into() {
                                    eprintln!("[voice][screen] Pool: WGC texture is SRGB, forcing UNORM for VideoProcessor compatibility");
                                    our_desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM.into();
                                }
                            }
                            std::thread::spawn(move || {
                                let mut textures: [Option<ID3D11Texture2D>; LATEST_SLOT_COUNT] =
                                    std::array::from_fn(|_| None);
                                for (i, slot) in textures.iter_mut().enumerate() {
                                    if let Err(e) = unsafe {
                                        device_clone.CreateTexture2D(
                                            &our_desc,
                                            None,
                                            Some(std::ptr::from_mut(slot)),
                                        )
                                    } {
                                        eprintln!(
                                            "[voice][screen] CreateTexture2D[{}] failed: {:?}",
                                            i, e
                                        );
                                        pool_fail.store(true, Ordering::Relaxed);
                                        return;
                                    }
                                }
                                let textures = textures.map(|t| t.expect("CreateTexture2D null"));
                                eprintln!(
                                    "[voice][screen] D3D11 texture pool created: {}x{} format {:?} ({} slots, OBS-style latest)",
                                    our_desc.Width, our_desc.Height, our_desc.Format, LATEST_SLOT_COUNT
                                );
                                *pool_ref.lock() = Some(GpuTexturePool {
                                    device: device_clone,
                                    context: context_clone,
                                    textures,
                                    width: our_desc.Width,
                                    height: our_desc.Height,
                                    context_mutex: Arc::new(parking_lot::Mutex::new(())),
                                });
                            });
                            return Ok(()); // Return immediately; pool will be ready in ~50–100ms
                        }
                    }

                    // Copy frame into our pool texture. OBS-style: overwrite "other" slot, store as latest.
                    // Double buffer: WGC writes to slot encoder isn't reading.
                    // Do not Flush() here: forcing the immediate context to submit inside
                    // FrameArrived can stall the callback long enough for WGC to drop into
                    // half-rate/low-rate delivery on heavy scenes. The encoder uses the same
                    // immediate context behind context_mutex, so command order is preserved
                    // without a synchronous flush from the callback.
                    //
                    // CRITICAL: never block in FrameArrived. If we block >~16 ms, WGC throttles to 30 FPS.
                    // Use try_lock for both pool_ref and context_mutex; skip frame if either is busy.
                    let (our_tex_clone, context_mutex_clone, write_slot) = {
                        let Some(guard) = self.pool_ref.try_lock() else {
                            return Ok(()); // encoder holds pool; skip frame to avoid blocking (30 FPS throttle)
                        };
                        match guard.as_ref() {
                            None => (None, None, 0u8),
                            Some(pool) => {
                                let current = self.latest_slot.slot.load(Ordering::Acquire);
                                let slot = if current >= LATEST_SLOT_NONE {
                                    0u8
                                } else {
                                    (1 - current) as u8
                                };
                                (
                                    Some(pool.textures[slot as usize].clone()),
                                    Some(Arc::clone(&pool.context_mutex)),
                                    slot,
                                )
                            }
                        }
                    };
                    // pool_ref lock released — encoder can now access the pool concurrently.
                    // CRITICAL: never block in FrameArrived. If we block >~16 ms, WGC throttles to 30 FPS.
                    // Use try_lock only; skip this frame if encoder still holds the context.
                    if let (Some(our_tex), Some(ctx_mutex)) = (our_tex_clone, context_mutex_clone) {
                        if let Some(_ctx_guard) = ctx_mutex.try_lock() {
                            let capture_start = std::time::Instant::now();
                            unsafe {
                                context.CopyResource(&our_tex, texture);
                            }
                            self.telemetry
                                .set_capture(capture_start.elapsed().as_micros() as u64);
                            self.latest_slot.store(write_slot);
                        }
                        // else: encoder still converting; drop frame so WGC thread never blocks
                    }
                    if self.gpu_encode_active.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                }
            }

            // Pool being created in background — skip CPU fallback to avoid blocking.
            // frame.buffer() does GPU→CPU readback that takes >16 ms at high resolutions;
            // blocking FrameArrived that long triggers WGC half-rate throttle (30 FPS).
            if self.pool_creation_started.load(Ordering::Relaxed)
                && !self.pool_creation_failed.load(Ordering::Relaxed)
            {
                return Ok(());
            }

            // CPU fallback path: push RGBA for encoder (only when GPU encode is not active).
            let mut buf = frame.buffer()?;
            let raw = buf.as_nopadding_buffer()?;
            let new_frame = Box::into_raw(Box::new(RawFrame {
                pixels: raw.to_vec(),
                width: w,
                height: h,
            }));
            self.ring.push(new_frame);
            Ok(())
        }
    }

    let wgc_frame_count = Arc::new(AtomicU64::new(0));
    let wgc_source_fps_milli = Arc::new(AtomicU64::new(0));
    let wgc_log_state = Arc::new(Mutex::new((0u64, None::<std::time::Instant>, 0i64)));
    let telemetry_sender = Arc::new(PipelineTelemetry::new());

    if use_dxgi {
        let stop_dxgi = Arc::clone(&stop_flag);
        let latest_slot_dxgi = Arc::clone(&latest_slot);
        let pool_ref_dxgi = Arc::clone(&pool_ref);
        let wgc_frame_count_dxgi = Arc::clone(&wgc_frame_count);
        let wgc_source_fps_milli_dxgi = Arc::clone(&wgc_source_fps_milli);
        let wgc_log_state_dxgi = Arc::clone(&wgc_log_state);
        let telemetry_dxgi = Arc::clone(&telemetry_sender);
        std::thread::Builder::new()
            .name("livekit-screen-dxgi".into())
            .spawn(move || {
                let mut capture = match DxgiDuplicationCapture::new(screen_index) {
                    Ok(capture) => capture,
                    Err(e) => {
                        eprintln!("[voice][screen] DXGI init failed: {e}");
                        return;
                    }
                };

                loop {
                    if stop_dxgi.load(Ordering::Relaxed) {
                        break;
                    }

                    let frame = match capture.acquire_next_frame(33) {
                        Ok(Some(frame)) => frame,
                        Ok(None) => continue,
                        Err(DxgiDuplicationError::AccessLost) => {
                            eprintln!("[voice][screen] DXGI access lost, reinitializing duplication");
                            capture = match DxgiDuplicationCapture::new(screen_index) {
                                Ok(capture) => capture,
                                Err(e) => {
                                    eprintln!("[voice][screen] DXGI reinit failed: {e}");
                                    break;
                                }
                            };
                            continue;
                        }
                        Err(e) => {
                            eprintln!("[voice][screen] DXGI error: {e}");
                            break;
                        }
                    };

                    let total = wgc_frame_count_dxgi.fetch_add(1, Ordering::Relaxed) + 1;
                    let now = std::time::Instant::now();
                    let frame_timestamp_100ns = frame.info.LastPresentTime;
                    if let Some(mut guard) = wgc_log_state_dxgi.try_lock() {
                        let (last_count, last_t, last_frame_timestamp_100ns) = *guard;
                        match last_t {
                            None => *guard = (total, Some(now), frame_timestamp_100ns),
                            Some(t0) if now.duration_since(t0) >= std::time::Duration::from_secs(1) => {
                                let elapsed_sec = now.duration_since(t0).as_secs_f32().max(0.001);
                                let callback_fps = (total - last_count) as f32 / elapsed_sec;
                                let frames = total.saturating_sub(last_count);
                                let compositor_fps = if last_frame_timestamp_100ns > 0
                                    && frame_timestamp_100ns > last_frame_timestamp_100ns
                                    && frames > 1
                                {
                                    let compositor_elapsed_sec = (frame_timestamp_100ns - last_frame_timestamp_100ns)
                                        as f32
                                        / 10_000_000.0;
                                    (frames - 1) as f32 / compositor_elapsed_sec.max(0.001)
                                } else {
                                    callback_fps
                                };
                                wgc_source_fps_milli_dxgi
                                    .store((compositor_fps * 1000.0) as u64, Ordering::Relaxed);
                                if is_telemetry_enabled() {
                                    eprintln!(
                                        "[voice][screen] DXGI rate: callback {:.1} fps, compositor {:.1} fps",
                                        callback_fps, compositor_fps
                                    );
                                }
                                *guard = (total, Some(now), frame_timestamp_100ns);
                            }
                            _ => {}
                        }
                    }

                    let texture = match frame.texture() {
                        Ok(texture) => texture,
                        Err(e) => {
                            eprintln!("[voice][screen] DXGI frame cast failed: {:?}", e);
                            continue;
                        }
                    };
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    unsafe { texture.GetDesc(&mut desc) };

                    {
                        let mut pool_guard = pool_ref_dxgi.lock();
                        let needs_recreate = pool_guard
                            .as_ref()
                            .map_or(true, |pool| pool.width != desc.Width || pool.height != desc.Height);
                        if needs_recreate {
                            let mut our_desc = desc;
                            our_desc.Usage = D3D11_USAGE_DEFAULT;
                            our_desc.BindFlags =
                                (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
                            our_desc.CPUAccessFlags = 0;
                            our_desc.MipLevels = 1;
                            our_desc.ArraySize = 1;
                            our_desc.MiscFlags = 0;
                            {
                                use windows::Win32::Graphics::Dxgi::Common::{
                                    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
                                };
                                if our_desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB.into() {
                                    eprintln!("[voice][screen] DXGI texture is SRGB, forcing UNORM for VideoProcessor compatibility");
                                    our_desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM.into();
                                }
                            }

                            let mut textures: [Option<ID3D11Texture2D>; LATEST_SLOT_COUNT] =
                                std::array::from_fn(|_| None);
                            let mut pool_ok = true;
                            for (i, slot) in textures.iter_mut().enumerate() {
                                if let Err(e) = unsafe {
                                    capture.device.CreateTexture2D(
                                        &our_desc,
                                        None,
                                        Some(std::ptr::from_mut(slot)),
                                    )
                                } {
                                    eprintln!("[voice][screen] DXGI CreateTexture2D[{}] failed: {:?}", i, e);
                                    pool_ok = false;
                                    break;
                                }
                            }
                            if !pool_ok {
                                continue;
                            }
                            let textures = textures.map(|t| t.expect("DXGI CreateTexture2D null"));
                            eprintln!(
                                "[voice][screen] DXGI texture pool created: {}x{} format {:?} ({} slots, latest-frame)",
                                our_desc.Width, our_desc.Height, our_desc.Format, LATEST_SLOT_COUNT
                            );
                            *pool_guard = Some(GpuTexturePool {
                                device: capture.device.clone(),
                                context: capture.context.clone(),
                                textures,
                                width: our_desc.Width,
                                height: our_desc.Height,
                                context_mutex: Arc::new(parking_lot::Mutex::new(())),
                            });
                        }
                    }

                    let (our_tex_clone, context_mutex_clone, write_slot) = {
                        let Some(guard) = pool_ref_dxgi.try_lock() else {
                            continue;
                        };
                        match guard.as_ref() {
                            None => (None, None, 0u8),
                            Some(pool) => {
                                let current = latest_slot_dxgi.slot.load(Ordering::Acquire);
                                let slot = if current >= LATEST_SLOT_NONE {
                                    0u8
                                } else {
                                    (1 - current) as u8
                                };
                                (
                                    Some(pool.textures[slot as usize].clone()),
                                    Some(Arc::clone(&pool.context_mutex)),
                                    slot,
                                )
                            }
                        }
                    };
                    if let (Some(our_tex), Some(ctx_mutex)) = (our_tex_clone, context_mutex_clone) {
                        if let Some(_ctx_guard) = ctx_mutex.try_lock() {
                            let capture_start = std::time::Instant::now();
                            unsafe {
                                capture.context.CopyResource(&our_tex, &texture);
                            }
                            telemetry_dxgi.set_capture(capture_start.elapsed().as_micros() as u64);
                            latest_slot_dxgi.store(write_slot);
                            if total == 1 {
                                eprintln!("[voice][screen] DXGI first frame copied into latest-slot");
                            }
                        }
                    }
                }

                eprintln!("[voice][screen] DXGI capture thread stopped");
            })
            .ok();
    } else if let StreamSourceTarget::Window {
        window_id,
        process_id,
    } = source_target
    {
        if find_stream_window(window_id, process_id).is_none() {
            eprintln!(
                "[voice][screen] WGC window not found: hwnd={} pid={}",
                window_id, process_id
            );
            return (None, None);
        }
        eprintln!(
            "[voice][screen] WGC capturing window hwnd={} at {}x{} @ {}fps",
            window_id, video_width, video_height, max_fps
        );

        let flags = WgcFlags {
            ring: Arc::clone(&ring),
            stop_flag: Arc::clone(&stop_flag),
            latest_slot: Arc::clone(&latest_slot),
            pool_ref: Arc::clone(&pool_ref),
            gpu_encode_active: Arc::clone(&gpu_encode_active),
            pool_creation_started: Arc::clone(&pool_creation_started),
            pool_creation_failed: Arc::clone(&pool_creation_failed),
            wgc_frame_count: Arc::clone(&wgc_frame_count),
            wgc_source_fps_milli: Arc::clone(&wgc_source_fps_milli),
            wgc_log_state: Arc::clone(&wgc_log_state),
            telemetry: Arc::clone(&telemetry_sender),
        };
        let min_update_interval = if max_fps >= 55.0 {
            eprintln!(
                "[voice][screen] WGC MinUpdateInterval: 1000 Вµs (target {} fps)",
                max_fps
            );
            MinimumUpdateIntervalSettings::Custom(Duration::from_micros(1000))
        } else {
            MinimumUpdateIntervalSettings::Default
        };
        let settings = Settings::new(
            WgcWindow::from_raw_hwnd(window_id as usize as *mut std::ffi::c_void),
            CursorCaptureSettings::Default,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            min_update_interval,
            DirtyRegionSettings::Default,
            ColorFormat::Rgba8,
            flags,
        );

        std::thread::Builder::new()
            .name("livekit-window-wgc".into())
            .spawn(move || {
                if let Err(e) = ScreenHandler::start(settings) {
                    eprintln!("[voice][screen] WGC window error: {e}");
                }
                eprintln!("[voice][screen] WGC capture thread stopped");
            })
            .ok();
    } else {
        let monitors = WgcMonitor::enumerate().unwrap_or_default();
        if monitors.is_empty() {
            eprintln!("[voice][screen] no monitors found");
            return (None, None);
        }
        let idx = screen_index.unwrap_or(0).min(monitors.len() - 1);
        let Some(monitor) = monitors.into_iter().nth(idx) else {
            return (None, None);
        };
        eprintln!(
            "[voice][screen] WGC capturing monitor {} at {}×{} @ {}fps",
            idx, video_width, video_height, max_fps
        );

        let flags = WgcFlags {
            ring: Arc::clone(&ring),
            stop_flag: Arc::clone(&stop_flag),
            latest_slot: Arc::clone(&latest_slot),
            pool_ref: Arc::clone(&pool_ref),
            gpu_encode_active: Arc::clone(&gpu_encode_active),
            pool_creation_started: Arc::clone(&pool_creation_started),
            pool_creation_failed: Arc::clone(&pool_creation_failed),
            wgc_frame_count: Arc::clone(&wgc_frame_count),
            wgc_source_fps_milli: Arc::clone(&wgc_source_fps_milli),
            wgc_log_state: Arc::clone(&wgc_log_state),
            telemetry: Arc::clone(&telemetry_sender),
        };
        // Request capture rate from WGC: Default can throttle to ~30 fps on some systems.
        // MinUpdateInterval = minimum time between frames; smaller = higher max capture rate.
        // Use 1 ms (doc: values >= 1 ms work; < 1 ms can cap at ~50 fps) so WGC can deliver 60+ fps;
        // our encoder thread throttles to max_fps via next_frame_at.
        // Try micros explicitly in case crate rounds millis to 33 ms on some code paths.
        let min_update_interval = if max_fps >= 55.0 {
            eprintln!(
                "[voice][screen] WGC MinUpdateInterval: 1000 µs (target {} fps)",
                max_fps
            );
            MinimumUpdateIntervalSettings::Custom(Duration::from_micros(1000))
        } else {
            MinimumUpdateIntervalSettings::Default
        };
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::Default,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            min_update_interval,
            DirtyRegionSettings::Default,
            ColorFormat::Rgba8,
            flags,
        );

        // WGC capture thread: GPU→CPU copy only, then atomic swap into slot.
        std::thread::Builder::new()
            .name("livekit-screen-wgc".into())
            .spawn(move || {
                if let Err(e) = ScreenHandler::start(settings) {
                    eprintln!("[voice][screen] WGC error: {e}");
                }
                eprintln!("[voice][screen] WGC capture thread stopped");
            })
            .ok();
    }

    // ── Encoder thread ────────────────────────────────────────────────────────
    /// Sleep until deadline - 1 ms, then spin for the last 1 ms.
    /// timeBeginPeriod(1) is called in the encoder thread, so 1 ms timer accuracy is guaranteed.
    const PRE_BUFFER_MS: u64 = 1;
    let source_enc = source.clone();
    let mut encoded_source_enc = encoded_source.clone();
    let encode_path_enc = encode_path;
    // Encoded-path fallback: if the encoded backend hard-fails, the encoder thread
    // creates a Native track and sends it here
    // so the async context can unpublish the Encoded track and republish the Native one.
    // Capture tokio Handle so the encoder thread can create livekit objects (NativeVideoSource,
    // LocalVideoTrack) which internally require a Tokio reactor.
    let tokio_handle = tokio::runtime::Handle::current();
    let mut mft_fallback_tx = fallback_tx_opt;
    let stop_enc = Arc::clone(&stop_flag);
    let ring_enc = Arc::clone(&ring);
    let latest_slot_enc = Arc::clone(&latest_slot);
    let pool_ref_enc = Arc::clone(&pool_ref);
    let wgc_frame_count_enc = Arc::clone(&wgc_frame_count);
    let wgc_source_fps_milli_enc = Arc::clone(&wgc_source_fps_milli);
    let gpu_encode_active_enc = Arc::clone(&gpu_encode_active);
    let stats_enc = Arc::clone(&session_stats);
    let telemetry_enc = Arc::clone(&telemetry_sender);
    let keyframe_request_enc = Arc::clone(&keyframe_requested);
    // Phase 5.2: For MFT path we must use latest_slot (D3D11 textures). For CPU path, try GPU first.
    let capture_path_mft = encode_path == EncodePath::Mft || encode_path == EncodePath::Auto;
    if capture_path_mft {
        eprintln!("[voice][screen] encode path: encoded GPU path (push_frame -> ExternalH264Encoder -> RTP)");
    } else {
        eprintln!("[voice][screen] encode path: CPU/OpenH264");
    }

    std::thread::Builder::new()
        .name("livekit-screen-enc".into())
        .spawn(move || {
            // Phase 4.4: raise encoder thread priority — reduces jitter when CPU is loaded (e.g. gaming).
            #[cfg(all(target_os = "windows", feature = "wgc-capture"))]
            unsafe {
                use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            }

            // Raise Windows timer resolution to 1 ms.
            unsafe {
                #[link(name = "winmm")]
                extern "system" { fn timeBeginPeriod(uPeriod: u32) -> u32; }
                timeBeginPeriod(1);
            }

            // Non-blocking capture_frame: send frames to a dedicated thread so heavy keyframes
            // don't block the encoder loop (convert → send → next frame).
            // Phase 5.1: Only needed for I420 path (NativeVideoSource); MFT path uses push_frame directly.
            const CAPTURE_QUEUE_LEN: usize = 2;
            let (capture_tx, capture_rx) = mpsc::sync_channel(CAPTURE_QUEUE_LEN);
            let mut capture_tx_opt = source_enc.as_ref().map(|_| capture_tx);
            let capture_handle = if let Some(ref src) = source_enc {
                let source_for_capture = src.clone();
                Some(std::thread::Builder::new()
                    .name("livekit-capture-frame".into())
                    .spawn(move || {
                        while let Ok(vf) = capture_rx.recv() {
                            source_for_capture.capture_frame(&vf);
                        }
                    })
                    .expect("spawn capture_frame thread"))
            } else {
                drop(capture_rx); // MFT path: no capture_frame thread
                None
            };

            // ── Warmup phase ───────────────────────────────────────────────────
            // CPU path keeps the old fixed warmup.
            // Encoded path now uses a transport-aware startup cap below, so a fixed warmup
            // window only adds timestamp/schedule mismatch without helping startup quality.
            const WARMUP_FRAMES_CPU: i64 = 30;
            const WARMUP_FRAMES_MFT: i64 = 0;
            const WARMUP_FPS_CPU: f64 = 15.0;
            const WARMUP_FPS_MFT: f64 = 30.0;
            let (warmup_frames, warmup_fps) = if capture_path_mft {
                (WARMUP_FRAMES_MFT, WARMUP_FPS_MFT)
            } else {
                (WARMUP_FRAMES_CPU, WARMUP_FPS_CPU)
            };
            /// Don't drop frames for this many published frames so the stream starts (no 5–6 s freeze on first frame).
            const STARTUP_ACCEPT_FRAMES: i64 = 45;
            /// Minimum FPS on static screen — never throttle below this so stream stays responsive.
            const MIN_STATIC_FPS: f64 = 10.0;
            const MAX_STATIC_INTERVAL_NS: u64 = 1_000_000_000 / MIN_STATIC_FPS as u64;
            let warmup_interval = Duration::from_nanos((1_000_000_000.0 / warmup_fps) as u64);

            let mut frame_count: i64 = 0;
            let mut next_frame_at = std::time::Instant::now();
            let mut fps_window_start = std::time::Instant::now();
            let mut fps_frame_count: i64 = 0;
            // Phase 4.5: adaptive FPS — reduce when encode exceeds budget, restore when under.
            let max_fps_f64 = max_fps;
            let preset_bitrate_bps_u32 = bitrate_bps.min(u32::MAX as u64) as u32;
            let initial_sender_fps = max_fps_f64;
            let mut current_fps = initial_sender_fps;
            let mut current_source_cap_fps = max_fps_f64;
            let mut webrtc_target_bitrate_bps = preset_bitrate_bps_u32;
            let mut webrtc_target_fps = max_fps_f64;
            // Legacy env name, but the lock applies to the whole Windows GPU capture path
            // here (WGC and DXGI). Only an explicit `ASTRIX_DXGI_LOCK_PRESET_FPS=0`
            // enables adaptive sender FPS.
            let lock_env = std::env::var("ASTRIX_DXGI_LOCK_PRESET_FPS").ok();
            let lock_preset_fps = !matches!(lock_env.as_deref(), Some("0"));
            eprintln!(
                "[voice][screen] preset FPS lock: {} (ASTRIX_DXGI_LOCK_PRESET_FPS={}, backend={})",
                if lock_preset_fps {
                    "enabled"
                } else {
                    "disabled"
                },
                lock_env.unwrap_or_else(|| "<default:on>".to_string()),
                if use_dxgi { "dxgi" } else { "wgc" }
            );
            let prefer_mft_no_fallback = use_dxgi && capture_path_mft;
            let mut encode_avg_ms: f64 = 0.0;
            let mut downgrade_frames: i64 = 0;
            let mut upgrade_frames: i64 = 0;
            const ADAPTIVE_FPS_LADDER: [f64; 5] = [30.0, 45.0, 60.0, 90.0, 120.0];
            const SOURCE_CAP_HYSTERESIS_FPS: f64 = 10.0;
            const SOURCE_CAP_CONFIRM_WINDOWS: u32 = 3;
            let mut source_cap_down_votes: u32 = 0;
            let mut source_cap_up_votes: u32 = 0;
            let next_lower_fps = |fps: f64| -> f64 {
                ADAPTIVE_FPS_LADDER.iter().rev().find(|&&s| s < fps).copied().unwrap_or(*ADAPTIVE_FPS_LADDER.first().unwrap())
            };
            let next_higher_fps = |fps: f64| -> f64 {
                ADAPTIVE_FPS_LADDER.iter().find(|&&s| s > fps).copied().unwrap_or(fps)
            };
            let mut current_frame_interval_ns = (1_000_000_000.0 / current_fps) as u64;
            let mut current_full_interval = Duration::from_nanos(current_frame_interval_ns);
            let mut current_effective_interval = if current_full_interval.as_nanos() as u64 > MAX_STATIC_INTERVAL_NS {
                Duration::from_nanos(MAX_STATIC_INTERVAL_NS)
            } else {
                current_full_interval
            };
            let mut current_rtp_step = VIDEO_RTP_CLOCK_HZ as u32 / current_fps as u32;
            // RTP timestamps: fixed 90000/fps step — monotonic from stream start.
            let mut rtp_timestamp: u32 = rand::random();
            let enc_session_start = std::time::Instant::now();
            let mut last_send_capture_us: i64 = 0;
            let telemetry_enabled = is_telemetry_enabled();
            const STARTUP_BWE_GUARD_SECS: u64 = 4;
            let startup_transport_fps_cap =
                |target_bitrate_bps: u32, elapsed: Duration| -> f64 {
                    if !capture_path_mft
                        || elapsed >= Duration::from_secs(STARTUP_BWE_GUARD_SECS)
                        || preset_bitrate_bps_u32 == 0
                    {
                        return max_fps_f64;
                    }
                    let ratio = target_bitrate_bps.max(250_000) as f64
                        / preset_bitrate_bps_u32 as f64;
                    let capped = if ratio < 0.10 {
                        30.0
                    } else if ratio < 0.25 {
                        45.0
                    } else if ratio < 0.45 {
                        60.0
                    } else if ratio < 0.70 {
                        75.0
                    } else {
                        max_fps_f64
                    };
                    capped.min(max_fps_f64)
                };
            let mut last_startup_transport_cap_fps = current_fps;
            let push_trace_verbose = std::env::var("ASTRIX_DXGI_PUSH_TRACE")
                .map(|v| v != "0")
                .unwrap_or(false);
            let push_trace_frame_limit = std::env::var("ASTRIX_DXGI_PUSH_TRACE_FRAMES")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(120);
            let nv12_ready_trace = std::env::var("ASTRIX_DXGI_NV12_READY_TRACE")
                .map(|v| v != "0")
                .unwrap_or(false);
            let mut push_trace_tick_idx: u64 = 0;
            let mut push_trace_window_start = std::time::Instant::now();
            let mut push_trace_stats = DxgiPushTraceStats::default();
            let mft_pipelined_enabled = std::env::var("ASTRIX_MFT_PIPELINED")
                .map(|v| v != "0")
                .unwrap_or(true);
            // Monotonic MFT sample timestamp accumulator.
            // Using frame_count * interval regresses when FPS changes (e.g. 30→60 makes
            // ts_us jump backward), which can trigger MFT frame reordering. This accumulator
            // always advances forward by exactly frame_interval_us per submitted frame.
            let mut running_ts_us: i64 = 0;

            // ── Instrumentation accumulators (reset every 120 frames) ─────────
            const INSTR_WINDOW: i64 = 120;
            let mut instr_scale_ns:   u64 = 0;
            let mut instr_convert_ns: u64 = 0;
            let mut instr_total_ns:   u64 = 0;
            let frame_budget_ns = current_frame_interval_ns;

            let mut encoder = select_screen_encoder();
            eprintln!("[voice][screen] video codec encoder: {}", encoder.name());

            // Phase 4: GPU path — cache D3D11 RGBA→I420 converter per capture size.
            // Phase 5.1: once D3D11 init fails in auto mode, use CPU-only for the rest of the session.
            // Phase 6.1: use D3d11RgbaToI420Scaled when src != dst (GPU downscale in shader).
            // Phase 6.3: recreate converter when capture resolution changes.
            enum GpuConverter {
                NoScale(D3d11RgbaToI420),
                Scaled(D3d11RgbaToI420Scaled),
            }
            impl GpuConverter {
                fn src_dims(&self) -> (u32, u32) {
                    match self {
                        GpuConverter::NoScale(c) => (c.width(), c.height()),
                        GpuConverter::Scaled(c) => (c.src_width, c.src_height),
                    }
                }
                fn convert(
                    &self,
                    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
                    context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
                    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
                    context_mutex: &parking_lot::Mutex<()>,
                    out: &mut I420Planes,
                ) -> Result<GpuConvertTiming, crate::d3d11_i420::D3d11I420Error> {
                    match self {
                        GpuConverter::NoScale(c) => c.convert(device, context, texture, context_mutex, out),
                        GpuConverter::Scaled(c) => c.convert(device, context, texture, context_mutex, out),
                    }
                }
            }
            let mut gpu_converter: Option<GpuConverter> = None;
            let mut gpu_path_failed = false;
            let mut gpu_path_logged = false;

            // Encoded H.264 path — D3d11BgraToNv12 + backend (NVENC D3D11 preferred on NVIDIA,
            // then MFT fallback) lazily initialized when the pool is ready.
            let mut bgra_to_nv12: Option<D3d11BgraToNv12> = None;
            let mut mft_encoder: Option<EncodedH264Encoder> = None;
            let mut mft_path_failed = false;
            let mut mft_hard_failed = false;
            let mut mft_retry_at: Option<std::time::Instant> = None;
            let mut mft_transient_timeouts = 0u32;
            let mut mft_path_logged = false;
            let mut cpu_path_encoder_logged = false;
            let mut gpu_i420_encoder_logged = false;
            // Encoded-path fallback: when the encoded backend hard-fails, create a Native
            // (I420) track and send it to the async context so it can unpublish the
            // Encoded track and republish Native.
            let mut mft_fallback_sent = false;
            let mft_fallback_resolution = resolution.clone();
            // Reusable I420 plane buffers — allocated once, reused every frame to avoid
            // ~3 MB/frame heap churn (186 MB/s at 1080p60) that causes allocator jitter.
            let mut planes_buf = I420Planes::new_empty();
            // True once planes_buf contains a valid I420 frame (set after first successful encode).
            // Used to re-send the last frame at constant FPS when WGC has no new content (static screen).
            let mut have_last_frame = false;
            // Force-keyframe: repeat the very first frame 3× so late-joining viewers
            // receive the IDR before the stream stabilises. The first I420 frame sent to
            // NativeVideoSource is always encoded as an IDR by OpenH264; repeating it
            // ensures it arrives even under initial packet loss / jitter.
            let mut startup_keyframe_done = false;

            // CPU path buffer reuse: two I420 buffers, no per-frame alloc after first frame.
            let mut cpu_returned_buffer: Option<livekit::webrtc::video_frame::I420Buffer> = None;
            let mut cpu_other_buffer: Option<livekit::webrtc::video_frame::I420Buffer> = None;

            // Phase 6.2: GPU timing accumulators (reset every INSTR_WINDOW frames).
            let mut instr_gpu_dispatch_ns: u64 = 0;
            let mut instr_gpu_copy_ns:     u64 = 0;
            let mut instr_gpu_map_ns:      u64 = 0;
            let mut instr_gpu_total_ns:    u64 = 0;

            // Track WGC frame counter to skip BGRA→NV12 when content unchanged.
            let mut last_wgc_count: u64 = 0;

            // Extra intervals to advance when we sent multiple frames in one round (e.g. 3× keyframe).
            let mut extra_intervals = 0i64;
            let mut prev_frame_time: Option<std::time::Instant> = None;
            let mut prev_send_time: Option<std::time::Instant> = None;
            // Phase 4.7: GIR + periodic forced IDR for packet loss recovery.
            // ASTRIX_PERIODIC_IDR_SECS=N (0 = disable, not recommended on WAN). Longer interval = fewer IDR spikes.
            let periodic_idr_override_secs: Option<u64> = std::env::var("ASTRIX_PERIODIC_IDR_SECS")
                .ok()
                .and_then(|s| s.parse().ok());
            if periodic_idr_override_secs == Some(0) {
                eprintln!("[voice][screen] ASTRIX_PERIODIC_IDR_SECS=0: periodic IDR disabled");
            } else if let Some(periodic_idr_secs) = periodic_idr_override_secs {
                eprintln!(
                    "[voice][screen] periodic IDR interval: {}s (from ASTRIX_PERIODIC_IDR_SECS)",
                    periodic_idr_secs
                );
            }
            let mut last_forced_idr_at: Option<std::time::Instant> = None;
            let effective_periodic_idr_secs = |enc: Option<&EncodedH264Encoder>| -> u64 {
                periodic_idr_override_secs.unwrap_or(match enc.map(|e| e.backend_kind()) {
                    // Keep NVENC less bursty than before, but stay conservative
                    // enough to preserve reliable startup/recovery across drivers.
                    Some(EncodedBackendKind::NvencD3d11) => 20,
                    _ => 12,
                })
            };
            loop {
                if stop_enc.load(Ordering::Relaxed) {
                    break;
                }

                let now = std::time::Instant::now();
                if prefer_mft_no_fallback && mft_path_failed && !mft_hard_failed {
                    if mft_retry_at.map(|t| now >= t).unwrap_or(true) {
                        eprintln!("[voice][screen] DXGI+MFT: retrying encoder path after reset");
                        mft_path_failed = false;
                        mft_hard_failed = false;
                        mft_retry_at = None;
                        mft_encoder = None;
                        bgra_to_nv12 = None;
                        mft_transient_timeouts = 0;
                        last_wgc_count = 0;
                    } else {
                        let wait_interval = if frame_count < warmup_frames {
                            warmup_interval
                        } else {
                            current_effective_interval
                        };
                        next_frame_at += wait_interval;
                        while next_frame_at <= std::time::Instant::now() {
                            next_frame_at += wait_interval;
                        }
                        let remaining = next_frame_at - std::time::Instant::now();
                        if remaining > Duration::from_millis(PRE_BUFFER_MS) {
                            std::thread::sleep(remaining - Duration::from_millis(PRE_BUFFER_MS));
                        }
                        while std::time::Instant::now() < next_frame_at {
                            std::hint::spin_loop();
                        }
                        continue;
                    }
                }

                let source_fps = (wgc_source_fps_milli_enc.load(Ordering::Relaxed) as f64) / 1000.0;
                if !lock_preset_fps && source_fps >= 10.0 {
                    let source_cap_candidate = ADAPTIVE_FPS_LADDER
                        .iter()
                        .rev()
                        .copied()
                        .find(|&fps| fps <= source_fps + 2.0 && fps <= max_fps_f64)
                        .unwrap_or(30.0);
                    let lower_enough =
                        source_cap_candidate < current_source_cap_fps
                        && source_fps <= current_source_cap_fps - SOURCE_CAP_HYSTERESIS_FPS;
                    let higher_enough =
                        source_cap_candidate > current_source_cap_fps
                        && source_fps >= current_source_cap_fps + SOURCE_CAP_HYSTERESIS_FPS;

                    if lower_enough {
                        source_cap_down_votes = source_cap_down_votes.saturating_add(1);
                        source_cap_up_votes = 0;
                    } else if higher_enough {
                        source_cap_up_votes = source_cap_up_votes.saturating_add(1);
                        source_cap_down_votes = 0;
                    } else {
                        source_cap_down_votes = 0;
                        source_cap_up_votes = 0;
                    }

                    let should_apply =
                        (lower_enough && source_cap_down_votes >= SOURCE_CAP_CONFIRM_WINDOWS)
                        || (higher_enough && source_cap_up_votes >= SOURCE_CAP_CONFIRM_WINDOWS);

                    if should_apply {
                        if telemetry_enabled {
                            eprintln!(
                                "[voice][screen] source-adaptive: fps ceiling {:.0} → {:.0} (capture source {:.1} fps)",
                                current_source_cap_fps, source_cap_candidate, source_fps
                            );
                        }
                        current_source_cap_fps = source_cap_candidate;
                        source_cap_down_votes = 0;
                        source_cap_up_votes = 0;
                        if current_fps > current_source_cap_fps {
                            current_fps = current_source_cap_fps;
                            current_frame_interval_ns = (1_000_000_000.0 / current_fps) as u64;
                            current_full_interval = Duration::from_nanos(current_frame_interval_ns);
                            current_effective_interval = if current_full_interval.as_nanos() as u64 > MAX_STATIC_INTERVAL_NS {
                                Duration::from_nanos(MAX_STATIC_INTERVAL_NS)
                            } else {
                                current_full_interval
                            };
                            current_rtp_step = 90_000_u32 / current_fps as u32;
                        } else if current_fps < current_source_cap_fps {
                            let raised_fps = current_source_cap_fps;
                            current_fps = raised_fps;
                            current_frame_interval_ns = (1_000_000_000.0 / current_fps) as u64;
                            current_full_interval = Duration::from_nanos(current_frame_interval_ns);
                            current_effective_interval = if current_full_interval.as_nanos() as u64 > MAX_STATIC_INTERVAL_NS {
                                Duration::from_nanos(MAX_STATIC_INTERVAL_NS)
                            } else {
                                current_full_interval
                            };
                            current_rtp_step = 90_000_u32 / current_fps as u32;
                        }
                    }
                } else if lock_preset_fps {
                    current_source_cap_fps = max_fps_f64;
                }
                let startup_cap_fps = startup_transport_fps_cap(
                    webrtc_target_bitrate_bps,
                    enc_session_start.elapsed(),
                );
                if telemetry_enabled
                    && (startup_cap_fps - last_startup_transport_cap_fps).abs() >= 0.5
                {
                    eprintln!(
                        "[voice][screen] startup transport cap: fps {:.0} → {:.0} (bwe={:.1} Mbps / preset={:.1} Mbps)",
                        last_startup_transport_cap_fps,
                        startup_cap_fps,
                        webrtc_target_bitrate_bps as f64 / 1_000_000.0,
                        preset_bitrate_bps_u32 as f64 / 1_000_000.0
                    );
                    last_startup_transport_cap_fps = startup_cap_fps;
                }
                if lock_preset_fps {
                    if (current_fps - startup_cap_fps).abs() >= 0.5 {
                        current_fps = startup_cap_fps;
                        current_frame_interval_ns = (1_000_000_000.0 / current_fps) as u64;
                        current_full_interval = Duration::from_nanos(current_frame_interval_ns);
                        current_effective_interval = if current_full_interval.as_nanos() as u64 > MAX_STATIC_INTERVAL_NS {
                            Duration::from_nanos(MAX_STATIC_INTERVAL_NS)
                        } else {
                            current_full_interval
                        };
                        current_rtp_step = 90_000_u32 / current_fps as u32;
                        downgrade_frames = 0;
                        upgrade_frames = 0;
                    }
                } else if current_fps > startup_cap_fps {
                    current_fps = startup_cap_fps;
                    current_frame_interval_ns = (1_000_000_000.0 / current_fps) as u64;
                    current_full_interval = Duration::from_nanos(current_frame_interval_ns);
                    current_effective_interval = if current_full_interval.as_nanos() as u64 > MAX_STATIC_INTERVAL_NS {
                        Duration::from_nanos(MAX_STATIC_INTERVAL_NS)
                    } else {
                        current_full_interval
                    };
                    current_rtp_step = 90_000_u32 / current_fps as u32;
                    downgrade_frames = 0;
                    upgrade_frames = 0;
                }
                let interval = if frame_count < warmup_frames { warmup_interval } else { current_effective_interval };
                let past_startup = frame_count >= STARTUP_ACCEPT_FRAMES;
                // RTP timestamps must be monotonic from stream start (microseconds). Unix epoch
                // causes LiveKit SFU "adjusting first packet time, too big" spam.
                let frame_interval_us = 1_000_000_u64 / current_fps as u64;
                let ts_us = running_ts_us;
                // Monotonic sample timestamp for the encoder pipeline.
                let capture_us = enc_session_start.elapsed().as_micros() as i64;
                push_trace_stats.maybe_log(
                    &mut push_trace_window_start,
                    max_fps,
                    current_fps,
                    if mft_pipelined_enabled && startup_keyframe_done {
                        "pipelined"
                    } else {
                        "blocking"
                    },
                );
                push_trace_tick_idx = push_trace_tick_idx.saturating_add(1);
                push_trace_stats.ticks = push_trace_stats.ticks.saturating_add(1);
                let trace_this_tick =
                    push_trace_verbose && push_trace_tick_idx <= push_trace_frame_limit;
                if trace_this_tick {
                    eprintln!(
                        "[voice][screen][push][tick {}] capture_us={} ts_us={} target={:.0} schedule={:.0} startup={} have_last={} mft_failed={}",
                        push_trace_tick_idx,
                        capture_us,
                        ts_us,
                        max_fps,
                        current_fps,
                        past_startup,
                        have_last_frame,
                        mft_path_failed,
                    );
                }

                // Phase 5.1/5.2: MFT path must use GPU; CPU path tries GPU first (auto).
                // OBS-style: encoder reads single "latest" slot on each timer tick (no queue).
                let try_gpu = capture_path_mft || (matches!(encode_path_enc, EncodePath::Cpu) && !gpu_path_failed);
                let gpu_slot = if try_gpu { latest_slot_enc.load() } else { None };
                if gpu_slot.is_some() {
                    push_trace_stats.slot_ready = push_trace_stats.slot_ready.saturating_add(1);
                } else {
                    push_trace_stats.slot_missing = push_trace_stats.slot_missing.saturating_add(1);
                }
                if trace_this_tick {
                    eprintln!(
                        "[voice][screen][push][tick {}] gpu_slot={:?} try_gpu={}",
                        push_trace_tick_idx,
                        gpu_slot,
                        try_gpu,
                    );
                }
                // Phase 5.2: MFT path requires GPU; when no slot yet wait and retry (no CPU fallback).
                if capture_path_mft && gpu_slot.is_none() {
                    if trace_this_tick {
                        eprintln!(
                            "[voice][screen][push][tick {}] no gpu slot; have_last_frame={} mft_failed={}",
                            push_trace_tick_idx,
                            have_last_frame,
                            mft_path_failed,
                        );
                    }
                    // Static screen: re-encode last NV12 texture to maintain constant FPS.
                    if have_last_frame && !mft_path_failed {
                        if let (Some(ref enc_src), Some(ref b2n), Some(ref mut mft)) =
                            (&encoded_source_enc, &bgra_to_nv12, &mut mft_encoder)
                        {
                            push_trace_stats.static_reencode_ticks =
                                push_trace_stats.static_reencode_ticks.saturating_add(1);
                            let nv12_tex = b2n.output_texture();
                            let force_keyframe_now =
                                keyframe_request_enc.load(Ordering::Relaxed);
                            let reencode_start = std::time::Instant::now();
                                match mft.encode(nv12_tex, ts_us, force_keyframe_now) {
                                Ok(frames) => {
                                    let reencode_us = reencode_start.elapsed().as_micros() as u64;
                                    push_trace_stats.static_reencode_ok =
                                        push_trace_stats.static_reencode_ok.saturating_add(1);
                                    push_trace_stats.encode_ok =
                                        push_trace_stats.encode_ok.saturating_add(1);
                                    push_trace_stats.encode_us_total =
                                        push_trace_stats.encode_us_total.saturating_add(reencode_us);
                                    if trace_this_tick {
                                        eprintln!(
                                            "[voice][screen][push][tick {}] static re-encode ok frames={} encode_us={}",
                                            push_trace_tick_idx,
                                            frames.len(),
                                            reencode_us,
                                        );
                                    }
                                    let send_start = std::time::Instant::now();
                                    let mut sent_keyframe = false;
                                    for ef in frames.iter() {
                                        sent_keyframe |= ef.key_frame;
                                        enc_src.push_frame(&ef.data, rtp_timestamp, capture_us, ef.key_frame);
                                        if telemetry_enabled {
                                            let now_us = enc_session_start.elapsed().as_micros() as i64;
                                            println!("SEND: rtp={} capture={} delta={} now={}", rtp_timestamp, capture_us, capture_us - last_send_capture_us, now_us);
                                            last_send_capture_us = capture_us;
                                        }
                                        rtp_timestamp = rtp_timestamp.wrapping_add(current_rtp_step);
                                    }
                                    if sent_keyframe {
                                        keyframe_request_enc.store(false, Ordering::Relaxed);
                                    }
                                    push_trace_stats.send_calls =
                                        push_trace_stats.send_calls.saturating_add(1);
                                    push_trace_stats.sent_frames = push_trace_stats
                                        .sent_frames
                                        .saturating_add(frames.len() as u64);
                                    push_trace_stats.send_us_total = push_trace_stats
                                        .send_us_total
                                        .saturating_add(send_start.elapsed().as_micros() as u64);
                                    frame_count += 1;
                                    fps_frame_count += 1;
                                    running_ts_us += frame_interval_us as i64;
                                }
                                Err(err) => {
                                    push_trace_stats.encode_fail =
                                        push_trace_stats.encode_fail.saturating_add(1);
                                    if trace_this_tick {
                                        eprintln!(
                                            "[voice][screen][push][tick {}] static re-encode failed: {:?}",
                                            push_trace_tick_idx,
                                            err,
                                        );
                                    }
                                }
                            }
                            let elapsed = fps_window_start.elapsed();
                            if elapsed >= Duration::from_secs(1) {
                                let actual_fps = fps_frame_count as f32 / elapsed.as_secs_f32();
                                if let Some(mut st) = stats_enc.try_lock() {
                                    st.stream_fps = Some(actual_fps);
                                    st.frames_per_second = Some(actual_fps);
                                }
                                fps_window_start = std::time::Instant::now();
                                fps_frame_count = 0;
                            }
                        }
                    }
                    let now = std::time::Instant::now();
                    if next_frame_at > now {
                        let rem = next_frame_at - now;
                        if rem > Duration::from_millis(PRE_BUFFER_MS) {
                            std::thread::sleep(rem - Duration::from_millis(PRE_BUFFER_MS));
                        }
                        while std::time::Instant::now() < next_frame_at {
                            std::hint::spin_loop();
                        }
                    }
                    next_frame_at += if frame_count < warmup_frames { warmup_interval } else { current_effective_interval };
                    continue;
                }
                if let Some(gpu_slot) = gpu_slot {
                    // Throttle: drop slot only if we are more than 2 intervals late past startup.
                    // At 60 fps the GPU convert() takes ~16 ms (one full interval), so checking
                    // "late by more than one interval" would drop every other frame and halve the
                    // effective FPS. Two-interval threshold gives the GPU time to finish without
                    // false drops.
                    let late_by_more_than_two = now > next_frame_at + interval + interval;
                    if past_startup && late_by_more_than_two {
                        push_trace_stats.late_drop_ticks =
                            push_trace_stats.late_drop_ticks.saturating_add(1);
                        let mut dropped = 0u64;
                        while next_frame_at <= now {
                            next_frame_at += interval;
                            dropped += 1;
                        }
                        push_trace_stats.late_drop_frames =
                            push_trace_stats.late_drop_frames.saturating_add(dropped);
                        if trace_this_tick {
                            eprintln!(
                                "[voice][screen][push][tick {}] late drop: dropped={} interval_ms={:.2}",
                                push_trace_tick_idx,
                                dropped,
                                interval.as_secs_f64() * 1000.0,
                            );
                        }
                        if dropped > 0 {
                            telemetry_enc.add_frames_dropped(dropped);
                        }
                        let remaining = next_frame_at - std::time::Instant::now();
                        if remaining > Duration::from_millis(PRE_BUFFER_MS) {
                            std::thread::sleep(remaining - Duration::from_millis(PRE_BUFFER_MS));
                        }
                        while std::time::Instant::now() < next_frame_at {
                            std::hint::spin_loop();
                        }
                        continue;
                    }

                    // Phase 5.1: MFT path — BGRA→NV12→H.264→push_frame (zero-copy).
                    if let Some(ref enc_src) = encoded_source_enc {
                        if !mft_path_failed {
                            let mut converted_nv12_tex = None;
                            let requested_bitrate_bps = enc_src.target_bitrate_bps();
                            let requested_fps = enc_src.target_framerate_fps();
                            let mut bwe_changed = false;
                            if requested_bitrate_bps > 0 {
                                let capped_bitrate_bps =
                                    requested_bitrate_bps.clamp(250_000, preset_bitrate_bps_u32);
                                if capped_bitrate_bps.abs_diff(webrtc_target_bitrate_bps) >= 250_000 {
                                    if let Some(ref mut mft) = mft_encoder {
                                        match mft.set_bitrate(capped_bitrate_bps) {
                                            Ok(()) => {
                                                webrtc_target_bitrate_bps = capped_bitrate_bps;
                                                bwe_changed = true;
                                            }
                                            Err(e) => {
                                                eprintln!(
                                                    "[voice][screen] MFT bitrate update failed ({} bps): {:?}",
                                                    capped_bitrate_bps,
                                                    e
                                                );
                                            }
                                        }
                                    } else {
                                        webrtc_target_bitrate_bps = capped_bitrate_bps;
                                        bwe_changed = true;
                                    }
                                }
                            }
                            if requested_fps > 0.0 {
                                let capped_fps = requested_fps.clamp(1.0, max_fps_f64);
                                if (capped_fps - webrtc_target_fps).abs() >= 1.0 {
                                    webrtc_target_fps = capped_fps;
                                    bwe_changed = true;
                                }
                            }
                            if bwe_changed {
                                if telemetry_enabled {
                                    eprintln!(
                                        "[voice][screen] WebRTC BWE target: bitrate={:.1} Mbps fps_hint={:.1}",
                                        webrtc_target_bitrate_bps as f64 / 1_000_000.0,
                                        webrtc_target_fps
                                    );
                                }
                                if let Some(mut st) = stats_enc.try_lock() {
                                    st.connection_speed_mbps =
                                        Some(webrtc_target_bitrate_bps as f32 / 1_000_000.0);
                                }
                            }
                            // Phase 1: lazy init + BGRA→NV12 (pool lock scope).
                            // Skip conversion when WGC hasn't delivered new content —
                            // saves GPU work and reduces pool lock hold time so WGC
                            // can write new frames during encode.
                            let current_wgc = wgc_frame_count_enc.load(Ordering::Relaxed);
                            let is_new_content = current_wgc != last_wgc_count;
                            let need_init = bgra_to_nv12.is_none() || mft_encoder.is_none();
                            if need_init {
                                push_trace_stats.need_init_ticks =
                                    push_trace_stats.need_init_ticks.saturating_add(1);
                            }
                            if is_new_content {
                                push_trace_stats.new_content_ticks =
                                    push_trace_stats.new_content_ticks.saturating_add(1);
                            } else {
                                push_trace_stats.reuse_content_ticks =
                                    push_trace_stats.reuse_content_ticks.saturating_add(1);
                            }
                            if trace_this_tick {
                                eprintln!(
                                    "[voice][screen][push][tick {}] content: current_wgc={} last_wgc={} new={} need_init={}",
                                    push_trace_tick_idx,
                                    current_wgc,
                                    last_wgc_count,
                                    is_new_content,
                                    need_init,
                                );
                            }

                            if need_init || is_new_content {
                                let pool_guard = pool_ref_enc.lock();
                                let pool = match pool_guard.as_ref() {
                                    Some(p) => p,
                                    None => {
                                        push_trace_stats.pool_missing =
                                            push_trace_stats.pool_missing.saturating_add(1);
                                        if trace_this_tick {
                                            eprintln!(
                                                "[voice][screen][push][tick {}] pool missing",
                                                push_trace_tick_idx
                                            );
                                        }
                                        next_frame_at += interval;
                                        continue
                                    },
                                };

                                // Lazy init D3d11BgraToNv12 and the encoded H.264 backend.
                                let fps_u32 = max_fps as u32;
                                let bitrate_u32 = webrtc_target_bitrate_bps;
                                if bgra_to_nv12.is_none() {
                                    match D3d11BgraToNv12::new(
                                        &pool.device, &pool.context,
                                        pool.width, pool.height,
                                        video_width, video_height,
                                        fps_u32,
                                    ) {
                                        Ok(conv) => bgra_to_nv12 = Some(conv),
                                        Err(e) => {
                                            eprintln!("[voice][screen] D3d11BgraToNv12 init failed: {:?}", e);
                                            mft_path_failed = true;
                                            mft_hard_failed = true;
                                            next_frame_at += interval;
                                            continue;
                                        }
                                    }
                                }
                                if mft_encoder.is_none() {
                                    let encoder_init = if encode_path_enc == EncodePath::Auto {
                                        EncodedH264Encoder::new_auto(
                                            &pool.device,
                                            video_width,
                                            video_height,
                                            fps_u32,
                                            bitrate_u32,
                                            bgra_to_nv12.as_ref().unwrap().output_textures(),
                                        )
                                    } else {
                                        EncodedH264Encoder::new_mft(
                                            &pool.device,
                                            video_width,
                                            video_height,
                                            fps_u32,
                                            bitrate_u32,
                                        )
                                    };
                                    match encoder_init {
                                        Ok(enc) => {
                                            mft_encoder = Some(enc);
                                            if !mft_path_logged {
                                                let enc_ref = mft_encoder.as_ref().unwrap();
                                                let path_str = match enc_ref.backend_kind() {
                                                    EncodedBackendKind::NvencD3d11 => {
                                                        format!("NVENC D3D11 ({}, hardware)", enc_ref.encoder_name())
                                                    }
                                                    EncodedBackendKind::MftHardware => {
                                                        format!("MFT GPU ({}, hardware)", enc_ref.encoder_name())
                                                    }
                                                    EncodedBackendKind::MftSoftware => {
                                                        format!("MFT software ({})", enc_ref.encoder_name())
                                                    }
                                                };
                                                eprintln!("[voice][screen] Screen capture: {}", path_str);
                                                if periodic_idr_override_secs.is_none()
                                                    && matches!(enc_ref.backend_kind(), EncodedBackendKind::NvencD3d11)
                                                {
                                                    eprintln!(
                                                        "[voice][screen] NVENC tuning: preset bitrate cap {:.1} Mbps, periodic IDR default 20s",
                                                        bitrate_u32 as f64 / 1_000_000.0
                                                    );
                                                }
                                                if enc_ref.is_async() {
                                                    if mft_pipelined_enabled {
                                                        eprintln!("[voice][screen] encoded backend steady-state: pipelined submit+collect mode");
                                                    } else {
                                                        eprintln!("[voice][screen] encoded backend steady-state: blocking encode() mode (ASTRIX_MFT_PIPELINED=0)");
                                                    }
                                                }
                                                telemetry_enc.set_encoder_type(&path_str);
                                                mft_path_logged = true;
                                                gpu_encode_active_enc.store(true, Ordering::Relaxed);
                                            }
                                        }
                                        Err(e) => {
                                            eprintln!("[voice][screen] encoded H.264 backend init failed: {:?}", e);
                                            mft_path_failed = true;
                                            mft_hard_failed = true;
                                            next_frame_at += interval;
                                            continue;
                                        }
                                    }
                                }

                                let b2n = bgra_to_nv12.as_ref().unwrap();
                                let texture = pool.textures[gpu_slot as usize].clone();
                                let context_mutex = Arc::clone(&pool.context_mutex);
                                let convert_start = std::time::Instant::now();
                                match b2n.convert(&pool.context, &texture, &context_mutex) {
                                    Ok(nv12_tex) => {
                                        let convert_us = convert_start.elapsed().as_micros() as u64;
                                        converted_nv12_tex = Some(nv12_tex);
                                        push_trace_stats.convert_ok =
                                            push_trace_stats.convert_ok.saturating_add(1);
                                        push_trace_stats.convert_us_total =
                                            push_trace_stats.convert_us_total.saturating_add(convert_us);
                                        if trace_this_tick {
                                            eprintln!(
                                                "[voice][screen][push][tick {}] convert ok slot={} us={}",
                                                push_trace_tick_idx,
                                                gpu_slot,
                                                convert_us,
                                            );
                                        }
                                        let flush_start = std::time::Instant::now();
                                        if let Err(e) = b2n.flush(&context_mutex) {
                                            push_trace_stats.flush_fail =
                                                push_trace_stats.flush_fail.saturating_add(1);
                                            if trace_this_tick {
                                                eprintln!(
                                                    "[voice][screen][push][tick {}] flush failed: {:?}",
                                                    push_trace_tick_idx,
                                                    e,
                                                );
                                            }
                                            eprintln!("[voice][screen] BGRA→NV12 flush failed: {:?}", e);
                                            mft_path_failed = true;
                                            mft_hard_failed = true;
                                            continue;
                                        }
                                        let flush_us = flush_start.elapsed().as_micros() as u64;
                                        push_trace_stats.flush_ok =
                                            push_trace_stats.flush_ok.saturating_add(1);
                                        push_trace_stats.flush_us_total =
                                            push_trace_stats.flush_us_total.saturating_add(flush_us);
                                        if trace_this_tick {
                                            eprintln!(
                                                "[voice][screen][push][tick {}] flush ok us={}",
                                                push_trace_tick_idx,
                                                flush_us,
                                            );
                                        }
                                        if nv12_ready_trace {
                                            match b2n.poll_output_ready() {
                                                Ok(true) => {
                                                    push_trace_stats.ready_immediate =
                                                        push_trace_stats.ready_immediate.saturating_add(1);
                                                    if trace_this_tick {
                                                        eprintln!(
                                                            "[voice][screen][push][tick {}] ready immediate=true wait_us=0",
                                                            push_trace_tick_idx,
                                                        );
                                                    }
                                                }
                                                Ok(false) => match b2n.wait_output_ready(200) {
                                                    Ok(wait_us) => {
                                                        push_trace_stats.ready_waited =
                                                            push_trace_stats.ready_waited.saturating_add(1);
                                                        push_trace_stats.ready_wait_us_total =
                                                            push_trace_stats.ready_wait_us_total.saturating_add(wait_us);
                                                        if trace_this_tick {
                                                            eprintln!(
                                                                "[voice][screen][push][tick {}] ready immediate=false wait_us={}",
                                                                push_trace_tick_idx,
                                                                wait_us,
                                                            );
                                                        }
                                                    }
                                                    Err(e) => {
                                                        push_trace_stats.ready_timeout =
                                                            push_trace_stats.ready_timeout.saturating_add(1);
                                                        if trace_this_tick {
                                                            eprintln!(
                                                                "[voice][screen][push][tick {}] ready wait failed: {:?}",
                                                                push_trace_tick_idx,
                                                                e,
                                                            );
                                                        }
                                                    }
                                                },
                                                Err(e) => {
                                                    push_trace_stats.ready_timeout =
                                                        push_trace_stats.ready_timeout.saturating_add(1);
                                                    if trace_this_tick {
                                                        eprintln!(
                                                            "[voice][screen][push][tick {}] ready poll failed: {:?}",
                                                            push_trace_tick_idx,
                                                            e,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        telemetry_enc.set_convert(convert_us.saturating_add(flush_us));
                                        last_wgc_count = current_wgc;
                                    }
                                    Err(e) => {
                                        push_trace_stats.convert_fail =
                                            push_trace_stats.convert_fail.saturating_add(1);
                                        if trace_this_tick {
                                            eprintln!(
                                                "[voice][screen][push][tick {}] convert failed: {:?}",
                                                push_trace_tick_idx,
                                                e,
                                            );
                                        }
                                        eprintln!("[voice][screen] BGRA→NV12 convert failed: {:?}", e);
                                        mft_path_failed = true;
                                        mft_hard_failed = true;
                                        continue;
                                    }
                                }
                                // pool_guard dropped here — WGC can write during encode
                            }

                            // Phase 2: H.264 encode (no pool lock needed).
                            // When WGC hasn't delivered new content yet, reuse the last
                            // converter output texture instead of unwrapping a missing
                            // per-tick convert result.
                            let nv12_tex = match converted_nv12_tex
                                .as_ref()
                                .or_else(|| bgra_to_nv12.as_ref().map(|b2n| b2n.output_texture()))
                            {
                                Some(tex) => tex,
                                None => {
                                    eprintln!(
                                        "[voice][screen] encoded path missing NV12 texture (no converted frame available)"
                                    );
                                    mft_path_failed = true;
                                    mft_hard_failed = true;
                                    continue;
                                }
                            };
                            // Phase 4.6: pipelined submit/collect for async MFT.
                            // Use submit + collect_blocking so frame is pushed in same iteration (no 1-frame delay).
                            let use_pipelined = mft_encoder.as_ref().map(|m| m.is_async()).unwrap_or(false)
                                && startup_keyframe_done
                                && mft_pipelined_enabled;
                            let (mft_result, encode_us, pipelined_collected) = if use_pipelined {
                                let mft = mft_encoder.as_mut().unwrap();
                                let periodic_idr_secs = effective_periodic_idr_secs(Some(mft));
                                let now_idr = std::time::Instant::now();
                                let need_periodic_idr = if periodic_idr_secs == 0 {
                                    false
                                } else {
                                    match last_forced_idr_at {
                                        None => {
                                            last_forced_idr_at = Some(now_idr);
                                            false
                                        }
                                        Some(t) => {
                                            now_idr.duration_since(t)
                                                >= Duration::from_secs(periodic_idr_secs)
                                        }
                                    }
                                };
                                if need_periodic_idr {
                                    last_forced_idr_at = Some(now_idr);
                                    eprintln!(
                                        "[voice][screen] periodic IDR (GIR recovery, every {}s)",
                                        periodic_idr_secs
                                    );
                                }
                                let key_frame =
                                    frame_count < 3
                                        || need_periodic_idr
                                        || keyframe_request_enc.load(Ordering::Relaxed);
                                // The first frame already went through blocking encode(). In pipelined
                                // mode, only keep a tiny blocking window for the next startup IDRs.
                                // Using collect_blocking() for the full STARTUP_ACCEPT_FRAMES window
                                // caps sender throughput at roughly the MFT output latency.
                                let startup_collect_blocking = frame_count < 3;
                                let steady_timeout_ms = match mft.backend_kind() {
                                    EncodedBackendKind::NvencD3d11 => {
                                        ((current_effective_interval.as_millis() as u32)
                                            .saturating_mul(6))
                                        .clamp(20, 120)
                                    }
                                    _ => ((current_effective_interval.as_millis() as u32)
                                        .saturating_mul(2))
                                    .clamp(10, 80),
                                };
                                let event_timeout_ms = if startup_collect_blocking || key_frame {
                                    120
                                } else {
                                    steady_timeout_ms
                                };
                                let encode_start = std::time::Instant::now();
                                // Submit current frame, then block until it is encoded.
                                // No pre-drain collect() loop: draining multiple frames at once
                                // causes a burst send that inflates WebRTC jitter buffer delay.
                                let submit_start = std::time::Instant::now();
                                let mft_res = mft.submit(
                                    nv12_tex,
                                    ts_us as i64,
                                    key_frame,
                                    rtp_timestamp,
                                    capture_us,
                                    event_timeout_ms,
                                );
                                let submit_us = submit_start.elapsed().as_micros() as u64;
                                push_trace_stats.submit_us_total =
                                    push_trace_stats.submit_us_total.saturating_add(submit_us);
                                match &mft_res {
                                    Ok(()) => {
                                        push_trace_stats.submit_ok =
                                            push_trace_stats.submit_ok.saturating_add(1);
                                        if trace_this_tick {
                                            eprintln!(
                                                "[voice][screen][push][tick {}] submit ok key={} timeout_ms={} us={}",
                                                push_trace_tick_idx,
                                                key_frame,
                                                event_timeout_ms,
                                                submit_us,
                                            );
                                        }
                                    }
                                    Err(err) => {
                                        push_trace_stats.submit_fail =
                                            push_trace_stats.submit_fail.saturating_add(1);
                                        if trace_this_tick {
                                            eprintln!(
                                                "[voice][screen][push][tick {}] submit failed key={} timeout_ms={} us={} err={:?}",
                                                push_trace_tick_idx,
                                                key_frame,
                                                event_timeout_ms,
                                                submit_us,
                                                err,
                                            );
                                        }
                                    }
                                }
                                let mut collected_this: i64 = 0;
                                if mft_res.is_ok() {
                                    let collect_start = std::time::Instant::now();
                                    let collect_res = if startup_collect_blocking {
                                        mft.collect_blocking(event_timeout_ms)
                                    } else {
                                        mft.collect()
                                    };
                                    let collect_us = collect_start.elapsed().as_micros() as u64;
                                    match collect_res {
                                        Ok(Some((frames, _rtp_prev, cap_prev, enc_us))) => {
                                            push_trace_stats.collect_some =
                                                push_trace_stats.collect_some.saturating_add(1);
                                            push_trace_stats.collect_frames = push_trace_stats
                                                .collect_frames
                                                .saturating_add(frames.len() as u64);
                                            push_trace_stats.collect_us_total = push_trace_stats
                                                .collect_us_total
                                                .saturating_add(collect_us);
                                            telemetry_enc.set_encode(enc_us);
                                            if trace_this_tick {
                                                eprintln!(
                                                    "[voice][screen][push][tick {}] collect some frames={} cap_prev={} enc_us={} wait_us={}",
                                                    push_trace_tick_idx,
                                                    frames.len(),
                                                    cap_prev,
                                                    enc_us,
                                                    collect_us,
                                                );
                                            }
                                            let send_start = std::time::Instant::now();
                                            let mut sent_keyframe = false;
                                            for ef in frames {
                                                sent_keyframe |= ef.key_frame;
                                                enc_src.push_frame(&ef.data, rtp_timestamp, cap_prev, ef.key_frame);
                                                if telemetry_enabled {
                                                    let now_us = enc_session_start.elapsed().as_micros() as i64;
                                                    println!("SEND: rtp={} capture={} delta={} now={}", rtp_timestamp, cap_prev, cap_prev - last_send_capture_us, now_us);
                                                    last_send_capture_us = cap_prev;
                                                }
                                                collected_this += 1;
                                                rtp_timestamp = rtp_timestamp.wrapping_add(current_rtp_step);
                                            }
                                            if sent_keyframe {
                                                keyframe_request_enc.store(false, Ordering::Relaxed);
                                            }
                                            push_trace_stats.send_calls =
                                                push_trace_stats.send_calls.saturating_add(1);
                                            let send_us = send_start.elapsed().as_micros() as u64;
                                            push_trace_stats.sent_frames = push_trace_stats
                                                .sent_frames
                                                .saturating_add(collected_this as u64);
                                            push_trace_stats.send_us_total = push_trace_stats
                                                .send_us_total
                                                .saturating_add(send_us);
                                            telemetry_enc.set_send(send_us);
                                        }
                                        Ok(None) => {
                                            push_trace_stats.collect_none =
                                                push_trace_stats.collect_none.saturating_add(1);
                                            push_trace_stats.collect_us_total = push_trace_stats
                                                .collect_us_total
                                                .saturating_add(collect_us);
                                            if trace_this_tick {
                                                eprintln!(
                                                    "[voice][screen][push][tick {}] collect none wait_us={}",
                                                    push_trace_tick_idx,
                                                    collect_us,
                                                );
                                            }
                                        }
                                        Err(err) => {
                                            push_trace_stats.collect_err =
                                                push_trace_stats.collect_err.saturating_add(1);
                                            push_trace_stats.collect_us_total = push_trace_stats
                                                .collect_us_total
                                                .saturating_add(collect_us);
                                            if trace_this_tick {
                                                eprintln!(
                                                    "[voice][screen][push][tick {}] collect failed wait_us={} err={:?}",
                                                    push_trace_tick_idx,
                                                    collect_us,
                                                    err,
                                                );
                                            }
                                        }
                                    }
                                }
                                let us = encode_start.elapsed().as_micros() as u64;
                                (mft_res.map(|_| vec![]), us, collected_this)
                            } else {
                                let mft = mft_encoder.as_mut().unwrap();
                                let periodic_idr_secs = effective_periodic_idr_secs(Some(mft));
                                let now_idr = std::time::Instant::now();
                                let need_periodic_idr = if periodic_idr_secs == 0 {
                                    false
                                } else {
                                    match last_forced_idr_at {
                                        None => {
                                            last_forced_idr_at = Some(now_idr);
                                            false
                                        }
                                        Some(t) => {
                                            now_idr.duration_since(t)
                                                >= Duration::from_secs(periodic_idr_secs)
                                        }
                                    }
                                };
                                if need_periodic_idr {
                                    last_forced_idr_at = Some(now_idr);
                                    eprintln!(
                                        "[voice][screen] periodic IDR (GIR recovery, every {}s)",
                                        periodic_idr_secs
                                    );
                                }
                                let key_frame =
                                    frame_count < 3
                                        || need_periodic_idr
                                        || keyframe_request_enc.load(Ordering::Relaxed);
                                let encode_start = std::time::Instant::now();
                                if !startup_keyframe_done {
                                    eprintln!(
                                        "[voice][screen] encoded backend startup: calling encode() key_frame={} ts_us={}",
                                        key_frame,
                                        ts_us
                                    );
                                }
                                let mft_res = mft.encode(nv12_tex, ts_us as i64, key_frame);
                                let us = encode_start.elapsed().as_micros() as u64;
                                if !startup_keyframe_done {
                                    match &mft_res {
                                        Ok(frames) => eprintln!(
                                            "[voice][screen] encoded backend startup: encode() returned {} frame(s) in {} us",
                                            frames.len(),
                                            us
                                        ),
                                        Err(err) => eprintln!(
                                            "[voice][screen] encoded backend startup: encode() failed after {} us: {:?}",
                                            us,
                                            err
                                        ),
                                    }
                                }
                                telemetry_enc.set_encode(us);
                                (mft_res, us, 0i64)
                            };

                            match mft_result {
                                Ok(frames) => {
                                    push_trace_stats.encode_ok =
                                        push_trace_stats.encode_ok.saturating_add(1);
                                    push_trace_stats.encode_us_total =
                                        push_trace_stats.encode_us_total.saturating_add(encode_us);
                                    mft_transient_timeouts = 0;
                                    mft_hard_failed = false;
                                    mft_retry_at = None;
                                    have_last_frame = true;
                                    let n = frames.len() as i64 + pipelined_collected;
                                    if n == 0 {
                                        push_trace_stats.empty_output_ticks =
                                            push_trace_stats.empty_output_ticks.saturating_add(1);
                                    }
                                    if trace_this_tick {
                                        eprintln!(
                                            "[voice][screen][push][tick {}] encode ok direct_frames={} pipelined_frames={} total_frames={} encode_us={}",
                                            push_trace_tick_idx,
                                            frames.len(),
                                            pipelined_collected,
                                            n,
                                            encode_us,
                                        );
                                    }
                                    if n == 0 && !startup_keyframe_done {
                                        eprintln!("[voice][screen] encoded backend startup: input accepted, still waiting for first encoded frame");
                                    }
                                    let direct_frame_count = frames.len() as u64;
                                    let send_start = std::time::Instant::now();
                                    let mut sent_keyframe = false;
                                    for ef in frames.into_iter() {
                                        sent_keyframe |= ef.key_frame;
                                        if !startup_keyframe_done {
                                            eprintln!(
                                                "[voice][screen] encoded backend startup: pushing first encoded frame bytes={} key={}",
                                                ef.data.len(),
                                                ef.key_frame
                                            );
                                        }
                                        enc_src.push_frame(&ef.data, rtp_timestamp, capture_us, ef.key_frame);
                                        if telemetry_enabled {
                                            let now_us = enc_session_start.elapsed().as_micros() as i64;
                                            println!("SEND: rtp={} capture={} delta={} now={}", rtp_timestamp, capture_us, capture_us - last_send_capture_us, now_us);
                                            last_send_capture_us = capture_us;
                                        }
                                        rtp_timestamp = rtp_timestamp.wrapping_add(current_rtp_step);
                                    }
                                    if sent_keyframe {
                                        keyframe_request_enc.store(false, Ordering::Relaxed);
                                    }
                                    let send_us = send_start.elapsed().as_micros() as u64;
                                    if !use_pipelined || direct_frame_count > 0 {
                                        push_trace_stats.send_calls =
                                            push_trace_stats.send_calls.saturating_add(1);
                                        push_trace_stats.sent_frames = push_trace_stats
                                            .sent_frames
                                            .saturating_add(direct_frame_count);
                                        push_trace_stats.send_us_total = push_trace_stats
                                            .send_us_total
                                            .saturating_add(send_us);
                                    }
                                    if trace_this_tick {
                                        eprintln!(
                                            "[voice][screen][push][tick {}] send direct_frames={} us={}",
                                            push_trace_tick_idx,
                                            direct_frame_count,
                                            send_us,
                                        );
                                    }
                                    telemetry_enc.set_send(send_us);
                                    // Phase 4.5: adaptive FPS — downgrade when encode > 80% budget for 1 sec
                                    let encode_ms = encode_us as f64 / 1000.0;
                                    encode_avg_ms = if encode_avg_ms == 0.0 { encode_ms } else { encode_avg_ms * 0.9 + encode_ms * 0.1 };
                                    let budget_ms = 1000.0 / current_fps;
                                    if !lock_preset_fps && past_startup && current_fps > 30.0 {
                                        if encode_avg_ms > budget_ms * 0.8 {
                                            downgrade_frames += 1;
                                            upgrade_frames = 0;
                                            if downgrade_frames >= current_fps as i64 {
                                                let old_fps = current_fps;
                                                current_fps = next_lower_fps(current_fps);
                                                current_frame_interval_ns = (1_000_000_000.0 / current_fps) as u64;
                                                current_full_interval = Duration::from_nanos(current_frame_interval_ns);
                                                current_effective_interval = if current_full_interval.as_nanos() as u64 > MAX_STATIC_INTERVAL_NS {
                                                    Duration::from_nanos(MAX_STATIC_INTERVAL_NS)
                                                } else {
                                                    current_full_interval
                                                };
                                                current_rtp_step = 90_000_u32 / current_fps as u32;
                                                downgrade_frames = 0;
                                                eprintln!("[voice][screen] adaptive: fps {:.0} → {:.0} (encode_avg={:.1}ms > budget={:.1}ms)",
                                                    old_fps, current_fps, encode_avg_ms, budget_ms);
                                            }
                                        } else if encode_avg_ms < budget_ms * 0.5 {
                                            upgrade_frames += 1;
                                            downgrade_frames = 0;
                                            if upgrade_frames >= (current_fps * 5.0) as i64 {
                                                let old_fps = current_fps;
                                                let maybe = next_higher_fps(current_fps);
                                                if maybe <= current_source_cap_fps && maybe != current_fps {
                                                    current_fps = maybe;
                                                    current_frame_interval_ns = (1_000_000_000.0 / current_fps) as u64;
                                                    current_full_interval = Duration::from_nanos(current_frame_interval_ns);
                                                    current_effective_interval = if current_full_interval.as_nanos() as u64 > MAX_STATIC_INTERVAL_NS {
                                                        Duration::from_nanos(MAX_STATIC_INTERVAL_NS)
                                                    } else {
                                                        current_full_interval
                                                    };
                                                    current_rtp_step = 90_000_u32 / current_fps as u32;
                                                    upgrade_frames = 0;
                                                    eprintln!("[voice][screen] adaptive: fps {:.0} → {:.0} (encode_avg={:.1}ms < budget={:.1}ms)",
                                                        old_fps, current_fps, encode_avg_ms, budget_ms);
                                                } else {
                                                    upgrade_frames = (current_fps * 5.0) as i64 - 1;
                                                }
                                            }
                                        } else {
                                            downgrade_frames = 0;
                                            upgrade_frames = 0;
                                        }
                                    }
                                    let frames_this_round = if frame_count < 3 && n == 0 { 1i64 } else { n };
                                    if frame_count == 0 && !startup_keyframe_done {
                                        startup_keyframe_done = true;
                                        eprintln!("[voice][screen] encoded backend startup keyframe: first 3 frames as IDR");
                                    }
                                    if let Some(mut st) = stats_enc.try_lock() {
                                        let enc = mft_encoder.as_ref().unwrap();
                                        st.encoding_path = Some(match enc.backend_kind() {
                                            EncodedBackendKind::NvencD3d11 => {
                                                EncodingPath::NvencD3d11 {
                                                    adapter: enc.encoder_name().into_owned(),
                                                }
                                                .to_display_string()
                                            }
                                            EncodedBackendKind::MftHardware => {
                                                EncodingPath::MftHardware {
                                                    adapter: enc.encoder_name().into_owned(),
                                                }
                                                .to_display_string()
                                            }
                                            EncodedBackendKind::MftSoftware => {
                                                EncodingPath::MftSoftware.to_display_string()
                                            }
                                        });
                                        st.encoder_threads = None; // MFT doesn't use CPU encoder threads
                                    }
                                    frame_count += frames_this_round.max(1);
                                    fps_frame_count += n.max(0);
                                    running_ts_us += frame_interval_us as i64 * frames_this_round.max(1);
                                    let elapsed = fps_window_start.elapsed();
                                    if elapsed >= Duration::from_secs(1) {
                                        let actual_fps = fps_frame_count as f32 / elapsed.as_secs_f32();
                                        if let Some(mut st) = stats_enc.try_lock() {
                                            st.stream_fps = Some(actual_fps);
                                            st.frames_per_second = Some(actual_fps);
                                        }
                                        if telemetry_enabled {
                                            eprintln!(
                                                "[voice][screen] push_frame rate: {:.1} fps (target {:.0}, schedule {:.0}, mode={})",
                                                actual_fps,
                                                max_fps,
                                                current_fps,
                                                if use_pipelined { "pipelined" } else { "blocking" }
                                            );
                                        }
                                        telemetry_enc.print("sender");
                                        fps_window_start = std::time::Instant::now();
                                        fps_frame_count = 0;
                                    }
                                    let mft_interval = if frame_count < warmup_frames { warmup_interval } else { current_effective_interval };
                                    next_frame_at += mft_interval;
                                    while next_frame_at <= std::time::Instant::now() {
                                        next_frame_at += mft_interval;
                                    }
                                    let remaining = next_frame_at - std::time::Instant::now();
                                    if remaining > Duration::from_millis(PRE_BUFFER_MS) {
                                        std::thread::sleep(remaining - Duration::from_millis(PRE_BUFFER_MS));
                                    }
                                    while std::time::Instant::now() < next_frame_at {
                                        std::hint::spin_loop();
                                    }
                                }
                                Err(e) => {
                                    push_trace_stats.encode_fail =
                                        push_trace_stats.encode_fail.saturating_add(1);
                                    push_trace_stats.encode_us_total =
                                        push_trace_stats.encode_us_total.saturating_add(encode_us);
                                    if trace_this_tick {
                                        eprintln!(
                                            "[voice][screen][push][tick {}] encode failed after {} us: {:?}",
                                            push_trace_tick_idx,
                                            encode_us,
                                            e,
                                        );
                                    }
                                    let err_text = format!("{:?}", e);
                                    let transient_async_mft = err_text.contains("timeout waiting for METransformNeedInput")
                                        || err_text.contains("timeout waiting for METransformHaveOutput")
                                        || err_text.contains("still not accepting after draining output");
                                    let transient_nvenc_backpressure = matches!(
                                        &e,
                                        EncodedH264EncoderError::Nvenc(NvencD3d11Error::QueueFull { .. })
                                    ) || err_text.contains("queue stayed full")
                                        || err_text.contains("queue is full");
                                    if transient_async_mft || transient_nvenc_backpressure {
                                        mft_transient_timeouts = mft_transient_timeouts.saturating_add(1);
                                        if mft_transient_timeouts <= 3 || mft_transient_timeouts % 30 == 0 {
                                            if transient_nvenc_backpressure {
                                                eprintln!(
                                                    "[voice][screen] NVENC backpressure/stall (transient {}/120), dropping frame: {}",
                                                    mft_transient_timeouts,
                                                    err_text
                                                );
                                            } else {
                                                eprintln!(
                                                    "[voice][screen] MFT async state timeout/stall (transient {}/120), dropping frame: {}",
                                                    mft_transient_timeouts,
                                                    err_text
                                                );
                                            }
                                        }
                                        if mft_transient_timeouts >= 120 {
                                            if transient_nvenc_backpressure {
                                                eprintln!(
                                                    "[voice][screen] NVENC backpressure persisted for {} frames, falling back",
                                                    mft_transient_timeouts
                                                );
                                            } else if prefer_mft_no_fallback {
                                                eprintln!(
                                                    "[voice][screen] DXGI+MFT: persistent async MFT stall after {} frames, resetting encoder path",
                                                    mft_transient_timeouts
                                                );
                                                mft_retry_at = Some(std::time::Instant::now() + Duration::from_millis(250));
                                            } else {
                                                eprintln!(
                                                    "[voice][screen] MFT async stall persisted for {} frames, falling back",
                                                    mft_transient_timeouts
                                                );
                                            }
                                            mft_path_failed = true;
                                        } else {
                                            let mft_interval = if frame_count < warmup_frames {
                                                warmup_interval
                                            } else {
                                                current_effective_interval
                                            };
                                            next_frame_at += mft_interval;
                                            while next_frame_at <= std::time::Instant::now() {
                                                next_frame_at += mft_interval;
                                            }
                                            let remaining = next_frame_at - std::time::Instant::now();
                                            if remaining > Duration::from_millis(PRE_BUFFER_MS) {
                                                std::thread::sleep(remaining - Duration::from_millis(PRE_BUFFER_MS));
                                            }
                                            while std::time::Instant::now() < next_frame_at {
                                                std::hint::spin_loop();
                                            }
                                            continue;
                                        }
                                    } else {
                                        eprintln!("[voice][screen] encoded backend encode failed: {:?}", e);
                                        mft_path_failed = true;
                                        mft_hard_failed = true;
                                    }
                                }
                            }
                            continue;
                        } else if prefer_mft_no_fallback && !mft_hard_failed {
                            let mft_interval = if frame_count < warmup_frames {
                                warmup_interval
                            } else {
                                current_effective_interval
                            };
                            next_frame_at += mft_interval;
                            while next_frame_at <= std::time::Instant::now() {
                                next_frame_at += mft_interval;
                            }
                            let remaining = next_frame_at - std::time::Instant::now();
                            if remaining > Duration::from_millis(PRE_BUFFER_MS) {
                                std::thread::sleep(remaining - Duration::from_millis(PRE_BUFFER_MS));
                            }
                            while std::time::Instant::now() < next_frame_at {
                                std::hint::spin_loop();
                            }
                            continue;
                        }
                    }

                    // Encoded-path fallback: hard failure -> send a Native (I420) track
                    // to async context
                    // so it can unpublish the Encoded track and republish a working Native one.
                    if mft_path_failed
                        && !mft_fallback_sent
                        && (!prefer_mft_no_fallback || mft_hard_failed)
                    {
                        // CRITICAL: drop the encoded source to unregister the global
                        // EncodedChannel BEFORE the async context publishes the fallback
                        // track. Without this, VideoEncoderFactory::Create still finds
                        // the active channel and creates ExternalH264Encoder (a no-op)
                        // instead of OpenH264, so no frames ever reach the viewer.
                        drop(encoded_source_enc.take());
                        if let Some(tx) = mft_fallback_tx.take() {
                            let _tokio_guard = tokio_handle.enter();
                            let native_src = NativeVideoSource::new(mft_fallback_resolution.clone(), true);
                            let fallback_track = LocalVideoTrack::create_video_track(
                                "screen",
                                RtcVideoSource::Native(native_src.clone()),
                            );
                            // Spawn a capture_frame thread for the fallback source.
                            let (fb_tx, fb_rx) = mpsc::sync_channel::<livekit::webrtc::video_frame::VideoFrame<livekit::webrtc::video_frame::I420Buffer>>(2);
                            std::thread::Builder::new()
                                .name("livekit-capture-frame-fb".into())
                                .spawn(move || {
                                    while let Ok(vf) = fb_rx.recv() {
                                        native_src.capture_frame(&vf);
                                    }
                                })
                                .ok();
                            // Store the fallback capture_tx so the CPU path below can use it.
                            // We replace capture_tx_opt with the fallback channel.
                            // SAFETY: capture_tx_opt is None here (MFT path), so we can set it.
                            let _ = capture_tx_opt.replace(fb_tx);
                            eprintln!("[voice][screen] encoded H.264 backend failed, switching to CPU/I420 fallback track");
                            let _ = tx.send(fallback_track);
                        }
                        mft_fallback_sent = true;
                    }

                    // Phase 6.3: recreate converter when capture resolution changes (needs pool lock).
                    let (device, context, texture, context_mutex, converter_ref) = {
                        let pool_guard = pool_ref_enc.lock();
                        let pool = match pool_guard.as_ref() {
                            Some(p) => p,
                            None => continue,
                        };
                        let needs_new = match &gpu_converter {
                            None => true,
                            Some(c) => c.src_dims() != (pool.width, pool.height),
                        };
                        if needs_new {
                                    let sw = pool.width;
                                    let sh = pool.height;
                            let new_conv = if sw == video_width && sh == video_height {
                                D3d11RgbaToI420::new(&pool.device, sw, sh)
                                    .map(GpuConverter::NoScale)
                            } else {
                                D3d11RgbaToI420Scaled::new(&pool.device, sw, sh, video_width, video_height)
                                    .map(GpuConverter::Scaled)
                            };
                            match new_conv {
                                Ok(conv) => {
                                    if gpu_converter.is_some() {
                                        eprintln!(
                                            "[voice][screen] GPU converter recreated: {}x{} → {}x{}",
                                            sw, sh, video_width, video_height
                                        );
                                    } else if mft_fallback_sent {
                                        eprintln!(
                                            "[voice][screen] MFT→CPU fallback: I420 GPU converter created {}x{} → {}x{}",
                                            sw, sh, video_width, video_height
                                        );
                                    }
                                    gpu_converter = Some(conv);
                                }
                                Err(e) => {
                                    eprintln!("[voice][screen] D3D11 I420 init failed, fallback to CPU: {:?}", e);
                                    gpu_path_failed = true;
                                    gpu_encode_active_enc.store(false, Ordering::Relaxed);
                                    if let Some(mut st) = stats_enc.try_lock() {
                                        let threads = encoder_threads_for_resolution(video_width, video_height);
                                        st.encoding_path = Some(EncodingPath::OpenH264 { threads, gpu_capture: true }.to_display_string());
                                        st.encoder_threads = Some(threads);
                                    }
                                    continue;
                                }
                            }
                        }
                        // Clone refs and release pool lock BEFORE convert. Holding the pool lock during
                        // convert blocked WGC from pushing new frames (static/frozen broadcast at non-native res).
                        // Thread-safety of the Immediate Context is now handled by context_mutex inside convert().
                        let device = pool.device.clone();
                        let context = pool.context.clone();
                        let texture = pool.textures[gpu_slot as usize].clone();
                        let context_mutex = Arc::clone(&pool.context_mutex);
                        let conv = gpu_converter.as_ref().unwrap();
                        (device, context, texture, context_mutex, conv)
                    };

                    let frame_delta_ms = prev_frame_time.map(|p| now.duration_since(p).as_millis()).unwrap_or(0);
                    prev_frame_time = Some(now);
                    let encode_start = std::time::Instant::now();
                    let gpu_timing: GpuConvertTiming = match converter_ref.convert(&device, &context, &texture, &context_mutex, &mut planes_buf) {
                        Ok(timing) => {
                            if !gpu_path_logged {
                                let path_label = match converter_ref {
                                    GpuConverter::NoScale(_) => "gpu (D3D11 compute, no scale)",
                                    GpuConverter::Scaled(_) => "gpu (D3D11 compute + bilinear downscale)",
                                };
                                eprintln!("[voice][screen] RGBA→I420 conversion: {}", path_label);
                                gpu_path_logged = true;
                                gpu_encode_active_enc.store(true, Ordering::Relaxed);
                            }
                            timing
                        }
                        Err(e) => {
                            eprintln!("[voice][screen] D3D11 convert failed, fallback to CPU: {:?}", e);
                            gpu_path_failed = true;
                            gpu_encode_active_enc.store(false, Ordering::Relaxed);
                            if let Some(mut st) = stats_enc.try_lock() {
                                let threads = encoder_threads_for_resolution(video_width, video_height);
                                st.encoding_path = Some(EncodingPath::OpenH264 { threads, gpu_capture: true }.to_display_string());
                                st.encoder_threads = Some(threads);
                            }
                            continue;
                        }
                    };

                    // Build I420Buffer from planes (Phase 4.2).
                    // D3d11RgbaToI420Scaled already outputs at preset resolution (Phase 6.1);
                    // D3d11RgbaToI420 (no-scale) also matches preset — no CPU scale needed.
                    have_last_frame = true;
                    use livekit::webrtc::video_frame::I420Buffer;
                    let mut i420 = I420Buffer::new(planes_buf.width, planes_buf.height);
                    let (y_dst, u_dst, v_dst) = i420.data_mut();
                    let _ = y_dst.get_mut(..planes_buf.y.len()).map(|s| s.copy_from_slice(&planes_buf.y));
                    let _ = u_dst.get_mut(..planes_buf.u.len()).map(|s| s.copy_from_slice(&planes_buf.u));
                    let _ = v_dst.get_mut(..planes_buf.v.len()).map(|s| s.copy_from_slice(&planes_buf.v));
                    let vf = VideoFrame {
                        rotation: VideoRotation::VideoRotation0,
                        timestamp_us: ts_us as i64,
                        color_space: None,
                        buffer: i420,
                    };
                    // On the very first frame: send it 3× so the IDR reaches viewers
                    // even under initial packet loss. Do not send the same frame again with
                    // timestamp 0 (would duplicate timestamp and can freeze decoder).
                    let mut frames_this_round = 1i64;
                    if !startup_keyframe_done && frame_count == 0 {
                        let frame_interval_us = 1_000_000_u64 / max_fps as u64;
                        for repeat in 0..3u64 {
                            use livekit::webrtc::video_frame::I420Buffer;
                            let mut i420r = I420Buffer::new(planes_buf.width, planes_buf.height);
                            let (yd, ud, vd) = i420r.data_mut();
                            let _ = yd.get_mut(..planes_buf.y.len()).map(|s| s.copy_from_slice(&planes_buf.y));
                            let _ = ud.get_mut(..planes_buf.u.len()).map(|s| s.copy_from_slice(&planes_buf.u));
                            let _ = vd.get_mut(..planes_buf.v.len()).map(|s| s.copy_from_slice(&planes_buf.v));
                            let vfr = VideoFrame {
                                rotation: VideoRotation::VideoRotation0,
                                timestamp_us: repeat.saturating_mul(frame_interval_us) as i64,
                                color_space: None,
                                buffer: i420r,
                            };
                            if let Some(ref tx) = capture_tx_opt { let _ = tx.try_send(vfr); }
                        }
                        startup_keyframe_done = true;
                        frames_this_round = 3;
                        eprintln!("[voice][screen] startup keyframe: sent 3× IDR copies");
                    } else {
                        if let Some(ref tx) = capture_tx_opt { let _ = tx.try_send(vf); }
                    }
                    prev_send_time = Some(std::time::Instant::now());
                    if let Some(mut st) = stats_enc.try_lock() {
                        let threads = encoder_threads_for_resolution(video_width, video_height);
                        st.encoding_path = Some(EncodingPath::OpenH264 { threads, gpu_capture: false }.to_display_string());
                        st.encoder_threads = Some(threads);
                    }
                    if !gpu_i420_encoder_logged {
                        telemetry_enc.set_encoder_type("GPU I420 (LiveKit OpenH264)");
                        gpu_i420_encoder_logged = true;
                    }
                    frame_count += frames_this_round;
                    fps_frame_count += frames_this_round;
                    running_ts_us += frame_interval_us as i64 * frames_this_round;

                    // Phase 6.2: accumulate GPU timing metrics.
                    instr_gpu_dispatch_ns += gpu_timing.dispatch_ns;
                    instr_gpu_copy_ns     += gpu_timing.copy_ns;
                    instr_gpu_map_ns      += gpu_timing.map_ns;
                    instr_gpu_total_ns    += gpu_timing.total_ns;

                    let elapsed = fps_window_start.elapsed();
                    if elapsed >= Duration::from_secs(1) {
                        let actual_fps = fps_frame_count as f32 / elapsed.as_secs_f32();
                        if let Some(mut st) = stats_enc.try_lock() {
                            st.stream_fps = Some(actual_fps);
                            st.frames_per_second = Some(actual_fps);
                        }
                        eprintln!("[voice][screen] capture_frame rate: {:.1} fps (target {})", actual_fps, max_fps);
                        telemetry_enc.print("sender");
                        fps_window_start = std::time::Instant::now();
                        fps_frame_count = 0;
                    }

                    // Phase 6.2: log GPU timing every INSTR_WINDOW frames.
                    if frame_count > 0 && frame_count % INSTR_WINDOW == 0 {
                        let n = INSTR_WINDOW as u64;
                        let avg_dispatch = instr_gpu_dispatch_ns / n / 1000;
                        let avg_copy     = instr_gpu_copy_ns     / n / 1000;
                        let avg_map      = instr_gpu_map_ns      / n / 1000;
                        let avg_total    = instr_gpu_total_ns     / n / 1000;
                        let budget_us    = frame_budget_ns / 1000;
                        let pct = avg_total * 100 / budget_us.max(1);
                        let bottleneck = if pct > 60 { " ← BOTTLENECK" } else { "" };
                        eprintln!(
                            "[voice][screen] gpu perf @{} frames: dispatch={}µs copy={}µs map={}µs total={}µs / {}µs budget ({}%{})",
                            frame_count, avg_dispatch, avg_copy, avg_map, avg_total, budget_us, pct, bottleneck
                        );
                        instr_gpu_dispatch_ns = 0;
                        instr_gpu_copy_ns     = 0;
                        instr_gpu_map_ns      = 0;
                        instr_gpu_total_ns    = 0;
                    }

                    let interval = if frame_count < warmup_frames { warmup_interval } else { current_effective_interval };
                    // Advance clock by N intervals when we sent N frames (e.g. 3× keyframe).
                    for _ in 1..frames_this_round {
                        next_frame_at += interval;
                    }
                    next_frame_at += interval;
                    while next_frame_at <= std::time::Instant::now() {
                        next_frame_at += interval;
                    }
                    let remaining = next_frame_at - std::time::Instant::now();
                    if remaining > Duration::from_millis(PRE_BUFFER_MS) {
                        std::thread::sleep(remaining - Duration::from_millis(PRE_BUFFER_MS));
                    }
                    while std::time::Instant::now() < next_frame_at {
                        std::hint::spin_loop();
                    }
                    continue;
                }

                // CPU path: pop RawFrame from ring.
                let raw_ptr = match ring_enc.pop() {
                    Some(p) => p,
                    None => {
                        // Constant FPS: re-send last frame when WGC has no new content (static screen).
                        if have_last_frame
                            && frame_count >= warmup_frames
                            && planes_buf.width > 0
                            && planes_buf.height > 0
                        {
                            let ts_us = (frame_count as u64).saturating_mul(1_000_000) / max_fps as u64;
                            use livekit::webrtc::video_frame::I420Buffer;
                            let mut i420 = I420Buffer::new(planes_buf.width, planes_buf.height);
                            let (yd, ud, vd) = i420.data_mut();
                            let _ = yd.get_mut(..planes_buf.y.len()).map(|s| s.copy_from_slice(&planes_buf.y));
                            let _ = ud.get_mut(..planes_buf.u.len()).map(|s| s.copy_from_slice(&planes_buf.u));
                            let _ = vd.get_mut(..planes_buf.v.len()).map(|s| s.copy_from_slice(&planes_buf.v));
                            let vf = VideoFrame {
                                rotation: VideoRotation::VideoRotation0,
                                timestamp_us: ts_us as i64,
                                color_space: None,
                                buffer: i420,
                            };
                            if let Some(ref tx) = capture_tx_opt { let _ = tx.try_send(vf); }
                            frame_count += 1;
                            fps_frame_count += 1;
                            running_ts_us += frame_interval_us as i64;
                            let elapsed = fps_window_start.elapsed();
                            if elapsed >= Duration::from_secs(1) {
                                let actual_fps = fps_frame_count as f32 / elapsed.as_secs_f32();
                                if let Some(mut st) = stats_enc.try_lock() {
                                    st.stream_fps = Some(actual_fps);
                                    st.frames_per_second = Some(actual_fps);
                                }
                                fps_window_start = std::time::Instant::now();
                                fps_frame_count = 0;
                            }
                        }
                        let now = std::time::Instant::now();
                        if next_frame_at > now {
                            let rem = next_frame_at - now;
                            if rem > Duration::from_millis(PRE_BUFFER_MS) {
                                std::thread::sleep(rem - Duration::from_millis(PRE_BUFFER_MS));
                            }
                            while std::time::Instant::now() < next_frame_at {
                                std::hint::spin_loop();
                            }
                        }
                        next_frame_at += if frame_count < warmup_frames { warmup_interval } else { current_effective_interval };
                        continue;
                    }
                };
                let frame = unsafe { Box::from_raw(raw_ptr) };
                // Use same threshold as GPU path: drop only when more than 2 intervals late.
                // At 60 fps one interval is ~16.7 ms; CPU encode often takes ~15–20 ms, so
                // "late by more than one" would drop every other frame and cap effective FPS at 30.
                let late_by_more_than_two_cpu = now > next_frame_at + interval + interval;
                if past_startup && late_by_more_than_two_cpu {
                    let mut dropped = 0u64;
                    while next_frame_at <= now {
                        next_frame_at += interval;
                        dropped += 1;
                    }
                    if dropped > 0 {
                        telemetry_enc.add_frames_dropped(dropped);
                    }
                    let remaining = next_frame_at - std::time::Instant::now();
                    if remaining > Duration::from_millis(PRE_BUFFER_MS) {
                        std::thread::sleep(remaining - Duration::from_millis(PRE_BUFFER_MS));
                    }
                    while std::time::Instant::now() < next_frame_at {
                        std::hint::spin_loop();
                    }
                    continue;
                }

                let frame_delta_ms = prev_frame_time.map(|p| now.duration_since(p).as_millis()).unwrap_or(0);
                prev_frame_time = Some(now);
                let encode_start = std::time::Instant::now();
                let output = match encoder.encode_frame(
                    &frame,
                    video_width,
                    video_height,
                    ts_us as i64,
                    cpu_returned_buffer.take(),
                ) {
                    Ok(out) => out,
                    Err(e) => {
                        eprintln!("[voice][screen] encode error: {:?}", e);
                        continue;
                    }
                };

                if let EncoderOutput::RawI420 { frame: mut vf, timing } = output {
                    // Save I420 planes for constant-FPS re-send on static content.
                    {
                        let (ys, us, vs) = vf.buffer.data();
                        planes_buf.ensure_size(video_width, video_height);
                        planes_buf.y[..ys.len()].copy_from_slice(ys);
                        planes_buf.u[..us.len()].copy_from_slice(us);
                        planes_buf.v[..vs.len()].copy_from_slice(vs);
                        have_last_frame = true;
                    }
                    // On the very first frame: send it 3× so the IDR reaches viewers; do not
                    // send the same frame again with timestamp 0 (duplicate timestamp can freeze decoder).
                    let mut frames_this_round = 1i64;
                    if !startup_keyframe_done && frame_count == 0 {
                        let frame_interval_us = 1_000_000_u64 / max_fps as u64;
                        use livekit::webrtc::video_frame::I420Buffer;
                        let (ys, us, vs) = vf.buffer.data();
                        for repeat in 0..3u64 {
                            let mut i420r = I420Buffer::new(video_width, video_height);
                            let (yd, ud, vd) = i420r.data_mut();
                            let _ = yd.get_mut(..ys.len()).map(|s| s.copy_from_slice(ys));
                            let _ = ud.get_mut(..us.len()).map(|s| s.copy_from_slice(us));
                            let _ = vd.get_mut(..vs.len()).map(|s| s.copy_from_slice(vs));
                            let vfr = VideoFrame {
                                rotation: VideoRotation::VideoRotation0,
                                timestamp_us: repeat.saturating_mul(frame_interval_us) as i64,
                                color_space: None,
                                buffer: i420r,
                            };
                            if let Some(ref tx) = capture_tx_opt { let _ = tx.try_send(vfr); }
                        }
                        startup_keyframe_done = true;
                        frames_this_round = 3;
                        extra_intervals = 2;
                        eprintln!("[voice][screen] startup keyframe: sent 3× IDR copies (CPU path)");
                    } else {
                        // Clone buffer for send so we can reuse encoder_buf (capture_frame thread takes ownership).
                        let (ys, us, vs) = vf.buffer.data();
                        let mut i420_send = livekit::webrtc::video_frame::I420Buffer::new(video_width, video_height);
                        let (yd, ud, vd) = i420_send.data_mut();
                        let _ = yd.get_mut(..ys.len()).map(|s| s.copy_from_slice(ys));
                        let _ = ud.get_mut(..us.len()).map(|s| s.copy_from_slice(us));
                        let _ = vd.get_mut(..vs.len()).map(|s| s.copy_from_slice(vs));
                        let vf_send = VideoFrame {
                            rotation: vf.rotation,
                            timestamp_us: vf.timestamp_us,
                            color_space: None,
                            buffer: i420_send,
                        };
                        if let Some(ref tx) = capture_tx_opt { let _ = tx.try_send(vf_send); }
                    }
                    prev_send_time = Some(std::time::Instant::now());
                    // Reuse buffers: encoder's buffer → cpu_returned_buffer, other → cpu_other_buffer.
                    // Previous 8-line dance left cpu_returned_buffer = None after 3× keyframe; with 2
                    // slots and 2 buffers we need one placeholder in vf (one alloc per frame when reusing).
                    let other = cpu_other_buffer.take().unwrap_or_else(|| {
                        livekit::webrtc::video_frame::I420Buffer::new(video_width, video_height)
                    });
                    let encoder_buf = std::mem::replace(&mut vf.buffer, other);
                    cpu_returned_buffer = Some(encoder_buf);
                    let placeholder = livekit::webrtc::video_frame::I420Buffer::new(video_width, video_height);
                    cpu_other_buffer = Some(std::mem::replace(&mut vf.buffer, placeholder));
                    if let Some(mut st) = stats_enc.try_lock() {
                        st.encoding_path = Some("CPU".into());
                        st.encoder_threads = Some(encoder_threads_for_resolution(video_width, video_height));
                    }
                    if !cpu_path_encoder_logged {
                        telemetry_enc.set_encoder_type("CPU I420 (LiveKit OpenH264)");
                        cpu_path_encoder_logged = true;
                    }
                    frame_count += frames_this_round;
                    fps_frame_count += frames_this_round;
                    running_ts_us += frame_interval_us as i64 * frames_this_round;
                    instr_scale_ns += timing.scale_ns;
                    instr_convert_ns += timing.convert_ns;
                    instr_total_ns += timing.total_ns;
                    let elapsed = fps_window_start.elapsed();
                    if elapsed >= Duration::from_secs(1) {
                        let actual_fps = fps_frame_count as f32 / elapsed.as_secs_f32();
                        if let Some(mut st) = stats_enc.try_lock() {
                            st.stream_fps = Some(actual_fps);
                            st.frames_per_second = Some(actual_fps);
                        }
                        eprintln!("[voice][screen] capture_frame rate (CPU): {:.1} fps (target {})", actual_fps, max_fps);
                        telemetry_enc.print("sender");
                        fps_window_start = std::time::Instant::now();
                        fps_frame_count = 0;
                    }
                }

                if frame_count > 0 && frame_count % INSTR_WINDOW == 0 {
                    let avg_scale_us   = instr_scale_ns   / INSTR_WINDOW as u64 / 1000;
                    let avg_convert_us = instr_convert_ns / INSTR_WINDOW as u64 / 1000;
                    let avg_total_us   = instr_total_ns   / INSTR_WINDOW as u64 / 1000;
                    let budget_us      = frame_budget_ns / 1000;
                    let pct = avg_total_us * 100 / budget_us.max(1);
                    let bottleneck = if pct > 60 { " ← BOTTLENECK" } else { "" };
                    let scale_slow = if avg_scale_us > 8000 {
                        " (scale>8ms: check GPU/capture load)"
                    } else {
                        ""
                    };
                    eprintln!(
                        "[voice][screen] perf @{} frames: scale={}µs conv={}µs total={}µs / {}µs budget ({}%{}){}",
                        frame_count, avg_scale_us, avg_convert_us, avg_total_us, budget_us, pct, bottleneck, scale_slow
                    );
                    instr_scale_ns   = 0;
                    instr_convert_ns = 0;
                    instr_total_ns   = 0;
                }

                let interval = if frame_count < warmup_frames { warmup_interval } else { current_effective_interval };
                for _ in 0..extra_intervals {
                    next_frame_at += interval;
                }
                extra_intervals = 0;
                next_frame_at += interval;
                while next_frame_at <= std::time::Instant::now() {
                    next_frame_at += interval;
                }
                let remaining = next_frame_at - std::time::Instant::now();
                if remaining > Duration::from_millis(PRE_BUFFER_MS) {
                    std::thread::sleep(remaining - Duration::from_millis(PRE_BUFFER_MS));
                }
                while std::time::Instant::now() < next_frame_at {
                    std::hint::spin_loop();
                }
            }

            // Phase 5.3: drain rings on stop so no frames/slots are leaked.
            ring_enc.drain_drop();

            // Signal capture_frame thread to exit and wait for it (I420 path only).
            if let Some(tx) = capture_tx_opt { drop(tx); }
            if let Some(handle) = capture_handle {
                let _ = handle.join();
            }

            unsafe {
                #[link(name = "winmm")]
                extern "system" { fn timeEndPeriod(uPeriod: u32) -> u32; }
                timeEndPeriod(1);
            }
            eprintln!("[voice][screen] encoder thread stopped");
        })
        .ok();

    (Some(track), fallback_rx_opt)
}

/// Fallback for macOS/Linux: xcap polling loop.
#[cfg(not(all(target_os = "windows", feature = "wgc-capture")))]
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
fn start_screen_capture(
    source_target: StreamSourceTarget,
    preset: ScreenPreset,
    stop_flag: Arc<AtomicBool>,
    _keyframe_requested: Arc<AtomicBool>,
    session_stats: Arc<Mutex<VoiceSessionStats>>,
) -> (
    Option<LocalVideoTrack>,
    Option<tokio::sync::oneshot::Receiver<LocalVideoTrack>>,
) {
    let (video_width, video_height, max_fps, bitrate) = preset.params();
    {
        let mut st = session_stats.lock();
        st.resolution = Some((video_width, video_height));
        st.stream_fps = Some(max_fps as f32);
        st.frames_per_second = Some(max_fps as f32);
        st.connection_speed_mbps = Some(bitrate as f32 / 1_000_000.0);
        st.encoding_path = Some(
            EncodingPath::OpenH264 {
                threads: encoder_threads_for_resolution(video_width, video_height),
                gpu_capture: false,
            }
            .to_display_string(),
        );
        st.encoder_threads = Some(encoder_threads_for_resolution(video_width, video_height));
    }
    let frame_interval_ms = (1000.0 / max_fps).round() as u64;
    let resolution = VideoResolution {
        width: video_width,
        height: video_height,
    };
    let source = NativeVideoSource::new(resolution, true);
    let track =
        LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(source.clone()));

    if let StreamSourceTarget::Window {
        window_id,
        process_id,
    } = source_target.clone()
    {
        let stop_for_thread = Arc::clone(&stop_flag);
        std::thread::Builder::new()
            .name("livekit-window".into())
            .spawn(move || {
                let Some(window) = find_stream_window(window_id, process_id) else {
                    eprintln!(
                        "[voice][screen] xcap window not found: hwnd={} pid={}",
                        window_id, process_id
                    );
                    return;
                };
                eprintln!(
                    "[voice][screen] xcap capturing window '{}' [{}] ({}x{})",
                    window.title(),
                    window.app_name(),
                    window.width(),
                    window.height()
                );

                let mut frame_count: i64 = 0;
                let mut i420_buffers = XcapI420Buffers::new(video_width, video_height);
                while !stop_for_thread.load(Ordering::Relaxed) {
                    let t0 = std::time::Instant::now();
                    match window.capture_image() {
                        Ok(img) => {
                            let src_w = img.width();
                            let src_h = img.height();
                            let raw = img.as_raw();
                            if let Some(vf) = i420_buffers.rgba_to_i420_scaled_reuse(
                                raw,
                                src_w,
                                src_h,
                                video_width,
                                video_height,
                                frame_count,
                            ) {
                                source.capture_frame(&vf);
                                frame_count += 1;
                            }
                        }
                        Err(e) => eprintln!("[voice][screen] window capture error: {}", e),
                    }
                    let elapsed_ms = t0.elapsed().as_millis() as u64;
                    let to_sleep = frame_interval_ms.saturating_sub(elapsed_ms);
                    if to_sleep > 0 {
                        std::thread::sleep(Duration::from_millis(to_sleep));
                    }
                }
                eprintln!("[voice][screen] capture thread stopped");
            })
            .ok();

        return (Some(track), None);
    }

    let screen_index = match source_target {
        StreamSourceTarget::Monitor { index } => Some(index),
        StreamSourceTarget::Window { .. } => None,
    };

    let stop_for_thread = Arc::clone(&stop_flag);
    std::thread::Builder::new()
        .name("livekit-screen".into())
        .spawn(move || {
            let monitors = enumerate_unique_screens();
            if monitors.is_empty() {
                eprintln!("[voice][screen] no monitors found");
                return;
            }
            let idx = screen_index.unwrap_or(0).min(monitors.len() - 1);
            let monitor = monitors.into_iter().nth(idx).unwrap();
            eprintln!(
                "[voice][screen] xcap capturing monitor {} ({}×{})",
                idx,
                monitor.width(),
                monitor.height()
            );

            let mut frame_count: i64 = 0;
            let mut i420_buffers = XcapI420Buffers::new(video_width, video_height);
            while !stop_for_thread.load(Ordering::Relaxed) {
                let t0 = std::time::Instant::now();
                match monitor.capture_image() {
                    Ok(img) => {
                        let src_w = img.width();
                        let src_h = img.height();
                        let raw = img.as_raw();
                        if let Some(vf) = i420_buffers.rgba_to_i420_scaled_reuse(
                            raw,
                            src_w,
                            src_h,
                            video_width,
                            video_height,
                            frame_count,
                        ) {
                            source.capture_frame(&vf);
                            frame_count += 1;
                        }
                    }
                    Err(e) => eprintln!("[voice][screen] capture error: {}", e),
                }
                let elapsed_ms = t0.elapsed().as_millis() as u64;
                let to_sleep = frame_interval_ms.saturating_sub(elapsed_ms);
                if to_sleep > 0 {
                    std::thread::sleep(Duration::from_millis(to_sleep));
                }
            }
            eprintln!("[voice][screen] capture thread stopped");
        })
        .ok();

    (Some(track), None)
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn start_screen_capture(
    _source_target: StreamSourceTarget,
    _preset: ScreenPreset,
    _stop_flag: Arc<AtomicBool>,
    _keyframe_requested: Arc<AtomicBool>,
    _session_stats: Arc<Mutex<VoiceSessionStats>>,
) -> (
    Option<LocalVideoTrack>,
    Option<tokio::sync::oneshot::Receiver<LocalVideoTrack>>,
) {
    (None, None)
}

/// Convert xcap RGBA capture to a scaled I420 VideoFrame using libwebrtc/libyuv.
///
/// xcap 0.0.12 on Windows uses GDI BitBlt + GetDIBits, applies a B↔R byte swap internally,
/// and returns pixels as [R, G, B, A] in memory (standard `image::RgbaImage`).
/// libyuv `abgr_to_i420` expects exactly [R, G, B, A] in memory (libyuv calls this ABGR
/// because in a 32-bit LE integer A is high byte, R is next, etc.).
///
/// Downscaling from native resolution to VIDEO_WIDTH×VIDEO_HEIGHT is done via
/// `I420Buffer::scale()` (libyuv bilinear/box filter, SIMD-accelerated).
fn bgra_to_i420_scaled(
    rgba: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    frame_count: i64,
) -> Option<VideoFrame<livekit::webrtc::video_frame::I420Buffer>> {
    use livekit::webrtc::native::yuv_helper;
    use livekit::webrtc::video_frame::I420Buffer;

    if rgba.len() < (src_w * src_h * 4) as usize {
        return None;
    }

    // Step 1: RGBA → I420 at native resolution using libyuv (SIMD path).
    // xcap gives [R,G,B,A] = libyuv ABGR.
    let mut src_i420 = I420Buffer::new(src_w, src_h);
    {
        let (y_plane, u_plane, v_plane) = src_i420.data_mut();
        let (stride_y, stride_u, stride_v) = (src_w, (src_w + 1) / 2, (src_w + 1) / 2);
        yuv_helper::abgr_to_i420(
            rgba,
            src_w * 4,
            y_plane,
            stride_y,
            u_plane,
            stride_u,
            v_plane,
            stride_v,
            src_w as i32,
            src_h as i32,
        );
    }

    // Step 2: scale to target resolution using libwebrtc's built-in scaler.
    let scaled = if src_w == dst_w && src_h == dst_h {
        src_i420
    } else {
        src_i420.scale(dst_w as i32, dst_h as i32)
    };

    let ts = frame_count * 16_667; // ~60 fps in µs
    Some(VideoFrame {
        rotation: VideoRotation::VideoRotation0,
        timestamp_us: ts,
        color_space: None,
        buffer: scaled,
    })
}

/// Capture microphone into a shared ring buffer (consumed by the timer task every 10 ms).
///
/// FIX: replaces the old channel-based capture_mic_to_channel.  Polling stop flag instead
/// of thread::park() ensures the thread exits cleanly when the session ends.
fn capture_mic_to_ring(
    ring: Arc<Mutex<VecDeque<i16>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host.default_input_device().ok_or("no input device")?;
    let config = preferred_input_config_48k(&device)?;
    let channels = config.channels as usize;
    let input_rate = config.sample_rate.0;
    eprintln!(
        "[voice][livekit] mic capture: {} Hz, {} ch",
        input_rate, channels
    );

    let err_fn = |e| eprintln!("[voice][livekit] mic stream error: {}", e);
    // Cap the capture-side ring tightly so sender latency stays bounded even if DSP falls behind.
    let max_ring = ((input_rate / 100) as usize).saturating_mul(MIC_RING_CAPTURE_MAX_FRAMES);

    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut buf = ring.lock();
                for frame in data.chunks(channels) {
                    let mono = frame.iter().sum::<f32>() / channels as f32;
                    let s = (mono * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    buf.push_back(s);
                }
                // Drop oldest samples if consumer falls behind.
                while buf.len() > max_ring {
                    buf.pop_front();
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| e.to_string())?;

    stream.play().map_err(|e| e.to_string())?;

    // Keep thread alive until stop is signalled (stream dropped → capture stops).
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Prefer 48 kHz input config; fallback to default (may cause distortion).
fn preferred_input_config_48k(device: &cpal::Device) -> Result<cpal::StreamConfig, String> {
    use cpal::traits::DeviceTrait;
    use cpal::SampleRate;
    for range in device
        .supported_input_configs()
        .map_err(|e| e.to_string())?
    {
        if let Some(supported) = range.try_with_sample_rate(SampleRate(SAMPLE_RATE)) {
            return Ok(supported.config());
        }
    }
    eprintln!(
        "[voice][livekit] 48 kHz not supported for input, using default (audio may be distorted)"
    );
    device
        .default_input_config()
        .map_err(|e| e.to_string())
        .map(|c| c.config())
}

#[cfg(test)]
mod benches {
    use super::{bgra_to_i420_scaled, XcapI420Buffers};

    /// Compare speed: current path (RGBA→I420 full then scale) vs xcap I420 path (scale-first + reuse).
    /// Run with: cargo test bench_rgba_i420_comparison -- --nocapture
    /// For release timings: cargo test bench_rgba_i420_comparison --release -- --nocapture
    #[test]
    fn bench_rgba_i420_comparison() {
        const SRC_W: u32 = 1920;
        const SRC_H: u32 = 1080;
        const DST_W: u32 = 1280;
        const DST_H: u32 = 720;
        const ITER: u32 = 30;

        eprintln!("[bench] allocating RGBA buffer...");
        let rgba: Vec<u8> = (0..(SRC_W * SRC_H * 4) as usize)
            .map(|i| (i % 256) as u8)
            .collect();

        // Warmup: first touch of libwebrtc can be slow (DLL/init)
        eprintln!("[bench] warmup path 1...");
        let _ = bgra_to_i420_scaled(&rgba, SRC_W, SRC_H, DST_W, DST_H, 0);
        eprintln!("[bench] warmup path 2...");
        let mut buffers = XcapI420Buffers::new(DST_W, DST_H);
        let _ = buffers.rgba_to_i420_scaled_reuse(&rgba, SRC_W, SRC_H, DST_W, DST_H, 0);

        // Path 1: current — I420 full-res then I420::scale (two allocs when scaling)
        eprintln!("[bench] timing path 1 ({} iters)...", ITER);
        let t1 = std::time::Instant::now();
        for i in 0..ITER {
            let _ = bgra_to_i420_scaled(&rgba, SRC_W, SRC_H, DST_W, DST_H, i as i64);
        }
        let ns1 = t1.elapsed().as_nanos() / ITER as u128;

        // Path 2: xcap I420 — scale-first (box_scale_rgba) then abgr_to_i420, buffer reuse
        eprintln!("[bench] timing path 2 ({} iters)...", ITER);
        let t2 = std::time::Instant::now();
        for i in 0..ITER {
            let _ = buffers.rgba_to_i420_scaled_reuse(&rgba, SRC_W, SRC_H, DST_W, DST_H, i as i64);
        }
        let ns2 = t2.elapsed().as_nanos() / ITER as u128;

        eprintln!(
            "[bench] RGBA→I420 ({}×{} → {}×{}), {} iters:",
            SRC_W, SRC_H, DST_W, DST_H, ITER
        );
        eprintln!(
            "  current (I420 full then scale): {:>8} µs/frame",
            ns1 / 1000
        );
        eprintln!(
            "  xcap I420 (scale-first + reuse): {:>8} µs/frame",
            ns2 / 1000
        );
        if ns2 < ns1 {
            eprintln!(
                "  → xcap I420 path is {:.1}% faster",
                100.0 * (1.0 - ns2 as f64 / ns1 as f64)
            );
        } else {
            eprintln!(
                "  → current path is {:.1}% faster",
                100.0 * (1.0 - ns1 as f64 / ns2 as f64)
            );
        }
    }
}
