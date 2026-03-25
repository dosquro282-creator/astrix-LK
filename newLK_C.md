# Roadmap: EncodedVideoSource (Вариант C)

Цель: добавить новый тип видеоисточника в libwebrtc, который сразу отдаёт `EncodedImage` в RTP-пайплайн, обходя слой `VideoEncoder`.

**Целевая платформа:** только Windows.

---

## Идея

```
WGC → RGBA → NVENC/AMF/QSV → H.264 NAL
                                    ↓
EncodedVideoSource::PushEncodedFrame(EncodedImage)
                                    ↓
RTP packetizer (внутри libwebrtc) → SRTP → transport
```

Отличие от варианта A: не создаём fake VideoEncoder, а добавляем новый путь в архитектуре — источник, который производит уже закодированные кадры.

---

## Phase 0: Исследование

- [ ] **0.1** Изучить libwebrtc: `VideoTrackSource`, `VideoSource`, `VideoBroadcaster` — как raw frames попадают в `VideoEncoder`
- [ ] **0.2** Найти в коде: где `EncodedImage` передаётся в RTP packetizer (после `EncodedImageCallback::OnEncodedImage`)
- [ ] **0.3** Проверить: есть ли в Chromium/WebRTC «encoded frame source» — например, для Android hardware encoder или iOS VideoToolbox
- [ ] **0.4** Изучить `VideoSendStream`, `VideoStreamEncoder` — можно ли подать `EncodedImage` в обход `Encode()`
- [ ] **0.5** Сравнить с вариантом A: вариант C — архитектурно чище (нет fake encoder), но требует больше изменений в C++

**Результат:** понимание, где разорвать цепочку и вставить `EncodedImage` напрямую.

---

## Phase 1: Форк и окружение

- [ ] **1.1** Форкнуть libwebrtc (Chromium WebRTC или LiveKit fork)
- [ ] **1.2** Форкнуть `arcas-io/libwebrtc` и `livekit/rust-sdks`
- [ ] **1.3** Настроить сборку с `LK_CUSTOM_WEBRTC`
- [ ] **1.4** Собрать Astrix с форком (без изменений) — убедиться, что всё работает

**Результат:** воспроизводимая сборка.

---

## Phase 2: C++ — EncodedVideoSource

- [ ] **2.1** Реализовать `EncodedVideoSource` (или `EncodedFrameVideoSource`):
  - наследует `VideoTrackSource` или аналог
  - метод `PushEncodedFrame(EncodedImage)` — принимает готовый H.264
  - передаёт `EncodedImage` в `EncodedImageCallback` (тот же, что использует обычный encoder)
- [ ] **2.2** Место врезки:
  - **вариант 2.2a:** Новый `VideoSource` тип, который не генерирует `VideoFrame`, а только `EncodedImage`
  - **вариант 2.2b:** Модификация `VideoStreamEncoder` — добавить путь `OnEncodedImage()` без предшествующего `Encode()`
  - **вариант 2.2c:** Отдельный `EncodedVideoTrack` — track без source, с прямым `PushEncodedFrame`
- [ ] **2.3** Согласовать с `VideoSendStream`: RTP packetizer, RTP header (SSRC, payload type) — уже есть в pipeline
- [ ] **2.4** Обработка RTCP: keyframe requests (FIR/PLI) → callback в Rust → клиентский encoder генерирует IDR
- [ ] **2.5** Обработка BWE: REMB, TMMBR — проброс в клиент (если нужна адаптация bitrate)

**Результат:** libwebrtc принимает `EncodedImage` и отправляет в RTP без повторного кодирования.

---

## Phase 3: FFI и Rust (libwebrtc crate)

- [ ] **3.1** Добавить в `webrtc-sys` FFI для:
  - создание `EncodedVideoSource` (или получение из `VideoTrack`)
  - `PushEncodedFrame(EncodedImage)` или `push_encoded_h264(data, keyframe, timestamp_us, width, height)`
- [ ] **3.2** Структура `EncodedImage` в FFI: data, size, keyframe, timestamp, width, height
- [ ] **3.3** Callback для keyframe request: C++ → Rust (для RTCP FIR/PLI)
- [ ] **3.4** Согласовать thread-safety: encoder thread в Rust → FFI → C++

**Результат:** Rust может передавать encoded H.264 в C++ пайплайн.

---

## Phase 4: LiveKit Rust SDK

- [ ] **4.1** Добавить `EncodedVideoSource` в публичный API (или `LocalVideoTrack::from_encoded_source`)
- [ ] **4.2** Метод `capture_encoded_frame(EncodedFrame)` — аналогично варианту A
- [ ] **4.3** Интеграция с `Room`: publish track с `EncodedVideoSource` вместо `NativeVideoSource`
- [ ] **4.4** Документация и пример

**Результат:** публичный API для pre-encoded H.264.

---

## Phase 5: Интеграция в Astrix (Windows)

- [ ] **5.1** Реализовать NVENC в `screen_encoder.rs` (реальный encode)
- [ ] **5.2** Реализовать AMF, QSV (опционально на первом этапе)
- [ ] **5.3** Переключить screen share: `EncodedVideoSource` + `capture_encoded_frame` вместо `NativeVideoSource` + `capture_frame`
- [ ] **5.4** Fallback: при ошибке HW → текущий путь (I420 → OpenH264)
- [ ] **5.5** Тесты: CPU load, latency, качество

**Результат:** Astrix использует HW-энкодеры через EncodedVideoSource.

---

## Phase 6: Поддержка и upstream

- [ ] **6.1** Стабилизация, edge cases (resolution change, keyframe on demand)
- [ ] **6.2** Подготовка PR в LiveKit (если заинтересуются)
- [ ] **6.3** Стратегия обновлений

---

## Риски и зависимости

| Риск | Митигация |
|------|-----------|
| Архитектура libwebrtc не допускает «encoded-only» source | Исследовать Android/iOS paths — там HW encoder может отдавать encoded напрямую |
| `VideoTrackSource` жёстко завязан на `VideoFrame` | Вариант 2.2b: модифицировать `VideoStreamEncoder` напрямую |
| Большой объём изменений в C++ | Выделить минимальный patch set, документировать |
| Несовместимость с LiveKit server | EncodedImage → RTP — стандартный путь; SFU должен принять |

---

## Сравнение с вариантами A и B

| Аспект | A (Passthrough) | B (RTP injection) | C (EncodedVideoSource) |
|--------|-----------------|-------------------|-------------------------|
| Изменения в libwebrtc | Энкодер | Транспорт | Архитектура (новый source) |
| Объём C++ кода | Средний | Средний–высокий | Высокий |
| Контроль над RTP | Нет | Полный | Нет (внутри libwebrtc) |
| BWE | Встроенный | Свой | Встроенный |
| «Чистота» архитектуры | Hack (fake encoder) | Обход | Наиболее чисто |
| Сложность | Средняя | Высокая | Высокая |

---

## Оценка трудозатрат (грубо)

| Phase | Оценка |
|-------|--------|
| 0 | 2–3 дня |
| 1 | 0.5–1 день |
| 2 | 3–7 дней (критическая часть) |
| 3 | 1–2 дня |
| 4 | 0.5–1 день |
| 5 | 2–4 дня |
| 6 | по мере необходимости |

**Итого:** ~2–3 недели. Сопоставимо с вариантом A, но Phase 2 архитектурно сложнее.
