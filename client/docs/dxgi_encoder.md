# DXGI + MFT Debug Context

## Goal

Need a stable Windows screen-share pipeline for full-monitor capture with:

- capture backend: `DXGI Desktop Duplication`
- encode path: existing `MFT` GPU H.264 path
- decode path: existing `MFT` GPU decode path
- fixed cadence by preset: `30 / 60 / 90 / 120 fps`

User explicitly does **not** want to pivot to `xcap` or back to `WGC` as the main solution. `DXGI` should be fixed.

## User Environment

- Workspace: `C:\MyProjects\Astrix LK\astrix-LK`
- Sender and viewer clients are launched on the **same PC**
- GPU: `NVIDIA GeForce RTX 3080`
- Capture mode in product: full monitor only, not per-window/app capture
- Problem reproduced even on desktop capture by moving Explorer windows

## Main Symptoms Seen Over Time

1. Original problem:
- viewer periodically lags behind
- stream then catches up with a "rubber-band" effect
- startup may take 20-30 seconds to reduce delay

2. With game content:
- sender logs sometimes showed very low source FPS
- viewer logs showed long `STALL` / `LONG_STALL`

3. After introducing `DXGI`:
- `DXGI + MFT` eventually became able to initialize and send first frames
- but low viewer FPS / lag remained
- later regressions caused startup loops / no frames
- current state is better again: startup works, first encoded frame is delivered, but viewer steady-state FPS can still sit around `10-15`

## Important Conclusions Reached

### 1. Rare IDR was not the main root cause

Periodic IDR could amplify bursts, but logs repeatedly showed sender-side cadence issues and/or fallback to slow paths. The dominant issue was not "IDR too rare".

### 2. WGC was unreliable for heavy gaming scenes

Earlier logs showed `WGC on_frame_arrived rate` or later `WGC rate` collapsing on heavy scenes even when in-game FPS was high.

This motivated trying `DXGI Desktop Duplication` while preserving the GPU encode/decode path.

### 3. `xcap` was not a valid final solution

`xcap` could capture, but at `1440p90` it went through `OpenH264`, which skipped frames heavily and could not sustain the target. So `xcap` was treated only as a diagnostic comparison, not a final backend.

### 4. The low-FPS logs on sender identified fallback

When sender logs contained:

- `capture_frame rate: ...`
- `gpu perf @... BOTTLENECK`

that meant the stream was no longer on the intended `DXGI + MFT` path. It had fallen into the expensive `GPU I420 / OpenH264` path in `voice_livekit.rs`.

That path is far too slow for `1440p90` and explains viewer FPS collapsing to ~5-20.

### 5. Current state after async MFT fix

Async MFT event handling was later corrected so that:

- `METransformNeedInput` and `METransformHaveOutput` are both buffered correctly
- `ProcessOutput` is not called blindly after `MF_E_NOTACCEPTING`
- transient async MFT stalls reset the encoder path instead of immediately forcing slow fallback

With that in place, `DXGI + MFT` can now initialize, deliver the first encoded frame, and the viewer can reach hardware decode plus GPU zero-copy.

The remaining blocker is no longer startup itself. The current issue is low steady-state FPS somewhere between:

- sender `push_frame`
- WebRTC / receive cadence
- viewer convert / render

### 6. Historical startup failure on the `DXGI + MFT` path

Most recent sender logs show:

- `MFT startup: encode() returned 0 frame(s) in 77679 us`
- `MFT startup: input accepted, still waiting for first encoded frame`
- then repeated:
  `MFT encode failed: Encode("ProcessOutput failed: Error { code: HRESULT(0x8000FFFF), message: "Разрушительный сбой" }")`

This means the current blocker is a `ProcessOutput` catastrophic failure in the NVIDIA H.264 MFT after successful init and initial input acceptance.

## Key Logs To Remember

### Latest sender log shape

Relevant recent log excerpt:

```text
[voice][screen] capture backend override: dxgi
[voice][screen] encode path: MFT GPU (push_frame → ExternalH264Encoder → RTP)
[voice][screen] DXGI first frame copied into latest-slot
[d3d11_nv12] VP path OK: input=2560x1440 -> output=2560x1440 (first frame logged)
[voice][screen] MFT startup: calling encode() key_frame=true ts_us=0
[voice][screen] MFT startup: encode() returned 0 frame(s) in 77679 us
[voice][screen] MFT startup: input accepted, still waiting for first encoded frame
[voice][screen] MFT startup keyframe: first 3 frames as IDR
[voice][screen] MFT encode failed: Encode("ProcessOutput failed: Error { code: HRESULT(0x8000FFFF), message: \"Разрушительный сбой\" }")
[voice][screen] DXGI+MFT: retrying encoder path after reset
```

Then the same loop repeats.

### Newer sender log shape after the async MFT fix

```text
[voice][screen] capture backend override: dxgi
[voice][screen] encode path: MFT GPU (push_frame → ExternalH264Encoder → RTP)
[voice][screen] MFT async steady-state: pipelined submit+collect mode
[voice][screen] MFT startup: calling encode() key_frame=true ts_us=0
[voice][screen] MFT startup: encode() returned 0 frame(s) in 219933 us
[voice][screen] MFT startup: input accepted, still waiting for first encoded frame
[voice][screen] MFT startup keyframe: first 3 frames as IDR
[EncodedVideoTrackSource] first encoded frame delivered (105550 bytes, keyframe=1)
```

This confirms startup is no longer the primary failure point.

### Newer viewer log shape after the async MFT fix

```text
[voice][screen][viewer] receive task started, waiting for first frame...
[MFT decoder] Hardware D3D11 path: ENABLED
[MFT decoder] Phase 3 hw frame #0 2560x1440 ... path=kNative
[voice][screen][viewer] first raw frame 2560x1440
[voice][screen][viewer] first frame converted 2560x1440 path=GPU zero-copy (WGL)
```

This confirms the viewer is also on the intended GPU decode/display path.

### Earlier sender log proving fallback path

```text
[voice][screen] capture_frame rate: 12.7 fps (target 90)
[voice][screen] gpu perf @1560 frames: dispatch=36035µs copy=1634µs map=26543µs total=68182µs / 11111µs budget (613% ← BOTTLENECK)
```

This was interpreted as proof that sender had already dropped into the slow GPU-I420/OpenH264 path.

### Earlier viewer lag pattern

Typical viewer logs during bad state:

```text
[voice][screen][viewer] STALL recv_wait_ms=338 expected_us=338000 network_us=196 ts_delta=338000
[voice][screen][viewer] STALL recv_wait_ms=341 expected_us=342000 network_us=0 ts_delta=342000
```

This indicated sender cadence itself had degraded badly, not just the network.

## Important Code Areas

### `client/src/voice_livekit.rs`

Main file controlling:

- backend selection (`WGC` / `DXGI` / `xcap`)
- sender loop
- `DXGI + MFT` path
- fallback to native/I420 path
- adaptive FPS logic

Important observations:

- `capture_frame rate: ...` and `gpu perf @...` logs belong to the fallback GPU-I420/OpenH264 path, not the true MFT path
- `source-adaptive` logic exists here and was later disabled for `DXGI` lock-to-preset mode
- `DXGI` build marker was recently added near backend selection:
  `"[voice][screen] DXGI build marker: hardfail-fallback-v2"`
- newer steady-state diagnostics were added here:
  - `"[voice][screen] push_frame rate: ..."`
  - `"[voice][screen][viewer] recv rate: ..."`
  - `"[voice][screen][viewer] convert rate: ..."`
- async MFT pipelining is now the default steady-state mode; `ASTRIX_MFT_PIPELINED=0` forces the old blocking mode for comparison

If this line is absent in logs, the user is likely running an old binary.

### `client/src/mft_encoder.rs`

Handles `MftH264Encoder`.

Recent work included:

- reduced async wait durations
- changed `MF_E_NOTACCEPTING` handling
- added faster drain/retry logic

Current hard blocker appears later than that: `ProcessOutput failed 0x8000FFFF`.

### `client/src/dxgi_duplication.rs`

Contains the Desktop Duplication backend.

### `client/src/gpu_device.rs`

Recent important fix:

- enabled `ID3D11Multithread::SetMultithreadProtected(true)`

This was necessary and at one point helped `DXGI` start updating properly.

## Notable Implemented Changes So Far

The following were already implemented in code before this handoff:

1. Real sender capture timestamps
- sender no longer always used only synthetic timing for outgoing capture timestamps

2. Source-adaptive sender cap
- initially added to follow source cadence more closely
- later user requested not to keep downshifting too aggressively

3. Hysteresis for adaptive source cap
- added thresholding / confirmations to avoid ceiling oscillation

4. `CreateFreeThreaded` WGC attempt
- tried to improve callback stability

5. `DXGI Desktop Duplication` backend
- added and wired into `voice_livekit.rs`

6. Shared-adapter device creation
- `create_for_adapter_idx` in `gpu_device.rs`

7. `DXGI -> BGRA -> NV12 -> MFT`
- integrated with existing GPU pipeline

8. `ID3D11Multithread` protection on device/context
- critical fix that helped DXGI behavior

9. Async MFT startup / collect changes
- multiple iterations to make NVENC startup work

10. Prevent immediate fallback on transient `NeedInput` timeout
- turned into soft drop / retry behavior

11. `D3D11 I420` SRV format fix
- fixed `CreateShaderResourceView(0x80070057)` in fallback converter

12. Lock preset FPS for DXGI
- added so `DXGI` path should respect preset FPS and not downshift itself

13. Attempt to keep `DXGI + MFT` from falling into slow GPU-I420 path
- added logic to avoid the expensive fallback path during transient MFT issues

14. Added build marker
- `DXGI build marker: hardfail-fallback-v2`

## Current State Of The Code

At the moment, there were recent edits in `voice_livekit.rs` intended to:

- lock `DXGI` path to preset FPS
- keep `DXGI + MFT` from dropping into slow fallback on transient MFT issues
- allow fallback on hard failures instead of infinite retry

However, the latest user-provided log still showed:

- repeated `DXGI+MFT: retrying encoder path after reset`
- and it did **not** show the build marker

That strongly suggests the user was probably running an older binary, not the latest build.

## What The Next Agent Should Check First

1. Confirm the user is running the newest binary
- sender log must contain:
  `"[voice][screen] DXGI build marker: hardfail-fallback-v2"`

2. If the build marker is missing
- stop debugging logic
- first resolve how the client is being built/launched
- likely wrong exe or stale binary is being run

3. If the build marker is present and issue persists
- investigate `ProcessOutput failed: 0x8000FFFF`
- likely in `MftH264Encoder::encode` / `drain_output_once` / `ProcessOutput` handling
- likely not a network problem

4. Check whether `DXGI+MFT` still falls into slow fallback
- if sender shows `capture_frame rate` or `gpu perf @...`, fallback path is active
- if not, focus only on MFT failure and cadence

## Most Likely Next Technical Targets

If fresh binary is confirmed and failure persists, next debugging should focus on:

1. Why first/second `ProcessOutput` on NVIDIA H.264 MFT fails with `0x8000FFFF`
- inspect `mft_encoder.rs`
- inspect async/sync interaction after initial zero-frame `encode()`
- verify whether the transform wants a different startup handling pattern

2. Whether startup should use a stricter direct `ProcessInput/ProcessOutput` loop
- especially for the first keyframe(s)

3. Whether `DXGI` cadence collapses only because of the MFT reset loop
- current repeated re-init is likely making `DXGI rate` look artificially low

4. Only after startup is stable, return to:
- low viewer FPS
- lag / rubber-band
- final fixed preset cadence behavior

## Notes For Continuation

- User wants to continue in a new agent window
- User explicitly wants the solution to stay on `DXGI + MFT`
- User does not want the main answer to be "switch backend"
- Recent `cargo check -q` was passing, with many unrelated warnings already present in repo
