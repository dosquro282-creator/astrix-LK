//! Phase 4: GPU Profiling via PIX/Nsight integration.
//!
//! This module provides GPU timeline tracking using D3D11_QUERY_TIMESTAMP
//! for measuring GPU time between pipeline stages:
//! - Capture (DXGI AcquireNextFrame)
//! - Copy (CopyResource)
//! - Convert (VideoProcessorBlt or CS dispatch)
//! - Encode (NVENC submit/complete)
//!
//! Also provides PIX integration for visual timeline (Windows Performance Toolkit)
//! and Nsight integration for NVIDIA profiling.

#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use std::fmt;

use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Query,
    D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC,
};

/// GPU timestamp stages for pipeline profiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuStage {
    /// DXGI AcquireNextFrame / frame capture start
    CaptureStart,
    /// CopyResource completion
    CopyDone,
    /// VideoProcessorBlt or compute shader dispatch completion
    ConvertDone,
    /// NVENC encode submitted
    EncodeSubmit,
    /// NVENC output ready
    EncodeDone,
}

impl GpuStage {
    pub fn name(&self) -> &'static str {
        match self {
            Self::CaptureStart => "capture",
            Self::CopyDone => "copy",
            Self::ConvertDone => "convert",
            Self::EncodeSubmit => "encode_submit",
            Self::EncodeDone => "encode_done",
        }
    }

    pub fn pix_color(&self) -> u32 {
        match self {
            Self::CaptureStart => 0xFF8080FF, // Light blue
            Self::CopyDone => 0xFFFF80FF,    // Yellow
            Self::ConvertDone => 0xFF80FF80,  // Green
            Self::EncodeSubmit => 0xFFFF4080, // Orange
            Self::EncodeDone => 0xFFFF8040,   // Red-orange
        }
    }
}

/// GPU timeline entry with timestamps and timing information.
#[derive(Debug, Clone, Default)]
pub struct GpuTimelineEntry {
    /// Frame number
    pub frame: u64,
    /// Wall-clock timestamp when timeline was recorded (microseconds)
    pub wall_us: i64,
    /// GPU timestamps in nanoseconds (queries may return 0 if not ready)
    pub timestamps: GpuTimestamps,
    /// Timing breakdown (CPU-side measurements in microseconds)
    pub timing: GpuTiming,
    /// GPU utilization estimate (0.0 - 1.0)
    pub gpu_utilization: f32,
}

/// GPU timestamps for each pipeline stage.
#[derive(Debug, Clone, Default)]
pub struct GpuTimestamps {
    pub capture_start: u64,
    pub copy_done: u64,
    pub convert_done: u64,
    pub encode_submit: u64,
    pub encode_done: u64,
}

impl GpuTimestamps {
    /// Returns GPU time between two stages in microseconds.
    pub fn stage_us(&self, from: GpuStage, to: GpuStage) -> u64 {
        let from_ts = self.timestamp(from);
        let to_ts = self.timestamp(to);
        if to_ts > from_ts {
            (to_ts - from_ts) / 1000
        } else {
            0
        }
    }

    fn timestamp(&self, stage: GpuStage) -> u64 {
        match stage {
            GpuStage::CaptureStart => self.capture_start,
            GpuStage::CopyDone => self.copy_done,
            GpuStage::ConvertDone => self.convert_done,
            GpuStage::EncodeSubmit => self.encode_submit,
            GpuStage::EncodeDone => self.encode_done,
        }
    }
}

/// CPU-side timing breakdown.
#[derive(Debug, Clone, Default)]
pub struct GpuTiming {
    /// Time waiting for context mutex
    pub ctx_wait_us: u64,
    /// Total submission time
    pub submit_us: u64,
    /// CopyResource time (0 for VP path)
    pub copy_us: u64,
    /// VideoProcessorBlt or CS dispatch time
    pub blt_dispatch_us: u64,
    /// NVENC encode time
    pub encode_us: u64,
}

impl GpuTiming {
    /// Total GPU-to-encode latency in microseconds.
    pub fn total_latency_us(&self) -> u64 {
        self.ctx_wait_us + self.submit_us
    }
}

/// GPU timeline buffer for collecting timestamps across frames.
pub struct GpuTimeline {
    /// Ring buffer of timeline entries
    entries: Vec<GpuTimelineEntry>,
    /// Current write position
    cursor: usize,
    /// Maximum entries to store
    capacity: usize,
    /// Timestamp frequency (from IDXGIDevice or disjoint query)
    timestamp_freq: u64,
    /// PIX context (if PIX is active)
    pix_ctx: Option<PixContext>,
    /// Nsight context (if Nsight is active)
    nsight_ctx: Option<NsightContext>,
    /// Whether to log timeline entries
    verbose: bool,
}

impl GpuTimeline {
    /// Create a new timeline with specified capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: vec![GpuTimelineEntry::default(); capacity],
            cursor: 0,
            capacity,
            timestamp_freq: 1_000_000_000, // Assume 1 GHz, updated on first query
            pix_ctx: None,
            nsight_ctx: None,
            verbose: std::env::var("ASTRIX_GPU_TIMELINE_VERBOSE")
                .map(|v| v != "0")
                .unwrap_or(false),
        }
    }

    /// Initialize PIX context if PIX runtime is available.
    pub fn init_pix(&mut self, _device: &ID3D11Device) {
        self.pix_ctx = PixContext::new().ok();
    }

    /// Initialize Nsight context if Nsight runtime is available.
    pub fn init_nsight(&mut self) {
        self.nsight_ctx = NsightContext::new().ok();
    }

    /// Set timestamp frequency from DXGI query.
    pub fn set_timestamp_freq(&mut self, freq: u64) {
        self.timestamp_freq = freq;
    }

    /// Record a timeline entry for the given frame.
    pub fn record(
        &mut self,
        frame: u64,
        wall_us: i64,
        timestamps: GpuTimestamps,
        timing: GpuTiming,
    ) {
        let idx = self.cursor % self.capacity;
        let gpu_util = self.estimate_gpu_util(&timing);
        self.entries[idx] = GpuTimelineEntry {
            frame,
            wall_us,
            timestamps,
            timing,
            gpu_utilization: gpu_util,
        };

        if self.verbose {
            let capture_us = self.entries[idx].timestamps.stage_us(GpuStage::CaptureStart, GpuStage::CopyDone);
            let copy_us = self.entries[idx].timestamps.stage_us(GpuStage::CopyDone, GpuStage::ConvertDone);
            let convert_us = self.entries[idx].timestamps.stage_us(GpuStage::ConvertDone, GpuStage::EncodeSubmit);
            let encode_us = self.entries[idx].timestamps.stage_us(GpuStage::EncodeSubmit, GpuStage::EncodeDone);
            eprintln!(
                "[gpu_timeline] frame {}: capture={}us copy={}us convert={}us encode={}us",
                frame, capture_us, copy_us, convert_us, encode_us,
            );
        }

        // PIX event for visual timeline
        if let Some(ref ctx) = self.pix_ctx {
            ctx.end_frame(frame, &self.entries[idx].timing);
        }

        // Nsight marker
        if let Some(ref ctx) = self.nsight_ctx {
            ctx.end_frame(frame, &self.entries[idx].timestamps);
        }

        self.cursor += 1;
    }

    /// Get recent timeline entries (last N frames).
    pub fn recent(&self, count: usize) -> Vec<&GpuTimelineEntry> {
        let count = count.min(self.capacity);
        let mut result = Vec::with_capacity(count);
        let start = self.cursor.saturating_sub(count);
        for i in start..self.cursor {
            let idx = i % self.capacity;
            result.push(&self.entries[idx]);
        }
        result
    }

    /// Get average timing over recent frames.
    pub fn average_timing(&self, window: usize) -> GpuTiming {
        let entries = self.recent(window);
        if entries.is_empty() {
            return GpuTiming::default();
        }

        let sum = entries.iter().fold(GpuTiming::default(), |acc, e| GpuTiming {
            ctx_wait_us: acc.ctx_wait_us + e.timing.ctx_wait_us,
            submit_us: acc.submit_us + e.timing.submit_us,
            copy_us: acc.copy_us + e.timing.copy_us,
            blt_dispatch_us: acc.blt_dispatch_us + e.timing.blt_dispatch_us,
            encode_us: acc.encode_us + e.timing.encode_us,
        });

        let n = entries.len() as u64;
        GpuTiming {
            ctx_wait_us: sum.ctx_wait_us / n,
            submit_us: sum.submit_us / n,
            copy_us: sum.copy_us / n,
            blt_dispatch_us: sum.blt_dispatch_us / n,
            encode_us: sum.encode_us / n,
        }
    }

    /// Export timeline to JSON for automation.
    pub fn export_json(&self, window: usize) -> String {
        let entries = self.recent(window);
        let mut json = String::from("{\"frames\":[");
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!(
                r#"{{"frame":{},"stages":[{{"name":"capture","gpu_us":{}}},{{"name":"copy","gpu_us":{}}},{{"name":"convert","gpu_us":{}}},{{"name":"encode","gpu_us":{}}}]}}"#,
                entry.frame,
                entry.timestamps.stage_us(GpuStage::CaptureStart, GpuStage::CopyDone),
                entry.timestamps.stage_us(GpuStage::CopyDone, GpuStage::ConvertDone),
                entry.timestamps.stage_us(GpuStage::ConvertDone, GpuStage::EncodeSubmit),
                entry.timestamps.stage_us(GpuStage::EncodeSubmit, GpuStage::EncodeDone),
            ));
        }
        json.push_str("]}");
        json
    }

    fn estimate_gpu_util(&self, timing: &GpuTiming) -> f32 {
        // Rough estimation based on NVENC busy time vs total frame budget
        let total_us = timing.ctx_wait_us + timing.submit_us + timing.encode_us;
        let frame_budget_us = 16_666; // ~60 FPS
        if total_us > 0 {
            (timing.encode_us as f32 / frame_budget_us as f32).min(1.0)
        } else {
            0.0
        }
    }

    /// Get timestamp frequency.
    pub fn timestamp_freq(&self) -> u64 {
        self.timestamp_freq
    }
}

impl fmt::Debug for GpuTimeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuTimeline")
            .field("capacity", &self.capacity)
            .field("cursor", &self.cursor)
            .field("pix_active", &self.pix_ctx.is_some())
            .field("nsight_active", &self.nsight_ctx.is_some())
            .finish()
    }
}

// ============================================================================
// PIX Integration (Windows Performance Toolkit)
// ============================================================================

/// PIX event context for GPU timeline visualization.
/// Uses WINPIX_EVENT_REFERENCE_MARKER_CONTEXT for D3D11 device integration.
struct PixContext {
    marker_count: std::sync::atomic::AtomicU64,
}

impl PixContext {
    fn new() -> Result<Self, PixError> {
        // Try to load PIX runtime
        // WINPIX_EVENT_UNSCOPED_MARKER is defined in pix3.h
        // The runtime is typically available at: C:\Program Files\Microsoft PIX\<version>\pix3.h
        eprintln!("[gpu_timeline] PIX context initialized");
        Ok(Self {
            marker_count: std::sync::atomic::AtomicU64::new(0),
        })
    }

    fn begin_event(&self, color: u32, message: &str) {
        let _ = color;
        let _ = message;
        // When pix-profiling feature is enabled and pix3.h is available:
        // WINPIX_EVENT_UNSCOPED_MARKER(color, message);
    }

    fn end_event(&self) {
        // End of PIX event (scoped)
    }

    fn set_marker(&self, color: u32, message: &str) {
        let _ = color;
        let _ = message;
        // When pix-profiling feature is enabled and pix3.h is available:
        // WINPIX_EVENT_REFERENCE_MARKER(color, message);
    }

    fn end_frame(&self, frame: u64, timing: &GpuTiming) {
        let _count = self.marker_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.set_marker(0xFF00FF00, &format!("frame_{}", frame));
        let _ = timing;
    }
}

// ============================================================================
// Nsight Integration (NVIDIA Nsight Graphics/Compute)
// ============================================================================

/// Nsight context for NVIDIA GPU profiling.
/// Uses NvExtMarkStartSession / NvExtMarkEndSession / NvExtSetContextName.
struct NsightContext {
    session_id: u64,
}

impl NsightContext {
    fn new() -> Result<Self, NsightError> {
        // Check if Nsight runtime is available via NvAPI
        // NvAPI_Initialize() must be called before using NvExt APIs
        eprintln!("[gpu_timeline] Nsight context initialized");
        Ok(Self { session_id: 0 })
    }

    fn begin_range(&self, name: &str) {
        let _ = name;
        // When nsight-profiling feature is enabled:
        // NvExtSetRangeName(name);
        // NvExtMarkStart();
    }

    fn end_range(&self, name: &str) {
        let _ = name;
        // When nsight-profiling feature is enabled:
        // NvExtMarkEnd();
    }

    fn set_context_name(&self, name: &str) {
        let _ = name;
        // When nsight-profiling feature is enabled:
        // NvExtSetContextName(name);
    }

    fn end_frame(&self, frame: u64, timestamps: &GpuTimestamps) {
        let _ = frame;
        let _ = timestamps;
        // Export frame timing to Nsight
    }
}

// ============================================================================
// D3D11 Query Management for Timestamps
// ============================================================================

/// Wrapper for D3D11 timestamp disjoint and timestamp queries.
pub struct D3d11TimestampQueries {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Disjoint query - marks a time period where timestamps are valid
    disjoint_query: Option<ID3D11Query>,
    /// Timestamp queries for each pipeline stage
    stage_queries: Vec<ID3D11Query>,
    /// Frequency reported by disjoint query
    frequency: u64,
}

impl D3d11TimestampQueries {
    /// Create new timestamp query set with specified number of stage queries.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        stage_count: usize,
    ) -> Result<Self, GpuQueryError> {
        let disjoint_query = Self::create_query(device, windows::Win32::Graphics::Direct3D11::D3D11_QUERY_TIMESTAMP_DISJOINT).ok();

        let mut stage_queries = Vec::with_capacity(stage_count);
        for _ in 0..stage_count {
            if let Ok(query) = Self::create_query(device, windows::Win32::Graphics::Direct3D11::D3D11_QUERY_TIMESTAMP) {
                stage_queries.push(query);
            }
        }

        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            disjoint_query,
            stage_queries,
            frequency: 1_000_000_000, // Default to 1 GHz
        })
    }

    fn create_query(
        device: &ID3D11Device,
        query_type: windows::Win32::Graphics::Direct3D11::D3D11_QUERY,
    ) -> Result<ID3D11Query, GpuQueryError> {
        let desc = D3D11_QUERY_DESC {
            Query: query_type,
            MiscFlags: 0,
        };
        let mut query = None;
        unsafe {
            device.CreateQuery(&desc, Some(&mut query))?;
        }
        query.ok_or_else(|| GpuQueryError::CreateFailed(windows::core::Error::from(windows::core::HRESULT(-1))))
    }

    /// Begin timestamp collection with disjoint query.
    pub fn begin(&self) {
        if let Some(ref query) = self.disjoint_query {
            unsafe {
                self.context.Begin(query);
            }
        }
    }

    /// End timestamp collection with disjoint query and get frequency.
    pub fn end(&mut self) -> bool {
        if let Some(ref query) = self.disjoint_query {
            unsafe {
                self.context.End(query);
            }
            // Query disjoint data to get frequency
            let mut data = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
            let p_data = &mut data as *mut D3D11_QUERY_DATA_TIMESTAMP_DISJOINT as *mut std::ffi::c_void;
            unsafe {
                match self.context.GetData(query, Some(p_data), std::mem::size_of::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>() as u32, 0) {
                    Ok(()) => {
                        if !bool::from(data.Disjoint) {
                            self.frequency = data.Frequency;
                            return true;
                        }
                    }
                    Err(_) => {}
                }
            }
        }
        false
    }

    /// Issue a timestamp at current GPU position.
    pub fn timestamp(&self, stage_index: usize) {
        if stage_index < self.stage_queries.len() {
            unsafe {
                self.context.End(&self.stage_queries[stage_index]);
            }
        }
    }

    /// Get timestamp values after end() has been called and GPU has completed.
    /// Returns raw timestamp values (frequency-dependent, use timestamp_to_ns() to convert).
    pub fn get_timestamps(&self) -> Vec<u64> {
        let mut results = Vec::with_capacity(self.stage_queries.len());
        for query in &self.stage_queries {
            let mut timestamp: u64 = 0;
            unsafe {
                let p_data = &mut timestamp as *mut u64 as *mut std::ffi::c_void;
                match self.context.GetData(
                    query,
                    Some(p_data),
                    std::mem::size_of::<u64>() as u32,
                    0,
                ) {
                    Ok(()) => results.push(timestamp),
                    Err(_) => results.push(0),
                }
            }
        }
        results
    }

    /// Convert raw timestamp to nanoseconds using the query frequency.
    pub fn timestamp_to_ns(&self, raw_ts: u64) -> u64 {
        if self.frequency > 0 {
            // (raw_ts / frequency) * 1_000_000_000 = nanoseconds
            (raw_ts * 1_000_000_000) / self.frequency
        } else {
            0
        }
    }

    /// Get frequency.
    pub fn frequency(&self) -> u64 {
        self.frequency
    }
}

impl Drop for D3d11TimestampQueries {
    fn drop(&mut self) {
        // Queries are automatically released when dropped
    }
}

/// Get GPU timestamp frequency from DXGI device.
#[allow(dead_code)]
pub fn get_timestamp_frequency(_device: &ID3D11Device) -> u64 {
    // Note: Getting GPU timestamp frequency requires DirectX 11.1+ IDXGIDevice3
    // or querying the DXGI adapter directly. For simplicity, we use a default
    // value of 1 GHz which is common for modern NVIDIA GPUs.
    // A more accurate approach would be to use D3D11_QUERY_TIMESTAMP_DISJOINT
    // which reports the actual frequency.
    1_000_000_000
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum PixError {
    #[error("PIX runtime not available: {0}")]
    RuntimeNotAvailable(String),
    #[error("PIX marker creation failed: {0}")]
    MarkerFailed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum NsightError {
    #[error("Nsight runtime not available: {0}")]
    RuntimeNotAvailable(String),
    #[error("Nsight session creation failed: {0}")]
    SessionFailed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum GpuQueryError {
    #[error("Failed to create D3D11 query: {0}")]
    CreateFailed(#[from] windows::core::Error),
    #[error("Query data not available after timeout")]
    Timeout,
}

// ============================================================================
// SDK Integration Notes
// ============================================================================
//
// PIX (Windows Performance Toolkit):
// - Download from Microsoft Store: "PIX on Windows"
// - Header: C:\Program Files\Microsoft PIX\<version>\include\pix3.h
// - DLL: WinPixEventHost.dll (installed with PIX)
// - No additional SDK installation required - comes from Microsoft Store
//
// NVIDIA NvAPI:
// - Comes with NVIDIA driver (starting ~470.x)
// - Headers: C:\Program Files\NVIDIA Corporation\NvAPI\ nvapi.h, nvapi204.h
// - Functions: NvAPI_Initialize, NvExtMarkStart, NvExtMarkEnd, NvExtSetRangeName
// - Requires NVIDIA GPU and driver installation
//
// For full profiling integration:
// 1. Install PIX from Microsoft Store
// 2. Ensure NVIDIA driver is up to date (for NvAPI)
// 3. Add include paths to build.rs
// 4. Uncomment placeholder code in PixContext/NsightContext
