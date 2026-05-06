# Capture Benchmark

A minimal Rust benchmark tool for comparing screen capture performance between DXGI Desktop Duplication and Windows Graphics Capture (WGC), with support for testing device architecture (single vs separated), GPU/CPU priority settings, and detailed latency metrics.

## Purpose

This tool helps diagnose screen capture performance issues by providing detailed metrics for both DXGI and WGC capture backends. It's designed to isolate whether capture performance problems originate from:

1. **Desktop Duplication / GPU contention / DWM** - if DXGI benchmark is slow
2. **Astrix pipeline issues** - if DXGI benchmark is fast but Astrix capture is slow
3. **GPU scheduler starvation at 100% load** - if separated device architecture helps

## Building

### Prerequisites

- Rust 1.70+ (stable)
- Windows 10/11 SDK
- Visual Studio with C++ build tools

### Build Command

```bash
cargo build --release
```

## Usage

### Basic Commands

```bash
# Run DXGI benchmark only
cargo run --release -- --backend dxgi --frames 1000 --monitor 0

# Run WGC benchmark only
cargo run --release -- --backend wgc --frames 1000 --monitor 0

# Run both benchmarks sequentially
cargo run --release -- --backend both --frames 1000 --monitor 0
```

### Command Line Arguments

| Argument | Description | Default |
|----------|-------------|---------|
| `--backend` | Capture backend: `dxgi`, `wgc`, or `both` | `dxgi` |
| `--frames` | Number of frames to capture for benchmark | `1000` |
| `--monitor` | Monitor index (0, 1, 2, ...) | `0` |
| `--timeout-ms` | DXGI acquire timeout in milliseconds | `2` |
| `--copy-mode` | `copy` (copy to ring texture) or `none` | `copy` |
| `--warmup` | Number of warmup frames (excluded from stats) | `60` |
| `--csv` | Optional path to save per-frame metrics CSV | none |
| `--flush-each-frame` | Flush GPU after each frame (diagnostic) | `false` |

#### New Parameters for Device/Priority Testing

| Argument | Description | Default |
|----------|-------------|---------|
| `--device-mode` | `single` (one device) or `separated` (capture + media devices) | `single` |
| `--ring-size` | Ring size for shared textures in separated mode | `4` |
| `--convert-test` | `none`, `copy-only`, or `bgra-to-nv12` | `none` |
| `--ready-wait-budget-us` | Ready wait budget in microseconds (0 = no wait) | `0` |
| `--gpu-priority-capture` | GPU thread priority for capture device (`off` or 0-7) | `off` |
| `--gpu-priority-media` | GPU thread priority for media device (`off` or 0-7) | `off` |
| `--cpu-priority` | CPU priority: `off`, `high`, or `realtime` | `off` |
| `--summary-every` | Print summary every N frames (0 = only at end) | `300` |
| `--overlay-mode` | Overlay compatibility test: `off`, `tiny`, `transparent`, or `visible-border` | `off` |

### Environment Variables

All parameters can also be set via environment variables:

- `ASTRIX_BENCH_DEVICE_MODE`
- `ASTRIX_BENCH_RING_SIZE`
- `ASTRIX_BENCH_CONVERT_TEST`
- `ASTRIX_BENCH_READY_WAIT_BUDGET_US`
- `ASTRIX_BENCH_GPU_PRIORITY_CAPTURE`
- `ASTRIX_BENCH_GPU_PRIORITY_MEDIA`
- `ASTRIX_BENCH_CPU_PRIORITY`
- `ASTRIX_BENCH_SUMMARY_EVERY`
- `ASTRIX_BENCH_OVERLAY_MODE`

## Foreground and Overlay Diagnostics

The benchmark logs the active foreground window at startup, every `--summary-every` frames, and whenever a produced-frame gap exceeds 100 ms. Each foreground snapshot includes HWND, PID, process exe, title, window/client rects, monitor info, captured-monitor intersection/coverage, window styles/exstyles, and these flags:

- `foreground_on_captured_monitor`
- `foreground_covers_captured_monitor`
- `foreground_fullscreen_like`
- `foreground_exe_changed`

Use `--overlay-mode` to test whether a topmost, no-activate, click-through overlay changes the game foreground present path, DWM composition, independent flip, or MPO behavior. The overlay is created on the captured monitor and is destroyed when the benchmark exits.

### Overlay Compatibility Test Commands

```bash
# No overlay
cargo run --release -- --device-mode separated --convert-test copy-only --cpu-priority realtime --gpu-priority-capture 7 --gpu-priority-media 7 --ready-wait-budget-us 0 --frames 1000 --summary-every 100 --overlay-mode off

# Tiny 8x8 overlay
cargo run --release -- --device-mode separated --convert-test copy-only --cpu-priority realtime --gpu-priority-capture 7 --gpu-priority-media 7 --ready-wait-budget-us 0 --frames 1000 --summary-every 100 --overlay-mode tiny

# Almost transparent full-monitor overlay
cargo run --release -- --device-mode separated --convert-test copy-only --cpu-priority realtime --gpu-priority-capture 7 --gpu-priority-media 7 --ready-wait-budget-us 0 --frames 1000 --summary-every 100 --overlay-mode transparent

# Visible 1-2 px monitor border overlay
cargo run --release -- --device-mode separated --convert-test copy-only --cpu-priority realtime --gpu-priority-capture 7 --gpu-priority-media 7 --ready-wait-budget-us 0 --frames 1000 --summary-every 100 --overlay-mode visible-border
```

## Test Matrix for Device Architecture Analysis

### Baseline Single

```bash
# Single device, no priority - baseline measurement
cargo run --release -- --device-mode single --cpu-priority off --gpu-priority-capture off --convert-test bgra-to-nv12 --frames 1000
```

### Single with Priority

```bash
# Single device with priority settings
cargo run --release -- --device-mode single --cpu-priority high --gpu-priority-capture 2 --ready-wait-budget-us 0 --convert-test bgra-to-nv12 --frames 1000
```

### Separated No Priority

```bash
# Separated devices, no priority - test device architecture effect
cargo run --release -- --device-mode separated --cpu-priority off --gpu-priority-capture off --ready-wait-budget-us 0 --convert-test bgra-to-nv12 --frames 1000
```

### Separated Soft

```bash
# Separated devices with soft priority
cargo run --release -- --device-mode separated --cpu-priority high --gpu-priority-capture 1 --gpu-priority-media 2 --ready-wait-budget-us 0 --convert-test bgra-to-nv12 --frames 1000
```

### Separated Aggressive

```bash
# Separated devices with aggressive priority
cargo run --release -- --device-mode separated --cpu-priority high --gpu-priority-capture 2 --gpu-priority-media 3 --ready-wait-budget-us 0 --convert-test bgra-to-nv12 --frames 1000
```

## What to Compare at 100% GPU Load

When testing under heavy GPU load (100% utilization), compare these metrics:

### Metrics for Separated Device Effectiveness

| Metric | What to Look For | Why It Matters |
|--------|------------------|----------------|
| `acquire_wait_us p95/max` | Lower is better | Indicates GPU scheduler responsiveness |
| `acquire_to_release_us p95/max` | Should be shorter in separated | Early ReleaseFrame reduces capture hold time |
| `copy_ready_delay_us p95/max` | Lower is better | Indicates GPU memory copy contention |
| `convert_ready_delay_us p95/max` | Lower is better | Convert shader execution time |
| `longest_gap_between_produced_ms` | Lower is better | Frame pacing stability |
| `dropped_gpu_not_ready_count` | Should be lower in separated | Indicates GPU contention resolution |
| `captured_fps / produced_fps` | Higher is better | Overall capture throughput |

### Metrics for GPU Priority Effectiveness

| Metric | What to Look For | Why It Matters |
|--------|------------------|----------------|
| `acquire_wait_us p95` | Should decrease with priority | Capture thread gets GPU time faster |
| `dropped_gpu_not_ready_count` | Should decrease with priority | Prioritized thread completes faster |
| `captured_fps` | Should increase with priority | More frames captured |

### Risk: Shared Texture Sync as New Bottleneck

In separated mode, watch for:
- `shared_open_us p95/max` - Time to open shared texture on media device
- `shared_sync_wait_us p95/max` - Time waiting for keyed mutex synchronization
- `shared_busy_drop_count` - Frames dropped because shared texture was busy

If these metrics are high, the shared texture synchronization may be a bottleneck.

## Examples

```bash
# Quick test with fewer frames
cargo run --release -- --backend dxgi --frames 100 --monitor 0

# Test with no copying (pure acquire/release timing)
cargo run --release -- --backend dxgi --frames 500 --monitor 0 --copy-mode none

# Test separated device architecture
cargo run --release -- --device-mode separated --frames 500

# Test with all new features
cargo run --release -- --device-mode separated --cpu-priority high --gpu-priority-capture 2 --frames 500

# Save detailed metrics to CSV
cargo run --release -- --backend both --frames 1000 --monitor 0 --csv metrics.csv

# Test second monitor
cargo run --release -- --backend dxgi --frames 500 --monitor 1

# Longer timeout for DXGI
cargo run --release -- --backend dxgi --frames 1000 --monitor 0 --timeout-ms 10
```

## Understanding the Metrics

### DXGI Metrics

| Metric | Description |
|--------|-------------|
| `acquire_wait_us` | Time AcquireNextFrame took (including wait) |
| `get_resource_us` | Time to get ID3D11Texture2D from IDXGIResource |
| `copy_submit_us` | Time to submit CopyResource command |
| `acquire_to_release_us` | Time from AcquireNextFrame to ReleaseFrame |
| `release_frame_us` | Time to execute ReleaseFrame |
| `total_capture_stage_us` | Total time for capture stage (acquire to ReleaseFrame) |
| `copy_ready_delay_us` | Time waiting for copy to be GPU-ready |
| `convert_ready_delay_us` | Time waiting for convert shader to complete |
| `held_frame_us` | Time from AcquireNextFrame to ReleaseFrame |
| `accumulated_frames` | Number of frames accumulated since last capture |
| `timeouts` | Number of AcquireNextFrame timeouts |
| `access_lost` | Number of desktop duplication access lost errors |
| `captured_fps` | Successfully captured frames per second |
| `effective_source_fps` | Estimated source FPS based on accumulated frames |
| `percent_time_foreground_on_captured_monitor` | Share of sampled benchmark time where the foreground window was on/intersecting the captured monitor |
| `long_gaps_while_game_foreground` | Produced-frame gaps over 100 ms while the foreground window was on the captured monitor |
| `long_gaps_while_other_foreground` | Produced-frame gaps over 100 ms while another window was foreground |
| `foreground_exe_most_common` | Foreground process name observed for the most sampled time |
| `foreground_title_most_common` | Foreground title observed for the most sampled time |
| `overlay_mode` | Requested overlay compatibility mode |
| `overlay_created` | Whether the compatibility overlay was created successfully |
| `foreground_unchanged` | Whether foreground HWND stayed the same after overlay creation |

### WGC Metrics

| Metric | Description |
|--------|-------------|
| `callback_gap_us` | Time between frame arrivals |
| `copy_us` | Time to copy frame to ring texture |
| `held_frame_us` | Time from frame available to release |
| `captured_fps` | Successfully captured frames per second |

### Percentile Definitions

| Percentile | Meaning |
|------------|---------|
| `avg` | Average value |
| `p50` | 50th percentile (median) |
| `p95` | 95th percentile |
| `p99` | 99th percentile |
| `max` | Maximum value |

## Testing Under Heavy Game Load

1. **Start the benchmark first:**
   ```bash
   cargo run --release -- --backend both --frames 1000 --monitor 0
   ```

2. **Launch a GPU-intensive game** on the same monitor being captured.

3. **Observe the results:**
   - If DXGI captured_fps drops to 3-25 fps under heavy game load → the issue is in Desktop Duplication / GPU contention / DWM
   - If separated mode maintains better FPS → the device separation is helping

4. **Run comparison:**
   ```bash
   # Compare single vs separated mode under game load
   cargo run --release -- --device-mode single --frames 500 --monitor 0 > single.txt
   cargo run --release -- --device-mode separated --frames 500 --monitor 0 > separated.txt
   ```

## CSV Output Format

When `--csv path/to/output.csv` is specified, each row contains:

```
backend,frame_index,timestamp_us,acquire_wait_us,get_resource_us,copy_submit_us,acquire_to_release_us,release_frame_us,total_capture_stage_us,callback_gap_us,copy_us,held_frame_us,copy_ready_delay_us,shared_open_us,shared_sync_wait_us,convert_submit_us,convert_ready_delay_us,accumulated_frames,timeout,error,warmup,dropped,dropped_reason
```

- `acquire_wait_us`: DXGI only, null for WGC
- `callback_gap_us`: WGC only, null for DXGI
- `warmup`: `true` if this frame was during warmup phase

## Diagnostic Checklist

Use this checklist when diagnosing capture performance:

### Before Game (Baseline)
- [ ] DXGI p95 acquire_wait_us < 5000µs
- [ ] DXGI p95 acquire_to_release_us < 10000µs
- [ ] DXGI captured_fps ~ monitor refresh rate (e.g., 60/120/144)

### Under Heavy Game (Single Mode)
- [ ] DXGI captured_fps still high (>50% of refresh rate)
- [ ] No significant increase in timeouts
- [ ] No access_lost errors

### Under Heavy Game (Separated Mode)
- [ ] Separated mode shows better metrics than single mode
- [ ] acquire_to_release_us is shorter in separated mode
- [ ] dropped_gpu_not_ready_count is lower in separated mode

### If DXGI Benchmark is Good but Astrix is Slow
- [ ] Check keyed mutex contention
- [ ] Check ReleaseFrame timing (too late?)
- [ ] Check downstream processing queue
- [ ] Check if encoder is blocking capture thread

## Troubleshooting

### "Monitor index X not found"
- Check available monitors by running without arguments
- Try different monitor index values

### "Access lost" errors
- Desktop duplication can fail when exclusive fullscreen apps take over
- Try increasing `--timeout-ms`

### Low FPS with no errors
- This typically indicates GPU contention
- Try `--copy-mode none` to isolate copy overhead
- Check if other GPU applications are running
- Try `--device-mode separated` to test if device separation helps

### High acquire_wait_us at 100% GPU load
- This indicates GPU scheduler starvation
- Try `--device-mode separated` to isolate capture from other GPU work
- Try `--gpu-priority-capture 2` to prioritize capture thread

## Architecture

```
src/
├── main.rs          # Entry point, benchmark orchestration
├── cli.rs           # Command-line argument parsing
├── stats.rs         # Statistics computation, CSV export
├── d3d.rs           # D3D11 device setup, adapter enumeration
├── dxgi_capture.rs # DXGI Desktop Duplication implementation
└── wgc_capture.rs  # Windows Graphics Capture implementation
```

## Design Constraints

This benchmark intentionally excludes:
- NVENC encoding (to isolate capture from encoding)
- CPU readback (GPU-only operations)
- UI frameworks (console-only output)
- Network transmission
- Scaling or format conversion (unless --convert-test is used)
- astrix/LiveKit/WebRTC integration

The goal is to measure pure capture performance without downstream processing interference.

## Real WGC Benchmark

The WGC backend provides real Windows Graphics Capture implementation:

```bash
# Real WGC benchmark
cargo run --release -- --backend wgc --frames 1000 --monitor 0 --copy-mode copy

# Compare WGC vs DXGI
cargo run --release -- --backend both --frames 1000 --monitor 0
```

### How to Test

1. Run the benchmark without any heavy applications:
   ```bash
   cargo run --release -- --backend both --frames 1000 --monitor 0
   ```

2. Note the baseline metrics:
   - DXGI captured_fps (typically 60-144 fps for static desktop)
   - WGC captured_fps (typically matches monitor refresh rate)
   - callback_gap_us p95/p99
   - held_frame_us p95

3. Start a GPU-intensive game on monitor 0.

4. Run the benchmark again while the game is running:
   ```bash
   cargo run --release -- --backend both --frames 1000 --monitor 0
   ```

5. Compare the results.

### What to Look For

| Scenario | DXGI Behavior | WGC Behavior | Implication |
|----------|---------------|--------------|-------------|
| Idle desktop | 60-144 fps | 60-144 fps | Normal |
| Heavy game (GPU bound) | 3-30 fps, high access_lost | Usually maintains 60+ fps | WGC more resilient |
| Both degrade | Both slow | Both slow | GPU contention / DWM issue |

### Comparison Interpretation

If **DXGI** gives 1-30 fps while **WGC** maintains 60/120/165 fps under game load, WGC may be a better choice for screen sharing in Astrix.

If **both** degrade similarly, the issue is deeper:
- GPU contention between game and compositor
- Desktop Window Manager (DWM) synchronization
- Monitor/GPU driver configuration
- Fullscreen exclusive mode in the game

### WGC Implementation Details

The WGC backend:
- Uses `IGraphicsCaptureItemInterop::CreateForMonitor` for programmatic capture (no picker UI)
- Creates `Direct3D11CaptureFramePool` with 2 buffers
- Polls `TryGetNextFrame()` for frame availability
- Converts `IDirect3DSurface` to `ID3D11Texture2D` via `IDirect3DDxgiInterfaceAccess`
- Copies frames to ring textures when `--copy-mode copy`
- Reports real callback gaps between frame arrivals

## Priority Settings Reference

### GPU Thread Priority (IDXGIDevice::SetGPUThreadPriority)

| Value | Meaning |
|-------|---------|
| `off` | Don't change GPU thread priority |
| `0` | Normal priority (default) |
| `1-3` | Elevated priority (recommended for capture) |
| `4-7` | High priority (use with caution) |

### CPU Process/Thread Priority

| Mode | Process Priority | Thread Priority |
|------|------------------|------------------|
| `off` | Normal | Normal |
| `high` | HIGH_PRIORITY_CLASS | THREAD_PRIORITY_HIGHEST |
| `realtime` | HIGH_PRIORITY_CLASS | THREAD_PRIORITY_HIGHEST (WARNING: experimental) |

**Warning**: Realtime priority can cause system instability if the benchmark thread blocks. Use `high` instead.
