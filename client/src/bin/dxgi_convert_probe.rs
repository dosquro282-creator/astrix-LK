#![cfg_attr(not(all(target_os = "windows", feature = "wgc-capture")), allow(dead_code))]

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
#[path = "../gpu_device.rs"]
mod gpu_device;

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
#[path = "../dxgi_duplication.rs"]
mod dxgi_duplication;

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
#[path = "../d3d11_nv12.rs"]
mod d3d11_nv12;

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
mod mft_encoder {
    #[derive(Debug, Clone)]
    pub struct EncodedFrame {
        pub data: Vec<u8>,
        pub timestamp_us: i64,
        pub key_frame: bool,
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
mod nvenc_d11_bridge;

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
#[path = "../nvenc_d11.rs"]
mod nvenc_d11;

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
mod app {
    use std::env;
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use crate::d3d11_nv12::D3d11BgraScale;
    use crate::dxgi_duplication::{AcquiredDesktopFrame, DxgiDuplicationCapture};
    use crate::mft_encoder::EncodedFrame;
    use crate::nvenc_d11::{NvencD3d11Encoder, NvencD3d11Error};
    use parking_lot::Mutex;
    use windows::core::w;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, HBRUSH};
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        PostThreadMessageW, RegisterClassW, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
        HTTRANSPARENT, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_NCHITTEST, WM_QUIT,
        WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    #[derive(Debug, Clone)]
    struct Args {
        screen: usize,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_mbps: f64,
        frames: usize,
        acquire_timeout_ms: u32,
        ready_timeout_ms: u32,
        collect_timeout_ms: u32,
        max_timeouts: usize,
        nvenc: bool,
        overlay: bool,
        overlay_thickness: i32,
    }

    impl Default for Args {
        fn default() -> Self {
            Self {
                screen: 0,
                width: 1920,
                height: 1080,
                fps: 120,
                bitrate_mbps: 26.0,
                frames: 10,
                acquire_timeout_ms: 250,
                ready_timeout_ms: 250,
                collect_timeout_ms: 40,
                max_timeouts: 40,
                nvenc: true,
                overlay: true,
                overlay_thickness: 6,
            }
        }
    }

    #[derive(Default, Clone)]
    struct Metric {
        count: u64,
        total_us: u128,
        min_us: u128,
        max_us: u128,
    }

    impl Metric {
        fn push(&mut self, value_us: u128) {
            self.count += 1;
            self.total_us += value_us;
            if self.count == 1 {
                self.min_us = value_us;
                self.max_us = value_us;
            } else {
                self.min_us = self.min_us.min(value_us);
                self.max_us = self.max_us.max(value_us);
            }
        }

        fn avg_us(&self) -> f64 {
            if self.count == 0 {
                0.0
            } else {
                self.total_us as f64 / self.count as f64
            }
        }
    }

    #[derive(Default)]
    struct Summary {
        acquire_wait: Metric,
        frame_cast: Metric,
        copy_to_pool: Metric,
        scale_submit: Metric,
        flush: Metric,
        ready_poll: Metric,
        ready_wait: Metric,
        total_prepare: Metric,
        nvenc_submit_cpu: Metric,
        nvenc_submit_gpu_total: Metric,
        nvenc_map: Metric,
        nvenc_pic: Metric,
        nvenc_collect_wait: Metric,
        nvenc_encode_reported: Metric,
        frame_total: Metric,
        acquire_timeouts: usize,
        ready_immediate: usize,
        ready_waited: usize,
        queue_full: usize,
        collect_none: usize,
        encoded_frames: usize,
        encoded_key_frames: usize,
        encoded_bytes: usize,
    }

    #[derive(Default)]
    struct EncodedOutputStats {
        frames: usize,
        key_frames: usize,
        bytes: usize,
    }

    impl EncodedOutputStats {
        fn merge(&mut self, other: Self) {
            self.frames += other.frames;
            self.key_frames += other.key_frames;
            self.bytes += other.bytes;
        }
    }

    struct MonitorBorderOverlay {
        thread_id: u32,
        join: Option<JoinHandle<()>>,
    }

    pub fn main() -> Result<(), Box<dyn std::error::Error>> {
        let args = parse_args()?;

        eprintln!(
            "[dxgi_convert_probe] settings: screen={} target={}x{} fps={} bitrate={:.1}Mbps frames={} acquire_timeout={}ms ready_timeout={}ms collect_timeout={}ms nvenc={} overlay={}",
            args.screen,
            args.width,
            args.height,
            args.fps,
            args.bitrate_mbps,
            args.frames,
            args.acquire_timeout_ms,
            args.ready_timeout_ms,
            args.collect_timeout_ms,
            args.nvenc,
            args.overlay
        );
        eprintln!(
            "[dxgi_convert_probe] tip: keep the chosen monitor animated while probing, otherwise DXGI may wait for frame changes"
        );

        let init_capture_start = Instant::now();
        let capture = DxgiDuplicationCapture::new(Some(args.screen))?;
        let init_capture_us = init_capture_start.elapsed().as_micros();
        eprintln!(
            "[dxgi_convert_probe] capture init: {} us, source={}x{} monitor='{}'",
            init_capture_us,
            capture.selection.width,
            capture.selection.height,
            capture.selection.monitor_name
        );

        let _overlay = if args.overlay {
            Some(MonitorBorderOverlay::start(
                capture.selection.desktop_x,
                capture.selection.desktop_y,
                capture.selection.width as i32,
                capture.selection.height as i32,
                args.overlay_thickness,
            )?)
        } else {
            None
        };
        if args.overlay {
            eprintln!(
                "[dxgi_convert_probe] overlay: yellow border on {}x{} at {},{} (thickness={})",
                capture.selection.width,
                capture.selection.height,
                capture.selection.desktop_x,
                capture.selection.desktop_y,
                args.overlay_thickness
            );
        }

        let (first_frame, initial_wait_us, initial_timeouts) =
            acquire_frame_blocking(&capture, args.acquire_timeout_ms, args.max_timeouts)?;
        let first_texture = first_frame.texture()?;
        let input_texture = create_probe_input_texture(&capture.device, &first_texture)?;
        log_texture_desc("[dxgi_convert_probe] source texture", &first_texture);
        log_texture_desc("[dxgi_convert_probe] probe input texture", &input_texture);

        let init_scaler_start = Instant::now();
        let scaler = D3d11BgraScale::new(
            &capture.device,
            &capture.context,
            capture.selection.width,
            capture.selection.height,
            args.width,
            args.height,
            args.fps,
        )?;
        let init_scaler_us = init_scaler_start.elapsed().as_micros();
        eprintln!(
            "[dxgi_convert_probe] scaler init: {} us, output_ring={} surface(s)",
            init_scaler_us,
            scaler.output_textures().len()
        );

        let mut nvenc = if args.nvenc {
            let bitrate = (args.bitrate_mbps * 1_000_000.0).round().max(1.0) as u32;
            let encoder = NvencD3d11Encoder::new(
                &capture.device,
                args.width,
                args.height,
                args.fps.max(1),
                bitrate,
                scaler.output_textures(),
            )?;
            eprintln!(
                "[dxgi_convert_probe] nvenc init: {} async={} input_ring={} rgb_input={}",
                encoder.encoder_name(),
                encoder.is_async(),
                encoder.input_ring_size(),
                encoder.uses_rgb_input()
            );
            Some(encoder)
        } else {
            eprintln!("[dxgi_convert_probe] nvenc disabled (--no-nvenc)");
            None
        };

        let context_mutex = Mutex::new(());
        let mut summary = Summary::default();
        let mut frame_index = 0usize;
        let mut pending_first_frame = Some(first_frame);

        while frame_index < args.frames {
            let frame_total_start = Instant::now();
            let (frame, acquire_wait_us, acquire_timeouts) =
                if let Some(frame) = pending_first_frame.take() {
                    (frame, initial_wait_us, initial_timeouts)
                } else {
                    acquire_frame_blocking(&capture, args.acquire_timeout_ms, args.max_timeouts)?
                };

            summary.acquire_wait.push(acquire_wait_us);
            summary.acquire_timeouts += acquire_timeouts;

            let cast_start = Instant::now();
            let frame_texture = frame.texture()?;
            let cast_us = cast_start.elapsed().as_micros();
            summary.frame_cast.push(cast_us);

            let prepare_start = Instant::now();

            let copy_start = Instant::now();
            {
                let _ctx_guard = context_mutex.lock();
                unsafe {
                    capture.context.CopyResource(&input_texture, &frame_texture);
                }
            }
            let copy_us = copy_start.elapsed().as_micros();
            summary.copy_to_pool.push(copy_us);

            let convert_start = Instant::now();
            let output_texture = scaler.convert(&input_texture, &context_mutex)?;
            let convert_us = convert_start.elapsed().as_micros();
            summary.scale_submit.push(convert_us);

            let flush_start = Instant::now();
            scaler.flush(&context_mutex)?;
            let flush_us = flush_start.elapsed().as_micros();
            summary.flush.push(flush_us);

            let poll_start = Instant::now();
            let ready_now = scaler.poll_output_ready()?;
            let poll_us = poll_start.elapsed().as_micros();
            summary.ready_poll.push(poll_us);

            let ready_wait_us = if ready_now {
                summary.ready_immediate += 1;
                0u128
            } else {
                summary.ready_waited += 1;
                scaler.wait_output_ready(args.ready_timeout_ms)? as u128
            };
            summary.ready_wait.push(ready_wait_us);

            let total_prepare_us = prepare_start.elapsed().as_micros();
            summary.total_prepare.push(total_prepare_us);

            let mut submit_cpu_us = 0u128;
            let mut submit_gpu_total_us = 0u128;
            let mut submit_map_us = 0u128;
            let mut submit_pic_us = 0u128;
            let mut collect_wait_us = 0u128;
            let mut collect_outputs = EncodedOutputStats::default();

            if let Some(encoder) = nvenc.as_mut() {
                let timestamp_us =
                    ((frame_index as u64) * 1_000_000u64 / args.fps.max(1) as u64) as i64;
                loop {
                    let submit_start = Instant::now();
                    match encoder.submit(
                        &output_texture,
                        timestamp_us,
                        frame_index == 0,
                        0,
                        timestamp_us,
                        args.collect_timeout_ms,
                    ) {
                        Ok(()) => {
                            submit_cpu_us = submit_start.elapsed().as_micros();
                            let breakdown = encoder.last_submit_breakdown();
                            submit_map_us = breakdown.map_us as u128;
                            submit_pic_us = breakdown.encode_picture_us as u128;
                            submit_gpu_total_us = breakdown.total_us as u128;
                            summary.nvenc_submit_cpu.push(submit_cpu_us);
                            summary.nvenc_submit_gpu_total.push(submit_gpu_total_us);
                            summary.nvenc_map.push(submit_map_us);
                            summary.nvenc_pic.push(submit_pic_us);
                            break;
                        }
                        Err(NvencD3d11Error::QueueFull { .. }) => {
                            summary.queue_full += 1;
                            let collect_start = Instant::now();
                            let drained = encoder.collect_blocking(args.collect_timeout_ms)?;
                            let wait_us = collect_start.elapsed().as_micros();
                            let drained_none = drained.is_none();
                            collect_wait_us += wait_us;
                            collect_outputs.merge(record_encoded_output(
                                &mut summary,
                                drained,
                                wait_us,
                            ));
                            if drained_none {
                                break;
                            }
                        }
                        Err(err) => return Err(err.into()),
                    }
                }

                let collect_start = Instant::now();
                let collected = encoder.collect_blocking(args.collect_timeout_ms)?;
                let wait_us = collect_start.elapsed().as_micros();
                collect_wait_us += wait_us;
                collect_outputs.merge(record_encoded_output(&mut summary, collected, wait_us));
            }

            let frame_total_us = frame_total_start.elapsed().as_micros();
            summary.frame_total.push(frame_total_us);

            if frame_index == 0 {
                log_texture_desc("[dxgi_convert_probe] scaler output texture", &output_texture);
            }

            eprintln!(
                "[dxgi_convert_probe][frame {:02}] acquire={}us cast={}us copy={}us convert={}us flush={}us ready_poll={}us ready_wait={}us submit_cpu={}us map={}us pic={}us collect={}us prepare={}us total={}us ready_now={} outputs={} bytes={} key={} qfull={} timeouts={}",
                frame_index + 1,
                acquire_wait_us,
                cast_us,
                copy_us,
                convert_us,
                flush_us,
                poll_us,
                ready_wait_us,
                submit_cpu_us,
                submit_map_us,
                submit_pic_us,
                collect_wait_us,
                total_prepare_us,
                frame_total_us,
                ready_now,
                collect_outputs.frames,
                collect_outputs.bytes,
                collect_outputs.key_frames,
                summary.queue_full,
                acquire_timeouts
            );

            frame_index += 1;
            drop(output_texture);
            drop(frame_texture);
            drop(frame);
        }

        if let Some(encoder) = nvenc.as_mut() {
            for _ in 0..encoder.input_ring_size().saturating_add(2) {
                let collect_start = Instant::now();
                let collected = encoder.collect_blocking(args.collect_timeout_ms)?;
                let wait_us = collect_start.elapsed().as_micros();
                if collected.is_none() {
                    summary.collect_none += 1;
                    break;
                }
                let outputs = record_encoded_output(&mut summary, collected, wait_us);
                eprintln!(
                    "[dxgi_convert_probe][drain] collect={}us outputs={} bytes={} key={}",
                    wait_us, outputs.frames, outputs.bytes, outputs.key_frames
                );
            }
        }

        print_summary(&summary, init_capture_us, init_scaler_us);
        Ok(())
    }

    fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
        let mut args = Args::default();
        let mut iter = env::args().skip(1);
        while let Some(flag) = iter.next() {
            let value = match flag.as_str() {
                "--screen"
                | "--width"
                | "--height"
                | "--fps"
                | "--bitrate-mbps"
                | "--frames"
                | "--acquire-timeout-ms"
                | "--ready-timeout-ms"
                | "--collect-timeout-ms"
                | "--max-timeouts"
                | "--overlay-thickness" => iter
                    .next()
                    .ok_or_else(|| format!("missing value for {}", flag))?,
                "--no-nvenc" => {
                    args.nvenc = false;
                    continue;
                }
                "--no-overlay" => {
                    args.overlay = false;
                    continue;
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {}", other).into()),
            };

            match flag.as_str() {
                "--screen" => args.screen = value.parse()?,
                "--width" => args.width = value.parse()?,
                "--height" => args.height = value.parse()?,
                "--fps" => args.fps = value.parse()?,
                "--bitrate-mbps" => args.bitrate_mbps = value.parse()?,
                "--frames" => args.frames = value.parse()?,
                "--acquire-timeout-ms" => args.acquire_timeout_ms = value.parse()?,
                "--ready-timeout-ms" => args.ready_timeout_ms = value.parse()?,
                "--collect-timeout-ms" => args.collect_timeout_ms = value.parse()?,
                "--max-timeouts" => args.max_timeouts = value.parse()?,
                "--overlay-thickness" => args.overlay_thickness = value.parse()?,
                _ => unreachable!(),
            }
        }
        Ok(args)
    }

    fn print_help() {
        eprintln!("dxgi_convert_probe");
        eprintln!("  --screen <idx>               monitor index (default: 0)");
        eprintln!("  --width <px>                target width (default: 1920)");
        eprintln!("  --height <px>               target height (default: 1080)");
        eprintln!("  --fps <n>                   scaler fps hint (default: 120)");
        eprintln!("  --bitrate-mbps <n>          NVENC bitrate target in Mbps (default: 26.0)");
        eprintln!("  --frames <n>                frames to probe (default: 10)");
        eprintln!("  --acquire-timeout-ms <ms>   DXGI wait timeout per poll (default: 250)");
        eprintln!("  --ready-timeout-ms <ms>     scaler ready wait timeout (default: 250)");
        eprintln!("  --collect-timeout-ms <ms>   NVENC collect wait timeout (default: 40)");
        eprintln!("  --max-timeouts <n>          max DXGI timeouts before abort (default: 40)");
        eprintln!("  --no-nvenc                  disable NVENC stage");
        eprintln!("  --no-overlay                disable yellow monitor border overlay");
        eprintln!("  --overlay-thickness <px>    monitor border thickness (default: 6)");
    }

    fn acquire_frame_blocking(
        capture: &DxgiDuplicationCapture,
        timeout_ms: u32,
        max_timeouts: usize,
    ) -> Result<(AcquiredDesktopFrame, u128, usize), Box<dyn std::error::Error>> {
        let start = Instant::now();
        let mut timeouts = 0usize;
        loop {
            match capture.acquire_next_frame(timeout_ms)? {
                Some(frame) => return Ok((frame, start.elapsed().as_micros(), timeouts)),
                None => {
                    timeouts += 1;
                    if timeouts >= max_timeouts {
                        return Err(format!(
                            "DXGI timed out waiting for a changed frame {} times in a row; keep the target monitor animated",
                            timeouts
                        )
                        .into());
                    }
                }
            }
        }
    }

    fn create_probe_input_texture(
        device: &ID3D11Device,
        source: &ID3D11Texture2D,
    ) -> Result<ID3D11Texture2D, Box<dyn std::error::Error>> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            source.GetDesc(&mut desc);
        }
        if desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB.into() {
            desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM.into();
        }
        desc.Usage = D3D11_USAGE_DEFAULT;
        desc.BindFlags = (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32;
        desc.CPUAccessFlags = 0;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.MiscFlags = 0;

        let mut texture = None;
        unsafe {
            device.CreateTexture2D(&desc, None, Some(&mut texture))?;
        }
        texture.ok_or_else(|| "CreateTexture2D returned null".into())
    }

    fn log_texture_desc(label: &str, texture: &ID3D11Texture2D) {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut desc);
        }
        eprintln!(
            "{}: {}x{} format={:?} bind=0x{:x} usage={:?} sample={}",
            label,
            desc.Width,
            desc.Height,
            desc.Format,
            desc.BindFlags,
            desc.Usage,
            desc.SampleDesc.Count
        );
    }

    fn record_encoded_output(
        summary: &mut Summary,
        output: Option<(Vec<EncodedFrame>, u32, i64, u64)>,
        wait_us: u128,
    ) -> EncodedOutputStats {
        summary.nvenc_collect_wait.push(wait_us);
        let Some((frames, _, _, encode_us)) = output else {
            summary.collect_none += 1;
            return EncodedOutputStats::default();
        };

        summary.nvenc_encode_reported.push(encode_us as u128);
        let mut stats = EncodedOutputStats::default();
        for frame in frames {
            let _ = frame.timestamp_us;
            stats.frames += 1;
            stats.bytes += frame.data.len();
            if frame.key_frame {
                stats.key_frames += 1;
            }
        }
        summary.encoded_frames += stats.frames;
        summary.encoded_key_frames += stats.key_frames;
        summary.encoded_bytes += stats.bytes;
        stats
    }

    fn print_summary(summary: &Summary, init_capture_us: u128, init_scaler_us: u128) {
        eprintln!();
        eprintln!("[dxgi_convert_probe] summary");
        eprintln!(
            "  init_capture: {:.3} ms, init_scaler: {:.3} ms",
            init_capture_us as f64 / 1000.0,
            init_scaler_us as f64 / 1000.0
        );
        print_metric("acquire_wait", &summary.acquire_wait);
        print_metric("frame_cast", &summary.frame_cast);
        print_metric("copy_to_pool", &summary.copy_to_pool);
        print_metric("convert_submit", &summary.scale_submit);
        print_metric("flush", &summary.flush);
        print_metric("ready_poll", &summary.ready_poll);
        print_metric("ready_wait", &summary.ready_wait);
        print_metric("prepare_total", &summary.total_prepare);
        print_metric("nvenc_submit", &summary.nvenc_submit_cpu);
        print_metric("submit_gpu", &summary.nvenc_submit_gpu_total);
        print_metric("map_avg", &summary.nvenc_map);
        print_metric("pic_avg", &summary.nvenc_pic);
        print_metric("collect_wait", &summary.nvenc_collect_wait);
        print_metric("encode_reported", &summary.nvenc_encode_reported);
        print_metric("frame_total", &summary.frame_total);
        eprintln!(
            "  ready_state: immediate={} waited={} acquire_timeouts={}",
            summary.ready_immediate, summary.ready_waited, summary.acquire_timeouts
        );
        eprintln!(
            "  nvenc_state: queue_full={} collect_none={} encoded_frames={} key_frames={} encoded_bytes={}",
            summary.queue_full,
            summary.collect_none,
            summary.encoded_frames,
            summary.encoded_key_frames,
            summary.encoded_bytes
        );
    }

    fn print_metric(label: &str, metric: &Metric) {
        eprintln!(
            "  {:>14}: avg={:8.3} ms min={:8.3} ms max={:8.3} ms count={}",
            label,
            metric.avg_us() / 1000.0,
            metric.min_us as f64 / 1000.0,
            metric.max_us as f64 / 1000.0,
            metric.count
        );
    }

    impl MonitorBorderOverlay {
        fn start(
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            thickness: i32,
        ) -> Result<Self, Box<dyn std::error::Error>> {
            let thickness = thickness.max(1);
            let (ready_tx, ready_rx) = mpsc::channel();
            let join = thread::spawn(move || {
                let thread_id = unsafe { GetCurrentThreadId() };
                let start_result = run_overlay_thread(
                    thread_id,
                    x,
                    y,
                    width.max(1),
                    height.max(1),
                    thickness,
                    &ready_tx,
                );
                if let Err(err) = start_result {
                    let _ = ready_tx.send(Err(err.to_string()));
                }
            });

            let thread_id = match ready_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(Ok(thread_id)) => thread_id,
                Ok(Err(error)) => {
                    let _ = join.join();
                    return Err(error.into());
                }
                Err(_) => {
                    return Err("overlay startup timed out".into());
                }
            };

            Ok(Self {
                thread_id,
                join: Some(join),
            })
        }
    }

    impl Drop for MonitorBorderOverlay {
        fn drop(&mut self) {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    fn run_overlay_thread(
        thread_id: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        thickness: i32,
        ready_tx: &mpsc::Sender<Result<u32, String>>,
    ) -> Result<(), windows::core::Error> {
        let instance = unsafe { GetModuleHandleW(None)? };
        let brush = unsafe { CreateSolidBrush(COLORREF(0x0000_FFFF)) };
        let class_name = w!("AstrixDxgiProbeOverlay");
        let class = WNDCLASSW {
            lpfnWndProc: Some(overlay_wndproc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            hbrBackground: HBRUSH(brush.0),
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }

        let ex_style =
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let style = WS_POPUP | WS_VISIBLE;
        let windows = [
            unsafe { create_overlay_window(ex_style, style, x, y, width, thickness)? },
            unsafe {
                create_overlay_window(
                    ex_style,
                    style,
                    x,
                    y + height.saturating_sub(thickness),
                    width,
                    thickness,
                )?
            },
            unsafe { create_overlay_window(ex_style, style, x, y, thickness, height)? },
            unsafe {
                create_overlay_window(
                    ex_style,
                    style,
                    x + width.saturating_sub(thickness),
                    y,
                    thickness,
                    height,
                )?
            },
        ];

        let _ = ready_tx.send(Ok(thread_id));

        let mut message = MSG::default();
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.into() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        for hwnd in windows {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        unsafe {
            let _ = DeleteObject(brush.into());
        }
        Ok(())
    }

    unsafe fn create_overlay_window(
        ex_style: WINDOW_EX_STYLE,
        style: WINDOW_STYLE,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<HWND, windows::core::Error> {
        let hwnd = CreateWindowExW(
            ex_style,
            w!("AstrixDxgiProbeOverlay"),
            w!(""),
            style,
            x,
            y,
            width.max(1),
            height.max(1),
            None,
            None,
            Some(GetModuleHandleW(None)?.into()),
            None,
        )?;
        Ok(hwnd)
    }

    extern "system" fn overlay_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    app::main()
}

#[cfg(not(all(target_os = "windows", feature = "wgc-capture")))]
fn main() {
    eprintln!("dxgi_convert_probe is available only on Windows with feature `wgc-capture`");
}
