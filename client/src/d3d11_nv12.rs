//! Phase 3: BGRA → NV12 on GPU via ID3D11VideoProcessor.
//!
//! MFT H.264 encoder expects MFVideoFormat_NV12; WGC provides DXGI_FORMAT_B8G8R8A8_UNORM (BGRA).
//! This module converts BGRA textures to NV12 entirely on GPU, no CPU readback.
//!
//! Primary path: ID3D11VideoProcessor. Fallback: compute shader (Phase 4) when VideoProcessorBlt
//! returns E_INVALIDARG on some GPUs.

#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use thiserror::Error;
use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCompile, D3DCOMPILE_DEBUG, D3DCOMPILE_SKIP_VALIDATION,
};
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext, ID3D11Query,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
    ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS,
    D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_MAP_WRITE, D3D11_MAP_WRITE_DISCARD, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_SAMPLER_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_SUBRESOURCE_DATA,
    D3D11_TEX2D_SRV, D3D11_TEX2D_UAV, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_UAV_DIMENSION_TEXTURE2D,
    D3D11_UNORDERED_ACCESS_VIEW_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC_0,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_USAGE_IMMUTABLE, D3D11_USAGE_STAGING,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CAPS,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE, D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION, D3D11_VPOV_DIMENSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_NV12,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_RATIONAL,
    DXGI_SAMPLE_DESC,
};

/// D3D11_VPIV_DIMENSION_UNKNOWN = 0, D3D11_VPIV_DIMENSION_TEXTURE2D = 1
const D3D11_VPIV_DIMENSION_TEXTURE2D: D3D11_VPIV_DIMENSION = D3D11_VPIV_DIMENSION(1);
/// D3D11_VPOV_DIMENSION_UNKNOWN = 0, D3D11_VPOV_DIMENSION_TEXTURE2D = 1
const D3D11_VPOV_DIMENSION_TEXTURE2D: D3D11_VPOV_DIMENSION = D3D11_VPOV_DIMENSION(1);

/// E_INVALIDARG HRESULT — VideoProcessorBlt fails with this on some GPUs.
const E_INVALIDARG: u32 = 0x80070057;
const NV12_RING_ENV: &str = "ASTRIX_DXGI_NV12_RING";
const NV12_SPEED_ENV: &str = "ASTRIX_DXGI_NV12_OPTIMAL_SPEED";
const RGB_SCALE_CS_ENV: &str = "ASTRIX_DXGI_RGB_SCALE_COMPUTE";

#[derive(Debug, Clone)]
pub struct D3d11ConvertTextureTiming {
    pub texture: ID3D11Texture2D,
    pub ctx_wait_us: u64,
    pub submit_us: u64,
    pub copy_us: u64,
    pub srv_us: u64,
    pub cb_us: u64,
    pub bind_us: u64,
    pub bind_state_us: u64,
    pub bind_shader_us: u64,
    pub bind_cb_us: u64,
    pub bind_sampler_us: u64,
    pub bind_srv_us: u64,
    pub bind_uav_us: u64,
    pub dispatch_us: u64,
    pub unbind_us: u64,
    pub query_us: u64,
    pub blt_us: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct D3d11FlushTiming {
    pub skipped: bool,
    pub ctx_wait_us: u64,
    pub call_us: u64,
}

/// Compute shader: BGRA/RGBA → NV12 with nearest-neighbor scaling. BT.601.
/// `src_width/src_height` = input texture dimensions (WGC native).
/// `width/height` = output dimensions (target resolution).
/// When src == output dimensions, no scaling overhead (integer math only).
const HLSL_BGRA_TO_NV12: &str = r"
cbuffer Params : register(b0) {
    uint width;
    uint height;
    uint phase;
    uint is_bgra;
    uint src_width;
    uint src_height;
    uint _pad0;
    uint _pad1;
};
Texture2D<float4> srcTex : register(t0);
RWTexture2D<unorm float> outY : register(u0);
RWTexture2D<unorm float2> outUV : register(u1);

static const float3 kY = float3(0.299, 0.587, 0.114);
static const float kCbScale = 1.0 / 1.772;
static const float kCrScale = 1.0 / 1.402;

float3 to_rgb(float4 c) {
    return is_bgra ? c.bgr : c.rgb;
}

uint2 scale_coord(uint ox, uint oy) {
    uint sx = (uint)((float)ox * (float)src_width  / (float)width);
    uint sy = (uint)((float)oy * (float)src_height / (float)height);
    return uint2(min(sx, src_width - 1u), min(sy, src_height - 1u));
}

[numthreads(16, 16, 1)]
void main(uint3 gid : SV_GroupID, uint3 tid : SV_GroupThreadID) {
    uint x = gid.x * 16u + tid.x;
    uint y = gid.y * 16u + tid.y;
    // H.264 standard: limited range. Y 16-235, CbCr 16-240.
    // Y_out = (16 + Y*219)/255, CbCr_out = (128 + (cb,cr)*224)/255
    if (phase == 0u) {
        if (x >= width || y >= height) return;
        float3 rgb = to_rgb(srcTex[scale_coord(x, y)]);
        float Y = saturate(dot(rgb, kY));
        outY[uint2(x, y)] = saturate(16.0/255.0 + Y * (219.0/255.0));
    } else {
        uint uw = width >> 1u;
        uint uh = height >> 1u;
        if (x >= uw || y >= uh) return;
        float3 rgb00 = to_rgb(srcTex[scale_coord(x * 2u,     y * 2u)]);
        float3 rgb10 = to_rgb(srcTex[scale_coord(x * 2u + 1u, y * 2u)]);
        float3 rgb01 = to_rgb(srcTex[scale_coord(x * 2u,     y * 2u + 1u)]);
        float3 rgb11 = to_rgb(srcTex[scale_coord(x * 2u + 1u, y * 2u + 1u)]);
        float3 avg = (rgb00 + rgb10 + rgb01 + rgb11) * 0.25;
        float Y = dot(avg, kY);
        float cb = (avg.b - Y) * kCbScale;  // -0.5..0.5
        float cr = (avg.r - Y) * kCrScale;  // -0.5..0.5
        float u = saturate(128.0/255.0 + cb * (224.0/255.0));
        float v = saturate(128.0/255.0 + cr * (224.0/255.0));
        outUV[uint2(x, y)] = float2(u, v);
    }
}
";

const HLSL_BGRA_TO_BGRA_SCALE: &str = r"
cbuffer Params : register(b0) {
    uint width;
    uint height;
    uint src_width;
    uint src_height;
    uint _pad0;
    uint _pad1;
    uint _pad2;
};
Texture2D<float4> srcTex : register(t0);
SamplerState linearSampler : register(s0);
RWTexture2D<unorm float4> outTex : register(u0);

[numthreads(16, 16, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
    uint x = tid.x;
    uint y = tid.y;
    if (x >= width || y >= height) return;
    float2 uv = (float2(x, y) + 0.5) / float2(width, height);
    float4 c = srcTex.SampleLevel(linearSampler, uv, 0.0);
    outTex[uint2(x, y)] = c;
}
";

#[derive(Error, Debug)]
pub enum D3d11Nv12Error {
    #[error("Compute shader fallback failed: {0}")]
    ComputeShaderFallback(String),
    #[error("ID3D11VideoDevice not available (device created without D3D11_CREATE_DEVICE_VIDEO_SUPPORT)")]
    NoVideoDevice,
    #[error("ID3D11VideoContext not available")]
    NoVideoContext,
    #[error("Windows API error: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("NV12 output format not supported by video processor")]
    Nv12OutputNotSupported,
    #[error("BGRA input format not supported by video processor")]
    BgraInputNotSupported,
    #[error("Video processor has no rate conversion caps (RateConversionCapsCount == 0); CreateVideoProcessor index 0 would be invalid")]
    NoRateConversionCaps,
    #[error("Timed out waiting for NV12 output readiness after {0} ms")]
    OutputReadyTimeout(u32),
}

/// BGRA → NV12 converter with hardware scaling.
/// Primary: ID3D11VideoProcessor (scales + color-converts). Fallback: compute shader.
pub struct D3d11BgraToNv12 {
    _device: ID3D11Device,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    _processor_enum: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_textures: Vec<ID3D11Texture2D>,
    output_views: Vec<ID3D11VideoProcessorOutputView>,
    /// Input (WGC native) dimensions — used for SourceRect.
    input_width: u32,
    input_height: u32,
    /// Output (target) dimensions — NV12 texture / DestRect / MFT encode size.
    output_width: u32,
    output_height: u32,
    /// When true, VideoProcessorBlt failed (E_INVALIDARG); use compute shader path.
    use_cs_fallback: AtomicBool,
    /// Lazy-initialized compute shader fallback (Phase 4).
    cs_fallback: parking_lot::Mutex<Option<D3d11BgraToNv12Cs>>,
    /// Intermediate SRV|RTV texture for VideoProcessor when input lacks BIND_RENDER_TARGET.
    /// Lazy-created; recreated if input size/format changes. NVIDIA requires RTV on VP input.
    intermediate_tex: parking_lot::Mutex<Option<ID3D11Texture2D>>,
    input_views: parking_lot::Mutex<HashMap<usize, ID3D11VideoProcessorInputView>>,
    /// Guard for per-frame diagnostic logs: true after first successful VP frame is logged.
    vp_logged_once: AtomicBool,
    vp_state_initialized: AtomicBool,
    /// Round-robin over several NV12 surfaces so async MFT can keep frames in flight.
    output_ring_cursor: AtomicU32,
    /// Last successfully written NV12 surface (used by static re-encode path).
    last_output_index: AtomicU32,
    /// GPU fence for diagnosing whether submit waits on VP/CS completion.
    ready_query: ID3D11Query,
    /// Per-frame Flush() serializes the GPU queue. Keep only a small startup window so
    /// NVENC sees the first converted surfaces without stalling forever after that.
    flush_frames_remaining: AtomicU32,
}

impl D3d11BgraToNv12 {
    fn startup_flush_frames() -> u32 {
        std::env::var("ASTRIX_DXGI_NV12_FLUSH_FRAMES")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(3)
    }

    fn output_ring_size(fps: u32) -> u32 {
        std::env::var(NV12_RING_ENV)
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            // High-FPS presets need a deeper surface queue; otherwise brief GPU/NVENC
            // stalls under game load can collapse sender FPS before the pipeline has any
            // room to absorb them.
            .unwrap_or(if fps >= 90 { 16 } else { 4 })
            .clamp(1, 16)
    }

    fn video_usage_for_fps(fps: u32) -> (D3D11_VIDEO_USAGE, &'static str, String) {
        let env_value = std::env::var(NV12_SPEED_ENV).ok();
        let prefer_speed = env_value
            .as_deref()
            .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
            .unwrap_or(fps >= 90);
        (
            if prefer_speed {
                D3D11_VIDEO_USAGE_OPTIMAL_SPEED
            } else {
                D3D11_VIDEO_USAGE_OPTIMAL_QUALITY
            },
            if prefer_speed {
                "optimal-speed"
            } else {
                "optimal-quality"
            },
            env_value.unwrap_or_else(|| {
                if fps >= 90 {
                    "<default:on>".to_string()
                } else {
                    "<default:off>".to_string()
                }
            }),
        )
    }

    fn next_output_index(&self) -> usize {
        let ring_len = self.output_textures.len().max(1) as u32;
        (self.output_ring_cursor.fetch_add(1, Ordering::Relaxed) % ring_len) as usize
    }

    fn last_output_index(&self) -> usize {
        let ring_len = self.output_textures.len().max(1) as u32;
        self.last_output_index
            .load(Ordering::Relaxed)
            .min(ring_len.saturating_sub(1)) as usize
    }

    fn immediate_context(&self) -> Result<ID3D11DeviceContext, D3d11Nv12Error> {
        self.video_context.cast().map_err(D3d11Nv12Error::Windows)
    }

    /// Create converter that scales `input_width×input_height` → `output_width×output_height`
    /// and converts BGRA → NV12 in one pass on the GPU.
    ///
    /// `fps` is used for D3D11_VIDEO_PROCESSOR_CONTENT_DESC frame rate (e.g. 30, 60).
    /// Device must be created with D3D11_CREATE_DEVICE_VIDEO_SUPPORT.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        input_width: u32,
        input_height: u32,
        output_width: u32,
        output_height: u32,
        fps: u32,
    ) -> Result<Self, D3d11Nv12Error> {
        eprintln!(
            "[d3d11_nv12] Creating converter: {}x{} → {}x{} @ {} fps",
            input_width, input_height, output_width, output_height, fps
        );
        let startup_flush_frames = Self::startup_flush_frames();
        eprintln!(
            "[d3d11_nv12] Flush policy: startup-only ({} frame(s))",
            startup_flush_frames
        );
        let output_ring_size = Self::output_ring_size(fps);
        eprintln!(
            "[d3d11_nv12] NV12 output ring: {} surface(s)",
            output_ring_size
        );
        let (video_usage, video_usage_label, video_usage_env) = Self::video_usage_for_fps(fps);
        eprintln!(
            "[d3d11_nv12] VideoProcessor usage: {} ({}={})",
            video_usage_label, NV12_SPEED_ENV, video_usage_env
        );
        let video_device: ID3D11VideoDevice =
            device.cast().map_err(|_| D3d11Nv12Error::NoVideoDevice)?;
        let video_context: ID3D11VideoContext =
            context.cast().map_err(|_| D3d11Nv12Error::NoVideoContext)?;

        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input_width,
            InputHeight: input_height,
            OutputFrameRate: rate,
            OutputWidth: output_width,
            OutputHeight: output_height,
            Usage: video_usage,
        };
        eprintln!(
            "[d3d11_nv12] ContentDesc: {}x{} -> {}x{} fps={}/{}",
            content_desc.InputWidth,
            content_desc.InputHeight,
            content_desc.OutputWidth,
            content_desc.OutputHeight,
            content_desc.InputFrameRate.Numerator,
            content_desc.InputFrameRate.Denominator
        );

        let processor_enum = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc)? };

        let bgra_flags =
            unsafe { processor_enum.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM.into())? };
        eprintln!("[d3d11_nv12] BGRA format flags: 0x{:x}", bgra_flags);
        if bgra_flags == 0 {
            return Err(D3d11Nv12Error::BgraInputNotSupported);
        }

        let rgba_flags =
            unsafe { processor_enum.CheckVideoProcessorFormat(DXGI_FORMAT_R8G8B8A8_UNORM.into())? };
        eprintln!("[d3d11_nv12] RGBA format flags: 0x{:x}", rgba_flags);

        let nv12_flags =
            unsafe { processor_enum.CheckVideoProcessorFormat(DXGI_FORMAT_NV12.into())? };
        eprintln!("[d3d11_nv12] NV12 format flags: 0x{:x}", nv12_flags);
        if nv12_flags == 0 {
            return Err(D3d11Nv12Error::Nv12OutputNotSupported);
        }

        let mut vp_caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
        unsafe {
            processor_enum.GetVideoProcessorCaps(&mut vp_caps)?;
        }
        eprintln!(
            "[d3d11_nv12] VideoProcessorCaps: RateConversionCapsCount={}, MaxInputStreams={}, InputFormatCaps=0x{:x}, FeatureCaps=0x{:x}",
            vp_caps.RateConversionCapsCount, vp_caps.MaxInputStreams,
            vp_caps.InputFormatCaps, vp_caps.FeatureCaps
        );
        if vp_caps.RateConversionCapsCount == 0 {
            return Err(D3d11Nv12Error::NoRateConversionCaps);
        }

        let rate_conversion_index: u32 = 0;
        eprintln!(
            "[d3d11_nv12] Using rate conversion index: {} (RateConversionCapsCount={})",
            rate_conversion_index, vp_caps.RateConversionCapsCount
        );
        let processor =
            unsafe { video_device.CreateVideoProcessor(&processor_enum, rate_conversion_index)? };

        let (output_textures, output_views) = create_nv12_output_ring(
            device,
            &video_device,
            &processor_enum,
            output_width,
            output_height,
            output_ring_size,
        )?;
        let ready_query = create_event_query(device)?;
        {
            let mut nv12_desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { output_textures[0].GetDesc(&mut nv12_desc) };
            eprintln!(
                "[d3d11_nv12] NV12Tex desc: {}x{} format={:?} mip={} array={} sample={} bind=0x{:x} ring={}",
                nv12_desc.Width,
                nv12_desc.Height,
                nv12_desc.Format,
                nv12_desc.MipLevels,
                nv12_desc.ArraySize,
                nv12_desc.SampleDesc.Count,
                nv12_desc.BindFlags,
                output_ring_size,
            );
        }

        Ok(Self {
            _device: device.clone(),
            video_device,
            video_context,
            _processor_enum: processor_enum,
            processor,
            output_textures,
            output_views,
            input_width,
            input_height,
            output_width,
            output_height,
            use_cs_fallback: AtomicBool::new(false),
            cs_fallback: parking_lot::Mutex::new(None),
            intermediate_tex: parking_lot::Mutex::new(None),
            input_views: parking_lot::Mutex::new(HashMap::new()),
            vp_logged_once: AtomicBool::new(false),
            vp_state_initialized: AtomicBool::new(false),
            output_ring_cursor: AtomicU32::new(0),
            last_output_index: AtomicU32::new(0),
            ready_query,
            flush_frames_remaining: AtomicU32::new(startup_flush_frames),
        })
    }

    /// Convert BGRA texture to NV12. Returns the NV12 surface written for this frame.
    ///
    /// Serializes command submission on a shared immediate context (e.g. capture thread + encoder).
    /// On first VideoProcessorBlt E_INVALIDARG, falls back to compute shader (Phase 4).
    pub fn convert(
        &self,
        context: &ID3D11DeviceContext,
        input: &ID3D11Texture2D,
        context_mutex: &parking_lot::Mutex<()>,
    ) -> Result<ID3D11Texture2D, D3d11Nv12Error> {
        Ok(self.convert_timed(context, input, context_mutex)?.texture)
    }

    pub fn convert_timed(
        &self,
        context: &ID3D11DeviceContext,
        input: &ID3D11Texture2D,
        context_mutex: &parking_lot::Mutex<()>,
    ) -> Result<D3d11ConvertTextureTiming, D3d11Nv12Error> {
        let lock_start = std::time::Instant::now();
        let _ctx_guard = context_mutex.lock();
        let ctx_wait_us = lock_start.elapsed().as_micros() as u64;
        let submit_start = std::time::Instant::now();
        let (texture, copy_us, blt_us) = if self.use_cs_fallback.load(Ordering::Relaxed) {
            let texture = self.convert_cs(context, input)?;
            let submit_us = submit_start.elapsed().as_micros() as u64;
            (texture, 0, submit_us)
        } else {
            match self.convert_vp(input) {
                Ok(result) => result,
                Err(D3d11Nv12Error::Windows(e)) => {
                    let hr = e.code().0 as u32;
                    if hr == E_INVALIDARG {
                        eprintln!("[d3d11_nv12] convert_vp E_INVALIDARG hr=0x{:x} (CreateInputView/OutputView/Blt), switching to CS fallback", hr);
                        self.use_cs_fallback.store(true, Ordering::Relaxed);
                        let texture = self.convert_cs(context, input)?;
                        let submit_us = submit_start.elapsed().as_micros() as u64;
                        (texture, 0, submit_us)
                    } else {
                        return Err(D3d11Nv12Error::Windows(e));
                    }
                }
                Err(e) => return Err(e),
            }
        };
        let submit_us = submit_start.elapsed().as_micros() as u64;
        Ok(D3d11ConvertTextureTiming {
            texture,
            ctx_wait_us,
            submit_us,
            copy_us,
            srv_us: 0,
            cb_us: 0,
            bind_us: 0,
            bind_state_us: 0,
            bind_shader_us: 0,
            bind_cb_us: 0,
            bind_sampler_us: 0,
            bind_srv_us: 0,
            bind_uav_us: 0,
            dispatch_us: 0,
            unbind_us: 0,
            query_us: 0,
            blt_us,
        })
    }

    fn convert_vp(
        &self,
        input: &ID3D11Texture2D,
    ) -> Result<(ID3D11Texture2D, u64, u64), D3d11Nv12Error> {
        // first_frame = true only on the very first successful call (or after resize).
        let first_frame = !self.vp_logged_once.load(Ordering::Relaxed);
        let output_index = self.next_output_index();
        let output_texture = &self.output_textures[output_index];
        let output_view = self.output_views[output_index].clone();

        let mut input_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { input.GetDesc(&mut input_desc) };
        if first_frame {
            eprintln!(
                "[d3d11_nv12] InputTex desc: {}x{} format={:?}({}) mip={} array={} sample={} bind=0x{:x} misc=0x{:x}",
                input_desc.Width, input_desc.Height, input_desc.Format, input_desc.Format.0,
                input_desc.MipLevels, input_desc.ArraySize, input_desc.SampleDesc.Count,
                input_desc.BindFlags, input_desc.MiscFlags
            );
        }

        let bgra_srgb_fmt: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT =
            DXGI_FORMAT_B8G8R8A8_UNORM_SRGB.into();
        if input_desc.Format == bgra_srgb_fmt {
            // SRGB input: VideoProcessor on many NVIDIA drivers returns E_INVALIDARG.
            // The pool texture should have been created as UNORM (voice_livekit.rs strips SRGB).
            // This path should not be reached after that fix. Return early to trigger CS fallback.
            eprintln!(
                "[d3d11_nv12] convert_vp: input is DXGI_FORMAT_B8G8R8A8_UNORM_SRGB — \
                 VideoProcessor rejects SRGB on many drivers; falling back to CS path"
            );
            return Err(D3d11Nv12Error::Windows(windows::core::Error::from(
                windows::core::HRESULT(E_INVALIDARG as i32),
            )));
        }

        // VideoProcessor on NVIDIA requires:
        //   1. D3D11_BIND_RENDER_TARGET on the input texture (WGC pool = SRV-only).
        //   2. BGRA or RGBA — VP accepts both; use input's format for intermediate.
        // CopyResource requires SAME format for source and dest; using B8G8R8A8 when
        // input is R8G8B8A8 (WGC ColorFormat::Rgba8) produced black output.
        let intermediate_holder: Option<ID3D11Texture2D>;
        let mut copy_us = 0u64;
        let vp_input: &ID3D11Texture2D = {
            let needs_intermediate = input_desc.BindFlags & D3D11_BIND_RENDER_TARGET.0 as u32 == 0
                || input_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM;
            if needs_intermediate {
                let mut guard = self.intermediate_tex.lock();
                let needs_create = guard.as_ref().map_or(true, |tex| {
                    let mut d = D3D11_TEXTURE2D_DESC::default();
                    unsafe { tex.GetDesc(&mut d) };
                    d.Width != input_desc.Width
                        || d.Height != input_desc.Height
                        || d.Format.0 != input_desc.Format.0
                });
                if needs_create {
                    eprintln!(
                        "[d3d11_nv12] Creating intermediate SRV|RTV texture {}x{} format={} \
                         (input bind=0x{:x}) — same format as input for valid CopyResource",
                        input_desc.Width,
                        input_desc.Height,
                        input_desc.Format.0,
                        input_desc.BindFlags
                    );
                    let tex = create_intermediate_texture(
                        &self._device,
                        input_desc.Width,
                        input_desc.Height,
                        input_desc.Format.into(),
                    )?;
                    *guard = Some(tex);
                }
                let ctx: ID3D11DeviceContext =
                    self.video_context.cast().map_err(D3d11Nv12Error::Windows)?;
                let copy_start = std::time::Instant::now();
                unsafe { ctx.CopyResource(guard.as_ref().unwrap(), input) };
                copy_us = copy_start.elapsed().as_micros() as u64;
                intermediate_holder = Some(guard.as_ref().unwrap().clone());
                drop(guard);
                intermediate_holder.as_ref().unwrap()
            } else {
                intermediate_holder = None;
                input
            }
        };

        if first_frame {
            let mut vp_desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { vp_input.GetDesc(&mut vp_desc) };
            eprintln!(
                "[d3d11_nv12] VP input texture desc: {}x{} format={} bind=0x{:x}",
                vp_desc.Width, vp_desc.Height, vp_desc.Format.0, vp_desc.BindFlags
            );
        }

        let input_view = self.cached_input_view(vp_input)?;

        if first_frame {
            eprintln!("[d3d11_nv12] input_view and output_view created (non-null)");
        }

        let src_rect = RECT {
            left: 0,
            top: 0,
            right: self.input_width as i32,
            bottom: self.input_height as i32,
        };
        let dst_rect = RECT {
            left: 0,
            top: 0,
            right: self.output_width as i32,
            bottom: self.output_height as i32,
        };

        if first_frame {
            eprintln!(
                "[d3d11_nv12] SrcRect: {} {} {} {} | DstRect: {} {} {} {}",
                src_rect.left,
                src_rect.top,
                src_rect.right,
                src_rect.bottom,
                dst_rect.left,
                dst_rect.top,
                dst_rect.right,
                dst_rect.bottom
            );
        }

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL::from(true),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };

        if first_frame {
            let input_surface_is_none = (*stream.pInputSurface).is_none();
            eprintln!(
                "[d3d11_nv12] Stream: Enable={} OutputIndex={} PastFrames={} FutureFrames={} pInputSurface_is_none={}",
                stream.Enable.as_bool(), stream.OutputIndex,
                stream.PastFrames, stream.FutureFrames,
                input_surface_is_none
            );
        }

        let blt_start = std::time::Instant::now();
        unsafe {
            if !self.vp_state_initialized.swap(true, Ordering::Relaxed) {
                self.video_context.VideoProcessorSetStreamFrameFormat(
                    &self.processor,
                    0,
                    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                );
                self.video_context.VideoProcessorSetStreamSourceRect(
                    &self.processor,
                    0,
                    true,
                    Some(&src_rect),
                );
                self.video_context.VideoProcessorSetStreamDestRect(
                    &self.processor,
                    0,
                    true,
                    Some(&dst_rect),
                );
                self.video_context.VideoProcessorSetOutputTargetRect(
                    &self.processor,
                    true,
                    Some(&dst_rect),
                );
                let color_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: 0b0001_0100,
                };
                self.video_context
                    .VideoProcessorSetStreamColorSpace(&self.processor, 0, &color_space);
                self.video_context
                    .VideoProcessorSetOutputColorSpace(&self.processor, &color_space);
            }
            self.video_context
                .VideoProcessorBlt(&self.processor, &output_view, 0, &[stream])
                .map_err(D3d11Nv12Error::Windows)?;
        }
        let blt_us = blt_start.elapsed().as_micros() as u64;

        if first_frame {
            eprintln!(
                "[d3d11_nv12] VP path OK: input={}x{} -> output={}x{} (first frame logged)",
                self.input_width, self.input_height, self.output_width, self.output_height
            );
            self.vp_logged_once.store(true, Ordering::Relaxed);
        }

        let ctx = self.immediate_context()?;
        unsafe {
            ctx.End(&self.ready_query);
        }
        self.last_output_index
            .store(output_index as u32, Ordering::Relaxed);
        Ok((output_texture.clone(), copy_us, blt_us))
    }

    fn convert_cs(
        &self,
        context: &ID3D11DeviceContext,
        input: &ID3D11Texture2D,
    ) -> Result<ID3D11Texture2D, D3d11Nv12Error> {
        let mut guard = self.cs_fallback.lock();
        if guard.is_none() {
            let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { input.GetDesc(&mut tex_desc) };
            let is_bgra = tex_desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM.into();
            eprintln!(
                "[d3d11_nv12] Input texture format: {:?} (is_bgra={}), src={}x{} → out={}x{}",
                tex_desc.Format,
                is_bgra,
                self.input_width,
                self.input_height,
                self.output_width,
                self.output_height
            );
            match D3d11BgraToNv12Cs::new(
                &self._device,
                self.output_width,
                self.output_height,
                self.input_width,
                self.input_height,
                is_bgra,
            ) {
                Ok(cs) => *guard = Some(cs),
                Err(e) => {
                    eprintln!("[d3d11_nv12] CS fallback init failed: {:?}", e);
                    return Err(e);
                }
            }
        }
        let cs = guard.as_ref().unwrap();
        let output_index = self.next_output_index();
        let output_texture = &self.output_textures[output_index];
        cs.convert(&self._device, context, input, output_texture)?;
        unsafe {
            context.End(&self.ready_query);
        }
        self.last_output_index
            .store(output_index as u32, Ordering::Relaxed);
        Ok(output_texture.clone())
    }

    /// Recreate processor and output texture for new dimensions.
    pub fn resize(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        input_width: u32,
        input_height: u32,
        output_width: u32,
        output_height: u32,
        fps: u32,
    ) -> Result<(), D3d11Nv12Error> {
        let video_device: ID3D11VideoDevice =
            device.cast().map_err(|_| D3d11Nv12Error::NoVideoDevice)?;
        let video_context: ID3D11VideoContext =
            context.cast().map_err(|_| D3d11Nv12Error::NoVideoContext)?;

        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let (video_usage, _, _) = Self::video_usage_for_fps(fps);
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input_width,
            InputHeight: input_height,
            OutputFrameRate: rate,
            OutputWidth: output_width,
            OutputHeight: output_height,
            Usage: video_usage,
        };

        let processor_enum = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc)? };

        let nv12_flags =
            unsafe { processor_enum.CheckVideoProcessorFormat(DXGI_FORMAT_NV12.into())? };
        if nv12_flags == 0 {
            return Err(D3d11Nv12Error::Nv12OutputNotSupported);
        }

        let mut vp_caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
        unsafe {
            processor_enum.GetVideoProcessorCaps(&mut vp_caps)?;
        }
        if vp_caps.RateConversionCapsCount == 0 {
            return Err(D3d11Nv12Error::NoRateConversionCaps);
        }

        let processor = unsafe { video_device.CreateVideoProcessor(&processor_enum, 0)? };

        let (output_textures, output_views) = create_nv12_output_ring(
            device,
            &video_device,
            &processor_enum,
            output_width,
            output_height,
            Self::output_ring_size(fps),
        )?;
        let ready_query = create_event_query(device)?;

        self.video_device = video_device;
        self.video_context = video_context;
        self._processor_enum = processor_enum;
        self.processor = processor;
        self.output_textures = output_textures;
        self.output_views = output_views;
        self.input_width = input_width;
        self.input_height = input_height;
        self.output_width = output_width;
        self.output_height = output_height;
        *self.cs_fallback.lock() = None;
        *self.intermediate_tex.lock() = None;
        self.input_views.lock().clear();
        self.vp_logged_once.store(false, Ordering::Relaxed);
        self.vp_state_initialized.store(false, Ordering::Relaxed);
        self.output_ring_cursor.store(0, Ordering::Relaxed);
        self.last_output_index.store(0, Ordering::Relaxed);
        self.ready_query = ready_query;
        self.flush_frames_remaining
            .store(Self::startup_flush_frames(), Ordering::Relaxed);

        Ok(())
    }

    pub fn output_width(&self) -> u32 {
        self.output_width
    }
    pub fn output_height(&self) -> u32 {
        self.output_height
    }
    pub fn output_texture(&self) -> &ID3D11Texture2D {
        &self.output_textures[self.last_output_index()]
    }

    pub fn output_textures(&self) -> &[ID3D11Texture2D] {
        &self.output_textures
    }

    pub fn poll_output_ready(&self) -> Result<bool, D3d11Nv12Error> {
        let ctx = self.immediate_context()?;
        unsafe {
            match ctx.GetData(&self.ready_query, None, 0, 0) {
                Ok(()) => Ok(true),
                Err(_) => Ok(false),
            }
        }
    }

    pub fn wait_output_ready(&self, timeout_ms: u32) -> Result<u64, D3d11Nv12Error> {
        let ctx = self.immediate_context()?;
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(timeout_ms as u64);
        unsafe {
            loop {
                match ctx.GetData(&self.ready_query, None, 0, 0) {
                    Ok(()) => return Ok(start.elapsed().as_micros() as u64),
                    Err(_) => {
                        if std::time::Instant::now() >= deadline {
                            return Err(D3d11Nv12Error::OutputReadyTimeout(timeout_ms));
                        }
                        std::thread::yield_now();
                    }
                }
            }
        }
    }

    /// Flush the D3D11 context used by the BGRA->NV12 converter.
    /// Startup-only by default: enough to avoid the first ProcessInput blocking on
    /// some NVIDIA drivers, without paying a per-frame Flush() penalty forever.
    pub fn flush(&self, context_mutex: &parking_lot::Mutex<()>) -> Result<(), D3d11Nv12Error> {
        self.flush_timed(context_mutex).map(|_| ())
    }

    pub fn flush_timed(
        &self,
        context_mutex: &parking_lot::Mutex<()>,
    ) -> Result<D3d11FlushTiming, D3d11Nv12Error> {
        let remaining = self.flush_frames_remaining.load(Ordering::Relaxed);
        if remaining == 0 {
            return Ok(D3d11FlushTiming {
                skipped: true,
                ..Default::default()
            });
        }
        self.flush_frames_remaining
            .store(remaining.saturating_sub(1), Ordering::Relaxed);
        let lock_start = std::time::Instant::now();
        let _ctx_guard = context_mutex.lock();
        let ctx_wait_us = lock_start.elapsed().as_micros() as u64;
        let ctx: ID3D11DeviceContext =
            self.video_context.cast().map_err(D3d11Nv12Error::Windows)?;
        let flush_start = std::time::Instant::now();
        unsafe {
            ctx.Flush();
        }
        Ok(D3d11FlushTiming {
            skipped: false,
            ctx_wait_us,
            call_us: flush_start.elapsed().as_micros() as u64,
        })
    }

    fn cached_input_view(
        &self,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorInputView, D3d11Nv12Error> {
        let key = texture.as_raw() as usize;
        if let Some(view) = self.input_views.lock().get(&key).cloned() {
            return Ok(view);
        }
        let view = create_input_view(&self.video_device, &self._processor_enum, texture)?;
        self.input_views.lock().insert(key, view.clone());
        Ok(view)
    }
}

/// BGRA -> BGRA scaler with hardware scaling via ID3D11VideoProcessor.
/// Used by the NVENC direct-RGB path when the capture surface is larger than
/// the encode resolution (for example 2560x1440 -> 1920x1080).
pub struct D3d11BgraScale {
    _device: ID3D11Device,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    _processor_enum: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_textures: Vec<ID3D11Texture2D>,
    output_views: Vec<ID3D11VideoProcessorOutputView>,
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
    intermediate_tex: parking_lot::Mutex<Option<ID3D11Texture2D>>,
    input_views: parking_lot::Mutex<HashMap<usize, ID3D11VideoProcessorInputView>>,
    vp_logged_once: AtomicBool,
    vp_state_initialized: AtomicBool,
    use_cs_scale: AtomicBool,
    cs_scale: Option<D3d11BgraScaleCs>,
    output_ring_cursor: AtomicU32,
    last_output_index: AtomicU32,
    ready_query: ID3D11Query,
    flush_frames_remaining: AtomicU32,
}

impl D3d11BgraScale {
    fn next_output_index(&self) -> usize {
        let ring_len = self.output_textures.len().max(1) as u32;
        (self.output_ring_cursor.fetch_add(1, Ordering::Relaxed) % ring_len) as usize
    }

    fn immediate_context(&self) -> Result<ID3D11DeviceContext, D3d11Nv12Error> {
        self.video_context.cast().map_err(D3d11Nv12Error::Windows)
    }

    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        input_width: u32,
        input_height: u32,
        output_width: u32,
        output_height: u32,
        fps: u32,
    ) -> Result<Self, D3d11Nv12Error> {
        eprintln!(
            "[d3d11_nv12] Creating RGB scaler: {}x{} -> {}x{} @ {} fps",
            input_width, input_height, output_width, output_height, fps
        );
        let prefer_cs_scale = std::env::var(RGB_SCALE_CS_ENV)
            .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
            .unwrap_or(true);
        let startup_flush_frames = D3d11BgraToNv12::startup_flush_frames();
        let output_ring_size = D3d11BgraToNv12::output_ring_size(fps);
        let (video_usage, video_usage_label, video_usage_env) =
            D3d11BgraToNv12::video_usage_for_fps(fps);
        eprintln!(
            "[d3d11_nv12] RGB scaler usage: {} ({}={})",
            video_usage_label, NV12_SPEED_ENV, video_usage_env
        );
        let video_device: ID3D11VideoDevice =
            device.cast().map_err(|_| D3d11Nv12Error::NoVideoDevice)?;
        let video_context: ID3D11VideoContext =
            context.cast().map_err(|_| D3d11Nv12Error::NoVideoContext)?;

        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input_width,
            InputHeight: input_height,
            OutputFrameRate: rate,
            OutputWidth: output_width,
            OutputHeight: output_height,
            Usage: video_usage,
        };
        let processor_enum = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc)? };

        let bgra_flags =
            unsafe { processor_enum.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM.into())? };
        if bgra_flags == 0 {
            return Err(D3d11Nv12Error::BgraInputNotSupported);
        }
        let rgba_flags =
            unsafe { processor_enum.CheckVideoProcessorFormat(DXGI_FORMAT_R8G8B8A8_UNORM.into())? };
        eprintln!(
            "[d3d11_nv12] RGB scaler formats: BGRA=0x{:x} RGBA=0x{:x}",
            bgra_flags, rgba_flags
        );

        let mut vp_caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
        unsafe {
            processor_enum.GetVideoProcessorCaps(&mut vp_caps)?;
        }
        if vp_caps.RateConversionCapsCount == 0 {
            return Err(D3d11Nv12Error::NoRateConversionCaps);
        }
        let processor = unsafe { video_device.CreateVideoProcessor(&processor_enum, 0)? };
        let output_format = DXGI_FORMAT_B8G8R8A8_UNORM;
        let (output_textures, output_views) = create_rgb_output_ring(
            device,
            &video_device,
            &processor_enum,
            output_width,
            output_height,
            output_ring_size,
            output_format,
            prefer_cs_scale,
        )?;
        let cs_scale = if prefer_cs_scale {
            match D3d11BgraScaleCs::new(
                device,
                &output_textures,
                input_width,
                input_height,
                output_width,
                output_height,
            ) {
                Ok(cs) => {
                    eprintln!(
                        "[d3d11_nv12] RGB scaler path: compute bilinear BGRA ({}={})",
                        RGB_SCALE_CS_ENV,
                        std::env::var(RGB_SCALE_CS_ENV)
                            .unwrap_or_else(|_| "<default:on>".to_string())
                    );
                    Some(cs)
                }
                Err(err) => {
                    eprintln!(
                        "[d3d11_nv12] RGB scaler compute init failed, falling back to VideoProcessor: {:?}",
                        err
                    );
                    None
                }
            }
        } else {
            eprintln!(
                "[d3d11_nv12] RGB scaler path: VideoProcessor ({}={})",
                RGB_SCALE_CS_ENV,
                std::env::var(RGB_SCALE_CS_ENV).unwrap_or_else(|_| "<default:on>".to_string())
            );
            None
        };
        let ready_query = create_event_query(device)?;
        Ok(Self {
            _device: device.clone(),
            video_device,
            video_context,
            _processor_enum: processor_enum,
            processor,
            output_textures,
            output_views,
            input_width,
            input_height,
            output_width,
            output_height,
            intermediate_tex: parking_lot::Mutex::new(None),
            input_views: parking_lot::Mutex::new(HashMap::new()),
            vp_logged_once: AtomicBool::new(false),
            vp_state_initialized: AtomicBool::new(false),
            use_cs_scale: AtomicBool::new(cs_scale.is_some()),
            cs_scale,
            output_ring_cursor: AtomicU32::new(0),
            last_output_index: AtomicU32::new(0),
            ready_query,
            flush_frames_remaining: AtomicU32::new(startup_flush_frames),
        })
    }

    pub fn convert(
        &self,
        input: &ID3D11Texture2D,
        context_mutex: &parking_lot::Mutex<()>,
    ) -> Result<ID3D11Texture2D, D3d11Nv12Error> {
        Ok(self.convert_timed(input, context_mutex)?.texture)
    }

    pub fn convert_timed(
        &self,
        input: &ID3D11Texture2D,
        context_mutex: &parking_lot::Mutex<()>,
    ) -> Result<D3d11ConvertTextureTiming, D3d11Nv12Error> {
        let lock_start = std::time::Instant::now();
        let _ctx_guard = context_mutex.lock();
        let ctx_wait_us = lock_start.elapsed().as_micros() as u64;
        let submit_start = std::time::Instant::now();
        let (
            texture,
            copy_us,
            srv_us,
            cb_us,
            bind_us,
            bind_state_us,
            bind_shader_us,
            bind_cb_us,
            bind_sampler_us,
            bind_srv_us,
            bind_uav_us,
            dispatch_us,
            unbind_us,
            query_us,
            blt_us,
        ) = if self.use_cs_scale.load(Ordering::Relaxed) {
                match self.convert_cs(input) {
                    Ok((
                        texture,
                        srv_us,
                        cb_us,
                        bind_us,
                        bind_state_us,
                        bind_shader_us,
                        bind_cb_us,
                        bind_sampler_us,
                        bind_srv_us,
                        bind_uav_us,
                        dispatch_us,
                        unbind_us,
                        query_us,
                    )) => (
                        texture,
                        0,
                        srv_us,
                        cb_us,
                        bind_us,
                        bind_state_us,
                        bind_shader_us,
                        bind_cb_us,
                        bind_sampler_us,
                        bind_srv_us,
                        bind_uav_us,
                        dispatch_us,
                        unbind_us,
                        query_us,
                        0,
                    ),
                    Err(err) => {
                        self.use_cs_scale.store(false, Ordering::Relaxed);
                        eprintln!(
                            "[d3d11_nv12] RGB scaler compute failed, falling back to VideoProcessor: {:?}",
                            err
                        );
                        let (texture, copy_us, query_us, blt_us) = self.convert_vp(input)?;
                        (texture, copy_us, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, query_us, blt_us)
                    }
                }
            } else {
                let (texture, copy_us, query_us, blt_us) = self.convert_vp(input)?;
                (texture, copy_us, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, query_us, blt_us)
            };
        let submit_us = submit_start.elapsed().as_micros() as u64;
        Ok(D3d11ConvertTextureTiming {
            texture,
            ctx_wait_us,
            submit_us,
            copy_us,
            srv_us,
            cb_us,
            bind_us,
            bind_state_us,
            bind_shader_us,
            bind_cb_us,
            bind_sampler_us,
            bind_srv_us,
            bind_uav_us,
            dispatch_us,
            unbind_us,
            query_us,
            blt_us,
        })
    }

    fn convert_vp(
        &self,
        input: &ID3D11Texture2D,
    ) -> Result<(ID3D11Texture2D, u64, u64, u64), D3d11Nv12Error> {
        let first_frame = !self.vp_logged_once.load(Ordering::Relaxed);
        let output_index = self.next_output_index();
        let output_texture = &self.output_textures[output_index];
        let output_view = self.output_views[output_index].clone();

        let mut input_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { input.GetDesc(&mut input_desc) };
        let bgra_srgb_fmt: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT =
            DXGI_FORMAT_B8G8R8A8_UNORM_SRGB.into();
        if input_desc.Format == bgra_srgb_fmt {
            return Err(D3d11Nv12Error::Windows(windows::core::Error::from(
                windows::core::HRESULT(E_INVALIDARG as i32),
            )));
        }

        let intermediate_holder: Option<ID3D11Texture2D>;
        let mut copy_us = 0u64;
        let vp_input: &ID3D11Texture2D = {
            let needs_intermediate = input_desc.BindFlags & D3D11_BIND_RENDER_TARGET.0 as u32 == 0
                || (input_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
                    && input_desc.Format != DXGI_FORMAT_R8G8B8A8_UNORM);
            if needs_intermediate {
                let mut guard = self.intermediate_tex.lock();
                let needs_create = guard.as_ref().map_or(true, |tex| {
                    let mut d = D3D11_TEXTURE2D_DESC::default();
                    unsafe { tex.GetDesc(&mut d) };
                    d.Width != input_desc.Width
                        || d.Height != input_desc.Height
                        || d.Format.0 != input_desc.Format.0
                });
                if needs_create {
                    let tex = create_intermediate_texture(
                        &self._device,
                        input_desc.Width,
                        input_desc.Height,
                        input_desc.Format.into(),
                    )?;
                    *guard = Some(tex);
                }
                let ctx: ID3D11DeviceContext =
                    self.video_context.cast().map_err(D3d11Nv12Error::Windows)?;
                let copy_start = std::time::Instant::now();
                unsafe { ctx.CopyResource(guard.as_ref().unwrap(), input) };
                copy_us = copy_start.elapsed().as_micros() as u64;
                intermediate_holder = Some(guard.as_ref().unwrap().clone());
                drop(guard);
                intermediate_holder.as_ref().unwrap()
            } else {
                intermediate_holder = None;
                input
            }
        };

        let input_view = self.cached_input_view(vp_input)?;
        let src_rect = RECT {
            left: 0,
            top: 0,
            right: self.input_width as i32,
            bottom: self.input_height as i32,
        };
        let dst_rect = RECT {
            left: 0,
            top: 0,
            right: self.output_width as i32,
            bottom: self.output_height as i32,
        };
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: BOOL::from(true),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let blt_start = std::time::Instant::now();
        unsafe {
            if !self.vp_state_initialized.swap(true, Ordering::Relaxed) {
                self.video_context.VideoProcessorSetStreamFrameFormat(
                    &self.processor,
                    0,
                    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                );
                self.video_context.VideoProcessorSetStreamSourceRect(
                    &self.processor,
                    0,
                    true,
                    Some(&src_rect),
                );
                self.video_context.VideoProcessorSetStreamDestRect(
                    &self.processor,
                    0,
                    true,
                    Some(&dst_rect),
                );
                self.video_context.VideoProcessorSetOutputTargetRect(
                    &self.processor,
                    true,
                    Some(&dst_rect),
                );
                let color_space = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                self.video_context
                    .VideoProcessorSetStreamColorSpace(&self.processor, 0, &color_space);
                self.video_context
                    .VideoProcessorSetOutputColorSpace(&self.processor, &color_space);
            }
            self.video_context
                .VideoProcessorBlt(&self.processor, &output_view, 0, &[stream])
                .map_err(D3d11Nv12Error::Windows)?;
        }
        let blt_us = blt_start.elapsed().as_micros() as u64;
        if first_frame {
            eprintln!(
                "[d3d11_nv12] RGB scaler OK: input={}x{} -> output={}x{}",
                self.input_width, self.input_height, self.output_width, self.output_height
            );
            self.vp_logged_once.store(true, Ordering::Relaxed);
        }
        let ctx = self.immediate_context()?;
        let query_start = std::time::Instant::now();
        unsafe {
            ctx.End(&self.ready_query);
        }
        let query_us = query_start.elapsed().as_micros() as u64;
        self.last_output_index
            .store(output_index as u32, Ordering::Relaxed);
        Ok((output_texture.clone(), copy_us, query_us, blt_us))
    }

    fn convert_cs(
        &self,
        input: &ID3D11Texture2D,
    ) -> Result<(ID3D11Texture2D, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64), D3d11Nv12Error> {
        let Some(cs) = self.cs_scale.as_ref() else {
            return self
                .convert_vp(input)
                .map(|(texture, _, query_us, _)| (texture, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, query_us));
        };
        let ctx = self.immediate_context()?;
        let output_index = self.next_output_index();
        let output_texture = &self.output_textures[output_index];
        let cs_timing = cs.convert(&self._device, &ctx, input, output_index)?;
        let query_start = std::time::Instant::now();
        unsafe {
            ctx.End(&self.ready_query);
        }
        let query_us = query_start.elapsed().as_micros() as u64;
        self.last_output_index
            .store(output_index as u32, Ordering::Relaxed);
        Ok((
            output_texture.clone(),
            cs_timing.srv_us,
            cs_timing.cb_us,
            cs_timing.bind_us,
            cs_timing.bind_state_us,
            cs_timing.bind_shader_us,
            cs_timing.bind_cb_us,
            cs_timing.bind_sampler_us,
            cs_timing.bind_srv_us,
            cs_timing.bind_uav_us,
            cs_timing.dispatch_us,
            cs_timing.unbind_us,
            query_us,
        ))
    }

    pub fn output_textures(&self) -> &[ID3D11Texture2D] {
        &self.output_textures
    }

    pub fn poll_output_ready(&self) -> Result<bool, D3d11Nv12Error> {
        let ctx = self.immediate_context()?;
        unsafe {
            match ctx.GetData(&self.ready_query, None, 0, 0) {
                Ok(()) => Ok(true),
                Err(_) => Ok(false),
            }
        }
    }

    pub fn wait_output_ready(&self, timeout_ms: u32) -> Result<u64, D3d11Nv12Error> {
        let ctx = self.immediate_context()?;
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(timeout_ms as u64);
        unsafe {
            loop {
                match ctx.GetData(&self.ready_query, None, 0, 0) {
                    Ok(()) => return Ok(start.elapsed().as_micros() as u64),
                    Err(_) => {
                        if std::time::Instant::now() >= deadline {
                            return Err(D3d11Nv12Error::OutputReadyTimeout(timeout_ms));
                        }
                        std::thread::yield_now();
                    }
                }
            }
        }
    }

    pub fn flush(&self, context_mutex: &parking_lot::Mutex<()>) -> Result<(), D3d11Nv12Error> {
        self.flush_timed(context_mutex).map(|_| ())
    }

    pub fn flush_timed(
        &self,
        context_mutex: &parking_lot::Mutex<()>,
    ) -> Result<D3d11FlushTiming, D3d11Nv12Error> {
        let remaining = self.flush_frames_remaining.load(Ordering::Relaxed);
        if remaining == 0 {
            return Ok(D3d11FlushTiming {
                skipped: true,
                ..Default::default()
            });
        }
        self.flush_frames_remaining
            .store(remaining.saturating_sub(1), Ordering::Relaxed);
        let lock_start = std::time::Instant::now();
        let _ctx_guard = context_mutex.lock();
        let ctx_wait_us = lock_start.elapsed().as_micros() as u64;
        let ctx: ID3D11DeviceContext =
            self.video_context.cast().map_err(D3d11Nv12Error::Windows)?;
        let flush_start = std::time::Instant::now();
        unsafe {
            ctx.Flush();
        }
        Ok(D3d11FlushTiming {
            skipped: false,
            ctx_wait_us,
            call_us: flush_start.elapsed().as_micros() as u64,
        })
    }

    fn cached_input_view(
        &self,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorInputView, D3d11Nv12Error> {
        let key = texture.as_raw() as usize;
        if let Some(view) = self.input_views.lock().get(&key).cloned() {
            return Ok(view);
        }
        let view = create_input_view(&self.video_device, &self._processor_enum, texture)?;
        self.input_views.lock().insert(key, view.clone());
        Ok(view)
    }
}

struct D3d11BgraScaleCs {
    cs: ID3D11ComputeShader,
    cb_params_ring: Vec<ID3D11Buffer>,
    sampler: ID3D11SamplerState,
    output_uavs: Vec<ID3D11UnorderedAccessView>,
    input_srvs: parking_lot::Mutex<HashMap<usize, ID3D11ShaderResourceView>>,
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct D3d11BgraScaleCsTiming {
    srv_us: u64,
    cb_us: u64,
    bind_us: u64,
    bind_state_us: u64,
    bind_shader_us: u64,
    bind_cb_us: u64,
    bind_sampler_us: u64,
    bind_srv_us: u64,
    bind_uav_us: u64,
    dispatch_us: u64,
    unbind_us: u64,
}

impl D3d11BgraScaleCs {
    fn new(
        device: &ID3D11Device,
        output_textures: &[ID3D11Texture2D],
        input_width: u32,
        input_height: u32,
        output_width: u32,
        output_height: u32,
    ) -> Result<Self, D3d11Nv12Error> {
        let first_texture = output_textures.first().ok_or_else(|| {
            D3d11Nv12Error::ComputeShaderFallback("RGB scaler output ring is empty".into())
        })?;
        let mut output_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { first_texture.GetDesc(&mut output_desc) };
        if output_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM.into() {
            return Err(D3d11Nv12Error::ComputeShaderFallback(format!(
                "RGB compute scaler requires BGRA output textures, got {:?}",
                output_desc.Format
            )));
        }
        let cs = compile_cs_rgb_scale(device)?;
        let mut cb_params_ring = Vec::with_capacity(output_textures.len().max(1));
        for _ in 0..output_textures.len().max(1) {
            cb_params_ring.push(create_cb_params_rgb_scale(
                device,
                input_width,
                input_height,
                output_width,
                output_height,
            )?);
        }
        let sampler = create_linear_sampler_rgb(device)?;
        let mut output_uavs = Vec::with_capacity(output_textures.len());
        for texture in output_textures {
            output_uavs.push(create_uav_for_texture(device, texture)?);
        }
        eprintln!(
            "[d3d11_nv12] RGB scaler compute CB ring: {} buffer(s)",
            cb_params_ring.len()
        );
        Ok(Self {
            cs,
            cb_params_ring,
            sampler,
            output_uavs,
            input_srvs: parking_lot::Mutex::new(HashMap::new()),
            input_width,
            input_height,
            output_width,
            output_height,
        })
    }

    fn convert(
        &self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        input: &ID3D11Texture2D,
        output_index: usize,
    ) -> Result<D3d11BgraScaleCsTiming, D3d11Nv12Error> {
        let srv_start = std::time::Instant::now();
        let srv = self.cached_input_srv(device, input)?;
        let srv_us = srv_start.elapsed().as_micros() as u64;
        let mut input_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { input.GetDesc(&mut input_desc) };
        if input_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM.into() {
            return Err(D3d11Nv12Error::ComputeShaderFallback(format!(
                "RGB compute scaler supports only BGRA input, got {:?}",
                input_desc.Format
            )));
        }
        if input_desc.Width != self.input_width || input_desc.Height != self.input_height {
            return Err(D3d11Nv12Error::ComputeShaderFallback(format!(
                "RGB compute scaler input size mismatch: got {}x{}, expected {}x{}",
                input_desc.Width, input_desc.Height, self.input_width, self.input_height
            )));
        }
        let uavs = [Some(self.output_uavs[output_index].clone())];
        let uav_counts = [u32::MAX];
        let uavs_clear = [None];
        let counts_clear = [0u32];
        let cb_params = self.cb_params_ring[output_index % self.cb_params_ring.len()].clone();
        let cb_us = 0;
        unsafe {
            let bind_shader_start = std::time::Instant::now();
            context.CSSetShader(Some(&self.cs), None);
            let bind_shader_us = bind_shader_start.elapsed().as_micros() as u64;
            let bind_cb_start = std::time::Instant::now();
            context.CSSetConstantBuffers(0, Some(&[Some(cb_params)]));
            let bind_cb_us = bind_cb_start.elapsed().as_micros() as u64;
            let bind_sampler_start = std::time::Instant::now();
            context.CSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            let bind_sampler_us = bind_sampler_start.elapsed().as_micros() as u64;
            let bind_state_us = bind_shader_us
                .saturating_add(bind_cb_us)
                .saturating_add(bind_sampler_us);
            let bind_srv_start = std::time::Instant::now();
            context.CSSetShaderResources(0, Some(&[Some(srv)]));
            let bind_srv_us = bind_srv_start.elapsed().as_micros() as u64;
            let bind_uav_start = std::time::Instant::now();
            context.CSSetUnorderedAccessViews(
                0,
                1,
                Some(uavs.as_ptr()),
                Some(uav_counts.as_ptr()),
            );
            let bind_uav_us = bind_uav_start.elapsed().as_micros() as u64;
            let bind_us = bind_state_us
                .saturating_add(bind_srv_us)
                .saturating_add(bind_uav_us);
            let dispatch_start = std::time::Instant::now();
            context.Dispatch(
                (self.output_width + 15) / 16,
                (self.output_height + 15) / 16,
                1,
            );
            let dispatch_us = dispatch_start.elapsed().as_micros() as u64;
            let unbind_start = std::time::Instant::now();
            context.CSSetUnorderedAccessViews(
                0,
                1,
                Some(uavs_clear.as_ptr()),
                Some(counts_clear.as_ptr()),
            );
            context.CSSetShaderResources(0, Some(&[None]));
            context.CSSetSamplers(0, Some(&[None]));
            context.CSSetShader(None, None);
            let unbind_us = unbind_start.elapsed().as_micros() as u64;
            return Ok(D3d11BgraScaleCsTiming {
                srv_us,
                cb_us,
                bind_us,
                bind_state_us,
                bind_shader_us,
                bind_cb_us,
                bind_sampler_us,
                bind_srv_us,
                bind_uav_us,
                dispatch_us,
                unbind_us,
            });
        }
    }

    fn cached_input_srv(
        &self,
        device: &ID3D11Device,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11ShaderResourceView, D3d11Nv12Error> {
        let key = texture.as_raw() as usize;
        if let Some(view) = self.input_srvs.lock().get(&key).cloned() {
            return Ok(view);
        }
        let view = create_srv_for_texture(device, texture)?;
        self.input_srvs.lock().insert(key, view.clone());
        Ok(view)
    }
}

/// Compute shader fallback: BGRA/RGBA → tex_y (R8) + tex_uv (R8G8) → staging → NV12 output.
/// Supports scaling: src dimensions can differ from output dimensions.
struct D3d11BgraToNv12Cs {
    cs: ID3D11ComputeShader,
    cb_params: ID3D11Buffer,
    tex_y: ID3D11Texture2D,
    tex_uv: ID3D11Texture2D,
    staging_y: ID3D11Texture2D,
    staging_uv: ID3D11Texture2D,
    staging_nv12: ID3D11Texture2D,
    uav_y: ID3D11UnorderedAccessView,
    uav_uv: ID3D11UnorderedAccessView,
    /// Output dimensions (target resolution for NV12).
    width: u32,
    height: u32,
    /// Source (WGC native) dimensions for scaling.
    src_width: u32,
    src_height: u32,
    is_bgra: u32,
}

impl D3d11BgraToNv12Cs {
    fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        src_width: u32,
        src_height: u32,
        is_bgra: bool,
    ) -> Result<Self, D3d11Nv12Error> {
        let cs = compile_cs_nv12(device).map_err(|e| {
            eprintln!("[d3d11_nv12] CS compile failed: {:?}", e);
            e
        })?;
        let cb_params = create_cb_params(device).map_err(|e| {
            eprintln!("[d3d11_nv12] CS cb_params failed: {:?}", e);
            e
        })?;
        let (tex_y, tex_uv, staging_y, staging_uv, staging_nv12, uav_y, uav_uv) =
            create_cs_textures_and_uavs(device, width, height).map_err(|e| {
                eprintln!("[d3d11_nv12] CS textures/UAVs creation failed: {:?}", e);
                e
            })?;

        let is_bgra_u32 = if is_bgra { 1 } else { 0 };
        eprintln!(
            "[d3d11_nv12] CS fallback initialized OK (src={}x{} → out={}x{}, bgra={})",
            src_width, src_height, width, height, is_bgra
        );
        Ok(Self {
            cs,
            cb_params,
            tex_y,
            tex_uv,
            staging_y,
            staging_uv,
            staging_nv12,
            uav_y,
            uav_uv,
            width,
            height,
            src_width,
            src_height,
            is_bgra: is_bgra_u32,
        })
    }

    fn convert(
        &self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        input: &ID3D11Texture2D,
        output: &ID3D11Texture2D,
    ) -> Result<(), D3d11Nv12Error> {
        let srv = create_srv_for_texture(device, input).map_err(|e| {
            eprintln!("[d3d11_nv12] CS create_srv failed: {:?}", e);
            e
        })?;

        let uavs = [Some(self.uav_y.clone()), Some(self.uav_uv.clone())];
        let uav_counts = [u32::MAX, u32::MAX];

        let w = self.width;
        let h = self.height;
        let uw = w / 2;
        let uh = h / 2;

        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();

            // Phase 0: Y plane — cbuffer is 32 bytes (8 x u32).
            let params: [u32; 8] = [w, h, 0, self.is_bgra, self.src_width, self.src_height, 0, 0];
            context
                .Map(
                    &self.cb_params,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped),
                )
                .map_err(|e| D3d11Nv12Error::ComputeShaderFallback(e.to_string()))?;
            std::ptr::copy_nonoverlapping(
                params.as_ptr() as *const u8,
                mapped.pData as *mut u8,
                32,
            );
            context.Unmap(&self.cb_params, 0);

            context.CSSetShader(Some(&self.cs), None);
            context.CSSetConstantBuffers(0, Some(&[Some(self.cb_params.clone())]));
            context.CSSetShaderResources(0, Some(&[Some(srv.clone())]));
            context.CSSetUnorderedAccessViews(0, 2, Some(uavs.as_ptr()), Some(uav_counts.as_ptr()));
            context.Dispatch((w + 15) / 16, (h + 15) / 16, 1);

            // Phase 1: UV plane
            let params_uv: [u32; 8] =
                [w, h, 1, self.is_bgra, self.src_width, self.src_height, 0, 0];
            context
                .Map(
                    &self.cb_params,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped),
                )
                .map_err(|e| D3d11Nv12Error::ComputeShaderFallback(e.to_string()))?;
            std::ptr::copy_nonoverlapping(
                params_uv.as_ptr() as *const u8,
                mapped.pData as *mut u8,
                32,
            );
            context.Unmap(&self.cb_params, 0);
            context.Dispatch((uw + 15) / 16, (uh + 15) / 16, 1);

            let uavs_clear = [None, None];
            let counts_clear = [0u32; 2];
            context.CSSetUnorderedAccessViews(
                0,
                2,
                Some(uavs_clear.as_ptr()),
                Some(counts_clear.as_ptr()),
            );
            context.CSSetShaderResources(0, Some(&[None]));
            context.CSSetShader(None, None);

            context.CopyResource(&self.staging_y, &self.tex_y);
            context.CopyResource(&self.staging_uv, &self.tex_uv);
            context.Flush();
        }

        // Map staging, copy Y+UV into NV12 layout, unmap, copy to output
        unsafe {
            let mut mapped_y = D3D11_MAPPED_SUBRESOURCE::default();
            let mut mapped_uv = D3D11_MAPPED_SUBRESOURCE::default();
            let mut mapped_nv12 = D3D11_MAPPED_SUBRESOURCE::default();

            context
                .Map(&self.staging_y, 0, D3D11_MAP_READ, 0, Some(&mut mapped_y))
                .map_err(|e| D3d11Nv12Error::ComputeShaderFallback(e.to_string()))?;
            context
                .Map(&self.staging_uv, 0, D3D11_MAP_READ, 0, Some(&mut mapped_uv))
                .map_err(|e| D3d11Nv12Error::ComputeShaderFallback(e.to_string()))?;
            context
                .Map(
                    &self.staging_nv12,
                    0,
                    D3D11_MAP_WRITE,
                    0,
                    Some(&mut mapped_nv12),
                )
                .map_err(|e| D3d11Nv12Error::ComputeShaderFallback(e.to_string()))?;

            let y_row = w as usize;
            let uv_row = uw as usize * 2;
            let nv12_y_row = mapped_nv12.RowPitch as usize;
            let nv12_uv_offset = nv12_y_row * h as usize;
            let nv12_uv_row = mapped_nv12.RowPitch as usize;

            let dst_ptr = mapped_nv12.pData as *mut u8;
            let src_y = mapped_y.pData as *const u8;
            let src_uv = mapped_uv.pData as *const u8;

            for row in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    src_y.add(row * mapped_y.RowPitch as usize),
                    dst_ptr.add(row * nv12_y_row),
                    y_row,
                );
            }
            for row in 0..uh as usize {
                std::ptr::copy_nonoverlapping(
                    src_uv.add(row * mapped_uv.RowPitch as usize),
                    dst_ptr.add(nv12_uv_offset + row * nv12_uv_row),
                    uv_row,
                );
            }

            context.Unmap(&self.staging_y, 0);
            context.Unmap(&self.staging_uv, 0);
            context.Unmap(&self.staging_nv12, 0);

            context.CopyResource(output, &self.staging_nv12);
        }

        Ok(())
    }
}

fn compile_cs_nv12(device: &ID3D11Device) -> Result<ID3D11ComputeShader, D3d11Nv12Error> {
    let source = std::ffi::CString::new(HLSL_BGRA_TO_NV12)
        .map_err(|_| D3d11Nv12Error::ComputeShaderFallback("HLSL null".into()))?;
    let entry = std::ffi::CString::new("main").unwrap();
    let profile = std::ffi::CString::new("cs_5_0").unwrap();
    let flags = D3DCOMPILE_DEBUG | D3DCOMPILE_SKIP_VALIDATION;

    let mut blob = None;
    let mut err_blob = None;
    unsafe {
        use windows::core::PCSTR;
        let hr = D3DCompile(
            source.as_ptr() as *const _,
            HLSL_BGRA_TO_NV12.len(),
            PCSTR::null(),
            None,
            None::<&windows::Win32::Graphics::Direct3D::ID3DInclude>,
            PCSTR(entry.as_ptr() as *const u8),
            PCSTR(profile.as_ptr() as *const u8),
            flags,
            0,
            &mut blob,
            Some(&mut err_blob),
        );
        if hr.is_err() {
            let msg = if let Some(ref b) = err_blob {
                let ptr = b.GetBufferPointer();
                let len = b.GetBufferSize();
                String::from_utf8_lossy(std::slice::from_raw_parts(ptr as *const u8, len))
                    .into_owned()
            } else {
                format!("D3DCompile failed: {:?}", hr)
            };
            return Err(D3d11Nv12Error::ComputeShaderFallback(msg));
        }
        let blob = blob.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("no blob".into()))?;
        let bytecode =
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());

        let mut cs = None;
        device
            .CreateComputeShader(bytecode, None, Some(&mut cs))
            .map_err(|e| D3d11Nv12Error::ComputeShaderFallback(e.to_string()))?;
        cs.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("CreateComputeShader null".into()))
    }
}

fn compile_cs_rgb_scale(device: &ID3D11Device) -> Result<ID3D11ComputeShader, D3d11Nv12Error> {
    let source = std::ffi::CString::new(HLSL_BGRA_TO_BGRA_SCALE)
        .map_err(|_| D3d11Nv12Error::ComputeShaderFallback("HLSL null".into()))?;
    let entry = std::ffi::CString::new("main").unwrap();
    let profile = std::ffi::CString::new("cs_5_0").unwrap();
    let flags = D3DCOMPILE_DEBUG | D3DCOMPILE_SKIP_VALIDATION;

    let mut blob = None;
    let mut err_blob = None;
    unsafe {
        use windows::core::PCSTR;
        let hr = D3DCompile(
            source.as_ptr() as *const _,
            HLSL_BGRA_TO_BGRA_SCALE.len(),
            PCSTR::null(),
            None,
            None::<&windows::Win32::Graphics::Direct3D::ID3DInclude>,
            PCSTR(entry.as_ptr() as *const u8),
            PCSTR(profile.as_ptr() as *const u8),
            flags,
            0,
            &mut blob,
            Some(&mut err_blob),
        );
        if hr.is_err() {
            let msg = if let Some(ref b) = err_blob {
                let ptr = b.GetBufferPointer();
                let len = b.GetBufferSize();
                String::from_utf8_lossy(std::slice::from_raw_parts(ptr as *const u8, len))
                    .into_owned()
            } else {
                format!("D3DCompile failed: {:?}", hr)
            };
            return Err(D3d11Nv12Error::ComputeShaderFallback(msg));
        }
        let blob = blob.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("no blob".into()))?;
        let bytecode =
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());

        let mut cs = None;
        device
            .CreateComputeShader(bytecode, None, Some(&mut cs))
            .map_err(|e| D3d11Nv12Error::ComputeShaderFallback(e.to_string()))?;
        cs.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("CreateComputeShader null".into()))
    }
}

fn create_cb_params(device: &ID3D11Device) -> Result<ID3D11Buffer, D3d11Nv12Error> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut buf = None;
    unsafe {
        device.CreateBuffer(&desc, None, Some(&mut buf))?;
    }
    buf.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("cb_params null".into()))
}

fn create_cb_params_rgb_scale(
    device: &ID3D11Device,
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
) -> Result<ID3D11Buffer, D3d11Nv12Error> {
    let params: [u32; 8] = [
        output_width,
        output_height,
        input_width,
        input_height,
        0,
        0,
        0,
        0,
    ];
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 32,
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: params.as_ptr() as *const _,
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    let mut buf = None;
    unsafe {
        device.CreateBuffer(&desc, Some(&init), Some(&mut buf))?;
    }
    buf.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("cb_params_rgb_scale null".into()))
}

fn create_linear_sampler_rgb(device: &ID3D11Device) -> Result<ID3D11SamplerState, D3d11Nv12Error> {
    let desc = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 1,
        ComparisonFunc: windows::Win32::Graphics::Direct3D11::D3D11_COMPARISON_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
    };
    let mut sampler = None;
    unsafe {
        device
            .CreateSamplerState(&desc, Some(&mut sampler))
            .map_err(|e| D3d11Nv12Error::ComputeShaderFallback(e.to_string()))?;
    }
    sampler.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("CreateSamplerState null".into()))
}

/// Create SRV for packed-format texture (BGRA/RGBA). NV12 cannot be read as single SRV —
/// use d3d11_rgba::create_nv12_plane_srv (two SRVs with PlaneSlice 0 and 1) for NV12.
fn create_srv_for_texture(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11ShaderResourceView, D3d11Nv12Error> {
    let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut tex_desc) };
    if tex_desc.Format == DXGI_FORMAT_NV12.into() {
        return Err(D3d11Nv12Error::ComputeShaderFallback(
            "NV12 texture needs two SRVs (PlaneSlice 0/1) via create_nv12_plane_srv, not single SRV".into(),
        ));
    }
    let format = tex_desc.Format;
    let resource: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
        texture.clone().cast().map_err(D3d11Nv12Error::Windows)?;
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    };
    let mut srv = None;
    unsafe {
        device.CreateShaderResourceView(&resource, Some(&desc), Some(&mut srv))?;
    }
    srv.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("SRV null".into()))
}

fn create_uav_for_texture(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11UnorderedAccessView, D3d11Nv12Error> {
    let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut tex_desc) };
    let resource: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
        texture.clone().cast().map_err(D3d11Nv12Error::Windows)?;
    let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: tex_desc.Format,
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
        },
    };
    let mut uav = None;
    unsafe {
        device
            .CreateUnorderedAccessView(&resource, Some(&desc), Some(&mut uav))
            .map_err(|e| D3d11Nv12Error::ComputeShaderFallback(e.to_string()))?;
    }
    uav.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("UAV null".into()))
}

fn create_cs_textures_and_uavs(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<
    (
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11UnorderedAccessView,
        ID3D11UnorderedAccessView,
    ),
    D3d11Nv12Error,
> {
    let uw = width / 2;
    let uh = height / 2;

    let tex_desc =
        |w: u32, h: u32, fmt: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT, bind: u32| {
            D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: fmt.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: bind,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            }
        };

    let mut tex_y = None;
    let mut tex_uv = None;
    unsafe {
        device.CreateTexture2D(
            &tex_desc(
                width,
                height,
                DXGI_FORMAT_R8_UNORM,
                D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            ),
            None,
            Some(&mut tex_y),
        )?;
        device.CreateTexture2D(
            &tex_desc(
                uw,
                uh,
                DXGI_FORMAT_R8G8_UNORM,
                D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            ),
            None,
            Some(&mut tex_uv),
        )?;
    }
    let tex_y = tex_y.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("tex_y null".into()))?;
    let tex_uv =
        tex_uv.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("tex_uv null".into()))?;

    let staging_desc =
        |w: u32, h: u32, fmt: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT| {
            D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: fmt.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            }
        };

    let mut staging_y = None;
    let mut staging_uv = None;
    let mut staging_nv12 = None;
    unsafe {
        device.CreateTexture2D(
            &staging_desc(width, height, DXGI_FORMAT_R8_UNORM),
            None,
            Some(&mut staging_y),
        )?;
        device.CreateTexture2D(
            &staging_desc(uw, uh, DXGI_FORMAT_R8G8_UNORM),
            None,
            Some(&mut staging_uv),
        )?;
        let desc_nv12 = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
        };
        device.CreateTexture2D(&desc_nv12, None, Some(&mut staging_nv12))?;
    }
    let staging_y =
        staging_y.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("staging_y null".into()))?;
    let staging_uv = staging_uv
        .ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("staging_uv null".into()))?;
    let staging_nv12 = staging_nv12
        .ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("staging_nv12 null".into()))?;

    let uav_desc = |fmt: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT| {
        D3D11_UNORDERED_ACCESS_VIEW_DESC {
            Format: fmt.into(),
            ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
            },
        }
    };

    let mut uav_y = None;
    let mut uav_uv = None;
    unsafe {
        let res_y: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
            tex_y.clone().cast().map_err(D3d11Nv12Error::Windows)?;
        let res_uv: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
            tex_uv.clone().cast().map_err(D3d11Nv12Error::Windows)?;
        device.CreateUnorderedAccessView(
            &res_y,
            Some(&uav_desc(DXGI_FORMAT_R8_UNORM)),
            Some(&mut uav_y),
        )?;
        device.CreateUnorderedAccessView(
            &res_uv,
            Some(&uav_desc(DXGI_FORMAT_R8G8_UNORM)),
            Some(&mut uav_uv),
        )?;
    }
    let uav_y = uav_y.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("uav_y null".into()))?;
    let uav_uv =
        uav_uv.ok_or_else(|| D3d11Nv12Error::ComputeShaderFallback("uav_uv null".into()))?;

    Ok((
        tex_y,
        tex_uv,
        staging_y,
        staging_uv,
        staging_nv12,
        uav_y,
        uav_uv,
    ))
}

/// Create an intermediate texture with D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE.
/// Used when the WGC pool texture has only D3D11_BIND_SHADER_RESOURCE, which causes
/// E_INVALIDARG in VideoProcessorBlt on NVIDIA drivers.
fn create_intermediate_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> Result<ID3D11Texture2D, D3d11Nv12Error> {
    let bind_flags = D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind_flags,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    eprintln!(
        "[d3d11_nv12] Intermediate RTV texture created: {}x{} format={:?} bind=0x{:x}",
        width, height, format, bind_flags
    );
    tex.ok_or_else(|| {
        D3d11Nv12Error::Windows(windows::core::Error::from(windows::core::HRESULT(-1)))
    })
}

fn create_nv12_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, D3d11Nv12Error> {
    // NV12 output texture for ID3D11VideoProcessor:
    //   D3D11_BIND_RENDER_TARGET   — required for CreateVideoProcessorOutputView
    //   D3D11_BIND_SHADER_RESOURCE — required for MFT (NVENC) to read the texture as input
    // D3D11_BIND_DECODER (0x200) is for ID3D11VideoDecoderOutputView and must NOT be used here.
    let bind_flags = D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12.into(),
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
        BindFlags: bind_flags,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    texture.ok_or_else(|| {
        D3d11Nv12Error::Windows(windows::core::Error::from(windows::core::HRESULT(-1)))
    })
}

fn create_nv12_output_ring(
    device: &ID3D11Device,
    video_device: &ID3D11VideoDevice,
    processor_enum: &ID3D11VideoProcessorEnumerator,
    width: u32,
    height: u32,
    ring_size: u32,
) -> Result<(Vec<ID3D11Texture2D>, Vec<ID3D11VideoProcessorOutputView>), D3d11Nv12Error> {
    let mut textures = Vec::with_capacity(ring_size as usize);
    let mut views = Vec::with_capacity(ring_size as usize);
    for _ in 0..ring_size {
        let texture = create_nv12_texture(device, width, height)?;
        let view = create_output_view(video_device, processor_enum, &texture)?;
        textures.push(texture);
        views.push(view);
    }
    Ok((textures, views))
}

fn create_rgb_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    allow_uav: bool,
) -> Result<ID3D11Texture2D, D3d11Nv12Error> {
    let mut bind_flags =
        D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32;
    if allow_uav {
        bind_flags |= D3D11_BIND_UNORDERED_ACCESS.0 as u32;
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format.into(),
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
        BindFlags: bind_flags,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    texture.ok_or_else(|| {
        D3d11Nv12Error::Windows(windows::core::Error::from(windows::core::HRESULT(-1)))
    })
}

fn create_rgb_output_ring(
    device: &ID3D11Device,
    video_device: &ID3D11VideoDevice,
    processor_enum: &ID3D11VideoProcessorEnumerator,
    width: u32,
    height: u32,
    ring_size: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    allow_uav: bool,
) -> Result<(Vec<ID3D11Texture2D>, Vec<ID3D11VideoProcessorOutputView>), D3d11Nv12Error> {
    let mut textures = Vec::with_capacity(ring_size as usize);
    let mut views = Vec::with_capacity(ring_size as usize);
    for _ in 0..ring_size {
        let texture = create_rgb_texture(device, width, height, format, allow_uav)?;
        let view = create_output_view(video_device, processor_enum, &texture)?;
        textures.push(texture);
        views.push(view);
    }
    Ok((textures, views))
}

fn create_event_query(device: &ID3D11Device) -> Result<ID3D11Query, D3d11Nv12Error> {
    let desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_EVENT,
        MiscFlags: 0,
    };
    let mut query = None;
    unsafe {
        device.CreateQuery(&desc, Some(&mut query))?;
    }
    query.ok_or_else(|| {
        D3d11Nv12Error::Windows(windows::core::Error::from(windows::core::HRESULT(-1)))
    })
}

fn create_input_view(
    video_device: &ID3D11VideoDevice,
    processor_enum: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11VideoProcessorInputView, D3d11Nv12Error> {
    let resource: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
        texture.clone().cast().map_err(D3d11Nv12Error::Windows)?;

    let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: 0, // driver uses texture format (BGRA)
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPIV {
                MipSlice: 0,
                ArraySlice: 0,
            },
        },
    };
    let mut view = None;
    unsafe {
        video_device.CreateVideoProcessorInputView(
            &resource,
            processor_enum,
            &desc,
            Some(&mut view),
        )?;
    }
    view.ok_or_else(|| {
        D3d11Nv12Error::Windows(windows::core::Error::from(windows::core::HRESULT(-1)))
    })
}

fn create_output_view(
    video_device: &ID3D11VideoDevice,
    processor_enum: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11VideoProcessorOutputView, D3d11Nv12Error> {
    let resource: windows::Win32::Graphics::Direct3D11::ID3D11Resource =
        texture.clone().cast().map_err(D3d11Nv12Error::Windows)?;

    let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
        },
    };
    let mut view = None;
    unsafe {
        video_device.CreateVideoProcessorOutputView(
            &resource,
            processor_enum,
            &desc,
            Some(&mut view),
        )?;
    }
    view.ok_or_else(|| {
        D3d11Nv12Error::Windows(windows::core::Error::from(windows::core::HRESULT(-1)))
    })
}
