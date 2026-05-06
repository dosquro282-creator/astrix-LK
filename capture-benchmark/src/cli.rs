use clap::Parser;
use std::env;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    #[default]
    Dxgi,
    Wgc,
    Both,
}

impl std::str::FromStr for Backend {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "dxgi" => Ok(Backend::Dxgi),
            "wgc" => Ok(Backend::Wgc),
            "both" => Ok(Backend::Both),
            _ => Err(format!("Invalid backend: {}. Use dxgi, wgc, or both", s)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CopyMode {
    #[default]
    Copy,
    None,
}

impl std::str::FromStr for CopyMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "copy" => Ok(CopyMode::Copy),
            "none" => Ok(CopyMode::None),
            _ => Err(format!("Invalid copy-mode: {}. Use copy or none", s)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceMode {
    #[default]
    Single,
    Separated,
}

impl std::str::FromStr for DeviceMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "single" => Ok(DeviceMode::Single),
            "separated" => Ok(DeviceMode::Separated),
            _ => Err(format!(
                "Invalid device-mode: {}. Use single or separated",
                s
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConvertTest {
    #[default]
    None,
    CopyOnly,
    BgraToNv12,
}

impl std::str::FromStr for ConvertTest {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "none" => Ok(ConvertTest::None),
            "copy-only" | "copy_only" => Ok(ConvertTest::CopyOnly),
            "bgra-to-nv12" | "bgra_to_nv12" => Ok(ConvertTest::BgraToNv12),
            _ => Err(format!(
                "Invalid convert-test: {}. Use none, copy-only, or bgra-to-nv12",
                s
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CpuPriority {
    #[default]
    Off,
    High,
    Realtime,
}

impl std::str::FromStr for CpuPriority {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "off" => Ok(CpuPriority::Off),
            "high" => Ok(CpuPriority::High),
            "realtime" => Ok(CpuPriority::Realtime),
            _ => Err(format!(
                "Invalid cpu-priority: {}. Use off, high, or realtime",
                s
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayMode {
    #[default]
    Off,
    Tiny,
    Transparent,
    VisibleBorder,
}

impl OverlayMode {
    pub fn as_str(self) -> &'static str {
        match self {
            OverlayMode::Off => "off",
            OverlayMode::Tiny => "tiny",
            OverlayMode::Transparent => "transparent",
            OverlayMode::VisibleBorder => "visible-border",
        }
    }
}

impl std::str::FromStr for OverlayMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "off" => Ok(OverlayMode::Off),
            "tiny" => Ok(OverlayMode::Tiny),
            "transparent" => Ok(OverlayMode::Transparent),
            "visible-border" | "visible_border" => Ok(OverlayMode::VisibleBorder),
            _ => Err(format!(
                "Invalid overlay-mode: {}. Use off, tiny, transparent, or visible-border",
                s
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GpuPriority(pub Option<i8>);

impl std::str::FromStr for GpuPriority {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "off" => Ok(GpuPriority(None)),
            v => {
                let parsed = v
                    .parse::<i8>()
                    .map_err(|_| format!("Invalid gpu-priority: {}. Use off or 0-7", s))?;
                if !(0..=7).contains(&parsed) {
                    return Err(format!("Invalid gpu-priority: {}. Must be 0-7", s));
                }
                Ok(GpuPriority(Some(parsed)))
            }
        }
    }
}

fn env_parse<T: std::str::FromStr>(var: &str) -> Option<T> {
    env::var(var).ok().and_then(|v| v.parse().ok())
}

#[derive(Debug, Clone, Parser)]
#[command(name = "capture-benchmark")]
#[command(about = "Benchmark DXGI Desktop Duplication vs Windows Graphics Capture", long_about = None)]
pub struct Args {
    #[arg(long, default_value = "dxgi", value_parser = clap::value_parser!(Backend))]
    pub backend: Backend,

    #[arg(long, default_value_t = 1000)]
    pub frames: usize,

    /// Run benchmark for N seconds after warmup instead of waiting for --frames successes
    #[arg(long)]
    pub duration_sec: Option<f64>,

    #[arg(long, default_value_t = 0)]
    pub monitor: usize,

    #[arg(long, default_value_t = 2)]
    pub timeout_ms: u32,

    #[arg(long, default_value = "copy", value_parser = clap::value_parser!(CopyMode))]
    pub copy_mode: CopyMode,

    #[arg(long, default_value_t = 60)]
    pub warmup: usize,

    #[arg(long)]
    pub csv: Option<String>,

    #[arg(long)]
    pub flush_each_frame: bool,

    // ===== NEW PARAMETERS =====
    /// Device mode: single (one device) or separated (capture + media devices)
    #[arg(long, default_value = "single", value_parser = clap::value_parser!(DeviceMode))]
    pub device_mode: DeviceMode,

    /// Ring size for shared textures in separated mode
    #[arg(long, default_value_t = 4)]
    pub ring_size: usize,

    /// Convert test mode: none, copy-only, or bgra-to-nv12
    #[arg(long, default_value = "none", value_parser = clap::value_parser!(ConvertTest))]
    pub convert_test: ConvertTest,

    /// Ready wait budget in microseconds (0 = no wait)
    #[arg(long, default_value_t = 0)]
    pub ready_wait_budget_us: u64,

    /// GPU thread priority for capture device (off or 0-7)
    #[arg(long, default_value = "off", value_parser = clap::value_parser!(GpuPriority))]
    pub gpu_priority_capture: GpuPriority,

    /// GPU thread priority for media device (off or 0-7)
    #[arg(long, default_value = "off", value_parser = clap::value_parser!(GpuPriority))]
    pub gpu_priority_media: GpuPriority,

    /// CPU priority mode: off, high, or realtime
    #[arg(long, default_value = "off", value_parser = clap::value_parser!(CpuPriority))]
    pub cpu_priority: CpuPriority,

    /// Print summary every N frames (0 = only at end)
    #[arg(long, default_value_t = 300)]
    pub summary_every: usize,

    /// Overlay compatibility mode for composition/present-path diagnostics
    #[arg(long, default_value = "off", value_parser = clap::value_parser!(OverlayMode))]
    pub overlay_mode: OverlayMode,
}

impl Args {
    /// Parse CLI args with env var overrides
    pub fn parse() -> Self {
        let args = <Self as clap::Parser>::parse();

        // Apply env var overrides if set
        Self {
            device_mode: env_parse::<DeviceMode>("ASTRIX_BENCH_DEVICE_MODE")
                .unwrap_or(args.device_mode),
            ring_size: env_parse("ASTRIX_BENCH_RING_SIZE").unwrap_or(args.ring_size),
            convert_test: env_parse::<ConvertTest>("ASTRIX_BENCH_CONVERT_TEST")
                .unwrap_or(args.convert_test),
            ready_wait_budget_us: env_parse("ASTRIX_BENCH_READY_WAIT_BUDGET_US")
                .unwrap_or(args.ready_wait_budget_us),
            gpu_priority_capture: env_parse::<GpuPriority>("ASTRIX_BENCH_GPU_PRIORITY_CAPTURE")
                .unwrap_or(args.gpu_priority_capture),
            gpu_priority_media: env_parse::<GpuPriority>("ASTRIX_BENCH_GPU_PRIORITY_MEDIA")
                .unwrap_or(args.gpu_priority_media),
            cpu_priority: env_parse::<CpuPriority>("ASTRIX_BENCH_CPU_PRIORITY")
                .unwrap_or(args.cpu_priority),
            summary_every: env_parse("ASTRIX_BENCH_SUMMARY_EVERY").unwrap_or(args.summary_every),
            overlay_mode: env_parse::<OverlayMode>("ASTRIX_BENCH_OVERLAY_MODE")
                .unwrap_or(args.overlay_mode),
            ..args
        }
    }

    pub fn to_config(&self) -> BenchConfig {
        BenchConfig {
            backend: self.backend,
            frames: self.frames,
            duration_sec: self.duration_sec,
            warmup: self.warmup,
            monitor_index: self.monitor,
            timeout_ms: self.timeout_ms,
            copy_mode: self.copy_mode,
            csv_path: self.csv.clone(),
            flush_each_frame: self.flush_each_frame,
            // NEW
            device_mode: self.device_mode,
            ring_size: self.ring_size,
            convert_test: self.convert_test,
            ready_wait_budget_us: self.ready_wait_budget_us,
            gpu_priority_capture: self.gpu_priority_capture,
            gpu_priority_media: self.gpu_priority_media,
            cpu_priority: self.cpu_priority,
            summary_every: self.summary_every,
            overlay_mode: self.overlay_mode,
            overlay_created: false,
            overlay_foreground_unchanged: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchConfig {
    pub backend: Backend,
    pub frames: usize,
    pub duration_sec: Option<f64>,
    pub warmup: usize,
    pub monitor_index: usize,
    pub timeout_ms: u32,
    pub copy_mode: CopyMode,
    pub csv_path: Option<String>,
    pub flush_each_frame: bool,
    // NEW
    pub device_mode: DeviceMode,
    pub ring_size: usize,
    pub convert_test: ConvertTest,
    pub ready_wait_budget_us: u64,
    pub gpu_priority_capture: GpuPriority,
    pub gpu_priority_media: GpuPriority,
    pub cpu_priority: CpuPriority,
    pub summary_every: usize,
    pub overlay_mode: OverlayMode,
    pub overlay_created: bool,
    pub overlay_foreground_unchanged: bool,
}

impl Default for BenchConfig {
    fn default() -> Self {
        let args = Args::parse();
        args.to_config()
    }
}
