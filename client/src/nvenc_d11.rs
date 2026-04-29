//! Phase 2 NVENC D3D11 backend.
//!
//! This path keeps the existing sender contract intact:
//! - D3D11 textures in (NV12 or packed RGB)
//! - Annex B H.264 packets out
//! - async submit/collect semantics compatible with the current MFT path

#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use std::collections::VecDeque;
use std::env;

use cxx::UniquePtr;
use thiserror::Error;
use windows::core::{w, Interface};
use windows::Win32::Foundation::FreeLibrary;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
};
use windows::Win32::System::LibraryLoader::LoadLibraryW;

use crate::gpu_device::{GpuDevice, GpuDeviceError};
use crate::mft_encoder::EncodedFrame;
use crate::nvenc_d11_bridge::ffi;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Unknown(u32),
}

impl GpuVendor {
    pub fn from_vendor_id(vendor_id: u32) -> Self {
        match vendor_id {
            0x10DE => Self::Nvidia,
            0x1002 | 0x1022 => Self::Amd,
            0x8086 => Self::Intel,
            other => Self::Unknown(other),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nvidia => "NVIDIA",
            Self::Amd => "AMD",
            Self::Intel => "Intel",
            Self::Unknown(_) => "Unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NvencProbe {
    pub adapter_name: String,
    pub vendor: GpuVendor,
    pub vendor_id: u32,
    pub device_id: u32,
    pub runtime_present: bool,
}

impl NvencProbe {
    pub fn is_nvidia(&self) -> bool {
        self.vendor == GpuVendor::Nvidia
    }
}

#[derive(Debug, Error)]
pub enum NvencD3d11Error {
    #[error("DXGI adapter probe failed: {0}")]
    Adapter(#[from] GpuDeviceError),
    #[error("NVENC D3D11 is only enabled for NVIDIA adapters (got {vendor})")]
    UnsupportedVendor { vendor: String },
    #[error("nvEncodeAPI64.dll not found")]
    RuntimeMissing,
    #[error("NVENC D3D11 bridge error: {0}")]
    Bridge(#[from] cxx::Exception),
    #[error("NVENC D3D11 requires a non-empty input ring")]
    EmptyInputRing,
    #[error("NVENC D3D11 session returned output without matching frame metadata")]
    MissingFrameMeta,
    #[error("NVENC D3D11 queue stayed full (pending={pending}, ring={ring_size})")]
    QueueFull { pending: usize, ring_size: usize },
}

impl NvencD3d11Error {
    pub fn should_fallback_to_mft(&self) -> bool {
        !matches!(self, Self::QueueFull { .. })
            && !matches!(self, Self::Bridge(err) if is_nvenc_backpressure_message(&err.to_string()))
    }
}

fn is_nvenc_backpressure_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("queue is full")
        || lower.contains("queue stayed full")
        || lower.contains("output slot is still reserved")
        || lower.contains("output slot became busy")
}

#[derive(Debug)]
struct FrameMeta {
    timestamp_us: i64,
    rtp_ts: u32,
    capture_us: i64,
    requested_key_frame: bool,
}

#[derive(Debug)]
struct PendingOutput {
    frame: EncodedFrame,
    rtp_ts: u32,
    capture_us: i64,
    encode_us: u64,
}

pub struct NvencD3d11Encoder {
    probe: NvencProbe,
    encoder_name: String,
    session: UniquePtr<ffi::NvencD3D11Session>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    async_encode: bool,
    input_ring_size: usize,
    uses_rgb_input: bool,
    meta_queue: VecDeque<FrameMeta>,
    pending_outputs: VecDeque<PendingOutput>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NvencSubmitBreakdown {
    pub map_us: u64,
    pub encode_picture_us: u64,
    pub total_us: u64,
}

impl NvencD3d11Encoder {
    pub fn probe_device(device: &ID3D11Device) -> Result<NvencProbe, NvencD3d11Error> {
        let adapter = GpuDevice::adapter_info_from_device(device)?;
        Ok(NvencProbe {
            adapter_name: adapter.name.clone(),
            vendor: GpuVendor::from_vendor_id(adapter.vendor_id),
            vendor_id: adapter.vendor_id,
            device_id: adapter.device_id,
            runtime_present: nvenc_runtime_present(),
        })
    }

    pub fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        input_ring: &[ID3D11Texture2D],
    ) -> Result<Self, NvencD3d11Error> {
        if input_ring.is_empty() {
            return Err(NvencD3d11Error::EmptyInputRing);
        }

        let mut probe = Self::probe_device(device)?;
        if !probe.is_nvidia() {
            return Err(NvencD3d11Error::UnsupportedVendor {
                vendor: probe.vendor.as_str().to_string(),
            });
        }
        if !probe.runtime_present {
            return Err(NvencD3d11Error::RuntimeMissing);
        }

        let mut first_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { input_ring[0].GetDesc(&mut first_desc) };
        let uses_rgb_input = first_desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM.into()
            || first_desc.Format == DXGI_FORMAT_R8G8B8A8_UNORM.into();
        let texture_ptrs: Vec<usize> = input_ring.iter().map(texture_raw_ptr).collect();

        // Parse optional GIR (Gradual Intra Refresh) from environment variables
        let (gir_period, gir_duration) = parse_gir_env_vars();

        let session = ffi::nvenc_d3d11_create(
            device.as_raw() as usize,
            width,
            height,
            fps,
            bitrate,
            texture_ptrs,
            gir_period,
            gir_duration,
        )?;
        let session_ref = session
            .as_ref()
            .expect("nvenc_d3d11_create returned a null session");
        let encoder_name = session_ref.encoder_name();
        let async_encode = session_ref.is_async();
        let input_ring_size = session_ref.input_ring_size().max(1) as usize;
        if !encoder_name.is_empty() {
            probe.adapter_name = encoder_name.clone();
        }

        Ok(Self {
            probe,
            encoder_name,
            session,
            width,
            height,
            fps,
            bitrate,
            async_encode,
            input_ring_size,
            uses_rgb_input,
            meta_queue: VecDeque::new(),
            pending_outputs: VecDeque::new(),
        })
    }

    pub fn encoder_name(&self) -> &str {
        &self.encoder_name
    }

    pub fn probe(&self) -> &NvencProbe {
        &self.probe
    }

    pub fn is_hardware(&self) -> bool {
        true
    }

    pub fn is_async(&self) -> bool {
        self.async_encode
    }

    pub fn uses_rgb_input(&self) -> bool {
        self.uses_rgb_input
    }

    pub fn input_ring_size(&self) -> usize {
        self.input_ring_size
    }

    pub fn in_flight_count(&self) -> usize {
        self.session
            .as_ref()
            .map(|session| session.in_flight_count() as usize)
            .unwrap_or(0)
    }

    pub fn last_submit_breakdown(&self) -> NvencSubmitBreakdown {
        let Some(session) = self.session.as_ref() else {
            return NvencSubmitBreakdown::default();
        };
        NvencSubmitBreakdown {
            map_us: session.last_submit_map_us(),
            encode_picture_us: session.last_submit_encode_picture_us(),
            total_us: session.last_submit_total_us(),
        }
    }

    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        ts_us: i64,
        key_frame: bool,
    ) -> Result<Vec<EncodedFrame>, NvencD3d11Error> {
        let timeout_ms = self.output_timeout_ms(key_frame);
        self.submit(texture, ts_us, key_frame, 0, ts_us, timeout_ms)?;
        // Use blocking collect only for keyframes (startup), non-blocking for P-frames.
        // Keyframes are critical for stream startup - receiver needs the first IDR before decoding.
        if key_frame {
            match self.collect_blocking(timeout_ms)? {
                Some((frames, _, _, _)) => Ok(frames),
                None => Ok(Vec::new()),
            }
        } else {
            match self.collect()? {
                Some((frames, _, _, _)) => Ok(frames),
                None => Ok(Vec::new()),
            }
        }
    }

    pub fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        ts_us: i64,
        key_frame: bool,
        rtp_ts: u32,
        capture_us: i64,
        need_input_timeout_ms: u32,
    ) -> Result<(), NvencD3d11Error> {
        while self
            .session
            .as_ref()
            .map(|session| session.in_flight_count() as usize)
            .unwrap_or(0)
            >= self.input_ring_size
        {
            let collect_timeout_ms = if self.async_encode {
                // Only wait when the native ring is already full. A tiny grace
                // period lets the output worker harvest a just-completed packet
                // instead of surfacing a transient QueueFull under IDR/heavy scenes.
                1
            } else {
                need_input_timeout_ms.max(1)
            };
            match self.collect_impl(collect_timeout_ms)? {
                Some(output) => self.pending_outputs.push_back(output),
                None => {
                    return Err(NvencD3d11Error::QueueFull {
                        pending: self.meta_queue.len(),
                        ring_size: self.input_ring_size,
                    });
                }
            }
        }

        if let Err(err) = self.session.pin_mut().submit(texture_raw_ptr(texture), key_frame) {
            if is_nvenc_backpressure_message(&err.to_string()) {
                return Err(NvencD3d11Error::QueueFull {
                    pending: self.meta_queue.len(),
                    ring_size: self.input_ring_size,
                });
            }
            return Err(NvencD3d11Error::Bridge(err));
        }
        self.meta_queue.push_back(FrameMeta {
            timestamp_us: ts_us,
            rtp_ts,
            capture_us,
            requested_key_frame: key_frame,
        });
        Ok(())
    }

    pub fn collect(
        &mut self,
    ) -> Result<Option<(Vec<EncodedFrame>, u32, i64, u64)>, NvencD3d11Error> {
        if let Some(pending) = self.pending_outputs.pop_front() {
            return Ok(Some((
                vec![pending.frame],
                pending.rtp_ts,
                pending.capture_us,
                pending.encode_us,
            )));
        }
        self.collect_impl(0).map(map_pending_output)
    }

    pub fn collect_blocking(
        &mut self,
        timeout_ms: u32,
    ) -> Result<Option<(Vec<EncodedFrame>, u32, i64, u64)>, NvencD3d11Error> {
        if let Some(pending) = self.pending_outputs.pop_front() {
            return Ok(Some((
                vec![pending.frame],
                pending.rtp_ts,
                pending.capture_us,
                pending.encode_us,
            )));
        }
        self.collect_impl(timeout_ms).map(map_pending_output)
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), NvencD3d11Error> {
        if bitrate == 0 || bitrate == self.bitrate {
            return Ok(());
        }
        self.session.pin_mut().set_bitrate(bitrate)?;
        self.bitrate = bitrate;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn config(&self) -> (u32, u32, u32, u32) {
        (self.width, self.height, self.fps, self.bitrate)
    }

    fn collect_impl(&mut self, timeout_ms: u32) -> Result<Option<PendingOutput>, NvencD3d11Error> {
        let data = self.session.pin_mut().collect(timeout_ms)?;
        if data.is_empty() {
            return Ok(None);
        }

        let meta = self
            .meta_queue
            .pop_front()
            .ok_or(NvencD3d11Error::MissingFrameMeta)?;
        let encode_us = self
            .session
            .as_ref()
            .map(|s| s.last_encode_time_us())
            .unwrap_or(0);
        let key_frame = contains_idr_nal(&data) || meta.requested_key_frame;
        Ok(Some(PendingOutput {
            frame: EncodedFrame {
                data,
                timestamp_us: meta.timestamp_us,
                key_frame,
            },
            rtp_ts: meta.rtp_ts,
            capture_us: meta.capture_us,
            encode_us,
        }))
    }

    fn output_timeout_ms(&self, key_frame: bool) -> u32 {
        if key_frame {
            120
        } else {
            ((2_000u32 / self.fps.max(1)).clamp(8, 40)).max(8)
        }
    }
}

fn map_pending_output(
    pending: Option<PendingOutput>,
) -> Option<(Vec<EncodedFrame>, u32, i64, u64)> {
    pending.map(|pending| {
        (
            vec![pending.frame],
            pending.rtp_ts,
            pending.capture_us,
            pending.encode_us,
        )
    })
}

fn texture_raw_ptr(texture: &ID3D11Texture2D) -> usize {
    texture.as_raw() as usize
}

fn nvenc_runtime_present() -> bool {
    unsafe {
        if let Ok(module) = LoadLibraryW(w!("nvEncodeAPI64.dll")) {
            let _ = FreeLibrary(module);
            true
        } else {
            false
        }
    }
}

/// Parse optional GIR (Gradual Intra Refresh) parameters from environment variables.
///
/// GIR is disabled by default. To enable, set both:
/// - `ASTRIX_NVENC_GIR_PERIOD_FRAMES` - Number of frames between GIR cycles
/// - `ASTRIX_NVENC_GIR_DURATION_FRAMES` - Number of frames the refresh takes
///
/// Example: For 60fps with 2-second refresh cycle lasting 1 second:
/// - `ASTRIX_NVENC_GIR_PERIOD_FRAMES=120`
/// - `ASTRIX_NVENC_GIR_DURATION_FRAMES=60`
///
/// Returns `(period_frames, duration_frames)` - both 0 means GIR is disabled.
fn parse_gir_env_vars() -> (u32, u32) {
    let period: u32 = env::var("ASTRIX_NVENC_GIR_PERIOD_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let duration: u32 = env::var("ASTRIX_NVENC_GIR_DURATION_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if period > 0 && duration > 0 {
        if duration >= period {
            eprintln!(
                "[nvenc_d11] WARNING: GIR duration ({}) >= period ({}) is invalid, disabling GIR",
                duration, period
            );
            (0, 0)
        } else {
            (period, duration)
        }
    } else {
        (0, 0)
    }
}

fn contains_idr_nal(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 4 < data.len() {
        let start_len = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            3
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            4
        } else {
            i += 1;
            continue;
        };
        let nal_header_idx = i + start_len;
        if nal_header_idx < data.len() && (data[nal_header_idx] & 0x1f) == 5 {
            return true;
        }
        i = nal_header_idx.saturating_add(1);
    }
    false
}
