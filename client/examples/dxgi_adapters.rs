//! Phase 0.4: Enumerate DXGI adapters with DedicatedVideoMemory.
//! Verifies logic for selecting discrete GPU (largest VRAM).
//!
//! Run: cargo run --example dxgi_adapters

#![cfg(windows)]

use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ADAPTER_DESC1, IDXGIFactory1,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    println!("=== DXGI Adapter Enumeration (Phase 0.4) ===\n");

    let adapters = unsafe { enumerate_adapters()? };

    for (i, info) in adapters.iter().enumerate() {
        let flags_str = if info.software {
            "SOFTWARE (WARP)"
        } else if info.dedicated_video_memory > 512 * 1024 * 1024 {
            "discrete"
        } else {
            "integrated"
        };
        println!(
            "  [{}] {} | {} MB VRAM | {} MB shared | {}",
            i,
            info.name,
            info.dedicated_video_memory / (1024 * 1024),
            info.shared_system_memory / (1024 * 1024),
            flags_str
        );
    }

    // Simulate select_best logic: skip SOFTWARE, pick max DedicatedVideoMemory
    let best_idx = adapters
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.software)
        .max_by_key(|(_, a)| a.dedicated_video_memory)
        .map(|(i, _)| i);

    if let Some(idx) = best_idx {
        let b = &adapters[idx];
        println!(
            "\nSelected (by logic): [{}] {} ({} MB VRAM)",
            idx,
            b.name,
            b.dedicated_video_memory / (1024 * 1024)
        );
    } else {
        println!("\nNo suitable adapter found (all SOFTWARE or empty).");
    }

    Ok(())
}

struct AdapterInfo {
    name: String,
    dedicated_video_memory: u64,
    shared_system_memory: u64,
    software: bool,
}

unsafe fn enumerate_adapters() -> Result<Vec<AdapterInfo>, windows::core::Error> {
    let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
    let mut adapters = Vec::new();
    let mut i = 0u32;

    loop {
        let adapter = match factory.EnumAdapters1(i) {
            Ok(a) => a,
            Err(_) => break,
        };

        let desc: DXGI_ADAPTER_DESC1 = adapter.GetDesc1()?;
        let name = String::from_utf16_lossy(
            &desc.Description[..desc.Description.iter().position(|&c| c == 0).unwrap_or(0)],
        )
        .trim_end_matches('\0')
        .to_string();
        // DXGI_ADAPTER_FLAG_SOFTWARE = WARP (software rasterizer) — skip for MFT
        let software = (desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32)) != 0;

        adapters.push(AdapterInfo {
            name: if name.is_empty() {
                format!("Adapter {}", i)
            } else {
                name
            },
            dedicated_video_memory: desc.DedicatedVideoMemory as u64,
            shared_system_memory: desc.SharedSystemMemory as u64,
            software,
        });

        i += 1;
    }

    Ok(adapters)
}
