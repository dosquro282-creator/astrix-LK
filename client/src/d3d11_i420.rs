//! D3D11 Compute Shader: RGBA texture → I420 (Y, U, V planes).
//!
//! Phase 3 of d3dcap.md: compile HLSL, create SRV/UAV, dispatch, readback.
//! BT.601 coefficients; WGC uses ColorFormat::Rgba8 (R,G,B,A in memory).
//!
//! Phase 6.1: combined RGBA→downscale→I420 shader (bilinear, single pass).

#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCompile, D3DCOMPILE_DEBUG, D3DCOMPILE_SKIP_VALIDATION,
};
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext, ID3D11Query,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_UNORDERED_ACCESS, D3D11_BUFFER_DESC, D3D11_BUFFER_UAV,
    D3D11_BUFFER_UAV_FLAG_RAW, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_MAP_WRITE_DISCARD, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS, D3D11_SAMPLER_DESC,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_SRV,
    D3D11_TEXTURE2D_DESC, D3D11_UAV_DIMENSION_BUFFER, D3D11_UNORDERED_ACCESS_VIEW_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_TEXTURE_ADDRESS_CLAMP,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R32_TYPELESS,
    DXGI_FORMAT_R8G8B8A8_UNORM,
};

/// Phase 3: no-downscale shader (src == dst dimensions, two dispatches: Y then UV).
/// TODO-5: output buffers are RWByteAddressBuffer; each thread packs 4 pixels into one uint
/// so staging is 1 byte/pixel — allows direct memcpy on readback, no loop needed.
/// width (and width/2 for UV) must be divisible by 4 (true for all standard presets).
const HLSL_RGBA_TO_I420: &str = r"
// BT.601: Y = 0.299*R + 0.587*G + 0.114*B; Cb = (B-Y)/1.772 + 0.5; Cr = (R-Y)/1.402 + 0.5
// Each thread processes 4 horizontally adjacent pixels and packs them into one uint
// (byte0=pixel0, byte1=pixel1, byte2=pixel2, byte3=pixel3) via Store.
// phase 0: Y plane  (width x height pixels, width/4 uints per row)
// phase 1: U/V planes ((width/2) x (height/2) pixels, width/8 uints per row)
cbuffer Params : register(b0) {
    uint width;
    uint height;
    uint phase;
};
Texture2D<float4> rgbaTex : register(t0);
RWByteAddressBuffer outY : register(u0);
RWByteAddressBuffer outU : register(u1);
RWByteAddressBuffer outV : register(u2);

static const float3 kY = float3(0.299, 0.587, 0.114);
static const float kCbScale = 1.0 / 1.772;
static const float kCrScale = 1.0 / 1.402;

[numthreads(16, 16, 1)]
void main(uint3 gid : SV_GroupID, uint3 tid : SV_GroupThreadID) {
    // Each thread handles 4 pixels along X; gid.x steps in units of 4*16=64 pixels.
    uint x4 = (gid.x * 16u + tid.x) * 4u;
    uint y  = gid.y * 16u + tid.y;
    if (phase == 0u) {
        if (x4 >= width || y >= height) return;
        float4 c0 = rgbaTex[uint2(x4,     y)];
        float4 c1 = rgbaTex[uint2(x4 + 1u, y)];
        float4 c2 = rgbaTex[uint2(x4 + 2u, y)];
        float4 c3 = rgbaTex[uint2(x4 + 3u, y)];
        uint b0 = (uint)(saturate(dot(c0.rgb, kY)) * 255.0);
        uint b1 = (uint)(saturate(dot(c1.rgb, kY)) * 255.0);
        uint b2 = (uint)(saturate(dot(c2.rgb, kY)) * 255.0);
        uint b3 = (uint)(saturate(dot(c3.rgb, kY)) * 255.0);
        uint packed = b0 | (b1 << 8u) | (b2 << 16u) | (b3 << 24u);
        outY.Store((y * width + x4), packed);
    } else {
        uint uw = width >> 1u;
        uint uh = height >> 1u;
        if (x4 >= uw || y >= uh) return;
        // Each of the 4 UV pixels averages a 2x2 luma block.
        uint packed_u = 0u;
        uint packed_v = 0u;
        [unroll] for (uint i = 0u; i < 4u; i++) {
            uint cx = x4 + i;
            if (cx >= uw) break;
            float4 c00 = rgbaTex[uint2(cx * 2u,      y * 2u)];
            float4 c10 = rgbaTex[uint2(cx * 2u + 1u,  y * 2u)];
            float4 c01 = rgbaTex[uint2(cx * 2u,      y * 2u + 1u)];
            float4 c11 = rgbaTex[uint2(cx * 2u + 1u,  y * 2u + 1u)];
            float3 avg = (c00.rgb + c10.rgb + c01.rgb + c11.rgb) * 0.25;
            float Y  = dot(avg, kY);
            uint ub = (uint)(saturate((avg.b - Y) * kCbScale + 0.5) * 255.0);
            uint vb = (uint)(saturate((avg.r - Y) * kCrScale + 0.5) * 255.0);
            packed_u |= (ub << (i * 8u));
            packed_v |= (vb << (i * 8u));
        }
        outU.Store((y * uw + x4), packed_u);
        outV.Store((y * uw + x4), packed_v);
    }
}
";

/// Phase 6.1: combined RGBA→bilinear-downscale→I420 shader (single pass, src != dst).
/// TODO-5: RWByteAddressBuffer, 4 pixels packed per uint → 1 byte/pixel in staging.
/// cbuffer: src_width, src_height, dst_width, dst_height, phase (0=Y, 1=UV).
const HLSL_RGBA_TO_I420_SCALED: &str = r"
// BT.601 combined downscale + RGBA→I420 in one pass.
// Each thread processes 4 horizontally adjacent dst pixels, packs into one uint.
// phase 0: write Y plane (dst_width x dst_height)
// phase 1: write U/V planes ((dst_width/2) x (dst_height/2))
cbuffer Params : register(b0) {
    uint src_width;
    uint src_height;
    uint dst_width;
    uint dst_height;
    uint phase;
    uint _pad0;
    uint _pad1;
    uint _pad2;
};
Texture2D<float4> rgbaTex : register(t0);
RWByteAddressBuffer outY : register(u0);
RWByteAddressBuffer outU : register(u1);
RWByteAddressBuffer outV : register(u2);

SamplerState linearSampler : register(s0);

static const float3 kY  = float3(0.299, 0.587, 0.114);
static const float kCbScale = 1.0 / 1.772;
static const float kCrScale = 1.0 / 1.402;

float4 sampleBilinear(float2 uv) {
    return rgbaTex.SampleLevel(linearSampler, uv, 0);
}

[numthreads(16, 16, 1)]
void main(uint3 gid : SV_GroupID, uint3 tid : SV_GroupThreadID) {
    uint x4 = (gid.x * 16u + tid.x) * 4u;
    uint y  = gid.y * 16u + tid.y;
    float dw = (float)dst_width;
    float dh = (float)dst_height;
    if (phase == 0u) {
        if (x4 >= dst_width || y >= dst_height) return;
        uint packed = 0u;
        [unroll] for (uint i = 0u; i < 4u; i++) {
            uint xi = x4 + i;
            float2 uv = float2((xi + 0.5) / dw, (y + 0.5) / dh);
            float4 c = sampleBilinear(uv);
            uint b = (uint)(saturate(dot(c.rgb, kY)) * 255.0);
            packed |= (b << (i * 8u));
        }
        outY.Store((y * dst_width + x4), packed);
    } else {
        uint uw = dst_width  >> 1u;
        uint uh = dst_height >> 1u;
        if (x4 >= uw || y >= uh) return;
        uint packed_u = 0u;
        uint packed_v = 0u;
        [unroll] for (uint i = 0u; i < 4u; i++) {
            uint cx = x4 + i;
            if (cx >= uw) break;
            float2 uv00 = float2((cx * 2u + 0.5) / dw, (y * 2u + 0.5) / dh);
            float2 uv10 = float2((cx * 2u + 1.5) / dw, (y * 2u + 0.5) / dh);
            float2 uv01 = float2((cx * 2u + 0.5) / dw, (y * 2u + 1.5) / dh);
            float2 uv11 = float2((cx * 2u + 1.5) / dw, (y * 2u + 1.5) / dh);
            float3 avg = (sampleBilinear(uv00).rgb + sampleBilinear(uv10).rgb
                        + sampleBilinear(uv01).rgb + sampleBilinear(uv11).rgb) * 0.25;
            float Y  = dot(avg, kY);
            uint ub = (uint)(saturate((avg.b - Y) * kCbScale + 0.5) * 255.0);
            uint vb = (uint)(saturate((avg.r - Y) * kCrScale + 0.5) * 255.0);
            packed_u |= (ub << (i * 8u));
            packed_v |= (vb << (i * 8u));
        }
        outU.Store((y * uw + x4), packed_u);
        outV.Store((y * uw + x4), packed_v);
    }
}
";

/// Result of GPU RGBA→I420 conversion (owned planes for I420Buffer / capture_frame).
/// Reuse the same instance across frames: convert() calls ensure_size() which only
/// reallocates when dimensions change, avoiding ~3 MB/frame heap churn at 1080p60.
pub struct I420Planes {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl I420Planes {
    /// Create an empty instance to be filled by convert().
    pub fn new_empty() -> Self {
        Self {
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
            width: 0,
            height: 0,
        }
    }

    /// Resize buffers to match dimensions without reallocating if already large enough.
    pub fn ensure_size(&mut self, w: u32, h: u32) {
        let y_len = (w * h) as usize;
        let uv_len = ((w / 2) * (h / 2)) as usize;
        if self.y.len() != y_len {
            self.y.resize(y_len, 0);
        }
        if self.u.len() != uv_len {
            self.u.resize(uv_len, 0);
            self.v.resize(uv_len, 0);
        }
        self.width = w;
        self.height = h;
    }
}

/// Phase 6.2: timing breakdown for one GPU convert call (nanoseconds).
#[derive(Default, Clone, Copy, Debug)]
pub struct GpuConvertTiming {
    /// Time to dispatch compute shader (Y + UV passes).
    pub dispatch_ns: u64,
    /// Time to CopyResource GPU→staging.
    pub copy_ns: u64,
    /// Time to Map + read staging buffers.
    pub map_ns: u64,
    /// Total wall time of convert().
    pub total_ns: u64,
}

/// D3D11 pipeline: one compute shader + Y/U/V buffers (GPU + staging) + UAVs. SRV created per convert.
/// Double-buffered staging: we Map the buffer that was filled in the *previous* call, so the GPU had
/// a full frame to finish the copy (avoids E_INVALIDARG on some drivers when mapping immediately).
/// Two event queries, one per staging set.
pub struct D3d11RgbaToI420 {
    cs: ID3D11ComputeShader,
    cb_params: ID3D11Buffer,
    buf_y: ID3D11Buffer,
    buf_u: ID3D11Buffer,
    buf_v: ID3D11Buffer,
    /// Staging buffers: [0] and [1]; we alternate CopyResource and Map between them.
    staging_y: [ID3D11Buffer; 2],
    staging_u: [ID3D11Buffer; 2],
    staging_v: [ID3D11Buffer; 2],
    uav_y: ID3D11UnorderedAccessView,
    uav_u: ID3D11UnorderedAccessView,
    uav_v: ID3D11UnorderedAccessView,
    event_query: [ID3D11Query; 2],
    /// Index of the staging set we last copied to (next convert() will Map this set after wait).
    last_write_idx: std::sync::atomic::AtomicUsize,
    width: u32,
    height: u32,
}

#[derive(Debug)]
pub enum D3d11I420Error {
    Compile(String),
    CreateBuffer(String),
    CreateQuery(String),
    CreateSampler(String),
    CreateShaderResourceView(String),
    CreateUnorderedAccessView(String),
    Map(String),
}

impl D3d11RgbaToI420 {
    /// Build pipeline for the given dimensions.
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self, D3d11I420Error> {
        let cs = compile_cs(device)?;
        let (cb_params, buf_y, buf_u, buf_v, staging_y, staging_u, staging_v, uav_y, uav_u, uav_v) =
            create_buffers_and_uavs(device, width, height)?;
        let event_query = [create_event_query(device)?, create_event_query(device)?];
        // Pre-signal both queries so the first convert() call can skip GetData without a special case.
        // The immediate context is not shared yet (new() is called before any threads use it).
        unsafe {
            let ctx: ID3D11DeviceContext = device
                .GetImmediateContext()
                .map_err(|e| D3d11I420Error::CreateQuery(format!("GetImmediateContext: {}", e)))?;
            ctx.End(&event_query[0]);
            ctx.End(&event_query[1]);
            ctx.Flush();
        }
        Ok(Self {
            cs,
            cb_params,
            buf_y,
            buf_u,
            buf_v,
            staging_y,
            staging_u,
            staging_v,
            uav_y,
            uav_u,
            uav_v,
            event_query,
            last_write_idx: std::sync::atomic::AtomicUsize::new(1),
            width,
            height,
        })
    }

    /// Run compute: texture (pool copy) → Y/U/V buffers, copy to staging, read back.
    /// Returns I420Planes and GpuConvertTiming (Phase 6.2).
    ///
    /// `context_mutex`: shared lock protecting the Immediate Context from concurrent use
    /// by the WGC callback thread. The lock is held only during GPU command submission
    /// (Dispatch, CopyResource) and released before GetData polling, so WGC is never
    /// blocked for more than a few microseconds.
    pub fn convert(
        &self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
        context_mutex: &parking_lot::Mutex<()>,
        out: &mut I420Planes,
    ) -> Result<GpuConvertTiming, D3d11I420Error> {
        let t_total = std::time::Instant::now();
        let w = self.width;
        let h = self.height;
        let uw = w / 2;
        let uh = h / 2;

        let srv = create_srv_for_texture(device, texture)?;

        let uavs = [
            Some(self.uav_y.clone()),
            Some(self.uav_u.clone()),
            Some(self.uav_v.clone()),
        ];
        let uav_counts = [u32::MAX, u32::MAX, u32::MAX];

        // Double-buffered staging: map the set written in the previous call, write to the other.
        // last_write_idx starts at 1, so first call: map_idx=1 (pre-signalled), write_idx=0.
        let last = self
            .last_write_idx
            .load(std::sync::atomic::Ordering::Relaxed);
        let map_idx = last;
        let write_idx = 1 - last;

        let t_dispatch = std::time::Instant::now();
        // Hold context_mutex only for GPU command submission; release before GetData polling
        // so the WGC callback is never blocked waiting for the GPU.
        {
            let _ctx_guard = context_mutex.lock();
            let params: [u32; 4] = [w, h, 0, 0];
            unsafe {
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context
                    .Map(
                        &self.cb_params,
                        0,
                        D3D11_MAP_WRITE_DISCARD,
                        0,
                        Some(&mut mapped as *mut _),
                    )
                    .map_err(|e| D3d11I420Error::Map(e.to_string()))?;
                std::ptr::copy_nonoverlapping(
                    params.as_ptr() as *const u8,
                    mapped.pData as *mut u8,
                    16,
                );
                context.Unmap(&self.cb_params, 0);

                context.CSSetShader(Some(&self.cs), None);
                context.CSSetConstantBuffers(0, Some(&[Some(self.cb_params.clone())]));
                context.CSSetShaderResources(0, Some(&[Some(srv.clone())]));
                context.CSSetUnorderedAccessViews(
                    0,
                    3,
                    Some(uavs.as_ptr()),
                    Some(uav_counts.as_ptr()),
                );
                // Each thread covers 4 pixels along X → divide X dispatch by 4.
                context.Dispatch((w / 4 + 15) / 16, (h + 15) / 16, 1);

                let params_uv: [u32; 4] = [w, h, 1, 0];
                context
                    .Map(
                        &self.cb_params,
                        0,
                        D3D11_MAP_WRITE_DISCARD,
                        0,
                        Some(&mut mapped as *mut _),
                    )
                    .map_err(|e| D3d11I420Error::Map(e.to_string()))?;
                std::ptr::copy_nonoverlapping(
                    params_uv.as_ptr() as *const u8,
                    mapped.pData as *mut u8,
                    16,
                );
                context.Unmap(&self.cb_params, 0);
                context.Dispatch((uw / 4 + 15) / 16, (uh + 15) / 16, 1);

                let uavs_clear = [None, None, None];
                let counts_clear = [0u32; 3];
                context.CSSetUnorderedAccessViews(
                    0,
                    3,
                    Some(uavs_clear.as_ptr()),
                    Some(counts_clear.as_ptr()),
                );
                context.CSSetShaderResources(0, Some(&[None]));
                context.CSSetShader(None, None);
            }
            // Lock released — WGC can now use the context while we wait for previous staging.
        }
        let dispatch_ns = t_dispatch.elapsed().as_nanos() as u64;

        let t_copy = std::time::Instant::now();
        // Wait for the staging set filled in the *previous* call — no lock needed (GPU polling only).
        // On the first call both queries are pre-signalled, so this returns immediately.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        unsafe {
            loop {
                match context.GetData(&self.event_query[map_idx], None, 0, 0) {
                    Ok(()) => break,
                    Err(_) => {
                        if std::time::Instant::now() > deadline {
                            return Err(D3d11I420Error::Map("GetData timeout (>200ms)".into()));
                        }
                        // sleep(1ms) is more predictable than yield_now on Windows with
                        // timeBeginPeriod(1): avoids busy-spin while GPU finishes.
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
            }
        }
        let copy_ns = t_copy.elapsed().as_nanos() as u64;

        let y_len = (w * h) as usize;
        let uv_len = (uw * uh) as usize;
        // Reuse caller-provided buffers — no allocation after first frame.
        out.ensure_size(w, h);

        // Map staging buffers — no lock needed (staging buffers are private to encoder thread).
        // Staging is 1 byte/pixel (packed by shader): direct memcpy, no per-element loop.
        let t_map = std::time::Instant::now();
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(
                    &self.staging_y[map_idx],
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(std::ptr::addr_of_mut!(mapped)),
                )
                .map_err(|e| D3d11I420Error::Map(e.to_string()))?;
            std::ptr::copy_nonoverlapping(mapped.pData as *const u8, out.y.as_mut_ptr(), y_len);
            context.Unmap(&self.staging_y[map_idx], 0);

            context
                .Map(
                    &self.staging_u[map_idx],
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(std::ptr::addr_of_mut!(mapped)),
                )
                .map_err(|e| D3d11I420Error::Map(e.to_string()))?;
            std::ptr::copy_nonoverlapping(mapped.pData as *const u8, out.u.as_mut_ptr(), uv_len);
            context.Unmap(&self.staging_u[map_idx], 0);

            context
                .Map(
                    &self.staging_v[map_idx],
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(std::ptr::addr_of_mut!(mapped)),
                )
                .map_err(|e| D3d11I420Error::Map(e.to_string()))?;
            std::ptr::copy_nonoverlapping(mapped.pData as *const u8, out.v.as_mut_ptr(), uv_len);
            context.Unmap(&self.staging_v[map_idx], 0);
        }
        let map_ns = t_map.elapsed().as_nanos() as u64;

        // Copy current GPU results into the other staging set for next Map.
        // Must hold context_mutex since CopyResource touches the shared Immediate Context.
        {
            let _ctx_guard = context_mutex.lock();
            unsafe {
                context.CopyResource(&self.staging_y[write_idx], &self.buf_y);
                context.CopyResource(&self.staging_u[write_idx], &self.buf_u);
                context.CopyResource(&self.staging_v[write_idx], &self.buf_v);
                context.Flush();
                context.End(&self.event_query[write_idx]);
                // No second Flush: End() is a fence, one Flush before it is sufficient.
            }
        }
        self.last_write_idx
            .store(write_idx, std::sync::atomic::Ordering::Relaxed);

        let total_ns = t_total.elapsed().as_nanos() as u64;
        Ok(GpuConvertTiming {
            dispatch_ns,
            copy_ns,
            map_ns,
            total_ns,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase 6.1: D3d11RgbaToI420Scaled — combined downscale + RGBA→I420 in one pass.
// Uses a bilinear sampler; output buffers are sized to (dst_width × dst_height).
// When src == dst this is equivalent to D3d11RgbaToI420 but slightly slower due
// to sampler overhead; prefer D3d11RgbaToI420 when no scaling is needed.
// ─────────────────────────────────────────────────────────────────────────────

/// D3D11 pipeline: RGBA texture → bilinear downscale → I420 (single compute pass).
/// Output buffers are sized to dst_width × dst_height; no CPU scaling needed.
pub struct D3d11RgbaToI420Scaled {
    cs: ID3D11ComputeShader,
    cb_params: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    buf_y: ID3D11Buffer,
    buf_u: ID3D11Buffer,
    buf_v: ID3D11Buffer,
    staging_y: [ID3D11Buffer; 2],
    staging_u: [ID3D11Buffer; 2],
    staging_v: [ID3D11Buffer; 2],
    uav_y: ID3D11UnorderedAccessView,
    uav_u: ID3D11UnorderedAccessView,
    uav_v: ID3D11UnorderedAccessView,
    event_query: [ID3D11Query; 2],
    last_write_idx: std::sync::atomic::AtomicUsize,
    /// Source (capture) dimensions — used to detect resize (Phase 6.3).
    pub src_width: u32,
    pub src_height: u32,
    /// Destination (preset) dimensions.
    pub dst_width: u32,
    pub dst_height: u32,
}

impl D3d11RgbaToI420Scaled {
    /// Build pipeline for given source and destination dimensions.
    pub fn new(
        device: &ID3D11Device,
        src_width: u32,
        src_height: u32,
        dst_width: u32,
        dst_height: u32,
    ) -> Result<Self, D3d11I420Error> {
        let cs = compile_cs_scaled(device)?;
        let sampler = create_linear_sampler(device)?;
        let (cb_params, buf_y, buf_u, buf_v, staging_y, staging_u, staging_v, uav_y, uav_u, uav_v) =
            create_buffers_and_uavs(device, dst_width, dst_height)?;
        let event_query = [create_event_query(device)?, create_event_query(device)?];
        // Pre-signal both queries so the first convert() call can skip GetData without a special case.
        unsafe {
            let ctx: ID3D11DeviceContext = device
                .GetImmediateContext()
                .map_err(|e| D3d11I420Error::CreateQuery(format!("GetImmediateContext: {}", e)))?;
            ctx.End(&event_query[0]);
            ctx.End(&event_query[1]);
            ctx.Flush();
        }
        Ok(Self {
            cs,
            cb_params,
            sampler,
            buf_y,
            buf_u,
            buf_v,
            staging_y,
            staging_u,
            staging_v,
            uav_y,
            uav_u,
            uav_v,
            event_query,
            last_write_idx: std::sync::atomic::AtomicUsize::new(1),
            src_width,
            src_height,
            dst_width,
            dst_height,
        })
    }

    /// Run combined downscale + RGBA→I420 compute pass, then readback.
    /// Returns I420Planes (at dst dimensions) and GpuConvertTiming.
    ///
    /// `context_mutex`: shared lock protecting the Immediate Context from concurrent use
    /// by the WGC callback thread. The lock is held only during GPU command submission
    /// and released before GetData polling so WGC is never blocked for long.
    pub fn convert(
        &self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
        context_mutex: &parking_lot::Mutex<()>,
        out: &mut I420Planes,
    ) -> Result<GpuConvertTiming, D3d11I420Error> {
        let t_total = std::time::Instant::now();
        let sw = self.src_width;
        let sh = self.src_height;
        let dw = self.dst_width;
        let dh = self.dst_height;
        let uw = dw / 2;
        let uh = dh / 2;

        let srv = create_srv_for_texture(device, texture)?;

        let write_params = |phase: u32| -> [u32; 8] { [sw, sh, dw, dh, phase, 0, 0, 0] };

        let uavs = [
            Some(self.uav_y.clone()),
            Some(self.uav_u.clone()),
            Some(self.uav_v.clone()),
        ];
        let uav_counts = [u32::MAX, u32::MAX, u32::MAX];

        // Double-buffered staging: map the set written in the previous call, write to the other.
        // last_write_idx starts at 1, so first call: map_idx=1 (pre-signalled), write_idx=0.
        let last = self
            .last_write_idx
            .load(std::sync::atomic::Ordering::Relaxed);
        let map_idx = last;
        let write_idx = 1 - last;

        let t_dispatch = std::time::Instant::now();
        // Hold context_mutex only for GPU command submission; release before GetData polling
        // so the WGC callback is never blocked waiting for the GPU.
        {
            let _ctx_guard = context_mutex.lock();
            unsafe {
                context.CSSetShader(Some(&self.cs), None);
                context.CSSetConstantBuffers(0, Some(&[Some(self.cb_params.clone())]));
                context.CSSetShaderResources(0, Some(&[Some(srv.clone())]));
                context.CSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
                context.CSSetUnorderedAccessViews(
                    0,
                    3,
                    Some(uavs.as_ptr()),
                    Some(uav_counts.as_ptr()),
                );

                let p = write_params(0);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context
                    .Map(
                        &self.cb_params,
                        0,
                        D3D11_MAP_WRITE_DISCARD,
                        0,
                        Some(&mut mapped as *mut _),
                    )
                    .map_err(|e| D3d11I420Error::Map(e.to_string()))?;
                std::ptr::copy_nonoverlapping(p.as_ptr() as *const u8, mapped.pData as *mut u8, 32);
                context.Unmap(&self.cb_params, 0);
                // Each thread covers 4 pixels along X → divide X dispatch by 4.
                context.Dispatch((dw / 4 + 15) / 16, (dh + 15) / 16, 1);
                context.Flush();

                let p = write_params(1);
                context
                    .Map(
                        &self.cb_params,
                        0,
                        D3D11_MAP_WRITE_DISCARD,
                        0,
                        Some(&mut mapped as *mut _),
                    )
                    .map_err(|e| D3d11I420Error::Map(e.to_string()))?;
                std::ptr::copy_nonoverlapping(p.as_ptr() as *const u8, mapped.pData as *mut u8, 32);
                context.Unmap(&self.cb_params, 0);
                context.Dispatch((uw / 4 + 15) / 16, (uh + 15) / 16, 1);

                let uavs_clear = [None, None, None];
                let counts_clear = [0u32; 3];
                context.CSSetUnorderedAccessViews(
                    0,
                    3,
                    Some(uavs_clear.as_ptr()),
                    Some(counts_clear.as_ptr()),
                );
                context.CSSetShaderResources(0, Some(&[None]));
                context.CSSetSamplers(0, Some(&[None]));
                context.CSSetShader(None, None);
            }
            // Lock released — WGC can now use the context while we wait for previous staging.
        }
        let dispatch_ns = t_dispatch.elapsed().as_nanos() as u64;

        let t_copy = std::time::Instant::now();
        // Wait for the staging set filled in the *previous* call — no lock needed (GPU polling only).
        // On the first call both queries are pre-signalled, so this returns immediately.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        unsafe {
            loop {
                match context.GetData(&self.event_query[map_idx], None, 0, 0) {
                    Ok(()) => break,
                    Err(_) => {
                        if std::time::Instant::now() > deadline {
                            return Err(D3d11I420Error::Map("GetData timeout (>200ms)".into()));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
            }
        }
        let copy_ns = t_copy.elapsed().as_nanos() as u64;

        let y_len = (dw * dh) as usize;
        let uv_len = (uw * uh) as usize;
        // Reuse caller-provided buffers — no allocation after first frame.
        out.ensure_size(dw, dh);

        // Map staging buffers — no lock needed (staging buffers are private to encoder thread).
        // Staging is 1 byte/pixel (packed by shader): direct memcpy, no per-element loop.
        let t_map = std::time::Instant::now();
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(
                    &self.staging_y[map_idx],
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(std::ptr::addr_of_mut!(mapped)),
                )
                .map_err(|e| D3d11I420Error::Map(e.to_string()))?;
            std::ptr::copy_nonoverlapping(mapped.pData as *const u8, out.y.as_mut_ptr(), y_len);
            context.Unmap(&self.staging_y[map_idx], 0);

            context
                .Map(
                    &self.staging_u[map_idx],
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(std::ptr::addr_of_mut!(mapped)),
                )
                .map_err(|e| D3d11I420Error::Map(e.to_string()))?;
            std::ptr::copy_nonoverlapping(mapped.pData as *const u8, out.u.as_mut_ptr(), uv_len);
            context.Unmap(&self.staging_u[map_idx], 0);

            context
                .Map(
                    &self.staging_v[map_idx],
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(std::ptr::addr_of_mut!(mapped)),
                )
                .map_err(|e| D3d11I420Error::Map(e.to_string()))?;
            std::ptr::copy_nonoverlapping(mapped.pData as *const u8, out.v.as_mut_ptr(), uv_len);
            context.Unmap(&self.staging_v[map_idx], 0);
        }
        let map_ns = t_map.elapsed().as_nanos() as u64;

        // Copy current GPU results into the other staging set for next Map.
        // Must hold context_mutex since CopyResource touches the shared Immediate Context.
        {
            let _ctx_guard = context_mutex.lock();
            unsafe {
                context.CopyResource(&self.staging_y[write_idx], &self.buf_y);
                context.CopyResource(&self.staging_u[write_idx], &self.buf_u);
                context.CopyResource(&self.staging_v[write_idx], &self.buf_v);
                context.Flush();
                context.End(&self.event_query[write_idx]);
                // No second Flush: End() is a fence, one Flush before it is sufficient.
            }
        }
        self.last_write_idx
            .store(write_idx, std::sync::atomic::Ordering::Relaxed);

        let total_ns = t_total.elapsed().as_nanos() as u64;
        Ok(GpuConvertTiming {
            dispatch_ns,
            copy_ns,
            map_ns,
            total_ns,
        })
    }
}

fn create_event_query(device: &ID3D11Device) -> Result<ID3D11Query, D3d11I420Error> {
    let desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_EVENT,
        MiscFlags: 0,
    };
    let mut query = None;
    unsafe {
        device
            .CreateQuery(&desc, Some(&mut query))
            .map_err(|e| D3d11I420Error::CreateQuery(e.to_string()))?;
    }
    query.ok_or_else(|| D3d11I420Error::CreateQuery("CreateQuery returned null".into()))
}

fn compile_cs(device: &ID3D11Device) -> Result<ID3D11ComputeShader, D3d11I420Error> {
    compile_cs_from_source(device, HLSL_RGBA_TO_I420, HLSL_RGBA_TO_I420.len())
}

fn compile_cs_from_source(
    device: &ID3D11Device,
    hlsl: &str,
    hlsl_len: usize,
) -> Result<ID3D11ComputeShader, D3d11I420Error> {
    let source = std::ffi::CString::new(hlsl)
        .map_err(|_| D3d11I420Error::Compile("HLSL string null".into()))?;
    let entry = std::ffi::CString::new("main").unwrap();
    let profile = std::ffi::CString::new("cs_5_0").unwrap();
    let flags = D3DCOMPILE_DEBUG | D3DCOMPILE_SKIP_VALIDATION;

    let mut blob = None;
    let mut err_blob = None;
    unsafe {
        use windows::core::PCSTR;
        let hr = D3DCompile(
            source.as_ptr() as *const _,
            hlsl_len,
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
            return Err(D3d11I420Error::Compile(msg));
        }
        let blob = blob.ok_or_else(|| D3d11I420Error::Compile("no blob".into()))?;
        let bytecode =
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());

        let mut cs = None;
        device
            .CreateComputeShader(bytecode, None, Some(&mut cs))
            .map_err(|e| D3d11I420Error::Compile(e.to_string()))?;
        cs.ok_or_else(|| D3d11I420Error::Compile("CreateComputeShader returned null".into()))
    }
}

fn create_srv_for_texture(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11ShaderResourceView, D3d11I420Error> {
    let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut tex_desc) };
    let srv_format = match tex_desc.Format {
        f if f == DXGI_FORMAT_B8G8R8A8_UNORM.into() => DXGI_FORMAT_B8G8R8A8_UNORM,
        f if f == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB.into() => DXGI_FORMAT_B8G8R8A8_UNORM,
        _ => DXGI_FORMAT_R8G8B8A8_UNORM,
    };
    let anonymous = unsafe {
        let mut u = D3D11_SHADER_RESOURCE_VIEW_DESC_0::default();
        u.Texture2D = D3D11_TEX2D_SRV {
            MostDetailedMip: 0,
            MipLevels: 1,
        };
        u
    };
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: srv_format.into(),
        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
        Anonymous: anonymous,
    };
    let mut srv = None;
    unsafe {
        let resource: windows::Win32::Graphics::Direct3D11::ID3D11Resource = texture
            .clone()
            .cast()
            .map_err(|e| D3d11I420Error::CreateShaderResourceView(e.to_string()))?;
        device
            .CreateShaderResourceView(&resource, Some(&desc), Some(&mut srv))
            .map_err(|e| D3d11I420Error::CreateShaderResourceView(e.to_string()))?;
    }
    srv.ok_or_else(|| {
        D3d11I420Error::CreateShaderResourceView("CreateShaderResourceView returned null".into())
    })
}

fn compile_cs_scaled(device: &ID3D11Device) -> Result<ID3D11ComputeShader, D3d11I420Error> {
    compile_cs_from_source(
        device,
        HLSL_RGBA_TO_I420_SCALED,
        HLSL_RGBA_TO_I420_SCALED.len(),
    )
}

fn create_linear_sampler(device: &ID3D11Device) -> Result<ID3D11SamplerState, D3d11I420Error> {
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
            .map_err(|e| D3d11I420Error::CreateSampler(e.to_string()))?;
    }
    sampler.ok_or_else(|| D3d11I420Error::CreateSampler("CreateSamplerState returned null".into()))
}

fn create_buffers_and_uavs(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<
    (
        ID3D11Buffer,
        ID3D11Buffer,
        ID3D11Buffer,
        ID3D11Buffer,
        [ID3D11Buffer; 2],
        [ID3D11Buffer; 2],
        [ID3D11Buffer; 2],
        ID3D11UnorderedAccessView,
        ID3D11UnorderedAccessView,
        ID3D11UnorderedAccessView,
    ),
    D3d11I420Error,
> {
    let y_count = width * height;
    let uv_count = (width / 2) * (height / 2);
    // 16 bytes for the no-scale shader (4 × u32); scaled shader uses 32 bytes (8 × u32).
    // We always allocate 32 bytes so the same function works for both.
    let cb_size = 32u32;

    let cb_desc = D3D11_BUFFER_DESC {
        ByteWidth: cb_size,
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
            .map_err(|e| D3d11I420Error::CreateBuffer(e.to_string()))?;
    }
    let cb_params = cb_params.unwrap();

    // GPU buffers: RWByteAddressBuffer — 1 byte/pixel, ByteWidth must be multiple of 4.
    // MiscFlags = D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS required for raw UAV.
    let buf_desc = |num_elems: u32| {
        let aligned = (num_elems + 3) & !3;
        D3D11_BUFFER_DESC {
            ByteWidth: aligned,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS.0 as u32,
            StructureByteStride: 0,
        }
    };
    let mut buf_y = None;
    let mut buf_u = None;
    let mut buf_v = None;
    unsafe {
        device
            .CreateBuffer(&buf_desc(y_count), None, Some(&mut buf_y))
            .map_err(|e| D3d11I420Error::CreateBuffer(format!("buf_y: {}", e)))?;
        device
            .CreateBuffer(&buf_desc(uv_count), None, Some(&mut buf_u))
            .map_err(|e| D3d11I420Error::CreateBuffer(format!("buf_u: {}", e)))?;
        device
            .CreateBuffer(&buf_desc(uv_count), None, Some(&mut buf_v))
            .map_err(|e| D3d11I420Error::CreateBuffer(format!("buf_v: {}", e)))?;
    }
    let buf_y = buf_y.unwrap();
    let buf_u = buf_u.unwrap();
    let buf_v = buf_v.unwrap();

    // Staging: 1 byte/pixel (same size as GPU buffer), 16-byte aligned for driver compatibility.
    let staging_desc = |num_elems: u32| {
        let aligned_bytes = (num_elems + 15) & 0xFFFF_FFF0;
        D3D11_BUFFER_DESC {
            ByteWidth: aligned_bytes,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        }
    };
    let mut sy0 = None;
    let mut sy1 = None;
    let mut su0 = None;
    let mut su1 = None;
    let mut sv0 = None;
    let mut sv1 = None;
    unsafe {
        device
            .CreateBuffer(&staging_desc(y_count), None, Some(&mut sy0))
            .map_err(|e| D3d11I420Error::CreateBuffer(format!("staging_y[0]: {}", e)))?;
        device
            .CreateBuffer(&staging_desc(y_count), None, Some(&mut sy1))
            .map_err(|e| D3d11I420Error::CreateBuffer(format!("staging_y[1]: {}", e)))?;
        device
            .CreateBuffer(&staging_desc(uv_count), None, Some(&mut su0))
            .map_err(|e| D3d11I420Error::CreateBuffer(format!("staging_u[0]: {}", e)))?;
        device
            .CreateBuffer(&staging_desc(uv_count), None, Some(&mut su1))
            .map_err(|e| D3d11I420Error::CreateBuffer(format!("staging_u[1]: {}", e)))?;
        device
            .CreateBuffer(&staging_desc(uv_count), None, Some(&mut sv0))
            .map_err(|e| D3d11I420Error::CreateBuffer(format!("staging_v[0]: {}", e)))?;
        device
            .CreateBuffer(&staging_desc(uv_count), None, Some(&mut sv1))
            .map_err(|e| D3d11I420Error::CreateBuffer(format!("staging_v[1]: {}", e)))?;
    }
    let staging_y = [sy0.unwrap(), sy1.unwrap()];
    let staging_u = [su0.unwrap(), su1.unwrap()];
    let staging_v = [sv0.unwrap(), sv1.unwrap()];

    // UAV for RWByteAddressBuffer: DXGI_FORMAT_R32_TYPELESS + RAW flag.
    // NumElements = number of 4-byte words covering the buffer.
    let uav_desc = |num_elems: u32| {
        let num_words = (num_elems + 3) / 4;
        D3D11_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS.into(),
            ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: num_words,
                    Flags: D3D11_BUFFER_UAV_FLAG_RAW.0 as u32,
                },
            },
        }
    };
    let mut uav_y = None;
    let mut uav_u = None;
    let mut uav_v = None;
    unsafe {
        let desc_y = uav_desc(y_count);
        device
            .CreateUnorderedAccessView(&buf_y, Some(&desc_y), Some(&mut uav_y))
            .map_err(|e| D3d11I420Error::CreateUnorderedAccessView(format!("uav_y: {}", e)))?;

        let desc_u = uav_desc(uv_count);
        device
            .CreateUnorderedAccessView(&buf_u, Some(&desc_u), Some(&mut uav_u))
            .map_err(|e| D3d11I420Error::CreateUnorderedAccessView(format!("uav_u: {}", e)))?;

        let desc_v = uav_desc(uv_count);
        device
            .CreateUnorderedAccessView(&buf_v, Some(&desc_v), Some(&mut uav_v))
            .map_err(|e| D3d11I420Error::CreateUnorderedAccessView(format!("uav_v: {}", e)))?;
    }

    Ok((
        cb_params,
        buf_y,
        buf_u,
        buf_v,
        staging_y,
        staging_u,
        staging_v,
        uav_y.unwrap(),
        uav_u.unwrap(),
        uav_v.unwrap(),
    ))
}
