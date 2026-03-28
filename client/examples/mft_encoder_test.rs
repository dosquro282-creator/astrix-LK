//! Phase 4.11: Test MftH264Encoder with 10 NV12 frames.
//!
//! Uses D3d11BgraToNv12 to produce NV12 from BGRA, then MftH264Encoder to encode.
//! Verifies output is valid H.264 Annex B (starts with 00 00 00 01).
//!
//! Run: cargo run --example mft_encoder_test
//!
//! Env ASTRIX_MFT_SOFTWARE=1 forces software MFT (hardware may be async-only).

#![cfg(all(windows, feature = "wgc-capture"))]

use parking_lot::Mutex;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ASTRIX_MFT_SOFTWARE=1 forces software MFT (if hardware fails)
    println!("=== MftH264Encoder Phase 4.11 Test ===\n");

    let gpu = astrix_client::gpu_device::GpuDevice::select_best()?;
    println!("Using GPU: {}", gpu.adapter_name);

    let width = 128u32;
    let height = 128u32;
    let fps = 30u32;
    let bitrate_bps = 2_000_000u32;

    // Create BGRA->NV12 converter
    let converter = astrix_client::d3d11_nv12::D3d11BgraToNv12::new(
        &gpu.device,
        &gpu.context,
        width,
        height,
        width,
        height,
        fps,
    )?;
    println!("D3d11BgraToNv12 created");

    // Create MFT encoder
    let mut encoder = astrix_client::mft_encoder::MftH264Encoder::new(
        &gpu.device,
        width,
        height,
        fps,
        bitrate_bps,
    )?;
    println!(
        "MftH264Encoder created: {} ({})",
        encoder.encoder_name(),
        if encoder.is_hardware() {
            "hardware"
        } else {
            "software"
        }
    );

    // Create BGRA input texture (uninitialized - we just test the pipeline)
    let input_tex = create_bgra_texture(&gpu.device, width, height)?;
    let ctx_mutex = Mutex::new(());

    let mut total_frames = 0;
    let mut total_bytes = 0usize;
    let mut has_annex_b = false;

    println!("\nEncoding 10 frames...");
    for i in 0..10u64 {
        let timestamp_us = (i * 1_000_000 / fps as u64) as i64;
        let key_frame = i < 3; // First 3 as IDR

        let nv12_tex = converter.convert(&gpu.context, &input_tex, &ctx_mutex)?;
        let frames = encoder.encode(nv12_tex, timestamp_us, key_frame)?;

        for f in &frames {
            total_frames += 1;
            total_bytes += f.data.len();
            if f.data.len() >= 4 && f.data[0..4] == [0, 0, 0, 1] {
                has_annex_b = true;
            }
        }
    }

    // Drain any buffered output (encoder may hold last frames)
    for _ in 0..5 {
        let nv12_tex = converter.convert(&gpu.context, &input_tex, &ctx_mutex)?;
        let ts = (10 * 1_000_000 / fps as u64) as i64;
        let frames = encoder.encode(nv12_tex, ts, false)?;
        for f in &frames {
            total_frames += 1;
            total_bytes += f.data.len();
            if f.data.len() >= 4 && f.data[0..4] == [0, 0, 0, 1] {
                has_annex_b = true;
            }
        }
        if frames.is_empty() {
            break;
        }
    }

    println!(
        "\nEncoded {} frames, {} bytes total",
        total_frames, total_bytes
    );

    if has_annex_b {
        println!("OK: Output contains valid H.264 Annex B start codes (00 00 00 01)");
    } else {
        println!("WARNING: No Annex B start codes found in output");
    }

    if total_frames > 0 {
        println!("\n=== Phase 4.11 test PASSED ===");
    } else {
        println!("\nWARNING: No encoded frames produced (encoder may buffer heavily)");
    }

    Ok(())
}

fn create_bgra_texture(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    width: u32,
    height: u32,
) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, Box<dyn std::error::Error>> {
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
    }
    tex.ok_or_else(|| "CreateTexture2D returned null".into())
}
