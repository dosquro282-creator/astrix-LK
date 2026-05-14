# capture_fps_service_probe

Minimal Windows-only Rust probe for checking whether a capture loop behaves better when it runs in a separate lightweight worker process.

This is intentionally **not** a real Windows Service yet. DXGI Desktop Duplication and Windows Graphics Capture need access to the interactive user's desktop. A normal Windows Service usually runs in Session 0, and Session 0 does not have normal access to the logged-in user's interactive desktop. For screen capture, the first useful test is therefore a worker process started inside the user session, not a system service managed by SCM.

## Build

```powershell
cargo build --release
```

The binary is:

```text
target\release\capture_fps_service_probe.exe
```

## Commands

```powershell
capture_fps_service_probe.exe --backend dxgi --monitor 0
capture_fps_service_probe.exe --backend wgc --monitor 0
capture_fps_service_probe.exe --backend dxgi --monitor 0 --service-like
capture_fps_service_probe.exe --backend dxgi --monitor 0 --spawn-worker
```

Useful knobs:

```text
--backend dxgi|wgc
--monitor N
--duration-sec N
--timeout-ms N
--interval-sec N
--service-like
--no-stdout
--spawn-worker
--allow-copy
--ring-size N
--wait-copy-ready
--simulate-encode-delay-ms N
--latest-only
--cpu-priority off|high|realtime
--gpu-priority off|0..7
```

`--service-like` still runs in the interactive user session. It creates no UI, no overlay, no windows, and writes FPS to `capture_fps_service_probe.log` next to the exe. If stdout is attached to a terminal, it also prints there.

`--no-stdout` disables stdout output. Use it with `--service-like` when the worker should write only to `capture_fps_service_probe.log`.

`--spawn-worker` starts a child copy of the same exe with the same arguments plus `--service-like`, then waits until the worker exits. The worker writes FPS to the log file. This models how Astrix can later start a capture worker as a separate user-session process.

## Output

Every interval, the probe emits exactly one FPS line:

```text
[start] backend=dxgi monitor=0 allow_copy=true wait_copy_ready=false latest_only=true ring_size=4
[fps] ts=2026-05-06T12:34:56.789 backend=dxgi monitor=0 frames=XX mode=acquire_only
[fps] ts=2026-05-06T12:34:56.789 backend=dxgi monitor=0 frames=XX mode=acquire_latest
[fps] ts=2026-05-06T12:34:56.789 backend=dxgi monitor=0 frames=XX mode=copy
[fps] ts=2026-05-06T12:34:56.789 backend=dxgi monitor=0 frames=XX mode=copy_latest
[fps] ts=2026-05-06T12:34:56.789 backend=dxgi monitor=0 frames=XX mode=copy_wait
[fps] ts=2026-05-06T12:34:56.789 backend=dxgi monitor=0 frames=XX mode=copy_wait_latest
```

`frames` is the number of frames acquired during the previous interval.

## What It Does

DXGI path:

- creates a D3D11 device on the adapter for the selected monitor;
- opens `IDXGIOutputDuplication`;
- loops on `AcquireNextFrame(timeout_ms)`;
- increments the frame counter for each acquired frame;
- immediately releases the frame;
- by default does not copy, map, read back, encode, create shared textures, or use NVENC;
- with `--allow-copy`, copies the acquired texture into a local GPU texture ring and does not wait for completion unless `--wait-copy-ready` is also set;
- treats `DXGI_ERROR_WAIT_TIMEOUT` as idle/no-frame;
- recreates duplication on `DXGI_ERROR_ACCESS_LOST`.

WGC path:

- creates a monitor `GraphicsCaptureItem` for the selected monitor;
- creates a D3D11/WinRT device on the monitor adapter;
- uses a free-threaded `Direct3D11CaptureFramePool`;
- drains all available frames through `TryGetNextFrame`;
- increments the frame counter for each frame;
- immediately drops each frame;
- by default does not copy, map, read back, encode, create shared textures, or use NVENC;
- with `--allow-copy`, copies the frame texture into a local GPU texture ring and does not wait for completion unless `--wait-copy-ready` is also set;
- attempts to disable cursor capture and the WGC border where the OS/API permits it.

## Test Scenario

Test in this order:

1. acquire only:

```powershell
capture_fps_service_probe.exe --backend dxgi --monitor 0 --service-like --no-stdout
```

2. copy without waiting:

```powershell
capture_fps_service_probe.exe --backend dxgi --monitor 0 --service-like --no-stdout --allow-copy
```

3. copy with GPU completion wait:

```powershell
capture_fps_service_probe.exe --backend dxgi --monitor 0 --service-like --no-stdout --allow-copy --wait-copy-ready
```

4. latest-only:

```powershell
capture_fps_service_probe.exe --backend dxgi --monitor 0 --service-like --no-stdout --allow-copy --latest-only
```

Run each mode through:

a) desktop idle

b) Dota active foreground

c) Dota running but not foreground

d) Dota with FPS cap

Compare only `frames` per second.

To check whether logging continued while the game was active, run the worker in service-like mode and follow the log:

```powershell
capture_fps_service_probe.exe --backend dxgi --monitor 0 --service-like --no-stdout
Get-Content .\capture_fps_service_probe.log -Wait
```

While Dota is active foreground, the `ts=` values should keep advancing at the configured interval. If timestamps stop or show large gaps only during active gameplay, capture/logging stalled during that period. If timestamps continue but `frames` drops, the capture loop is alive but receiving fewer frames.

Criteria:

- `acquire_only` stable and `copy` stable: the issue is not DXGI and not `CopyResource`;
- `acquire_only` stable but `copy` drops: likely GPU copy, ring, or device contention;
- `copy` stable but `copy_wait` drops: do not wait for GPU completion in the capture loop;
- `copy` stable but Astrix drops: the issue is downstream, such as convert, encode, send, or thread scheduling.
