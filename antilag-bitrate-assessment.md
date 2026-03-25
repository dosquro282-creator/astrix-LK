# Оценка: исправление bitrate и latency (MFT H.264)

Оценка возможности реализации предложенных оптимизаций. Проблемы: **100–150 Mbps на 2 стрима** и **задержка**.

---

## Текущее состояние

### Encoder (mft_encoder.rs)
- **B-frames**: 0 ✅
- **GOP**: fps (1 сек) — ок для low latency
- **MF_LOW_LATENCY**: установлен ✅
- **ICodecAPI**: B-frames, LowLatency, GOP — используются
- **Bitrate**: только `MF_MT_AVG_BITRATE` в media type — **NVENC может игнорировать** и использовать CQP по умолчанию
- **Rate control**: **не задан** — вероятная причина аномального bitrate
- **set_bitrate()**: stub, не вызывает ICodecAPI

### Presets (voice.rs)
| Preset   | Bitrate   | Норма (по таблице) |
|----------|-----------|---------------------|
| 720p60   | 20 Mbps   | 4–6 Mbps            |
| 1080p60  | 35 Mbps   | 6–10 Mbps           |
| 1080p120 | 70 Mbps   | 12–16 Mbps          |
| 1440p90  | 75 Mbps   | ~16–20 Mbps         |

Presets в **3–5× выше** нормы. Даже с rate control, target bitrate завышен.

### Decoder (mft_h264_decoder_impl.cpp)
- **MF_LOW_LATENCY**: установлен ✅
- **CODECAPI_AVDecVideoAcceleration_H264**: DXVA включён
- **CODECAPI_AVLowLatencyMode**: не вызывается отдельно (MF_LOW_LATENCY = тот же GUID)

### Pipeline
- Capture: WGC → BGRA
- Encode: BGRA→NV12 (VideoProcessor/CS) → MFT → H.264
- Decode: MFT → NV12 → D3d11Nv12ToRgba (compute) → RGBA → egui

---

## Оценка по пунктам

### 1. Rate control + MeanBitRate — **КРИТИЧНО, РЕАЛИЗУЕМО**

**Проблема:** NVENC MFT по умолчанию может использовать CQP/Quality mode → bitrate не ограничен.

**Решение:**
- `CODECAPI_AVEncCommonRateControlMode` = 4 (LowDelayVBR) или 0 (CBR)
- `CODECAPI_AVEncCommonMeanBitRate` = target bps (например 8_000_000)

**GUID (вручную, если нет в windows-rs):**
- RateControlMode: `{1c0608e9-370c-4710-8a58-cb6181c42423}`
- MeanBitRate: `{f7222374-2144-4815-b550-a37f8e12ee52}`

**Сложность:** низкая. Добавить в блок ICodecAPI после SetOutputType, до BEGIN_STREAMING.

**Риск:** NVENC MFT может не поддерживать LowDelayVBR (E_NOTIMPL) — тогда пробовать CBR.

---

### 2. Снижение target bitrate в presets — **РЕАЛИЗУЕМО**

**Текущие значения** завышены для hardware encoder (NVENC эффективнее OpenH264).

**Предложение (примерно):**
| Preset   | Было   | Предложение |
|----------|--------|-------------|
| 720p30   | 10     | 4–5 Mbps    |
| 720p60   | 20     | 6–8 Mbps    |
| 720p120  | 40     | 12–16 Mbps  |
| 1080p30  | 24     | 6–8 Mbps    |
| 1080p60  | 35     | 8–12 Mbps   |
| 1080p120 | 70     | 16–20 Mbps  |
| 1440p30  | 35     | 10–12 Mbps  |
| 1440p60  | 50     | 14–18 Mbps  |
| 1440p90  | 75     | 18–24 Mbps  |

**Сложность:** очень низкая. Изменить `voice.rs` ScreenPreset::params().

---

### 3. GOP = 2 секунды — **ОПЦИОНАЛЬНО**

**Сейчас:** GOP = fps (1 сек). Для 60 fps → keyframe каждую секунду.

**Предложение:** GOP = fps × 2 (2 сек). Меньше keyframe → меньше всплесков bitrate, но чуть больше latency при seek.

**Сложность:** низкая. `CODECAPI_AVEncMPVGOPSize = fps * 2`.

**Приоритет:** средний. Текущий GOP уже разумный.

---

### 4. Slice mode (2–4 slices) — **СЛОЖНО ОЦЕНИТЬ**

**Контекст:** OpenH264 patch (h264_encoder_params.patch) задаёт `uiSliceNum`. MFT/NVENC — другой API.

**MFT:** ICodecAPI может иметь `CODECAPI_AVEncVideoNumMPEG2Slices` или аналог для H.264. Документация скудная. NVENC через MFT может не экспонировать slice control.

**Сложность:** средняя/высокая. Нужно искать GUID, тестировать поддержку.

**Приоритет:** низкий. Основной выигрыш — от rate control.

---

### 5. NV12 → VideoProcessor → swapchain (без RGBA) — **СЛОЖНАЯ АРХИТЕКТУРА**

**Сейчас:** decode → NV12 texture → D3d11Nv12ToRgba (compute) → RGBA → egui/OpenGL.

**Предложение:** decode → NV12 texture → ID3D11VideoProcessor → swapchain. GPU рендерит NV12 напрямую.

**Проблема:** egui рисует в RGBA. Чтобы рендерить NV12 напрямую, нужен:
- отдельный D3D11 swapchain/overlay для видео, или
- интеграция VideoProcessor в цепочку egui (egui-glow/egui-wgpu)

**Сложность:** высокая. Требует переработки render path.

**Приоритет:** низкий. D3d11Nv12ToRgba на GPU уже быстрый (~1–2 ms).

---

### 6. RTP MTU ≈ 1200 — **ПРОВЕРИТЬ**

WebRTC/LiveKit обычно используют ~1200–1400 bytes. Конфиг в libwebrtc, не в нашем коде.

**Действие:** при необходимости — проверить `webrtc-sys` / libwebrtc build flags. Скорее всего уже ок.

**Приоритет:** низкий.

---

### 7. Frame pacing / timestamps — **УЖЕ СДЕЛАНО**

antilag.md: RTP step = 90000/fps, capture_time = wall-clock. ✅

---

### 8. Color conversion (BGRA→NV12) — **УЖЕ НА GPU**

VideoProcessor или compute shader, без GPU→CPU копий. ✅

---

### 9. SFU playout_delay — **УЖЕ СДЕЛАНО**

livekit.yaml: playout_delay min=0, max=0. Per-frame SetPlayoutDelay(Minimal()) в video_track.cpp. ✅

---

### 10. Decoder CODECAPI_AVLowLatencyMode — **УЖЕ ЕСТЬ**

MF_LOW_LATENCY = CODECAPI_AVLowLatencyMode (тот же GUID). Decoder уже устанавливает MF_LOW_LATENCY. ✅

---

## Рекомендуемый порядок реализации

| # | Задача                               | Сложность | Ожидаемый эффект        |
|---|--------------------------------------|------------|--------------------------|
| 1 | Rate control + MeanBitRate в MFT     | Низкая     | **Снижение bitrate в 5–10×** |
| 2 | Снижение bitrate в presets           | Очень низкая | Доп. снижение, стабильность |
| 3 | GOP = fps×2 (опционально)            | Низкая     | Меньше keyframe spikes   |
| 4 | Slice mode (если найдём API)         | Средняя    | Меньше packet size       |
| 5 | NV12 direct render                  | Высокая    | −1–2 ms decode path     |

---

## Итог

**Главная причина 100–150 Mbps:** отсутствие rate control в MFT encoder. NVENC, скорее всего, кодирует в CQP/Quality mode и не ограничивает bitrate.

**Минимальный набор для быстрого эффекта:**
1. Добавить `CODECAPI_AVEncCommonRateControlMode` (LowDelayVBR или CBR) и `CODECAPI_AVEncCommonMeanBitRate` в mft_encoder.rs.
2. Снизить target bitrate в ScreenPreset::params() до разумных значений (см. таблицу выше).

Ожидаемый результат: bitrate 2 стримов с ~100–150 Mbps до ~15–30 Mbps при сопоставимом качестве.
