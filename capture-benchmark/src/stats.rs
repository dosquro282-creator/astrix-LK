use serde::Serialize;
use std::fs::OpenOptions;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::BenchConfig;
use crate::foreground::{ForegroundBucket, ForegroundBucketSummary};

// ===== Extended Frame Metrics for detailed capture analysis =====

#[derive(Debug, Clone, Serialize)]
pub struct FrameMetric {
    pub frame_index: usize,
    pub timestamp_us: i64,
    // Capture timing
    pub acquire_wait_us: Option<u64>,
    pub get_resource_us: Option<u64>,
    pub copy_submit_us: Option<u64>,
    pub acquire_to_release_us: Option<u64>,
    pub release_frame_us: Option<u64>,
    pub total_capture_stage_us: Option<u64>,
    // WGC timing
    pub callback_gap_us: Option<u64>,
    pub copy_us: Option<u64>,
    pub held_frame_us: Option<u64>,
    // GPU readiness
    pub copy_ready_delay_us: Option<u64>,
    // Separated/shared metrics
    pub shared_open_us: Option<u64>,
    pub shared_sync_wait_us: Option<u64>,
    // Convert metrics
    pub convert_submit_us: Option<u64>,
    pub convert_ready_delay_us: Option<u64>,
    // Frame info
    pub accumulated_frames: u32,
    pub timeout: bool,
    pub error: Option<String>,
    pub warmup: bool,
    pub dropped: bool,
    pub dropped_reason: Option<String>,
    pub foreground_bucket: String,
}

impl FrameMetric {
    pub fn success(
        frame_index: usize,
        timestamp_us: i64,
        acquire_wait_us: u64,
        get_resource_us: u64,
        copy_submit_us: u64,
        acquire_to_release_us: u64,
        release_frame_us: u64,
        total_capture_stage_us: u64,
        accumulated: u32,
        warmup: bool,
    ) -> Self {
        Self {
            frame_index,
            timestamp_us,
            acquire_wait_us: Some(acquire_wait_us),
            get_resource_us: Some(get_resource_us),
            copy_submit_us: Some(copy_submit_us),
            acquire_to_release_us: Some(acquire_to_release_us),
            release_frame_us: Some(release_frame_us),
            total_capture_stage_us: Some(total_capture_stage_us),
            callback_gap_us: None,
            copy_us: None,
            held_frame_us: None,
            copy_ready_delay_us: None,
            shared_open_us: None,
            shared_sync_wait_us: None,
            convert_submit_us: None,
            convert_ready_delay_us: None,
            accumulated_frames: accumulated,
            timeout: false,
            error: None,
            warmup,
            dropped: false,
            dropped_reason: None,
            foreground_bucket: "other".to_string(),
        }
    }

    pub fn wgc_success(
        frame_index: usize,
        timestamp_us: i64,
        callback_gap_us: u64,
        copy_us: u64,
        held_frame_us: u64,
        warmup: bool,
    ) -> Self {
        Self {
            frame_index,
            timestamp_us,
            acquire_wait_us: None,
            get_resource_us: None,
            copy_submit_us: None,
            acquire_to_release_us: None,
            release_frame_us: None,
            total_capture_stage_us: None,
            callback_gap_us: Some(callback_gap_us),
            copy_us: Some(copy_us),
            held_frame_us: Some(held_frame_us),
            copy_ready_delay_us: None,
            shared_open_us: None,
            shared_sync_wait_us: None,
            convert_submit_us: None,
            convert_ready_delay_us: None,
            accumulated_frames: 1,
            timeout: false,
            error: None,
            warmup,
            dropped: false,
            dropped_reason: None,
            foreground_bucket: "other".to_string(),
        }
    }

    pub fn timeout(frame_index: usize, timestamp_us: i64, warmup: bool) -> Self {
        Self {
            frame_index,
            timestamp_us,
            acquire_wait_us: None,
            get_resource_us: None,
            copy_submit_us: None,
            acquire_to_release_us: None,
            release_frame_us: None,
            total_capture_stage_us: None,
            callback_gap_us: None,
            copy_us: None,
            held_frame_us: None,
            copy_ready_delay_us: None,
            shared_open_us: None,
            shared_sync_wait_us: None,
            convert_submit_us: None,
            convert_ready_delay_us: None,
            accumulated_frames: 0,
            timeout: true,
            error: Some("timeout".to_string()),
            warmup,
            dropped: false,
            dropped_reason: None,
            foreground_bucket: "other".to_string(),
        }
    }

    pub fn dropped_metric(
        frame_index: usize,
        timestamp_us: i64,
        reason: &str,
        warmup: bool,
    ) -> Self {
        Self {
            frame_index,
            timestamp_us,
            acquire_wait_us: None,
            get_resource_us: None,
            copy_submit_us: None,
            acquire_to_release_us: None,
            release_frame_us: None,
            total_capture_stage_us: None,
            callback_gap_us: None,
            copy_us: None,
            held_frame_us: None,
            copy_ready_delay_us: None,
            shared_open_us: None,
            shared_sync_wait_us: None,
            convert_submit_us: None,
            convert_ready_delay_us: None,
            accumulated_frames: 0,
            timeout: false,
            error: Some(reason.to_string()),
            warmup,
            dropped: true,
            dropped_reason: Some(reason.to_string()),
            foreground_bucket: "other".to_string(),
        }
    }

    pub fn error_metric(
        frame_index: usize,
        timestamp_us: i64,
        error: String,
        warmup: bool,
    ) -> Self {
        Self {
            frame_index,
            timestamp_us,
            acquire_wait_us: None,
            get_resource_us: None,
            copy_submit_us: None,
            acquire_to_release_us: None,
            release_frame_us: None,
            total_capture_stage_us: None,
            callback_gap_us: None,
            copy_us: None,
            held_frame_us: None,
            copy_ready_delay_us: None,
            shared_open_us: None,
            shared_sync_wait_us: None,
            convert_submit_us: None,
            convert_ready_delay_us: None,
            accumulated_frames: 0,
            timeout: false,
            error: Some(error),
            warmup,
            dropped: false,
            dropped_reason: None,
            foreground_bucket: "other".to_string(),
        }
    }

    pub fn with_foreground_bucket(mut self, bucket: &str) -> Self {
        self.foreground_bucket = bucket.to_string();
        self
    }

    pub fn is_success(&self) -> bool {
        !self.timeout
            && !self.dropped
            && self.error.is_none()
            && (self.acquire_wait_us.is_some() || self.callback_gap_us.is_some())
    }
}

#[derive(Debug, Clone, Default)]
pub struct StatsCollector {
    pub metrics: Vec<FrameMetric>,
    pub warmup_count: usize,
    // Extended counters
    pub acquire_timeout_count: usize,
    pub copy_ready_timeout_count: usize,
    pub convert_timeout_count: usize,
    pub dropped_gpu_not_ready_count: usize,
    pub shared_busy_drop_count: usize,
    pub convert_dropped_not_ready_count: usize,
}

impl StatsCollector {
    pub fn new() -> Self {
        Self {
            metrics: Vec::new(),
            warmup_count: 0,
            acquire_timeout_count: 0,
            copy_ready_timeout_count: 0,
            convert_timeout_count: 0,
            dropped_gpu_not_ready_count: 0,
            shared_busy_drop_count: 0,
            convert_dropped_not_ready_count: 0,
        }
    }

    pub fn add_metric(&mut self, metric: FrameMetric) {
        if metric.warmup {
            self.warmup_count += 1;
        }
        if metric.timeout {
            self.acquire_timeout_count += 1;
        }
        if let Some(ref reason) = metric.dropped_reason {
            if reason.contains("gpu_not_ready") {
                self.dropped_gpu_not_ready_count += 1;
            } else if reason.contains("shared_busy") {
                self.shared_busy_drop_count += 1;
            } else if reason.contains("convert_not_ready") {
                self.convert_dropped_not_ready_count += 1;
            }
        }
        self.metrics.push(metric);
    }

    pub fn get_all_metrics(&self) -> &[FrameMetric] {
        &self.metrics
    }

    pub fn get_main_metrics(&self) -> Vec<&FrameMetric> {
        self.metrics.iter().filter(|m| !m.warmup).collect()
    }

    pub fn get_warmup_metrics(&self) -> Vec<&FrameMetric> {
        self.metrics.iter().filter(|m| m.warmup).collect()
    }

    pub fn frames_attempted(&self) -> usize {
        self.metrics.len()
    }

    pub fn frames_acquired(&self) -> usize {
        self.metrics
            .iter()
            .filter(|m| !m.warmup && m.is_success())
            .count()
    }

    pub fn frames_dropped(&self) -> usize {
        self.metrics
            .iter()
            .filter(|m| !m.warmup && m.dropped)
            .count()
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CallbackGapHistogram {
    pub callback_gap_0_16ms: usize,
    pub callback_gap_16_33ms: usize,
    pub callback_gap_33_50ms: usize,
    pub callback_gap_50_100ms: usize,
    pub callback_gap_100_500ms: usize,
    pub callback_gap_500_1000ms: usize,
    pub callback_gap_over_1000ms: usize,
}

impl CallbackGapHistogram {
    pub fn compute(values_us: &[u64]) -> Self {
        let mut histogram = Self::default();
        for value in values_us {
            let ms = *value as f64 / 1000.0;
            if ms < 16.0 {
                histogram.callback_gap_0_16ms += 1;
            } else if ms < 33.0 {
                histogram.callback_gap_16_33ms += 1;
            } else if ms < 50.0 {
                histogram.callback_gap_33_50ms += 1;
            } else if ms < 100.0 {
                histogram.callback_gap_50_100ms += 1;
            } else if ms < 500.0 {
                histogram.callback_gap_100_500ms += 1;
            } else if ms < 1000.0 {
                histogram.callback_gap_500_1000ms += 1;
            } else {
                histogram.callback_gap_over_1000ms += 1;
            }
        }
        histogram
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Percentiles {
    pub avg: f64,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

impl Percentiles {
    pub fn compute(values: &[u64]) -> Self {
        if values.is_empty() {
            return Self {
                avg: 0.0,
                p50: 0,
                p95: 0,
                p99: 0,
                max: 0,
            };
        }

        let sum: u64 = values.iter().sum();
        let avg = sum as f64 / values.len() as f64;

        let mut sorted = values.to_vec();
        sorted.sort_unstable();

        let p50_idx = (values.len() as f64 * 0.50) as usize;
        let p95_idx = (values.len() as f64 * 0.95) as usize;
        let p99_idx = (values.len() as f64 * 0.99) as usize;

        let p50 = sorted
            .get(p50_idx.min(sorted.len() - 1))
            .copied()
            .unwrap_or(0);
        let p95 = sorted
            .get(p95_idx.min(sorted.len() - 1))
            .copied()
            .unwrap_or(0);
        let p99 = sorted
            .get(p99_idx.min(sorted.len() - 1))
            .copied()
            .unwrap_or(0);
        let max = *sorted.last().unwrap_or(&0);

        Self {
            avg,
            p50,
            p95,
            p99,
            max,
        }
    }
}

// ===== Extended Bench Summary with all metrics =====

#[derive(Debug, Clone, Serialize)]
pub struct BenchSummary {
    pub backend: String,
    pub monitor_info: String,
    pub gpu_name: String,
    pub frames_requested: usize,
    pub warmup_frames: usize,
    pub successful_frames: usize,
    pub timeouts: usize,
    pub access_lost: usize,
    pub duplicated_errors: usize,
    pub elapsed_ms: u64,
    pub captured_fps: f64,
    pub steady_state_fps: f64,
    pub effective_source_fps: Option<f64>,
    pub accumulated_frames_total: u64,
    pub accumulated_frames_max: u32,
    // Legacy metrics (kept for compatibility)
    pub acquire_wait_us: Option<Percentiles>,
    pub callback_gap_us: Option<Percentiles>,
    pub copy_us: Option<Percentiles>,
    pub held_frame_us: Option<Percentiles>,
    // NEW: Extended capture metrics
    pub get_resource_us: Option<Percentiles>,
    pub copy_submit_us: Option<Percentiles>,
    pub acquire_to_release_us: Option<Percentiles>,
    pub release_frame_us: Option<Percentiles>,
    pub total_capture_stage_us: Option<Percentiles>,
    // GPU readiness
    pub copy_ready_delay_us: Option<Percentiles>,
    pub copy_ready_timeout_count: usize,
    pub dropped_gpu_not_ready_count: usize,
    // Separated/shared
    pub shared_create_handle_us: Option<Percentiles>,
    pub shared_open_us: Option<Percentiles>,
    pub shared_sync_wait_us: Option<Percentiles>,
    pub shared_busy_drop_count: usize,
    pub media_actual_used_count: usize,
    pub media_open_failed_count: usize,
    pub separated_path_valid: bool,
    pub same_adapter_luid: Option<bool>,
    pub shared_path: String,
    // Convert
    pub convert_submit_us: Option<Percentiles>,
    pub convert_ready_delay_us: Option<Percentiles>,
    pub convert_timeout_count: usize,
    pub convert_dropped_not_ready_count: usize,
    // Frame pacing
    pub frames_attempted: usize,
    pub frames_acquired: usize,
    pub frames_dropped: usize,
    pub frame_age_us: Option<Percentiles>,
    pub longest_gap_between_acquired_ms: Option<f64>,
    pub longest_gap_between_produced_ms: Option<f64>,
    pub p95_gap_ms: f64,
    pub p99_gap_ms: f64,
    pub long_gap_count: usize,
    pub warmup_elapsed_ms: u64,
    pub benchmark_elapsed_ms: u64,
    pub startup_long_gap_count: usize,
    pub first_frame_delay_ms: Option<f64>,
    pub first_steady_frame_time_ms: Option<f64>,
    pub startup_gap_ms: f64,
    pub foreground_buckets: Vec<ForegroundBucketSummary>,
    pub foreground_game_fps: f64,
    pub foreground_other_fps: f64,
    pub callback_gap_histogram: Option<CallbackGapHistogram>,
    // Priority info
    pub device_mode: String,
    pub cpu_priority_mode: String,
    pub gpu_priority_capture: String,
    pub gpu_priority_media: String,
    pub ready_wait_budget_us: u64,
    // Foreground/composition diagnostics
    pub percent_time_foreground_on_captured_monitor: f64,
    pub long_gaps_while_game_foreground: usize,
    pub long_gaps_while_other_foreground: usize,
    pub foreground_exe_most_common: String,
    pub foreground_title_most_common: String,
    // Overlay compatibility diagnostics
    pub overlay_mode: String,
    pub overlay_created: bool,
    pub foreground_unchanged: bool,
}

impl BenchSummary {
    pub fn print(&self) {
        println!("\n============================================");
        println!("  {} BENCHMARK SUMMARY", self.backend.to_uppercase());
        println!("============================================");
        println!("monitor: {}", self.monitor_info);
        println!("gpu: {}", self.gpu_name);
        println!("device_mode: {}", self.device_mode);
        println!();
        println!("frames requested: {}", self.frames_requested);
        println!("warmup frames: {}", self.warmup_frames);
        println!("successful frames: {}", self.successful_frames);
        println!("timeouts: {}", self.timeouts);
        println!("access_lost: {}", self.access_lost);
        println!("duplicated_errors: {}", self.duplicated_errors);
        println!("elapsed_ms: {}", self.elapsed_ms);
        println!("warmup_elapsed_ms: {}", self.warmup_elapsed_ms);
        println!("benchmark_elapsed_ms: {}", self.benchmark_elapsed_ms);
        println!("captured_fps: {:.1}", self.captured_fps);
        println!("steady_state_fps: {:.1}", self.steady_state_fps);
        println!("startup_long_gap_count: {}", self.startup_long_gap_count);
        if let Some(v) = self.first_frame_delay_ms {
            println!("first_frame_delay_ms: {:.2}", v);
        }
        if let Some(v) = self.first_steady_frame_time_ms {
            println!("first_steady_frame_time_ms: {:.2}", v);
        }
        println!("startup_gap_ms: {:.2}", self.startup_gap_ms);

        if let Some(fps) = self.effective_source_fps {
            println!("effective_source_fps: {:.1}", fps);
        }

        println!();
        println!("--- Priority Settings ---");
        println!("cpu_priority: {}", self.cpu_priority_mode);
        println!("gpu_priority_capture: {}", self.gpu_priority_capture);
        println!("gpu_priority_media: {}", self.gpu_priority_media);
        println!("ready_wait_budget_us: {}", self.ready_wait_budget_us);

        println!();
        println!("--- Foreground Diagnostics ---");
        println!(
            "percent_time_foreground_on_captured_monitor: {:.1}",
            self.percent_time_foreground_on_captured_monitor
        );
        println!(
            "long_gaps_while_game_foreground: {}",
            self.long_gaps_while_game_foreground
        );
        println!(
            "long_gaps_while_other_foreground: {}",
            self.long_gaps_while_other_foreground
        );
        println!(
            "foreground_exe_most_common: {}",
            self.foreground_exe_most_common
        );
        println!(
            "foreground_title_most_common: {}",
            self.foreground_title_most_common
        );
        println!();
        println!("--- Foreground Buckets ---");
        for bucket in &self.foreground_buckets {
            println!(
                "{}: duration_ms={} successful_frames={} fps={:.1} longest_gap_ms={:.2} long_gap_count={} p95_gap_ms={:.2} p99_gap_ms={:.2}",
                bucket.bucket,
                bucket.duration_ms,
                bucket.successful_frames,
                bucket.fps,
                bucket.longest_gap_ms,
                bucket.long_gap_count,
                bucket.p95_gap_ms,
                bucket.p99_gap_ms
            );
        }

        println!();
        println!("--- Overlay Compatibility ---");
        println!("overlay_mode: {}", self.overlay_mode);
        println!("overlay_created: {}", self.overlay_created);
        println!("foreground_unchanged: {}", self.foreground_unchanged);

        println!();
        println!("--- Capture Timing (us) ---");
        self.print_percentiles("acquire_wait_us", self.acquire_wait_us.as_ref());
        self.print_percentiles("get_resource_us", self.get_resource_us.as_ref());
        self.print_percentiles("copy_submit_us", self.copy_submit_us.as_ref());
        self.print_percentiles("acquire_to_release_us", self.acquire_to_release_us.as_ref());
        self.print_percentiles("release_frame_us", self.release_frame_us.as_ref());
        self.print_percentiles(
            "total_capture_stage_us",
            self.total_capture_stage_us.as_ref(),
        );

        println!();
        println!("--- GPU Readiness ---");
        if let Some(ref p) = self.copy_ready_delay_us {
            println!(
                "copy_ready_delay_us: avg={:.1}, p50={}, p95={}, p99={}, max={}",
                p.avg, p.p50, p.p95, p.p99, p.max
            );
        }
        println!(
            "copy_ready_timeout_count: {}",
            self.copy_ready_timeout_count
        );
        println!(
            "dropped_gpu_not_ready_count: {}",
            self.dropped_gpu_not_ready_count
        );

        if let Some(ref p) = self.callback_gap_us {
            self.print_percentiles("callback_gap_us", Some(p));
        }
        if let Some(histogram) = self.callback_gap_histogram {
            println!("callback_gap_0_16ms: {}", histogram.callback_gap_0_16ms);
            println!("callback_gap_16_33ms: {}", histogram.callback_gap_16_33ms);
            println!("callback_gap_33_50ms: {}", histogram.callback_gap_33_50ms);
            println!("callback_gap_50_100ms: {}", histogram.callback_gap_50_100ms);
            println!(
                "callback_gap_100_500ms: {}",
                histogram.callback_gap_100_500ms
            );
            println!(
                "callback_gap_500_1000ms: {}",
                histogram.callback_gap_500_1000ms
            );
            println!(
                "callback_gap_over_1000ms: {}",
                histogram.callback_gap_over_1000ms
            );
        }
        if let Some(ref p) = self.copy_us {
            self.print_percentiles("copy_us", Some(p));
        }
        if let Some(ref p) = self.held_frame_us {
            self.print_percentiles("held_frame_us", Some(p));
        }

        println!();
        println!("[separated]");
        println!("path_valid={}", self.separated_path_valid);
        println!("shared_path={}", self.shared_path);
        println!("media_actual_used_count={}", self.media_actual_used_count);
        println!("media_open_failed_count={}", self.media_open_failed_count);
        if let Some(same_adapter_luid) = self.same_adapter_luid {
            println!("same_adapter_luid={}", same_adapter_luid);
        }
        self.print_percentiles(
            "shared_create_handle_us",
            self.shared_create_handle_us.as_ref(),
        );
        self.print_percentiles("shared_open_us", self.shared_open_us.as_ref());
        self.print_percentiles("shared_sync_wait_us", self.shared_sync_wait_us.as_ref());
        println!("shared_busy_drop_count: {}", self.shared_busy_drop_count);

        println!();
        println!("--- Convert ---");
        self.print_percentiles("convert_submit_us", self.convert_submit_us.as_ref());
        self.print_percentiles(
            "convert_ready_delay_us",
            self.convert_ready_delay_us.as_ref(),
        );
        println!("convert_timeout_count: {}", self.convert_timeout_count);
        println!(
            "convert_dropped_not_ready_count: {}",
            self.convert_dropped_not_ready_count
        );

        println!();
        println!("--- Frame Pacing ---");
        println!("frames_attempted: {}", self.frames_attempted);
        println!("frames_acquired: {}", self.frames_acquired);
        println!("frames_dropped: {}", self.frames_dropped);
        if let Some(ref p) = self.frame_age_us {
            self.print_percentiles("frame_age_us", Some(p));
        }
        if let Some(v) = self.longest_gap_between_acquired_ms {
            println!("longest_gap_between_acquired_ms: {:.2}", v);
        }
        if let Some(v) = self.longest_gap_between_produced_ms {
            println!("longest_gap_between_produced_ms: {:.2}", v);
        }
        println!("p95_gap_ms: {:.2}", self.p95_gap_ms);
        println!("p99_gap_ms: {:.2}", self.p99_gap_ms);
        println!("long_gap_count: {}", self.long_gap_count);

        if self.accumulated_frames_total > 0 {
            println!();
            println!(
                "accumulated_frames_total: {}",
                self.accumulated_frames_total
            );
            println!("accumulated_frames_max: {}", self.accumulated_frames_max);
        }

        println!();
    }

    fn print_percentiles(&self, name: &str, p: Option<&Percentiles>) {
        if let Some(p) = p {
            println!(
                "{}: avg={:.1}, p50={}, p95={}, p99={}, max={}",
                name, p.avg, p.p50, p.p95, p.p99, p.max
            );
        }
    }
}

pub fn write_summary_csv(
    summary: &BenchSummary,
    csv_path: &str,
    config: &BenchConfig,
) -> anyhow::Result<()> {
    let exists = Path::new(csv_path).exists();
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(csv_path)?;
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .from_writer(file);

    if !exists {
        writer.write_record(&[
            "timestamp",
            "backend",
            "duration_sec",
            "foreground_game_fps",
            "foreground_other_fps",
            "captured_fps",
            "steady_state_fps",
            "longest_gap_ms",
            "p95_gap_ms",
            "p99_gap_ms",
            "long_gap_count",
            "startup_gap_ms",
            "overlay_mode",
            "cpu_priority",
            "gpu_priority_capture",
            "gpu_priority_media",
        ])?;
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());
    let duration_sec = config
        .duration_sec
        .unwrap_or(summary.benchmark_elapsed_ms as f64 / 1000.0);
    let longest_gap_ms = summary
        .longest_gap_between_produced_ms
        .or(summary.longest_gap_between_acquired_ms)
        .unwrap_or(0.0);

    writer.write_record(&[
        timestamp,
        summary.backend.clone(),
        format!("{:.3}", duration_sec),
        format!("{:.3}", summary.foreground_game_fps),
        format!("{:.3}", summary.foreground_other_fps),
        format!("{:.3}", summary.captured_fps),
        format!("{:.3}", summary.steady_state_fps),
        format!("{:.3}", longest_gap_ms),
        format!("{:.3}", summary.p95_gap_ms),
        format!("{:.3}", summary.p99_gap_ms),
        summary.long_gap_count.to_string(),
        format!("{:.3}", summary.startup_gap_ms),
        summary.overlay_mode.clone(),
        summary.cpu_priority_mode.clone(),
        summary.gpu_priority_capture.clone(),
        summary.gpu_priority_media.clone(),
    ])?;

    writer.flush()?;
    Ok(())
}

pub fn foreground_bucket_fps(buckets: &[ForegroundBucketSummary], bucket: ForegroundBucket) -> f64 {
    buckets
        .iter()
        .find(|summary| summary.bucket == bucket.as_str())
        .map(|summary| summary.fps)
        .unwrap_or(0.0)
}
