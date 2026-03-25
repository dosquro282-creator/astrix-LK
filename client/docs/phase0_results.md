# Phase 0: Результаты исследования (Zero-CPU-Readback GPU H.264 Pipeline)

## 0.1 webrtc-sys: PeerConnectionFactory, VideoEncoderFactory, EncodedImageCallback

**Структура livekit/rust-sdks:**
- `webrtc-sys/` — FFI-обёртка над libwebrtc (cxx bridge)
- `webrtc-sys/include/livekit/*.h` — C++ заголовки (нет единого livekit_rtc.h)
- `webrtc-sys/src/peer_connection_factory.rs` — `create_peer_connection_factory()`, `create_video_track(label, source)`
- `webrtc-sys/include/livekit/video_track.h` — `VideoTrackSource` (наследует `webrtc::AdaptedVideoTrackSource`), `on_captured_frame()`

**Точка интеграции для NativeEncodedVideoSource:**
- `NativeVideoSource` → `VideoTrackSource` → `AdaptedVideoTrackSource` — принимает raw `VideoFrame` (I420)
- Для encoded H.264 нужен **новый тип источника**: не `AdaptedVideoTrackSource`, а источник, который передаёт encoded frames в RTP packetizer
- В libwebrtc: `EncodedImageCallback::OnEncodedImage()` — вызывается энкодером при выходе кадра; RTP packetizer подписан на этот callback
- **Вывод:** Форк должен добавить C++ glue `rtc_create_encoded_video_source()` — создаёт `VideoTrackSource`-подобный объект, который обходит encoder и напрямую вызывает `EncodedImageCallback` с уже закодированными H.264 NAL units

---

## 0.2 windows features для MFT и D3D11VideoProcessor

**Добавлено в `client/Cargo.toml`:**
```toml
"Win32_Media_MediaFoundation",  # IMFTransform, IMFDXGIBuffer, MFTEnumEx, MFVideoFormat_*
"Win32_System_Com",             # CoInitializeEx, CoTaskMemFree
```

**ID3D11VideoProcessor** — уже входит в `Win32_Graphics_Direct3D11` (уже есть).

---

## 0.3 MFT H.264 энкодеры (MFTEnumEx)

**Пример:** `cargo run --example mft_enum`

**Результат на целевой машине:**
- Hardware: NVIDIA H.264 Encoder MFT, AMDh264Encoder (×2)
- Software: H264 Encoder MFT
- **MFVideoFormat_NV12** input подтверждён на всех энкодерах

---

## 0.4 DXGI адаптеры (EnumAdapters1)

**Пример:** `cargo run --example dxgi_adapters`

**Логика выбора:**
1. Пропустить `DXGI_ADAPTER_FLAG_SOFTWARE` (WARP)
2. Среди оставшихся — `max_by_key(|a| a.dedicated_video_memory)`
3. Env var `ASTRIX_GPU_ADAPTER=N` — ручной выбор по индексу

---

## 0.5 WGC D3D11 device → адаптер

**Текущий код (windows-capture):**
- `create_d3d_device()` вызывает `D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, ...)` — `None` = default adapter (primary display)
- Device передаётся в `GraphicsCaptureApi` и используется для WGC

**Получение адаптера из device:**
```rust
use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

let dxgi_device: IDXGIDevice = d3d_device.cast()?;
let adapter: IDXGIAdapter = dxgi_device.GetAdapter()?;
// adapter.GetDesc1() → DXGI_ADAPTER_DESC1 (имя, DedicatedVideoMemory, ...)
```

**Совместимость WGC + MFT:**
- WGC и MFT **должны использовать один и тот же** `ID3D11Device`
- MFT получает device через `IMFDXGIDeviceManager` (ProcessMessage MFT_MESSAGE_SET_D3D_MANAGER)
- **Стратегия:** использовать device, созданный WGC, для MFT. Не создавать второй device.
- **Альтернатива (Phase 2):** создать `GpuDevice::select_best()` **до** WGC, передать его device в windows-capture (требует патча create_d3d_device → принимать опциональный adapter)

---

## Файлы Phase 0

| Файл | Описание |
|------|----------|
| `client/examples/mft_enum.rs` | Перечисление MFT H.264 энкодеров (NV12→H.264) |
| `client/examples/dxgi_adapters.rs` | Перечисление DXGI адаптеров, логика select_best |
| `client/Cargo.toml` | +Win32_Media_MediaFoundation, +Win32_System_Com |
