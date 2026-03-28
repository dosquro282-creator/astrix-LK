//! Phase 6: Quick validation of MFT GPU path components.
//!
//! Runs GPU selection, MFT enum, and path selection logic.
//! Use before full validation (client/docs/phase6_validation.md).
//!
//! Run: cargo run --example validate_mft_path
//!
//! Env:
//!   ASTRIX_GPU_ADAPTER=N     — force adapter index
//!   ASTRIX_SCREEN_CAPTURE_PATH — show what path would be selected (mft/cpu/auto)
//!   ASTRIX_MFT_SOFTWARE=1    — force software MFT (for mft_encoder_test)

#![cfg(all(windows, feature = "wgc-capture"))]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Phase 6: MFT Path Validation ===\n");

    // 1. Env vars
    let path_env = std::env::var("ASTRIX_SCREEN_CAPTURE_PATH").unwrap_or_else(|_| "auto".into());
    let path_desc = match path_env.as_str() {
        "mft" => "MFT (hardware or software)",
        "cpu" => "OpenH264 (CPU)",
        _ => "auto (MFT first, fallback to CPU)",
    };
    println!("ASTRIX_SCREEN_CAPTURE_PATH={} → {}", path_env, path_desc);

    let gpu_adapter = std::env::var("ASTRIX_GPU_ADAPTER").ok();
    if let Some(a) = &gpu_adapter {
        println!("ASTRIX_GPU_ADAPTER={} (manual)", a);
    }
    println!();

    // 2. GPU adapters
    println!("--- GPU Adapters ---");
    let adapters = astrix_client::gpu_device::GpuDevice::enumerate();
    if adapters.is_empty() {
        println!("WARNING: No adapters found");
    } else {
        for a in &adapters {
            let kind = if a.software {
                "SOFTWARE"
            } else if a.is_discrete {
                "discrete"
            } else {
                "integrated"
            };
            println!(
                "  [{}] {} | {} MB VRAM | {}",
                a.adapter_idx,
                a.name.trim_end_matches('\0'),
                a.dedicated_video_memory / (1024 * 1024),
                kind
            );
        }

        let gpu = astrix_client::gpu_device::GpuDevice::select_best()?;
        println!(
            "\nSelected: {} (idx={}, discrete={})",
            gpu.adapter_name.trim_end_matches('\0'),
            gpu.adapter_idx,
            gpu.is_discrete
        );
    }
    println!();

    // 3. MFT encoders
    println!("--- MFT H.264 Encoders (NV12→H.264) ---");
    let (hw_count, sw_count) = enumerate_mft()?;
    println!("  Hardware: {}", hw_count);
    println!("  Software: {}", sw_count);
    if hw_count > 0 {
        println!("  → MFT hardware path available");
    }
    if sw_count > 0 {
        println!("  → MFT software fallback available");
    }
    if hw_count == 0 && sw_count == 0 {
        println!("  WARNING: No MFT encoders — will fallback to OpenH264");
    }
    println!();

    // 4. Quick init check (optional — may fail on some configs)
    println!("--- Quick Init Check ---");
    match quick_init_check() {
        Ok(msg) => println!("  OK: {}", msg),
        Err(e) => println!("  WARNING: {}", e),
    }

    println!("\n=== Phase 6 quick validation done ===");
    println!("  Full checklist: client/docs/phase6_validation.md");
    Ok(())
}

fn enumerate_mft() -> Result<(u32, u32), Box<dyn std::error::Error>> {
    use std::ptr;
    use windows::Win32::Media::MediaFoundation::{
        IMFActivate, MFMediaType_Video, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
        MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SYNCMFT,
        MFT_REGISTER_TYPE_INFO,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let input_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video.into(),
        guidSubtype: MFVideoFormat_NV12.into(),
    };
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video.into(),
        guidSubtype: MFVideoFormat_H264.into(),
    };

    let mut hw_count = 0u32;
    let mut sw_count = 0u32;

    unsafe {
        let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count: u32 = 0;
        let _ = MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE,
            Some(&input_type as *const _),
            Some(&output_type as *const _),
            &mut activates,
            &mut count,
        );
        if count > 0 {
            hw_count = count;
            let slice = std::slice::from_raw_parts(activates, count as usize);
            for act in slice {
                if let Some(a) = act {
                    let _ = a.ShutdownObject();
                }
            }
            CoTaskMemFree(Some(activates as *const _ as *mut _));
        }
    }

    unsafe {
        let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count: u32 = 0;
        let _ = MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT,
            Some(&input_type as *const _),
            Some(&output_type as *const _),
            &mut activates,
            &mut count,
        );
        if count > 0 {
            sw_count = count;
            let slice = std::slice::from_raw_parts(activates, count as usize);
            for act in slice {
                if let Some(a) = act {
                    let _ = a.ShutdownObject();
                }
            }
            CoTaskMemFree(Some(activates as *const _ as *mut _));
        }
    }

    Ok((hw_count, sw_count))
}

fn quick_init_check() -> Result<String, Box<dyn std::error::Error>> {
    let gpu = astrix_client::gpu_device::GpuDevice::select_best()?;
    let width = 1280u32;
    let height = 720u32;
    let fps = 30u32;
    let bitrate = 2_000_000u32;

    // Step 1: D3d11BgraToNv12
    print!("  D3d11BgraToNv12::new ... ");
    match astrix_client::d3d11_nv12::D3d11BgraToNv12::new(
        &gpu.device,
        &gpu.context,
        width,
        height,
        width,
        height,
        fps,
    ) {
        Ok(_) => println!("OK"),
        Err(e) => {
            println!("FAIL: {:?}", e);
            return Err(format!("D3d11BgraToNv12::new failed: {:?}", e).into());
        }
    }

    // Step 2: MftH264Encoder (hardware)
    print!("  MftH264Encoder::new (hardware) ... ");
    match astrix_client::mft_encoder::MftH264Encoder::new(&gpu.device, width, height, fps, bitrate)
    {
        Ok(enc) => {
            println!(
                "OK ({} {})",
                enc.encoder_name(),
                if enc.is_hardware() {
                    "hardware"
                } else {
                    "software"
                }
            );
            return Ok(format!(
                "D3d11BgraToNv12 + MftH264Encoder ({} {})",
                enc.encoder_name(),
                if enc.is_hardware() {
                    "hardware"
                } else {
                    "software"
                }
            ));
        }
        Err(e) => {
            println!("FAIL: {:?}", e);
        }
    }

    // Step 3: MftH264Encoder (force software)
    print!("  MftH264Encoder::new (software, ASTRIX_MFT_SOFTWARE=1) ... ");
    std::env::set_var("ASTRIX_MFT_SOFTWARE", "1");
    match astrix_client::mft_encoder::MftH264Encoder::new(&gpu.device, width, height, fps, bitrate)
    {
        Ok(enc) => {
            std::env::remove_var("ASTRIX_MFT_SOFTWARE");
            println!(
                "OK ({} {})",
                enc.encoder_name(),
                if enc.is_hardware() {
                    "hardware"
                } else {
                    "software"
                }
            );
            return Ok(format!(
                "D3d11BgraToNv12 + MftH264Encoder ({} {})",
                enc.encoder_name(),
                if enc.is_hardware() {
                    "hardware"
                } else {
                    "software"
                }
            ));
        }
        Err(e) => {
            std::env::remove_var("ASTRIX_MFT_SOFTWARE");
            println!("FAIL: {:?}", e);
            return Err(format!("MftH264Encoder::new (software) failed: {:?}", e).into());
        }
    }
}
