//! Phase 2: Test GpuDevice::select_best() and enumerate().
//!
//! Run: cargo run --example gpu_device_select
//!
//! Env: ASTRIX_GPU_ADAPTER=N to force adapter index N.

#![cfg(windows)]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== GpuDevice Phase 2 ===\n");

    let adapters = astrix_client::gpu_device::GpuDevice::enumerate();
    println!("Enumerated {} adapters:", adapters.len());
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
            a.name,
            a.dedicated_video_memory / (1024 * 1024),
            kind
        );
    }

    println!("\nSelecting best GPU...");
    let gpu = astrix_client::gpu_device::GpuDevice::select_best()?;
    println!(
        "Selected: {} (idx={}, discrete={})",
        gpu.adapter_name, gpu.adapter_idx, gpu.is_discrete
    );

    Ok(())
}
