//! Probe how many input frames the H.264 encoder MFT can keep in flight.
//!
//! This bypasses the Astrix screen-share client path and talks to the encoder
//! MFT directly with pre-created NV12 D3D11 textures.
//!
//! Run:
//!   cargo run --example mft_inflight_probe
//!
//! Useful env vars:
//!   ASTRIX_MFT_PROBE_WIDTH=2560
//!   ASTRIX_MFT_PROBE_HEIGHT=1440
//!   ASTRIX_MFT_PROBE_FPS=90
//!   ASTRIX_MFT_PROBE_BITRATE=24000000
//!   ASTRIX_MFT_PROBE_FRAMES=64
//!   ASTRIX_MFT_PROBE_TEXTURES=4
//!   ASTRIX_MFT_PROBE_WAIT_MS=80
//!   ASTRIX_MFT_PROBE_MODE=wave|pair|latency
//!   ASTRIX_MFT_SOFTWARE=1

#![cfg(all(windows, feature = "wgc-capture"))]

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::time::Instant;

use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    ICodecAPI, IMFActivate, IMFAttributes, IMFMediaBuffer, IMFMediaEventGenerator, IMFSample,
    IMFTransform, IMFDXGIDeviceManager, CODECAPI_AVEncCommonLowLatency,
    CODECAPI_AVEncMPVDefaultBPictureCount, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateSample, MFStartup, MFTEnumEx, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFMediaType_Video, MFVideoInterlace_Progressive,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_LOW_LATENCY as MF_LOW_LATENCY_ATTR, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

const CODECAPI_AVEncMPVGOPSize: GUID = GUID::from_u128(0x95f31b26_95a4_41aa_9303_246a7fc6eef1);
const CODECAPI_AVEncCommonRateControlMode: GUID =
    GUID::from_u128(0x1c0608e9_370c_4710_8a58_cb6181c42423);
const CODECAPI_AVEncCommonMeanBitRate: GUID =
    GUID::from_u128(0xf7222374_2144_4815_b550_a37f8e12ee52);
const CODECAPI_AVEncCommonQualityVsSpeed: GUID =
    GUID::from_u128(0x98332df8_03cd_476b_89fa_3f9e442dec9f);
const CODECAPI_AVEncH264CABACEnable: GUID =
    GUID::from_u128(0xee6cad62_d305_4248_a50e_e1b255f7caf6);
const CODECAPI_AVEncNumWorkerThreads: GUID =
    GUID::from_u128(0xb0e5b3a0_7c50_4b44_85a2_c48bed9a9640);
const CODECAPI_AVEncVideoIntraRefreshMode: GUID =
    GUID::from_u128(0xdc2f837c_f78a_4b9d_a8d4_2e76a337c0f0);
const CODECAPI_AVEncVideoEncodeSliceSizeControlMode: GUID =
    GUID::from_u128(0xa79e89a8_a437_4ee2_98dd_ed95e39b446c);

const ME_TRANSFORM_NEED_INPUT: u32 = 601;
const ME_TRANSFORM_HAVE_OUTPUT: u32 = 602;

#[derive(Default, Clone, Copy)]
struct EventBatch {
    need: u32,
    have: u32,
    found_target: bool,
}

#[derive(Default)]
struct ProbeStats {
    submitted: u32,
    drained: u32,
    outputs: u32,
    not_accepting: u32,
    stale_need_tokens: u32,
    max_inflight: u32,
    max_accepts_before_drain: u32,
    max_need_buffered: u32,
    max_have_buffered: u32,
    total_output_bytes: u64,
}

#[derive(Default)]
struct DrainResult {
    produced: bool,
    bytes: u32,
    timestamp_us: Option<i64>,
}

#[derive(Debug)]
enum SubmitState {
    Ok,
    NotAccepting,
}

#[derive(Debug)]
struct SubmitAttempt {
    state: SubmitState,
    submit_us: u64,
    need_seen: u32,
    have_seen: u32,
    need_buffered: u32,
    have_buffered: u32,
}

struct InflightFrame {
    frame_idx: u32,
    timestamp_us: i64,
    first_attempt_at: Instant,
    accepted_at: Instant,
    accept_delay_us: u64,
    submit_call_us: u64,
    attempts: u32,
}

struct LatencyResult {
    frame_idx: u32,
    timestamp_us: i64,
    bytes: u32,
    attempts: u32,
    accept_delay_us: u64,
    submit_call_us: u64,
    accept_to_output_us: u64,
    total_us: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let width = env_u32("ASTRIX_MFT_PROBE_WIDTH", 2560);
    let height = env_u32("ASTRIX_MFT_PROBE_HEIGHT", 1440);
    let fps = env_u32("ASTRIX_MFT_PROBE_FPS", 90);
    let bitrate = env_u32("ASTRIX_MFT_PROBE_BITRATE", 24_000_000);
    let frames = env_u32("ASTRIX_MFT_PROBE_FRAMES", 64);
    let textures = env_u32("ASTRIX_MFT_PROBE_TEXTURES", 4).max(1);
    let wait_ms = env_u32("ASTRIX_MFT_PROBE_WAIT_MS", 80);
    let mode = std::env::var("ASTRIX_MFT_PROBE_MODE").unwrap_or_else(|_| "wave".into());

    println!("=== MFT In-Flight Probe ===");
    println!(
        "settings: {}x{} @ {} fps, bitrate={} bps, frames={}, textures={}, wait_ms={}, mode={}",
        width, height, fps, bitrate, frames, textures, wait_ms, mode
    );
    if std::env::var("ASTRIX_MFT_SOFTWARE").is_ok() {
        println!("ASTRIX_MFT_SOFTWARE=1");
    }

    let gpu = astrix_client::gpu_device::GpuDevice::select_best()?;
    println!("gpu: {}", gpu.adapter_name);

    let mut probe = ProbeMft::new(&gpu.device, width, height, fps, bitrate)?;
    println!(
        "encoder: {} ({}, async={})",
        probe.encoder_name,
        if probe.is_hardware { "hardware" } else { "software" },
        probe.event_gen.is_some()
    );

    if probe.event_gen.is_none() {
        println!("WARNING: selected MFT is sync; in-flight probe is primarily for async MFTs.");
    }

    let textures = create_nv12_textures(&gpu.device, width, height, textures)?;
    if mode.eq_ignore_ascii_case("pair") {
        probe.run_pair_probe(&textures, wait_ms)?;
        return Ok(());
    }
    if mode.eq_ignore_ascii_case("latency") {
        probe.run_latency_probe(&textures, frames, wait_ms)?;
        return Ok(());
    }

    let stats = probe.run(&textures, frames, wait_ms)?;

    println!("\n=== Summary ===");
    println!("submitted={}", stats.submitted);
    println!("drained={}", stats.drained);
    println!("outputs={}", stats.outputs);
    println!("not_accepting={}", stats.not_accepting);
    println!("stale_need_tokens={}", stats.stale_need_tokens);
    println!("max_inflight={}", stats.max_inflight);
    println!("max_accepts_before_drain={}", stats.max_accepts_before_drain);
    println!("max_need_buffered={}", stats.max_need_buffered);
    println!("max_have_buffered={}", stats.max_have_buffered);
    println!("total_output_bytes={}", stats.total_output_bytes);

    Ok(())
}

struct ProbeMft {
    transform: IMFTransform,
    _device_manager: IMFDXGIDeviceManager,
    event_gen: Option<IMFMediaEventGenerator>,
    encoder_name: String,
    is_hardware: bool,
    fps: u32,
    need_buffered: u32,
    have_buffered: u32,
}

impl ProbeMft {
    fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, 0);
        }

        let mut reset_token = 0u32;
        let mut device_manager = None;
        unsafe {
            MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager)?;
        }
        let device_manager = device_manager.ok_or("MFCreateDXGIDeviceManager returned null")?;
        unsafe {
            device_manager.ResetDevice(device, reset_token)?;
        }

        let (transform, encoder_name, is_hardware) = create_mft(device, &device_manager)?;
        if is_hardware {
            set_output_type_from_available(&transform, width, height, fps, bitrate_bps)?;
            set_input_type_from_available(&transform, width, height, fps)?;
        } else {
            let mt_out = create_output_media_type(width, height, fps, bitrate_bps)?;
            unsafe { transform.SetOutputType(0, &mt_out, 0)?; }
            let mt_in = create_input_media_type(width, height, fps)?;
            unsafe { transform.SetInputType(0, &mt_in, 0)?; }
        }

        unsafe {
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY_ATTR, 1);
            }
        }

        if let Ok(codec) = transform.cast::<ICodecAPI>() {
            let gop_secs = std::env::var("ASTRIX_MFT_GOP_SECS")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(12)
                .max(1);
            let gop = fps.saturating_mul(gop_secs);
            unsafe {
                let _ = codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
                let _ = codec.SetValue(&CODECAPI_AVEncCommonLowLatency, &VARIANT::from(1u32));
                let rc_mode = if fps >= 60 { 0u32 } else { 4u32 };
                let rc_ok = codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(rc_mode));
                if rc_ok.is_err() {
                    let _ = codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(0u32));
                }
                let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate_bps));
                let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(gop));
                let _ = codec.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &VARIANT::from(100u32));
                let _ = codec.SetValue(&CODECAPI_AVEncH264CABACEnable, &VARIANT::from(0u32));
                let _ = codec.SetValue(&CODECAPI_AVEncNumWorkerThreads, &VARIANT::from(4u32));
                let _ = codec.SetValue(&CODECAPI_AVEncVideoIntraRefreshMode, &VARIANT::from(2u32));
                let skip_constrained = std::env::var("ASTRIX_MFT_SLICE_CONSTRAINED")
                    .map(|v| v == "0" || v.eq_ignore_ascii_case("false"))
                    .unwrap_or(false);
                if !skip_constrained {
                    let _ = codec.SetValue(
                        &CODECAPI_AVEncVideoEncodeSliceSizeControlMode,
                        &VARIANT::from(2u32),
                    );
                }
            }
        }

        let event_gen: Option<IMFMediaEventGenerator> = transform.cast().ok();

        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        Ok(Self {
            transform,
            _device_manager: device_manager,
            event_gen,
            encoder_name,
            is_hardware,
            fps,
            need_buffered: 0,
            have_buffered: 0,
        })
    }

    fn run(
        &mut self,
        textures: &[ID3D11Texture2D],
        total_frames: u32,
        wait_ms: u32,
    ) -> Result<ProbeStats, Box<dyn std::error::Error>> {
        let mut stats = ProbeStats::default();
        let mut next_frame: u32 = 0;
        let mut first_unconditional_submit = true;

        self.merge_events(self.poll_events_no_wait()?);
        stats.max_need_buffered = stats.max_need_buffered.max(self.need_buffered);
        stats.max_have_buffered = stats.max_have_buffered.max(self.have_buffered);

        let mut wave: u32 = 0;
        while next_frame < total_frames {
            wave += 1;
            let mut accepted_in_wave: u32 = 0;

            loop {
                let used_need_token = if self.need_buffered > 0 {
                    self.need_buffered -= 1;
                    true
                } else if first_unconditional_submit {
                    first_unconditional_submit = false;
                    false
                } else {
                    break;
                };

                let timestamp_us = (next_frame as i64) * 1_000_000 / self.fps as i64;
                let sample = create_sample_from_texture(
                    &textures[(next_frame as usize) % textures.len()],
                    timestamp_us,
                    self.fps,
                )?;

                match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                    Ok(()) => {
                        stats.submitted += 1;
                        accepted_in_wave += 1;
                        next_frame += 1;
                        stats.max_inflight = stats.max_inflight.max(stats.submitted - stats.drained);
                        self.merge_events(self.poll_events_no_wait()?);
                        stats.max_need_buffered = stats.max_need_buffered.max(self.need_buffered);
                        stats.max_have_buffered = stats.max_have_buffered.max(self.have_buffered);
                    }
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        stats.not_accepting += 1;
                        if used_need_token {
                            stats.stale_need_tokens += 1;
                        }
                        break;
                    }
                    Err(e) => {
                        return Err(format!("ProcessInput failed at frame {}: {:?}", next_frame, e).into());
                    }
                }

                if next_frame >= total_frames {
                    break;
                }
            }

            stats.max_accepts_before_drain =
                stats.max_accepts_before_drain.max(accepted_in_wave);

            println!(
                "wave {:>3}: accepted={} inflight={} need_buf={} have_buf={} submitted={} drained={}",
                wave,
                accepted_in_wave,
                stats.submitted.saturating_sub(stats.drained),
                self.need_buffered,
                self.have_buffered,
                stats.submitted,
                stats.drained,
            );

            if next_frame >= total_frames {
                break;
            }

            if self.have_buffered == 0 {
                let batch = self.wait_for_target(ME_TRANSFORM_HAVE_OUTPUT, wait_ms)?;
                self.merge_events(batch);
                stats.max_need_buffered = stats.max_need_buffered.max(self.need_buffered);
                stats.max_have_buffered = stats.max_have_buffered.max(self.have_buffered);
            }

            if self.have_buffered == 0 {
                println!("timeout waiting for METransformHaveOutput after wave {}", wave);
                break;
            }

            self.have_buffered -= 1;
            let drained = self.drain_one_output()?;
            if drained.produced {
                stats.drained += 1;
                stats.outputs += 1;
                stats.total_output_bytes += drained.bytes as u64;
            }
            self.merge_events(self.poll_events_no_wait()?);
            stats.max_need_buffered = stats.max_need_buffered.max(self.need_buffered);
            stats.max_have_buffered = stats.max_have_buffered.max(self.have_buffered);
        }

        while self.have_buffered > 0 {
            self.have_buffered -= 1;
            let drained = self.drain_one_output()?;
            if drained.produced {
                stats.drained += 1;
                stats.outputs += 1;
                stats.total_output_bytes += drained.bytes as u64;
            }
            self.merge_events(self.poll_events_no_wait()?);
            stats.max_need_buffered = stats.max_need_buffered.max(self.need_buffered);
            stats.max_have_buffered = stats.max_have_buffered.max(self.have_buffered);
        }

        Ok(stats)
    }

    fn run_pair_probe(
        &mut self,
        textures: &[ID3D11Texture2D],
        wait_ms: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let frame_interval_us = 1_000_000i64 / self.fps.max(1) as i64;
        let tex_f1 = &textures[0];
        let tex_f2 = &textures[(textures.len() > 1) as usize];

        self.need_buffered = 0;
        self.have_buffered = 0;
        self.merge_events(self.poll_events_no_wait()?);

        println!("\n=== Pair Depth Probe ===");

        let f1 = self.submit_attempt(tex_f1, 0)?;
        print_submit_attempt("F1", &f1);

        let f2_immediate = self.submit_attempt(tex_f2, frame_interval_us)?;
        print_submit_attempt("F2 immediate", &f2_immediate);

        if matches!(f2_immediate.state, SubmitState::NotAccepting) {
            if self.have_buffered == 0 {
                let batch = self.wait_for_target(ME_TRANSFORM_HAVE_OUTPUT, wait_ms)?;
                println!(
                    "wait HaveOutput: found={} need_seen={} have_seen={} need_buf={} have_buf={}",
                    batch.found_target,
                    batch.need,
                    batch.have,
                    self.need_buffered.saturating_add(batch.need),
                    self.have_buffered.saturating_add(batch.have),
                );
                self.merge_events(batch);
            } else {
                println!(
                    "wait HaveOutput: skipped (already buffered), need_buf={} have_buf={}",
                    self.need_buffered,
                    self.have_buffered,
                );
            }

            if self.have_buffered > 0 {
                self.have_buffered -= 1;
                let drained = self.drain_one_output()?;
                println!(
                    "ProcessOutput: produced={} bytes={} ts_us={:?} need_buf={} have_buf={}",
                    drained.produced,
                    drained.bytes,
                    drained.timestamp_us,
                    self.need_buffered,
                    self.have_buffered,
                );
            } else {
                println!("ProcessOutput: skipped (no buffered HaveOutput)");
            }

            let f2_retry = self.submit_attempt(tex_f2, frame_interval_us)?;
            print_submit_attempt("F2 retry", &f2_retry);

            match f2_retry.state {
                SubmitState::Ok => println!("\nPair probe result: effective in-flight depth is 1 frame."),
                SubmitState::NotAccepting => {
                    println!("\nPair probe result: even after one ProcessOutput, F2 is still NOT_ACCEPTING.")
                }
            }
        } else {
            println!("\nPair probe result: F2 was accepted immediately, so effective in-flight depth is at least 2.");
        }

        Ok(())
    }

    fn run_latency_probe(
        &mut self,
        textures: &[ID3D11Texture2D],
        total_frames: u32,
        wait_ms: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let frame_interval_us = 1_000_000i64 / self.fps.max(1) as i64;
        let probe_start = Instant::now();
        let mut inflight: HashMap<i64, InflightFrame> = HashMap::new();
        let mut results: Vec<LatencyResult> = Vec::with_capacity(total_frames as usize);
        let mut frame_idx = 0u32;

        self.need_buffered = 0;
        self.have_buffered = 0;
        self.merge_events(self.poll_events_no_wait()?);

        println!("\n=== Latency Probe ===");
        println!(
            "Submitting {} frame(s) and measuring per-frame latency from first submit attempt to ProcessOutput.",
            total_frames
        );

        while frame_idx < total_frames {
            let timestamp_us = frame_idx as i64 * frame_interval_us;
            let first_attempt_at = Instant::now();
            let mut attempts = 0u32;

            loop {
                attempts += 1;
                let sample = create_sample_from_texture(
                    &textures[(frame_idx as usize) % textures.len()],
                    timestamp_us,
                    self.fps,
                )?;
                let submit_start = Instant::now();
                let submit_result = unsafe { self.transform.ProcessInput(0, &sample, 0) };
                let submit_call_us = submit_start.elapsed().as_micros() as u64;

                match submit_result {
                    Ok(()) => {
                        let accepted_at = Instant::now();
                        let accept_delay_us =
                            accepted_at.duration_since(first_attempt_at).as_micros() as u64;
                        inflight.insert(
                            timestamp_us,
                            InflightFrame {
                                frame_idx,
                                timestamp_us,
                                first_attempt_at,
                                accepted_at,
                                accept_delay_us,
                                submit_call_us,
                                attempts,
                            },
                        );
                        self.merge_events(self.poll_events_no_wait()?);
                        frame_idx += 1;
                        break;
                    }
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        self.merge_events(self.poll_events_no_wait()?);
                        let drained =
                            self.wait_and_drain_latency_output(wait_ms, &mut inflight)?;
                        print_latency_result(&drained);
                        results.push(drained);
                    }
                    Err(e) => {
                        return Err(
                            format!("ProcessInput failed at frame {}: {:?}", frame_idx, e).into(),
                        );
                    }
                }
            }
        }

        while !inflight.is_empty() {
            let drained = self.wait_and_drain_latency_output(wait_ms, &mut inflight)?;
            print_latency_result(&drained);
            results.push(drained);
        }

        results.sort_by_key(|r| r.frame_idx);
        print_latency_summary(&results, probe_start.elapsed().as_micros() as u64);
        Ok(())
    }

    fn merge_events(&mut self, batch: EventBatch) {
        self.need_buffered = self.need_buffered.saturating_add(batch.need);
        self.have_buffered = self.have_buffered.saturating_add(batch.have);
    }

    fn poll_events_no_wait(&self) -> Result<EventBatch, Box<dyn std::error::Error>> {
        let Some(event_gen) = self.event_gen.as_ref() else {
            return Ok(EventBatch::default());
        };
        let mut batch = EventBatch::default();
        loop {
            let event = match unsafe { get_event_no_wait(event_gen) } {
                Ok(e) => e,
                Err(_) => return Ok(batch),
            };
            let event_type = unsafe { event.GetType() }.unwrap_or(0);
            match event_type {
                ME_TRANSFORM_NEED_INPUT => batch.need += 1,
                ME_TRANSFORM_HAVE_OUTPUT => batch.have += 1,
                _ => {}
            }
        }
    }

    fn wait_for_target(
        &self,
        target_type: u32,
        timeout_ms: u32,
    ) -> Result<EventBatch, Box<dyn std::error::Error>> {
        let Some(event_gen) = self.event_gen.as_ref() else {
            return Ok(EventBatch::default());
        };

        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
        let mut batch = EventBatch::default();
        loop {
            if let Ok(event) = unsafe { get_event_no_wait(event_gen) } {
                let event_type = unsafe { event.GetType() }.unwrap_or(0);
                match event_type {
                    ME_TRANSFORM_NEED_INPUT => batch.need += 1,
                    ME_TRANSFORM_HAVE_OUTPUT => batch.have += 1,
                    _ => {}
                }
                if event_type == target_type {
                    batch.found_target = true;
                    return Ok(batch);
                }
                continue;
            }

            if std::time::Instant::now() >= deadline {
                return Ok(batch);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn drain_one_output(&mut self) -> Result<DrainResult, Box<dyn std::error::Error>> {
        let mut output_buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut output_buffers = [output_buffer];
        let mut status: u32 = 0;

        let result = unsafe { self.transform.ProcessOutput(0, &mut output_buffers, &mut status) };
        match result {
            Ok(()) => {
                let sample = unsafe { ManuallyDrop::take(&mut output_buffers[0].pSample) };
                let _events = unsafe { ManuallyDrop::take(&mut output_buffers[0].pEvents) };
                let Some(sample) = sample else {
                    return Ok(DrainResult::default());
                };
                Ok(DrainResult {
                    produced: true,
                    bytes: sample_total_len(&sample).unwrap_or(0),
                    timestamp_us: unsafe { sample.GetSampleTime().ok().map(|v| v / 10) },
                })
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(DrainResult::default()),
            Err(e) => Err(format!("ProcessOutput failed: {:?}", e).into()),
        }
    }

    fn submit_attempt(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp_us: i64,
    ) -> Result<SubmitAttempt, Box<dyn std::error::Error>> {
        let sample = create_sample_from_texture(texture, timestamp_us, self.fps)?;
        let start = std::time::Instant::now();
        let result = unsafe { self.transform.ProcessInput(0, &sample, 0) };
        let submit_us = start.elapsed().as_micros() as u64;
        let batch = self.poll_events_no_wait()?;
        self.merge_events(batch);

        let state = match result {
            Ok(()) => SubmitState::Ok,
            Err(e) if e.code() == MF_E_NOTACCEPTING => SubmitState::NotAccepting,
            Err(e) => return Err(format!("ProcessInput failed: {:?}", e).into()),
        };

        Ok(SubmitAttempt {
            state,
            submit_us,
            need_seen: batch.need,
            have_seen: batch.have,
            need_buffered: self.need_buffered,
            have_buffered: self.have_buffered,
        })
    }

    fn wait_and_drain_latency_output(
        &mut self,
        wait_ms: u32,
        inflight: &mut HashMap<i64, InflightFrame>,
    ) -> Result<LatencyResult, Box<dyn std::error::Error>> {
        if self.have_buffered == 0 {
            let batch = self.wait_for_target(ME_TRANSFORM_HAVE_OUTPUT, wait_ms)?;
            self.merge_events(batch);
        }

        if self.have_buffered == 0 {
            return Err(format!(
                "timeout waiting for METransformHaveOutput with {} frame(s) inflight",
                inflight.len()
            )
            .into());
        }

        self.have_buffered -= 1;
        let output_at = Instant::now();
        let drained = self.drain_one_output()?;
        self.merge_events(self.poll_events_no_wait()?);

        if !drained.produced {
            return Err("ProcessOutput returned no sample while HaveOutput was buffered".into());
        }

        let timestamp_us = drained
            .timestamp_us
            .ok_or("ProcessOutput sample does not have timestamp")?;
        let frame = inflight
            .remove(&timestamp_us)
            .ok_or_else(|| format!("No inflight frame entry for ts_us={}", timestamp_us))?;

        Ok(LatencyResult {
            frame_idx: frame.frame_idx,
            timestamp_us: frame.timestamp_us,
            bytes: drained.bytes,
            attempts: frame.attempts,
            accept_delay_us: frame.accept_delay_us,
            submit_call_us: frame.submit_call_us,
            accept_to_output_us: output_at.duration_since(frame.accepted_at).as_micros() as u64,
            total_us: output_at.duration_since(frame.first_attempt_at).as_micros() as u64,
        })
    }
}

fn print_submit_attempt(label: &str, attempt: &SubmitAttempt) {
    println!(
        "{}: state={:?} submit_us={} need_seen={} have_seen={} need_buf={} have_buf={}",
        label,
        attempt.state,
        attempt.submit_us,
        attempt.need_seen,
        attempt.have_seen,
        attempt.need_buffered,
        attempt.have_buffered,
    );
}

fn print_latency_result(result: &LatencyResult) {
    println!(
        "frame {:>3}: ts_us={:<8} bytes={:<7} attempts={} accept_delay_us={:<7} submit_call_us={:<6} accept_to_output_us={:<7} total_us={}",
        result.frame_idx,
        result.timestamp_us,
        result.bytes,
        result.attempts,
        result.accept_delay_us,
        result.submit_call_us,
        result.accept_to_output_us,
        result.total_us,
    );
}

fn print_latency_summary(results: &[LatencyResult], wall_total_us: u64) {
    if results.is_empty() {
        println!("\n=== Latency Summary ===");
        println!("no output frames produced");
        return;
    }

    let mut min_total = u64::MAX;
    let mut max_total = 0u64;
    let mut sum_total = 0u128;

    let mut min_accept_to_output = u64::MAX;
    let mut max_accept_to_output = 0u64;
    let mut sum_accept_to_output = 0u128;

    let mut min_accept_delay = u64::MAX;
    let mut max_accept_delay = 0u64;
    let mut sum_accept_delay = 0u128;

    let mut max_attempts = 0u32;
    let mut total_bytes = 0u64;

    for r in results {
        min_total = min_total.min(r.total_us);
        max_total = max_total.max(r.total_us);
        sum_total += r.total_us as u128;

        min_accept_to_output = min_accept_to_output.min(r.accept_to_output_us);
        max_accept_to_output = max_accept_to_output.max(r.accept_to_output_us);
        sum_accept_to_output += r.accept_to_output_us as u128;

        min_accept_delay = min_accept_delay.min(r.accept_delay_us);
        max_accept_delay = max_accept_delay.max(r.accept_delay_us);
        sum_accept_delay += r.accept_delay_us as u128;

        max_attempts = max_attempts.max(r.attempts);
        total_bytes = total_bytes.saturating_add(r.bytes as u64);
    }

    let count = results.len() as u128;
    let avg_total = (sum_total / count) as u64;
    let avg_accept_to_output = (sum_accept_to_output / count) as u64;
    let avg_accept_delay = (sum_accept_delay / count) as u64;
    let throughput_fps = if wall_total_us > 0 {
        (results.len() as f64) * 1_000_000.0 / wall_total_us as f64
    } else {
        0.0
    };

    println!("\n=== Latency Summary ===");
    println!("frames={}", results.len());
    println!("wall_total_us={}", wall_total_us);
    println!("throughput_fps={:.2}", throughput_fps);
    println!(
        "accept_to_output_us: avg={} min={} max={}",
        avg_accept_to_output, min_accept_to_output, max_accept_to_output
    );
    println!(
        "accept_delay_us: avg={} min={} max={}",
        avg_accept_delay, min_accept_delay, max_accept_delay
    );
    println!(
        "total_us: avg={} min={} max={}",
        avg_total, min_total, max_total
    );
    println!("max_attempts={}", max_attempts);
    println!("total_output_bytes={}", total_bytes);
}

unsafe fn get_event_no_wait(
    event_gen: &IMFMediaEventGenerator,
) -> windows::core::Result<windows::Win32::Media::MediaFoundation::IMFMediaEvent> {
    event_gen.GetEvent(
        windows::Win32::Media::MediaFoundation::MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(1),
    )
}

fn sample_total_len(sample: &IMFSample) -> Result<u32, Box<dyn std::error::Error>> {
    let buffer_count = unsafe { sample.GetBufferCount()? };
    let mut total = 0u32;
    for idx in 0..buffer_count {
        let buffer: IMFMediaBuffer = unsafe { sample.GetBufferByIndex(idx)? };
        total = total.saturating_add(unsafe { buffer.GetCurrentLength()? });
    }
    Ok(total)
}

fn create_mft(
    _device: &ID3D11Device,
    device_manager: &IMFDXGIDeviceManager,
) -> Result<(IMFTransform, String, bool), Box<dyn std::error::Error>> {
    let prefer_software = std::env::var("ASTRIX_MFT_SOFTWARE").is_ok();
    let input_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video.into(),
        guidSubtype: MFVideoFormat_NV12.into(),
    };
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video.into(),
        guidSubtype: MFVideoFormat_H264.into(),
    };

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

                unsafe {
                    let attrs: &IMFAttributes = act;
                    let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                }

                let transform: IMFTransform = match unsafe { act.ActivateObject::<IMFTransform>() } {
                    Ok(t) => t,
                    Err(_) => continue,
                };

                unsafe {
                    if let Ok(attrs) = transform.GetAttributes() {
                        let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                    }
                }

                let ptr = unsafe { device_manager.as_raw() as usize };
                let accepts_d3d =
                    unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, ptr).is_ok() };
                if accepts_d3d {
                    unsafe { CoTaskMemFree(Some(activates as *const _ as *mut _)) };
                    return Ok((transform, name, true));
                }

                unsafe {
                    let _ = act.ShutdownObject();
                }
            }

            unsafe { CoTaskMemFree(Some(activates as *const _ as *mut _)) };
        }
    }

    let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count: u32 = 0;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT,
            Some(&input_type as *const _),
            Some(&output_type as *const _),
            &mut activates,
            &mut count,
        )?;
    }
    if count == 0 {
        return Err("No NV12->H264 MFT encoder found".into());
    }
    let activates_slice = unsafe { std::slice::from_raw_parts(activates, count as usize) };
    let act = activates_slice
        .first()
        .and_then(|v| v.as_ref())
        .ok_or("MFTEnumEx returned empty activate list")?;
    let name = get_friendly_name(act).unwrap_or_else(|_| "Software MFT".to_string());
    let transform = unsafe { act.ActivateObject::<IMFTransform>()? };
    unsafe { CoTaskMemFree(Some(activates as *const _ as *mut _)) };
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

fn set_output_type_from_available(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
) -> Result<(), Box<dyn std::error::Error>> {
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
        if !subtype.map(|g| g == h264_guid).unwrap_or(false) {
            continue;
        }
        unsafe {
            let _ = attrs.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps);
            let _ = attrs.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64);
            let _ = attrs.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1u64);
            let _ = attrs.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
            if transform.SetOutputType(0, &mt, 0).is_ok() {
                return Ok(());
            }
        }
    }
    let mt = create_output_media_type(width, height, fps, bitrate_bps)?;
    unsafe { transform.SetOutputType(0, &mt, 0)?; }
    Ok(())
}

fn set_input_type_from_available(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<(), Box<dyn std::error::Error>> {
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
        if !subtype.map(|g| g == nv12_guid).unwrap_or(false) {
            continue;
        }
        unsafe {
            let _ = attrs.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64);
            let _ = attrs.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1u64);
            let _ = attrs.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
            if transform.SetInputType(0, &mt, 0).is_ok() {
                return Ok(());
            }
        }
    }
    let mt = create_input_media_type(width, height, fps)?;
    unsafe { transform.SetInputType(0, &mt, 0)?; }
    Ok(())
}

fn create_output_media_type(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
) -> Result<windows::Win32::Media::MediaFoundation::IMFMediaType, Box<dyn std::error::Error>> {
    let mt = unsafe { MFCreateMediaType()? };
    let attrs: &IMFAttributes = &mt;
    unsafe {
        attrs.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video.into())?;
        attrs.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264.into())?;
        attrs.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
        attrs.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64)?;
        attrs.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1u64)?;
        attrs.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    Ok(mt)
}

fn create_input_media_type(
    width: u32,
    height: u32,
    fps: u32,
) -> Result<windows::Win32::Media::MediaFoundation::IMFMediaType, Box<dyn std::error::Error>> {
    let mt = unsafe { MFCreateMediaType()? };
    let attrs: &IMFAttributes = &mt;
    unsafe {
        attrs.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video.into())?;
        attrs.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12.into())?;
        attrs.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64)?;
        attrs.SetUINT64(&MF_MT_FRAME_RATE, (fps as u64) << 32 | 1u64)?;
        attrs.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    Ok(mt)
}

fn create_nv12_textures(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    count: u32,
) -> Result<Vec<ID3D11Texture2D>, Box<dyn std::error::Error>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let frame_bytes = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(3)
        / 2;
    let init_bytes = vec![0u8; frame_bytes];
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: init_bytes.as_ptr() as *const c_void,
        SysMemPitch: width,
        SysMemSlicePitch: frame_bytes as u32,
    };

    let mut textures = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut tex = None;
        unsafe {
            device.CreateTexture2D(&desc, Some(&init), Some(&mut tex))?;
        }
        textures.push(tex.ok_or("CreateTexture2D returned null")?);
    }
    Ok(textures)
}

fn create_sample_from_texture(
    texture: &ID3D11Texture2D,
    timestamp_us: i64,
    fps: u32,
) -> Result<IMFSample, Box<dyn std::error::Error>> {
    use windows::core::IUnknown;

    let unknown: IUnknown = texture.cast()?;
    let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &unknown, 0, false)? };
    let sample = unsafe { MFCreateSample()? };
    unsafe {
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(timestamp_us * 10)?;
        sample.SetSampleDuration(10_000_000 / fps as i64)?;
    }
    Ok(sample)
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(default)
}
