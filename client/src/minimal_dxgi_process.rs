#[cfg(not(all(target_os = "windows", feature = "wgc-capture")))]
pub fn run_from_env_if_requested() -> bool {
    env_truthy("ASTRIX_MINIMAL_DXGI_PROCESS")
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub fn run_from_env_if_requested() -> bool {
    if !env_truthy("ASTRIX_MINIMAL_DXGI_PROCESS") {
        return false;
    }

    match run_minimal_dxgi_process() {
        Ok(()) => {}
        Err(err) => {
            eprintln!("[minimal-dxgi] fatal: {err}");
            crate::telemetry::log_pipeline_heartbeat("minimal-dxgi", &format!("fatal={err}"));
        }
    }
    true
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on"))
        .unwrap_or(false)
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
mod windows_impl {
    use super::env_truthy;

    use std::thread;
    use std::time::{Duration, Instant};

    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_11_1,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Query, ID3D11Texture2D,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_SDK_VERSION,
        D3D11_TEXTURE2D_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
        IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
        DXGI_OUTDUPL_FRAME_INFO,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, SetPriorityClass, SetThreadPriority,
        HIGH_PRIORITY_CLASS, THREAD_PRIORITY_HIGHEST,
    };
    use windows_core::BOOL;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    pub(super) fn run_minimal_dxgi_process() -> Result<()> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        apply_priority_from_env();

        let config = Config::from_env();
        eprintln!(
            "[minimal-dxgi] start monitor={} timeout_ms={} ring_size={} wait_copy_ready={}",
            config.monitor, config.timeout_ms, config.ring_size, config.wait_copy_ready
        );
        crate::telemetry::log_pipeline_heartbeat(
            "minimal-dxgi",
            &format!(
                "start monitor={} timeout_ms={} ring_size={} wait_copy_ready={}",
                config.monitor, config.timeout_ms, config.ring_size, config.wait_copy_ready
            ),
        );

        let mut capture = MinimalDxgiCapture::new(config.monitor)?;
        let mut copy_stage = CopyStage::new(
            &capture.device,
            &capture.context,
            config.ring_size,
            config.wait_copy_ready,
        );
        let mut stats = WindowStats::new();

        loop {
            match capture.acquire_copy_release(config.timeout_ms, &mut copy_stage) {
                Ok(CaptureTick::Acquired) => stats.acquire_ok += 1,
                Ok(CaptureTick::Timeout) => stats.wait_timeout_count += 1,
                Ok(CaptureTick::CopyFailed) => {
                    stats.acquire_ok += 1;
                    stats.copy_fail_count += 1;
                }
                Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                    stats.access_lost_count += 1;
                    eprintln!("[minimal-dxgi] access lost, reinitializing duplication");
                    match MinimalDxgiCapture::new(config.monitor) {
                        Ok(next) => {
                            capture = next;
                            copy_stage = CopyStage::new(
                                &capture.device,
                                &capture.context,
                                config.ring_size,
                                config.wait_copy_ready,
                            );
                        }
                        Err(reinit_err) => {
                            stats.acquire_errors += 1;
                            eprintln!("[minimal-dxgi] reinit failed: {reinit_err}");
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
                Err(err) => {
                    stats.acquire_errors += 1;
                    eprintln!("[minimal-dxgi] acquire/copy failed: {err:?}");
                    thread::sleep(Duration::from_millis(10));
                }
            }

            if stats.window_start.elapsed() >= Duration::from_secs(1) {
                stats.emit_and_reset();
            }
        }
    }

    struct Config {
        monitor: usize,
        timeout_ms: u32,
        ring_size: usize,
        wait_copy_ready: bool,
    }

    impl Config {
        fn from_env() -> Self {
            Self {
                monitor: parse_monitor_index(),
                timeout_ms: parse_env_u32("ASTRIX_MINIMAL_DXGI_TIMEOUT_MS", 2),
                ring_size: parse_env_usize("ASTRIX_MINIMAL_DXGI_RING_SIZE", 4).max(1),
                wait_copy_ready: env_truthy("ASTRIX_MINIMAL_DXGI_WAIT_COPY_READY"),
            }
        }
    }

    struct WindowStats {
        window_start: Instant,
        acquire_ok: u64,
        wait_timeout_count: u64,
        access_lost_count: u64,
        acquire_errors: u64,
        copy_fail_count: u64,
    }

    impl WindowStats {
        fn new() -> Self {
            Self {
                window_start: Instant::now(),
                acquire_ok: 0,
                wait_timeout_count: 0,
                access_lost_count: 0,
                acquire_errors: 0,
                copy_fail_count: 0,
            }
        }

        fn emit_and_reset(&mut self) {
            let elapsed_sec = self.window_start.elapsed().as_secs_f32().max(0.001);
            let capture_fps = self.acquire_ok as f32 / elapsed_sec;
            let message = format!(
                "minimal=1 capture_fps={:.1} acquire_ok={} wait_timeout_count={} access_lost_count={} acquire_errors={} copy_fail_count={}",
                capture_fps,
                self.acquire_ok,
                self.wait_timeout_count,
                self.access_lost_count,
                self.acquire_errors,
                self.copy_fail_count,
            );
            eprintln!("[minimal-dxgi] {message}");
            crate::telemetry::log_pipeline_heartbeat("minimal-dxgi", &message);

            self.window_start = Instant::now();
            self.acquire_ok = 0;
            self.wait_timeout_count = 0;
            self.access_lost_count = 0;
            self.acquire_errors = 0;
            self.copy_fail_count = 0;
        }
    }

    struct MinimalDxgiCapture {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        duplication: IDXGIOutputDuplication,
    }

    impl MinimalDxgiCapture {
        fn new(monitor: usize) -> Result<Self> {
            let choice = select_dxgi_output(monitor)?;
            let (device, context) = create_d3d11_device(&choice.adapter)?;
            let output1: IDXGIOutput1 = choice.output.cast()?;
            let duplication = unsafe { output1.DuplicateOutput(&device)? };

            eprintln!("[minimal-dxgi] DuplicateOutput ready for DXGI output index {monitor}");
            crate::telemetry::log_pipeline_heartbeat(
                "minimal-dxgi",
                &format!("duplicate_output_ready monitor={monitor}"),
            );

            Ok(Self {
                device,
                context,
                duplication,
            })
        }

        fn acquire_copy_release(
            &mut self,
            timeout_ms: u32,
            copy_stage: &mut CopyStage,
        ) -> windows::core::Result<CaptureTick> {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            let result = unsafe {
                self.duplication
                    .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
            };

            match result {
                Ok(()) => {
                    let copy_result = if let Some(texture) = resource
                        .as_ref()
                        .and_then(|resource| resource.cast::<ID3D11Texture2D>().ok())
                    {
                        copy_stage.copy(&texture)
                    } else {
                        Err(windows::core::Error::from_win32())
                    };
                    drop(resource);
                    unsafe {
                        self.duplication.ReleaseFrame()?;
                    }
                    match copy_result {
                        Ok(()) => Ok(CaptureTick::Acquired),
                        Err(err) => {
                            eprintln!("[minimal-dxgi] CopyResource/texture cast failed: {err:?}");
                            Ok(CaptureTick::CopyFailed)
                        }
                    }
                }
                Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(CaptureTick::Timeout),
                Err(err) => Err(err),
            }
        }
    }

    enum CaptureTick {
        Acquired,
        Timeout,
        CopyFailed,
    }

    struct DxgiOutputChoice {
        adapter: IDXGIAdapter,
        output: IDXGIOutput,
    }

    fn select_dxgi_output(monitor_index: usize) -> Result<DxgiOutputChoice> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut flat_index = 0usize;
            let mut adapter_index = 0u32;

            while let Ok(adapter1) = factory.EnumAdapters1(adapter_index) {
                let adapter: IDXGIAdapter = adapter1.cast()?;
                let mut output_index = 0u32;

                while let Ok(output) = adapter.EnumOutputs(output_index) {
                    if flat_index == monitor_index {
                        eprintln!(
                            "[minimal-dxgi] selected adapter={} output={} flat_monitor={}",
                            adapter_index, output_index, monitor_index
                        );
                        return Ok(DxgiOutputChoice { adapter, output });
                    }

                    flat_index += 1;
                    output_index += 1;
                }

                adapter_index += 1;
            }
        }

        Err(format!("monitor {monitor_index} not found via DXGI").into())
    }

    fn create_d3d11_device(adapter: &IDXGIAdapter) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
        unsafe {
            let feature_levels = [
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ];
            let mut device = None;
            let mut context = None;
            let mut selected_feature_level = D3D_FEATURE_LEVEL(0);

            D3D11CreateDevice(
                Some(adapter),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut selected_feature_level),
                Some(&mut context),
            )?;

            eprintln!(
                "[minimal-dxgi] D3D11 device ready feature_level=0x{:x} flags=BGRA_SUPPORT",
                selected_feature_level.0
            );

            Ok((
                device.ok_or_else(|| "D3D11 device was not returned".to_string())?,
                context.ok_or_else(|| "D3D11 context was not returned".to_string())?,
            ))
        }
    }

    struct CopyStage {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        ring_size: usize,
        wait_ready: bool,
        ring: Vec<ID3D11Texture2D>,
        next_slot: usize,
        query: Option<ID3D11Query>,
    }

    impl CopyStage {
        fn new(
            device: &ID3D11Device,
            context: &ID3D11DeviceContext,
            ring_size: usize,
            wait_ready: bool,
        ) -> Self {
            Self {
                device: device.clone(),
                context: context.clone(),
                ring_size,
                wait_ready,
                ring: Vec::new(),
                next_slot: 0,
                query: None,
            }
        }

        fn copy(&mut self, source: &ID3D11Texture2D) -> windows::core::Result<()> {
            self.ensure_ring(source)?;
            let slot = self.next_slot;
            self.next_slot = (self.next_slot + 1) % self.ring.len();

            unsafe {
                self.context.CopyResource(&self.ring[slot], source);
            }

            if self.wait_ready {
                self.wait_for_copy()?;
            }

            Ok(())
        }

        fn ensure_ring(&mut self, source: &ID3D11Texture2D) -> windows::core::Result<()> {
            if !self.ring.is_empty() {
                return Ok(());
            }

            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe {
                source.GetDesc(&mut desc);
            }
            desc.BindFlags = 0;
            desc.CPUAccessFlags = 0;
            desc.MiscFlags = 0;

            for _ in 0..self.ring_size {
                let mut texture: Option<ID3D11Texture2D> = None;
                unsafe {
                    self.device.CreateTexture2D(
                        &desc,
                        None,
                        Some(std::ptr::from_mut(&mut texture)),
                    )?;
                }
                self.ring
                    .push(texture.ok_or_else(windows::core::Error::from_win32)?);
            }

            eprintln!(
                "[minimal-dxgi] private copy ring created {}x{} format={:?} slots={}",
                desc.Width, desc.Height, desc.Format, self.ring_size
            );
            Ok(())
        }

        fn wait_for_copy(&mut self) -> windows::core::Result<()> {
            if self.query.is_none() {
                let desc = D3D11_QUERY_DESC {
                    Query: D3D11_QUERY_EVENT,
                    MiscFlags: 0,
                };
                let mut query = None;
                unsafe {
                    self.device.CreateQuery(&desc, Some(&mut query))?;
                }
                self.query = Some(query.ok_or_else(windows::core::Error::from_win32)?);
            }

            let query = self.query.as_ref().expect("query initialized");
            unsafe {
                self.context.End(query);
            }

            loop {
                let mut done = BOOL(0);
                unsafe {
                    let _ = self.context.GetData(
                        query,
                        Some(&mut done as *mut _ as *mut core::ffi::c_void),
                        std::mem::size_of::<BOOL>() as u32,
                        0,
                    );
                }
                if done.as_bool() {
                    return Ok(());
                }
                thread::yield_now();
            }
        }
    }

    fn parse_monitor_index() -> usize {
        std::env::var("ASTRIX_MINIMAL_DXGI_MONITOR")
            .ok()
            .and_then(|value| value.parse().ok())
            .or_else(|| {
                std::env::var("ASTRIX_SCREEN_SOURCE")
                    .ok()
                    .and_then(|source| parse_screen_source_monitor(&source))
            })
            .unwrap_or(0)
    }

    fn parse_screen_source_monitor(source: &str) -> Option<usize> {
        let parts: Vec<&str> = source.split(':').collect();
        match parts.as_slice() {
            ["monitor", index] => index.parse().ok(),
            [index] => index.parse().ok(),
            _ => None,
        }
    }

    fn parse_env_u32(name: &str, default: u32) -> u32 {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    }

    fn parse_env_usize(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    }

    fn apply_priority_from_env() {
        if !env_truthy("ASTRIX_HIGH_PRIORITY_PIPELINE") {
            return;
        }

        unsafe {
            let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        }
        eprintln!(
            "[minimal-dxgi] priority HIGH_PRIORITY_CLASS / THREAD_PRIORITY_HIGHEST (ASTRIX_HIGH_PRIORITY_PIPELINE={})",
            std::env::var("ASTRIX_HIGH_PRIORITY_PIPELINE")
                .unwrap_or_else(|_| "<default:off>".to_string())
        );
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use windows_impl::run_minimal_dxgi_process;
