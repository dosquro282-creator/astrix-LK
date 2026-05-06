# Roadmap: real separated shared texture path

Goal: make `--device-mode separated` in `capture-benchmark` validate the real cross-device path between the capture D3D11 device and the media/convert D3D11 device.

Today the benchmark creates shared-looking ring textures, but it does not actually open each texture on the media device and does not perform media-side work through the shared resource. That means separated mode can currently report results without testing the real cross-device path.

## Desired behavior

- Device A is the capture device.
- Device B is the media/convert device.
- In separated mode, each captured frame is copied on Device A into a shared ring texture.
- Device B must open and use the corresponding shared texture.
- `ReleaseFrame` must still happen early, before media/convert processing.
- If shared resource opening or media-side usage fails, separated mode must be reported invalid or fail with a non-zero exit.
- No CPU readback is allowed in the hot path.
- No infinite waits are allowed.
- Default single-device mode must keep working.

## Recommended shared path

Implement both paths if possible:

1. Preferred path: NT shared handle plus keyed mutex.
2. Fallback path: legacy shared handle plus keyed mutex.

The local `windows-rs 0.58` bindings expose the required APIs:

- `D3D11_RESOURCE_MISC_SHARED_NTHANDLE`
- `D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`
- `IDXGIResource1::CreateSharedHandle`
- `ID3D11Device1::OpenSharedResource1`
- `IDXGIResource::GetSharedHandle`
- `ID3D11Device::OpenSharedResource`
- `IDXGIKeyedMutex::AcquireSync`
- `IDXGIKeyedMutex::ReleaseSync`

Using keyed mutex even with the NT handle path keeps synchronization explicit and avoids relying on unclear cross-device ordering guarantees.

## Implementation plan

### 1. Extend shared ring structures

Current location:

- `src/dxgi_capture.rs`
- `SharedTextureRing` currently starts near the top of the file.

Replace the current `textures: Vec<ID3D11Texture2D>` model with slots that hold both sides of the resource.

Suggested types:

```rust
enum SharedPath {
    Disabled,
    NtHandleKeyedMutex,
    LegacyKeyedMutex,
    Failed,
}

enum SharedSlotState {
    Free,
    CaptureWritten,
    MediaOpened,
    MediaBusy,
    Dropped,
}

struct SharedRingSlot {
    capture_texture: ID3D11Texture2D,
    shared_handle: HANDLE,
    media_texture: ID3D11Texture2D,
    keyed_mutex_capture: Option<IDXGIKeyedMutex>,
    keyed_mutex_media: Option<IDXGIKeyedMutex>,
    state: SharedSlotState,
    index: u64,
}
```

`SharedTextureRing` should also keep:

- selected `shared_path`;
- `shared_create_handle_us`;
- `shared_open_us`;
- `media_open_failed_count`;
- ring dimensions and format;
- current slot index.

### 2. Create shared textures on the capture device

Current location:

- `SharedTextureRing::new(...)` in `src/dxgi_capture.rs`.

Change the constructor signature so it receives both devices:

```rust
SharedTextureRing::new(
    capture_device: &ID3D11Device,
    media_device: &ID3D11Device,
    size: usize,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> anyhow::Result<Self>
```

Preferred NT path:

- create `ID3D11Texture2D` on the capture device;
- use `D3D11_USAGE_DEFAULT`;
- use bind flags needed by the media path, at minimum `D3D11_BIND_SHADER_RESOURCE`;
- use misc flags `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`;
- cast texture to `IDXGIResource1`;
- call `CreateSharedHandle`;
- cast media device to `ID3D11Device1`;
- call `OpenSharedResource1::<ID3D11Texture2D>(handle)`;
- cast capture/media textures to `IDXGIKeyedMutex`.

Fallback legacy path:

- create texture with `D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`;
- cast texture to `IDXGIResource`;
- call `GetSharedHandle`;
- call `media_device.OpenSharedResource::<ID3D11Texture2D>(handle)`;
- cast capture/media textures to `IDXGIKeyedMutex`.

For both paths:

- measure handle creation in `shared_create_handle_us`;
- measure media open in `shared_open_us`;
- increment `media_open_failed_count` on failed open;
- fail construction if no slot can be opened on the media device.

### 3. Fail fast when separated path is unavailable

Current location:

- `DxgiCapture::new(...)` in `src/dxgi_capture.rs`.

In `DeviceMode::Separated`:

- require `device_setup.media_device`;
- create `SharedTextureRing` with both devices;
- if the ring cannot open media-side textures, return an error:

```text
ERROR: separated mode requested but cross-device shared texture path is not available.
```

Default behavior must not silently fall back to fake separated timing.

Optional later CLI flag:

- `--allow-invalid-shared-skip`

If added, it should mark `separated_path_valid=false` and `shared_path=failed`, not pretend the benchmark is valid.

### 4. Implement the real separated frame flow

Current location:

- `DxgiCapture::acquire_and_release_frame(...)` in `src/dxgi_capture.rs`.

Required order:

1. `AcquireNextFrame`.
2. `GetResource`.
3. Select next shared ring slot.
4. Acquire capture-side keyed mutex.
5. `CopyResource(acquired_texture -> slot.capture_texture)`.
6. Release capture-side keyed mutex to the media key.
7. Drop `desktop_resource`.
8. `ReleaseFrame` immediately.
9. After `ReleaseFrame`, media device acquires media-side keyed mutex.
10. Media device processes `slot.media_texture`.
11. Media device releases keyed mutex back to the capture key.

Suggested keys:

```rust
const KEY_CAPTURE: u64 = 0;
const KEY_MEDIA: u64 = 1;
```

Use bounded waits only.

- Capture-side `AcquireSync(KEY_CAPTURE, small_timeout_ms)`.
- Media-side `AcquireSync(KEY_MEDIA, ready_wait_budget_ms)`.
- Convert microsecond budget to milliseconds with a minimum/rounding policy that is logged.
- If budget is `0`, use non-blocking or near-non-blocking behavior.

On timeout:

- set slot state to `Dropped`;
- increment `shared_busy_drop_count`;
- increment `dropped_gpu_not_ready_count` if appropriate;
- record `shared_sync_wait_us`;
- do not spin forever.

### 5. Ensure media device actually uses the shared texture

Current location:

- the TODO after `ReleaseFrame` in `DxgiCapture::acquire_and_release_frame(...)`.

For `--convert-test copy-only`:

- create or reuse a media-local `ID3D11Texture2D` on the media device;
- call `media_context.CopyResource(media_local_texture, slot.media_texture)`;
- record `convert_submit_us`;
- increment `media_actual_used_count`.

For `--convert-test bgra-to-nv12`:

- open/use `slot.media_texture` as the source texture on the media device;
- create SRV on media device from `slot.media_texture`;
- write into NV12/output texture using the chosen conversion path;
- record `convert_submit_us` and `convert_ready_delay_us`;
- increment `media_actual_used_count`.

If bgra-to-nv12 is too large for the first patch, implement copy-only first and keep bgra-to-nv12 explicitly failing in separated mode until the media-side conversion is real. Do not report `separated_path_valid=true` for a fake NV12 path.

For `--convert-test none`:

- do not silently skip media usage in separated mode;
- either perform a lightweight media validation copy, or split CLI semantics later into:
  - `none-validate-media`;
  - `none-skip-media`.

Default separated `none` should validate media usage.

### 6. Add separated validity metrics

Current locations:

- `src/stats.rs`
- `src/dxgi_capture.rs`

Add or verify these fields:

- `shared_create_handle_us`
- `shared_open_us`
- `shared_sync_wait_us`
- `media_actual_used_count`
- `media_open_failed_count`
- `separated_path_valid`
- `same_adapter_luid`
- `shared_path`

Important current bug to fix:

- `FrameResult` already has optional `shared_open_us`, `shared_sync_wait_us`, and convert fields.
- `run(...)` creates `FrameMetric::success(...)`, but does not copy those optional fields from `FrameResult`.
- Fix this before trusting summary percentiles.

Suggested approach:

- add a helper such as `FrameMetric::dxgi_success(frame_result, ...)`;
- or mutate the returned `FrameMetric` after `FrameMetric::success(...)` is created.

### 7. Print required summary block

Current location:

- `BenchSummary::print(...)` in `src/stats.rs`.

Add a dedicated block:

```text
[separated]
path_valid=true/false
shared_path=nt_handle_keyed_mutex|legacy_keyed_mutex|disabled|failed
media_actual_used_count=...
media_open_failed_count=...
shared_sync_wait_us p95/max=...
shared_busy_drop_count=...
```

Only print `path_valid=true` when:

- separated mode is active;
- at least one shared texture was opened on the media device;
- media device actually used the media-side texture;
- no fatal shared path initialization error occurred.

### 8. Improve device map logging

Current location:

- `DxgiCapture::log_device_map(...)` in `src/dxgi_capture.rs`.

Print:

- capture device pointer;
- capture adapter LUID;
- media device pointer;
- media adapter LUID;
- `same_device_capture_media=false` in separated mode;
- `same_adapter_luid=true/false`;
- selected shared path;
- `OpenSharedResource1` or `OpenSharedResource` status.

If LUIDs differ, print:

```text
WARNING: capture and media devices use different adapter LUIDs; cross-adapter sharing may be slow or unavailable.
```

### 9. Handle invalid separated mode explicitly

Default behavior:

- separated mode must fail if shared open is unavailable.
- it must not emit normal-looking benchmark results.

Required error:

```text
ERROR: separated mode requested but cross-device shared texture path is not available.
Try --device-mode single, or use an explicit invalid-skip flag if intentionally measuring capture-only timing.
```

If an invalid-skip mode is later added:

- summary must show `separated_path_valid=false`;
- summary must show `shared_path=failed` or `disabled`;
- results must be clearly marked invalid.

### 10. Verification commands

Copy-only separated path:

```powershell
cargo run --release -- --backend dxgi --device-mode separated --convert-test copy-only --frames 500
```

BGRA-to-NV12 separated path:

```powershell
cargo run --release -- --backend dxgi --device-mode separated --convert-test bgra-to-nv12 --frames 500
```

Single-device regression check:

```powershell
cargo run --release -- --backend dxgi --device-mode single --convert-test none --frames 500
```

Expected valid separated logs:

```text
[device-map]
same_device_capture_media=false
same_adapter_luid=true
shared_path=nt_handle_keyed_mutex
open_shared_status=ok

[separated]
path_valid=true
shared_path=nt_handle_keyed_mutex
media_actual_used_count=500
media_open_failed_count=0
shared_sync_wait_us p95/max=...
shared_busy_drop_count=...
```

Expected invalid separated behavior:

```text
ERROR: separated mode requested but cross-device shared texture path is not available.
```

The process should return non-zero unless an explicit invalid-skip flag is added and requested.

## Acceptance criteria

- Separated mode really opens shared textures on the media device.
- Media device really performs `copy-only` or `bgra-to-nv12` using `slot.media_texture`.
- `separated_path_valid=true` is printed only after real cross-device media usage.
- If `OpenSharedResource1` or `OpenSharedResource` fails, separated benchmark is not considered valid.
- Early `ReleaseFrame` is preserved.
- No CPU readback exists in the hot path.
- No infinite waits exist.
- Default single mode is not broken.

## Final implementation report checklist

When the implementation is done, report:

1. Which shared path is implemented: NT handle or legacy keyed mutex.
2. Where shared textures are created.
3. Where shared handles are obtained.
4. Where the media device opens resources.
5. How the media device really uses the texture.
6. Which logs prove separated path validity.
7. How to run separated plus copy-only.
8. How to run separated plus bgra-to-nv12.
