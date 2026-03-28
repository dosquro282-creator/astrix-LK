//! Validate the direct D3D11 -> NV12 -> NVENC path on NVIDIA hardware.
//!
//! Run: cargo run --example nvenc_d11_smoke --features wgc-capture

#![cfg(all(windows, feature = "wgc-capture"))]

use parking_lot::Mutex;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use astrix_client::d3d11_nv12::D3d11BgraToNv12;
use astrix_client::encoded_h264::{EncodedBackendKind, EncodedH264Encoder};
use astrix_client::gpu_device::GpuDevice;
use astrix_client::nvenc_d11::NvencD3d11Encoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    std::env::set_var("ASTRIX_DXGI_NV12_RING", "4");

    println!("=== NVENC D3D11 Smoke Test ===\n");

    let gpu = GpuDevice::select_best()?;
    println!("Using GPU: {}", gpu.adapter_name.trim_end_matches('\0'));

    let probe = NvencD3d11Encoder::probe_device(&gpu.device)?;
    println!(
        "Probe: vendor={} vendor_id=0x{:04x} device_id=0x{:04x} runtime_present={}",
        probe.vendor.as_str(),
        probe.vendor_id,
        probe.device_id,
        probe.runtime_present
    );

    // Turing/Ampere NVENC requires H.264 width >= 145, so keep the smoke test
    // comfortably above that floor.
    let width = 256u32;
    let height = 144u32;
    let fps = 60u32;
    let bitrate_bps = 8_000_000u32;

    let converter =
        D3d11BgraToNv12::new(&gpu.device, &gpu.context, width, height, width, height, fps)?;
    println!(
        "Converter created: {}x{} @ {} fps, NV12 ring={}",
        width,
        height,
        fps,
        converter.output_textures().len()
    );

    let input_tex = create_bgra_texture(&gpu.device, width, height)?;
    let ctx_mutex = Mutex::new(());

    {
        let mut encoder = NvencD3d11Encoder::new(
            &gpu.device,
            width,
            height,
            fps,
            bitrate_bps,
            converter.output_textures(),
        )?;
        println!(
            "Direct NVENC init OK: {} | async={} | hardware={}",
            encoder.encoder_name().trim_end_matches('\0'),
            encoder.is_async(),
            encoder.is_hardware()
        );

        let mut produced_frames = 0usize;
        let mut total_bytes = 0usize;
        let mut saw_annex_b = false;
        let mut saw_key_frame = false;

        println!("\nEncoding test frames through direct NVENC...");
        for i in 0..12u64 {
            let timestamp_us = (i * 1_000_000 / fps as u64) as i64;
            let request_key_frame = i == 0 || i == 6;
            let nv12_tex = converter.convert(&gpu.context, &input_tex, &ctx_mutex)?;

            encoder.submit(
                &nv12_tex,
                timestamp_us,
                request_key_frame,
                i as u32,
                timestamp_us,
                40,
            )?;

            match encoder.collect_blocking(120)? {
                Some((frames, _, _, encode_us)) => {
                    for frame in frames {
                        let size = frame.data.len();
                        produced_frames += 1;
                        total_bytes += size;
                        saw_annex_b |= has_annex_b_start(&frame.data);
                        saw_key_frame |= frame.key_frame;
                        println!(
                            "  frame {:02} -> {:6} bytes | key={} | encode={} us",
                            i, size, frame.key_frame, encode_us
                        );
                    }
                }
                None => {
                    println!("  frame {:02} -> no output yet", i);
                }
            }
        }

        println!("\nDraining any remaining NVENC output...");
        for drain_idx in 0..4u32 {
            match encoder.collect_blocking(20)? {
                Some((frames, _, _, encode_us)) => {
                    for frame in frames {
                        let size = frame.data.len();
                        produced_frames += 1;
                        total_bytes += size;
                        saw_annex_b |= has_annex_b_start(&frame.data);
                        saw_key_frame |= frame.key_frame;
                        println!(
                            "  drain {:02} -> {:6} bytes | key={} | encode={} us",
                            drain_idx, size, frame.key_frame, encode_us
                        );
                    }
                }
                None => break,
            }
        }

        println!(
            "\nDirect NVENC summary: produced={} total_bytes={} annex_b={} keyframe={}",
            produced_frames, total_bytes, saw_annex_b, saw_key_frame
        );

        if produced_frames == 0 {
            return Err("Direct NVENC init succeeded but produced no frames".into());
        }
        if !saw_annex_b {
            return Err("Direct NVENC output does not contain Annex B start codes".into());
        }
        if !saw_key_frame {
            return Err("Direct NVENC output did not report any key frame".into());
        }
    }

    let auto = EncodedH264Encoder::new_auto(
        &gpu.device,
        width,
        height,
        fps,
        bitrate_bps,
        converter.output_textures(),
    )?;
    println!(
        "\nAuto backend selection: {:?} ({})",
        auto.backend_kind(),
        auto.encoder_name().trim_end_matches('\0')
    );
    if auto.backend_kind() != EncodedBackendKind::NvencD3d11 {
        return Err(format!(
            "auto backend did not select NVENC, got {:?}",
            auto.backend_kind()
        )
        .into());
    }

    println!("\n=== NVENC D3D11 smoke test PASSED ===");
    Ok(())
}

fn create_bgra_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, Box<dyn std::error::Error>> {
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

fn has_annex_b_start(data: &[u8]) -> bool {
    data.windows(4).any(|w| w == [0, 0, 0, 1]) || data.windows(3).any(|w| w == [0, 0, 1])
}
