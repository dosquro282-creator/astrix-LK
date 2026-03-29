# Denoise Plan

## Goals

- Add sender-side microphone denoise using ordinary DeepFilterNet.
- Add receiver-side denoise as a local, per-user toggle only.
- Keep the existing LiveKit/WebRTC voice path stable and low-latency.
- Avoid doing heavy DSP work inside `cpal` input callbacks.

## Current Voice Pipeline

### Sender

1. `cpal` input callback captures microphone samples.
2. Samples are folded to mono and pushed into a shared ring.
3. A steady 10 ms timer drains the ring, optionally resamples to 48 kHz, applies volume, and sends frames into `NativeAudioSource`.

### Receiver

1. Each remote audio track is read from `NativeAudioStream`.
2. Samples are routed into the local mixer.
3. The mixer applies per-user/per-stream volume and writes to the speaker buffer.

## Chosen Integration Points

### Sender-side DeepFilterNet

DeepFilterNet is inserted after microphone samples have been normalized to the internal transport format:

`mic callback -> mono ring -> resample to 48 kHz -> DeepFilterNet -> input volume -> LiveKit publish`

Why this point:

- The microphone callback stays minimal and resilient.
- DeepFilterNet always receives the same sample rate and channel layout.
- Backpressure and fallback logic stay in the existing timer task instead of the device callback.
- It is easier to add telemetry and graceful bypass.

### Receiver-side DeepFilterNet

Receiver denoise is applied only to individual remote voice users:

`Remote voice track -> optional per-user DeepFilterNet -> mixer -> speaker`

Rules:

- Only regular remote voice tracks are processed.
- `ScreenshareAudio` is explicitly excluded.
- The toggle is local-only and does not affect other participants.
- The toggle is exposed in the participant context menu inside `channel_panel`.

## Backend Strategy

The implementation uses the ordinary DeepFilterNet C API via runtime-loaded DLL instead of a compile-time linked dependency.

Benefits:

- No extra linker step is required right now.
- The app can start even when DeepFilterNet artifacts are absent.
- We can ship or swap model artifacts later without rewriting the voice pipeline.

Current runtime lookup order:

- DLL path from `ASTRIX_DF_DLL_PATH`
- Model path from `ASTRIX_DF_MODEL_PATH`
- Selected model from app settings in `client/vendor/deepfilternet/models/`
- Fallback search in `client/vendor/deepfilternet/`

Bundled Windows runtime:

- `client/vendor/deepfilternet/libdf.dll`
- Built from upstream `DeepFilterNet` commit `d375b2d8309e0935d165700c91da9de862a99c31`
- Build command: `cargo build -p deep_filter --release --features capi`

## Model Catalog

The settings UI now exposes only the models that were validated against the bundled Windows `libDF` runtime:

- `DeepFilterNet3_ll_onnx.tar.gz`
- `DeepFilterNet3_onnx.tar.gz`

Default selection:

- `DeepFilterNet3_onnx.tar.gz`

The plain `*.zip` checkpoints stay on disk if downloaded, but they are not offered in settings and are not used by the current `libDF` runtime path.

Validation result on the current Windows runtime:

- `DeepFilterNet3_onnx.tar.gz`: OK
- `DeepFilterNet3_ll_onnx.tar.gz`: OK
- `DeepFilterNet2_onnx.tar.gz`: crashes inside `tract` during model init
- `DeepFilterNet2_onnx_ll.tar.gz`: crashes inside `tract` during model init

If the backend is unavailable, the denoiser falls back to bypass and logs once.

## Implementation Roadmap

1. Add a dedicated `denoise` module with:
   - runtime DeepFilterNet loader
   - per-instance streaming adapter
   - safe bypass fallback
2. Wire sender-side microphone denoise into the 10 ms publish task.
3. Add per-user receiver denoise state and `VoiceCmd`.
4. Add the per-user toggle to the participant context menu in `channel_panel`.
5. Mirror the same toggle in the voice grid context menu for UX consistency.
6. Persist receiver-side per-user toggles in settings.
7. Add logs and failure handling so missing DeepFilterNet artifacts do not break voice.
8. Validate with `cargo check` and then with real voice sessions once DLL/model artifacts are available.

## Notes

- Echo cancellation is a separate concern and is not replaced by DeepFilterNet.
- The current implementation wires the real integration points and runtime loader, but actual denoising will only activate when the DeepFilterNet DLL and model are present on disk.
- A future pass should replace the current linear microphone resampler with a higher-quality streaming resampler to give DeepFilterNet better input on 44.1 kHz devices.
