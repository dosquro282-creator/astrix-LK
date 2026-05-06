mod border;
mod cli;
mod d3d;
mod dxgi_capture;
mod foreground;
mod overlay;
mod stats;
mod wgc_capture;

use border::BorderOverlay;
use cli::{Args, Backend, CpuPriority};
use dxgi_capture::DxgiCapture;
use overlay::CompatibilityOverlay;
use stats::BenchSummary;
use std::time::Instant;
use wgc_capture::WgcCapture;

/// Apply CPU process/thread priority
fn apply_cpu_priority(priority: CpuPriority) {
    match priority {
        CpuPriority::Off => {
            println!("[priority][cpu] mode=off skipped");
        }
        CpuPriority::High => {
            #[cfg(windows)]
            {
                unsafe {
                    use windows::Win32::System::Threading::{
                        GetCurrentProcess, GetCurrentThread, SetPriorityClass, SetThreadPriority,
                        HIGH_PRIORITY_CLASS, THREAD_PRIORITY_HIGHEST,
                    };

                    // Set process priority
                    let process = GetCurrentProcess();
                    if SetPriorityClass(process, HIGH_PRIORITY_CLASS).is_ok() {
                        println!("[priority][cpu] mode=high process=ok");
                    } else {
                        println!("[priority][cpu] mode=high process=failed");
                    }

                    // Set thread priority
                    let thread = GetCurrentThread();
                    if SetThreadPriority(thread, THREAD_PRIORITY_HIGHEST).is_ok() {
                        println!("[priority][cpu] mode=high capture_thread=ok");
                    } else {
                        println!("[priority][cpu] mode=high capture_thread=failed");
                    }

                    println!("[priority][cpu] mode=high media_thread=ok (same thread)");
                }
            }
            #[cfg(not(windows))]
            {
                println!("[priority][cpu] mode=high skipped (not Windows)");
            }
        }
        CpuPriority::Realtime => {
            println!("[priority][cpu] mode=realtime WARNING experimental");
            println!("[priority][cpu] mode=realtime not fully implemented - use high instead");

            #[cfg(windows)]
            {
                // Fallback to high for safety
                unsafe {
                    use windows::Win32::System::Threading::{
                        GetCurrentProcess, GetCurrentThread, SetPriorityClass, SetThreadPriority,
                        HIGH_PRIORITY_CLASS, THREAD_PRIORITY_HIGHEST,
                    };

                    let process = GetCurrentProcess();
                    let _ = SetPriorityClass(process, HIGH_PRIORITY_CLASS);

                    let thread = GetCurrentThread();
                    let _ = SetThreadPriority(thread, THREAD_PRIORITY_HIGHEST);
                }
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut config = args.to_config();
    if let Some(duration_sec) = config.duration_sec {
        if !duration_sec.is_finite() || duration_sec <= 0.0 {
            anyhow::bail!("--duration-sec must be a positive finite number");
        }
    }

    println!("============================================");
    println!("  Capture Benchmark - DXGI vs WGC");
    println!("============================================");
    println!("backend: {:?}", config.backend);
    println!("device_mode: {:?}", config.device_mode);
    println!("frames: {}", config.frames);
    if let Some(duration_sec) = config.duration_sec {
        println!("duration_sec: {:.1}", duration_sec);
    }
    println!("monitor: {}", config.monitor_index);
    println!("timeout_ms: {}", config.timeout_ms);
    println!("copy_mode: {:?}", config.copy_mode);
    println!("warmup: {}", config.warmup);
    println!("flush_each_frame: {}", config.flush_each_frame);
    println!();

    // Apply CPU priority early
    apply_cpu_priority(config.cpu_priority);

    println!("============================================");
    println!();

    // New parameters summary
    println!("[NEW] device_mode: {:?}", config.device_mode);
    println!("[NEW] ring_size: {}", config.ring_size);
    println!("[NEW] convert_test: {:?}", config.convert_test);
    println!(
        "[NEW] ready_wait_budget_us: {}",
        config.ready_wait_budget_us
    );
    println!(
        "[NEW] gpu_priority_capture: {:?}",
        config.gpu_priority_capture
    );
    println!("[NEW] gpu_priority_media: {:?}", config.gpu_priority_media);
    println!("[NEW] cpu_priority: {:?}", config.cpu_priority);
    println!("[NEW] summary_every: {}", config.summary_every);
    println!("[NEW] overlay_mode: {}", config.overlay_mode.as_str());
    println!();

    if let Some(ref csv) = config.csv_path {
        println!("CSV output: {}", csv);
    }

    // Get monitor info and show border
    let monitor_rect = d3d::get_monitor_rect(config.monitor_index)?;

    // Create yellow border overlay
    let border = BorderOverlay::new();
    border.show_border(
        monitor_rect.0,
        monitor_rect.1,
        monitor_rect.2,
        monitor_rect.3,
    );

    println!(
        "Monitor {} coordinates: ({}, {}) - ({}, {})",
        config.monitor_index, monitor_rect.0, monitor_rect.1, monitor_rect.2, monitor_rect.3
    );
    println!("Yellow border displayed around selected monitor\n");

    let (overlay, overlay_status) = CompatibilityOverlay::create(config.overlay_mode, monitor_rect);
    config.overlay_created = overlay_status.created;
    config.overlay_foreground_unchanged = overlay_status.foreground_unchanged;

    println!("Listing available monitors:");
    match d3d::get_all_monitors() {
        Ok(monitors) => {
            for (idx, m) in monitors.iter().enumerate() {
                if idx == config.monitor_index {
                    println!("  {} <-- SELECTED", m);
                } else {
                    println!("  {}", m);
                }
            }
        }
        Err(e) => println!("  Failed to list monitors: {}", e),
    }
    println!();

    match config.backend {
        Backend::Dxgi => {
            run_dxgi_benchmark(&config)?;
        }
        Backend::Wgc => {
            run_wgc_benchmark(&config)?;
        }
        Backend::Both => {
            let dxgi_summary = run_dxgi_benchmark(&config)?;
            println!("\n\n");
            let wgc_summary = run_wgc_benchmark(&config)?;

            print_comparison(&dxgi_summary, &wgc_summary);
        }
    }

    // Hide border before exiting
    drop(overlay);
    border.hide_border(
        monitor_rect.0,
        monitor_rect.1,
        monitor_rect.2,
        monitor_rect.3,
    );

    Ok(())
}

fn run_dxgi_benchmark(config: &cli::BenchConfig) -> anyhow::Result<BenchSummary> {
    println!("\n========================================");
    println!("  DXGI DESKTOP DUPLICATION BENCHMARK");
    println!("========================================\n");

    let start = Instant::now();
    let mut capture = DxgiCapture::new(config)?;
    let summary = capture.run(config)?;
    let total_elapsed = start.elapsed();

    summary.print();

    println!(
        "[BENCH] DXGI Total benchmark time: {:.1}s",
        total_elapsed.as_secs_f64()
    );

    Ok(summary)
}

fn run_wgc_benchmark(config: &cli::BenchConfig) -> anyhow::Result<BenchSummary> {
    println!("\n========================================");
    println!("  WINDOWS GRAPHICS CAPTURE BENCHMARK");
    println!("========================================\n");

    let start = Instant::now();
    let mut capture = WgcCapture::new(config)?;
    let summary = capture.run(config)?;
    let total_elapsed = start.elapsed();

    summary.print();

    println!(
        "[BENCH] WGC Total benchmark time: {:.1}s",
        total_elapsed.as_secs_f64()
    );

    Ok(summary)
}

fn print_comparison(dxgi: &BenchSummary, wgc: &BenchSummary) {
    println!("\n========================================");
    println!("           COMPARISON");
    println!("========================================\n");

    println!("DXGI captured_fps: {:.1}", dxgi.captured_fps);
    println!("WGC captured_fps: {:.1}", wgc.captured_fps);

    if let Some(ref p) = dxgi.acquire_wait_us {
        println!("DXGI p95 acquire_wait_us: {}", p.p95);
    }
    if let Some(ref p) = dxgi.acquire_to_release_us {
        println!("DXGI p95 acquire_to_release_us: {}", p.p95);
    }
    if let Some(ref p) = dxgi.copy_ready_delay_us {
        println!("DXGI p95 copy_ready_delay_us: {}", p.p95);
    }
    if let Some(ref p) = dxgi.convert_ready_delay_us {
        println!("DXGI p95 convert_ready_delay_us: {}", p.p95);
    }

    if let Some(ref p) = dxgi.held_frame_us {
        println!("DXGI p95 held_frame_us: {}", p.p95);
    }
    if let Some(ref p) = wgc.held_frame_us {
        println!("WGC p95 held_frame_us: {}", p.p95);
    }

    if let Some(ref dxgi_gap) = dxgi.callback_gap_us {
        if let Some(ref wgc_gap) = wgc.callback_gap_us {
            println!("DXGI p99 frame gap: {}", dxgi_gap.p99);
            println!("WGC p99 frame gap: {}", wgc_gap.p99);
        }
    }

    println!("\nRecommendation:");

    let dxgi_fps = dxgi.captured_fps;
    let wgc_fps = wgc.captured_fps;

    if dxgi_fps > wgc_fps {
        let diff = dxgi_fps - wgc_fps;
        let pct = (diff / wgc_fps.max(1.0)) * 100.0;
        println!("  DXGI is {:.1} fps ({:.0}% faster)", dxgi_fps, pct);
        println!("  More stable backend for high-FPS scenarios.");
    } else if wgc_fps > dxgi_fps {
        let diff = wgc_fps - dxgi_fps;
        let pct = (diff / dxgi_fps.max(1.0)) * 100.0;
        println!("  WGC is {:.1} fps ({:.0}% faster)", wgc_fps, pct);
        println!("  More stable backend for high-FPS scenarios.");
    } else {
        println!("  Both backends perform similarly.");
    }

    let dxgi_p95_held = dxgi
        .acquire_wait_us
        .as_ref()
        .map(|p| p.p95)
        .unwrap_or(u64::MAX);
    let wgc_p95_held = dxgi
        .held_frame_us
        .as_ref()
        .map(|p| p.p95)
        .unwrap_or(u64::MAX);

    if dxgi_p95_held < wgc_p95_held {
        println!(
            "  DXGI has lower frame wait latency (p95: {}us vs {}us)",
            dxgi_p95_held, wgc_p95_held
        );
    } else {
        println!(
            "  WGC has lower frame wait latency (p95: {}us vs {}us)",
            wgc_p95_held, dxgi_p95_held
        );
    }

    println!();
    println!("DIAGNOSTIC INTERPRETATION:");
    println!("  If DXGI benchmark shows low FPS (3-25) under heavy game load:");
    println!("    -> Problem is in Desktop Duplication / GPU contention / DWM");
    println!("  If DXGI benchmark is stable but Astrix capture is slow:");
    println!("    -> Problem is in Astrix pipeline: mutex, queue, ReleaseFrame timing");
    println!();
    println!("SEPARATED DEVICE ANALYSIS:");
    println!("  Device mode: {}", dxgi.device_mode);
    if dxgi.device_mode == "Separated" {
        println!("  Key metrics to compare:");
        println!("    - acquire_wait_us p95/max (should be similar between modes)");
        println!("    - acquire_to_release_us p95/max (should be shorter in separated)");
        println!("    - copy_ready_delay_us p95/max (indicates GPU contention)");
        println!("    - longest_gap_between_produced_ms (should be lower in separated)");
        println!("    - dropped_gpu_not_ready_count (should be lower in separated)");
    }
}
