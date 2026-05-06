use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC, D3D11_USAGE,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};

use crate::cli::CopyMode;
use crate::cli::{BenchConfig, DeviceMode};
use crate::d3d::D3D11Setup;
use crate::foreground::{ForegroundBucket, ForegroundTracker, ScreenRect};
use crate::stats::{
    foreground_bucket_fps, BenchSummary, CallbackGapHistogram, FrameMetric, Percentiles,
    StatsCollector,
};

// WinRT / WGC imports
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;

fn get_monitor_handle(monitor_index: usize) -> anyhow::Result<(HMONITOR, String)> {
    let monitors = Arc::new(Mutex::new(Vec::<(HMONITOR, String)>::new()));
    let monitors_clone = monitors.clone();

    unsafe extern "system" fn enum_callback(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _clip: *mut RECT,
        dw_data: LPARAM,
    ) -> BOOL {
        let monitors = &*(dw_data.0 as *const Mutex<Vec<(HMONITOR, String)>>);

        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

        let result = GetMonitorInfoW(hmonitor, &mut info as *mut _ as *mut MONITORINFO);
        if result.as_bool() {
            let name = String::from_utf16_lossy(&info.szDevice[..])
                .trim_end_matches('\0')
                .to_string();
            monitors.lock().unwrap().push((hmonitor, name));
        }
        BOOL(1)
    }

    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_callback),
            LPARAM(monitors_clone.as_ref() as *const _ as isize),
        );
    };

    let monitors = monitors.lock().unwrap();

    if monitor_index >= monitors.len() {
        return Err(anyhow::anyhow!(
            "Monitor index {} not found. Available monitors: {}",
            monitor_index,
            monitors.len()
        ));
    }

    let (hmonitor, name) = monitors[monitor_index].clone();
    Ok((hmonitor, name))
}

pub struct WgcCapture {
    setup: D3D11Setup,
    width: u32,
    height: u32,
    monitor_name: String,
    ring_textures: Vec<ID3D11Texture2D>,
    ring_index: usize,
    copy_mode: CopyMode,
    // WGC specific
    frame_pool: Option<Direct3D11CaptureFramePool>,
    session: Option<GraphicsCaptureSession>,
    item: Option<GraphicsCaptureItem>,
    winrt_device: Option<IDirect3DDevice>,
    running: Arc<AtomicBool>,
}

impl WgcCapture {
    pub fn new(config: &BenchConfig) -> anyhow::Result<Self> {
        // Get monitor handle by index
        let (hmonitor, device_name) = get_monitor_handle(config.monitor_index)?;

        println!(
            "[WGC] Creating capture for monitor {}: {}",
            config.monitor_index, device_name
        );
        if config.device_mode == DeviceMode::Separated {
            println!(
                "[WGC] WARNING: separated media path is not implemented for WGC benchmark yet."
            );
        }

        // Create D3D11 device
        let setup = D3D11Setup::with_monitor(config.monitor_index)?;

        let monitor_rect = crate::d3d::get_monitor_rect(config.monitor_index)?;
        let width = (monitor_rect.2 - monitor_rect.0) as u32;
        let height = (monitor_rect.3 - monitor_rect.1) as u32;

        println!("[WGC] Monitor size: {}x{}", width, height);
        println!("[WGC] GPU: {}", setup.gpu_name);

        // Create WinRT Direct3D device from D3D11 device
        let winrt_device = create_winrt_device(&setup.device)?;

        println!("[WGC] Created WinRT Direct3D device");

        // Create GraphicsCaptureItem using IGraphicsCaptureItemInterop
        let item = create_capture_item_for_monitor(hmonitor)?;

        let item_size = item.Size()?;
        println!(
            "[WGC] Capture item size: {}x{}",
            item_size.Width, item_size.Height
        );

        // Get actual capture size from item
        let capture_width = item_size.Width as u32;
        let capture_height = item_size.Height as u32;

        // Create ring textures if copy mode is enabled
        let ring_textures = if config.copy_mode == CopyMode::Copy {
            create_ring_textures(&setup.device, capture_width, capture_height)?
        } else {
            Vec::new()
        };

        println!("[WGC] Created {} ring textures", ring_textures.len());

        Ok(Self {
            setup,
            width: capture_width,
            height: capture_height,
            monitor_name: device_name,
            ring_textures,
            ring_index: 0,
            copy_mode: config.copy_mode,
            frame_pool: None,
            session: None,
            item: Some(item),
            winrt_device: Some(winrt_device),
            running: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn gpu_name(&self) -> &str {
        &self.setup.gpu_name
    }

    pub fn monitor_info(&self) -> String {
        format!("{} {}x{}", self.monitor_name, self.width, self.height)
    }

    fn get_next_ring_texture(&mut self) -> Option<ID3D11Texture2D> {
        if self.ring_textures.is_empty() {
            return None;
        }
        let tex = self.ring_textures[self.ring_index].clone();
        self.ring_index = (self.ring_index + 1) % self.ring_textures.len();
        Some(tex)
    }

    pub fn run(&mut self, config: &BenchConfig) -> anyhow::Result<BenchSummary> {
        // Re-acquire frame pool and session (they might be None after new())
        let (frame_pool, _session) = self.start_capture()?;

        println!("[WGC] Capture session started");
        println!("[WGC] Starting warmup ({} frames)...", config.warmup);

        let mut collector = StatsCollector::new();
        let benchmark_start_time = Instant::now();
        let captured_monitor_rect =
            ScreenRect::from_tuple(crate::d3d::get_monitor_rect(config.monitor_index)?);
        let mut foreground_tracker = ForegroundTracker::new(captured_monitor_rect);
        foreground_tracker.log_startup();

        let mut last_frame_time = Instant::now();
        let mut successful_frames = 0usize;
        let mut frame_index = 0usize;
        let mut first_frame_delay_ms: Option<f64> = None;
        let mut first_steady_frame_time_ms: Option<f64> = None;
        let mut startup_long_gap_count = 0usize;
        let mut startup_gap_ms = 0.0;
        let mut steady_gap_values_ms: Vec<f64> = Vec::new();
        let mut seen_steady_frame = false;

        // WGC frame receiving using polling (simpler than event-based for now)
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::SeqCst);

        let warmup_start = Instant::now();
        let mut warmup_complete = config.warmup == 0;
        let mut benchmark_start: Option<Instant> = if warmup_complete {
            Some(Instant::now())
        } else {
            None
        };
        if warmup_complete {
            if let Some(duration_sec) = config.duration_sec {
                println!(
                    "[WGC] No warmup requested. Starting benchmark ({:.1}s duration)...",
                    duration_sec
                );
            } else {
                println!(
                    "[WGC] No warmup requested. Starting benchmark ({} frames)...",
                    config.frames
                );
            }
        }

        loop {
            if warmup_complete {
                if let Some(started_at) = benchmark_start {
                    if config
                        .duration_sec
                        .map(|duration_sec| {
                            started_at.elapsed() >= Duration::from_secs_f64(duration_sec)
                        })
                        .unwrap_or(successful_frames >= config.frames)
                    {
                        break;
                    }
                }
            }

            // Wait for a frame
            let frame_result = self.wait_for_frame(&frame_pool, config.timeout_ms);
            let mut foreground_logged = false;

            match frame_result {
                Ok(Some((surface, frame))) => {
                    let now = Instant::now();
                    let callback_gap_us = now.duration_since(last_frame_time).as_micros() as u64;
                    last_frame_time = now;
                    let callback_gap_ms = callback_gap_us as f64 / 1000.0;
                    let bucket = foreground_tracker.current_bucket();
                    let bucket_name = bucket.as_str().to_string();

                    let held_start = Instant::now();
                    let mut copy_us = 0u64;

                    // Process the frame - get texture and copy if needed
                    if let Ok(texture) = get_d3d11_texture_from_surface(&surface) {
                        if self.copy_mode == CopyMode::Copy {
                            if let Some(dst_tex) = self.get_next_ring_texture() {
                                let copy_start = Instant::now();
                                unsafe {
                                    self.setup.context.CopyResource(&dst_tex, &texture);
                                }
                                copy_us = copy_start.elapsed().as_micros() as u64;
                            }
                        }
                    }

                    let held_frame_us = held_start.elapsed().as_micros() as u64;
                    let timestamp_us = benchmark_start_time.elapsed().as_micros() as i64;

                    // Drop frame immediately
                    drop(frame);
                    drop(surface);

                    // Check if we're past warmup
                    let is_warmup = frame_index < config.warmup;
                    if first_frame_delay_ms.is_none() {
                        first_frame_delay_ms =
                            Some(now.duration_since(benchmark_start_time).as_secs_f64() * 1000.0);
                    }
                    if is_warmup {
                        if callback_gap_ms > startup_gap_ms {
                            startup_gap_ms = callback_gap_ms;
                        }
                        if callback_gap_us > 100_000 {
                            startup_long_gap_count += 1;
                        }
                    } else {
                        if first_steady_frame_time_ms.is_none() {
                            first_steady_frame_time_ms = Some(
                                now.duration_since(benchmark_start_time).as_secs_f64() * 1000.0,
                            );
                        }
                        if seen_steady_frame {
                            steady_gap_values_ms.push(callback_gap_ms);
                            foreground_tracker.record_gap(bucket, callback_gap_ms);
                            if callback_gap_us > 100_000 {
                                foreground_tracker.log_long_gap(callback_gap_ms, frame_index);
                                foreground_logged = true;
                            }
                        } else {
                            if callback_gap_ms > startup_gap_ms {
                                startup_gap_ms = callback_gap_ms;
                            }
                            if callback_gap_us > 100_000 {
                                startup_long_gap_count += 1;
                            }
                            seen_steady_frame = true;
                        }
                    }

                    let frame_metric = FrameMetric::wgc_success(
                        frame_index,
                        timestamp_us,
                        callback_gap_us,
                        copy_us,
                        held_frame_us,
                        is_warmup,
                    )
                    .with_foreground_bucket(&bucket_name);
                    collector.add_metric(frame_metric);

                    frame_index += 1;

                    // Only count towards successful after warmup
                    if !is_warmup {
                        successful_frames += 1;
                        foreground_tracker.record_successful_frame(bucket);
                    } else if frame_index >= config.warmup && !warmup_complete {
                        warmup_complete = true;
                        benchmark_start = Some(Instant::now());
                        if let Some(duration_sec) = config.duration_sec {
                            println!(
                                "[WGC] Warmup complete. Starting benchmark ({:.1}s duration)...",
                                duration_sec
                            );
                        } else {
                            println!(
                                "[WGC] Warmup complete. Starting benchmark ({} frames)...",
                                config.frames
                            );
                        }
                    }

                    if config.summary_every > 0 && frame_index % config.summary_every == 0 {
                        println!(
                            "[WGC] Progress: {}/{} frames, successful={}",
                            frame_index,
                            config.frames + config.warmup,
                            successful_frames
                        );
                        foreground_tracker.log_periodic(frame_index);
                        foreground_logged = true;
                    }
                }
                Ok(None) => {
                    // No frame available yet, continue polling
                    std::thread::sleep(std::time::Duration::from_micros(500));
                }
                Err(e) => {
                    println!("[WGC] Error getting frame: {}", e);
                    // Record error and continue
                    let timestamp_us = benchmark_start_time.elapsed().as_micros() as i64;
                    let bucket_name = foreground_tracker.current_bucket_name().to_string();
                    let frame_metric = FrameMetric::error_metric(
                        frame_index,
                        timestamp_us,
                        e.to_string(),
                        frame_index < config.warmup,
                    )
                    .with_foreground_bucket(&bucket_name);
                    collector.add_metric(frame_metric);
                    frame_index += 1;
                    if frame_index >= config.warmup && !warmup_complete {
                        warmup_complete = true;
                        benchmark_start = Some(Instant::now());
                    }
                }
            }

            if !foreground_logged {
                let _ = foreground_tracker.sample();
            }
        }

        // Stop capture
        self.stop_capture();

        let elapsed = benchmark_start_time.elapsed();
        let elapsed_ms = elapsed.as_millis() as u64;
        let warmup_elapsed_ms = if let Some(started_at) = benchmark_start {
            started_at.duration_since(warmup_start).as_millis() as u64
        } else {
            warmup_start.elapsed().as_millis() as u64
        };
        let benchmark_elapsed_ms = benchmark_start
            .map(|started_at| started_at.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let captured_fps = if benchmark_elapsed_ms > 0 {
            successful_frames as f64 / (benchmark_elapsed_ms as f64 / 1000.0)
        } else {
            0.0
        };
        let steady_state_fps = captured_fps;
        let foreground_summary = foreground_tracker.summary();
        let p95_gap_ms = percentile_f64(&steady_gap_values_ms, 0.95);
        let p99_gap_ms = percentile_f64(&steady_gap_values_ms, 0.99);
        let long_gap_count = steady_gap_values_ms
            .iter()
            .filter(|gap| **gap > 100.0)
            .count();
        let foreground_game_fps = foreground_bucket_fps(
            &foreground_summary.buckets,
            ForegroundBucket::GameForegroundOnCapturedMonitor,
        );
        let foreground_other_fps = foreground_bucket_fps(
            &foreground_summary.buckets,
            ForegroundBucket::OtherForegroundOnOtherMonitor,
        );

        println!(
            "[WGC] Benchmark complete. Elapsed: {}ms, FPS: {:.1}",
            elapsed_ms, captured_fps
        );

        // Compute statistics from main (non-warmup) metrics
        let main_metrics: Vec<&FrameMetric> = collector
            .get_all_metrics()
            .iter()
            .filter(|m| !m.warmup)
            .collect();

        let callback_gap_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.callback_gap_us)
            .collect();
        let copy_values: Vec<u64> = main_metrics.iter().filter_map(|m| m.copy_us).collect();
        let held_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.held_frame_us)
            .collect();

        let callback_gap_stats = Percentiles::compute(&callback_gap_values);
        let callback_gap_histogram = CallbackGapHistogram::compute(&callback_gap_values);
        let copy_stats = Percentiles::compute(&copy_values);
        let held_stats = Percentiles::compute(&held_values);

        if config.flush_each_frame {
            self.setup.flush();
        }

        let summary = BenchSummary {
            backend: "WGC".to_string(),
            monitor_info: self.monitor_info(),
            gpu_name: self.gpu_name().to_string(),
            frames_requested: config.frames,
            warmup_frames: config.warmup,
            successful_frames,
            timeouts: 0,
            access_lost: 0,
            duplicated_errors: 0,
            elapsed_ms,
            captured_fps,
            steady_state_fps,
            effective_source_fps: None,
            accumulated_frames_total: successful_frames as u64,
            accumulated_frames_max: 1,
            // Legacy
            acquire_wait_us: None,
            callback_gap_us: Some(callback_gap_stats.clone()),
            copy_us: if self.copy_mode == CopyMode::Copy {
                Some(copy_stats.clone())
            } else {
                None
            },
            held_frame_us: Some(held_stats.clone()),
            // Extended (WGC doesn't provide these, using defaults)
            get_resource_us: None,
            copy_submit_us: None,
            acquire_to_release_us: None,
            release_frame_us: None,
            total_capture_stage_us: None,
            copy_ready_delay_us: None,
            copy_ready_timeout_count: 0,
            dropped_gpu_not_ready_count: 0,
            shared_create_handle_us: None,
            shared_open_us: None,
            shared_sync_wait_us: None,
            shared_busy_drop_count: 0,
            media_actual_used_count: 0,
            media_open_failed_count: 0,
            separated_path_valid: false,
            same_adapter_luid: None,
            shared_path: if config.device_mode == DeviceMode::Separated {
                "not_implemented".to_string()
            } else {
                "disabled".to_string()
            },
            convert_submit_us: None,
            convert_ready_delay_us: None,
            convert_timeout_count: 0,
            convert_dropped_not_ready_count: 0,
            frames_attempted: collector.frames_attempted(),
            frames_acquired: collector.frames_acquired(),
            frames_dropped: collector.frames_dropped(),
            frame_age_us: None,
            longest_gap_between_acquired_ms: None,
            longest_gap_between_produced_ms: Some(
                steady_gap_values_ms.iter().copied().fold(0.0, f64::max),
            ),
            p95_gap_ms,
            p99_gap_ms,
            long_gap_count,
            warmup_elapsed_ms,
            benchmark_elapsed_ms,
            startup_long_gap_count,
            first_frame_delay_ms,
            first_steady_frame_time_ms,
            startup_gap_ms,
            foreground_buckets: foreground_summary.buckets,
            foreground_game_fps,
            foreground_other_fps,
            callback_gap_histogram: Some(callback_gap_histogram),
            device_mode: format!("{:?}", config.device_mode),
            cpu_priority_mode: format!("{:?}", config.cpu_priority),
            gpu_priority_capture: format!("{:?}", config.gpu_priority_capture),
            gpu_priority_media: format!("{:?}", config.gpu_priority_media),
            ready_wait_budget_us: config.ready_wait_budget_us,
            percent_time_foreground_on_captured_monitor: foreground_summary
                .percent_time_foreground_on_captured_monitor,
            long_gaps_while_game_foreground: foreground_summary.long_gaps_while_game_foreground,
            long_gaps_while_other_foreground: foreground_summary.long_gaps_while_other_foreground,
            foreground_exe_most_common: foreground_summary.foreground_exe_most_common,
            foreground_title_most_common: foreground_summary.foreground_title_most_common,
            overlay_mode: config.overlay_mode.as_str().to_string(),
            overlay_created: config.overlay_created,
            foreground_unchanged: config.overlay_foreground_unchanged,
        };

        if let Some(csv_path) = &config.csv_path {
            crate::stats::write_summary_csv(&summary, csv_path, config)?;
            println!("[WGC] Summary CSV written to {}", csv_path);
        }

        Ok(summary)
    }

    fn start_capture(
        &mut self,
    ) -> anyhow::Result<(Direct3D11CaptureFramePool, GraphicsCaptureSession)> {
        let item = self
            .item
            .take()
            .ok_or_else(|| anyhow::anyhow!("Capture item not available"))?;
        let device = self
            .winrt_device
            .take()
            .ok_or_else(|| anyhow::anyhow!("WinRT device not available"))?;

        // Create frame pool
        let size = item.Size()?;

        let frame_pool: Direct3D11CaptureFramePool = Direct3D11CaptureFramePool::Create(
            &device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2, // buffer count
            size,
        )?;

        println!(
            "[WGC] Created frame pool with size {}x{}",
            size.Width, size.Height
        );

        // Create capture session
        let session: GraphicsCaptureSession = frame_pool.CreateCaptureSession(&item)?;

        // Start capture
        session.StartCapture()?;

        println!("[WGC] Capture started");
        println!("[WGC] Pixel format: B8G8R8A8UIntNormalized");
        println!("[WGC] Buffer count: 2");

        // Store for cleanup
        self.item = Some(item);
        self.frame_pool = Some(frame_pool.clone());
        self.session = Some(session.clone());

        Ok((frame_pool, session))
    }

    fn stop_capture(&mut self) {
        self.running.store(false, Ordering::SeqCst);

        // Session will be dropped and capture stopped
        self.session = None;
        self.frame_pool = None;

        println!("[WGC] Capture session stopped");
    }

    fn wait_for_frame(
        &self,
        frame_pool: &Direct3D11CaptureFramePool,
        _timeout_ms: u32,
    ) -> anyhow::Result<
        Option<(
            windows::Graphics::DirectX::Direct3D11::IDirect3DSurface,
            windows::Graphics::Capture::Direct3D11CaptureFrame,
        )>,
    > {
        // Try to get a frame
        let frame = match frame_pool.TryGetNextFrame() {
            Ok(f) => f,
            Err(_) => {
                // No frame available, return None
                return Ok(None);
            }
        };

        let surface = frame.Surface()?;
        Ok(Some((surface, frame)))
    }
}

pub trait CaptureBenchmark {
    fn name(&self) -> &'static str;
    fn run(&mut self, config: &BenchConfig) -> anyhow::Result<BenchSummary>;
}

impl CaptureBenchmark for WgcCapture {
    fn name(&self) -> &'static str {
        "WGC"
    }

    fn run(&mut self, config: &BenchConfig) -> anyhow::Result<BenchSummary> {
        self.run(config)
    }
}

// Helper: Create WinRT IDirect3DDevice from D3D11 device
fn create_winrt_device(d3d11_device: &ID3D11Device) -> anyhow::Result<IDirect3DDevice> {
    use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;

    // Get the DXGI device from D3D11 device
    unsafe {
        let dxgi_device: windows::Win32::Graphics::Dxgi::IDXGIDevice = d3d11_device.cast()?;

        // Create WinRT device from DXGI device - returns IInspectable that we need to cast
        let inspectable: windows::core::IInspectable =
            CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)?;

        // Cast to IDirect3DDevice
        let winrt_device: IDirect3DDevice = inspectable.cast()?;

        Ok(winrt_device)
    }
}

// Helper: Create GraphicsCaptureItem for a monitor using IGraphicsCaptureItemInterop
fn create_capture_item_for_monitor(hmonitor: HMONITOR) -> anyhow::Result<GraphicsCaptureItem> {
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

    unsafe {
        // Get the activation factory for GraphicsCaptureItem using RoGetActivationFactory
        let factory: windows::core::IUnknown =
            windows::Win32::System::WinRT::RoGetActivationFactory(&windows::core::HSTRING::from(
                "Windows.Graphics.Capture.GraphicsCaptureItem",
            ))?;

        // Cast to IGraphicsCaptureItemInterop
        let interop: IGraphicsCaptureItemInterop = factory.cast()?;

        // Create the capture item for this monitor - returns Result<T>
        let item: GraphicsCaptureItem = interop.CreateForMonitor(hmonitor)?;

        Ok(item)
    }
}

// Helper: Extract ID3D11Texture2D from WGC surface
fn get_d3d11_texture_from_surface(
    surface: &windows::Graphics::DirectX::Direct3D11::IDirect3DSurface,
) -> anyhow::Result<ID3D11Texture2D> {
    use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;

    unsafe {
        // Use IDirect3DDxgiInterfaceAccess to get the DXGI surface
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;

        // Get the underlying DXGI resource
        let dxgi_resource: windows::Win32::Graphics::Dxgi::IDXGISurface = access.GetInterface()?;

        // Cast to ID3D11Texture2D
        let texture: ID3D11Texture2D = dxgi_resource.cast()?;

        Ok(texture)
    }
}

// Helper: Create ring textures for copy mode
fn create_ring_textures(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> anyhow::Result<Vec<ID3D11Texture2D>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT(87), // DXGI_FORMAT_B8G8R8A8_UNORM
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE(0),
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut textures = Vec::new();
    for _ in 0..12 {
        unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let texture = tex.ok_or_else(|| anyhow::anyhow!("Failed to create ring texture"))?;
            textures.push(texture);
        }
    }

    Ok(textures)
}

fn percentile_f64(values: &[f64], pct: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((sorted.len() as f64) * pct) as usize;
    sorted[idx.min(sorted.len() - 1)]
}
