# Roadmap: MFT H.264 Decoder (Viewer Side)

## Контекст

**Проект:** Astrix — десктопный клиент (Rust, egui/eframe, Windows 10/11), screen share через LiveKit.

**Целевая ОС:** пока что только Windows (MFT — Windows-only API).

**Текущий путь декодирования (viewer):**
```
RTP → libwebrtc H.264 decoder (FFmpeg/OpenH264, CPU) → I420
  → video_frame_to_rgba (D3d11I420ToRgba или to_argb) → RGBA → egui
```

**Цель:** аппаратное декодирование H.264 на стороне смотрящего через Windows MFT (Media Foundation Transform). Снижение CPU load, особенно при 1080p60/1440p60.

**Целевой стек:**
```
RTP → MFT H.264 decoder (GPU: DXVA2/D3D11 Video Decoding) → NV12
  → NV12→I420 (или D3D11 texture path) → video_frame_to_rgba → RGBA → egui
```

**Преимущества MFT vs NVIDIA NVDEC:**
- Универсален: Intel, AMD, NVIDIA — без вендор-SDK
- Уже используется MFT encoder на publisher; симметричный стек
- Windows 10/11, встроенная поддержка

---

## Ключевые файлы

| Файл | Роль |
|------|------|
| `webrtc-sys/src/video_decoder_factory.cpp` | VideoDecoderFactory::Create — выбор декодера для H.264 |
| `webrtc-sys/src/mft/mft_h264_decoder_impl.cpp` | MftH264DecoderImpl — MFT H.264 decoder (Windows) |
| `webrtc-sys/src/mft/mft_decoder_factory.cpp` | MftVideoDecoderFactory — фабрика MFT декодеров |
| `webrtc-sys/src/nvidia/h264_decoder_impl.cpp` | Референс: реализация webrtc::VideoDecoder |
| `webrtc-sys/include/livekit/video_decoder_factory.h` | Заголовок фабрики |
| `client/src/d3d11_rgba.rs` | I420→RGBA (decode path); при NV12 output нужна NV12→RGBA или NV12→I420 |
| `client/src/gpu_device.rs` | Выбор D3D11 device (переиспользовать для MFT decoder) |

---

## Phase 0: Исследование ✅

### Задачи

- [x] **0.1** Изучить MFT H.264 decoder API: `MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, ...)`, `IMFTransform`, input/output media types
- [x] **0.2** Проверить: MFT decoder принимает Annex B или AVCC? WebRTC передаёт Annex B.
- [x] **0.3** Изучить output format MFT decoder: NV12, DXVA2 surface, D3D11 texture?
- [x] **0.4** Изучить `webrtc::VideoDecoder` interface: `Configure`, `Decode`, `RegisterDecodeCompleteCallback`, output `VideoFrame` с `I420Buffer`
- [x] **0.5** Проверить порядок выбора в VideoDecoderFactory: NVIDIA → ? → software. Куда вставить MFT.

**Результат:** понятна точка интеграции, формат входа/выхода MFT decoder.

### Phase 0: Отчёт исследования

#### 0.1 MFT H.264 Decoder API

| Элемент | Детали |
|---------|--------|
| **Создание** | `MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, Flags, &inputType, NULL, &ppActivate, &count)` или `CoCreateInstance(CLSID_CMSH264DecoderMFT)` (wmcodecdsp.h) |
| **Input type** | `MFVideoFormat_H264` или `MFVideoFormat_H264_ES`; MF_MT_MAJOR_TYPE=MFMediaType_Video |
| **Input атрибуты** | MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE (рекоменд. MFVideoInterlace_MixedInterlaceOrProgressive) |
| **Output types** | MFVideoFormat_NV12, MFVideoFormat_I420, MFVideoFormat_IYUV, MFVideoFormat_YV12, MFVideoFormat_YUY2 |
| **IMFTransform** | ProcessInput, ProcessOutput; MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE |
| **Hardware** | MFT_ENUM_FLAG_HARDWARE для GPU; CODECAPI_AVDecVideoAcceleration_H264; MF_SA_D3D_AWARE (DXVA) |
| **DLL** | Msmpeg2vdec.dll |

#### 0.2 Annex B vs AVCC

**MFT decoder принимает Annex B.** Документация Microsoft: *"Input data must conform to Annex B of ISO/IEC 14496-10. The data must include the start codes. The decoder skips bytes until it finds a valid sequence parameter set (SPS) and picture parameter set (PPS) in the byte stream."*

WebRTC передаёт Annex B (EncodedImage с NAL units в Annex B). **Совместимость подтверждена.**

#### 0.3 Output format MFT decoder

| Режим | Формат выхода |
|-------|---------------|
| **Software** | IMFSample с IMFMediaBuffer — NV12, I420, YV12, YUY2, IYUV (системная память) |
| **Hardware (DXVA2)** | MF_SA_D3D_AWARE; DXVA2 surfaces |
| **Hardware (D3D11)** | MFT_MESSAGE_SET_D3D_MANAGER + IMFDXGIDeviceManager; D3D11 textures через MFCreateDXGISurfaceBuffer |

**Рекомендация для Phase 1:** использовать software path с output MFVideoFormat_NV12 или MFVideoFormat_I420. I420 — zero-copy в webrtc::I420Buffer; NV12 требует libyuv::NV12ToI420 (как в NvidiaH264DecoderImpl).

#### 0.4 webrtc::VideoDecoder interface

```cpp
// NvidiaH264DecoderImpl — референс
bool Configure(const Settings& settings) override;
int32_t RegisterDecodeCompleteCallback(DecodedImageCallback* callback) override;
int32_t Decode(const EncodedImage& input_image, bool missing_frames, int64_t render_time_ms) override;
int32_t Release() override;
DecoderInfo GetDecoderInfo() const override;
```

**Output:** `decoded_complete_callback_->Decoded(VideoFrame, std::optional<int32_t> decode_time, std::optional<int32_t> qp)`. VideoFrame содержит `scoped_refptr<I420Buffer>`.

**EncodedImage:** `data()`, `size()`, `RtpTimestamp()`, `ColorSpace()`.

#### 0.5 VideoDecoderFactory: порядок выбора

Текущий код (`video_decoder_factory.cpp`):

1. **macOS:** ObjCVideoDecoderFactory
2. **Android:** CreateAndroidVideoDecoderFactory
3. **Linux + CUDA:** NvidiaVideoDecoderFactory (USE_NVIDIA_VIDEO_CODEC)
4. **Fallback:** CreateVp8Decoder, VP9Decoder::Create, **H264Decoder::Create** (software)

**Windows:** USE_NVIDIA_VIDEO_CODEC не определён (NVIDIA только на Linux в build.rs). Для H.264 используется только `webrtc::H264Decoder::Create()`.

**Точка вставки MFT:** добавить `MftVideoDecoderFactory` в `factories_` перед fallback. Порядок: **MFT → software** (на Windows NVIDIA нет). Для симметрии с roadmap: при появлении NVIDIA на Windows — **NVIDIA → MFT → software**.

---

## Phase 1: MFT H.264 Decoder (C++)

### Архитектура

```cpp
class MftH264DecoderImpl : public webrtc::VideoDecoder {
  // IMFTransform (MFT H.264 decoder)
  // IMFDXGIDeviceManager (для hardware path)
  // Input: EncodedImage (Annex B)
  // Output: I420Buffer (или NV12 → I420 конвертация)
 public:
  bool Configure(const Settings& settings) override;
  int32_t RegisterDecodeCompleteCallback(DecodedImageCallback* callback) override;
  int32_t Decode(const EncodedImage& input_image, bool missing_frames, int64_t render_time_ms) override;
  int32_t Release() override;
  DecoderInfo GetDecoderInfo() const override;
};
```

### Задачи

- [x] **1.1** Создать `webrtc-sys/src/mft/mft_h264_decoder_impl.cpp` и `.h`
- [x] **1.2** `Configure()`: CoCreateInstance(CLSID_CMSH264DecoderMFT), input/output media types (Phase 1 — software path)
- [x] **1.3** Установить input media type: `MFVideoFormat_H264_ES` (Annex B)
- [x] **1.4** Установить output media type: `MFVideoFormat_NV12`
- [x] **1.5** `Decode()`: подать `EncodedImage` в MFT через `ProcessInput`; получить output sample; конвертировать NV12→I420 (libyuv); вызвать `callback->Decoded()`
- [x] **1.6** Обработать `MF_E_TRANSFORM_NEED_MORE_INPUT`, `MF_E_TRANSFORM_STREAM_CHANGE`
- [x] **1.7** Fallback: `MftVideoDecoderFactory::IsSupported()` — при ошибке CoCreateInstance возвращает false
- [x] **1.8** Hardware MFT path — D3D11 Video Decoding (IMFDXGIDeviceManager)

**Результат Phase 1:** MftH264DecoderImpl компилируется, декодирует H.264 software-путём.

---

### Phase 1.8: Hardware MFT (D3D11 Video Decoding)

**Цель:** переключить MFT decoder с software (NV12 в системной RAM) на hardware (NV12 D3D11 texture). Это необходимое условие для zero CPU readback в Phase 3.

**Целевой стек:**
```
RTP → MFT hardware decoder (DXVA2/D3D11)
    → NV12 D3D11 texture (GPU-resident, без копирования в RAM)
    → NV12→RGBA compute shader (Phase 3)
    → RGBA texture → egui
```

**Сравнение software vs hardware output:**

| Параметр | Software MFT | Hardware MFT |
|----------|-------------|--------------|
| Буфер выхода | IMFMediaBuffer (системная RAM) | IMFDXGIBuffer → ID3D11Texture2D |
| PROVIDES_SAMPLES | 0 (мы выделяем) | 1 (MFT выделяет) |
| CPU readback | Lock/Lock2D на CPU | не нужен (texture на GPU) |
| Интерфейс доступа | IMFMediaBuffer::Lock | IMFDXGIBuffer::GetResource + GetSubresourceIndex |

---

#### 1.8.1 D3D11 Device для декодера

**Опции:**

| Опция | За | Против |
|-------|-----|--------|
| **Shared device** (переиспользовать rendering device из Rust) | Zero-copy: NV12 texture и RGBA target на одном device | Contention риск (см. Phase 5.5); device-lost затрагивает обоих |
| **Dedicated decoder device** | Изоляция; device-lost декодера не рушит rendering | Нужна cross-device copy через IDXGIKeyedMutex или shared texture |

**Рекомендация:** начать с **shared device** (adapter LUID из gpu_device.rs), если device-lost станет проблемой — перейти на dedicated.

**Задача:** передать `ID3D11Device*` из Rust (`gpu_device.rs`) в `MftH264DecoderImpl::Configure()` через FFI. Альтернатива — создавать device внутри C++ по тому же adapter LUID.

**В C++ (`CreateMftDecoder`):**
```cpp
// Получить D3D11 device (переданный через Configure или созданный внутри)
ComPtr<ID3D11Device> d3d11_device;
// ... (см. 1.8.2)
```

---

#### 1.8.2 IMFDXGIDeviceManager

```cpp
UINT reset_token = 0;
ComPtr<IMFDXGIDeviceManager> device_manager;
HRESULT hr = MFCreateDXGIDeviceManager(&reset_token, &device_manager);
if (FAILED(hr)) { /* fallback to software */ }

hr = device_manager->ResetDevice(d3d11_device.Get(), reset_token);
if (FAILED(hr)) { /* fallback to software */ }
```

**Задача 1.8.2:** Создать `IMFDXGIDeviceManager` и привязать к D3D11 device до инициализации MFT.

---

#### 1.8.3 Передача D3D Manager в MFT

```cpp
hr = transform_->ProcessMessage(
    MFT_MESSAGE_SET_D3D_MANAGER,
    reinterpret_cast<ULONG_PTR>(device_manager.Get())
);
if (FAILED(hr)) {
    RTC_LOG(LS_WARNING) << "[MFT decoder] MFT_MESSAGE_SET_D3D_MANAGER failed hr=" << hr << ", fallback to software";
    // не вызывать Release() — просто не использовать device_manager
    use_hardware_ = false;
}
```

**Важно:** вызвать **до** `MFT_MESSAGE_NOTIFY_BEGIN_STREAMING` и до `SetOutputType`.

**Задача 1.8.3:** После `CoCreateInstance`, до `SetInputType` — вызвать `ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER)`.

---

#### 1.8.4 Output media type для D3D11 path

После успешного SetD3DManager MFT предложит D3D11-backed output types:

```cpp
// Проверить MF_SA_D3D11_AWARE
UINT32 d3d11_aware = 0;
transform_->GetAttributes(&attribs);
attribs->GetUINT32(MF_SA_D3D11_AWARE, &d3d11_aware);
// d3d11_aware == 1 → MFT поддерживает D3D11 textures

// SetOutputType — MFVideoFormat_NV12; MFT сам выделит D3D11 texture
ComPtr<IMFMediaType> output_type;
MFCreateMediaType(&output_type);
output_type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
output_type->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_NV12);
// ... ширина, высота, fps ...
transform_->SetOutputType(0, output_type.Get(), 0);
```

**Задача 1.8.4:** После SetD3DManager выставить output type. Проверить `MF_SA_D3D11_AWARE`. Логировать результат.

---

#### 1.8.5 ProcessOutput: извлечение D3D11 texture

В hardware path MFT сам создаёт sample (`MFT_OUTPUT_STREAM_PROVIDES_SAMPLES = 1`):

```cpp
// ProcessOutput (hardware path)
MFT_OUTPUT_DATA_BUFFER output_buffer = {};
output_buffer.pSample = nullptr;  // MFT создаёт сам

DWORD status = 0;
hr = transform_->ProcessOutput(0, 1, &output_buffer, &status);
// ...

ComPtr<IMFSample> mft_sample;
mft_sample.Attach(output_buffer.pSample);  // владение

ComPtr<IMFMediaBuffer> out_buf;
mft_sample->GetBufferByIndex(0, &out_buf);

// Получить ID3D11Texture2D через IMFDXGIBuffer
ComPtr<IMFDXGIBuffer> dxgi_buf;
out_buf.As(&dxgi_buf);

ComPtr<ID3D11Texture2D> texture;
UINT subresource_index = 0;
dxgi_buf->GetResource(IID_PPV_ARGS(&texture));
dxgi_buf->GetSubresourceIndex(&subresource_index);

// texture — NV12 D3D11 texture, GPU-resident
// subresource_index — индекс в texture array (часто 0)
```

**Задача 1.8.5:** В `ProcessOutput()` при `use_hardware_ = true` — извлекать `ID3D11Texture2D` вместо `IMFMediaBuffer::Lock`. Логировать texture dimensions для отладки.

---

#### 1.8.6 Передача texture downstream (VideoFrame)

**Проблема:** `webrtc::VideoFrame` ожидает `I420Buffer`. Передать `ID3D11Texture2D` — нужен кастомный `VideoFrameBuffer`.

**Задача 1.8.6 (подготовка к Phase 3):** Создать `D3D11TextureVideoFrameBuffer`:
```cpp
class D3D11TextureVideoFrameBuffer : public rtc::VideoFrameBuffer {
 public:
  Type type() const override { return Type::kNative; }
  int width() const override { return width_; }
  int height() const override { return height_; }
  rtc::scoped_refptr<I420BufferInterface> ToI420() override;
  // Для hardware path — fallback ToI420() через IMF2DBuffer Lock (CPU readback)
  // Phase 3 заменит это на GPU path

  ID3D11Texture2D* texture() const { return texture_.Get(); }
  UINT subresource() const { return subresource_; }

 private:
  ComPtr<ID3D11Texture2D> texture_;
  UINT subresource_;
  int width_, height_;
};
```

**Fallback `ToI420()`** (временный, пока Phase 3 не готова): CPU readback через `IMFMediaBuffer::Lock` на копии текстуры в staging resource. Позволит видеть картинку до завершения zero-copy пути.

---

#### 1.8.7 Флаги и fallback

```cpp
// в mft_h264_decoder_impl.h
bool use_hardware_ = false;  // true если SetD3DManager успешен
ComPtr<IMFDXGIDeviceManager> dxgi_device_manager_;
```

**Логика Configure():**
1. Попытаться создать D3D11 device + IMFDXGIDeviceManager → `MFT_MESSAGE_SET_D3D_MANAGER`
2. Если любой шаг упал — `use_hardware_ = false`, продолжить как software
3. Логировать: `[MFT decoder] Hardware D3D11 path: enabled / disabled (reason)`

**ProcessOutput():** два пути по `use_hardware_`:
- `true` → `IMFDXGIBuffer::GetResource` → `D3D11TextureVideoFrameBuffer`
- `false` → прежний `IMFMediaBuffer::Lock / Lock2D` → I420Buffer

---

#### 1.8: Чеклист

| # | Задача | Статус |
|---|--------|--------|
| 1.8.1 | D3D11 device для декодера (shared или dedicated) | [x] |
| 1.8.2 | `MFCreateDXGIDeviceManager` + `ResetDevice` | [x] |
| 1.8.3 | `ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER)` | [x] |
| 1.8.4 | SetOutputType после D3D manager; проверить `MF_SA_D3D11_AWARE` | [x] |
| 1.8.5 | `ProcessOutput`: `IMFDXGIBuffer::GetResource` → `ID3D11Texture2D` | [x] |
| 1.8.6 | `D3D11TextureVideoFrameBuffer` (kNative) + временный fallback `ToI420` | [x] |
| 1.8.7 | `use_hardware_` флаг, graceful fallback, логи | [x] |

**Результат Phase 1.8:** MFT decoder декодирует H.264 в NV12 D3D11 texture. Картинка видна (через временный CPU readback ToI420). CPU readback ещё присутствует — устраняется в Phase 3.

---

## Phase 2: MFT Decoder Factory

### Задачи

- [x] **2.1** Создать `MftVideoDecoderFactory` по аналогии с `NvidiaVideoDecoderFactory`
- [x] **2.2** `IsSupported()`: проверить наличие MFT H.264 decoder (CoCreateInstance CLSID_CMSH264DecoderMFT)
- [x] **2.3** `GetSupportedFormats()`: H.264 profiles через `webrtc::SupportedH264DecoderCodecs()`
- [x] **2.4** Интегрировать в `VideoDecoderFactory::Create`: для H.264 — MFT (если factories_) → software fallback
- [x] **2.5** `USE_MFT_VIDEO_CODEC` уже в build.rs (Windows only)
- [x] **2.6** Env var `ASTRIX_DECODE_PATH`: `mft` | `cpu` — по умолчанию `cpu` (MFT experimental, STATUS_STACK_BUFFER_OVERRUN)

**Результат:** при воспроизведении H.264 screen share используется MFT decoder (если доступен).

### Phase 2: Реализовано

**Файлы:**
- `webrtc-sys/src/mft/mft_decoder_factory.h` — MftVideoDecoderFactory
- `webrtc-sys/src/mft/mft_decoder_factory.cpp` — IsSupported (CoCreateInstance), GetSupportedFormats, Create
- `webrtc-sys/src/video_decoder_factory.cpp` — ASTRIX_DECODE_PATH, условное добавление MFT
- `webrtc-sys/build.rs` — include path `src/mft` для Windows

---

## Phase 3: Zero CPU Readback — NV12 D3D11 texture → RGBA (GPU)

**Цель:** полностью исключить CPU из пути decoded frame → egui. После Phase 1.8 декодер выдаёт `ID3D11Texture2D` (NV12). В Phase 3 эта текстура конвертируется в RGBA compute shader'ом без какого-либо `Map`/`Lock`/`CopyResource` в системную RAM.

**Целевой pipeline:**
```
MFT H.264 decoder (GPU) → NV12 D3D11 texture
  → Compute shader NV12→RGBA → RGBA D3D11 texture
  → egui (wgpu/D3D11 backend)
  [CPU readback: ОТСУТСТВУЕТ]
```

**Сравнение с текущим состоянием:**

| Этап | До (software MFT) | После Phase 3 |
|------|------------------|---------------|
| Декодирование | CPU (MFT software) → NV12 RAM | GPU (MFT HW) → NV12 D3D11 texture |
| Конвертация | CPU libyuv NV12→I420 | GPU compute shader NV12→RGBA |
| I420Buffer | Создаётся на CPU (alloc + copy) | Не нужен |
| Передача в egui | RGBA CPU buffer → GPU upload | RGBA already on GPU |
| CPU readback | Lock/Lock2D (систематически) | Нет |

---

### 3.1 FFI: извлечение D3D11 texture из VideoFrame в Rust

**Задача:** передать `ID3D11Texture2D*` из C++ VideoFrameBuffer в Rust без копирования.

**C++ side (`D3D11TextureVideoFrameBuffer`):** уже создан в Phase 1.8.6. Добавить экспортируемые функции:

```cpp
// webrtc-sys/src/mft/mft_h264_decoder_impl.h (или video_frame_buffer.h)
extern "C" {
  // Проверить, является ли VideoFrameBuffer D3D11TextureVideoFrameBuffer
  bool webrtc_video_frame_buffer_is_d3d11(const webrtc::VideoFrameBuffer* buf);
  // Получить ID3D11Texture2D* (caller не владеет — не Release)
  void* webrtc_video_frame_buffer_get_d3d11_texture(const webrtc::VideoFrameBuffer* buf);
  // Получить subresource index
  uint32_t webrtc_video_frame_buffer_get_d3d11_subresource(const webrtc::VideoFrameBuffer* buf);
}
```

**Rust side (`d3d11_rgba.rs` или новый `d3d11_nv12.rs`):**
```rust
extern "C" {
    fn webrtc_video_frame_buffer_is_d3d11(buf: *const c_void) -> bool;
    fn webrtc_video_frame_buffer_get_d3d11_texture(buf: *const c_void) -> *mut c_void;
    fn webrtc_video_frame_buffer_get_d3d11_subresource(buf: *const c_void) -> u32;
}
```

**Задача 3.1:** реализовать C++ экспорт + Rust FFI привязки. Написать тест: создать `D3D11TextureVideoFrameBuffer`, проверить `is_d3d11 = true`, `get_d3d11_texture ≠ null`.

**Реализовано (2025-03):** C++ функции в `video_frame_buffer.cpp`, cxx bridge в `video_frame_buffer.rs`. Тест `test_d3d11_ffi_with_i420_buffer` проверяет I420 path (false/0/0); D3D11 path будет проверен в Phase 3.4.

**Запуск теста:** `cd client/vendor/rust-sdks && cargo test -p webrtc-sys test_d3d11_ffi`

**Примечание:** STATUS_STACK_BUFFER_OVERRUN в тесте D3D11 FFI (исправлено): см. раздел «Исправлено: STATUS_STACK_BUFFER_OVERRUN в тесте D3D11 FFI (Phase 3.1)» ниже.

---

### 3.2 HLSL: compute shader NV12→RGBA

NV12 — biplanar формат:
- **Plane 0 (Y):** `R8_UNORM` texture, размер W×H
- **Plane 1 (UV):** `R8G8_UNORM` texture, размер W/2×H/2 (Cb и Cr interleaved)

**Создание SRV для NV12 MFT texture:**
```cpp
// Y plane
D3D11_SHADER_RESOURCE_VIEW_DESC srvY = {};
srvY.Format = DXGI_FORMAT_R8_UNORM;
srvY.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
srvY.Texture2D.MipLevels = 1;
device->CreateShaderResourceView(nv12_texture, &srvY, &srv_y_);

// UV plane  
D3D11_SHADER_RESOURCE_VIEW_DESC srvUV = {};
srvUV.Format = DXGI_FORMAT_R8G8_UNORM;
srvUV.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
device->CreateShaderResourceView(nv12_texture, &srvUV, &srv_uv_);
```

**HLSL (`nv12_to_rgba.hlsl`):**
```hlsl
Texture2D<float>  TexY  : register(t0);  // Y plane (R8)
Texture2D<float2> TexUV : register(t1);  // UV plane (RG8)
RWTexture2D<float4> OutRGBA : register(u0);

[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    int2 uv_pos = id.xy / 2;
    float y  = TexY[id.xy];
    float2 uv = TexUV[uv_pos] - float2(0.5, 0.5);

    // BT.601 full range → RGB
    float r = clamp(y + 1.402  * uv.y,              0.0, 1.0);
    float g = clamp(y - 0.3441 * uv.x - 0.7141 * uv.y, 0.0, 1.0);
    float b = clamp(y + 1.772  * uv.x,              0.0, 1.0);

    OutRGBA[id.xy] = float4(r, g, b, 1.0);
}
```

**Задача 3.2:** создать `webrtc-sys/src/mft/nv12_to_rgba.hlsl`. Скомпилировать через `fxc /T cs_5_0` в build.rs (или встроить как строку). Создать `D3d11Nv12ToRgba` struct в `d3d11_rgba.rs`.

**Реализовано (2025-03):**
- `client/nv12_to_rgba.hlsl` — compute shader BT.601 full range, numthreads(8,8,1)
- `client/src/d3d11_rgba.rs` — `D3d11Nv12ToRgba::new(device)`, `convert(nv12_texture, subresource, w, h)` → RGBA texture
- SRV Y (R8_UNORM) и UV (R8G8_UNORM) создаются на NV12 texture через format override

---

### 3.3 Rust: D3d11Nv12ToRgba

**Новый тип в `d3d11_rgba.rs` (или `d3d11_nv12.rs`):**

```rust
pub struct D3d11Nv12ToRgba {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    shader: ID3D11ComputeShader,
    output_texture: Option<ID3D11Texture2D>,   // RGBA UAV target
    output_srv: Option<ID3D11ShaderResourceView>,
    width: u32,
    height: u32,
}

impl D3d11Nv12ToRgba {
    pub fn new(device: &ID3D11Device) -> Result<Self, Error>;

    /// Конвертировать NV12 D3D11 texture → RGBA D3D11 texture (GPU only)
    pub fn convert(
        &mut self,
        nv12_texture: &ID3D11Texture2D,
        subresource: u32,
        width: u32,
        height: u32,
    ) -> Result<&ID3D11Texture2D, Error>;
    // Возвращает &ID3D11Texture2D (RGBA) для egui
}
```

**Задача 3.3:** Реализовать `D3d11Nv12ToRgba`:
- lazy init output RGBA texture при изменении размера
- создать SRV Y/UV из NV12 texture (см. 3.2)
- CSSetShaderResources, CSSetUnorderedAccessViews, Dispatch, ClearState
- вернуть output texture (без Map/CopyResource на CPU)

**Реализовано (2025-03):** все пункты выполнены в Phase 3.2.

**Отличия от спецификации:**
- `output_srv` не реализован — для Phase 3.3 достаточно возвращать texture; SRV для egui добавить в Phase 3.5 при необходимости
- `subresource` передаётся в `convert()` но не используется при создании SRV — для texture array (ArraySize>1) потребуется `TEXTURE2DARRAY` с `FirstArraySlice`; добавить при появлении артефактов

---

### 3.4 Интеграция в voice_livekit.rs / decode path

**В `voice_livekit.rs` (render loop):**

```rust
// Определить тип VideoFrame buffer
if unsafe { webrtc_video_frame_buffer_is_d3d11(buf_ptr) } {
    // Hardware path — zero CPU readback
    let texture_ptr = unsafe { webrtc_video_frame_buffer_get_d3d11_texture(buf_ptr) };
    let subresource = unsafe { webrtc_video_frame_buffer_get_d3d11_subresource(buf_ptr) };
    let nv12_texture: ID3D11Texture2D = unsafe { /* from raw ptr */ };
    let rgba_texture = nv12_to_rgba.convert(&nv12_texture, subresource, w, h)?;
    // передать rgba_texture в egui (shared handle или copy в egui texture)
} else {
    // Software fallback — прежний I420→RGBA путь
    d3d11_i420_to_rgba.convert(&i420_buffer)?;
}
```

**Задача 3.4:** внедрить ветку `is_d3d11` в decode path. Определить как передавать `ID3D11Texture2D` в egui (wgpu `register_native_texture` или D3D11 interop).

**Реализовано (2025-03):**
- libwebrtc: `NativeBuffer::video_frame_buffer_unique_ptr()` для FFI
- `video_frame_to_rgba`: ветка `as_native` → `video_frame_buffer_is_d3d11` → `D3d11Nv12ToRgba::convert_to_rgba_bytes`
- Подход C: NV12→RGBA на GPU, Map для egui (совместимость с текущим `(w, h, Vec<u8>)`). Phase 3.5 — full zero-copy.

---

### 3.5 egui/wgpu interop: передача RGBA texture ✅

**Проблема:** egui использует `glow` (OpenGL) backend. Нужно передать `ID3D11Texture2D` (RGBA) в egui без `Map` в CPU память.

**Выбранный подход: WGL_NV_DX_interop2**
- RGBA texture создаётся с `D3D11_RESOURCE_MISC_SHARED | D3D11_BIND_SHADER_RESOURCE`
- `IDXGIResource::GetSharedHandle` → Win32 HANDLE → `VideoFrame.shared_handle: Option<usize>`
- UI thread открывает shared handle на своём D3D11 device
- `wglDXOpenDeviceNV` + `wglDXRegisterObjectNV` регистрируют текстуру как GL texture
- `wglDXLockObjectsNV` / `wglDXUnlockObjectsNV` — синхронизация между D3D11 и GL
- egui получает `TextureId::User(gl_tex_id)` — полный zero-copy, CPU не участвует

**Реализовано (2026-03):**
- `d3d11_rgba.rs`: output texture создаётся с `D3D11_RESOURCE_MISC_SHARED`, `get_shared_handle()` возвращает Win32 HANDLE
- `voice.rs`: `VideoFrame.shared_handle: Option<usize>` — несёт HANDLE из voice thread в UI thread
- `d3d11_gl_interop.rs` (новый файл): `D3d11GlInterop` — WGL_NV_DX_interop2 менеджер с методами `try_new`, `update_texture`, `lock_all`, `unlock_all`
- `voice_livekit.rs`: при `GL_INTEROP_AVAILABLE` — GPU-only path (без `convert_to_rgba_bytes`)
- `ui.rs`: GPU textures stored в `voice_video_gpu_textures: HashMap<i64, (u32, u32, u32)>`, рендеринг через `egui::TextureId::User`
- `app.rs`: `D3d11GlInterop::try_new()` при старте, `lock_all()` в конце `update()`, `unlock_all()` в начале следующего `update()`

---

### 3.6 Fallback при hardware недоступности

**Условие:** `use_hardware_ = false` в MFT decoder → VideoFrame содержит `I420Buffer` → прежний `D3d11I420ToRgba` путь.

**Задача 3.6:** убедиться что оба пути сосуществуют корректно. Переключение по результату `webrtc_video_frame_buffer_is_d3d11()`.

---

### Phase 3: Чеклист

| # | Задача | Статус |
|---|--------|--------|
| 3.1 | FFI: C++ экспорт + Rust привязки для `D3D11TextureVideoFrameBuffer` | [x] |
| 3.2 | HLSL `nv12_to_rgba.hlsl` + SRV для NV12 biplanar | [x] |
| 3.3 | `D3d11Nv12ToRgba` struct в Rust — compute dispatch без readback | [x] |
| 3.4 | Интеграция `is_d3d11` ветки в decode path (`voice_livekit.rs`) | [x] |
| 3.5 | egui/glow interop: передать RGBA D3D11 texture без CPU (WGL_NV_DX_interop2) | [x] |
| 3.6 | Fallback на I420→RGBA если hardware недоступен | [ ] |
| 3.7 | C++ декодер: передавать kNative (skip ToI420), Rust GPU NV12→RGBA через staging copy | [x] |

**Результат Phase 3:** декодированный кадр не покидает GPU от момента декодирования до egui. CPU readback = 0.

**Реализовано (Phase 3, 2026-03):**
- `mft_h264_decoder_impl.cpp`: вместо `ToI420()` + `i420` передаётся `hw_buf` (kNative) в `Decoded()`. DPB слот жив до возврата из колбэка.
- `mft_device.rs`: `get_shared_device()` — доступ к shared D3D11 device из Rust decode callback.
- `d3d11_rgba.rs`: `D3d11Nv12ToRgba` добавлено поле `nv12_staging` (NV12 текстура с BIND_SHADER_RESOURCE). В `convert()`: `CopySubresourceRegion` копирует нужный subresource из MFT texture array (ArraySize=8) в single-subresource staging → SRV → compute dispatch → RGBA output.
- `voice_livekit.rs`: `DECODE_CONVERTER_NV12` инициализируется через `mft_device::get_shared_device()` (одинаковый device с MFT, без cross-device ошибок).

---

## Phase 4: Валидация и бенчмаркинг

**Цель:** подтвердить корректность картинки, отсутствие регрессий, измерить снижение CPU load и GPU utilization после Phase 1.8 + Phase 3.

---

### 4.1 Корректность декодирования (hardware vs software)

**Задача:** убедиться что hardware MFT decoder и software MFT decoder выдают визуально идентичный результат.

**Метод:**
- Декодировать один и тот же H.264 файл (известный test vector) через software path (`ASTRIX_DECODE_PATH=cpu`) и hardware path (`=mft` с `use_hardware_=true`)
- Сравнить PSNR первых N кадров (допускается незначительный difference из-за GPU rounding — PSNR > 45 dB)
- Проверить цвета на solid color тест-кадре (чистый красный, зелёный, синий) — нет перепутанных каналов (распространённая ошибка BT.601 vs BT.709)

**Инструменты:**
- FFmpeg для эталонного NV12: `ffmpeg -i test.h264 -pix_fmt nv12 out_%04d.yuv`
- Windows DXGI screenshot или staging texture dump первого кадра

**Задача 4.1:** написать простой тест-бинарь или cargo test: создать MftH264DecoderImpl, подать тестовый H.264 blob, сравнить первый NV12/I420 output с эталоном.

---

### 4.2 Корректность end-to-end (screen share сессия)

**Задача:** ручная проверка полного pipeline publisher→viewer с hardware decode.

| Тест | Ожидаемый результат |
|------|---------------------|
| Screen share 1080p30 | Чёткая картинка, без артефактов, без зелёных кадров |
| Screen share 1440p60 | То же |
| Разные сцены: рабочий стол, браузер, VSCode | Текст читаем, цвета верны |
| Первый keyframe | Картинка появляется за ≤1 с от начала трансляции |
| STREAM_CHANGE (resize окна publisher) | Декодер перенастраивается, картинка не теряется |

**Задача 4.2:** провести ручное тестирование. Зафиксировать результат в этом разделе.

---

### 4.3 CPU load: бенчмаркинг

**Цель:** количественно подтвердить снижение CPU нагрузки.

**Методология:**
- Запустить screen share 1080p60 в течение 60 секунд
- Замерить CPU% процесса `astrix-client.exe` через Task Manager (Process Details) или PowerShell `Get-Process`
- Сравнить три режима:

| Режим | `ASTRIX_DECODE_PATH` | Ожидаемый CPU% (viewer) |
|-------|----------------------|--------------------------|
| OpenH264 (старый) | `cpu` (старая сборка) | ~20–35% @ 1080p60 |
| Software MFT | `mft` + `use_hardware_=false` | ~15–25% (MFT эффективнее FFmpeg) |
| Hardware MFT (Phase 1.8) | `mft` + `use_hardware_=true` | ~5–15% (GPU decode) |
| Hardware MFT + zero-copy (Phase 3) | `mft` + zero-copy | ~2–8% (нет NV12→I420 на CPU) |

**Задача 4.3:** заполнить таблицу реальными значениями на тестовой машине. Зафиксировать конфигурацию (GPU, CPU, разрешение, FPS).

---

### 4.4 GPU utilization

**Цель:** подтвердить что decode происходит на Video Engine GPU, а не на Shader Engine (что указывало бы на software fallback).

**Инструменты:**
- **GPU-Z** (TechPowerUp) → вкладка Sensors → `Video Engine Load %`
- **Windows Task Manager** → Performance → GPU → вкладка "Video Decode"
- **Windows Performance Monitor** → счётчик `GPU Engine` → `engtype_VideoDecode`

**Ожидаемый результат при hardware MFT:**
- `Video Engine Load > 0%` при активной трансляции
- `3D/Compute Engine` не растёт от decode (только от compute shader в Phase 3)

**Задача 4.4:** Снять GPU-Z скриншот при software vs hardware decode. Приложить к этому документу.

---

### 4.5 Fallback: система без hardware H.264 decode

**Сценарии:**
- Виртуальная машина (VMware/Hyper-V без vGPU) — нет D3D11 Video Decoder
- Очень старая GPU (pre-2012) — нет DXVA2 для H.264
- `MFT_MESSAGE_SET_D3D_MANAGER` возвращает ошибку принудительно (тест через mock)

**Ожидаемое поведение:**
1. `[MFT decoder] Hardware D3D11 path: disabled (MFT_MESSAGE_SET_D3D_MANAGER failed hr=0x...)`
2. `use_hardware_ = false` → software MFT path
3. Картинка работает через `IMFMediaBuffer::Lock` + libyuv

**Задача 4.5:** проверить в ВМ (или искусственно вернуть `hr = E_NOTIMPL` в `ProcessMessage`). Убедиться в корректном fallback без краша.

---

### 4.6 iGPU-only система (Intel QuickSync)

**Задача:** убедиться что hardware MFT decode работает не только на NVIDIA/AMD, но и на Intel iGPU.

**Ожидаемые различия:**
- Intel iGPU: другой vendor, MFT может использовать QuickSync через D3D11 VA API
- Возможен иной NV12 texture layout (stride alignment, texture array vs non-array)
- `subresource_index` может быть > 0 при texture array

**Задача 4.6:** протестировать на Intel iGPU. Проверить субресурс-индекс. Логировать `GetSubresourceIndex()`.

---

### 4.7 Memory: утечки D3D11 texture

**Задача:** убедиться что `ID3D11Texture2D` из MFT sample корректно освобождаются.

**Метод:**
- Включить D3D11 debug layer: `D3D11CreateDevice` с `D3D11_CREATE_DEVICE_DEBUG`
- Запустить 5-минутную сессию → при выходе D3D11 debug layer пишет в OutputDebugString список не освобождённых объектов
- Ожидаемый результат: 0 живых texture объектов

**Ключевые места:**
- `D3D11TextureVideoFrameBuffer` деструктор → `ComPtr` auto-release
- SRV для Y/UV planes → освобождать перед следующим кадром или кэшировать по texture ptr

**Задача 4.7:** проверить с D3D11 debug layer. Исправить утечки если обнаружены.

---

### 4.8 Стресс-тест (30 мин, 1440p60)

**Задача:** гарантировать стабильность hardware path на длительной сессии.

**Критерии:**
- Нет крашей и зависаний
- Нет деградации FPS со временем
- GPU memory не растёт монотонно (нет texture leak)
- CPU% стабилен (нет drift вверх)

**Задача 4.8:** провести 30-минутную сессию. Мониторить GPU memory (GPU-Z → Dedicated Memory Used) каждые 5 минут.

---

### Phase 4: Чеклист

| # | Задача | Статус |
|---|--------|--------|
| 4.1 | Корректность: PSNR сравнение HW vs SW decode | [ ] |
| 4.2 | End-to-end screen share: визуальная проверка | [ ] |
| 4.3 | CPU load бенчмарк: таблица SW vs HW vs zero-copy | [ ] |
| 4.4 | GPU utilization: Video Engine Load % | [ ] |
| 4.5 | Fallback на системе без hardware H.264 decode | [ ] |
| 4.6 | iGPU (Intel QuickSync) тест | [ ] |
| 4.7 | D3D11 debug layer: нет texture утечек | [ ] |
| 4.8 | Стресс-тест 30 мин 1440p60 | [ ] |

**Результат Phase 4:** hardware MFT decode + zero CPU readback подтверждены количественно и стабильны на длительных сессиях.

---

## Риски и митигации

| Риск | Митигация |
|------|-----------|
| ~~**STATUS_STACK_BUFFER_OVERRUN** при MFT~~ | ✅ **Устранено** (раунд 2: double-free ComPtr, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, Lock2D) |
| ~~**Стрим не начинается (RTP off-by-1)**~~ | ✅ **Устранено** (FIFO-очередь input_rtp_queue_ вместо round-trip конвертации через 100ns) |
| MFT decoder output format — не NV12 | Проверить GetOutputAvailableType; конвертировать в I420 |
| Hardware MFT требует D3D11 device | Создать device на viewer (нет WGC); переиспользовать gpu_device.rs логику |
| Annex B vs AVCC | MFT decoder принимает оба; проверить документацию |
| Асинхронный MFT | Обработать события METransformNeedInput/METransformHaveOutput |
| Разные GPU у publisher и viewer | Не критично; decoder на своей машине |

---

## ✅ Исправлено: STATUS_STACK_BUFFER_OVERRUN

**Симптом (исторический):** При `ASTRIX_DECODE_PATH=mft` viewer крашился с exit code `0xc0000409` (STATUS_STACK_BUFFER_OVERRUN) сразу после первого декодированного кадра.

**Статус: УСТРАНЕНО** (раунд 2 fix'ов, 2025-03).

**Исправления (2025-03, раунд 2):**

1. **[Fix #1 — критический] Double free → убран `output_buffer.pSample->Release()`.**
   `sample` — это `ComPtr<IMFSample>`; `output_buffer.pSample = sample.Get()` — лишь raw-view без AddRef.
   Ручной `Release()` + деструктор ComPtr = двойное освобождение → heap corruption → `STATUS_STACK_BUFFER_OVERRUN`.
   Исправление: убран блок `if (output_buffer.pSample) { output_buffer.pSample->Release(); }` целиком.

2. **[Fix #2] MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.**
   Перед созданием буфера: `GetOutputStreamInfo(0, &stream_info)`.
   Если `stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES` → `pSample = nullptr` (MFT создаёт sample сам).
   Возвращённый сырой указатель берётся под управление через `mft_sample.Attach(output_buffer.pSample)`.

3. **[Fix #3] IMF2DBuffer::Lock2D для реального stride.**
   `out_buf.As(&buf2d)` + `buf2d->Lock2D(&data, &actual_stride)` — реальный stride GPU-поверхности.
   Fallback на `IMFMediaBuffer::Lock` для software MFT.

4. **[Fix #4] Убран scaling-workaround.**
   Константы `kMaxOutputWidth = 1920` / `kMaxOutputHeight = 1080` удалены — маскировали баг, не устраняя его.

---

## ✅ Исправлено: стрим не начинается на смотрящем (RTP timestamp off-by-1)

**Симптом:** После fix'а STATUS_STACK_BUFFER_OVERRUN клиент перестал крашиться, но видео на смотрящем не появлялось вовсе — ни миниатюра, ни полноэкранный режим. Декодер работал (510+ вызовов `Decode()`), но `NativeVideoSink::OnFrame()` никогда не вызывался.

**Диагностика:**

Добавлено логирование в `NativeVideoSink::OnFrame()` и `ProcessOutput()`. В логах стало видно:
```
Decode() #0  rtp=4090030349          ← WebRTC Map'ит frame_info с этим rtp
output frame #0  rtp_ts=4090030348   ← декодер возвращает на 1 МЕНЬШЕ
```

**Корень проблемы:** В `VCMDecodedFrameCallback::Decoded()` (WebRTC) декодированный кадр ищется в очереди `frame_infos_` по `decoded_frame.rtp_timestamp()`. Если точного совпадения нет — кадр дропается с предупреждением `"Too many frames backed up in the decoder"`.

Несовпадение было вызвано **потерей точности при round-trip конвертации** RTP timestamp через 100-наносекундные единицы (формат Windows MF):

```
// Вход:
RTP = 4090030349  (90 kHz clock)

// В Decode(): timestamp для ProcessInput
timestamp_100ns = 4090030349 * 10_000_000 / 90_000 = 454447816555  (integer div → потеря)

// В ProcessOutput(): GetSampleTime() возвращает то же значение → конвертируем обратно
rtp_ts = 454447816555 * 90 / 10_000 = 4090030348   ← не 4090030349!

// FindFrameInfo(4090030348) ищет в [4090030349_info, ...] → не находит → DROP
```

**Исправление — FIFO-очередь входных RTP timestamps:**

Вместо обратной конвертации через 100ns единицы — хранить оригинальные RTP timestamps в порядке FIFO и брать их при выводе:

```cpp
// mft_h264_decoder_impl.h
std::deque<uint32_t> input_rtp_queue_;

// В Decode() — после успешного ProcessInput():
input_rtp_queue_.push_back(input_image.RtpTimestamp());

// В ProcessOutput() — вместо GetSampleTime() + конвертации:
uint32_t rtp_ts = 0;
if (!input_rtp_queue_.empty()) {
    rtp_ts = input_rtp_queue_.front();
    input_rtp_queue_.pop_front();        // точное оригинальное значение, без потерь
} else {
    // fallback (не должен срабатывать в штатном режиме)
}
```

Это корректно работает при любой фиксированной задержке декодера (MFT с MF_LOW_LATENCY имеет задержку 1 кадр): входные RTP timestamps поступают в той же последовательности, в которой MFT производит выходные кадры.

**Результат:** `[NativeVideoSink] OnFrame #0 rtp=... 2560x1440` появился в логах, стрим начал отображаться.

**Изменённые файлы:**
- `webrtc-sys/src/mft/mft_h264_decoder_impl.h` — `std::deque<uint32_t> input_rtp_queue_` (вместо `uint32_t last_input_rtp_ts_`)
- `webrtc-sys/src/mft/mft_h264_decoder_impl.cpp` — `Decode()`: push после ProcessInput; `ProcessOutput()`: pop front вместо GetSampleTime-конвертации; `Release()`: `input_rtp_queue_.clear()`
- `webrtc-sys/src/video_track.cpp` — диагностический лог в `NativeVideoSink::OnFrame()`

---

## ✅ Исправлено: STATUS_STACK_BUFFER_OVERRUN в тесте D3D11 FFI (Phase 3.1)

**Симптом:** Тест `test_d3d11_ffi_with_i420_buffer` падал с exit code `0xc0000409` (STATUS_STACK_BUFFER_OVERRUN) при вызове `video_frame_buffer_get_d3d11_texture` или `video_frame_buffer_get_d3d11_subresource` для I420 буфера.

**Диагностика:**
- `video_frame_buffer_is_d3d11(&vfb)` — проходил
- `video_frame_buffer_get_d3d11_texture(&vfb)` — crash
- Причина: повторный вызов `vfb->get().get()` и `dynamic_cast` в `get_*` функциях вызывал crash (вероятно ABI/GS при множественных переходах через границу webrtc lib)

**Исправление (2025-03):**

1. **FFI принимает `&UniquePtr<VideoFrameBuffer>`** вместо `&VideoFrameBuffer` или raw pointer — cxx корректно передаёт managed pointer.

2. **Ранний выход в `get_d3d11_texture` и `get_d3d11_subresource`:** сначала вызывается `video_frame_buffer_is_d3d11(buf)`. Для I420 (и любого не-D3D11 буфера) возвращается 0 без повторного доступа к `vfb->get()` и `dynamic_cast`:
   ```cpp
   uintptr_t video_frame_buffer_get_d3d11_texture(...) {
     if (!buf || !buf.get()) return 0;
     if (!video_frame_buffer_is_d3d11(buf)) return 0;  // ранний выход
     // ... только для D3D11
   }
   ```

**Изменённые файлы:**
- `webrtc-sys/include/livekit/video_frame_buffer.h` — сигнатуры с `const std::unique_ptr<VideoFrameBuffer>&`
- `webrtc-sys/src/video_frame_buffer.cpp` — ранний return в get_texture/get_subresource
- `webrtc-sys/src/video_frame_buffer.rs` — FFI `buf: &UniquePtr<VideoFrameBuffer>`

**Результат:** тест проходит. D3D11 path будет проверен в Phase 3.4.

---

## Оценка трудозатрат

| Phase | Описание | Оценка |
|-------|----------|--------|
| 0 | Исследование MFT decoder API | ✅ |
| 1.1–1.7 | MftH264DecoderImpl software path | ✅ |
| 1.8 | Hardware MFT: D3D11DeviceManager + D3D11TextureVideoFrameBuffer | 1–2 дня |
| 2 | MftVideoDecoderFactory, интеграция | ✅ |
| 3 | Zero CPU readback: NV12 compute shader + FFI + egui interop | 2–3 дня |
| 4 | Валидация: PSNR, benchmarks, stress test | 1–2 дня |
| 5 | Оптимизация latency и стабильность (encoder side) | ✅ |

**Итого оставшееся:** ~4–7 дней.

---

## Ссылки

| Ресурс | URL |
|--------|-----|
| H.264 Video Decoder | https://learn.microsoft.com/en-us/windows/win32/medfound/h-264-video-decoder |
| MFTEnumEx | https://learn.microsoft.com/en-us/windows/win32/api/mfapi/nf-mfapi-mftenumex |
| IMFDXGIDeviceManager (decoder) | https://learn.microsoft.com/en-us/windows/win32/api/mfobjects/nn-mfobjects-imfdxgidevicemanager |
| webrtc VideoDecoder | https://source.chromium.org/chromium/chromium/src/+/main:third_party/webrtc/api/video_codecs/video_decoder.h |
| NVIDIA decoder (референс) | `webrtc-sys/src/nvidia/h264_decoder_impl.cpp` |

---

## Phase 5: Оптимизация latency и стабильность (encoder side + viewer I420→RGBA)

### Контекст

При использовании **GPU encoder (MFT) + CPU decoder (OpenH264)** наблюдалась задержка >1 секунды и неплавная трансляция. Также при запуске двух клиентов на одном ПК (publisher + viewer) смотрящий клиент вылетал. На разных ПК оба пути работали без проблем.

**RTP timestamps и задержка:** Неправильные RTP-метки (Unix epoch вместо frame-based) вызывают задержку: jitter buffer на приёмнике использует их для расчёта playout time; при неверных метках буферизация растёт. Исправление: `ts_us = frame_count * frame_interval_us` в MFT path. Клиент должен быть пересобран.

### Диагностика

| Проблема | Причина | Статус |
|----------|---------|--------|
| **>1 с задержка** | MFT encoder без MF_LOW_LATENCY → NVENC буферизирует 1–3 кадра | ✅ Исправлено |
| **>1 с задержка** | Timestamp из MFT output sample (может отличаться от capture time) → jitter buffer на приёмнике добавляет буферизацию | ✅ Исправлено |
| **>1 с задержка** | Warmup 30 кадров × 66.7 мс = 2 с при 15 fps — избыточно для MFT | ✅ Исправлено |
| **>1 с задержка, растёт с разрешением** | Длинный GOP (250+), B-frames, lookahead, очередь encoder → накопление кадров | ✅ 5.7–5.10 |
| **Неплавная трансляция** | `poll_event` с 2 мс sleep между попытками → 12% джиттер на 60 fps | ✅ Исправлено |
| **Краш viewer на одном ПК** | D3D11 resource contention: publisher (WGC+BGRA→NV12+NVENC) и viewer (D3d11I420ToRgba) на одном GPU → device-lost без обработки | ✅ Исправлено |

### Выполненные задачи

- [x] **5.1** `MF_LOW_LATENCY` на MFT encoder — `IMFAttributes::SetUINT32(&MF_LOW_LATENCY, 1)` перед `BEGIN_STREAMING`. Устраняет буферизацию кадров для реордеринга в NVENC. GUID: `{9c27891a-ed7a-40e1-88e8-b22727a024ee}` (= `CODECAPI_AVLowLatencyMode`).

- [x] **5.2** Оригинальный timestamp вместо MFT sample time — `extract_h264_from_sample()` теперь принимает `original_timestamp_us` и использует его в `EncodedFrame` вместо `sample.GetSampleTime()`. MFT может перезаписывать timestamp при frame reordering; оригинальный capture time гарантирует корректную работу jitter buffer на приёмнике.

- [x] **5.3** Сокращение warmup для MFT пути — 10 кадров × 33.3 мс = ~333 мс при 30 fps (вместо 30 × 66.7 мс = 2 с при 15 fps). CPU путь (OpenH264) сохраняет прежний warmup для BWE convergence.

- [x] **5.4** Замена busy-poll в `poll_event` — вместо `get_event_no_wait()` + `sleep(2ms)` теперь: fast path (non-blocking check) → slow path (`GetEvent(0)` blocking). Устраняет 2 мс джиттер на каждый кадр.

- [x] **5.5** Device-lost обработка в `D3d11I420ToRgba` — добавлен `DeviceLost` error variant и `check_device_lost()` (`GetDeviceRemovedReason()`) после GPU операций. Timeout 2 с на GPU query wait. При ошибке — `mark_decode_gpu_failed()` → постоянный fallback на CPU RGBA. Лог: `[voice][screen][viewer] D3D11 I420→RGBA failed: ...`.

- [x] **5.6** Переключатель decode path в настройках UI — поле `decode_path: String` в `Settings` (`ui.rs`), ComboBox в окне настроек (CPU/MFT), сохранение при закрытии. `std::env::set_var("ASTRIX_DECODE_PATH", ...)` перед `VoiceCmd::Start`.

- [x] **5.7** B-frames=0 в encoder — ICodecAPI (`CODECAPI_AVEncMPVDefaultBPictureCount = 0`, `CODECAPI_AVEncCommonLowLatency = 1`). Добавлен feature `Win32_System_Variant` в windows crate. Уменьшает буферизацию и frame reordering в NVENC.

- [x] **5.8** DXVA в decoder — `CODECAPI_AVDecVideoAcceleration_H264 = TRUE` (экспериментально; ранее FALSE из-за STATUS_STACK_BUFFER_OVERRUN). При ошибке SetValue — fallback на software decode.

- [x] **5.9** GOP=fps в encoder — `CODECAPI_AVEncMPVGOPSize = fps` (e.g. 60). Для low-latency streaming: keyframe каждую секунду вместо длинного GOP (250+), уменьшает накопление данных в encoder.

- [x] **5.10** Skip to newest при переполнении очереди — `GpuSlotRing::pop_skip_to_newest_if_behind()`: когда 2+ кадров в очереди (encoder отстаёт), берём только самый новый. Устраняет рост latency при высоком разрешении, когда encode медленнее capture.

- [x] **5.11** Drain пакетов: sync MFT использует `drain_output()` (полный цикл). Async MFT (NVENC) — только `drain_output_once()`: один HaveOutput на sample; повторный ProcessOutput без нового HaveOutput даёт E_UNEXPECTED.

- [x] **5.12** RTP timestamp: фиксированный шаг `frame_index * (90000 / fps)` вместо `timestamp_us * 90 / 1000`. `capture_time_ms` = реальное время с начала стрима. Устраняет накопление jitter buffer при дрейфе timestamp. Диагностика: `ASTRIX_DEBUG_RTP_TS=1` — лог rtp/capture_ms для каждого кадра.

### Изменённые файлы

| Файл | Изменения |
|------|-----------|
| `client/src/mft_encoder.rs` | `MF_LOW_LATENCY` GUID + SetUINT32; ICodecAPI: `CODECAPI_AVEncMPVDefaultBPictureCount=0`, `CODECAPI_AVEncCommonLowLatency=1`, `CODECAPI_AVEncMPVGOPSize=fps`; sync MFT: `drain_output()`; async MFT: `drain_output_once()` (повторный ProcessOutput без HaveOutput → E_UNEXPECTED); `encode()` / `encode_async()` передают `original_timestamp_us`; `poll_event()` — blocking GetEvent вместо busy-poll |
| `client/Cargo.toml` | Features `Win32_System_Ole`, `Win32_System_Variant` для ICodecAPI SetValue (VARIANT) |
| `client/src/voice_livekit.rs` | Warmup: `WARMUP_FRAMES_MFT=10` / `WARMUP_FPS_MFT=30.0` (vs CPU: 30/15); `GpuSlotRing::pop_skip_to_newest_if_behind()` — skip to newest при 2+ кадров в очереди; логирование при fallback `D3d11I420ToRgba` |
| `client/src/d3d11_rgba.rs` | `DeviceLost` error variant; `check_device_lost()` через `GetDeviceRemovedReason()`; timeout 2 с на GPU query; проверки после UpdateSubresource, Dispatch, CopyResource, Map |
| `client/src/ui.rs` | `Settings.decode_path`; ComboBox в настройках; `set_var("ASTRIX_DECODE_PATH")` перед `VoiceCmd::Start`; `settings.save()` при закрытии |
| `webrtc-sys/src/mft/mft_h264_decoder_impl.cpp` | `CODECAPI_AVDecVideoAcceleration_H264 = VARIANT_TRUE` (экспериментально; ранее FALSE из-за STATUS_STACK_BUFFER_OVERRUN) |

---

## Следующие шаги

1. ~~Выполнить Phase 0 (исследование)~~ ✅
2. ~~Реализовать Phase 1 (MftH264DecoderImpl)~~ ✅
3. ~~Интегрировать в VideoDecoderFactory (Phase 2)~~ ✅
4. ~~Phase 5: Оптимизация latency и стабильность~~ ✅
5. ~~Отладить STATUS_STACK_BUFFER_OVERRUN~~ ✅ (double-free ComPtr + MFT_OUTPUT_STREAM_PROVIDES_SAMPLES + Lock2D)
6. ~~Отладить «стрим не начинается»~~ ✅ (FIFO input_rtp_queue_ — устранён off-by-1 в RTP timestamp)
7. **Phase 1.8** — Hardware MFT: D3D11 device + IMFDXGIDeviceManager + D3D11TextureVideoFrameBuffer
8. **Phase 3** — Zero CPU readback: NV12→RGBA compute shader + FFI + egui interop
9. **Phase 4** — Валидация: PSNR, CPU/GPU benchmarks, stress test, fallback, iGPU

---

## Диагностика: DXVA enable failed
**Симптом:** `[MFT decoder] DXVA enable failed, using software decode` — SetValue(CODECAPI_AVDecVideoAcceleration_H264) возвращает ошибку.

**Добавленные логи (для сужения круга поиска):**

| Лог | Место | Назначение |
|-----|-------|------------|
| `ICodecAPI query failed, hr=...` | CreateMftDecoder | Не удаётся получить ICodecAPI от MFT |
| `DXVA enable failed, hr=... (0x80070057=E_INVALIDARG, 0x80004001=E_NOTIMPL)` | CreateMftDecoder | Точный HRESULT при SetValue — позволяет определить причину (не поддерживается, неверный аргумент и т.д.) |
| `MF_SA_D3D_AWARE: get_hr=... value=... (0=no D3D, 1=D3D9, 11=D3D11)` | CreateMftDecoder | Поддержка D3D у MFT; для DXVA нужен D3D device manager |
| `Setting input/output types (width=... height=...)` | CreateMftDecoder | Проверка, что доходим до SetInputType до краша |
| `SetInputType failed` | CreateMftDecoder | Сбой при установке input type |
| `OutputStreamInfo: PROVIDES_SAMPLES=... FIXED_SIZE=... cbSize=...` | ProcessOutput (первый вызов) | PROVIDES_SAMPLES=1 означает DXVA/D3D11 surface path |
| `DXVA path: MFT returned pSample=null (unexpected)` | ProcessOutput | MFT должен вернуть sample при PROVIDES_SAMPLES |
| `Lock2D OK: stride=... (DXVA surface path)` | ProcessOutput | Успешный Lock2D на DXVA surface |
| `Lock2D failed, hr=... falling back to Lock` | ProcessOutput | Lock2D не сработал — fallback на Lock |
| `ProcessOutput failed, hr=... status=...` | ProcessOutput | Ошибка декодирования (0xC00D36B2=MF_E_INVALIDREQUEST) |
| `MF_E_TRANSFORM_STREAM_CHANGE, calling HandleStreamChange` | ProcessOutput | Смена формата (часто при первом keyframe в DXVA) |

**Интерпретация HRESULT при DXVA SetValue:**
- `0x80070057` (E_INVALIDARG) — **исправлено:** тип VARIANT должен быть VT_UI4 (UINT32), не VT_BOOL. Документация: Data type = UINT32 (VT_UI4).
- `0x80004001` (E_NOTIMPL) — кодек не поддерживает CODECAPI для MFT (документация: для MF нужен D3D11 video decoding через MFT_MESSAGE_SET_D3D_MANAGER)

**Исправление E_INVALIDARG (2025-03):** Заменён `var.vt = VT_BOOL; var.boolVal = VARIANT_TRUE` на `var.vt = VT_UI4; var.ulVal = 1`. Microsoft: "Data type: UINT32 (VT_UI4)".

---

## Исправлено: зависание стрима и клиента после Phase 1.8 (2025-03)

**Симптомы:**
- Стрим зависает после первых кадров
- Дисконнект смотрящего клиента через 3–5 секунд
- Клиент не закрывается по крестику (перестаёт отвечать)
- В логах: `[MFT decoder] Decode() #9` — дальше сообщений нет

**Причины и исправления:**

1. **Лог "pSample=null (unexpected)"** — при `MF_E_TRANSFORM_NEED_MORE_INPUT` MFT не заполняет `pSample`, это ожидаемо. Лог выводился до проверки `hr`, что вводило в заблуждение. **Исправление:** проверка `hr` перенесена до обработки `pSample`; сообщение "unexpected" выводится только при `S_OK` и `pSample=null`.

2. **Блокировка Map() в ToI420** — `ID3D11DeviceContext::Map` на staging-текстуре ждёт завершения GPU-копирования. При contention (один GPU для publisher+viewer) Map может блокироваться надолго или бесконечно. **Исправление:** `D3D11_MAP_FLAG_DO_NOT_WAIT` + retry-цикл (до 1000 попыток по 1 мс ≈ 1 с) при `DXGI_ERROR_WAS_STILL_DRAWING`; при таймауте возвращается `nullptr`. Диагностика: `[D3D11TextureVideoFrameBuffer] ToI420 Map TIMEOUT` и `[MFT decoder] ToI420 failed` в логах.

3. **Flush перед Map** — добавлен `ctx->Flush()` после `CopySubresourceRegion`, чтобы команды были отправлены на GPU до Map.

**Изменённые файлы:**
- `webrtc-sys/src/mft/mft_h264_decoder_impl.cpp` — порядок проверок `hr`/`pSample`, явный return при `S_OK`+`pSample=null`
- `webrtc-sys/src/mft/d3d11_texture_video_frame_buffer.cpp` — Flush, Map с `DO_NOT_WAIT` и retry

**Примечание о закрытии клиента:** если клиент всё ещё не закрывается, возможна блокировка в `stream.next().await` (ожидание следующего кадра при остановленном декодере). Нужно обеспечить корректное завершение voice-сессии при закрытии окна (abort stream task, `clear_mft_device()`).

**4. Multithread protection для D3D11 device (MS docs):** Microsoft рекомендует включить `ID3D10Multithread::SetMultithreadProtected(TRUE)` на D3D11 device для предотвращения deadlock при вызовах MFT `GetDecoderBuffer`/`ReleaseDecoderBuffer`. **Исправление:** в `CreateMftDecoder` после получения `d3d11_device_` — QI для `ID3D10Multithread`, вызов `SetMultithreadProtected(TRUE)`. Лог: `[MFT decoder] D3D11 device: multithread protection ENABLED`.

**5. MFStartup — обязательная инициализация Media Foundation (2025-03):** Без вызова `MFStartup(MF_VERSION, 0)` перед любым использованием MFT декодер зависает после ~8 кадров в `CDXVAFrameManager::WaitOnSampleReturnedByRenderer`. Media Foundation не сообщает об ошибке — просто блокируется. **Исправление:** в `mft_decoder_factory.cpp` добавлен `EnsureMFStartup()` — однократный вызов `MFStartup` при первом `CheckMftH264DecoderAvailable()` или `Create()`. Лог: `[MFT factory] MFStartup OK (required before any MFT use)`. **Источник:** [Stack Overflow: MFTransform::ProcessOutput hangs](https://stackoverflow.com/questions/77838802/mftransformprocessoutput-hangs-in-the-function-cdxvaframemanagerwaitonsample).

**Дополнительно (ускорение возврата surface в пул MFT):** Раннее освобождение D3D11 surface — `hw_buf = nullptr` и `mft_sample.Reset()` сразу после `ToI420()`, до вызова `Decoded()` callback. Это возвращает surface в пул MFT до того, как callback (и downstream pipeline) завершит обработку. DrainOutput перед ProcessInput — освобождает pending frames перед подачей нового input.

**Изменённые файлы (п. 5):**
- `webrtc-sys/src/mft/mft_decoder_factory.cpp` — `EnsureMFStartup()`, `#include <mfapi.h>`
- `webrtc-sys/src/mft/mft_h264_decoder_impl.cpp` — DrainOutput перед ProcessInput; раннее освобождение surface (`hw_buf = nullptr`, `mft_sample.Reset()`)

**Результат:** зависание устранено, клиент стабильно воспроизводит screen share через MFT hardware decoder.

**Если стрим всё ещё зависает после исправлений:**
1. Проверить лог `[MFT factory] MFStartup OK` — если отсутствует или `MFStartup failed`, MFT не инициализирован.
2. Проверить логи на `ToI420 Map TIMEOUT` или `ToI420 failed` — если есть, причина в GPU contention.
3. Проверить лог `multithread protection ENABLED` — если `QI failed`, возможен deadlock.

---

## ✅ Исправлено: белый оттенок трансляции + пустое окно после остановки стрима (2025-03)

**Симптомы (после внедрения zero-copy + register_native_glow_texture):**
1. Трансляция имеет белый оттенок (вся полностью)
2. После остановки стрима остаётся пустое окно стрима вместо возвращения плитки пользователя

**Исправления:**

### 1. Пересвеченность (белый оттенок) — sRGB double conversion (гипотеза)

**Гипотеза:** NV12 shader выдаёт linear RGB, egui/GL ожидает sRGB. При включённом `GL_FRAMEBUFFER_SRGB` происходит:
`linear → sRGB (egui) → sRGB (framebuffer)` → двойная gamma → высветление, низкий контраст.

**Цепочка GPU path:**
```
NV12 → compute shader (linear RGB)
     → D3D11 RGBA texture (DXGI_FORMAT_R8G8B8A8_UNORM)
     → WGL_NV_DX_interop
     → OpenGL texture (GL_RGBA8)
     → egui (gamma conversion)
```

**Диагностика (добавлено 2025-03):**

1. **Быстрый тест:** `ASTRIX_VIDEO_DISABLE_FRAMEBUFFER_SRGB=1` — перед рендером egui вызывается `glDisable(GL_FRAMEBUFFER_SRGB)`. Если картинка станет нормальной — это double sRGB.
2. **Лог:** При первом кадре с GPU-видео выводится `[Phase 3.5] GL_FRAMEBUFFER_SRGB was enabled/disabled`.
3. **Формат текстуры:** WGL interop создаёт GL-текстуру из D3D11 `R8G8B8A8_UNORM` — это linear, не `GL_SRGB8_ALPHA8`. Проблема скорее в framebuffer, не в формате текстуры.

**Варианты исправления (если тест подтвердит):**
- **Вариант 1 (проще):** Linear pipeline: shader → linear, `GL_RGBA8`, `glDisable(GL_FRAMEBUFFER_SRGB)` для всего кадра.
- **Вариант 2:** sRGB pipeline: в shader `rgb = pow(rgb, 1/2.2)`, использовать `GL_SRGB8_ALPHA8` (требует изменения WGL interop).

**Почему CPU path работает:** `ColorImage` (u8) egui всегда считает sRGB. Pipeline совпадает.

**Тест диапазона (ASTRIX_VIDEO_DEBUG_GRAY_Y=1):** шейдер выводит grayscale из Y с limited-range:
`y = saturate((y - 0.0625) * 1.164)`. Если контраст станет нормальным — проблема 100% в диапазоне.

**PlaneSlice для NV12 SRV (исправлено):** NV12 имеет 2 plane: 0=Y, 1=UV. Без `PlaneSlice` драйвер может вернуть не тот plane → пересвечено. Добавлено использование `CreateShaderResourceView1` (D3D11.3) с `PlaneSlice: 0` для Y и `PlaneSlice: 1` для UV.

**Full range vs limited range:** Encoder (BGRA→NV12 CS fallback) теперь пишет limited range по стандарту H.264: `Y = 16 + Y*219`, `CbCr = 128 + (cb,cr)*224`. Decoder по умолчанию limited range. `ASTRIX_VIDEO_NV12_FULL_RANGE=1` — для full range потоков.

**Gamma-коррекция (ASTRIX_VIDEO_DECODER_GAMMA):** При WGL zero-copy цвета могут отличаться от CPU path. `ASTRIX_VIDEO_DECODER_GAMMA=0.55` применяет `pow(rgb, 1/0.55)` в декодере — приближение к реальным цветам для WGC→shader→NV12 pipeline. Значение 0 или не задано — без gamma. Работает совместно с `OUTPUT_SRGB`.

**Билинейная UV (ASTRIX_VIDEO_UV_BILINEAR=1):** Point sampling UV в 4:2:0 даёт едва заметные вертикальные полосы на градиентах. Билинейная интерполяция 4 соседних UV-текселей уменьшает артефакты.

**Гамма из настроек (вариант 4):** В меню настроек клиента добавлен слайдер «Гамма декодера (MFT, GPU)»: 0..3.00, шаг 0.01. 0 = выкл. Значение применяется при просмотре стрима (runtime через cbuffer). По умолчанию 0.55. Сохраняется в `astrix_settings.json` как `video_decoder_gamma`.

**Тестирование цветов по этапам (ASTRIX_VIDEO_COLOR_STAGE=1,2,3,4):**

| Этап | Команда | Что проверяем |
|------|---------|---------------|
| **1** | `set ASTRIX_VIDEO_COLOR_STAGE=1` | Стандартная sRGB: `pow(rgb, 1/2.2)` в шейдере |
| **2** | `set ASTRIX_VIDEO_COLOR_STAGE=2` | Линейный вывод (без sRGB) — тест двойной gamma. *SRGB-текстура не поддерживает UAV в D3D11* |
| **3** | `set ASTRIX_VIDEO_COLOR_STAGE=3` + `set ASTRIX_VIDEO_DISABLE_FRAMEBUFFER_SRGB=1` | Как этап 1, но отключён GL_FRAMEBUFFER_SRGB |
| **4** | `set ASTRIX_VIDEO_COLOR_STAGE=4` | Только `pow(rgb, 1/0.55)`, без OUTPUT_SRGB (как при ручном тесте) |

### 2. Пустое окно и мерцание плитки после остановки стрима

**Исправление:** В `main_screen` (ui.rs):
- Используем только `voice.participants` для `streaming_user_ids`
- Debounce: удаляем текстуру после 2 последовательных кадров с `streaming=false` (`stream_ended_prev_frame`)
- Сбрасываем `fullscreen_stream_user` при удалении текстуры или когда WS сообщает `streaming=false`

**Файл:** `client/src/ui.rs`
