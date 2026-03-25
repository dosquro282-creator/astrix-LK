// Phase 3.2: NV12 → RGBA compute shader (decode path, zero CPU readback).
// NV12 biplanar: Y plane R8 (W×H), UV plane RG8 (W/2×H/2, CbCr interleaved).
//
// FULL_RANGE: Y/UV already 0-1, neutral UV at 0.5. Use when source is GPU shader→NV12→MFT.
// LIMITED_RANGE: Y 16-235, UV 16-240. Use for broadcast H.264.
// GAMMA_DECODER_ENABLED + GAMMA_DECODER: pow(rgb, 1/gamma) compile-time.
// GAMMA_RUNTIME: cbuffer CbGamma at b0, gamma>0 → pow(rgb, 1/gamma). From settings.
// UV_BILINEAR: bilinear UV sampling. CbNV12 at b1 when GAMMA_RUNTIME, else b0.

#if GAMMA_RUNTIME
cbuffer CbGamma : register(b0) {
    float gamma;
};
#endif

#if UV_BILINEAR
#if GAMMA_RUNTIME
cbuffer CbNV12 : register(b1) {
#else
cbuffer CbNV12 : register(b0) {
#endif
    uint width;
    uint height;
};
#endif

Texture2D<float>  TexY  : register(t0);   // Y plane (R8_UNORM)
Texture2D<float2> TexUV : register(t1);  // UV plane (R8G8_UNORM, Cb=x Cr=y)
RWTexture2D<float4> OutRGBA : register(u0);

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    int2 uv_pos = id.xy / 2;

    float y  = TexY[id.xy];
#if UV_BILINEAR
    int2 dim_uv = int2(width / 2, height / 2);
    float2 f = 0.5 * float2(id.x & 1, id.y & 1);
    int2 uv10 = min(uv_pos + int2(1, 0), dim_uv - 1);
    int2 uv01 = min(uv_pos + int2(0, 1), dim_uv - 1);
    int2 uv11 = min(uv_pos + int2(1, 1), dim_uv - 1);
    float2 uv00 = TexUV[uv_pos];
    float2 uv10_val = TexUV[uv10];
    float2 uv01_val = TexUV[uv01];
    float2 uv11_val = TexUV[uv11];
    float2 uv = lerp(lerp(uv00, uv10_val, f.x), lerp(uv01_val, uv11_val, f.x), f.y);
#else
    float2 uv = TexUV[uv_pos];
#endif

    #if FULL_RANGE
    // Full range: Y and UV already 0-1, neutral at 0.5
    float cb = uv.x - 0.5;
    float cr = uv.y - 0.5;
    #else
    // Limited range: Y 16-235, UV 16-240 → scale to 0-1
    y = (y - 16.0/255.0) * (255.0/219.0);
    float cb = (uv.x - 128.0/255.0) * (255.0/224.0);
    float cr = (uv.y - 128.0/255.0) * (255.0/224.0);
    #endif

    // BT.601 matrix (matches encoder in d3d11_nv12 BGRA→NV12)
    float r = saturate(y + 1.402 * cr);
    float g = saturate(y - 0.344 * cb - 0.714 * cr);
    float b = saturate(y + 1.772 * cb);

    #if GAMMA_DECODER_ENABLED
    float gamma_exp = 1.0 / GAMMA_DECODER;
    r = pow(r, gamma_exp);
    g = pow(g, gamma_exp);
    b = pow(b, gamma_exp);
    #endif

    #if GAMMA_RUNTIME
    if (gamma > 0.0) {
        float gamma_exp = 1.0 / gamma;
        r = pow(r, gamma_exp);
        g = pow(g, gamma_exp);
        b = pow(b, gamma_exp);
    }
    #endif

    #if OUTPUT_SRGB
    r = pow(r, 1.0/2.2);
    g = pow(g, 1.0/2.2);
    b = pow(b, 1.0/2.2);
    #endif

    OutRGBA[id.xy] = float4(r, g, b, 1.0);
}
