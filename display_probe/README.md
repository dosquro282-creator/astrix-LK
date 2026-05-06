# display_probe

`display_probe` is a small Windows-only Rust utility for inspecting DXGI outputs and the hardware composition / MPO capability flags reported by `IDXGIOutput6`.

It does not capture frames and does not use Windows Graphics Capture or DXGI Desktop Duplication. The tool only creates a DXGI factory, enumerates adapters/outputs, queries `IDXGIOutput6`, calls `GetDesc1`, and calls `CheckHardwareCompositionSupport`.

## Build

Requirements:

- Windows 10/11
- Rust stable
- Windows SDK / Visual Studio Build Tools

```powershell
cd display_probe
cargo build --release
```

## Run

Probe every DXGI output:

```powershell
cargo run --release
```

Probe one monitor by global output index:

```powershell
cargo run --release -- --monitor 0
```

Print only machine-readable JSON:

```powershell
cargo run --release -- --monitor 0 --json-only
```

Print only the readable summary:

```powershell
cargo run --release -- --summary-only
```

Watch the foreground window and the DXGI output under its window center:

```powershell
cargo run --release -- --watch --interval-ms 500
```

Write one JSON object per line, suitable for logging while a capture benchmark or game is running:

```powershell
cargo run --release -- --watch --json-lines --interval-ms 250 --duration-sec 120 > display_probe_watch.jsonl
```

The JSON contains:

- adapter name, vendor id, device id
- output/device name
- desktop coordinates
- `attached_to_desktop`
- rotation
- `IDXGIOutput6` availability
- `bits_per_color`, `color_space`, primaries and luminance values from `IDXGIOutput6::GetDesc1`
- raw `CheckHardwareCompositionSupport` flags and decoded:
  - `DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_FULLSCREEN`
  - `DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_WINDOWED`
  - `DXGI_HARDWARE_COMPOSITION_SUPPORT_FLAG_CURSOR_STRETCHED`

In `--watch` mode each sample contains:

- local timestamp and Unix timestamp in milliseconds
- foreground window title
- foreground process name, path, and pid
- foreground window rect and center point
- monitor/output matched from the foreground window center
- adapter/output name, desktop rect, `GetDesc1` fields, and hardware composition flags

## Checking Dota 2 With PresentMon

Use `display_probe` first to confirm which monitor/output has hardware composition support:

```powershell
cd display_probe
cargo run --release -- --monitor 0
```

Then run Dota 2 on that same monitor and collect PresentMon data in parallel. The exact PresentMon command depends on the version you use, but the useful pattern is:

```powershell
PresentMon.exe -process_name dota2.exe -output_file dota2_presentmon.csv
```

For live correlation, keep `display_probe` running in JSON Lines mode while PresentMon records Dota 2:

```powershell
cargo run --release -- --watch --json-lines --interval-ms 250 --duration-sec 180 > display_probe_watch.jsonl
```

In another terminal:

```powershell
PresentMon.exe -process_name dota2.exe -output_file dota2_presentmon.csv
```

Start both before entering a match or test scene, keep Dota 2 focused, then stop after you have captured a representative sample.

Compare:

- PresentMon `PresentMode`: whether Dota 2 is in `Independent Flip`, `Hardware Composed Independent Flip`, or `Composed Flip`.
- WGC/DXGI capture FPS and latency from your benchmark/client logs.
- `display_probe_watch.jsonl`: whether the foreground window was actually Dota 2, which monitor its center was on, and what `CheckHardwareCompositionSupport` reported for that output at the same time.

The useful pattern is to line up timestamps and look for transitions: for example, Dota 2 moving from `Independent Flip` to `Composed Flip` at the same time WGC/DXGI FPS drops, or the foreground center moving to another monitor/output with different hardware composition flags.

## Important Present Modes

These modes are the most useful when diagnosing latency and MPO / hardware composition behavior:

- `Independent Flip`: the app can flip independently of normal DWM composition. This is usually the lowest-latency borderless/fullscreen path.
- `Hardware Composed Independent Flip`: the app still uses an independent flip path, but the final scanout is hardware-composed, often through an overlay plane / MPO path.
- `Composed Flip`: the app is composed by DWM. This can be normal for windowed cases, overlays, capture, HDR/color-management changes, or when independent flip/MPO is not available.

`CheckHardwareCompositionSupport` reports capability flags for the output. It does not prove that Dota 2 is currently using a specific present mode. PresentMon is the runtime confirmation.

`display_probe` intentionally does not capture the screen, does not use WGC, does not use DXGI Desktop Duplication, and does not take CPU screenshots.
