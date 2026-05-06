#[cfg(not(windows))]
compile_error!(
    "display_probe is Windows-only because it uses DXGI and Win32 foreground-window APIs."
);

use std::env;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::{Error, Interface, Result, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, POINT, RECT};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P2020, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_RGB_STUDIO_G2084_NONE_P2020, DXGI_COLOR_SPACE_TYPE, DXGI_MODE_ROTATION,
    DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
    DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput6,
    DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_CURSOR_STRETCHED,
    DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_FULLSCREEN,
    DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_WINDOWED,
};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
};

#[derive(Debug, Default)]
struct Args {
    monitor: Option<usize>,
    json_only: bool,
    summary_only: bool,
    watch: bool,
    json_lines: bool,
    interval_ms: u64,
    duration_sec: Option<u64>,
}

#[derive(Debug)]
struct AdapterProbe {
    index: u32,
    name: String,
    vendor_id: u32,
    device_id: u32,
    outputs: Vec<OutputProbe>,
}

#[derive(Debug)]
struct OutputProbe {
    adapter_index: u32,
    output_index: u32,
    global_index: usize,
    adapter_name: String,
    device_name: String,
    monitor_handle: isize,
    desktop_coordinates: Rect,
    attached_to_desktop: bool,
    rotation_raw: i32,
    rotation: &'static str,
    output6_available: bool,
    desc1: Option<OutputDesc1Probe>,
    hardware_composition: Option<HardwareCompositionProbe>,
    output6_error: Option<String>,
}

#[derive(Debug)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Rect {
    fn contains(&self, point: &PointProbe) -> bool {
        point.x >= self.left && point.x < self.right && point.y >= self.top && point.y < self.bottom
    }
}

#[derive(Debug)]
struct PointProbe {
    x: i32,
    y: i32,
}

#[derive(Debug)]
struct ForegroundProbe {
    hwnd: isize,
    title: String,
    process_id: u32,
    process_name: Option<String>,
    process_path: Option<String>,
    window_rect: Option<Rect>,
    window_center: Option<PointProbe>,
}

#[derive(Debug)]
struct OutputDesc1Probe {
    bits_per_color: u32,
    color_space_raw: i32,
    color_space: &'static str,
    red_primary: [f32; 2],
    green_primary: [f32; 2],
    blue_primary: [f32; 2],
    white_point: [f32; 2],
    min_luminance: f32,
    max_luminance: f32,
    max_full_frame_luminance: f32,
}

#[derive(Debug)]
struct HardwareCompositionProbe {
    raw_flags: u32,
    fullscreen: bool,
    windowed: bool,
    cursor_stretched: bool,
}

fn main() -> Result<()> {
    let args = parse_args();

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    if args.watch {
        run_watch(&args)?;
        return Ok(());
    }

    let adapters = probe_dxgi()?;
    let filtered = filter_adapters(adapters, args.monitor);

    if !args.json_only {
        print_summary(&filtered, args.monitor);
    }

    if !args.summary_only {
        if !args.json_only {
            println!();
            println!("JSON:");
        }
        println!("{}", to_json(&filtered));
    }

    Ok(())
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let mut raw = env::args().skip(1);

    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--monitor" | "--output" => {
                if let Some(value) = raw.next() {
                    args.monitor = value.parse().ok();
                }
            }
            "--json-only" => args.json_only = true,
            "--summary-only" => args.summary_only = true,
            "--watch" => args.watch = true,
            "--json-lines" => args.json_lines = true,
            "--interval-ms" => {
                if let Some(value) = raw.next() {
                    args.interval_ms = value.parse().unwrap_or(1000);
                }
            }
            "--duration-sec" => {
                if let Some(value) = raw.next() {
                    args.duration_sec = value.parse().ok();
                }
            }
            "--help" | "-h" => {
                print_help_and_exit();
            }
            _ => {
                eprintln!("Unknown argument: {arg}");
                print_help_and_exit();
            }
        }
    }

    if args.interval_ms == 0 {
        args.interval_ms = 1000;
    }

    args
}

fn print_help_and_exit() -> ! {
    println!("display_probe - DXGI output and hardware composition probe");
    println!();
    println!("Usage:");
    println!("  display_probe [--monitor N] [--json-only|--summary-only]");
    println!("  display_probe --watch [--interval-ms N] [--duration-sec N] [--json-lines]");
    println!();
    println!("Options:");
    println!("  --monitor N      Probe only global output/monitor index N");
    println!("  --output N       Alias for --monitor");
    println!("  --json-only      Print only JSON");
    println!("  --summary-only   Print only human-readable summary");
    println!("  --watch          Poll foreground window + DXGI output state");
    println!("  --interval-ms N  Watch interval in milliseconds (default: 1000)");
    println!("  --duration-sec N Stop watch mode after N seconds");
    println!("  --json-lines     In watch mode, print one JSON object per line");
    std::process::exit(0);
}

fn probe_dxgi() -> Result<Vec<AdapterProbe>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut adapters = Vec::new();
    let mut global_output_index = 0usize;
    let mut adapter_index = 0u32;

    loop {
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(_) => break,
        };

        let probe = probe_adapter(adapter_index, &adapter, &mut global_output_index)?;
        adapters.push(probe);
        adapter_index += 1;
    }

    Ok(adapters)
}

fn probe_adapter(
    adapter_index: u32,
    adapter: &IDXGIAdapter1,
    global_output_index: &mut usize,
) -> Result<AdapterProbe> {
    let desc = unsafe { adapter.GetDesc1()? };
    let adapter_name = wide_to_string(&desc.Description);

    let mut outputs = Vec::new();
    let mut output_index = 0u32;

    loop {
        let output = match unsafe { adapter.EnumOutputs(output_index) } {
            Ok(output) => output,
            Err(_) => break,
        };

        outputs.push(probe_output(
            adapter_index,
            output_index,
            *global_output_index,
            &adapter_name,
            &output,
        )?);
        *global_output_index += 1;
        output_index += 1;
    }

    Ok(AdapterProbe {
        index: adapter_index,
        name: adapter_name,
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
        outputs,
    })
}

fn probe_output(
    adapter_index: u32,
    output_index: u32,
    global_index: usize,
    adapter_name: &str,
    output: &IDXGIOutput,
) -> Result<OutputProbe> {
    let desc = unsafe { output.GetDesc()? };
    let device_name = wide_to_string(&desc.DeviceName);
    let desktop_coordinates = Rect {
        left: desc.DesktopCoordinates.left,
        top: desc.DesktopCoordinates.top,
        right: desc.DesktopCoordinates.right,
        bottom: desc.DesktopCoordinates.bottom,
    };

    let mut desc1_probe = None;
    let mut hardware_composition = None;
    let mut output6_available = false;
    let mut output6_error = None;

    match output.cast::<IDXGIOutput6>() {
        Ok(output6) => {
            output6_available = true;

            match unsafe { output6.GetDesc1() } {
                Ok(desc1) => {
                    desc1_probe = Some(OutputDesc1Probe {
                        bits_per_color: desc1.BitsPerColor,
                        color_space_raw: desc1.ColorSpace.0,
                        color_space: color_space_name(desc1.ColorSpace),
                        red_primary: desc1.RedPrimary,
                        green_primary: desc1.GreenPrimary,
                        blue_primary: desc1.BluePrimary,
                        white_point: desc1.WhitePoint,
                        min_luminance: desc1.MinLuminance,
                        max_luminance: desc1.MaxLuminance,
                        max_full_frame_luminance: desc1.MaxFullFrameLuminance,
                    });
                }
                Err(err) => output6_error = Some(format_windows_error("GetDesc1", &err)),
            }

            match unsafe { output6.CheckHardwareCompositionSupport() } {
                Ok(raw_flags) => {
                    hardware_composition = Some(HardwareCompositionProbe {
                        raw_flags,
                        fullscreen: has_flag(
                            raw_flags,
                            DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_FULLSCREEN.0 as u32,
                        ),
                        windowed: has_flag(
                            raw_flags,
                            DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_WINDOWED.0 as u32,
                        ),
                        cursor_stretched: has_flag(
                            raw_flags,
                            DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_CURSOR_STRETCHED.0 as u32,
                        ),
                    });
                }
                Err(err) => {
                    output6_error = Some(format_windows_error(
                        "CheckHardwareCompositionSupport",
                        &err,
                    ))
                }
            }
        }
        Err(err) => {
            output6_error = Some(format_windows_error("QueryInterface(IDXGIOutput6)", &err));
        }
    }

    Ok(OutputProbe {
        adapter_index,
        output_index,
        global_index,
        adapter_name: adapter_name.to_string(),
        device_name,
        monitor_handle: desc.Monitor.0 as isize,
        desktop_coordinates,
        attached_to_desktop: desc.AttachedToDesktop.0 != 0,
        rotation_raw: desc.Rotation.0,
        rotation: rotation_name(desc.Rotation),
        output6_available,
        desc1: desc1_probe,
        hardware_composition,
        output6_error,
    })
}

fn filter_adapters(adapters: Vec<AdapterProbe>, monitor: Option<usize>) -> Vec<AdapterProbe> {
    let Some(monitor) = monitor else {
        return adapters;
    };

    adapters
        .into_iter()
        .filter_map(|mut adapter| {
            adapter
                .outputs
                .retain(|output| output.global_index == monitor);
            if adapter.outputs.is_empty() {
                None
            } else {
                Some(adapter)
            }
        })
        .collect()
}

fn print_summary(adapters: &[AdapterProbe], monitor: Option<usize>) {
    println!("display_probe: DXGI outputs and hardware composition support");
    if let Some(monitor) = monitor {
        println!("selected monitor: {monitor}");
    }

    if adapters.iter().all(|adapter| adapter.outputs.is_empty()) {
        println!("No outputs found.");
        return;
    }

    for adapter in adapters {
        println!();
        println!(
            "Adapter #{}: {} (vendor=0x{:04x}, device=0x{:04x})",
            adapter.index, adapter.name, adapter.vendor_id, adapter.device_id
        );

        for output in &adapter.outputs {
            let rect = &output.desktop_coordinates;
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;

            println!(
                "  Output #{} / global #{}: {}",
                output.output_index, output.global_index, output.device_name
            );
            println!(
                "    desktop: left={} top={} right={} bottom={} ({}x{}), attached_to_desktop={}, rotation={}",
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                width,
                height,
                output.attached_to_desktop,
                output.rotation
            );
            println!("    IDXGIOutput6: {}", output.output6_available);

            if let Some(desc1) = &output.desc1 {
                println!(
                    "    desc1: bits_per_color={}, color_space={} ({})",
                    desc1.bits_per_color, desc1.color_space, desc1.color_space_raw
                );
                println!(
                    "    primaries: R=({:.6}, {:.6}) G=({:.6}, {:.6}) B=({:.6}, {:.6}) W=({:.6}, {:.6})",
                    desc1.red_primary[0],
                    desc1.red_primary[1],
                    desc1.green_primary[0],
                    desc1.green_primary[1],
                    desc1.blue_primary[0],
                    desc1.blue_primary[1],
                    desc1.white_point[0],
                    desc1.white_point[1],
                );
            }

            if let Some(composition) = &output.hardware_composition {
                println!(
                    "    hardware_composition: raw_flags=0x{:08x} fullscreen={} windowed={} cursor_stretched={}",
                    composition.raw_flags,
                    composition.fullscreen,
                    composition.windowed,
                    composition.cursor_stretched
                );
            }

            if let Some(error) = &output.output6_error {
                println!("    output6_note: {error}");
            }
        }
    }
}

fn run_watch(args: &Args) -> Result<()> {
    let interval = Duration::from_millis(args.interval_ms.max(1));
    let deadline = args
        .duration_sec
        .map(|duration| Instant::now() + Duration::from_secs(duration));

    if !args.json_lines {
        println!(
            "display_probe watch: interval={}ms duration={}",
            args.interval_ms,
            args.duration_sec
                .map(|value| format!("{value}s"))
                .unwrap_or_else(|| "until Ctrl+C".to_string())
        );
    }

    loop {
        let timestamp = timestamp_local();
        let timestamp_unix_ms = timestamp_unix_ms();
        let foreground = probe_foreground_window();
        let adapters = probe_dxgi()?;
        let output = foreground
            .window_center
            .as_ref()
            .and_then(|point| find_output_for_point(&adapters, point));

        if args.json_lines {
            println!(
                "{}",
                watch_sample_json(&timestamp, timestamp_unix_ms, &foreground, output)
            );
        } else {
            print_watch_sample(&timestamp, timestamp_unix_ms, &foreground, output);
        }

        let _ = io::stdout().flush();

        if let Some(deadline) = deadline {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            thread::sleep((deadline - now).min(interval));
        } else {
            thread::sleep(interval);
        }
    }

    Ok(())
}

fn probe_foreground_window() -> ForegroundProbe {
    let hwnd = unsafe { GetForegroundWindow() };
    let hwnd_raw = hwnd.0 as isize;

    if hwnd.0.is_null() {
        return ForegroundProbe {
            hwnd: 0,
            title: String::new(),
            process_id: 0,
            process_name: None,
            process_path: None,
            window_rect: None,
            window_center: None,
        };
    }

    let mut title_buf = [0u16; 512];
    let title_len = unsafe { GetWindowTextW(hwnd, &mut title_buf) }.max(0) as usize;
    let title = String::from_utf16_lossy(&title_buf[..title_len]);

    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }

    let mut rect = RECT::default();
    let window_rect = unsafe { GetWindowRect(hwnd, &mut rect) }
        .ok()
        .map(|_| Rect {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        });

    let window_center = window_rect.as_ref().and_then(|rect| {
        if rect.right > rect.left && rect.bottom > rect.top {
            Some(PointProbe {
                x: rect.left + (rect.right - rect.left) / 2,
                y: rect.top + (rect.bottom - rect.top) / 2,
            })
        } else {
            None
        }
    });

    let (process_path, process_name) = process_image(pid);

    ForegroundProbe {
        hwnd: hwnd_raw,
        title,
        process_id: pid,
        process_name,
        process_path,
        window_rect,
        window_center,
    }
}

fn process_image(pid: u32) -> (Option<String>, Option<String>) {
    if pid == 0 {
        return (None, None);
    }

    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, BOOL(0), pid) } {
        Ok(handle) => handle,
        Err(_) => return (None, None),
    };

    let mut buffer = vec![0u16; 32768];
    let mut size = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    let _ = unsafe { CloseHandle(handle) };

    if result.is_err() || size == 0 {
        return (None, None);
    }

    let path = String::from_utf16_lossy(&buffer[..size as usize]);
    let name = path
        .rsplit(['\\', '/'])
        .next()
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());

    (Some(path), name)
}

fn find_output_for_point<'a>(
    adapters: &'a [AdapterProbe],
    point: &PointProbe,
) -> Option<&'a OutputProbe> {
    let hmonitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: point.x,
                y: point.y,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
    let monitor_handle = hmonitor.0 as isize;

    adapters
        .iter()
        .flat_map(|adapter| adapter.outputs.iter())
        .find(|output| output.monitor_handle == monitor_handle)
        .or_else(|| {
            adapters
                .iter()
                .flat_map(|adapter| adapter.outputs.iter())
                .find(|output| output.desktop_coordinates.contains(point))
        })
}

fn print_watch_sample(
    timestamp: &str,
    timestamp_unix_ms: u128,
    foreground: &ForegroundProbe,
    output: Option<&OutputProbe>,
) {
    println!();
    println!("{timestamp} ({timestamp_unix_ms} ms)");
    println!(
        "  foreground: hwnd=0x{:x} pid={} process={} title=\"{}\"",
        foreground.hwnd,
        foreground.process_id,
        foreground.process_name.as_deref().unwrap_or("<unknown>"),
        foreground.title
    );

    if let Some(rect) = &foreground.window_rect {
        println!(
            "  foreground_rect: left={} top={} right={} bottom={}",
            rect.left, rect.top, rect.right, rect.bottom
        );
    }

    if let Some(center) = &foreground.window_center {
        println!("  foreground_center: x={} y={}", center.x, center.y);
    }

    match output {
        Some(output) => {
            let rect = &output.desktop_coordinates;
            println!(
                "  monitor: global #{} adapter=\"{}\" output=\"{}\"",
                output.global_index, output.adapter_name, output.device_name
            );
            println!(
                "  desktop_rect: left={} top={} right={} bottom={}",
                rect.left, rect.top, rect.right, rect.bottom
            );

            if let Some(desc1) = &output.desc1 {
                println!(
                    "  desc1: bits_per_color={} color_space={} ({}) primaries R=({:.6},{:.6}) G=({:.6},{:.6}) B=({:.6},{:.6}) W=({:.6},{:.6})",
                    desc1.bits_per_color,
                    desc1.color_space,
                    desc1.color_space_raw,
                    desc1.red_primary[0],
                    desc1.red_primary[1],
                    desc1.green_primary[0],
                    desc1.green_primary[1],
                    desc1.blue_primary[0],
                    desc1.blue_primary[1],
                    desc1.white_point[0],
                    desc1.white_point[1],
                );
            } else {
                println!("  desc1: unavailable");
            }

            if let Some(composition) = &output.hardware_composition {
                println!(
                    "  hardware_composition: raw_flags=0x{:08x} fullscreen={} windowed={} cursor_stretched={}",
                    composition.raw_flags,
                    composition.fullscreen,
                    composition.windowed,
                    composition.cursor_stretched
                );
            } else {
                println!("  hardware_composition: unavailable");
            }
        }
        None => println!("  monitor: <no DXGI output matched foreground center>"),
    }
}

fn watch_sample_json(
    timestamp: &str,
    timestamp_unix_ms: u128,
    foreground: &ForegroundProbe,
    output: Option<&OutputProbe>,
) -> String {
    let mut json = String::new();
    write!(
        &mut json,
        "{{\"timestamp\":\"{}\",\"timestamp_unix_ms\":{},\"foreground\":",
        json_escape(timestamp),
        timestamp_unix_ms
    )
    .unwrap();
    write_foreground_json_compact(&mut json, foreground);
    json.push_str(",\"monitor\":");
    match output {
        Some(output) => write_monitor_json_compact(&mut json, output),
        None => json.push_str("null"),
    }
    json.push_str(",\"output\":");
    match output {
        Some(output) => write_output_json_compact(&mut json, output),
        None => json.push_str("null"),
    }
    json.push('}');
    json
}

fn write_monitor_json_compact(json: &mut String, output: &OutputProbe) {
    write!(
        json,
        "{{\"global_index\":{},\"adapter_name\":\"{}\",\"output_index\":{},\"output_name\":\"{}\",\"desktop_coordinates\":",
        output.global_index,
        json_escape(&output.adapter_name),
        output.output_index,
        json_escape(&output.device_name)
    )
    .unwrap();
    write_rect_json_compact(json, &output.desktop_coordinates);
    json.push('}');
}

fn write_foreground_json_compact(json: &mut String, foreground: &ForegroundProbe) {
    write!(
        json,
        "{{\"hwnd\":\"0x{:x}\",\"title\":\"{}\",\"process_id\":{},\"process_name\":",
        foreground.hwnd,
        json_escape(&foreground.title),
        foreground.process_id
    )
    .unwrap();
    write_json_string_or_null(json, foreground.process_name.as_deref());
    json.push_str(",\"process_path\":");
    write_json_string_or_null(json, foreground.process_path.as_deref());
    json.push_str(",\"window_rect\":");
    match &foreground.window_rect {
        Some(rect) => write_rect_json_compact(json, rect),
        None => json.push_str("null"),
    }
    json.push_str(",\"window_center\":");
    match &foreground.window_center {
        Some(point) => write!(json, "{{\"x\":{},\"y\":{}}}", point.x, point.y).unwrap(),
        None => json.push_str("null"),
    }
    json.push('}');
}

fn write_output_json_compact(json: &mut String, output: &OutputProbe) {
    write!(
        json,
        "{{\"adapter_index\":{},\"output_index\":{},\"global_index\":{},\"adapter_name\":\"{}\",\"device_name\":\"{}\",\"monitor_handle\":\"0x{:x}\",\"desktop_coordinates\":",
        output.adapter_index,
        output.output_index,
        output.global_index,
        json_escape(&output.adapter_name),
        json_escape(&output.device_name),
        output.monitor_handle
    )
    .unwrap();
    write_rect_json_compact(json, &output.desktop_coordinates);
    write!(
        json,
        ",\"attached_to_desktop\":{},\"rotation\":{{\"raw\":{},\"name\":\"{}\"}},\"idxgi_output6_available\":{},\"desc1\":",
        output.attached_to_desktop,
        output.rotation_raw,
        output.rotation,
        output.output6_available
    )
    .unwrap();

    match &output.desc1 {
        Some(desc1) => write_desc1_json_compact(json, desc1),
        None => json.push_str("null"),
    }

    json.push_str(",\"hardware_composition\":");
    match &output.hardware_composition {
        Some(composition) => write_hardware_composition_json_compact(json, composition),
        None => json.push_str("null"),
    }

    json.push_str(",\"output6_error\":");
    write_json_string_or_null(json, output.output6_error.as_deref());
    json.push('}');
}

fn write_desc1_json_compact(json: &mut String, desc1: &OutputDesc1Probe) {
    write!(
        json,
        "{{\"bits_per_color\":{},\"color_space\":{{\"raw\":{},\"name\":\"{}\"}},\"red_primary\":[{:.9},{:.9}],\"green_primary\":[{:.9},{:.9}],\"blue_primary\":[{:.9},{:.9}],\"white_point\":[{:.9},{:.9}],\"min_luminance\":{:.9},\"max_luminance\":{:.9},\"max_full_frame_luminance\":{:.9}}}",
        desc1.bits_per_color,
        desc1.color_space_raw,
        desc1.color_space,
        desc1.red_primary[0],
        desc1.red_primary[1],
        desc1.green_primary[0],
        desc1.green_primary[1],
        desc1.blue_primary[0],
        desc1.blue_primary[1],
        desc1.white_point[0],
        desc1.white_point[1],
        desc1.min_luminance,
        desc1.max_luminance,
        desc1.max_full_frame_luminance
    )
    .unwrap();
}

fn write_hardware_composition_json_compact(
    json: &mut String,
    composition: &HardwareCompositionProbe,
) {
    write!(
        json,
        "{{\"raw_flags\":{},\"raw_flags_hex\":\"0x{:08x}\",\"DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_FULLSCREEN\":{},\"DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_WINDOWED\":{},\"DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_CURSOR_STRETCHED\":{}}}",
        composition.raw_flags,
        composition.raw_flags,
        composition.fullscreen,
        composition.windowed,
        composition.cursor_stretched
    )
    .unwrap();
}

fn write_rect_json_compact(json: &mut String, rect: &Rect) {
    write!(
        json,
        "{{\"left\":{},\"top\":{},\"right\":{},\"bottom\":{}}}",
        rect.left, rect.top, rect.right, rect.bottom
    )
    .unwrap();
}

fn write_json_string_or_null(json: &mut String, value: Option<&str>) {
    match value {
        Some(value) => write!(json, "\"{}\"", json_escape(value)).unwrap(),
        None => json.push_str("null"),
    }
}

fn timestamp_local() -> String {
    let time = unsafe { GetLocalTime() };
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

fn timestamp_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn to_json(adapters: &[AdapterProbe]) -> String {
    let mut json = String::new();
    writeln!(&mut json, "{{").unwrap();
    writeln!(&mut json, "  \"adapters\": [").unwrap();

    for (adapter_pos, adapter) in adapters.iter().enumerate() {
        writeln!(&mut json, "    {{").unwrap();
        writeln!(&mut json, "      \"index\": {},", adapter.index).unwrap();
        writeln!(
            &mut json,
            "      \"name\": \"{}\",",
            json_escape(&adapter.name)
        )
        .unwrap();
        writeln!(&mut json, "      \"vendor_id\": {},", adapter.vendor_id).unwrap();
        writeln!(&mut json, "      \"device_id\": {},", adapter.device_id).unwrap();
        writeln!(&mut json, "      \"outputs\": [").unwrap();

        for (output_pos, output) in adapter.outputs.iter().enumerate() {
            write_output_json(&mut json, output, "        ");
            if output_pos + 1 != adapter.outputs.len() {
                writeln!(&mut json, ",").unwrap();
            } else {
                writeln!(&mut json).unwrap();
            }
        }

        writeln!(&mut json, "      ]").unwrap();
        if adapter_pos + 1 != adapters.len() {
            writeln!(&mut json, "    }},").unwrap();
        } else {
            writeln!(&mut json, "    }}").unwrap();
        }
    }

    writeln!(&mut json, "  ]").unwrap();
    write!(&mut json, "}}").unwrap();
    json
}

fn write_output_json(json: &mut String, output: &OutputProbe, indent: &str) {
    let rect = &output.desktop_coordinates;
    writeln!(json, "{indent}{{").unwrap();
    writeln!(
        json,
        "{indent}  \"adapter_index\": {},",
        output.adapter_index
    )
    .unwrap();
    writeln!(json, "{indent}  \"output_index\": {},", output.output_index).unwrap();
    writeln!(json, "{indent}  \"global_index\": {},", output.global_index).unwrap();
    writeln!(
        json,
        "{indent}  \"adapter_name\": \"{}\",",
        json_escape(&output.adapter_name)
    )
    .unwrap();
    writeln!(
        json,
        "{indent}  \"device_name\": \"{}\",",
        json_escape(&output.device_name)
    )
    .unwrap();
    writeln!(
        json,
        "{indent}  \"monitor_handle\": \"0x{:x}\",",
        output.monitor_handle
    )
    .unwrap();
    writeln!(
        json,
        "{indent}  \"desktop_coordinates\": {{ \"left\": {}, \"top\": {}, \"right\": {}, \"bottom\": {} }},",
        rect.left, rect.top, rect.right, rect.bottom
    )
    .unwrap();
    writeln!(
        json,
        "{indent}  \"attached_to_desktop\": {},",
        output.attached_to_desktop
    )
    .unwrap();
    writeln!(
        json,
        "{indent}  \"rotation\": {{ \"raw\": {}, \"name\": \"{}\" }},",
        output.rotation_raw, output.rotation
    )
    .unwrap();
    writeln!(
        json,
        "{indent}  \"idxgi_output6_available\": {},",
        output.output6_available
    )
    .unwrap();

    match &output.desc1 {
        Some(desc1) => write_desc1_json(json, desc1, indent),
        None => writeln!(json, "{indent}  \"desc1\": null,").unwrap(),
    }

    match &output.hardware_composition {
        Some(composition) => write_hardware_composition_json(json, composition, indent),
        None => writeln!(json, "{indent}  \"hardware_composition\": null,").unwrap(),
    }

    match &output.output6_error {
        Some(error) => writeln!(
            json,
            "{indent}  \"output6_error\": \"{}\"",
            json_escape(error)
        )
        .unwrap(),
        None => writeln!(json, "{indent}  \"output6_error\": null").unwrap(),
    }

    write!(json, "{indent}}}").unwrap();
}

fn write_desc1_json(json: &mut String, desc1: &OutputDesc1Probe, indent: &str) {
    writeln!(json, "{indent}  \"desc1\": {{").unwrap();
    writeln!(
        json,
        "{indent}    \"bits_per_color\": {},",
        desc1.bits_per_color
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"color_space\": {{ \"raw\": {}, \"name\": \"{}\" }},",
        desc1.color_space_raw, desc1.color_space
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"red_primary\": [{:.9}, {:.9}],",
        desc1.red_primary[0], desc1.red_primary[1]
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"green_primary\": [{:.9}, {:.9}],",
        desc1.green_primary[0], desc1.green_primary[1]
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"blue_primary\": [{:.9}, {:.9}],",
        desc1.blue_primary[0], desc1.blue_primary[1]
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"white_point\": [{:.9}, {:.9}],",
        desc1.white_point[0], desc1.white_point[1]
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"min_luminance\": {:.9},",
        desc1.min_luminance
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"max_luminance\": {:.9},",
        desc1.max_luminance
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"max_full_frame_luminance\": {:.9}",
        desc1.max_full_frame_luminance
    )
    .unwrap();
    writeln!(json, "{indent}  }},").unwrap();
}

fn write_hardware_composition_json(
    json: &mut String,
    composition: &HardwareCompositionProbe,
    indent: &str,
) {
    writeln!(json, "{indent}  \"hardware_composition\": {{").unwrap();
    writeln!(
        json,
        "{indent}    \"raw_flags\": {},",
        composition.raw_flags
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"raw_flags_hex\": \"0x{:08x}\",",
        composition.raw_flags
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_FULLSCREEN\": {},",
        composition.fullscreen
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_WINDOWED\": {},",
        composition.windowed
    )
    .unwrap();
    writeln!(
        json,
        "{indent}    \"DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_CURSOR_STRETCHED\": {}",
        composition.cursor_stretched
    )
    .unwrap();
    writeln!(json, "{indent}  }},").unwrap();
}

fn has_flag(raw_flags: u32, flag: u32) -> bool {
    raw_flags & flag == flag
}

fn wide_to_string(wide: &[u16]) -> String {
    let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..len])
}

fn format_windows_error(context: &str, err: &Error) -> String {
    format!("{context} failed: HRESULT 0x{:08x}", err.code().0 as u32)
}

fn rotation_name(rotation: DXGI_MODE_ROTATION) -> &'static str {
    match rotation {
        DXGI_MODE_ROTATION_UNSPECIFIED => "UNSPECIFIED",
        DXGI_MODE_ROTATION_IDENTITY => "IDENTITY",
        DXGI_MODE_ROTATION_ROTATE90 => "ROTATE90",
        DXGI_MODE_ROTATION_ROTATE180 => "ROTATE180",
        DXGI_MODE_ROTATION_ROTATE270 => "ROTATE270",
        _ => "UNKNOWN",
    }
}

fn color_space_name(color_space: DXGI_COLOR_SPACE_TYPE) -> &'static str {
    match color_space {
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 => "RGB_FULL_G22_NONE_P709",
        DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709 => "RGB_FULL_G10_NONE_P709",
        DXGI_COLOR_SPACE_RGB_STUDIO_G2084_NONE_P2020 => "RGB_STUDIO_G2084_NONE_P2020",
        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 => "RGB_FULL_G2084_NONE_P2020",
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P2020 => "RGB_FULL_G22_NONE_P2020",
        _ => "UNKNOWN",
    }
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => {
                write!(&mut escaped, "\\u{:04x}", c as u32).unwrap();
            }
            c => escaped.push(c),
        }
    }
    escaped
}
