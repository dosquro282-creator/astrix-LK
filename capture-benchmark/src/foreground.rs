use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITORINFOEXW,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, GWL_EXSTYLE, GWL_STYLE,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl ScreenRect {
    pub fn width(self) -> i32 {
        self.right - self.left
    }

    pub fn height(self) -> i32 {
        self.bottom - self.top
    }

    pub fn area(self) -> i64 {
        self.width().max(0) as i64 * self.height().max(0) as i64
    }

    pub fn intersects(self, other: Self) -> bool {
        self.left < other.right
            && self.right > other.left
            && self.top < other.bottom
            && self.bottom > other.top
    }

    pub fn covers_with_tolerance(self, other: Self, tolerance_px: i32) -> bool {
        self.left <= other.left + tolerance_px
            && self.top <= other.top + tolerance_px
            && self.right >= other.right - tolerance_px
            && self.bottom >= other.bottom - tolerance_px
    }

    pub fn from_tuple(rect: (i32, i32, i32, i32)) -> Self {
        Self {
            left: rect.0,
            top: rect.1,
            right: rect.2,
            bottom: rect.3,
        }
    }
}

impl From<RECT> for ScreenRect {
    fn from(rect: RECT) -> Self {
        Self {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ForegroundSnapshot {
    pub hwnd: HWND,
    pub pid: u32,
    pub exe_name: String,
    pub title: String,
    pub window_rect: ScreenRect,
    pub client_rect: ScreenRect,
    pub monitor_name: String,
    pub monitor_rect: ScreenRect,
    pub intersects_captured_monitor: bool,
    pub covers_captured_monitor: bool,
    pub style: isize,
    pub exstyle: isize,
    pub foreground_on_captured_monitor: bool,
    pub foreground_covers_captured_monitor: bool,
    pub foreground_fullscreen_like: bool,
    pub foreground_exe_changed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ForegroundSummary {
    pub percent_time_foreground_on_captured_monitor: f64,
    pub long_gaps_while_game_foreground: usize,
    pub long_gaps_while_other_foreground: usize,
    pub foreground_exe_most_common: String,
    pub foreground_title_most_common: String,
    pub buckets: Vec<ForegroundBucketSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum ForegroundBucket {
    GameForegroundOnCapturedMonitor,
    OtherForegroundOnOtherMonitor,
    TaskSwitcherExplorer,
    Other,
}

impl ForegroundBucket {
    pub const ALL: [Self; 4] = [
        Self::GameForegroundOnCapturedMonitor,
        Self::OtherForegroundOnOtherMonitor,
        Self::TaskSwitcherExplorer,
        Self::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::GameForegroundOnCapturedMonitor => "game_foreground_on_captured_monitor",
            Self::OtherForegroundOnOtherMonitor => "other_foreground_on_other_monitor",
            Self::TaskSwitcherExplorer => "task_switcher/explorer",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ForegroundBucketSummary {
    pub bucket: String,
    pub duration_ms: u64,
    pub successful_frames: usize,
    pub fps: f64,
    pub longest_gap_ms: f64,
    pub long_gap_count: usize,
    pub p95_gap_ms: f64,
    pub p99_gap_ms: f64,
}

pub struct ForegroundTracker {
    captured_monitor_rect: ScreenRect,
    last_sample_time: Instant,
    last_snapshot: Option<ForegroundSnapshot>,
    on_captured_monitor_us: u128,
    observed_us: u128,
    exe_time_us: HashMap<String, u128>,
    title_time_us: HashMap<String, u128>,
    long_gaps_while_game_foreground: usize,
    long_gaps_while_other_foreground: usize,
    bucket_duration_us: HashMap<ForegroundBucket, u128>,
    bucket_successful_frames: HashMap<ForegroundBucket, usize>,
    bucket_gaps_ms: HashMap<ForegroundBucket, Vec<f64>>,
}

impl ForegroundTracker {
    pub fn new(captured_monitor_rect: ScreenRect) -> Self {
        Self {
            captured_monitor_rect,
            last_sample_time: Instant::now(),
            last_snapshot: None,
            on_captured_monitor_us: 0,
            observed_us: 0,
            exe_time_us: HashMap::new(),
            title_time_us: HashMap::new(),
            long_gaps_while_game_foreground: 0,
            long_gaps_while_other_foreground: 0,
            bucket_duration_us: HashMap::new(),
            bucket_successful_frames: HashMap::new(),
            bucket_gaps_ms: HashMap::new(),
        }
    }

    pub fn sample(&mut self) -> Option<ForegroundSnapshot> {
        let now = Instant::now();
        let elapsed_us = now.duration_since(self.last_sample_time).as_micros();
        if let Some(previous) = &self.last_snapshot {
            self.observed_us = self.observed_us.saturating_add(elapsed_us);
            if previous.foreground_on_captured_monitor {
                self.on_captured_monitor_us =
                    self.on_captured_monitor_us.saturating_add(elapsed_us);
            }
            *self
                .exe_time_us
                .entry(previous.exe_name.clone())
                .or_default() += elapsed_us;
            *self
                .title_time_us
                .entry(previous.title.clone())
                .or_default() += elapsed_us;
            *self
                .bucket_duration_us
                .entry(classify_snapshot(previous))
                .or_default() += elapsed_us;
        }
        self.last_sample_time = now;

        let previous_exe = self.last_snapshot.as_ref().map(|s| s.exe_name.clone());
        let snapshot = query_foreground(self.captured_monitor_rect, previous_exe.as_deref());
        self.last_snapshot = snapshot.clone();
        snapshot
    }

    pub fn log_startup(&mut self) {
        if let Some(snapshot) = self.sample() {
            log_snapshot("startup", &snapshot);
        } else {
            println!("[foreground][startup] hwnd=0 unavailable=true");
        }
    }

    pub fn log_periodic(&mut self, frame_index: usize) {
        if let Some(snapshot) = self.sample() {
            println!("[foreground][summary] frame_index={}", frame_index);
            log_snapshot("summary", &snapshot);
        } else {
            println!(
                "[foreground][summary] frame_index={} hwnd=0 unavailable=true",
                frame_index
            );
        }
    }

    pub fn log_long_gap(&mut self, gap_ms: f64, frame_index: usize) {
        if let Some(snapshot) = self.sample() {
            let bucket = classify_snapshot(&snapshot);
            if bucket == ForegroundBucket::GameForegroundOnCapturedMonitor {
                self.long_gaps_while_game_foreground += 1;
            } else {
                self.long_gaps_while_other_foreground += 1;
            }
            println!(
                "[foreground][long_gap] frame_index={} gap_ms={:.2}",
                frame_index, gap_ms
            );
            log_snapshot("long_gap", &snapshot);
        } else {
            self.long_gaps_while_other_foreground += 1;
            println!(
                "[foreground][long_gap] frame_index={} gap_ms={:.2} hwnd=0 unavailable=true",
                frame_index, gap_ms
            );
        }
    }

    pub fn current_bucket(&self) -> ForegroundBucket {
        self.last_snapshot
            .as_ref()
            .map(classify_snapshot)
            .unwrap_or(ForegroundBucket::Other)
    }

    pub fn current_bucket_name(&self) -> &'static str {
        self.current_bucket().as_str()
    }

    pub fn record_successful_frame(&mut self, bucket: ForegroundBucket) {
        *self.bucket_successful_frames.entry(bucket).or_default() += 1;
    }

    pub fn record_gap(&mut self, bucket: ForegroundBucket, gap_ms: f64) {
        self.bucket_gaps_ms.entry(bucket).or_default().push(gap_ms);
    }

    pub fn summary(&mut self) -> ForegroundSummary {
        let _ = self.sample();
        let percent_time_foreground_on_captured_monitor = if self.observed_us > 0 {
            (self.on_captured_monitor_us as f64 / self.observed_us as f64) * 100.0
        } else {
            0.0
        };

        let buckets = ForegroundBucket::ALL
            .iter()
            .map(|bucket| {
                let duration_us = self.bucket_duration_us.get(bucket).copied().unwrap_or(0);
                let duration_ms = (duration_us / 1000) as u64;
                let successful_frames = self
                    .bucket_successful_frames
                    .get(bucket)
                    .copied()
                    .unwrap_or(0);
                let fps = if duration_us > 0 {
                    successful_frames as f64 / (duration_us as f64 / 1_000_000.0)
                } else {
                    0.0
                };
                let gaps = self
                    .bucket_gaps_ms
                    .get(bucket)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let longest_gap_ms = gaps.iter().copied().fold(0.0, f64::max);
                let p95_gap_ms = percentile_f64(gaps, 0.95);
                let p99_gap_ms = percentile_f64(gaps, 0.99);
                ForegroundBucketSummary {
                    bucket: bucket.as_str().to_string(),
                    duration_ms,
                    successful_frames,
                    fps,
                    longest_gap_ms,
                    long_gap_count: gaps.iter().filter(|gap| **gap > 100.0).count(),
                    p95_gap_ms,
                    p99_gap_ms,
                }
            })
            .collect();

        ForegroundSummary {
            percent_time_foreground_on_captured_monitor,
            long_gaps_while_game_foreground: self.long_gaps_while_game_foreground,
            long_gaps_while_other_foreground: self.long_gaps_while_other_foreground,
            foreground_exe_most_common: most_common(&self.exe_time_us),
            foreground_title_most_common: most_common(&self.title_time_us),
            buckets,
        }
    }
}

pub fn current_foreground_hwnd() -> HWND {
    unsafe { GetForegroundWindow() }
}

fn query_foreground(
    captured_monitor_rect: ScreenRect,
    previous_exe: Option<&str>,
) -> Option<ForegroundSnapshot> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));

        let exe_name = process_exe_name(pid);
        let title = window_title(hwnd);

        let mut window_rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut window_rect);

        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);

        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let (monitor_name, monitor_rect) = monitor_info(monitor);
        let window_rect = ScreenRect::from(window_rect);
        let client_rect = ScreenRect::from(client_rect);
        let intersects_captured_monitor = window_rect.intersects(captured_monitor_rect);
        let covers_captured_monitor = window_rect.covers_with_tolerance(captured_monitor_rect, 2);
        let monitor_matches_capture = monitor_rect.area() > 0
            && monitor_rect.covers_with_tolerance(captured_monitor_rect, 2)
            && captured_monitor_rect.covers_with_tolerance(monitor_rect, 2);

        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let foreground_on_captured_monitor = monitor_matches_capture || intersects_captured_monitor;
        let foreground_covers_captured_monitor = covers_captured_monitor;
        let foreground_fullscreen_like = foreground_on_captured_monitor
            && foreground_covers_captured_monitor
            && window_rect.width() >= captured_monitor_rect.width() - 2
            && window_rect.height() >= captured_monitor_rect.height() - 2;
        let foreground_exe_changed = previous_exe
            .map(|prev| !prev.eq_ignore_ascii_case(&exe_name))
            .unwrap_or(false);

        Some(ForegroundSnapshot {
            hwnd,
            pid,
            exe_name,
            title,
            window_rect,
            client_rect,
            monitor_name,
            monitor_rect,
            intersects_captured_monitor,
            covers_captured_monitor,
            style,
            exstyle,
            foreground_on_captured_monitor,
            foreground_covers_captured_monitor,
            foreground_fullscreen_like,
            foreground_exe_changed,
        })
    }
}

fn process_exe_name(pid: u32) -> String {
    if pid == 0 {
        return String::new();
    }

    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => handle,
            Err(_) => return format!("pid:{}", pid),
        };

        let mut buffer = vec![0u16; 32768];
        let mut size = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);

        if result.is_ok() && size > 0 {
            let full_path = String::from_utf16_lossy(&buffer[..size as usize]);
            Path::new(&full_path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&full_path)
                .to_string()
        } else {
            format!("pid:{}", pid)
        }
    }
}

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buffer = vec![0u16; len as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut buffer);
        String::from_utf16_lossy(&buffer[..copied as usize])
    }
}

fn monitor_info(hmonitor: HMONITOR) -> (String, ScreenRect) {
    unsafe {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hmonitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            let name = String::from_utf16_lossy(&info.szDevice)
                .trim_end_matches('\0')
                .to_string();
            (name, ScreenRect::from(info.monitorInfo.rcMonitor))
        } else {
            (String::new(), ScreenRect::default())
        }
    }
}

fn log_snapshot(reason: &str, snapshot: &ForegroundSnapshot) {
    println!(
        "[foreground][{}] hwnd=0x{:X} pid={} exe=\"{}\" title=\"{}\" window_rect=({},{} {}x{}) client_rect=({},{} {}x{}) monitor=\"{}\" monitor_rect=({},{} {}x{}) intersects_captured_monitor={} covers_captured_monitor={} style=0x{:X} exstyle=0x{:X} foreground_on_captured_monitor={} foreground_covers_captured_monitor={} foreground_fullscreen_like={} foreground_exe_changed={}",
        reason,
        snapshot.hwnd.0 as usize,
        snapshot.pid,
        sanitize_log(&snapshot.exe_name),
        sanitize_log(&snapshot.title),
        snapshot.window_rect.left,
        snapshot.window_rect.top,
        snapshot.window_rect.width(),
        snapshot.window_rect.height(),
        snapshot.client_rect.left,
        snapshot.client_rect.top,
        snapshot.client_rect.width(),
        snapshot.client_rect.height(),
        sanitize_log(&snapshot.monitor_name),
        snapshot.monitor_rect.left,
        snapshot.monitor_rect.top,
        snapshot.monitor_rect.width(),
        snapshot.monitor_rect.height(),
        snapshot.intersects_captured_monitor,
        snapshot.covers_captured_monitor,
        snapshot.style as usize,
        snapshot.exstyle as usize,
        snapshot.foreground_on_captured_monitor,
        snapshot.foreground_covers_captured_monitor,
        snapshot.foreground_fullscreen_like,
        snapshot.foreground_exe_changed,
    );
}

fn classify_snapshot(snapshot: &ForegroundSnapshot) -> ForegroundBucket {
    let exe = snapshot.exe_name.to_ascii_lowercase();
    let title = snapshot.title.to_ascii_lowercase();
    if exe == "explorer.exe"
        || title.contains("task switching")
        || title.contains("alt-tab")
        || title.contains("task view")
    {
        return ForegroundBucket::TaskSwitcherExplorer;
    }

    if snapshot.foreground_on_captured_monitor && snapshot.foreground_fullscreen_like {
        return ForegroundBucket::GameForegroundOnCapturedMonitor;
    }

    if !snapshot.foreground_on_captured_monitor {
        return ForegroundBucket::OtherForegroundOnOtherMonitor;
    }

    ForegroundBucket::Other
}

fn sanitize_log(value: &str) -> String {
    value.replace(['\r', '\n', '"'], " ")
}

fn most_common(values: &HashMap<String, u128>) -> String {
    values
        .iter()
        .max_by_key(|(_, duration)| *duration)
        .map(|(value, _)| value.clone())
        .unwrap_or_default()
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
