# NVENC D3D11 Plan

## Goal

Replace hardware MFT as the primary H.264 sender backend on NVIDIA with a direct
D3D11 + NVENC path, while keeping:

- `MFT H.264` as the encoded fallback path
- `OpenH264` as the final native/I420 fallback path
- the existing `NativeEncodedVideoSource -> push_encoded_frame -> RTP` path

## Target Pipeline

```text
DXGI / WGC
-> D3D11 BGRA
-> D3D11 GPU convert to NV12
-> ring of 3-4 NV12 textures
-> one encode worker (NvEncEncodePicture submit)
-> one output worker (event wait + lock bitstream)
-> NativeEncodedVideoSource::push_frame
-> WebRTC RTP
```

## Non-Goals For Phase 1

- No D3D12 encode path
- No AMD AMF / Intel QSV implementation yet
- No removal of MFT fallback
- No republish logic changes unless we fall all the way back to OpenH264

## Architecture

### Backends

Introduce a shared encoded backend layer:

- `NvencD3d11Encoder` for NVIDIA primary path
- `MftH264Encoder` as encoded fallback
- existing `NativeVideoSource + OpenH264` path as final fallback

This keeps `voice_livekit.rs` talking to one backend interface instead of hardcoding MFT.

### Backend Selection

Runtime policy:

1. Detect adapter vendor from the active D3D11 device
2. If NVIDIA:
   try NVENC D3D11
3. On NVENC init/runtime failure:
   fall back to MFT encoded path
4. If MFT encoded path hard-fails:
   fall back to native/OpenH264 path
5. If adapter is AMD/Intel:
   skip NVENC and go directly to MFT

### Threading

- Capture thread stays cadence owner
- Encode worker owns NVENC session + registered input textures
- Output worker waits on completion events and extracts bitstream
- Metadata queue keeps `rtp_ts`, `capture_us`, `submit_time`, `keyframe`

### Memory / Resources

- Reuse existing `D3d11BgraToNv12`
- Register 3-4 NV12 textures with NVENC once per session
- Keep a matching pool of output bitstream buffers + completion events
- Avoid per-frame register/unregister churn

## Implementation Phases

### Phase 1: Scaffolding

- Create `nvenc_d11.rs`
- Add adapter vendor detection and NVENC runtime probe
- Add shared encoded backend wrapper around NVENC/MFT
- Route sender init through wrapper without changing RTP path

Status: completed

### Phase 2: NVENC Session Init

- Dynamically load `nvEncodeAPI64.dll`
- Create API function table
- Open encode session from D3D11 device
- Query codec/preset support
- Configure low-latency H.264 session

Status: completed and runtime-validated on RTX 3080

### Phase 3: Input / Output Queues

- Create input texture ring
- Register D3D11 NV12 textures with NVENC
- Create output bitstream buffers and completion events
- Add encode worker submit path
- Add output worker drain path

Status: completed in current branch

### Phase 4: Sender Integration

- Replace MFT steady-state encode path with NVENC path on NVIDIA
- Preserve timestamp/capture metadata flow
- Keep static-frame re-encode behavior
- Keep current RTP timestamp discipline

Status: started in current branch

### Phase 5: Failure Handling

- Device-lost / session reset handling
- Timed retries for transient NVENC failures
- Automatic fallback NVENC -> MFT -> OpenH264
- Clear stats/logging for active backend

### Phase 6: Tuning

- Low-latency preset/profile selection
- CBR / VBV tuning for screen content
- Forced IDR / intra-refresh policy
- Ring depth and worker timing validation at 60/90/120 fps

## Files To Touch

- `client/src/nvenc_d11.rs`
- `client/src/encoded_h264.rs`
- `client/src/voice_livekit.rs`
- `client/src/voice.rs`
- `client/src/gpu_device.rs`
- `client/src/lib.rs`
- `client/Cargo.toml`

## First Execution Slice

This slice should land first:

1. Backend abstraction for encoded H.264
2. NVIDIA adapter detection from active D3D11 device
3. NVENC runtime probe via `nvEncodeAPI64.dll`
4. `auto` selection path prepared for `NVENC -> MFT`
5. Current behavior preserved: if NVENC is not ready, MFT still works

## Risks

- NVENC SDK headers/import layer are not yet in the repo as Rust bindings
- Dynamic loading must stay optional and non-fatal on AMD/Intel
- Current sender loop has MFT-specific logs and fallback state; migrate carefully
- Static-frame resend currently reads the converter's last output texture directly; NVENC path must preserve an equivalent "last submitted frame" story

## Success Criteria

Current branch is successful when:

- project builds with the new backend wrapper
- active adapter vendor is detected correctly
- `NVENC D3D11` session/init compiles through a real bridge instead of a stub
- NV12 ring textures are registered once per session and used by submit/collect
- current MFT path still builds unchanged as the fallback backend

## Validation Notes

- `2026-03-27`: `client/examples/nvenc_d11_smoke.rs` validated the direct
  D3D11 -> NV12 -> NVENC path on `NVIDIA GeForce RTX 3080`
- Direct `NvencD3d11Encoder` init succeeded with `async=true`
- Direct encode produced 12 Annex B H.264 frames and reported key frames
- `EncodedH264Encoder::new_auto(...)` selected `NvencD3d11`
- Important gotcha: initial `128x128` smoke test failed with
  `NV_ENC_ERR_INVALID_PARAM`; on Turing/Ampere H.264 NVENC requires width
  `>= 145`, so the smoke test now uses `256x144`
- `2026-03-27`: Phase 3 queueing was validated with a background output worker
  in the C++ bridge
- Output completion-event wait and bitstream lock/unlock now run off the sender
  thread inside `client/src/nvenc_d11_bridge.cpp`
- Rust-side submit backpressure now keys off real NVENC in-flight depth instead
  of the local metadata queue length
- `2026-03-28`: Phase 4 sender wrapper now supports runtime `NVENC D3D11 -> MFT`
  fallback inside `client/src/encoded_h264.rs`
- This keeps the encoded RTP path alive after a mid-session NVENC failure instead
  of immediately falling through to CPU/OpenH264
- `cargo check --features wgc-capture` still passes and
  `client/examples/nvenc_d11_smoke.rs` still selects `NvencD3d11` on RTX 3080
- `2026-03-28`: sender stability pass landed after real 1440p90 testing
- NVENC bitrate reconfigure no longer resets the encoder on every BWE tick
- `ExternalH264Encoder` now disables WebRTC scaling/quality-scaler hints
- NVENC queue-full/backpressure is now treated as transient first, not an
  immediate runtime fallback to MFT
- Pipelined encoded path now gives NVENC a wider submit timeout window,
  especially around forced/periodic IDRs
- `2026-03-28`: high-FPS receiver freeze mitigation landed for `P1440F90`
- The preset bitrate cap was reduced from `60 Mbps` to `35 Mbps`
- Default periodic IDR interval is now `30s` on NVENC unless
  `ASTRIX_PERIODIC_IDR_SECS` explicitly overrides it
