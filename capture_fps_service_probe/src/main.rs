#![cfg_attr(not(windows), allow(unused))]

#[cfg(not(windows))]
compile_error!("capture_fps_service_probe is Windows-only");

#[cfg(windows)]
use std::{
    env,
    fs::{File, OpenOptions},
    io::{IsTerminal, Write},
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use anyhow::{Context, Result};
#[cfg(windows)]
use clap::{Parser, ValueEnum};
#[cfg(windows)]
use windows::{
    core::{Interface, HSTRING},
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{BOOL, LPARAM, RECT, SYSTEMTIME},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Query, ID3D11Texture2D,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
                D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
            },
            Dxgi::{
                CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory1, IDXGIOutput,
                IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, IDXGISurface,
                DXGI_OUTDUPL_FRAME_INFO,
            },
            Gdi::{
                EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
            },
        },
        System::{
            Com::{CoInitializeEx, COINIT_MULTITHREADED},
            SystemInformation::GetLocalTime,
            Threading::{
                GetCurrentProcess, GetCurrentThread, SetPriorityClass, SetThreadPriority,
                HIGH_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS, THREAD_PRIORITY_HIGHEST,
                THREAD_PRIORITY_TIME_CRITICAL,
            },
            WinRT::{
                Direct3D11::CreateDirect3D11DeviceFromDXGIDevice,
                Direct3D11::IDirect3DDxgiInterfaceAccess,
                Graphics::Capture::IGraphicsCaptureItemInterop, RoGetActivationFactory,
                RoInitialize, RO_INIT_MULTITHREADED,
            },
        },
        UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        },
    },
};

#[cfg(windows)]
const DXGI_ERROR_WAIT_TIMEOUT: i32 = 0x887A0027u32 as i32;
#[cfg(windows)]
const DXGI_ERROR_ACCESS_LOST: i32 = 0x887A0026u32 as i32;

#[cfg(windows)]
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Backend {
    Dxgi,
    Wgc,
}

#[cfg(windows)]
impl Backend {
    fn as_str(self) -> &'static str {
        match self {
            Backend::Dxgi => "dxgi",
            Backend::Wgc => "wgc",
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, ValueEnum)]
enum CpuPriority {
    Off,
    High,
    Realtime,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
struct GpuPriority(Option<i32>);

#[cfg(windows)]
impl std::str::FromStr for GpuPriority {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("off") {
            return Ok(Self(None));
        }

        let priority = value
            .parse::<i32>()
            .map_err(|_| format!("invalid gpu priority '{value}', use off or 0..7"))?;
        if !(0..=7).contains(&priority) {
            return Err(format!("invalid gpu priority '{value}', use off or 0..7"));
        }

        Ok(Self(Some(priority)))
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
enum ProbeMode {
    AcquireOnly,
    AcquireLatest,
    Copy,
    CopyLatest,
    CopyWait,
    CopyWaitLatest,
}

#[cfg(windows)]
impl ProbeMode {
    fn from_args(args: &Args) -> Self {
        match (args.allow_copy, args.wait_copy_ready, args.latest_only) {
            (false, _, false) => Self::AcquireOnly,
            (false, _, true) => Self::AcquireLatest,
            (true, false, false) => Self::Copy,
            (true, false, true) => Self::CopyLatest,
            (true, true, false) => Self::CopyWait,
            (true, true, true) => Self::CopyWaitLatest,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::AcquireOnly => "acquire_only",
            Self::AcquireLatest => "acquire_latest",
            Self::Copy => "copy",
            Self::CopyLatest => "copy_latest",
            Self::CopyWait => "copy_wait",
            Self::CopyWaitLatest => "copy_wait_latest",
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Parser)]
#[command(name = "capture_fps_service_probe")]
#[command(about = "Minimal DXGI/WGC FPS probe for an interactive-session capture worker")]
struct Args {
    #[arg(long, value_enum)]
    backend: Backend,

    #[arg(long, default_value_t = 0)]
    monitor: usize,

    #[arg(long, default_value_t = 0)]
    duration_sec: u64,

    #[arg(long, default_value_t = 2)]
    timeout_ms: u32,

    #[arg(long, default_value_t = 1)]
    interval_sec: u64,

    #[arg(long, default_value_t = false)]
    service_like: bool,

    #[arg(long, default_value_t = false)]
    no_stdout: bool,

    #[arg(long, default_value_t = false)]
    allow_copy: bool,

    #[arg(long, default_value_t = 4)]
    ring_size: usize,

    #[arg(long, default_value_t = false)]
    wait_copy_ready: bool,

    #[arg(long, default_value_t = 0)]
    simulate_encode_delay_ms: u64,

    #[arg(long, default_value_t = false)]
    latest_only: bool,

    #[arg(long, value_enum, default_value_t = CpuPriority::Off)]
    cpu_priority: CpuPriority,

    #[arg(long, default_value = "off")]
    gpu_priority: GpuPriority,

    #[arg(long, default_value_t = false)]
    spawn_worker: bool,
}

#[cfg(windows)]
struct FpsSink {
    stdout_enabled: bool,
    file: Option<File>,
}

#[cfg(windows)]
impl FpsSink {
    fn new(service_like: bool, no_stdout: bool) -> Result<Self> {
        let file = if service_like || no_stdout {
            let log_path = exe_neighbor_log_path()?;
            Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .with_context(|| format!("open log file {}", log_path.display()))?,
            )
        } else {
            None
        };

        let stdout_enabled = if no_stdout {
            false
        } else if service_like {
            std::io::stdout().is_terminal()
        } else {
            true
        };

        Ok(Self {
            stdout_enabled,
            file,
        })
    }

    fn start(&mut self, args: &Args) -> Result<()> {
        self.write_line(&format!(
            "[start] backend={} monitor={} allow_copy={} wait_copy_ready={} latest_only={} ring_size={}",
            args.backend.as_str(),
            args.monitor,
            args.allow_copy,
            args.wait_copy_ready,
            args.latest_only,
            args.ring_size,
        ))
    }

    fn fps(
        &mut self,
        backend: Backend,
        monitor: usize,
        frames: u64,
        mode: ProbeMode,
    ) -> Result<()> {
        let line = format!(
            "[fps] ts={} backend={} monitor={} frames={} mode={}",
            local_timestamp(),
            backend.as_str(),
            monitor,
            frames,
            mode.as_str()
        );

        self.write_line(&line)
    }

    fn write_line(&mut self, line: &str) -> Result<()> {
        if self.stdout_enabled {
            println!("{line}");
        }

        if let Some(file) = self.file.as_mut() {
            writeln!(file, "{line}")?;
            file.flush()?;
        }

        Ok(())
    }
}

#[cfg(windows)]
struct CopyStage {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    ring_size: usize,
    wait_ready: bool,
    ring: Vec<ID3D11Texture2D>,
    next_slot: usize,
    query: Option<ID3D11Query>,
}

#[cfg(windows)]
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
            ring_size: ring_size.max(1),
            wait_ready,
            ring: Vec::new(),
            next_slot: 0,
            query: None,
        }
    }

    fn copy(&mut self, source: &ID3D11Texture2D) -> Result<()> {
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

    fn ensure_ring(&mut self, source: &ID3D11Texture2D) -> Result<()> {
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
            let mut texture = None;
            unsafe {
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut texture))
                    .context("create local GPU copy ring texture")?;
            }
            self.ring.push(
                texture.ok_or_else(|| anyhow::anyhow!("CreateTexture2D returned no texture"))?,
            );
        }

        Ok(())
    }

    fn wait_for_copy(&mut self) -> Result<()> {
        if self.query.is_none() {
            let desc = D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            };
            let mut query = None;
            unsafe {
                self.device
                    .CreateQuery(&desc, Some(&mut query))
                    .context("create D3D11 event query")?;
            }
            self.query = Some(query.ok_or_else(|| anyhow::anyhow!("CreateQuery returned none"))?);
        }

        let query = self.query.as_ref().expect("query is initialized");
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
                break;
            }
            thread::yield_now();
        }

        Ok(())
    }
}

#[cfg(windows)]
fn local_timestamp() -> String {
    unsafe {
        let time: SYSTEMTIME = GetLocalTime();
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
            time.wYear,
            time.wMonth,
            time.wDay,
            time.wHour,
            time.wMinute,
            time.wSecond,
            time.wMilliseconds
        )
    }
}

#[cfg(windows)]
fn exe_neighbor_log_path() -> Result<PathBuf> {
    let exe = env::current_exe().context("current executable path")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("executable has no parent directory"))?;
    Ok(dir.join("capture_fps_service_probe.log"))
}

#[cfg(windows)]
fn spawn_worker_and_wait() -> Result<()> {
    let exe = env::current_exe().context("current executable path")?;
    let mut child_args = Vec::new();
    let mut has_service_like = false;

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--spawn-worker" => {}
            "--service-like" => {
                has_service_like = true;
                child_args.push(arg);
            }
            _ => child_args.push(arg),
        }
    }

    if !has_service_like {
        child_args.push("--service-like".to_string());
    }

    let status = Command::new(exe)
        .args(child_args)
        .status()
        .context("spawn capture worker")?;
    if !status.success() {
        anyhow::bail!("worker exited with {status}");
    }

    Ok(())
}

#[cfg(windows)]
fn init_apartment() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
}

#[cfg(windows)]
fn apply_cpu_priority(priority: CpuPriority) {
    unsafe {
        match priority {
            CpuPriority::Off => {}
            CpuPriority::High => {
                let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            }
            CpuPriority::Realtime => {
                let _ = SetPriorityClass(GetCurrentProcess(), REALTIME_PRIORITY_CLASS);
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            }
        }
    }
}

#[cfg(windows)]
fn apply_gpu_priority(device: &ID3D11Device, priority: GpuPriority) {
    if let Some(priority) = priority.0 {
        unsafe {
            if let Ok(dxgi_device) = device.cast::<IDXGIDevice>() {
                let _ = dxgi_device.SetGPUThreadPriority(priority);
            }
        }
    }
}

#[cfg(windows)]
#[derive(Clone)]
struct DxgiOutputChoice {
    adapter: IDXGIAdapter,
    output: IDXGIOutput,
}

#[cfg(windows)]
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
                    return Ok(DxgiOutputChoice { adapter, output });
                }

                flat_index += 1;
                output_index += 1;
            }

            adapter_index += 1;
        }
    }

    anyhow::bail!("monitor {monitor_index} not found via DXGI")
}

#[cfg(windows)]
fn select_dxgi_output_for_hmonitor(hmonitor: HMONITOR) -> Result<DxgiOutputChoice> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut adapter_index = 0u32;

        while let Ok(adapter1) = factory.EnumAdapters1(adapter_index) {
            let adapter: IDXGIAdapter = adapter1.cast()?;
            let mut output_index = 0u32;

            while let Ok(output) = adapter.EnumOutputs(output_index) {
                let desc = output.GetDesc()?;
                if desc.Monitor == hmonitor {
                    return Ok(DxgiOutputChoice { adapter, output });
                }

                output_index += 1;
            }

            adapter_index += 1;
        }
    }

    anyhow::bail!("monitor handle was not found via DXGI")
}

#[cfg(windows)]
fn create_d3d11_device(adapter: &IDXGIAdapter) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        const FL_11_1: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL(0x0000b100);
        const FL_11_0: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL(0x0000b000);
        const FL_10_1: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL(0x0000a100);

        let feature_levels = [FL_11_1, FL_11_0, FL_10_1];
        let mut device = None;
        let mut context = None;
        let mut selected_feature_level = D3D_FEATURE_LEVEL(0);

        D3D11CreateDevice(
            Some(adapter),
            D3D_DRIVER_TYPE_UNKNOWN,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut selected_feature_level),
            Some(&mut context),
        )?;

        Ok((
            device.ok_or_else(|| anyhow::anyhow!("D3D11 device was not returned"))?,
            context.ok_or_else(|| anyhow::anyhow!("D3D11 context was not returned"))?,
        ))
    }
}

#[cfg(windows)]
struct DxgiProbe {
    backend: Backend,
    monitor: usize,
    mode: ProbeMode,
    timeout_ms: u32,
    interval: Duration,
    duration: Option<Duration>,
    latest_only: bool,
    encode_delay: Duration,
    sink: FpsSink,
    device: ID3D11Device,
    _context: ID3D11DeviceContext,
    output: IDXGIOutput,
    duplication: IDXGIOutputDuplication,
    copy_stage: Option<CopyStage>,
}

#[cfg(windows)]
impl DxgiProbe {
    fn new(args: &Args, mut sink: FpsSink) -> Result<Self> {
        let choice = select_dxgi_output(args.monitor)?;
        let (device, context) = create_d3d11_device(&choice.adapter)?;
        apply_gpu_priority(&device, args.gpu_priority);
        let duplication = duplicate_output(&choice.output, &device)?;
        let copy_stage = args
            .allow_copy
            .then(|| CopyStage::new(&device, &context, args.ring_size, args.wait_copy_ready));

        sink.file.as_mut().map(|file| file.flush());

        Ok(Self {
            backend: args.backend,
            monitor: args.monitor,
            mode: ProbeMode::from_args(args),
            timeout_ms: args.timeout_ms,
            interval: Duration::from_secs(args.interval_sec.max(1)),
            duration: duration_from_args(args.duration_sec),
            latest_only: args.latest_only,
            encode_delay: Duration::from_millis(args.simulate_encode_delay_ms),
            sink,
            device,
            _context: context,
            output: choice.output,
            duplication,
            copy_stage,
        })
    }

    fn run(&mut self) -> Result<()> {
        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut frames = 0u64;

        loop {
            if self
                .duration
                .is_some_and(|duration| started.elapsed() >= duration)
            {
                break;
            }

            match self.capture_tick() {
                Ok(acquired) => frames += acquired,
                Err(error) if error.code().0 == DXGI_ERROR_ACCESS_LOST => {
                    self.duplication = duplicate_output(&self.output, &self.device)?;
                }
                Err(error) => return Err(error).context("DXGI AcquireNextFrame"),
            }

            if last_report.elapsed() >= self.interval {
                self.sink
                    .fps(self.backend, self.monitor, frames, self.mode)?;
                frames = 0;
                last_report = Instant::now();
            }
        }

        Ok(())
    }

    fn capture_tick(&mut self) -> windows::core::Result<u64> {
        if self.latest_only {
            self.acquire_latest_batch()
        } else {
            self.acquire_one(self.timeout_ms)
                .map(|acquired| if acquired { 1 } else { 0 })
        }
    }

    fn acquire_one(&mut self, timeout_ms: u32) -> windows::core::Result<bool> {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        let result = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
        };

        match result {
            Ok(()) => {
                let copy_result = if let Some(copy_stage) = self.copy_stage.as_mut() {
                    if let Some(texture) = resource
                        .as_ref()
                        .and_then(|resource| resource.cast::<ID3D11Texture2D>().ok())
                    {
                        copy_stage.copy(&texture)
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                };

                drop(resource);
                unsafe {
                    let _ = self.duplication.ReleaseFrame();
                }
                copy_result.map_err(anyhow_to_win_error)?;
                sleep_encode_delay(self.encode_delay);
                Ok(true)
            }
            Err(error) if error.code().0 == DXGI_ERROR_WAIT_TIMEOUT => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn acquire_latest_batch(&mut self) -> windows::core::Result<u64> {
        let mut acquired = 0u64;
        let mut last_texture = None;
        let mut timeout_ms = self.timeout_ms;

        loop {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            let result = unsafe {
                self.duplication
                    .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
            };

            match result {
                Ok(()) => {
                    acquired += 1;
                    if self.copy_stage.is_some() {
                        last_texture = resource
                            .as_ref()
                            .and_then(|resource| resource.cast::<ID3D11Texture2D>().ok());
                    }
                    drop(resource);
                    unsafe {
                        let _ = self.duplication.ReleaseFrame();
                    }
                    timeout_ms = 0;
                }
                Err(error) if error.code().0 == DXGI_ERROR_WAIT_TIMEOUT => break,
                Err(error) => return Err(error),
            }
        }

        if let (Some(copy_stage), Some(texture)) = (self.copy_stage.as_mut(), last_texture.as_ref())
        {
            copy_stage.copy(texture).map_err(anyhow_to_win_error)?;
        }
        if acquired > 0 {
            sleep_encode_delay(self.encode_delay);
        }

        Ok(acquired)
    }
}

#[cfg(windows)]
fn duplicate_output(output: &IDXGIOutput, device: &ID3D11Device) -> Result<IDXGIOutputDuplication> {
    let output1: IDXGIOutput1 = output.cast()?;
    unsafe { output1.DuplicateOutput(device) }.context("DuplicateOutput")
}

#[cfg(windows)]
fn duration_from_args(duration_sec: u64) -> Option<Duration> {
    (duration_sec > 0).then(|| Duration::from_secs(duration_sec))
}

#[cfg(windows)]
fn sleep_encode_delay(delay: Duration) {
    if !delay.is_zero() {
        thread::sleep(delay);
    }
}

#[cfg(windows)]
fn anyhow_to_win_error(error: anyhow::Error) -> windows::core::Error {
    windows::core::Error::new(
        windows::core::HRESULT(0x80004005u32 as i32),
        error.to_string(),
    )
}

#[cfg(windows)]
fn get_monitor_handle(monitor_index: usize) -> Result<HMONITOR> {
    struct MonitorList(Vec<HMONITOR>);

    unsafe extern "system" fn enum_callback(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _clip: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let monitors = &mut *(data.0 as *mut MonitorList);

        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

        if GetMonitorInfoW(hmonitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            monitors.0.push(hmonitor);
        }

        BOOL(1)
    }

    let mut monitors = MonitorList(Vec::new());
    unsafe {
        EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_callback),
            LPARAM(&mut monitors as *mut _ as isize),
        )
        .ok()
        .context("EnumDisplayMonitors")?;
    }

    monitors
        .0
        .get(monitor_index)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("monitor {monitor_index} not found via GDI"))
}

#[cfg(windows)]
fn create_winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    unsafe {
        let dxgi_device: IDXGIDevice = device.cast()?;
        let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)?;
        Ok(inspectable.cast()?)
    }
}

#[cfg(windows)]
fn create_capture_item_for_monitor(hmonitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    unsafe {
        let factory: windows::core::IUnknown = RoGetActivationFactory(&HSTRING::from(
            "Windows.Graphics.Capture.GraphicsCaptureItem",
        ))?;
        let interop: IGraphicsCaptureItemInterop = factory.cast()?;
        Ok(interop.CreateForMonitor(hmonitor)?)
    }
}

#[cfg(windows)]
struct WgcProbe {
    backend: Backend,
    monitor: usize,
    mode: ProbeMode,
    timeout: Duration,
    interval: Duration,
    duration: Option<Duration>,
    latest_only: bool,
    encode_delay: Duration,
    sink: FpsSink,
    _device: ID3D11Device,
    _context: ID3D11DeviceContext,
    _item: GraphicsCaptureItem,
    frame_pool: Direct3D11CaptureFramePool,
    _session: GraphicsCaptureSession,
    copy_stage: Option<CopyStage>,
}

#[cfg(windows)]
impl WgcProbe {
    fn new(args: &Args, sink: FpsSink) -> Result<Self> {
        let hmonitor = get_monitor_handle(args.monitor)?;
        let choice = select_dxgi_output_for_hmonitor(hmonitor)
            .or_else(|_| select_dxgi_output(args.monitor))
            .context("select D3D adapter for WGC monitor")?;
        let (device, context) = create_d3d11_device(&choice.adapter)?;
        apply_gpu_priority(&device, args.gpu_priority);
        let copy_stage = args
            .allow_copy
            .then(|| CopyStage::new(&device, &context, args.ring_size, args.wait_copy_ready));

        let winrt_device = create_winrt_device(&device)?;
        let item = create_capture_item_for_monitor(hmonitor)?;
        let size = item.Size()?;
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = frame_pool.CreateCaptureSession(&item)?;

        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);

        session.StartCapture()?;

        Ok(Self {
            backend: args.backend,
            monitor: args.monitor,
            mode: ProbeMode::from_args(args),
            timeout: Duration::from_millis(args.timeout_ms as u64),
            interval: Duration::from_secs(args.interval_sec.max(1)),
            duration: duration_from_args(args.duration_sec),
            latest_only: args.latest_only,
            encode_delay: Duration::from_millis(args.simulate_encode_delay_ms),
            sink,
            _device: device,
            _context: context,
            _item: item,
            frame_pool,
            _session: session,
            copy_stage,
        })
    }

    fn run(&mut self) -> Result<()> {
        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut frames = 0u64;

        loop {
            if self
                .duration
                .is_some_and(|duration| started.elapsed() >= duration)
            {
                break;
            }

            let acquired = self.capture_tick()?;
            frames += acquired;

            pump_messages();

            if last_report.elapsed() >= self.interval {
                self.sink
                    .fps(self.backend, self.monitor, frames, self.mode)?;
                frames = 0;
                last_report = Instant::now();
            }

            if acquired == 0 {
                thread::sleep(self.timeout);
            }
        }

        Ok(())
    }

    fn capture_tick(&mut self) -> Result<u64> {
        if self.latest_only {
            self.capture_latest_only()
        } else {
            self.capture_all_available()
        }
    }

    fn capture_all_available(&mut self) -> Result<u64> {
        let mut acquired = 0u64;

        while let Ok(frame) = self.frame_pool.TryGetNextFrame() {
            if let Some(copy_stage) = self.copy_stage.as_mut() {
                let texture = frame_to_texture(&frame)?;
                copy_stage.copy(&texture)?;
            }
            drop(frame);
            acquired += 1;
            sleep_encode_delay(self.encode_delay);
        }

        Ok(acquired)
    }

    fn capture_latest_only(&mut self) -> Result<u64> {
        let mut acquired = 0u64;
        let mut last_texture = None;

        while let Ok(frame) = self.frame_pool.TryGetNextFrame() {
            if self.copy_stage.is_some() {
                last_texture = Some(frame_to_texture(&frame)?);
            }
            drop(frame);
            acquired += 1;
        }

        if let (Some(copy_stage), Some(texture)) = (self.copy_stage.as_mut(), last_texture.as_ref())
        {
            copy_stage.copy(texture)?;
        }
        if acquired > 0 {
            sleep_encode_delay(self.encode_delay);
        }

        Ok(acquired)
    }
}

#[cfg(windows)]
fn frame_to_texture(
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
) -> Result<ID3D11Texture2D> {
    unsafe {
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let dxgi_surface: IDXGISurface = access.GetInterface()?;
        Ok(dxgi_surface.cast()?)
    }
}

#[cfg(windows)]
fn pump_messages() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(windows)]
fn run() -> Result<()> {
    let args = Args::parse();
    if args.ring_size == 0 {
        anyhow::bail!("--ring-size must be at least 1");
    }
    if args.wait_copy_ready && !args.allow_copy {
        anyhow::bail!("--wait-copy-ready requires --allow-copy");
    }

    if args.spawn_worker {
        return spawn_worker_and_wait();
    }

    init_apartment();
    apply_cpu_priority(args.cpu_priority);

    let mut sink = FpsSink::new(args.service_like, args.no_stdout)?;
    sink.start(&args)?;
    match args.backend {
        Backend::Dxgi => DxgiProbe::new(&args, sink)?.run(),
        Backend::Wgc => WgcProbe::new(&args, sink)?.run(),
    }
}

#[cfg(windows)]
fn main() -> Result<()> {
    run()
}
