//! Phase 4: MFT H.264 Encoder (client/src/mft_encoder.rs)
//!
//! Zero-copy H.264 encoding via Media Foundation Transform.
//! Accepts NV12 D3D11 textures, outputs H.264 Annex B NAL units.
//!
//! All code is pure Rust through windows-rs. No C++.

#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::ptr;

use thiserror::Error;
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::{
    ICodecAPI, IMFActivate, IMFAttributes, IMFMediaEventGenerator, IMFMediaType,
    IMFSample, IMFTransform, IMFDXGIDeviceManager,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncCommonLowLatency,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateSample, MFStartup, MF_VERSION,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_REGISTER_TYPE_INFO,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFMediaType_Video, MFVideoInterlace_Progressive, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_TRANSFORM_ASYNC_UNLOCK, MFTEnumEx,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;


/// MFT_ENUM_HARDWARE_URL_Attribute — presence indicates hardware MFT.
const MFT_ENUM_HARDWARE_URL_Attribute: GUID = GUID::from_u128(0x2f66c0d6_0d75_4e3c_ae93_cf0d938d30a1);

/// MF_LOW_LATENCY / CODECAPI_AVLowLatencyMode — reduces encoder/decoder buffering for real-time streaming.
/// {9c27891a-ed7a-40e1-88e8-b22727a024ee}
const MF_LOW_LATENCY: GUID = GUID::from_u128(0x9c27891a_ed7a_40e1_88e8_b22727a024ee);

/// CODECAPI_AVEncMPVGOPSize — max frames between GOP headers. GOP=fps*2 → keyframe every 2 sec.
/// {95f31b26-95a4-41aa-9303-246a7fc6eef1}
const CODECAPI_AVEncMPVGOPSize: GUID = GUID::from_u128(0x95f31b26_95a4_41aa_9303_246a7fc6eef1);

/// CODECAPI_AVEncCommonRateControlMode — eAVEncCommonRateControlMode_LowDelayVBR = 4.
/// {1c0608e9-370c-4710-8a58-cb6181c42423}
const CODECAPI_AVEncCommonRateControlMode: GUID = GUID::from_u128(0x1c0608e9_370c_4710_8a58_cb6181c42423);

/// CODECAPI_AVEncCommonMeanBitRate — target bitrate in bps. Required for CBR/VBR rate control.
/// {f7222374-2144-4815-b550-a37f8e12ee52}
const CODECAPI_AVEncCommonMeanBitRate: GUID = GUID::from_u128(0xf7222374_2144_4815_b550_a37f8e12ee52);

/// CODECAPI_AVEncCommonQualityVsSpeed — 0–100, 100 = max speed. Phase 4.2.
/// {98332df8-03cd-476b-89fa-3f9e442dec9f}
const CODECAPI_AVEncCommonQualityVsSpeed: GUID = GUID::from_u128(0x98332df8_03cd_476b_89fa_3f9e442dec9f);

/// CODECAPI_AVEncH264CABACEnable — 0 = CAVLC (Baseline), 1 = CABAC (Main/High). Phase 4.2.
/// {ee6cad62-d305-4248-a50e-e1b255f7caf6}
const CODECAPI_AVEncH264CABACEnable: GUID = GUID::from_u128(0xee6cad62_d305_4248_a50e_e1b255f7caf6);

/// CODECAPI_AVEncNumWorkerThreads — CPU threads for software MFT. Phase 4.2.
/// {b0e5b3a0-7c50-4b44-85a2-c48bed9a9640}
const CODECAPI_AVEncNumWorkerThreads: GUID = GUID::from_u128(0xb0e5b3a0_7c50_4b44_85a2_c48bed9a9640);

/// CODECAPI_AVEncVideoIntraRefreshMode — GIR instead of IDR keyframes. Phase 4.7.
/// eAVEncVideoIntraRefreshMode_Continuous = 2. Reduces encode peaks; software MFT often E_NOTIMPL.
/// {dc2f837c-f78a-4b9d-a8d4-2e76a337c0f0}
const CODECAPI_AVEncVideoIntraRefreshMode: GUID = GUID::from_u128(0xdc2f837c_f78a_4b9d_a8d4_2e76a337c0f0);

/// CODECAPI_AVEncVideoForceKeyFrame — force next frame as IDR. Phase 4.7: periodic IDR for packet loss recovery.
/// {398c1b98-8353-475a-9ef2-8f265d260345}
const CODECAPI_AVEncVideoForceKeyFrame: GUID = GUID::from_u128(0x398c1b98_8353_475a_9ef2_8f265d260345);

/// CODECAPI_AVEncVideoEncodeSliceSizeControlMode — eAVEncSliceControlMode_Constrained (2) asks the encoder
/// to respect slice size limits where supported (hardware MFT may ignore). Smaller slices → smaller NAL units
/// → less bursty RTP after packetization.
/// {a79e89a8-a437-4ee2-98dd-ed95e39b446c}
const CODECAPI_AVEncVideoEncodeSliceSizeControlMode: GUID =
    GUID::from_u128(0xa79e89a8a4374ee298dded95e39b446c);

/// Raw MediaEventType values (avoids type ambiguity across windows-rs versions).
const ME_TRANSFORM_NEED_INPUT: u32 = 601;
const ME_TRANSFORM_HAVE_OUTPUT: u32 = 602;
const ASYNC_HAVE_OUTPUT_WAIT_MS: u32 = 8;

#[derive(Default, Clone, Copy)]
struct PolledEvents {
    found: bool,
    need_input: u32,
    have_output: u32,
}

#[derive(Default, Clone, Copy)]
struct AsyncWaitTrace {
    found: bool,
    buffered: bool,
    need_input: u32,
    have_output: u32,
    wait_us: u64,
    poll_loops: u32,
}

#[derive(Default, Clone, Copy)]
struct DrainPendingTrace {
    found_output: bool,
    queued_frame: bool,
    wait: AsyncWaitTrace,
    drain_us: u64,
    encode_us: u64,
}

#[derive(Default)]
struct SubmitDiagWindow {
    submits: u64,
    sample_us_total: u64,
    initial_ok: u64,
    initial_not_accepting: u64,
    initial_err: u64,
    initial_us_total: u64,
    need_wait_calls: u64,
    need_wait_found: u64,
    need_wait_buffered: u64,
    need_wait_us_total: u64,
    need_wait_loops_total: u64,
    need_wait_seen_need_total: u64,
    need_wait_seen_have_total: u64,
    after_need_ok: u64,
    after_need_not_accepting: u64,
    after_need_err: u64,
    after_need_us_total: u64,
    drain_calls: u64,
    drain_found_output: u64,
    drain_queued_frame: u64,
    drain_wait_us_total: u64,
    drain_output_us_total: u64,
    drain_encode_us_total: u64,
    retry_after_drain_ok: u64,
    retry_after_drain_not_accepting: u64,
    retry_after_drain_err: u64,
    retry_after_drain_us_total: u64,
    total_submit_us_total: u64,
    meta_queue_max: usize,
    pending_outputs_max: usize,
    need_input_buffered_max: u32,
    have_output_buffered_max: u32,
}

impl SubmitDiagWindow {
    fn avg_us(total: u64, count: u64) -> u64 {
        if count == 0 { 0 } else { total / count }
    }

    fn maybe_log(&mut self, window_start: &mut std::time::Instant, encoder_name: &str) {
        let elapsed = window_start.elapsed();
        if elapsed < std::time::Duration::from_secs(1) || self.submits == 0 {
            return;
        }
        let secs = elapsed.as_secs_f32().max(0.001);
        eprintln!(
            "[mft_encoder][submit][summary] encoder=\"{}\" rate={:.1}/s sample_avg={}us initial ok={} not_accepting={} err={} avg={}us wait_need call={} found={} buffered={} avg={}us loops_avg={:.1} seen_need_avg={:.1} seen_have_avg={:.1}",
            encoder_name,
            self.submits as f32 / secs,
            Self::avg_us(self.sample_us_total, self.submits),
            self.initial_ok,
            self.initial_not_accepting,
            self.initial_err,
            Self::avg_us(self.initial_us_total, self.submits),
            self.need_wait_calls,
            self.need_wait_found,
            self.need_wait_buffered,
            Self::avg_us(self.need_wait_us_total, self.need_wait_calls),
            if self.need_wait_calls == 0 {
                0.0
            } else {
                self.need_wait_loops_total as f32 / self.need_wait_calls as f32
            },
            if self.need_wait_calls == 0 {
                0.0
            } else {
                self.need_wait_seen_need_total as f32 / self.need_wait_calls as f32
            },
            if self.need_wait_calls == 0 {
                0.0
            } else {
                self.need_wait_seen_have_total as f32 / self.need_wait_calls as f32
            },
        );
        eprintln!(
            "[mft_encoder][submit][summary] after_need ok={} not_accepting={} err={} avg={}us drain call={} found_output={} queued={} wait_avg={}us drain_avg={}us drained_encode_avg={}us retry ok={} not_accepting={} err={} avg={}us total_avg={}us meta_q_max={} pending_max={} need_buf_max={} have_buf_max={}",
            self.after_need_ok,
            self.after_need_not_accepting,
            self.after_need_err,
            Self::avg_us(self.after_need_us_total, self.after_need_ok + self.after_need_not_accepting + self.after_need_err),
            self.drain_calls,
            self.drain_found_output,
            self.drain_queued_frame,
            Self::avg_us(self.drain_wait_us_total, self.drain_calls),
            Self::avg_us(self.drain_output_us_total, self.drain_found_output),
            Self::avg_us(self.drain_encode_us_total, self.drain_queued_frame),
            self.retry_after_drain_ok,
            self.retry_after_drain_not_accepting,
            self.retry_after_drain_err,
            Self::avg_us(self.retry_after_drain_us_total, self.retry_after_drain_ok + self.retry_after_drain_not_accepting + self.retry_after_drain_err),
            Self::avg_us(self.total_submit_us_total, self.submits),
            self.meta_queue_max,
            self.pending_outputs_max,
            self.need_input_buffered_max,
            self.have_output_buffered_max,
        );
        *self = Self::default();
        *window_start = std::time::Instant::now();
    }
}

/// Get event from IMFMediaEventGenerator with MF_EVENT_FLAG_NO_WAIT (non-blocking).
unsafe fn get_event_no_wait(
    event_gen: &IMFMediaEventGenerator,
) -> windows::core::Result<windows::Win32::Media::MediaFoundation::IMFMediaEvent> {
    event_gen.GetEvent(
        windows::Win32::Media::MediaFoundation::MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(1),
    )
}

/// Get event blocking (MF_EVENT_FLAG_NONE = 0). Blocks until an event fires or the MFT errors.
unsafe fn get_event_blocking(
    event_gen: &IMFMediaEventGenerator,
) -> windows::core::Result<windows::Win32::Media::MediaFoundation::IMFMediaEvent> {
    event_gen.GetEvent(
        windows::Win32::Media::MediaFoundation::MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0),
    )
}

/// Non-blocking: drain queued events until the target is found or the queue is empty.
/// Any non-target NeedInput / HaveOutput events are counted so the caller can buffer them
/// instead of silently dropping async MFT state transitions.
/// Phase 4.6 pipelined collect.
fn poll_event_no_wait(event_gen: &IMFMediaEventGenerator, target_type: u32) -> PolledEvents {
    let mut events = PolledEvents::default();
    loop {
        let event = match unsafe { get_event_no_wait(event_gen) } {
            Ok(e) => e,
            Err(_) => return events,
        };
        let mt: u32 = unsafe { event.GetType() }.unwrap_or(0);
        if mt == target_type {
            events.found = true;
            return events;
        }
        if mt == ME_TRANSFORM_NEED_INPUT {
            events.need_input += 1;
        } else if mt == ME_TRANSFORM_HAVE_OUTPUT {
            events.have_output += 1;
        }
    }
}

/// Wait for a specific MFT media event. First tries a non-blocking poll (zero jitter for
/// the common case where the event is already queued), then falls back to a blocking
/// GetEvent call which suspends the thread until the MFT signals — no busy-wait, no sleep jitter.
/// Returns buffered counts for any non-target NeedInput / HaveOutput events that were observed.
fn poll_event_trace(
    event_gen: &IMFMediaEventGenerator,
    target_type: u32,
    timeout_ms: u32,
) -> (PolledEvents, u32) {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(timeout_ms as u64);
    let mut events = PolledEvents::default();
    let mut poll_loops: u32 = 0;

    loop {
        if let Ok(event) = unsafe { get_event_no_wait(event_gen) } {
            let mt: u32 = unsafe { event.GetType() }.unwrap_or(0);
            if mt == target_type {
                events.found = true;
                return (events, poll_loops);
            }
            if mt == ME_TRANSFORM_NEED_INPUT {
                events.need_input += 1;
            } else if mt == ME_TRANSFORM_HAVE_OUTPUT {
                events.have_output += 1;
            }
            continue;
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            return (events, poll_loops);
        }

        let remaining = deadline.saturating_duration_since(now);
        poll_loops = poll_loops.saturating_add(1);
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(1)));
    }
}

fn poll_event(event_gen: &IMFMediaEventGenerator, target_type: u32, timeout_ms: u32) -> PolledEvents {
    poll_event_trace(event_gen, target_type, timeout_ms).0
}

#[derive(Error, Debug)]
pub enum MftEncoderError {
    #[error("No MFT H.264 encoder found (hardware or software)")]
    NoEncoderFound,
    #[error("MFT initialization failed: {0}")]
    Init(String),
    #[error("Media type setup failed: {0}")]
    MediaType(String),
    #[error("Encode failed: {0}")]
    Encode(String),
    #[error("Windows API error: {0}")]
    Windows(#[from] windows::core::Error),
}

/// Encoded H.264 frame (Annex B).
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub timestamp_us: i64,
    pub key_frame: bool,
}

/// Per-frame metadata queued at submit time and popped at collect time.
/// Ensures each encoded output is tagged with the rtp_ts/capture_us
/// of the input frame that produced it, even when the async MFT pipeline
/// has multiple frames in flight simultaneously.
struct FrameMeta {
    rtp_ts: u32,
    capture_us: i64,
    submit_time: std::time::Instant,
    requested_key_frame: bool,
}

struct PendingOutput {
    frame: EncodedFrame,
    rtp_ts: u32,
    capture_us: i64,
    encode_us: u64,
}

/// MFT H.264 encoder. Zero-copy via IMFDXGIBuffer.
pub struct MftH264Encoder {
    transform: IMFTransform,
    device_manager: IMFDXGIDeviceManager,
    event_gen: Option<IMFMediaEventGenerator>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
    frame_count: u64,
    output_buf: Vec<u8>,
    encoder_name: String,
    is_hardware: bool,
    is_async: bool,
    /// FIFO of per-frame metadata pushed at submit() and popped at collect().
    /// One entry per in-flight frame — fixes rtp_ts/capture_us misassignment
    /// when multiple frames are simultaneously buffered inside the async MFT.
    meta_queue: VecDeque<FrameMeta>,
    /// Phase 4.6 fix: poll_event_no_wait (collect) may consume NeedInput events while
    /// searching for HaveOutput. Track how many NeedInput events were consumed so
    /// submit() can skip the event wait when one is already buffered.
    need_input_buffered: u32,
    /// Symmetric buffer for HaveOutput events consumed while another caller is
    /// waiting for NeedInput. Dropping these events breaks the async MFT contract
    /// and can leave the encoder permanently MF_E_NOTACCEPTING.
    have_output_buffered: u32,
    /// Encoded outputs drained inside submit() when the async MFT reports
    /// MF_E_NOTACCEPTING. collect()/collect_blocking() returns them first.
    pending_outputs: VecDeque<PendingOutput>,
    submit_diag_window_start: std::time::Instant,
    submit_diag: SubmitDiagWindow,
    submit_trace_verbose: bool,
}

impl MftH264Encoder {
    /// Create MFT H.264 encoder on the given D3D11 device.
    /// Prefers hardware MFT, falls back to software.
    pub fn new(
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self, MftEncoderError> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            // MFStartup is required before any MFT object creation.
            // NVIDIA/AMD hardware MFTs return E_FAIL on ProcessMessage(SET_D3D_MANAGER) without it.
            let _ = MFStartup(MF_VERSION, 0);
        }

        // Create DXGI device manager first — pass it to create_mft so it's used during probe.
        // This avoids the double-SET_D3D_MANAGER problem with NVIDIA NVENC MFT.
        let mut reset_token: u32 = 0;
        let mut device_manager: Option<IMFDXGIDeviceManager> = None;
        unsafe {
            MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager)?;
        }
        let device_manager = device_manager.ok_or_else(|| {
            MftEncoderError::Init("MFCreateDXGIDeviceManager returned null".into())
        })?;
        unsafe {
            device_manager.ResetDevice(device, reset_token)?;
        }

        // create_mft uses the provided device_manager for SET_D3D_MANAGER probe.
        // Returns (transform, name, is_hardware) — SET_D3D_MANAGER already applied for hardware.
        let (transform, encoder_name, is_hardware) = create_mft(device, &device_manager)?;

        if is_hardware {
            // Hardware MFT (NVIDIA NVENC): use GetOutputAvailableType/GetInputAvailableType
            // to find compatible types. Manual type creation fails with MF_E_UNSUPPORTED_D3D_TYPE.
            set_output_type_from_available(&transform, width, height, fps, bitrate_bps)?;
            set_input_type_from_available(&transform, width, height, fps)?;
        } else {
            // Software MFT: standard order (output first, then input)
            let mt_out = create_output_media_type(width, height, fps, bitrate_bps)?;
            unsafe { transform.SetOutputType(0, &mt_out, 0)?; }
            let mt_in = create_input_media_type(width, height, fps)?;
            unsafe { transform.SetInputType(0, &mt_in, 0)?; }
        }

        // Enable low-latency mode: eliminates frame reordering, one input → one output.
        // Critical for real-time streaming; without it NVENC can buffer 1-3 frames.
        unsafe {
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
                eprintln!("[mft_encoder] MF_LOW_LATENCY set on MFT");
            }
        }
        // ICodecAPI: B-frames=0, low-latency mode, rate control, long GOP, mean bitrate
        if let Ok(codec) = transform.cast::<ICodecAPI>() {
            // Keep the MFT's own GOP much longer than the old 2 s default.
            // We already drive recovery with startup IDRs + periodic forced IDR/GIR.
            // A short internal GOP adds large intra spikes that collapse WAN FPS.
            let gop_secs = std::env::var("ASTRIX_MFT_GOP_SECS")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(12)
                .max(1);
            let gop = fps.saturating_mul(gop_secs);
            unsafe {
                let _ = codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
                let _ = codec.SetValue(&CODECAPI_AVEncCommonLowLatency, &VARIANT::from(1u32));
                // Rate control: CBR (0) for ≥60fps (smooths encode peaks), LowDelayVBR (4) for ≤30fps.
                // Phase 4.3: CBR gives predictable encode time; VBR spikes on complex scenes.
                let rc_mode = if fps >= 60 { 0u32 } else { 4u32 }; // 0=CBR, 4=LowDelayVBR
                let rc_ok = codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(rc_mode));
                if rc_ok.is_err() {
                    let _ = codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(0u32));
                }
                let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate_bps));
                let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(gop));
                // Phase 4.2: speed tuning — software MFT ~20–50% faster, hardware typically ignores
                let _ = codec.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &VARIANT::from(100u32));
                let _ = codec.SetValue(&CODECAPI_AVEncH264CABACEnable, &VARIANT::from(0u32));
                let _ = codec.SetValue(&CODECAPI_AVEncNumWorkerThreads, &VARIANT::from(4u32));
                // Phase 4.7: GIR instead of IDR — smooth encode, no keyframe spikes. Hardware MFT supports; software often E_NOTIMPL.
                let gir_ok = codec.SetValue(&CODECAPI_AVEncVideoIntraRefreshMode, &VARIANT::from(2u32));
                if gir_ok.is_ok() {
                    eprintln!("[mft_encoder] ICodecAPI: GIR (Gradual Intra Refresh) enabled");
                }
                // eAVEncSliceControlMode_Constrained = 2 — optional; set ASTRIX_MFT_SLICE_CONSTRAINED=0 to skip.
                let skip_constrained = std::env::var("ASTRIX_MFT_SLICE_CONSTRAINED")
                    .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
                    .unwrap_or(false);
                if !skip_constrained {
                    let sc = codec.SetValue(
                        &CODECAPI_AVEncVideoEncodeSliceSizeControlMode,
                        &VARIANT::from(2u32),
                    );
                    if sc.is_ok() {
                        eprintln!("[mft_encoder] ICodecAPI: slice size control = Constrained (smaller slices if supported)");
                    }
                }
            }
            eprintln!("[mft_encoder] ICodecAPI: B-frames=0, low-latency, rate-control={}, GOP={}, bitrate={} bps, QualityVsSpeed=100, CABAC=0, WorkerThreads=4", if fps >= 60 { "CBR" } else { "LowDelayVBR" }, gop, bitrate_bps);
        }

        // Detect async MFT: try to get IMFMediaEventGenerator (only present on async MFTs).
        let event_gen: Option<IMFMediaEventGenerator> = transform.cast().ok();
        let is_async = event_gen.is_some();
        if is_async {
            eprintln!("[mft_encoder] Async MFT detected — using event-driven encode");
        }

        // Notify stream start (BEGIN_STREAMING must precede START_OF_STREAM for async MFTs).
        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        Ok(Self {
            transform,
            device_manager,
            event_gen,
            width,
            height,
            fps,
            bitrate_bps,
            frame_count: 0,
            output_buf: Vec::with_capacity(256 * 1024),
            encoder_name,
            is_hardware,
            is_async,
            meta_queue: VecDeque::new(),
            need_input_buffered: 0,
            have_output_buffered: 0,
            pending_outputs: VecDeque::new(),
            submit_diag_window_start: std::time::Instant::now(),
            submit_diag: SubmitDiagWindow::default(),
            submit_trace_verbose: std::env::var("ASTRIX_MFT_SUBMIT_TRACE")
                .map(|v| v != "0")
                .unwrap_or(false),
        })
    }

    /// Encode NV12 D3D11 texture. Zero-copy via IMFDXGIBuffer.
    /// Returns H.264 Annex B data. May return empty if encoder buffers (MF_E_TRANSFORM_NEED_MORE_INPUT).
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp_us: i64,
        key_frame: bool,
    ) -> Result<Vec<EncodedFrame>, MftEncoderError> {
        if key_frame {
            self.force_key_frame()?;
        }

        let sample = create_sample_from_texture(texture, timestamp_us, self.fps)?;

        if self.is_async {
            let mut frames = self.encode_async(sample, timestamp_us, key_frame)?;
            for frame in frames.iter_mut() {
                frame.key_frame = key_frame;
            }
            return Ok(frames);
        }

        unsafe {
            self.transform.ProcessInput(0, &sample, 0)?;
        }
        self.frame_count += 1;
        let mut frames = self.drain_output(timestamp_us)?;
        for frame in frames.iter_mut() {
            frame.key_frame = key_frame;
        }
        Ok(frames)
    }

    /// Event-driven encode for async MFTs (NVIDIA NVENC).
    /// Uses IMFMediaEventGenerator to wait for NeedInput/HaveOutput events.
    fn encode_async(
        &mut self,
        sample: IMFSample,
        original_timestamp_us: i64,
        key_frame: bool,
    ) -> Result<Vec<EncodedFrame>, MftEncoderError> {
        let mut frames = Vec::new();
        let output_wait_ms = self.async_output_timeout_ms(key_frame);

        // Some async NVIDIA MFTs accept the first input before they emit a
        // NeedInput event. Try ProcessInput directly first; only wait for
        // NeedInput when the transform reports MF_E_NOTACCEPTING.
        let process_input = unsafe { self.transform.ProcessInput(0, &sample, 0) };
        if let Err(e) = process_input {
            if e.code() != MF_E_NOTACCEPTING {
                return Err(MftEncoderError::Encode(format!("async ProcessInput: {:?}", e)));
            }
            if let Some(frame) =
                self.drain_async_ready_output(original_timestamp_us, output_wait_ms)?
            {
                frames.push(frame);
            } else {
                return Err(MftEncoderError::Encode(
                    "async MFT: timeout waiting for METransformHaveOutput".into(),
                ));
            }
            match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => {}
                Err(err) if err.code() == MF_E_NOTACCEPTING => {
                    return Err(MftEncoderError::Encode(
                        "async MFT: still not accepting after draining output".into(),
                    ));
                }
                Err(err) => {
                    return Err(MftEncoderError::Encode(format!(
                        "async ProcessInput after drain: {:?}",
                        err
                    )));
                }
            }
        }
        self.frame_count += 1;

        // Wait for METransformHaveOutput, then drain one frame. Async MFT (NVENC) signals
        // one HaveOutput per output sample; calling ProcessOutput without a matching
        // event can return E_UNEXPECTED or worse on NVIDIA. Buffered events let us
        // keep strict event ordering even if another wait consumed the signal earlier.
        if let Some(frame) = self.drain_async_ready_output(original_timestamp_us, output_wait_ms)? {
            frames.push(frame);
        }

        Ok(frames)
    }

    /// Phase 4.6: Submit frame for encoding (async MFT only). Non-blocking after brief NeedInput wait.
    /// Stores rtp_ts and capture_us for collect() to use when pushing.
    pub fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        ts_us: i64,
        key_frame: bool,
        rtp_ts: u32,
        capture_us: i64,
        need_input_timeout_ms: u32,
    ) -> Result<(), MftEncoderError> {
        if !self.is_async {
            return Err(MftEncoderError::Encode(
                "submit/collect only for async MFT; use encode() for sync".into(),
            ));
        }
        if key_frame {
            self.force_key_frame()?;
        }
        self.submit_diag
            .maybe_log(&mut self.submit_diag_window_start, &self.encoder_name);
        let submit_idx = self.frame_count + 1;
        let submit_start = std::time::Instant::now();
        let meta_before = self.meta_queue.len();
        let pending_before = self.pending_outputs.len();
        let need_buf_before = self.need_input_buffered;
        let have_buf_before = self.have_output_buffered;

        let sample_start = std::time::Instant::now();
        let sample = create_sample_from_texture(texture, ts_us, self.fps)?;
        let sample_us = sample_start.elapsed().as_micros() as u64;

        let initial_pi_start = std::time::Instant::now();
        let initial_process_input = unsafe { self.transform.ProcessInput(0, &sample, 0) };
        let initial_pi_us = initial_pi_start.elapsed().as_micros() as u64;

        let mut need_wait_trace = AsyncWaitTrace::default();
        let mut need_wait_used = false;
        let mut after_need_pi_us: u64 = 0;
        let mut after_need_state = "skipped";
        let mut drain_trace = DrainPendingTrace::default();
        let mut retry_after_drain_us: u64 = 0;
        let mut retry_after_drain_state = "skipped";

        if let Err(ref e) = initial_process_input {
            if e.code() != MF_E_NOTACCEPTING {
                if self.submit_trace_verbose {
                    eprintln!(
                        "[mft_encoder][submit #{}] key={} ts_us={} meta_q {}->{} pending {}->{} need_buf {}->{} have_buf {}->{} initial=err total={}us err={:?}",
                        submit_idx,
                        key_frame,
                        ts_us,
                        meta_before,
                        self.meta_queue.len(),
                        pending_before,
                        self.pending_outputs.len(),
                        need_buf_before,
                        self.need_input_buffered,
                        have_buf_before,
                        self.have_output_buffered,
                        submit_start.elapsed().as_micros() as u64,
                        e,
                    );
                }
                return Err(MftEncoderError::Encode(format!("async ProcessInput: {:?}", e)));
            }
            // Some async MFTs can accept another input after METransformNeedInput
            // without having a completed output ready yet. Try that first to keep
            // the pipeline depth >1; only fall back to draining output when the
            // transform still refuses more input.
            let mut accepted_after_need_input = false;
            need_wait_used = true;
            need_wait_trace =
                self.wait_async_event_trace(ME_TRANSFORM_NEED_INPUT, need_input_timeout_ms)?;
            if need_wait_trace.found {
                let after_need_start = std::time::Instant::now();
                match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                    Ok(()) => {
                        accepted_after_need_input = true;
                        after_need_state = "ok";
                    }
                    Err(err) if err.code() == MF_E_NOTACCEPTING => {
                        after_need_state = "not_accepting";
                    }
                    Err(err) => {
                        after_need_pi_us = after_need_start.elapsed().as_micros() as u64;
                        if self.submit_trace_verbose {
                            eprintln!(
                                "[mft_encoder][submit #{}] key={} ts_us={} initial=not_accepting wait_need(found={}, buffered={}, us={}, loops={}, seen_need={}, seen_have={}) after_need=err us={} meta_q {}->{} pending {}->{} err={:?}",
                                submit_idx,
                                key_frame,
                                ts_us,
                                need_wait_trace.found,
                                need_wait_trace.buffered,
                                need_wait_trace.wait_us,
                                need_wait_trace.poll_loops,
                                need_wait_trace.need_input,
                                need_wait_trace.have_output,
                                after_need_pi_us,
                                meta_before,
                                self.meta_queue.len(),
                                pending_before,
                                self.pending_outputs.len(),
                                err,
                            );
                        }
                        return Err(MftEncoderError::Encode(format!(
                            "async ProcessInput after NeedInput: {:?}",
                            err
                        )));
                    }
                }
                after_need_pi_us = after_need_start.elapsed().as_micros() as u64;
            } else {
                after_need_state = "need_input_timeout";
            }
            if !accepted_after_need_input {
                drain_trace = self.drain_output_to_pending_trace(need_input_timeout_ms)?;
                if !drain_trace.queued_frame {
                    if self.submit_trace_verbose {
                        eprintln!(
                            "[mft_encoder][submit #{}] key={} ts_us={} initial=not_accepting wait_need(found={}, buffered={}, us={}, loops={}, seen_need={}, seen_have={}) after_need={} us={} drain(found_output={}, queued={}, wait_us={}, drain_us={}, drained_encode_us={}) total={}us meta_q {}->{} pending {}->{} need_buf {}->{} have_buf {}->{}",
                            submit_idx,
                            key_frame,
                            ts_us,
                            need_wait_trace.found,
                            need_wait_trace.buffered,
                            need_wait_trace.wait_us,
                            need_wait_trace.poll_loops,
                            need_wait_trace.need_input,
                            need_wait_trace.have_output,
                            after_need_state,
                            after_need_pi_us,
                            drain_trace.found_output,
                            drain_trace.queued_frame,
                            drain_trace.wait.wait_us,
                            drain_trace.drain_us,
                            drain_trace.encode_us,
                            submit_start.elapsed().as_micros() as u64,
                            meta_before,
                            self.meta_queue.len(),
                            pending_before,
                            self.pending_outputs.len(),
                            need_buf_before,
                            self.need_input_buffered,
                            have_buf_before,
                            self.have_output_buffered,
                        );
                    }
                    return Err(MftEncoderError::Encode(
                        "async MFT: timeout waiting for METransformHaveOutput".into(),
                    ));
                }
                let retry_after_drain_start = std::time::Instant::now();
                match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                    Ok(()) => {
                        retry_after_drain_state = "ok";
                    }
                    Err(err) if err.code() == MF_E_NOTACCEPTING => {
                        retry_after_drain_us =
                            retry_after_drain_start.elapsed().as_micros() as u64;
                        retry_after_drain_state = "not_accepting";
                        if self.submit_trace_verbose {
                            eprintln!(
                                "[mft_encoder][submit #{}] key={} ts_us={} initial=not_accepting wait_need(found={}, buffered={}, us={}, loops={}, seen_need={}, seen_have={}) after_need={} us={} drain(found_output={}, queued={}, wait_us={}, drain_us={}, drained_encode_us={}) retry_after_drain={} us={} total={}us meta_q {}->{} pending {}->{} need_buf {}->{} have_buf {}->{}",
                                submit_idx,
                                key_frame,
                                ts_us,
                                need_wait_trace.found,
                                need_wait_trace.buffered,
                                need_wait_trace.wait_us,
                                need_wait_trace.poll_loops,
                                need_wait_trace.need_input,
                                need_wait_trace.have_output,
                                after_need_state,
                                after_need_pi_us,
                                drain_trace.found_output,
                                drain_trace.queued_frame,
                                drain_trace.wait.wait_us,
                                drain_trace.drain_us,
                                drain_trace.encode_us,
                                retry_after_drain_state,
                                retry_after_drain_us,
                                submit_start.elapsed().as_micros() as u64,
                                meta_before,
                                self.meta_queue.len(),
                                pending_before,
                                self.pending_outputs.len(),
                                need_buf_before,
                                self.need_input_buffered,
                                have_buf_before,
                                self.have_output_buffered,
                            );
                        }
                        return Err(MftEncoderError::Encode(
                            "async MFT: still not accepting after draining output".into(),
                        ));
                    }
                    Err(err) => {
                        retry_after_drain_us =
                            retry_after_drain_start.elapsed().as_micros() as u64;
                        retry_after_drain_state = "err";
                        if self.submit_trace_verbose {
                            eprintln!(
                                "[mft_encoder][submit #{}] key={} ts_us={} initial=not_accepting wait_need(found={}, buffered={}, us={}, loops={}, seen_need={}, seen_have={}) after_need={} us={} drain(found_output={}, queued={}, wait_us={}, drain_us={}, drained_encode_us={}) retry_after_drain=err us={} total={}us meta_q {}->{} pending {}->{} need_buf {}->{} have_buf {}->{} err={:?}",
                                submit_idx,
                                key_frame,
                                ts_us,
                                need_wait_trace.found,
                                need_wait_trace.buffered,
                                need_wait_trace.wait_us,
                                need_wait_trace.poll_loops,
                                need_wait_trace.need_input,
                                need_wait_trace.have_output,
                                after_need_state,
                                after_need_pi_us,
                                drain_trace.found_output,
                                drain_trace.queued_frame,
                                drain_trace.wait.wait_us,
                                drain_trace.drain_us,
                                drain_trace.encode_us,
                                retry_after_drain_us,
                                submit_start.elapsed().as_micros() as u64,
                                meta_before,
                                self.meta_queue.len(),
                                pending_before,
                                self.pending_outputs.len(),
                                need_buf_before,
                                self.need_input_buffered,
                                have_buf_before,
                                self.have_output_buffered,
                                err,
                            );
                        }
                        return Err(MftEncoderError::Encode(format!(
                            "async ProcessInput after drain: {:?}",
                            err
                        )));
                    }
                }
                retry_after_drain_us = retry_after_drain_start.elapsed().as_micros() as u64;
            }
        }
        self.frame_count += 1;
        self.meta_queue.push_back(FrameMeta {
            rtp_ts,
            capture_us,
            submit_time: std::time::Instant::now(),
            requested_key_frame: key_frame,
        });
        let total_submit_us = submit_start.elapsed().as_micros() as u64;
        self.submit_diag.submits = self.submit_diag.submits.saturating_add(1);
        self.submit_diag.sample_us_total =
            self.submit_diag.sample_us_total.saturating_add(sample_us);
        self.submit_diag.initial_us_total =
            self.submit_diag.initial_us_total.saturating_add(initial_pi_us);
        match &initial_process_input {
            Ok(()) => self.submit_diag.initial_ok = self.submit_diag.initial_ok.saturating_add(1),
            Err(err) if err.code() == MF_E_NOTACCEPTING => {
                self.submit_diag.initial_not_accepting =
                    self.submit_diag.initial_not_accepting.saturating_add(1)
            }
            Err(_) => self.submit_diag.initial_err = self.submit_diag.initial_err.saturating_add(1),
        }
        if need_wait_used {
            self.submit_diag.need_wait_calls =
                self.submit_diag.need_wait_calls.saturating_add(1);
            if need_wait_trace.found {
                self.submit_diag.need_wait_found =
                    self.submit_diag.need_wait_found.saturating_add(1);
            }
            if need_wait_trace.buffered {
                self.submit_diag.need_wait_buffered =
                    self.submit_diag.need_wait_buffered.saturating_add(1);
            }
            self.submit_diag.need_wait_us_total = self
                .submit_diag
                .need_wait_us_total
                .saturating_add(need_wait_trace.wait_us);
            self.submit_diag.need_wait_loops_total = self
                .submit_diag
                .need_wait_loops_total
                .saturating_add(need_wait_trace.poll_loops as u64);
            self.submit_diag.need_wait_seen_need_total = self
                .submit_diag
                .need_wait_seen_need_total
                .saturating_add(need_wait_trace.need_input as u64);
            self.submit_diag.need_wait_seen_have_total = self
                .submit_diag
                .need_wait_seen_have_total
                .saturating_add(need_wait_trace.have_output as u64);
        }
        if need_wait_used && need_wait_trace.found {
            self.submit_diag.after_need_us_total = self
                .submit_diag
                .after_need_us_total
                .saturating_add(after_need_pi_us);
            match after_need_state {
                "ok" => self.submit_diag.after_need_ok =
                    self.submit_diag.after_need_ok.saturating_add(1),
                "not_accepting" => self.submit_diag.after_need_not_accepting =
                    self.submit_diag.after_need_not_accepting.saturating_add(1),
                "err" => self.submit_diag.after_need_err =
                    self.submit_diag.after_need_err.saturating_add(1),
                _ => {}
            }
        }
        if need_wait_used && after_need_state != "ok" {
            self.submit_diag.drain_calls = self.submit_diag.drain_calls.saturating_add(1);
            if drain_trace.found_output {
                self.submit_diag.drain_found_output =
                    self.submit_diag.drain_found_output.saturating_add(1);
            }
            if drain_trace.queued_frame {
                self.submit_diag.drain_queued_frame =
                    self.submit_diag.drain_queued_frame.saturating_add(1);
            }
            self.submit_diag.drain_wait_us_total = self
                .submit_diag
                .drain_wait_us_total
                .saturating_add(drain_trace.wait.wait_us);
            self.submit_diag.drain_output_us_total = self
                .submit_diag
                .drain_output_us_total
                .saturating_add(drain_trace.drain_us);
            self.submit_diag.drain_encode_us_total = self
                .submit_diag
                .drain_encode_us_total
                .saturating_add(drain_trace.encode_us);
            if drain_trace.queued_frame {
                self.submit_diag.retry_after_drain_us_total = self
                    .submit_diag
                    .retry_after_drain_us_total
                    .saturating_add(retry_after_drain_us);
                match retry_after_drain_state {
                    "ok" => self.submit_diag.retry_after_drain_ok = self
                        .submit_diag
                        .retry_after_drain_ok
                        .saturating_add(1),
                    "not_accepting" => self.submit_diag.retry_after_drain_not_accepting = self
                        .submit_diag
                        .retry_after_drain_not_accepting
                        .saturating_add(1),
                    "err" => self.submit_diag.retry_after_drain_err = self
                        .submit_diag
                        .retry_after_drain_err
                        .saturating_add(1),
                    _ => {}
                }
            }
        }
        self.submit_diag.total_submit_us_total = self
            .submit_diag
            .total_submit_us_total
            .saturating_add(total_submit_us);
        self.submit_diag.meta_queue_max =
            self.submit_diag.meta_queue_max.max(self.meta_queue.len());
        self.submit_diag.pending_outputs_max = self
            .submit_diag
            .pending_outputs_max
            .max(self.pending_outputs.len());
        self.submit_diag.need_input_buffered_max = self
            .submit_diag
            .need_input_buffered_max
            .max(self.need_input_buffered);
        self.submit_diag.have_output_buffered_max = self
            .submit_diag
            .have_output_buffered_max
            .max(self.have_output_buffered);
        if self.submit_trace_verbose {
            let initial_state = match &initial_process_input {
                Ok(()) => "ok",
                Err(err) if err.code() == MF_E_NOTACCEPTING => "not_accepting",
                Err(_) => "err",
            };
            eprintln!(
                "[mft_encoder][submit #{}] key={} ts_us={} sample={}us initial={}({}us) wait_need(found={}, buffered={}, us={}, loops={}, seen_need={}, seen_have={}) after_need={}({}us) drain(found_output={}, queued={}, wait_us={}, drain_us={}, drained_encode_us={}) retry_after_drain={}({}us) total={}us meta_q {}->{} pending {}->{} need_buf {}->{} have_buf {}->{}",
                submit_idx,
                key_frame,
                ts_us,
                sample_us,
                initial_state,
                initial_pi_us,
                need_wait_trace.found,
                need_wait_trace.buffered,
                need_wait_trace.wait_us,
                need_wait_trace.poll_loops,
                need_wait_trace.need_input,
                need_wait_trace.have_output,
                after_need_state,
                after_need_pi_us,
                drain_trace.found_output,
                drain_trace.queued_frame,
                drain_trace.wait.wait_us,
                drain_trace.drain_us,
                drain_trace.encode_us,
                retry_after_drain_state,
                retry_after_drain_us,
                total_submit_us,
                meta_before,
                self.meta_queue.len(),
                pending_before,
                self.pending_outputs.len(),
                need_buf_before,
                self.need_input_buffered,
                have_buf_before,
                self.have_output_buffered,
            );
        }
        Ok(())
    }

    /// Phase 4.6: Collect encoded output from previous submit (non-blocking). Returns None if not ready.
    pub fn collect(&mut self) -> Result<Option<(Vec<EncodedFrame>, u32, i64, u64)>, MftEncoderError> {
        if !self.is_async {
            return Err(MftEncoderError::Encode(
                "submit/collect only for async MFT; use encode() for sync".into(),
            ));
        }
        if let Some(pending) = self.pending_outputs.pop_front() {
            return Ok(Some((vec![pending.frame], pending.rtp_ts, pending.capture_us, pending.encode_us)));
        }
        let found = self.poll_async_event_no_wait(ME_TRANSFORM_HAVE_OUTPUT)?;
        if !found {
            return Ok(None);
        }
        self.collect_inner()
    }

    /// Phase 4.6: Collect encoded output from previous submit (blocking). Waits up to timeout_ms for HaveOutput.
    /// Use after submit() to push frame in same iteration — avoids 1-frame delay that breaks stream startup.
    pub fn collect_blocking(&mut self, timeout_ms: u32) -> Result<Option<(Vec<EncodedFrame>, u32, i64, u64)>, MftEncoderError> {
        if !self.is_async {
            return Err(MftEncoderError::Encode(
                "submit/collect only for async MFT; use encode() for sync".into(),
            ));
        }
        if let Some(pending) = self.pending_outputs.pop_front() {
            return Ok(Some((vec![pending.frame], pending.rtp_ts, pending.capture_us, pending.encode_us)));
        }
        let found = self.wait_async_event(ME_TRANSFORM_HAVE_OUTPUT, timeout_ms)?;
        if !found {
            return Ok(None);
        }
        self.collect_inner()
    }

    fn collect_inner(&mut self) -> Result<Option<(Vec<EncodedFrame>, u32, i64, u64)>, MftEncoderError> {
        let Some(frame) = self.drain_output_once(0)? else {
            return Ok(None);
        };
        let meta = self.meta_queue.pop_front().unwrap_or_else(|| FrameMeta {
            rtp_ts: 0,
            capture_us: 0,
            submit_time: std::time::Instant::now(),
            requested_key_frame: false,
        });
        let encode_us = meta.submit_time.elapsed().as_micros() as u64;
        let mut frame = frame;
        frame.key_frame = meta.requested_key_frame;
        Ok(Some((vec![frame], meta.rtp_ts, meta.capture_us, encode_us)))
    }

    fn force_key_frame(&self) -> Result<(), MftEncoderError> {
        if let Ok(codec) = self.transform.cast::<ICodecAPI>() {
            unsafe {
                let _ = codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
            }
        }
        Ok(())
    }

    fn drain_output(&mut self, original_timestamp_us: i64) -> Result<Vec<EncodedFrame>, MftEncoderError> {
        let mut frames = Vec::new();
        loop {
            match self.drain_output_once(original_timestamp_us) {
                Ok(Some(f)) => frames.push(f),
                Ok(None) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(frames)
    }

    /// Try to pull one encoded frame. Returns Ok(None) when the MFT needs more input.
    fn drain_output_once(&mut self, original_timestamp_us: i64) -> Result<Option<EncodedFrame>, MftEncoderError> {
        let mut output_buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut output_buffers: [MFT_OUTPUT_DATA_BUFFER; 1] = [output_buffer];
        let mut status: u32 = 0;

        let result = unsafe {
            self.transform.ProcessOutput(0, &mut output_buffers, &mut status)
        };

        match result {
            Ok(()) => {
                if let Some(ref sample) = *output_buffers[0].pSample {
                    return extract_h264_from_sample(sample, &mut self.output_buf, original_timestamp_us);
                }
                Ok(None)
            }
            Err(e) => {
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(None);
                }
                Err(MftEncoderError::Encode(format!("ProcessOutput failed: {:?}", e)))
            }
        }
    }

    /// Update bitrate on the fly (CODECAPI_AVEncCommonMeanBitRate).
    pub fn set_bitrate(&mut self, bps: u32) -> Result<(), MftEncoderError> {
        if bps == 0 || self.bitrate_bps == bps {
            return Ok(());
        }
        if let Ok(codec) = self.transform.cast::<ICodecAPI>() {
            unsafe {
                codec
                    .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps))
                    .map_err(|e| MftEncoderError::Encode(format!("Set bitrate: {:?}", e)))?;
            }
        }
        if let Ok(mt) = unsafe { self.transform.GetOutputCurrentType(0) } {
            let attrs: &IMFAttributes = &mt;
            unsafe {
                let _ = attrs.SetUINT32(&MF_MT_AVG_BITRATE, bps);
            }
        }
        self.bitrate_bps = bps;
        Ok(())
    }

    pub fn is_hardware(&self) -> bool {
        self.is_hardware
    }

    pub fn is_async(&self) -> bool {
        self.is_async
    }

    pub fn encoder_name(&self) -> &str {
        &self.encoder_name
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }

    fn async_output_timeout_ms(&self, key_frame: bool) -> u32 {
        if self.frame_count < 3 || key_frame {
            120
        } else {
            ((2_000u32 / self.fps.max(1)).max(ASYNC_HAVE_OUTPUT_WAIT_MS)).clamp(8, 40)
        }
    }

    fn buffer_async_events(&mut self, events: PolledEvents) {
        self.need_input_buffered = self.need_input_buffered.saturating_add(events.need_input);
        self.have_output_buffered = self.have_output_buffered.saturating_add(events.have_output);
    }

    fn take_buffered_event(&mut self, target_type: u32) -> bool {
        match target_type {
            ME_TRANSFORM_NEED_INPUT if self.need_input_buffered > 0 => {
                self.need_input_buffered -= 1;
                true
            }
            ME_TRANSFORM_HAVE_OUTPUT if self.have_output_buffered > 0 => {
                self.have_output_buffered -= 1;
                true
            }
            _ => false,
        }
    }

    fn wait_async_event(
        &mut self,
        target_type: u32,
        timeout_ms: u32,
    ) -> Result<bool, MftEncoderError> {
        Ok(self.wait_async_event_trace(target_type, timeout_ms)?.found)
    }

    fn wait_async_event_trace(
        &mut self,
        target_type: u32,
        timeout_ms: u32,
    ) -> Result<AsyncWaitTrace, MftEncoderError> {
        let start = std::time::Instant::now();
        if self.take_buffered_event(target_type) {
            return Ok(AsyncWaitTrace {
                found: true,
                buffered: true,
                wait_us: start.elapsed().as_micros() as u64,
                ..Default::default()
            });
        }
        let event_gen = self.event_gen.as_ref().ok_or_else(|| {
            MftEncoderError::Encode("async MFT missing IMFMediaEventGenerator".into())
        })?;
        let (events, poll_loops) = poll_event_trace(event_gen, target_type, timeout_ms);
        self.buffer_async_events(events);
        Ok(AsyncWaitTrace {
            found: events.found,
            buffered: false,
            need_input: events.need_input,
            have_output: events.have_output,
            wait_us: start.elapsed().as_micros() as u64,
            poll_loops,
        })
    }

    fn poll_async_event_no_wait(&mut self, target_type: u32) -> Result<bool, MftEncoderError> {
        if self.take_buffered_event(target_type) {
            return Ok(true);
        }
        let event_gen = self.event_gen.as_ref().ok_or_else(|| {
            MftEncoderError::Encode("async MFT missing IMFMediaEventGenerator".into())
        })?;
        let events = poll_event_no_wait(event_gen, target_type);
        self.buffer_async_events(events);
        Ok(events.found)
    }

    fn drain_async_ready_output(
        &mut self,
        original_timestamp_us: i64,
        timeout_ms: u32,
    ) -> Result<Option<EncodedFrame>, MftEncoderError> {
        if !self.wait_async_event(ME_TRANSFORM_HAVE_OUTPUT, timeout_ms)? {
            return Ok(None);
        }
        self.drain_output_once(original_timestamp_us)
    }

    fn drain_output_to_pending(&mut self, timeout_ms: u32) -> Result<bool, MftEncoderError> {
        Ok(self.drain_output_to_pending_trace(timeout_ms)?.queued_frame)
    }

    fn drain_output_to_pending_trace(
        &mut self,
        timeout_ms: u32,
    ) -> Result<DrainPendingTrace, MftEncoderError> {
        let wait = self.wait_async_event_trace(ME_TRANSFORM_HAVE_OUTPUT, timeout_ms)?;
        if !wait.found {
            return Ok(DrainPendingTrace {
                found_output: false,
                queued_frame: false,
                wait,
                ..Default::default()
            });
        }
        let drain_start = std::time::Instant::now();
        let Some(frame) = self.drain_output_once(0)? else {
            return Ok(DrainPendingTrace {
                found_output: true,
                queued_frame: false,
                wait,
                drain_us: drain_start.elapsed().as_micros() as u64,
                ..Default::default()
            });
        };
        let meta = self.meta_queue.pop_front().unwrap_or_else(|| FrameMeta {
            rtp_ts: 0,
            capture_us: 0,
            submit_time: std::time::Instant::now(),
            requested_key_frame: false,
        });
        let encode_us = meta.submit_time.elapsed().as_micros() as u64;
        let mut frame = frame;
        frame.key_frame = meta.requested_key_frame;
        self.pending_outputs.push_back(PendingOutput {
            frame,
            rtp_ts: meta.rtp_ts,
            capture_us: meta.capture_us,
            encode_us,
        });
        Ok(DrainPendingTrace {
            found_output: true,
            queued_frame: true,
            wait,
            drain_us: drain_start.elapsed().as_micros() as u64,
            encode_us,
        })
    }
}

/// Create MFT from IMFActivate.
/// Tries hardware first; falls back to software (sync) if hardware is async-only.
/// Env ASTRIX_MFT_SOFTWARE=1 forces software MFT.
fn create_mft(
    _device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    device_manager: &IMFDXGIDeviceManager,
) -> Result<(IMFTransform, String, bool), MftEncoderError> {
    let prefer_software = std::env::var("ASTRIX_MFT_SOFTWARE").is_ok();
    let input_type: MFT_REGISTER_TYPE_INFO = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video.into(),
        guidSubtype: MFVideoFormat_NV12.into(),
    };
    let output_type: MFT_REGISTER_TYPE_INFO = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video.into(),
        guidSubtype: MFVideoFormat_H264.into(),
    };

    // Try hardware first (unless ASTRIX_MFT_SOFTWARE).
    // Iterate all hardware MFTs and pick the first one that accepts SET_D3D_MANAGER.
    // NVIDIA NVENC accepts it; AMD MFT may not. MFTEnumEx with SORTANDFILTER may put AMD first.
    if !prefer_software {
        let flags_hw = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
        let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count: u32 = 0;

        let result = unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags_hw,
                Some(&input_type as *const _),
                Some(&output_type as *const _),
                &mut activates,
                &mut count,
            )
        };

        if result.is_ok() && count > 0 {
            let activates_slice = unsafe { std::slice::from_raw_parts(activates, count as usize) };
            let mut chosen: Option<(IMFTransform, String)> = None;

            for act_opt in activates_slice.iter() {
                let Some(act) = act_opt else { continue };
                let name = get_friendly_name(act).unwrap_or_else(|_| "Hardware MFT".to_string());

                // Async unlock on activate before ActivateObject
                unsafe {
                    let act_attrs: &IMFAttributes = act;
                    let _ = act_attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                }

                let transform: IMFTransform = match unsafe { act.ActivateObject::<IMFTransform>() } {
                    Ok(t) => t,
                    Err(_) => continue,
                };

                // Async unlock on transform
                unsafe {
                    if let Ok(attrs) = transform.GetAttributes() {
                        let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                    }
                }

                // Test SET_D3D_MANAGER with the permanent device_manager
                let ptr = unsafe { device_manager.as_raw() as usize };
                let accepts_d3d = unsafe {
                    transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, ptr).is_ok()
                };

                if accepts_d3d {
                    eprintln!("[mft_encoder] Selected hardware MFT: {} (accepts D3D manager)", name);
                    chosen = Some((transform, name));
                    // Do NOT ShutdownObject on the chosen activate — it would invalidate the transform.
                    break;
                } else {
                    eprintln!("[mft_encoder] Skipping hardware MFT: {} (rejected D3D manager)", name);
                    unsafe { let _ = act.ShutdownObject(); }
                }
            }

            unsafe { CoTaskMemFree(Some(activates as *const _ as *mut _)) };

            if let Some((transform, name)) = chosen {
                return Ok((transform, name, true));
            }
        }
    }

    // Software MFT (sync) — always works with ProcessInput/ProcessOutput
    let mut activates_sw: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count_sw: u32 = 0;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT,
            Some(&input_type as *const _),
            Some(&output_type as *const _),
            &mut activates_sw,
            &mut count_sw,
        )?;
    }

    if count_sw == 0 {
        return Err(MftEncoderError::NoEncoderFound);
    }

    let activates_slice = unsafe { std::slice::from_raw_parts(activates_sw, count_sw as usize) };
    let act = activates_slice
        .first()
        .and_then(|o| o.as_ref())
        .ok_or(MftEncoderError::NoEncoderFound)?;

    let name = get_friendly_name(act).unwrap_or_else(|_| "Software MFT".to_string());
    let transform: IMFTransform = unsafe { act.ActivateObject::<IMFTransform>()? };

    for act_opt in activates_slice {
        if let Some(act) = act_opt {
            unsafe { let _ = act.ShutdownObject(); }
        }
    }
    unsafe { CoTaskMemFree(Some(activates_sw as *const _ as *mut _)) };

    Ok((transform, name, false))
}

fn get_friendly_name(activate: &IMFActivate) -> Result<String, windows::core::Error> {
    use windows::Win32::Media::MediaFoundation::MFT_FRIENDLY_NAME_Attribute;

    unsafe {
        let mut buf = [0u16; 256];
        let attrs: &IMFAttributes = activate;
        attrs.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None)?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(String::from_utf16_lossy(&buf[..len]))
    }
}

/// For hardware MFT (NVIDIA NVENC etc.): iterate GetOutputAvailableType to find H264,
/// copy it, add frame size / frame rate / bitrate, then SetOutputType.
/// Hardware MFT rejects manually-created types with MF_E_UNSUPPORTED_D3D_TYPE.
fn set_output_type_from_available(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
) -> Result<IMFMediaType, MftEncoderError> {
    use windows::core::GUID;

    let h264_guid: GUID = MFVideoFormat_H264.into();

    for idx in 0..32u32 {
        let mt = unsafe {
            match transform.GetOutputAvailableType(0, idx) {
                Ok(t) => t,
                Err(_) => break,
            }
        };
        let attrs: &IMFAttributes = &mt;
        let subtype = unsafe { attrs.GetGUID(&MF_MT_SUBTYPE) };
        if subtype.map(|g| g == h264_guid).unwrap_or(false) {
            // Strategy 1: try SetOutputType with the available type as-is (no extra attrs)
            unsafe {
                match transform.SetOutputType(0, &mt, 0) {
                    Ok(_) => {
                        // Now add bitrate/frame size/rate after SetOutputType succeeded
                        let _ = attrs.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps);
                        let _ = attrs.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | (height as u64));
                        let _ = attrs.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1u64);
                        let _ = attrs.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
                        return Ok(mt);
                    }
                    Err(_) => {}
                }
            }

            // Strategy 2: add attrs first, then SetOutputType
            unsafe {
                let _ = attrs.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps);
                let _ = attrs.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | (height as u64));
                let _ = attrs.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1u64);
                let _ = attrs.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);

                match transform.SetOutputType(0, &mt, 0) {
                    Ok(_) => return Ok(mt),
                    Err(e) => {
                        eprintln!("[mft_encoder] SetOutputType with available type [{}] failed: {:?}", idx, e);
                    }
                }
            }
        }
    }

    // Fallback: try manual type
    let mt = create_output_media_type(width, height, fps, bitrate_bps)?;
    unsafe {
        transform.SetOutputType(0, &mt, 0)
            .map_err(|e| MftEncoderError::MediaType(format!("SetOutputType (manual): {:?}", e)))?;
    }
    Ok(mt)
}

/// For hardware MFT: use GetInputAvailableType to find NV12 input type.
fn set_input_type_from_available(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(), MftEncoderError> {
    use windows::core::GUID;
    let nv12_guid: GUID = MFVideoFormat_NV12.into();

    for idx in 0..32u32 {
        let mt = unsafe {
            match transform.GetInputAvailableType(0, idx) {
                Ok(t) => t,
                Err(_) => break,
            }
        };
        let attrs: &IMFAttributes = &mt;
        let subtype = unsafe { attrs.GetGUID(&MF_MT_SUBTYPE) };
        if subtype.map(|g| g == nv12_guid).unwrap_or(false) {
            unsafe {
                let _ = attrs.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | (height as u64));
                let _ = attrs.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1u64);
                let _ = attrs.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);

                match transform.SetInputType(0, &mt, 0) {
                    Ok(_) => return Ok(()),
                    Err(e) => {
                        eprintln!("[mft_encoder] SetInputType with available type [{}] failed: {:?}", idx, e);
                    }
                }
            }
        }
    }

    // Fallback: manual NV12 type
    let mt = create_input_media_type(width, height, fps)?;
    unsafe {
        transform.SetInputType(0, &mt, 0)
            .map_err(|e| MftEncoderError::MediaType(format!("SetInputType (manual): {:?}", e)))?;
    }
    Ok(())
}

fn create_output_media_type(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
) -> Result<IMFMediaType, MftEncoderError> {
    use windows::core::GUID;

    let mt = unsafe { MFCreateMediaType()? };
    let attrs: &IMFAttributes = &mt;
    let major: GUID = MFMediaType_Video.into();
    let subtype: GUID = MFVideoFormat_H264.into();

    unsafe {
        attrs.SetGUID(&MF_MT_MAJOR_TYPE, &major)?;
        attrs.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        attrs.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
        // MF_MT_FRAME_SIZE: (width << 32) | height  (NOT height<<32|width — MF packs width in high DWORD)
        attrs.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | (height as u64))?;
        // MF_MT_FRAME_RATE: (numerator << 32) | denominator. 30 fps = 30/1
        attrs.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1u64)?;
        attrs.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }

    Ok(mt)
}

fn create_input_media_type(
    width: u32,
    height: u32,
    fps: u32,
) -> Result<IMFMediaType, MftEncoderError> {
    use windows::core::GUID;

    let mt = unsafe { MFCreateMediaType()? };
    let attrs: &IMFAttributes = &mt;
    let major: GUID = MFMediaType_Video.into();
    let subtype: GUID = MFVideoFormat_NV12.into();

    unsafe {
        attrs.SetGUID(&MF_MT_MAJOR_TYPE, &major)?;
        attrs.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        // MF_MT_FRAME_SIZE: (width << 32) | height
        attrs.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | (height as u64))?;
        // MF_MT_FRAME_RATE: (numerator << 32) | denominator
        attrs.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1u64)?;
        attrs.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }

    Ok(mt)
}

fn create_sample_from_texture(
    texture: &ID3D11Texture2D,
    timestamp_us: i64,
    fps: u32,
) -> Result<IMFSample, MftEncoderError> {
    use windows::core::IUnknown;

    let unknown: IUnknown = texture.cast()?;
    let buffer = unsafe {
        MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &unknown, 0, false)?
    };

    let sample = unsafe { MFCreateSample()? };
    unsafe {
        sample.AddBuffer(&buffer)?;
        // Timestamp in 100-nanosecond units
        sample.SetSampleTime(timestamp_us * 10)?;
        // Duration: 1 sec / fps in 100ns
        let duration = 10_000_000 / fps as i64;
        sample.SetSampleDuration(duration)?;
    }

    Ok(sample)
}

fn extract_h264_from_sample(
    sample: &IMFSample,
    output_buf: &mut Vec<u8>,
    original_timestamp_us: i64,
) -> Result<Option<EncodedFrame>, MftEncoderError> {
    let buffer_count = unsafe { sample.GetBufferCount()? };
    if buffer_count == 0 {
        return Ok(None);
    }

    let buffer = unsafe { sample.GetBufferByIndex(0)? };

    let mut data_ptr: *mut u8 = ptr::null_mut();
    let mut max_len: u32 = 0;
    let mut cur_len: u32 = 0;
    unsafe {
        buffer.Lock(&mut data_ptr, Some(&mut max_len), Some(&mut cur_len))?;
    }
    if data_ptr.is_null() || cur_len == 0 {
        unsafe { buffer.Unlock()? };
        return Ok(None);
    }

    let data = unsafe { std::slice::from_raw_parts(data_ptr, cur_len as usize) };
    output_buf.clear();
    output_buf.extend_from_slice(data);
    unsafe { buffer.Unlock()? };

    let key_frame = is_key_frame_sample(sample, output_buf);

    Ok(Some(EncodedFrame {
        data: output_buf.clone(),
        timestamp_us: original_timestamp_us,
        key_frame,
    }))
}

fn is_key_frame_sample(sample: &IMFSample, annex_b: &[u8]) -> bool {
    if contains_idr_nal(annex_b) {
        return true;
    }
    use windows::Win32::Media::MediaFoundation::MFSampleExtension_VideoEncodePictureType;

    unsafe {
        let attrs: &IMFAttributes = sample;
        if let Ok(pt) = attrs.GetUINT32(&MFSampleExtension_VideoEncodePictureType) {
            pt == 0 // 0 = I-frame (PictureType_I); fallback only when Annex B parse failed.
        } else {
            false
        }
    }
}

fn contains_idr_nal(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 4 < data.len() {
        let start_len = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            3usize
        } else if i + 5 < data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            4usize
        } else {
            i += 1;
            continue;
        };
        let nal_header_idx = i + start_len;
        if nal_header_idx < data.len() {
            let nal_type = data[nal_header_idx] & 0x1f;
            if nal_type == 5 {
                return true;
            }
        }
        i = nal_header_idx.saturating_add(1);
    }
    false
}
