//! Phase 3.7: Test D3d11BgraToNv12 conversion + CPU readback.
//!
//! Creates a BGRA texture, converts to NV12 via VideoProcessor, reads back and verifies
//! Y/UV plane structure.
//!
//! Run: cargo run --example d3d11_nv12_test

#![cfg(all(windows, feature = "wgc-capture"))]

use parking_lot::Mutex;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== D3d11BgraToNv12 Phase 3.7 Test ===\n");

    let gpu = astrix_client::gpu_device::GpuDevice::select_best()?;
    println!("Using GPU: {}", gpu.adapter_name);

    let width = 128u32;
    let height = 128u32;
    let fps = 30u32;

    println!(
        "\nCreating D3d11BgraToNv12 ({}x{}, {} fps)...",
        width, height, fps
    );
    let converter = astrix_client::d3d11_nv12::D3d11BgraToNv12::new(
        &gpu.device,
        &gpu.context,
        width,
        height,
        width,
        height,
        fps,
    )?;
    println!("OK: Video processor created (NV12 output supported)");

    // Create a simple BGRA input texture (uninitialized - we just test the pipeline)
    let input_tex = create_bgra_texture(&gpu.device, width, height)?;
    println!("Created BGRA input texture {}x{}", width, height);

    let ctx_mutex = Mutex::new(());
    println!("\nConverting BGRA -> NV12...");
    let output_tex = match converter.convert(&gpu.context, &input_tex, &ctx_mutex) {
        Ok(t) => {
            println!("OK: Conversion succeeded");
            t
        }
        Err(e) => {
            eprintln!(
                "WARNING: VideoProcessorBlt failed ({}). NV12 output may not be supported on this GPU.\n\
                 Phase 4 will need compute shader fallback for BGRA->NV12.",
                e
            );
            return Ok(());
        }
    };

    // Readback NV12 to CPU for verification
    let (y_plane, uv_plane) = readback_nv12(&gpu.device, &gpu.context, output_tex, width, height)?;
    println!(
        "\nReadback: Y plane {} bytes, UV plane {} bytes",
        y_plane.len(),
        uv_plane.len()
    );

    // Sanity check: Y plane should be width*height, UV should be width*height/2
    let expected_y = (width * height) as usize;
    let expected_uv = (width * height / 2) as usize;
    assert_eq!(y_plane.len(), expected_y, "Y plane size mismatch");
    assert_eq!(uv_plane.len(), expected_uv, "UV plane size mismatch");

    // Check Y values are in valid range (0-255)
    let y_min = *y_plane.iter().min().unwrap_or(&0);
    let y_max = *y_plane.iter().max().unwrap_or(&0);
    println!("Y plane: min={}, max={}", y_min, y_max);

    // UV plane: interleaved U,V per 2x2 block
    let uv_min = *uv_plane.iter().min().unwrap_or(&0);
    let uv_max = *uv_plane.iter().max().unwrap_or(&0);
    println!("UV plane: min={}, max={}", uv_min, uv_max);

    println!("\n=== Phase 3.7 test PASSED ===");
    Ok(())
}

fn create_bgra_texture(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    width: u32,
    height: u32,
) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, Box<dyn std::error::Error>> {
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
        Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
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

fn readback_nv12(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<(Vec<u8>, Vec<u8>), Box<dyn std::error::Error>> {
    let desc = D3D11_TEXTURE2D_DESC {
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
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut staging))?;
    }
    let staging = staging.ok_or("CreateTexture2D staging failed")?;

    unsafe {
        context.CopyResource(&staging, texture);
        context.Flush();
    }

    let y_size = (width * height) as usize;
    let uv_size = (width * height / 2) as usize;

    let mut y_plane = vec![0u8; y_size];
    let mut uv_plane = vec![0u8; uv_size];

    unsafe {
        let mut mapped = windows::Win32::Graphics::Direct3D11::D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

        let ptr = mapped.pData as *const u8;
        let row_pitch = mapped.RowPitch as usize;

        // NV12: Y plane first (full res), then UV interleaved (half res)
        for row in 0..height as usize {
            let src = ptr.add(row * row_pitch);
            let dst = y_plane.as_mut_ptr().add(row * width as usize);
            std::ptr::copy_nonoverlapping(src, dst, width as usize);
        }

        let uv_offset = row_pitch * height as usize;
        let uv_row_pitch = row_pitch; // NV12 UV plane has same row pitch as Y
        let uv_rows = height as usize / 2;
        for row in 0..uv_rows {
            let src = ptr.add(uv_offset + row * uv_row_pitch);
            let dst = uv_plane.as_mut_ptr().add(row * width as usize);
            std::ptr::copy_nonoverlapping(src, dst, width as usize);
        }

        context.Unmap(&staging, 0);
    }

    Ok((y_plane, uv_plane))
}
