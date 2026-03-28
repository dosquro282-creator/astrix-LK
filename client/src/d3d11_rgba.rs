//! D3D11 Compute Shader: I420 (Y, U, V) → RGBA for decode path.
//! Used to accelerate video_frame_to_rgba when the frame buffer is I420.
//! Creates its own D3D11 device (receive thread has no WGC device).

#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use std::sync::atomic::{AtomicU8, Ordering};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{D3D11_SRV_DIMENSION_TEXTURE2D, D3D_DRIVER_TYPE_HARDWARE};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAP_WRITE_DISCARD,
    D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_UAV_DIMENSION_TEXTURE2D, ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext,
    ID3D11Device3, ID3D11Query, ID3D11ShaderResourceView, ID3D11ShaderResourceView1,
    ID3D11Texture2D, ID3D11UnorderedAccessView,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_SHADER_RESOURCE_VIEW_DESC1, D3D11_SHADER_RESOURCE_VIEW_DESC1_0, D3D11_TEX2D_SRV,
    D3D11_TEX2D_SRV1, D3D11_TEXTURE2D_DESC, D3D11_UNORDERED_ACCESS_VIEW_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_TEX2D_UAV,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_UNSUPPORTED;
use windows::Win32::Graphics::Direct3D11::ID3D11Resource;

const HLSL_I420_TO_RGBA: &str = r"
// BT.601: R = Y + 1.402*(Cr-0.5); G = Y - 0.344*(Cb-0.5) - 0.714*(Cr-0.5); B = Y + 1.772*(Cb-0.5)
cbuffer Params : register(b0) {
    uint width;
    uint height;
};
Texture2D<float> Ytex : register(t0);
Texture2D<float> Utex : register(t1);
Texture2D<float> Vtex : register(t2);
RWTexture2D<float4> rgbaOut : register(u0);

static const float kR = 1.402;
static const float kGcb = 0.344;
static const float kGcr = 0.714;
static const float kB = 1.772;

[numthreads(16, 16, 1)]
void main(uint3 gid : SV_GroupID, uint3 tid : SV_GroupThreadID) {
    uint x = gid.x * 16u + tid.x;
    uint y = gid.y * 16u + tid.y;
    if (x >= width || y >= height) return;
    float yVal = Ytex[uint2(x, y)].r;
    float uVal = Utex[uint2(x / 2u, y / 2u)].r - 0.5;
    float vVal = Vtex[uint2(x / 2u, y / 2u)].r - 0.5;
    float r = saturate(yVal + kR * vVal);
    float g = saturate(yVal - kGcb * uVal - kGcr * vVal);
    float b = saturate(yVal + kB * uVal);
    rgbaOut[uint2(x, y)] = float4(r, g, b, 1.0);
}
";

/// Phase 3.2: NV12 → RGBA compute shader (decode path, zero CPU readback).
/// Input: NV12 D3D11 texture (Y plane R8, UV plane RG8). Output: RGBA texture on GPU.
const HLSL_NV12_TO_RGBA: &str = include_str!("../nv12_to_rgba.hlsl");

#[derive(Debug)]
pub enum D3d11RgbaError {
    CreateDevice(String),
    Compile(String),
    CreateTexture(String),
    CreateShaderResourceView(String),
    CreateUnorderedAccessView(String),
    CreateQuery(String),
    Map(String),
    DeviceLost(String),
}

/// Lazy-initialized D3D11 I420→RGBA converter for the receive path.
/// Creates device/context on first use.
pub struct D3d11I420ToRgba {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    cs: ID3D11ComputeShader,
    cb_params: windows::Win32::Graphics::Direct3D11::ID3D11Buffer,
    tex_y: ID3D11Texture2D,
    tex_u: ID3D11Texture2D,
    tex_v: ID3D11Texture2D,
    tex_rgba: ID3D11Texture2D,
    staging_rgba: ID3D11Texture2D,
    srv_y: ID3D11ShaderResourceView,
    srv_u: ID3D11ShaderResourceView,
    srv_v: ID3D11ShaderResourceView,
    uav_rgba: ID3D11UnorderedAccessView,
    event_query: ID3D11Query,
    width: u32,
    height: u32,
}

static DECODE_GPU_FAILED: AtomicU8 = AtomicU8::new(0);

/// Runtime gamma from settings (0 = off). Stored as f32 bits in AtomicU32. Default 0.
static VIDEO_DECODER_GAMMA: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0); // f32::to_bits(0.0)

/// Set gamma from settings UI. Called when settings load or user changes slider.
pub fn set_video_decoder_gamma(gamma: f32) {
    let g = gamma.clamp(0.0, 3.0);
    VIDEO_DECODER_GAMMA.store(f32::to_bits(g), std::sync::atomic::Ordering::Release);
}

fn get_video_decoder_gamma() -> f32 {
    f32::from_bits(VIDEO_DECODER_GAMMA.load(std::sync::atomic::Ordering::Acquire))
}

/// Call once when GPU decode path fails; subsequent frames use CPU only.
pub fn mark_decode_gpu_failed() {
    DECODE_GPU_FAILED.store(1, Ordering::Relaxed);
}

pub fn decode_gpu_failed() -> bool {
    DECODE_GPU_FAILED.load(Ordering::Relaxed) != 0
}

impl D3d11I420ToRgba {
    /// Create device and pipeline for the given dimensions. Call from receive thread.
    pub fn new(width: u32, height: u32) -> Result<Self, D3d11RgbaError> {
        let (device, context) = create_device()?;
        let cs = compile_cs_i420_to_rgba(&device)?;
        let (cb_params, tex_y, tex_u, tex_v, tex_rgba, staging_rgba, srv_y, srv_u, srv_v, uav_rgba, event_query) =
            create_resources(&device, width, height)?;
        Ok(Self {
            device,
            context,
            cs,
            cb_params,
            tex_y,
            tex_u,
            tex_v,
            tex_rgba,
            staging_rgba,
            srv_y,
            srv_u,
            srv_v,
            uav_rgba,
            event_query,
            width,
            height,
        })
    }

    /// Check for device-lost (DXGI_ERROR_DEVICE_REMOVED / DEVICE_RESET).
    fn check_device_lost(&self) -> Result<(), D3d11RgbaError> {
        let hr = unsafe { self.device.GetDeviceRemovedReason() };
        if hr.is_err() {
            return Err(D3d11RgbaError::DeviceLost(format!(
                "D3D11 device lost: {:?}", hr
            )));
        }
        Ok(())
    }

    /// Convert I420 planes to RGBA (ABGR byte order for egui). Returns (width, height, rgba_vec).
    /// Returns `DeviceLost` on GPU contention / device removal so the caller can fall back to CPU.
    pub fn convert(
        &self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
    ) -> Result<(u32, u32, Vec<u8>), D3d11RgbaError> {
        let w = self.width;
        let h = self.height;
        let y_len = (w * h) as usize;
        let uv_len = ((w / 2) * (h / 2)) as usize;
        if y.len() < y_len || u.len() < uv_len || v.len() < uv_len {
            return Err(D3d11RgbaError::Map("I420 plane size mismatch".into()));
        }

        unsafe {
            let res_y = self.tex_y.cast::<ID3D11Resource>().map_err(|e| D3d11RgbaError::Map(e.to_string()))?;
            let res_u = self.tex_u.cast::<ID3D11Resource>().map_err(|e| D3d11RgbaError::Map(e.to_string()))?;
            let res_v = self.tex_v.cast::<ID3D11Resource>().map_err(|e| D3d11RgbaError::Map(e.to_string()))?;
            self.context.UpdateSubresource(&res_y, 0, None, y.as_ptr() as *const _, w, 0);
            self.context.UpdateSubresource(&res_u, 0, None, u.as_ptr() as *const _, w / 2, 0);
            self.context.UpdateSubresource(&res_v, 0, None, v.as_ptr() as *const _, w / 2, 0);
        }

        self.check_device_lost()?;

        let params: [u32; 4] = [w, h, 0, 0];
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(
                    &self.cb_params,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(std::ptr::addr_of_mut!(mapped)),
                )
                .map_err(|e| {
                    let _ = self.check_device_lost();
                    D3d11RgbaError::Map(e.to_string())
                })?;
            std::ptr::copy_nonoverlapping(
                params.as_ptr() as *const u8,
                mapped.pData as *mut u8,
                16,
            );
            self.context.Unmap(&self.cb_params, 0);
        }

        let uavs = [Some(self.uav_rgba.clone())];
        let uav_counts = [u32::MAX];
        unsafe {
            self.context.CSSetShader(Some(&self.cs), None);
            self.context.CSSetConstantBuffers(0, Some(&[Some(self.cb_params.clone())]));
            self.context.CSSetShaderResources(
                0,
                Some(&[Some(self.srv_y.clone()), Some(self.srv_u.clone()), Some(self.srv_v.clone())]),
            );
            self.context.CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), Some(uav_counts.as_ptr()));
        }

        let gx = (w + 15) / 16;
        let gy = (h + 15) / 16;
        unsafe {
            self.context.Dispatch(gx, gy, 1);
        }

        let uavs_clear = [None];
        let counts_clear = [0u32];
        unsafe {
            self.context.CSSetUnorderedAccessViews(0, 1, Some(uavs_clear.as_ptr()), Some(counts_clear.as_ptr()));
            self.context.CSSetShaderResources(0, Some(&[None, None, None]));
            self.context.CSSetShader(None, None);
        }

        self.check_device_lost()?;

        // Copy to staging, wait, Map
        unsafe {
            let dst = self.staging_rgba.cast::<ID3D11Resource>().unwrap();
            let src = self.tex_rgba.cast::<ID3D11Resource>().unwrap();
            self.context.CopyResource(&dst, &src);
            self.context.Flush();
            self.context.End(&self.event_query);
            self.context.Flush();

            let wait_start = std::time::Instant::now();
            loop {
                match self.context.GetData(&self.event_query, None, 0, 0) {
                    Ok(()) => break,
                    Err(e) => {
                        self.check_device_lost()?;
                        if wait_start.elapsed() > std::time::Duration::from_secs(2) {
                            return Err(D3d11RgbaError::DeviceLost(format!(
                                "GPU query timeout (2s), last err: {:?}", e
                            )));
                        }
                        std::thread::yield_now();
                    }
                }
            }
        }

        self.check_device_lost()?;

        let mut rgba = vec![0u8; (w * h * 4) as usize];
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(
                    &self.staging_rgba,
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(std::ptr::addr_of_mut!(mapped)),
                )
                .map_err(|e| {
                    let _ = self.check_device_lost();
                    D3d11RgbaError::Map(e.to_string())
                })?;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, rgba.len());
            rgba.copy_from_slice(src);
            self.context.Unmap(&self.staging_rgba, 0);
        }

        Ok((w, h, rgba))
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
}

// --- Phase 3.2: D3d11Nv12ToRgba ---

/// Phase 3.2: NV12 D3D11 texture → RGBA D3D11 texture (GPU only, zero CPU readback).
/// Used when MFT hardware decoder outputs D3D11TextureVideoFrameBuffer (NV12 texture).
/// Accepts external device for shared device path (Phase 3.4).
///
/// Phase 3.5 (WGL zero-copy): the pipeline uses TWO RGBA textures to avoid the
/// incompatibility between D3D11_BIND_UNORDERED_ACCESS and D3D11_RESOURCE_MISC_SHARED:
///
///  NV12 input  →  [CopySubresourceRegion]  →  nv12_staging
///  nv12_staging →  [compute shader Dispatch]  →  output_texture   (UAV, no MISC_SHARED)
///  output_texture → [CopyResource]  →  display_texture  (SRV + MISC_SHARED, no UAV)
///
/// WGL_NV_DX_interop2 registers display_texture (the clean MISC_SHARED copy).
/// The GL texture is then backed by display_texture's VRAM — no UAV conflict.
pub struct D3d11Nv12ToRgba {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    cs: ID3D11ComputeShader,
    full_range: bool,
    /// Constant buffer for GAMMA_RUNTIME (gamma from settings). None when compile-time gamma.
    cb_gamma: Option<windows::Win32::Graphics::Direct3D11::ID3D11Buffer>,
    /// Constant buffer for UV_BILINEAR (width, height). None when UV_BILINEAR disabled.
    cb_nv12: Option<windows::Win32::Graphics::Direct3D11::ID3D11Buffer>,
    /// Compute shader writes here via UAV. No MISC_SHARED (UAV + MISC_SHARED conflicts).
    output_texture: Option<ID3D11Texture2D>,
    uav_rgba: Option<ID3D11UnorderedAccessView>,
    /// WGL display copy: MISC_SHARED + BIND_SHADER_RESOURCE only (no UAV).
    /// Created in parallel with output_texture; each frame we CopyResource into it.
    /// WGL_NV_DX_interop2 registers this texture's COM pointer for GL access.
    display_texture: Option<ID3D11Texture2D>,
    /// Phase 3.5: raw COM pointer of display_texture as usize for WGL registration.
    shared_handle: Option<usize>,
    /// Phase 3: single-subresource NV12 texture with D3D11_BIND_SHADER_RESOURCE.
    /// MFT output textures are texture arrays (ArraySize=8); we copy the requested
    /// subresource here so the compute shader can read it as a regular Texture2D.
    nv12_staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
}

impl D3d11Nv12ToRgba {
    /// Create with default device (decode path when shared device not available).
    pub fn new_with_default_device(default_full_range: bool) -> Result<Self, D3d11RgbaError> {
        let (device, _) = create_device()?;
        Self::new(&device, default_full_range)
    }

    /// Create converter with the given D3D11 device. Use shared device from gpu_device for zero-copy.
    pub fn new(device: &ID3D11Device, default_full_range: bool) -> Result<Self, D3d11RgbaError> {
        let context = unsafe {
            device
                .GetImmediateContext()
                .map_err(|e| D3d11RgbaError::CreateDevice(format!("GetImmediateContext: {:?}", e)))?
        };
        // ASTRIX_VIDEO_COLOR_STAGE=1,2,3,4: preset for color fix testing (overrides other env vars).
        // 1=OUTPUT_SRGB only, 2=SRGB texture format, 3=+DISABLE_FRAMEBUFFER_SRGB, 4=GAMMA_DECODER 0.55
        let color_stage = std::env::var("ASTRIX_VIDEO_COLOR_STAGE").ok().and_then(|v| v.parse::<u8>().ok());

        let (output_srgb, decoder_gamma, use_runtime_gamma) = if let Some(stage) = color_stage {
            let (os, dg, rt) = match stage {
                1 => (true, None, false),
                2 => (false, None, false),
                3 => (true, None, false),
                4 => (false, Some(0.55), false), // compile-time 0.55
                _ => (false, None, true),
            };
            eprintln!("[d3d11_rgba] COLOR_STAGE={}: output_srgb={} decoder_gamma={:?}", stage, os, dg);
            if stage == 3 {
                eprintln!("[d3d11_rgba] Stage 3: also set ASTRIX_VIDEO_DISABLE_FRAMEBUFFER_SRGB=1");
            }
            (os, dg, rt)
        } else {
            // Default: variant 4 — runtime gamma from settings, no OUTPUT_SRGB.
            let env_val = std::env::var("ASTRIX_VIDEO_OUTPUT_SRGB").ok();
            let output_srgb = match env_val.as_deref() {
                Some("0") | Some("false") => false,
                Some("1") | Some("true") => true,
                _ => false, // default variant 4: no sRGB
            };
            let decoder_gamma = std::env::var("ASTRIX_VIDEO_DECODER_GAMMA")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .filter(|&g| g > 0.0);
            let use_runtime_gamma = decoder_gamma.is_none();
            (output_srgb, decoder_gamma, use_runtime_gamma)
        };

        let full_range = std::env::var("ASTRIX_VIDEO_NV12_FULL_RANGE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(default_full_range);
        let uv_bilinear = std::env::var("ASTRIX_VIDEO_UV_BILINEAR")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        if crate::telemetry::is_telemetry_enabled() {
            eprintln!(
                "[d3d11_rgba] NV12→RGBA: output_srgb={} full_range={} decoder_gamma={:?} runtime_gamma={} runtime_gamma_current={:.2} uv_bilinear={}",
                output_srgb,
                full_range,
                decoder_gamma,
                use_runtime_gamma,
                get_video_decoder_gamma(),
                uv_bilinear
            );
        }

        let cs = compile_cs_nv12_to_rgba(device, output_srgb, full_range, decoder_gamma, use_runtime_gamma, uv_bilinear)?;
        let cb_gamma = if use_runtime_gamma {
            Some(create_gamma_cb(device)?)
        } else {
            None
        };
        let cb_nv12 = if uv_bilinear {
            Some(create_nv12_cb(device)?)
        } else {
            None
        };
        Ok(Self {
            device: device.clone(),
            context,
            cs,
            full_range,
            cb_gamma,
            cb_nv12,
            output_texture: None,
            uav_rgba: None,
            display_texture: None,
            shared_handle: None,
            nv12_staging: None,
            width: 0,
            height: 0,
        })
    }

    pub fn full_range(&self) -> bool {
        self.full_range
    }

    /// Convert NV12 D3D11 texture → RGBA D3D11 texture (GPU only). Returns reference to output texture.
    /// Recreates output texture when dimensions change.
    pub fn convert(
        &mut self,
        nv12_texture: &ID3D11Texture2D,
        subresource: u32,
        width: u32,
        height: u32,
    ) -> Result<&ID3D11Texture2D, D3d11RgbaError> {
        if self.output_texture.is_none() || self.width != width || self.height != height {
            // D3D11 UAV does not support R8G8B8A8_UNORM_SRGB — always use UNORM.
            let (tex_rgba, uav) = create_nv12_output_resources(&self.device, width, height)?;
            self.output_texture = Some(tex_rgba);
            self.uav_rgba = Some(uav);

            // Display copy: MISC_SHARED + SRV only — safe for WGL_NV_DX_interop2.
            // After each Dispatch we CopyResource here.  WGL registers this pointer.
            let disp = create_display_texture(&self.device, width, height)?;
            self.shared_handle = nv12_output_shared_handle(&disp);
            self.display_texture = Some(disp);
            // Phase 3: staging NV12 texture for compute shader input.
            self.nv12_staging = Some(create_nv12_shader_readable(&self.device, width, height)?);
            self.width = width;
            self.height = height;
        }

        // Copy the requested NV12 subresource (MFT texture array) into our
        // single-subresource staging texture so the shader sees a plain Texture2D.
        let staging = self.nv12_staging.as_ref().unwrap();
        unsafe {
            let dst = staging
                .cast::<ID3D11Resource>()
                .map_err(|e| D3d11RgbaError::CreateTexture(format!("staging cast: {:?}", e)))?;
            let src = nv12_texture
                .cast::<ID3D11Resource>()
                .map_err(|e| D3d11RgbaError::CreateTexture(format!("src cast: {:?}", e)))?;
            self.context.CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, subresource, None);
        }

        let srv_y = create_nv12_plane_srv(&self.device, staging, 0, true)?;
        let srv_uv = create_nv12_plane_srv(&self.device, staging, 0, false)?;

        let uav = self.uav_rgba.as_ref().unwrap();
        let uavs = [Some(uav.clone())];
        let uav_counts = [u32::MAX];

        if let Some(ref cb) = self.cb_gamma {
            let gamma = get_video_decoder_gamma();
            let params: [f32; 4] = [gamma, 0.0, 0.0, 0.0];
            let res = cb
                .clone()
                .cast::<ID3D11Resource>()
                .map_err(|e| D3d11RgbaError::Map(e.to_string()))?;
            unsafe {
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(&res, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(std::ptr::addr_of_mut!(mapped)))
                    .map_err(|e| D3d11RgbaError::Map(e.to_string()))?;
                std::ptr::copy_nonoverlapping(params.as_ptr() as *const u8, mapped.pData as *mut u8, 16);
                self.context.Unmap(&res, 0);
            }
        }
        if let Some(ref cb) = self.cb_nv12 {
            let params: [u32; 4] = [width, height, 0, 0];
            let res = cb
                .clone()
                .cast::<ID3D11Resource>()
                .map_err(|e| D3d11RgbaError::Map(e.to_string()))?;
            unsafe {
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(&res, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(std::ptr::addr_of_mut!(mapped)))
                    .map_err(|e| D3d11RgbaError::Map(e.to_string()))?;
                std::ptr::copy_nonoverlapping(params.as_ptr() as *const u8, mapped.pData as *mut u8, 16);
                self.context.Unmap(&res, 0);
            }
        }

        unsafe {
            self.context.CSSetShader(Some(&self.cs), None);
            if let (Some(ref cb_g), Some(ref cb_n)) = (self.cb_gamma.as_ref(), self.cb_nv12.as_ref()) {
                let bufs: [Option<ID3D11Buffer>; 2] = [Some((*cb_g).clone()), Some((*cb_n).clone())];
                self.context.CSSetConstantBuffers(0, Some(&bufs));
            } else if let Some(ref cb) = self.cb_gamma {
                self.context.CSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
            } else if let Some(ref cb) = self.cb_nv12 {
                self.context.CSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
            }
            self.context.CSSetShaderResources(
                0,
                Some(&[Some(srv_y), Some(srv_uv)]),
            );
            self.context.CSSetUnorderedAccessViews(0, 1, Some(uavs.as_ptr()), Some(uav_counts.as_ptr()));
        }

        let gx = (width + 7) / 8;
        let gy = (height + 7) / 8;
        unsafe {
            self.context.Dispatch(gx, gy, 1);
        }

        let uavs_clear = [None];
        let counts_clear = [0u32];
        unsafe {
            self.context.CSSetUnorderedAccessViews(0, 1, Some(uavs_clear.as_ptr()), Some(counts_clear.as_ptr()));
            self.context.CSSetShaderResources(0, Some(&[None, None]));
            if self.cb_gamma.is_some() || self.cb_nv12.is_some() {
                let n = if self.cb_gamma.is_some() && self.cb_nv12.is_some() { 2 } else { 1 };
                self.context.CSSetConstantBuffers(0, Some(&[None, None][..n]));
            }
            self.context.CSSetShader(None, None);
        }

        // Phase 3.5: copy compute output → display texture (WGL share target).
        // display_texture has MISC_SHARED but no UAV (those flags conflict).
        // The CopyResource command is in the same command stream as the Dispatch, so
        // wglDXLockObjectsNV will see display_texture contain fresh compute data.
        if let (Some(src), Some(dst)) = (self.output_texture.as_ref(), self.display_texture.as_ref()) {
            unsafe {
                let src_res = src
                    .cast::<ID3D11Resource>()
                    .map_err(|e| D3d11RgbaError::CreateTexture(format!("output cast: {:?}", e)))?;
                let dst_res = dst
                    .cast::<ID3D11Resource>()
                    .map_err(|e| D3d11RgbaError::CreateTexture(format!("display cast: {:?}", e)))?;
                self.context.CopyResource(&dst_res, &src_res);

                // Flush so wglDXLockObjectsNV on the UI thread sees all submitted work.
                self.context.Flush();
            }
        }

        Ok(self.output_texture.as_ref().unwrap())
    }

    pub fn output_texture(&self) -> Option<&ID3D11Texture2D> {
        self.output_texture.as_ref()
    }

    /// Phase 3.5: Win32 HANDLE from IDXGIResource::GetSharedHandle for the RGBA output texture.
    /// Valid while this converter and its output_texture are alive.
    /// Returns None if texture not yet created or sharing not supported.
    pub fn get_shared_handle(&self) -> Option<usize> {
        self.shared_handle
    }

    /// Phase 3.4: Convert NV12 texture → RGBA bytes (approach C: GPU convert + CPU readback for egui).
    /// NV12→RGBA on GPU; final Map for compatibility with current video_frame_to_rgba signature.
    pub fn convert_to_rgba_bytes(
        &mut self,
        nv12_texture: &ID3D11Texture2D,
        subresource: u32,
        width: u32,
        height: u32,
    ) -> Result<(u32, u32, Vec<u8>), D3d11RgbaError> {
        let (device, context) = (self.device.clone(), self.context.clone());
        let tex = self.convert(nv12_texture, subresource, width, height)?;
        let rgba = map_texture_to_rgba_bytes(&device, &context, tex, width, height)?;
        Ok((width, height, rgba))
    }
}

fn map_texture_to_rgba_bytes(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, D3d11RgbaError> {
    use windows::Win32::Graphics::Direct3D11::{D3D11_CPU_ACCESS_READ, D3D11_USAGE_STAGING};

    let tex_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    unsafe {
        device
            .CreateTexture2D(&tex_desc, None, Some(std::ptr::from_mut(&mut staging)))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("staging: {:?}", e)))?;
    }
    let staging = staging.ok_or_else(|| D3d11RgbaError::CreateTexture("staging null".into()))?;

    unsafe {
        let dst = staging.clone().cast::<ID3D11Resource>().map_err(|e| D3d11RgbaError::Map(e.to_string()))?;
        let src = texture.clone().cast::<ID3D11Resource>().map_err(|e| D3d11RgbaError::Map(e.to_string()))?;
        context.CopyResource(&dst, &src);
        context.Flush();
    }

    let mut rgba = vec![0u8; (width * height * 4) as usize];
    unsafe {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(
                &staging,
                0,
                D3D11_MAP_READ,
                0,
                Some(std::ptr::addr_of_mut!(mapped)),
            )
            .map_err(|e| D3d11RgbaError::Map(e.to_string()))?;
        let src = std::slice::from_raw_parts(mapped.pData as *const u8, rgba.len());
        rgba.copy_from_slice(src);
        context.Unmap(&staging, 0);
    }
    Ok(rgba)
}

fn create_gamma_cb(device: &ID3D11Device) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Buffer, D3d11RgbaError> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_WRITE, D3D11_USAGE_DYNAMIC,
    };
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 16, // float + padding for 16-byte alignment
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut buf = None;
    unsafe {
        device
            .CreateBuffer(&desc, None, Some(&mut buf))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("gamma cbuffer: {:?}", e)))?;
    }
    buf.ok_or_else(|| D3d11RgbaError::CreateTexture("gamma cbuffer null".into()))
}

fn create_nv12_cb(device: &ID3D11Device) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Buffer, D3d11RgbaError> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_WRITE, D3D11_USAGE_DYNAMIC,
    };
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 16,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut buf = None;
    unsafe {
        device
            .CreateBuffer(&desc, None, Some(&mut buf))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("NV12 cbuffer: {:?}", e)))?;
    }
    buf.ok_or_else(|| D3d11RgbaError::CreateTexture("NV12 cbuffer null".into()))
}

fn compile_cs_nv12_to_rgba(
    device: &ID3D11Device,
    output_srgb: bool,
    full_range: bool,
    decoder_gamma: Option<f32>,
    use_runtime_gamma: bool,
    uv_bilinear: bool,
) -> Result<ID3D11ComputeShader, D3d11RgbaError> {
    use windows::core::PCSTR;
    use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_DEBUG, D3DCOMPILE_SKIP_VALIDATION};
    // ASTRIX_VIDEO_DEBUG_SOLID_RED=1: output solid red to diagnose blending/UI issues.
    // If red appears pale/washed → problem is in egui blend or framebuffer, not NV12.
    let debug_solid_red = std::env::var("ASTRIX_VIDEO_DEBUG_SOLID_RED")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // ASTRIX_VIDEO_DEBUG_GRAY_Y=1: output grayscale from Y with limited-range conversion.
    // y = saturate((y - 0.0625) * 1.164). If contrast becomes normal → problem is 100% in range.
    let debug_gray_y = std::env::var("ASTRIX_VIDEO_DEBUG_GRAY_Y")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let hlsl: String = if debug_solid_red {
        eprintln!("[d3d11_rgba] ASTRIX_VIDEO_DEBUG_SOLID_RED=1: outputting solid red; if pale → egui blend/UI issue");
        r#"
RWTexture2D<float4> OutRGBA : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    OutRGBA[id.xy] = float4(1, 0, 0, 1);
}
"#
        .into()
    } else if debug_gray_y {
        eprintln!("[d3d11_rgba] ASTRIX_VIDEO_DEBUG_GRAY_Y=1: outputting Y grayscale (limited range); if contrast OK → problem is in range");
        r#"
Texture2D<float>  TexY  : register(t0);
Texture2D<float2> TexUV : register(t1);
RWTexture2D<float4> OutRGBA : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    float y = TexY[id.xy];
    y = saturate((y - 0.0625) * 1.164);
    OutRGBA[id.xy] = float4(y, y, y, 1);
}
"#
        .into()
    } else {
        let mut defines = String::new();
        if output_srgb {
            defines.push_str("#define OUTPUT_SRGB 1\n");
        }
        if full_range {
            defines.push_str("#define FULL_RANGE 1\n");
        }
        if let Some(g) = decoder_gamma {
            defines.push_str("#define GAMMA_DECODER_ENABLED 1\n");
            defines.push_str(&format!("#define GAMMA_DECODER {}\n", g));
        }
        if use_runtime_gamma {
            defines.push_str("#define GAMMA_RUNTIME 1\n");
        }
        if uv_bilinear {
            defines.push_str("#define UV_BILINEAR 1\n");
        }
        format!("{}{}", defines, HLSL_NV12_TO_RGBA)
    };
    let source = std::ffi::CString::new(hlsl.as_bytes().to_vec())
        .map_err(|_| D3d11RgbaError::Compile("HLSL NV12 string null".into()))?;
    let entry = std::ffi::CString::new("main").unwrap();
    let profile = std::ffi::CString::new("cs_5_0").unwrap();
    let flags = D3DCOMPILE_SKIP_VALIDATION | D3DCOMPILE_DEBUG;
    let mut blob = None;
    let mut err_blob = None;
    unsafe {
        let hr = D3DCompile(
            source.as_ptr() as *const _,
            hlsl.len(),
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
            let msg = err_blob
                .as_ref()
                .map(|b| {
                    let ptr = b.GetBufferPointer();
                    let len = b.GetBufferSize();
                    String::from_utf8_lossy(std::slice::from_raw_parts(ptr as *const u8, len)).into_owned()
                })
                .unwrap_or_else(|| format!("D3DCompile NV12 failed: {:?}", hr));
            return Err(D3d11RgbaError::Compile(msg));
        }
        let blob = blob.ok_or_else(|| D3d11RgbaError::Compile("no blob".into()))?;
        let bytecode = std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
        let mut cs = None;
        device
            .CreateComputeShader(bytecode, None, Some(&mut cs))
            .map_err(|e| D3d11RgbaError::Compile(e.to_string()))?;
        cs.ok_or_else(|| D3d11RgbaError::Compile("CreateComputeShader NV12 null".into()))
    }
}

fn create_nv12_plane_srv(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    _subresource: u32,
    y_plane: bool,
) -> Result<ID3D11ShaderResourceView, D3d11RgbaError> {
    let res = texture
        .clone()
        .cast::<ID3D11Resource>()
        .map_err(|e| D3d11RgbaError::CreateShaderResourceView(e.to_string()))?;
    let format = if y_plane {
        DXGI_FORMAT_R8_UNORM
    } else {
        DXGI_FORMAT_R8G8_UNORM
    };
    // NV12 requires PlaneSlice: 0 for Y, 1 for UV. Without it, driver may return wrong plane → washed out.
    // Use CreateShaderResourceView1 (D3D11.3) when available.
    let plane_slice = if y_plane { 0 } else { 1 };
    if let Ok(device3) = device.cast::<ID3D11Device3>() {
        static LOGGED_PLANE_SLICE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if crate::telemetry::is_telemetry_enabled()
            && !LOGGED_PLANE_SLICE.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            eprintln!("[d3d11_rgba] NV12 SRV: using PlaneSlice (Y=0, UV=1) via D3D11.3");
        }
        let desc1 = D3D11_SHADER_RESOURCE_VIEW_DESC1 {
            Format: format.into(),
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC1_0 {
                Texture2D: D3D11_TEX2D_SRV1 {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    PlaneSlice: plane_slice,
                },
            },
        };
        let mut srv1 = None;
        unsafe {
            device3
                .CreateShaderResourceView1(&res, Some(&desc1), Some(&mut srv1))
                .map_err(|e| D3d11RgbaError::CreateShaderResourceView(format!("NV12 plane SRV: {:?}", e)))?;
        }
        if let Some(srv1) = srv1 {
            return srv1.cast::<ID3D11ShaderResourceView>().map_err(|e| {
                D3d11RgbaError::CreateShaderResourceView(format!("NV12 SRV1 cast: {:?}", e))
            });
        }
    }
    // Fallback: D3D11.0 without PlaneSlice (may cause wrong plane on some drivers).
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: format.into(),
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
        device
            .CreateShaderResourceView(&res, Some(&desc), Some(&mut srv))
            .map_err(|e| D3d11RgbaError::CreateShaderResourceView(format!("NV12 plane SRV: {:?}", e)))?;
    }
    srv.ok_or_else(|| D3d11RgbaError::CreateShaderResourceView("NV12 plane SRV null".into()))
}

/// Return the raw ID3D11Texture2D COM interface pointer of display_texture as usize.
/// This pointer is passed to wglDXRegisterObjectNV so WGL can back the GL texture with
/// the display_texture's VRAM.  display_texture has MISC_SHARED (required by the WGL
/// spec) but no UAV bind flag (avoiding the UAV + MISC_SHARED conflict).
fn nv12_output_shared_handle(tex: &ID3D11Texture2D) -> Option<usize> {
    use windows::core::Interface;
    let raw = unsafe { tex.as_raw() as usize };
    if raw == 0 { None } else { Some(raw) }
}

/// Create the WGL display texture: MISC_SHARED + BIND_SHADER_RESOURCE, no UAV.
///
/// This texture is the WGL_NV_DX_interop2 share target.  Each frame the compute
/// output is CopyResource'd into it.  The separation from the UAV compute texture
/// is necessary because D3D11_RESOURCE_MISC_SHARED + D3D11_BIND_UNORDERED_ACCESS
/// may fail or produce non-shareable textures on some hardware.
fn create_display_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, D3d11RgbaError> {
    use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_SHARED};
    let tex_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
    };
    let mut tex = None;
    unsafe {
        device
            .CreateTexture2D(&tex_desc, None, Some(std::ptr::from_mut(&mut tex)))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("display tex: {:?}", e)))?;
    }
    tex.ok_or_else(|| D3d11RgbaError::CreateTexture("display tex null".into()))
}

fn create_nv12_output_resources(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, ID3D11UnorderedAccessView), D3d11RgbaError> {
    use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_UNORDERED_ACCESS, D3D11_BIND_SHADER_RESOURCE};

    let tex_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        // UAV: compute shader writes here.  No MISC_SHARED — the two flags conflict and
        // prevent CreateUnorderedAccessView from succeeding on some hardware/drivers.
        // The WGL display copy (display_texture) carries MISC_SHARED instead.
        BindFlags: (D3D11_BIND_UNORDERED_ACCESS.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex_rgba = None;
    unsafe {
        device
            .CreateTexture2D(&tex_desc, None, Some(std::ptr::from_mut(&mut tex_rgba)))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("NV12 output tex: {:?}", e)))?;
    }
    let tex_rgba = tex_rgba.ok_or_else(|| D3d11RgbaError::CreateTexture("CreateTexture2D null".into()))?;

    let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM.into(),
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
        },
    };
    let mut uav = None;
    unsafe {
        let res = tex_rgba
            .clone()
            .cast::<ID3D11Resource>()
            .map_err(|e| D3d11RgbaError::CreateUnorderedAccessView(e.to_string()))?;
        device
            .CreateUnorderedAccessView(&res, Some(&uav_desc), Some(&mut uav))
            .map_err(|e| D3d11RgbaError::CreateUnorderedAccessView(e.to_string()))?;
    }
    let uav = uav.ok_or_else(|| D3d11RgbaError::CreateUnorderedAccessView("UAV null".into()))?;

    Ok((tex_rgba, uav))
}

/// Phase 3: Create a single-subresource NV12 texture with D3D11_BIND_SHADER_RESOURCE.
/// Used as intermediate copy target: MFT texture array subresource → this texture → SRVs.
fn create_nv12_shader_readable(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, D3d11RgbaError> {
    use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("NV12 staging: {:?}", e)))?;
    }
    tex.ok_or_else(|| D3d11RgbaError::CreateTexture("NV12 staging null".into()))
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext), D3d11RgbaError> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_FLAG;
    let mut device = None;
    let mut context = None;
    unsafe {
        let hr = D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            windows::Win32::Graphics::Direct3D11::D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        );
        if hr == Err(DXGI_ERROR_UNSUPPORTED.into()) {
            return Err(D3d11RgbaError::CreateDevice("Hardware device unsupported".into()));
        }
        let device = device
            .ok_or_else(|| D3d11RgbaError::CreateDevice("CreateDevice returned null".into()))?;
        let context = context
            .ok_or_else(|| D3d11RgbaError::CreateDevice("GetImmediateContext null".into()))?;
        Ok((device, context))
    }
}

fn compile_cs_i420_to_rgba(device: &ID3D11Device) -> Result<ID3D11ComputeShader, D3d11RgbaError> {
    use windows::core::PCSTR;
    use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_SKIP_VALIDATION, D3DCOMPILE_DEBUG};
    let source = std::ffi::CString::new(HLSL_I420_TO_RGBA)
        .map_err(|_| D3d11RgbaError::Compile("HLSL string null".into()))?;
    let entry = std::ffi::CString::new("main").unwrap();
    let profile = std::ffi::CString::new("cs_5_0").unwrap();
    let flags = D3DCOMPILE_SKIP_VALIDATION | D3DCOMPILE_DEBUG;
    let mut blob = None;
    let mut err_blob = None;
    unsafe {
        let hr = D3DCompile(
            source.as_ptr() as *const _,
            HLSL_I420_TO_RGBA.len(),
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
            let msg = err_blob
                .as_ref()
                .map(|b| {
                    let ptr = b.GetBufferPointer();
                    let len = b.GetBufferSize();
                    String::from_utf8_lossy(std::slice::from_raw_parts(ptr as *const u8, len)).into_owned()
                })
                .unwrap_or_else(|| format!("D3DCompile failed: {:?}", hr));
            return Err(D3d11RgbaError::Compile(msg));
        }
        let blob = blob.ok_or_else(|| D3d11RgbaError::Compile("no blob".into()))?;
        let bytecode = std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
        let mut cs = None;
        device
            .CreateComputeShader(bytecode, None, Some(&mut cs))
            .map_err(|e| D3d11RgbaError::Compile(e.to_string()))?;
        cs.ok_or_else(|| D3d11RgbaError::Compile("CreateComputeShader null".into()))
    }
}

fn create_resources(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<
    (
        windows::Win32::Graphics::Direct3D11::ID3D11Buffer,
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11ShaderResourceView,
        ID3D11ShaderResourceView,
        ID3D11ShaderResourceView,
        ID3D11UnorderedAccessView,
        ID3D11Query,
    ),
    D3d11RgbaError,
> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS,
        D3D11_BUFFER_DESC, D3D11_USAGE_DYNAMIC, D3D11_CPU_ACCESS_WRITE,
    };

    let uw = width / 2;
    let uh = height / 2;

    let cb_desc = D3D11_BUFFER_DESC {
        ByteWidth: 16,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let mut cb_params = None;
    unsafe {
        device
            .CreateBuffer(&cb_desc, None, Some(&mut cb_params))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("cb: {:?}", e)))?;
    }
    let cb_params = cb_params.unwrap();

    let tex_desc_r8 = |w: u32, h: u32| D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8_UNORM.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut tex_y = None;
    let mut tex_u = None;
    let mut tex_v = None;
    unsafe {
        device
            .CreateTexture2D(&tex_desc_r8(width, height), None, Some(std::ptr::from_mut(&mut tex_y)))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("tex_y: {:?}", e)))?;
        device
            .CreateTexture2D(&tex_desc_r8(uw, uh), None, Some(std::ptr::from_mut(&mut tex_u)))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("tex_u: {:?}", e)))?;
        device
            .CreateTexture2D(&tex_desc_r8(uw, uh), None, Some(std::ptr::from_mut(&mut tex_v)))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("tex_v: {:?}", e)))?;
    }
    let tex_y = tex_y.unwrap();
    let tex_u = tex_u.unwrap();
    let tex_v = tex_v.unwrap();

    let tex_rgba_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex_rgba = None;
    unsafe {
        device
            .CreateTexture2D(&tex_rgba_desc, None, Some(std::ptr::from_mut(&mut tex_rgba)))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("tex_rgba: {:?}", e)))?;
    }
    let tex_rgba = tex_rgba.unwrap();

    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..tex_rgba_desc
    };
    let mut staging_rgba = None;
    unsafe {
        device
            .CreateTexture2D(&staging_desc, None, Some(std::ptr::from_mut(&mut staging_rgba)))
            .map_err(|e| D3d11RgbaError::CreateTexture(format!("staging: {:?}", e)))?;
    }
    let staging_rgba = staging_rgba.unwrap();

    let srv_desc_r8 = |_w: u32, _h: u32| D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8_UNORM.into(),
        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    };

    let mut srv_y = None;
    let mut srv_u = None;
    let mut srv_v = None;
    unsafe {
        let res_y = tex_y.clone().cast::<windows::Win32::Graphics::Direct3D11::ID3D11Resource>().map_err(|e| D3d11RgbaError::CreateShaderResourceView(e.to_string()))?;
        device.CreateShaderResourceView(&res_y, Some(&srv_desc_r8(width, height)), Some(&mut srv_y))
            .map_err(|e| D3d11RgbaError::CreateShaderResourceView(e.to_string()))?;
        let res_u = tex_u.clone().cast::<windows::Win32::Graphics::Direct3D11::ID3D11Resource>().map_err(|e| D3d11RgbaError::CreateShaderResourceView(e.to_string()))?;
        device.CreateShaderResourceView(&res_u, Some(&srv_desc_r8(uw, uh)), Some(&mut srv_u))
            .map_err(|e| D3d11RgbaError::CreateShaderResourceView(e.to_string()))?;
        let res_v = tex_v.clone().cast::<windows::Win32::Graphics::Direct3D11::ID3D11Resource>().map_err(|e| D3d11RgbaError::CreateShaderResourceView(e.to_string()))?;
        device.CreateShaderResourceView(&res_v, Some(&srv_desc_r8(uw, uh)), Some(&mut srv_v))
            .map_err(|e| D3d11RgbaError::CreateShaderResourceView(e.to_string()))?;
    }

    let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM.into(),
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
        },
    };
    let mut uav_rgba = None;
    unsafe {
        let res = tex_rgba.clone().cast::<windows::Win32::Graphics::Direct3D11::ID3D11Resource>().map_err(|e| D3d11RgbaError::CreateUnorderedAccessView(e.to_string()))?;
        device.CreateUnorderedAccessView(&res, Some(&uav_desc), Some(&mut uav_rgba))
            .map_err(|e| D3d11RgbaError::CreateUnorderedAccessView(e.to_string()))?;
    }

    let query_desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_EVENT,
        MiscFlags: 0,
    };
    let mut event_query = None;
    unsafe {
        device
            .CreateQuery(&query_desc, Some(&mut event_query))
            .map_err(|e| D3d11RgbaError::CreateQuery(e.to_string()))?;
    }
    let event_query = event_query.ok_or_else(|| D3d11RgbaError::CreateQuery("CreateQuery null".into()))?;

    Ok((
        cb_params,
        tex_y,
        tex_u,
        tex_v,
        tex_rgba,
        staging_rgba,
        srv_y.unwrap(),
        srv_u.unwrap(),
        srv_v.unwrap(),
        uav_rgba.unwrap(),
        event_query,
    ))
}
