//! Shared encoded H.264 backend wrapper.
//!
//! Keeps current sender/runtime behavior intact:
//! - prefer NVENC D3D11 on NVIDIA when it becomes available
//! - otherwise fall back to the existing MFT implementation
//! - when NVENC fails mid-session in `auto`, rebuild the backend as MFT
//!   without forcing an immediate drop to the CPU/OpenH264 path

#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use std::borrow::Cow;

use thiserror::Error;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

use crate::mft_encoder::{EncodedFrame, MftEncoderError, MftH264Encoder};
use crate::nvenc_d11::{NvencD3d11Encoder, NvencD3d11Error, NvencSubmitBreakdown};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodedBackendKind {
    NvencD3d11,
    MftHardware,
    MftSoftware,
}

#[derive(Debug, Error)]
pub enum EncodedH264EncoderError {
    #[error("NVENC D3D11: {0}")]
    Nvenc(#[from] NvencD3d11Error),
    #[error("MFT: {0}")]
    Mft(#[from] MftEncoderError),
}

enum EncodedH264EncoderImpl {
    Nvenc(NvencD3d11Encoder),
    Mft(MftH264Encoder),
}

struct EncodedBackendConfig {
    device: ID3D11Device,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    allow_runtime_mft_fallback: bool,
}

pub struct EncodedH264Encoder {
    inner: EncodedH264EncoderImpl,
    config: EncodedBackendConfig,
}

impl EncodedH264Encoder {
    pub fn new_mft(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, EncodedH264EncoderError> {
        Ok(Self {
            inner: EncodedH264EncoderImpl::Mft(MftH264Encoder::new(
                device, width, height, fps, bitrate,
            )?),
            config: EncodedBackendConfig::new(device, width, height, fps, bitrate, false),
        })
    }

    pub fn new_auto(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        nv12_ring: &[ID3D11Texture2D],
    ) -> Result<Self, EncodedH264EncoderError> {
        let config = EncodedBackendConfig::new(device, width, height, fps, bitrate, true);
        match NvencD3d11Encoder::new(device, width, height, fps, bitrate, nv12_ring) {
            Ok(enc) => {
                eprintln!(
                    "[encoded_h264] selected NVENC D3D11 backend: {}",
                    enc.encoder_name().trim_end_matches('\0')
                );
                return Ok(Self {
                    inner: EncodedH264EncoderImpl::Nvenc(enc),
                    config,
                });
            }
            Err(err) if err.should_fallback_to_mft() => {
                eprintln!(
                    "[encoded_h264] NVENC D3D11 unavailable, falling back to MFT: {}",
                    err
                );
            }
            Err(err) => return Err(err.into()),
        }

        Ok(Self {
            inner: EncodedH264EncoderImpl::Mft(MftH264Encoder::new(
                device, width, height, fps, bitrate,
            )?),
            config,
        })
    }

    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        ts_us: i64,
        key_frame: bool,
    ) -> Result<Vec<EncodedFrame>, EncodedH264EncoderError> {
        let mut runtime_fallback_attempted = false;
        loop {
            let result = match &mut self.inner {
                EncodedH264EncoderImpl::Nvenc(enc) => enc
                    .encode(texture, ts_us, key_frame)
                    .map_err(EncodedH264EncoderError::from),
                EncodedH264EncoderImpl::Mft(enc) => enc
                    .encode(texture, ts_us, key_frame)
                    .map_err(EncodedH264EncoderError::from),
            };

            match result {
                Ok(frames) => return Ok(frames),
                Err(EncodedH264EncoderError::Nvenc(err)) if !runtime_fallback_attempted => {
                    runtime_fallback_attempted = true;
                    self.promote_nvenc_to_mft("encode()", err)?;
                }
                Err(err) => return Err(err),
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
    ) -> Result<(), EncodedH264EncoderError> {
        let mut runtime_fallback_attempted = false;
        loop {
            let result = match &mut self.inner {
                EncodedH264EncoderImpl::Nvenc(enc) => enc
                    .submit(
                        texture,
                        ts_us,
                        key_frame,
                        rtp_ts,
                        capture_us,
                        need_input_timeout_ms,
                    )
                    .map_err(EncodedH264EncoderError::from),
                EncodedH264EncoderImpl::Mft(enc) => enc
                    .submit(
                        texture,
                        ts_us,
                        key_frame,
                        rtp_ts,
                        capture_us,
                        need_input_timeout_ms,
                    )
                    .map_err(EncodedH264EncoderError::from),
            };

            match result {
                Ok(()) => return Ok(()),
                Err(EncodedH264EncoderError::Nvenc(err)) if !runtime_fallback_attempted => {
                    runtime_fallback_attempted = true;
                    self.promote_nvenc_to_mft("submit()", err)?;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub fn collect(
        &mut self,
    ) -> Result<Option<(Vec<EncodedFrame>, u32, i64, u64)>, EncodedH264EncoderError> {
        let mut runtime_fallback_attempted = false;
        loop {
            let result = match &mut self.inner {
                EncodedH264EncoderImpl::Nvenc(enc) => {
                    enc.collect().map_err(EncodedH264EncoderError::from)
                }
                EncodedH264EncoderImpl::Mft(enc) => {
                    enc.collect().map_err(EncodedH264EncoderError::from)
                }
            };

            match result {
                Ok(output) => return Ok(output),
                Err(EncodedH264EncoderError::Nvenc(err)) if !runtime_fallback_attempted => {
                    runtime_fallback_attempted = true;
                    self.promote_nvenc_to_mft("collect()", err)?;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub fn collect_blocking(
        &mut self,
        timeout_ms: u32,
    ) -> Result<Option<(Vec<EncodedFrame>, u32, i64, u64)>, EncodedH264EncoderError> {
        let mut runtime_fallback_attempted = false;
        loop {
            let result = match &mut self.inner {
                EncodedH264EncoderImpl::Nvenc(enc) => enc
                    .collect_blocking(timeout_ms)
                    .map_err(EncodedH264EncoderError::from),
                EncodedH264EncoderImpl::Mft(enc) => enc
                    .collect_blocking(timeout_ms)
                    .map_err(EncodedH264EncoderError::from),
            };

            match result {
                Ok(output) => return Ok(output),
                Err(EncodedH264EncoderError::Nvenc(err)) if !runtime_fallback_attempted => {
                    runtime_fallback_attempted = true;
                    self.promote_nvenc_to_mft("collect_blocking()", err)?;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), EncodedH264EncoderError> {
        let mut runtime_fallback_attempted = false;
        loop {
            let result = match &mut self.inner {
                EncodedH264EncoderImpl::Nvenc(enc) => enc
                    .set_bitrate(bitrate)
                    .map_err(EncodedH264EncoderError::from),
                EncodedH264EncoderImpl::Mft(enc) => enc
                    .set_bitrate(bitrate)
                    .map_err(EncodedH264EncoderError::from),
            };

            match result {
                Ok(()) => {
                    self.config.bitrate = bitrate;
                    return Ok(());
                }
                Err(EncodedH264EncoderError::Nvenc(err)) if !runtime_fallback_attempted => {
                    runtime_fallback_attempted = true;
                    self.promote_nvenc_to_mft("set_bitrate()", err)?;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub fn set_rates(
        &mut self,
        fps: u32,
        bitrate: u32,
        nv12_ring: &[ID3D11Texture2D],
    ) -> Result<(), EncodedH264EncoderError> {
        let fps = fps.max(1);
        if fps == self.config.fps {
            return self.set_bitrate(bitrate);
        }

        let old_fps = self.config.fps;
        let old_bitrate = self.config.bitrate;
        let backend_kind = self.backend_kind();
        let new_inner = match backend_kind {
            EncodedBackendKind::NvencD3d11 => match NvencD3d11Encoder::new(
                &self.config.device,
                self.config.width,
                self.config.height,
                fps,
                bitrate,
                nv12_ring,
            ) {
                Ok(enc) => EncodedH264EncoderImpl::Nvenc(enc),
                Err(err)
                    if self.config.allow_runtime_mft_fallback && err.should_fallback_to_mft() =>
                {
                    eprintln!(
                        "[encoded_h264] NVENC D3D11 rate reconfigure failed, rebuilding as MFT: {}",
                        err
                    );
                    EncodedH264EncoderImpl::Mft(MftH264Encoder::new(
                        &self.config.device,
                        self.config.width,
                        self.config.height,
                        fps,
                        bitrate,
                    )?)
                }
                Err(err) => return Err(err.into()),
            },
            EncodedBackendKind::MftHardware | EncodedBackendKind::MftSoftware => {
                EncodedH264EncoderImpl::Mft(MftH264Encoder::new(
                    &self.config.device,
                    self.config.width,
                    self.config.height,
                    fps,
                    bitrate,
                )?)
            }
        };

        self.inner = new_inner;
        self.config.fps = fps;
        self.config.bitrate = bitrate;
        eprintln!(
            "[encoded_h264] backend rate reconfigured: fps {} -> {}, bitrate {:.2} -> {:.2} Mbps ({})",
            old_fps,
            fps,
            old_bitrate as f64 / 1_000_000.0,
            bitrate as f64 / 1_000_000.0,
            self.encoder_name().trim_end_matches('\0')
        );
        Ok(())
    }

    pub fn is_async(&self) -> bool {
        match &self.inner {
            EncodedH264EncoderImpl::Nvenc(enc) => enc.is_async(),
            EncodedH264EncoderImpl::Mft(enc) => enc.is_async(),
        }
    }

    pub fn is_hardware(&self) -> bool {
        match &self.inner {
            EncodedH264EncoderImpl::Nvenc(enc) => enc.is_hardware(),
            EncodedH264EncoderImpl::Mft(enc) => enc.is_hardware(),
        }
    }

    pub fn nvenc_uses_rgb_input(&self) -> bool {
        match &self.inner {
            EncodedH264EncoderImpl::Nvenc(enc) => enc.uses_rgb_input(),
            EncodedH264EncoderImpl::Mft(_) => false,
        }
    }

    pub fn nvenc_input_ring_size(&self) -> Option<usize> {
        match &self.inner {
            EncodedH264EncoderImpl::Nvenc(enc) => Some(enc.input_ring_size()),
            EncodedH264EncoderImpl::Mft(_) => None,
        }
    }

    pub fn nvenc_in_flight_count(&self) -> Option<usize> {
        match &self.inner {
            EncodedH264EncoderImpl::Nvenc(enc) => Some(enc.in_flight_count()),
            EncodedH264EncoderImpl::Mft(_) => None,
        }
    }

    pub fn backend_kind(&self) -> EncodedBackendKind {
        match &self.inner {
            EncodedH264EncoderImpl::Nvenc(_) => EncodedBackendKind::NvencD3d11,
            EncodedH264EncoderImpl::Mft(enc) if enc.is_hardware() => {
                EncodedBackendKind::MftHardware
            }
            EncodedH264EncoderImpl::Mft(_) => EncodedBackendKind::MftSoftware,
        }
    }

    pub fn encoder_name(&self) -> Cow<'_, str> {
        match &self.inner {
            EncodedH264EncoderImpl::Nvenc(enc) => Cow::Borrowed(enc.encoder_name()),
            EncodedH264EncoderImpl::Mft(enc) => Cow::Borrowed(enc.encoder_name()),
        }
    }

    pub fn last_nvenc_submit_breakdown(&self) -> Option<NvencSubmitBreakdown> {
        match &self.inner {
            EncodedH264EncoderImpl::Nvenc(enc) => Some(enc.last_submit_breakdown()),
            EncodedH264EncoderImpl::Mft(_) => None,
        }
    }

    pub fn bitrate_bps(&self) -> u32 {
        self.config.bitrate
    }

    pub fn fps(&self) -> u32 {
        self.config.fps
    }

    fn promote_nvenc_to_mft(
        &mut self,
        stage: &str,
        err: NvencD3d11Error,
    ) -> Result<(), EncodedH264EncoderError> {
        if !self.config.allow_runtime_mft_fallback || !err.should_fallback_to_mft() {
            return Err(err.into());
        }

        eprintln!(
            "[encoded_h264] NVENC D3D11 runtime failure during {}. Rebuilding backend as MFT: {}",
            stage, err
        );

        self.inner = EncodedH264EncoderImpl::Mft(MftH264Encoder::new(
            &self.config.device,
            self.config.width,
            self.config.height,
            self.config.fps,
            self.config.bitrate,
        )?);

        let encoder_name = self.encoder_name().into_owned();
        eprintln!(
            "[encoded_h264] runtime fallback active: {}",
            encoder_name.trim_end_matches('\0')
        );

        Ok(())
    }
}

impl EncodedBackendConfig {
    fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        allow_runtime_mft_fallback: bool,
    ) -> Self {
        Self {
            device: device.clone(),
            width,
            height,
            fps,
            bitrate,
            allow_runtime_mft_fallback,
        }
    }
}
