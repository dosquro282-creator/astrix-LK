# live-state.md — Screen Share Pipeline Audit

Дата: 2026-03-04

---

## Архитектура (кратко, для продолжения в новом диалоге)

**Стек:** Rust desktop app, LiveKit (self-hosted), Windows WGC (screen capture), H264 (OpenH264/NVENC), один слой, без симулкаста.

**Потоки:**
1. **WGC capture** — callback от Windows Graphics Capture, копирует кадр в `RawFrame`, атомарно кладёт в слот `Arc<AtomicPtr<RawFrame>>`, старый указатель дропает (lock-free, без блокировок).
2. **Encoder thread** — по таймеру (preset FPS) забирает из слота один кадр (`swap(null)`), конвертирует в I420 целевого разрешения, отдаёт в `NativeVideoSource::capture_frame()` → LiveKit RTP.

**Конвейер кадра (WGC path):**  
`RGBA (native) → rgba_to_i420()` (libyuv) → при необходимости `I420Buffer::scale(dst)` (libyuv) → I420 → `capture_frame()`. Масштабирование только через libyuv (I420 scale), не через `image::resize`.

**Темп (pacing):** Строго по preset FPS: `next_frame_at` — следующий слот; при опоздании кадр дропается только если уже прошла стартовая фаза **и** опоздание больше одного интервала. Первые **45 кадров** (STARTUP_ACCEPT_FRAMES) не дропаются — стрим сразу «оживает». Warmup: первые 10 кадров @ 15 fps, затем целевой fps.

**Файлы:**
- `client/src/voice_livekit.rs` — LiveKit-сессия, обработчик `StartScreen`, `start_screen_capture()` (WGC + encoder loop), `scale_rgba_to_target_libyuv`, `rgba_to_i420`, логика drop/pacing, лог perf каждые 120 кадров.
- `client/src/voice.rs` — `ScreenPreset`, `params()` (разрешение, fps, битрейт), `VoiceCmd`, симулкаст выключен (`use_simulcast()` не используется).

**Публикация:** В `StartScreen`: лог «Publishing screen: W×H @ fps», `VideoCodec::H264`, `opts.simulcast = false`, `video_encoding` из пресета. Разрешение публикации = разрешение пресета (масштаб делается до encode).

**Пресеты:** P720F30/60, P1080F30/60, P1440F30/60 — разрешение и битрейт в `voice.rs`. P1440F60 на нативном 1440p часто не тянет 60 fps на CPU (рекомендация: P1080F60 или P1440F30).

**Ограничения:** Симулкаст отключён; при включении нужно настроить битрейты слоёв (LOW не 250 kbps). OpenH264 предупреждения (SpsPpsIdStrategy, AdaptiveQuant, BackgroundDetection) — норма, энкодер сам подстраивается под screen content.

---

## Статус этапов

| Этап | Описание | Статус |
|------|----------|--------|
| 1 | Обновить битрейты + симулкаст + TrackPublishOptions | ✅ Завершён |
| 2 | Lock-free захват кадра через AtomicPtr | ✅ Завершён |
| 3 | RGBA scale ДО abgr_to_i420 (оптимальный порядок pipeline) | ✅ Завершён |
| 4 | Warmup phase (10 кадров @ 15fps перед целевым fps) | ✅ Завершён |
| 5 | Frame timing instrumentation (лог каждые 120 кадров) | ✅ Завершён |
| 6 | P1440F60 CPU-предупреждение + изолированный конверсионный слой | ✅ Завершён |

---

## Этап 1 — Битрейты и симулкаст

### Что изменилось

**`client/src/voice.rs` — `ScreenPreset::params()`**

Старые битрейты были занижены для 60fps контента. Обновлены:

| Пресет | Разрешение | FPS | Битрейт (было) | Битрейт (стало) |
|--------|-----------|-----|----------------|-----------------|
| P720F30  | 1280×720  | 30 | 2.5 Мбит/с | 3 Мбит/с |
| P720F60  | 1280×720  | 60 | 4 Мбит/с   | 6 Мбит/с |
| P1080F30 | 1920×1080 | 30 | 5 Мбит/с   | 8 Мбит/с |
| P1080F60 | 1920×1080 | 60 | 8 Мбит/с   | 15 Мбит/с |
| P1440F30 | 2560×1440 | 30 | 8 Мбит/с   | 12 Мбит/с |
| P1440F60 | 2560×1440 | 60 | 14 Мбит/с  | 20 Мбит/с |

**Добавлен метод `ScreenPreset::use_simulcast()`**

```rust
pub fn use_simulcast(self) -> bool {
    matches!(self, Self::P720F60 | Self::P1080F60 | Self::P1440F60)
}
```

60fps пресеты включают simulcast (два слоя: полное разрешение + 720p30 fallback).  
30fps пресеты — simulcast выключен (меньше CPU).

**`client/src/voice_livekit.rs` — `StartScreen` обработчик**

```rust
opts.simulcast = preset.use_simulcast();
```

---

## Этап 2 — Lock-free захват кадра (AtomicPtr)

### Проблема

Старый слот: `Arc<Mutex<Option<(Vec<u8>, u32, u32)>>>`.  
При 60–144 fps WGC callback и encoder thread конкурировали за mutex.  
Если encoder удерживал лок во время `abgr_to_i420` (тяжёлая операция), WGC callback вставал в очередь и терял кадры.

### Решение

Новый слот: `Arc<AtomicPtr<RawFrame>>`.

```rust
struct RawFrame { pixels: Vec<u8>, width: u32, height: u32 }
```

**WGC callback (producer):**
```rust
let new_frame = Box::into_raw(Box::new(RawFrame { ... }));
let old = self.latest.swap(new_frame, Ordering::AcqRel);
if !old.is_null() { drop(unsafe { Box::from_raw(old) }); }
```

**Encoder thread (consumer):**
```rust
let raw_ptr = latest_enc.swap(null_mut(), Ordering::AcqRel);
if raw_ptr.is_null() { /* no new frame, wait */ continue; }
let frame = unsafe { Box::from_raw(raw_ptr) };
```

**Гарантии:**
- WGC callback никогда не блокируется
- Encoder всегда получает самый свежий кадр
- Пропущенные кадры (144Hz → 60fps) явно дропаются (drop на old)
- Memory safety обеспечена: каждый non-null указатель — валидный `Box<RawFrame>`

---

## Этап 3 — Оптимальный порядок pipeline (RGBA scale → I420)

### Проблема

Старый порядок:
```
RGBA (2560×1440, 14.7 МБ) → abgr_to_i420 (14.7 МБ → 5.5 МБ) → I420::scale (5.5 МБ → 2 МБ)
```
`abgr_to_i420` обрабатывает полное разрешение захвата (например, 2560×1440 = 14.7 МБ RGBA).  
`I420::scale()` дополнительно аллоцирует новый буфер.

### Решение

Новый порядок (`convert_rgba_to_i420`):
```
RGBA full-res → scale RGBA → RGBA target-res → abgr_to_i420 → I420
```

Масштабирование RGBA делается **до** конверсии в I420. Реализация: `image::ImageBuffer` + `imageops::resize(..., FilterType::Triangle)` (bilinear). Символ libyuv `ABGRScale` не экспортируется из libwebrtc как C, поэтому используется крейт `image`.

**Выигрыш:** конверсия в I420 выполняется уже для целевого разрешения (меньше пикселей), один alloc I420 под target size.

**Reuse буферов:**
- `scaled_rgba_buf: Vec<u8>` — переиспользуется между кадрами, resize только при изменении разрешения
- `I420Buffer::new(target_w, target_h)` — аллокация только целевого размера (не нативного)

**Изолированный конверсионный слой:**
Вся конверсия сосредоточена в функции `convert_rgba_to_i420(rgba, src_w, src_h, dst_w, dst_h, scaled_buf)`.  
Для замены на NVENC/AMF/QuickSync — достаточно заменить тело этой функции.

---

## Этап 4 — Warmup phase

### Проблема

При старте трансляции:
- LiveKit BWE (bandwidth estimator) не имеет истории → начинает с минимального окна
- RTP jitter buffer пустой
- Encoder не разогрет

Результат: первые 0.5–2 с — тяжёлый lag и артефакты.

### Решение

Первые 10 кадров публикуются на 15 fps:

```rust
const WARMUP_FRAMES: i64 = 10;
const WARMUP_FPS: f64 = 15.0;
let warmup_interval = Duration::from_nanos((1_000_000_000.0 / WARMUP_FPS) as u64);
let full_interval   = Duration::from_nanos(frame_interval_ns);

let interval = if frame_count <= WARMUP_FRAMES { warmup_interval } else { full_interval };
```

За ~667 мс на 15fps LiveKit получает достаточно данных для BWE, после чего encoder переходит на целевой fps без рывка.

---

## Этап 5 — Frame timing instrumentation

Каждые 120 кадров в stderr выводится:

```
[voice][screen] perf @120 frames: scale≈1840µs conv≈1840µs total≈3680µs / 16666µs budget (22%)
[voice][screen] perf @240 frames: scale≈9200µs conv≈9200µs total≈18400µs / 16666µs budget (110% ← BOTTLENECK)
```

Если `total > 60% budget` — выводится маркер `← BOTTLENECK`.

Измеряются:
- `scale_ns` — время RGBA scale (image resize)
- `convert_ns` — время abgr_to_i420
- `total_ns` — всё время обработки кадра (включая I420 alloc, capture_frame)

Аккумуляторы обнуляются каждые 120 кадров, avg вычисляется на окно.

---

## Этап 6 — P1440F60 предупреждение + HW encoder interface

### Предупреждение

При старте P1440F60 в stderr:
```
[voice][screen] WARNING: P1440F60 requires ~800 MB/s RGBA bandwidth + heavy libyuv work.
If FPS < 50, consider switching to P1440F30 or P1080F60.
Hardware encoder (NVENC/AMF/QuickSync) support is planned.
```

### HW encoder interface (заготовка)

Функция `convert_rgba_to_i420` является точкой замены:
```rust
fn convert_rgba_to_i420(
    rgba: &[u8], src_w: u32, src_h: u32,
    dst_w: u32, dst_h: u32,
    scaled_buf: &mut Vec<u8>,
) -> Option<I420Buffer>
```

Для NVENC/AMF: эта функция не используется, вместо неё `NativeVideoSource` заменяется на hardware-encoded track напрямую.  
Изоляция конверсии в одном месте гарантирует минимальный diff при внедрении HW кодирования.

---

## Итоговый pipeline (Windows WGC)

```
Monitor @ 60–144Hz
    │ D3D11 GPU→CPU (WGC, no blocking)
    ▼
AtomicPtr<RawFrame>  ←── WGC callback: swap(new, AcqRel), drop old
    │ encoder steals: swap(null, AcqRel)
    ▼
RGBA scale (image::imageops::resize, Triangle)
RGBA full_res × [R,G,B,A]  →  RGBA target_res × [R,G,B,A]
    │ reused scaled_rgba_buf: Vec<u8>
    ▼
abgr_to_i420 (libyuv SIMD)
RGBA target_res  →  I420 target_res
    │ alloc once per frame at target size only
    ▼
NativeVideoSource::capture_frame()
    │
    ▼
LiveKit RTP encoder → SFU → receivers
```

**Encoder thread timing:**
- `timeBeginPeriod(1)` — Windows timer @ 1ms
- `sleep(deadline - 2ms)` + `spin_loop()` — точное попадание в кадровый бюджет
- `next_frame_at += interval` — без накопленного дрейфа
- Reset при отставании > 1 кадра

---

## Битрейт и кодек (VP9)

### Проблема

При дефолтном VP8 и консервативных битрейтах screen share уходил ~0.1 Мбит/с вместо ожидаемых 12–15 Мбит/с (как в Discord).

### Решение на клиенте

- **Кодек:** для screen share явно задаётся **VP9** (`opts.video_codec = VideoCodec::VP9`). VP8 плохо сжимает экран (текст, границы окон); VP9 даёт в 2–3 раза лучше качество при том же битрейте.
- **Битрейты** уже подняты в `ScreenPreset::params()` (см. Этап 1); `video_encoding` передаётся в `TrackPublishOptions` и уходит в RTP encodings.

Файл: `client/src/voice_livekit.rs` — в обработчике `StartScreen` добавлено `opts.video_codec = VideoCodec::VP9`.

### Конфиг LiveKit Server (livekit.yaml)

В конфиг добавлен блок `room:` с параметрами комнаты. **Важно:** в текущей версии LiveKit Server в `config.RoomConfig` **нет** полей `video_codecs` и `audio_codecs` — при их указании сервер не стартует:

```
could not parse config: yaml: unmarshal errors:
  line 25: field video_codecs not found in type config.RoomConfig
  line 30: field audio_codecs not found in type config.RoomConfig
```

В `livekit.yaml` оставлены только поддерживаемые поля комнаты:

```yaml
room:
  max_participants: 100
  empty_timeout: 300
  departure_timeout: 20
  enable_remote_unmute: false
```

Если сервер не поднимается из‑за неверного конфига, клиент получает **"Room::connect: Engine(Signal(Timeout(\"validate request timed out\")))"** — таймаут из‑за недоступности сервера. После исправления конфига и `docker-compose restart livekit` подключение восстанавливается.

---

## Файлы изменены

| Файл | Что изменено |
|------|-------------|
| `client/src/voice.rs` | Битрейты в `ScreenPreset::params()`, метод `use_simulcast()` |
| `client/src/voice_livekit.rs` | Lock-free AtomicPtr, convert_rgba_to_i420 (image resize), warmup, instrumentation, simulcast, **VideoCodec::VP9** для screen share |
| `livekit.yaml` | Блок `room:` без полей `video_codecs`/`audio_codecs` (не входят в RoomConfig этой версии) |

---

## Правки 2026-03-04 — H264, отключение симулкаста, инструментация

*(Текущий текст выше не менялся; ниже — вставка правок.)*

### Что сделано

1. **Кодек: VP9 → H264 для screen share**
   - В `StartScreen` задаётся `opts.video_codec = VideoCodec::H264`.
   - Причина: VP9 кодировался в софте (CPU) → перегрузка и обвал FPS (1 кадр/с или 20–30 fps с просадками). RTX 3080 и др. поддерживают NVENC H264 — кодирование уходит в GPU, CPU разгружен. Screen share по умолчанию должен быть H264; VP9 только если явно принудительно.

2. **Симулкаст временно отключён**
   - `opts.simulcast = false` для screen share.
   - Раньше симулкаст давал LOW слой 250 kbps, и BWE выбирал его → катастрофическое качество. Для отладки и стабильности публикуется один слой. В `voice.rs` у `use_simulcast()` добавлен `#[allow(dead_code)]` и комментарий: включать снова только после настройки битрейтов слоёв (LOW не 250 kbps).

3. **Лог перед публикацией**
   - Перед `publish_track` выводится:  
     `[voice][screen] Publishing screen: 1920x1080 @ 60 fps (bitrate 15000000 bps)`  
   - Подтверждает, что публикуется именно разрешение пресета (например 1920×1080 для P1080F60), а не нативное разрешение захвата.

4. **Раздельная инструментация scale / abgr_to_i420**
   - Конвейер разбит на две измеряемые части:  
     `scale_rgba_to_target()` (RGBA → целевое разрешение) и `rgba_to_i420()` (RGBA → I420).
   - Каждые 120 кадров лог:  
     `scale=…µs conv=…µs total=…µs / …µs budget (…%)`  
   - Если > 60% бюджета — маркер `← BOTTLENECK`; если средний scale > 8 ms — подсказка рассмотреть libyuv ARGBScale вместо `image::resize`.

5. **Битрейты пресетов**
   - В `voice.rs` уже были нужные значения (P720F30 → 3M, P720F60 → 6M, P1080F30 → 8M, P1080F60 → 15M, P1440F30 → 12M, P1440F60 → 20M). Не менялись.

6. **Частота захвата**
   - Encoder thread по-прежнему работает строго по FPS пресета, забирает последний кадр из AtomicPtr, старый дропается. Буферизации нет — логика не менялась.

### Изменённые файлы (правки)

| Файл | Правки |
|------|--------|
| `client/src/voice_livekit.rs` | H264 вместо VP9, `opts.simulcast = false`, лог «Publishing screen: WxH @ fps», вынесены `scale_rgba_to_target()` и `rgba_to_i420()`, в цикле encoder — раздельный замер scale_ns и convert_ns, лог с BOTTLENECK и подсказкой про libyuv при scale > 8 ms |
| `client/src/voice.rs` | У `use_simulcast()` добавлены комментарий и `#[allow(dead_code)]` |

### Ожидаемый результат

- 1080p60 стабильно 55–60 fps на RTX 3080.
- Нет режима «1 кадр в секунду», нет падений на 250 kbps слой, нет MULTIPLE_SPATIAL_LAYERS_PER_STREAM.
- Меньшая загрузка CPU за счёт NVENC H264.

### Как потом безопасно вернуть симулкаст

- Включить только после настройки битрейтов слоёв на клиенте/сервере так, чтобы LOW слой не был 250 kbps (например ≥ 1–2 Мбит/с и адекватное разрешение). Затем снова выставить `opts.simulcast = preset.use_simulcast()` в `StartScreen`.

---

## Правки 2026-03-04 (v2) — libyuv scaling, throttling, стабильный FPS

*(Ниже — дополнение к правкам выше; предыдущий текст не менялся.)*

### Проблема

- При ненативном разрешении (например 1440p на мониторе, пресет 1080p60): **1 кадр раз в 1–2 секунды**.
- P720F60 / P1080F60 иногда 1–2 fps, P1440F60 не выше ~30 fps.
- Масштабирование через `image::resize` (bilinear) — узкое место на 1440p/1080p.
- Нужно: строго заданный FPS энкодера, отбрасывание лишних кадров, выравнивание по сетке FPS.

### Что сделано

1. **Масштабирование через libyuv (I420), без image::resize**
   - Добавлена `scale_rgba_to_target_libyuv()`: RGBA full-res → `rgba_to_i420()` (full-res I420) → `I420Buffer::scale(dst_w, dst_h)` (libyuv SIMD).
   - В цикле энкодера при необходимости масштаба: сначала замеряется convert (RGBA→I420 full), затем scale (I420::scale). Без масштаба — только convert.
   - Убраны `scaled_rgba_buf` и вызовы `image::imageops::resize` из горячего пути. Конвейер: **RGBA → I420 (full) → I420 scale → publish**; NVENC по-прежнему получает I420 целевого разрешения.

2. **Throttling до preset FPS и отбрасывание кадров при опоздании**
   - Энкодер работает по сетке времени: `next_frame_at` — следующий слот (warmup 15 fps, затем preset FPS).
   - Если к моменту прихода кадра уже опоздали (`now > next_frame_at`): кадр **отбрасывается** (не публикуется), `next_frame_at += interval`, ожидание до следующего слота, затем цикл без «догоняющего» рывка.
   - При опоздании после публикации: `next_frame_at += interval` (один интервал), без сброса на `now` — нет пачки кадров подряд.

3. **Таймстамп по preset FPS**
   - `ts_us = frame_count * 1_000_000 / max_fps` — стабильная сетка для OpenH264, меньше предупреждений вида «Actual input framerate X is different from framerate in setting Y».

4. **Логирование**
   - Каждые 120 кадров: `scale=…µs conv=…µs total=…µs / …µs budget (…%)`, при > 60% бюджета — `← BOTTLENECK`, при scale > 8 ms — подсказка «check GPU/capture load» (масштаб уже на libyuv).

### Изменённые файлы (v2)

| Файл | Правки |
|------|--------|
| `client/src/voice_livekit.rs` | `scale_rgba_to_target` заменён на `scale_rgba_to_target_libyuv` (RGBA→I420 full→I420::scale); в цикле энкодера — путь libyuv с раздельным замером scale/convert; удалён `scaled_rgba_buf`; при опоздании — сброс кадра и ожидание следующего слота; при отставании после публикации — `next_frame_at += interval` без сброса на `now`; подсказка при scale>8ms обновлена на «check GPU/capture load». |

### Почему FPS был нестабильным по пресетам

- **P720F60 / P1080F60 (1–2 fps):** при ненативном разрешении (например 1440p) `image::resize` до 720p/1080p занимал сотни миллисекунд за кадр → энкодер не успевал за 16.67 ms → постоянное опоздание и фактически 1–2 кадра в секунду.
- **P1440F60 (~30 fps):** тот же resize на 1440p давал ~30 ms на кадр → укладывались только в 33 ms (30 fps), а не в 16.67 ms (60 fps).
- После перехода на **libyuv I420::scale** (SIMD) время масштабирования снижается в разы, укладываемся в бюджет кадра и получаем стабильные 55–60 fps для 60 fps пресетов и 50+ fps для P1440F60 при достаточной мощности.

### Ожидаемый результат после правок v2

- P720F60, P1080F60 — стабильно 55–60 fps на RTX 3080 при ненативном и нативном разрешении.
- P1440F60 — не менее ~50 fps, при нехватке — переключение на P1440F30 или P1080F60.
- Нет режима «1 кадр в 1–2 секунды», CPU минимизирован, NVENC загружен равномерно.
- Логи каждые 120 кадров подтверждают использование бюджета и наличие узких мест.

### Симулкаст

- По-прежнему `opts.simulcast = false`, один слой. Включать снова после настройки битрейтов слоёв (LOW не 250 kbps), как в блоке выше.

---

## Правки 2026-03-04 (v3) — HW Encoder Abstraction

### Цель

Рефакторинг пайплайна screen share под аппаратное кодирование (NVENC, AMF, QuickSync) с сохранением lock-free захвата, warmup, инструментации и fallback на CPU.

### Что сделано

1. **Модуль `client/src/screen_encoder.rs`**
   - **`RawFrame`** — перенесён в модуль (pixels, width, height). WGC callback и encoder thread используют один тип.
   - **Трейт `VideoEncoder`**: `encode_frame(raw, target_w, target_h, ts_us) -> Result<EncoderOutput, EncoderError>`; `name() -> &'static str`.
   - **`EncoderOutput`**: пока только `RawI420 { frame, timing }` (I420 для `NativeVideoSource::capture_frame`). Вариант `EncodedH264(EncodedFrame)` зарезервирован на случай, когда LiveKit начнёт принимать уже закодированные кадры.
   - **`FrameTiming`**: scale_ns, convert_ns, total_ns — для логов и BOTTLENECK.
   - **`CpuH264Encoder`**: текущий путь — RGBA → abgr_to_i420 (full) → I420::scale (libyuv) → I420; возвращает RawI420. Само H264-кодирование по-прежнему выполняет LiveKit/OpenH264.
   - **Стабы HW**: `NvencH264Encoder`, `AmfH264Encoder`, `QsvH264Encoder` (Windows). `try_new()` пока возвращает `None` (нет детекции GPU); при добавлении детекции будут возвращать `Some` и внутри использовать CPU fallback до появления API для encoded frames.
   - **`select_screen_encoder()`**: порядок NVENC → AMF → QSV → CPU; на не-Windows — всегда CPU.

2. **Интеграция в `StartScreen` (`voice_livekit.rs`)**
   - Слот по-прежнему `Arc<AtomicPtr<RawFrame>>` (тип из `screen_encoder`). WGC callback без изменений.
   - В encoder thread: `let mut encoder = select_screen_encoder();` перед циклом; в цикле — `encoder.encode_frame(&frame, video_width, video_height, ts_us)` → `EncoderOutput::RawI420 { frame, timing }` → `source_enc.capture_frame(&frame)`, накопление timing для инструментации.
   - Warmup (10 кадров @ 15 fps), pacing (next_frame_at, drop при опоздании), лог каждые 120 кадров (scale/conv/total, BOTTLENECK при >60%) сохранены.

3. **LiveKit**
   - `opts.video_codec = VideoCodec::H264`, разрешение/FPS по пресету, один слой, без симулкаста — без изменений.

### Новая схема пайплайна (Windows WGC + encoder abstraction)

```
Monitor @ 60–144Hz
    │ D3D11 GPU→CPU (WGC)
    ▼
AtomicPtr<RawFrame>  ← WGC: swap(new); encoder: swap(null)
    │
    ▼
select_screen_encoder() → CpuH264Encoder | NvencH264Encoder (stub) | ...
    │
    ▼
VideoEncoder::encode_frame(raw, target_w, target_h, ts_us)
    │  CPU: rgba → I420 full → I420::scale → RawI420
    │  HW (future): RGBA/ARGB → NVENC/AMF/QSV → EncodedH264 (when API exists)
    ▼
EncoderOutput::RawI420 { frame, timing }
    │
    ▼
NativeVideoSource::capture_frame(&frame)
    │
    ▼
LiveKit/WebRTC (OpenH264 or future HW in build) → RTP → SFU
```

### Fallback

- При отсутствии поддержки HW или при ошибке `encode_frame` используется CPU: либо через `select_screen_encoder()` (CPU по умолчанию, пока стабы возвращают `None`), либо при добавлении детекции — HW-стаб с внутренним CPU fallback.

### Возможные подводные камни

- **Драйверы**: разные версии драйверов NVIDIA/AMD/Intel по-разному ведут себя с NVENC/AMF/QSV; после внедрения детекции и реального кодирования нужны тесты на нескольких конфигурациях.
- **Память GPU**: при реальном HW encode буферы в VRAM; при нехватке памяти — fallback на CPU.
- **Масштабирование**: CPU путь масштабирует через libyuv (I420::scale); HW-энкодеры могут масштабировать внутри (нужно смотреть документацию по каждому API).
- **Целевой FPS**: pacing и таймстамп по preset FPS не менялись; при переходе на отдачу EncodedH264 нужно сохранить ту же сетку ts_us.
- **Публикация encoded**: пока LiveKit Rust принимает только сырые кадры в `capture_frame`; для настоящего HW encode без двойного кодирования потребуется либо поддержка в SDK (inject encoded H264), либо сборка livekit-webrtc с фабрикой HW-энкодеров.

---

## Варианты оптимизации (влияние на стабильность и качество)

Перед реализацией — оценка пяти направлений.

### 1. Libyuv напрямую вместо image::resize

**Текущее состояние:** В горячем пути screen share **image::resize не используется**. Сейчас: RGBA → `abgr_to_i420` (full-res) → `I420Buffer::scale(dst)` (libyuv). Масштабирование уже идёт через libyuv (SIMD).

**Что можно попробовать:** Вариант «scale в RGBA до целевого размера, потом один convert в I420» через **libyuv ARGBScale** (если доступен в libwebrtc/биндингах): меньше пикселей при конверсии. Альтернатива — оставить порядок I420::scale, но убедиться, что нет лишних копий.

| Аспект | Влияние |
|--------|--------|
| **Стабильность** | Плюс: при успешном ARGBScale путь может снизить пиковое время обработки кадра (меньше работы на кадр), меньше риск дропов при всплесках. |
| **Качество** | Нейтрально: libyuv (ARGBScale / I420 scale) того же класса, что и текущий pipeline; визуально не должно отличаться. |
| **Риски** | Нужно проверить наличие/сигнатуры ARGBScale в крейте; при ошибках в stride/формате — артефакты. |

---

### 2. Переиспользование буферов

**Текущее состояние:** Каждый кадр: `I420Buffer::new(src_w, src_h)` и при scale — ещё одна аллокация (результат `scale()`). `scaled_rgba_buf` в текущем коде не используется (пайплайн переведён на I420 scale).

**Что сделать:** Реиспользовать буферы: один I420 под full-res (или под target-res при scale), один под результат scale; при изменении разрешения — переаллоцировать.

| Аспект | Влияние |
|--------|--------|
| **Стабильность** | Плюс: меньше аллокаций → меньше пиков аллокатора и GC, более предсказуемое время кадра, меньше шанс «просадок» из‑за heap. |
| **Качество** | Нейтрально: содержимое кадра то же, меняется только способ выделения памяти. |
| **Риски** | Минимальные: корректный размер буфера при смене разрешения и при первом кадре. |

---

### 3. Параллелизм: WGC → очередь → несколько worker’ов

**Идея:** WGC пишет в lock-free очередь RawFrame; несколько потоков забирают кадры, делают RGBA→I420+scale; один encoder thread забирает готовый I420 и вызывает `capture_frame()`.

| Аспект | Влияние |
|--------|--------|
| **Стабильность** | Может улучшиться, если узкое место — один поток convert: нагрузка распределяется, реже «нет кадра» к моменту слота. Риск: при малой глубине очереди — те же дропы; при большой — рост задержки. |
| **Качество** | Нейтрально при той же частоте и порядке кадров. При нарушении порядка (если не сортировать по timestamp) — возможны дёргания. |
| **Риски** | Сложность: синхронизация, ограничение глубины очереди, порядок кадров и таймстампы должны оставаться монотонными. Один publish-поток — уже есть; добавляются только worker’ы до него. |

---

### 4. Увеличить буферизацию (1–2 кадра)

**Сейчас:** Один слот `AtomicPtr<RawFrame>`: только последний кадр; encoder раз в интервал забирает кадр или уходит «пусто».

**Вариант:** Очередь на 2–3 кадра (например, lock-free ring buffer): WGC кладёт, encoder забирает самый старый (или последний — зависит от цели). Тогда при кратковременных задержках encoder реже просыпается «в пустоту».

| Аспект | Влияние |
|--------|--------|
| **Стабильность** | Плюс: меньше пропущенных слотов, более ровный FPS при мелких всплесках. |
| **Качество** | Нейтрально или чуть лучше за счёт меньшего числа дропов. |
| **Задержка** | Минус: +1–2 кадра (при 60 fps это ~17–33 ms). Для screen share часто приемлемо. |
| **Риски** | При постоянной перегрузке очередь будет полной → те же дропы, но уже на входе; важно ограничить глубину. |

---

### 5. Таймер Windows (1 ms) + spin; pre-buffer 2 ms

**Сейчас:** `timeBeginPeriod(1)`, sleep до (deadline − 2 ms), затем spin до слота.

**Варианты:** Увеличить «предбуфер» до 3 ms (меньше spin, меньше конкуренция за CPU) или снизить разрешение таймера до 2 ms.

| Аспект | Влияние |
|--------|--------|
| **Стабильность** | Плюс: меньше времени в spin_loop → меньше contention с другими потоками (в т.ч. WGC/другими ядрами), возможна более ровная загрузка. |
| **Качество** | Практически без изменений: сдвиг на 1–2 ms даёт незначительный jitter. |
| **Риски** | При слишком большом «предбуфере» можно чуть чаще опаздывать к точному слоту; имеет смысл замерять jitter до/после. |

---

### Сводка

| Вариант | Стабильность | Качество | Сложность | Рекомендация |
|---------|--------------|----------|-----------|--------------|
| 1. Libyuv ARGBScale | ↑ возможен выигрыш | ≈ | средняя (поиск API) | Пробовать, если есть биндинги |
| 2. Переиспользование буферов | ↑ | ≈ | низкая | Делать в первую очередь |
| 3. Несколько worker’ов | ↑ при bottleneck на convert | ≈ | высокая | После 1–2, при необходимости |
| 4. Буфер 1–2 кадра | ↑ | ≈ или чуть лучше | средняя | Имеет смысл при частых «пустых» тиках |
| 5. Pre-buffer 2–3 ms | ↑ (меньше contention) | ≈ | низкая | Простой эксперимент |

---

## Архитектура: клиентский HW-энкодер + LiveKit Ingress (WHIP)

Дата: 2026-03-04.  
Цель: 1440p60, динамическое разрешение на лету, кодирование только на клиенте, сервер только ретрансляция.

### Требования (напоминание)

- Стабильные 60 fps при 2560×1440.
- Динамическая смена разрешения (1440 → 1080 → 720) на клиенте, сервер ретранслирует без пересоздания комнаты.
- Качество и стабильность FPS в приоритете; сервер без GPU, кодирование только на клиенте (NVENC/AMF/QSV).
- Текущий LiveKit Rust SDK: только `capture_frame(I420)`, pre-encoded H.264 не поддерживается.

### Поддержка HW-энкодеров в Rust (NVENC, AMF, QSV, VAAPI)

| Энкодер | Платформа | Нативные крейты / биндинги | Через GStreamer (gstreamer-rs) |
|--------|-----------|----------------------------|-------------------------------|
| **NVENC** | Windows, Linux | ✅ **nvenc** (docs.rs/nvenc), **nvenc-sys** (legion-labs), **nvidia-video-codec-sdk** — H.264/HEVC/AV1, low-latency | ✅ **nvh264enc** (плагин nvcodec), из Rust через `gst-plugins-bad` / системный GStreamer |
| **AMF** (AMD) | Windows | ❌ Нет готового крейта; только C++ SDK от AMD | ✅ **amfh264enc**, **amfh265enc**, **amfav1enc** (плагин amfcodec) — из Rust через gstreamer-rs |
| **QSV** (Intel) | Windows, Linux | ✅ **onevpl-rs** (Intel oneVPL), **intel-mediasdk-rs**, **qsv-rust** — Media SDK / oneVPL | ✅ Элементы msdk (msdkh264enc и др.) или vaapi на Linux |
| **VAAPI** | Linux (Intel/AMD) | ✅ **cros-libva** — биндинги libva; энкодер поверх них нужно собирать самому или через FFmpeg | ✅ **vaapih264enc** (плагин vaapi) — из Rust через gstreamer-rs |

**Итог:** Для единого кода на Rust разумно либо использовать **gstreamer-rs** и собирать пайплайн с nvh264enc/amfh264enc/vaapih264enc (один интерфейс, разные элементы под ОС/GPU), либо брать нативные крейты под каждую платформу (nvenc, onevpl-rs и т.д.). AMF в «чистом» Rust без GStreamer — только свои FFI-биндинги к C++ AMF SDK.

---

### 1. Насколько реально: Rust-клиент + GStreamer/NVENC + LiveKit Ingress (self-hosted)?

**Реализуемо**, но с выбором пути.

| Компонент | Реалистичность | Комментарий |
|-----------|----------------|-------------|
| **Кодирование на клиенте (NVENC/AMF/QSV)** | ✅ Реально | GStreamer (gstreamer-rs) с nvh264enc/amfh264enc/vaapih264enc, или FFmpeg libav (rust bindings), или нативные SDK (NVENC API, AMF, MFX). В проекте уже есть абстракция `VideoEncoder` и стабы HW в `screen_encoder.rs`. |
| **LiveKit Ingress WHIP** | ✅ Реально | Ingress по умолчанию для WHIP **не транскодирует** (`enable_transcoding: false`) — поток передаётся в комнату как есть. Идеально для pre-encoded H.264. |
| **Rust → WHIP** | ⚠️ Зависит от пути | **Вариант A:** Текущий LiveKit Rust SDK — только `capture_frame(I420)`; внутри WebRTC кодирует (OpenH264). Для настоящего HW без двойного кодирования нужен обход. **Вариант B:** Отдельный WHIP-клиент (например webrtc-rs/whip или аналог): клиент поднимает WebRTC-соединение к Ingress и должен отдавать уже RTP с H.264. Это значит: NVENC → RTP packetizer (H.264 NAL → RTP) → SRTP → WHIP. LibWebRTC умеет packetize H.264 (`RtpPacketizerH264`), но в Rust SDK этот путь не выставлен. **Вариант C:** Гибрид — использовать один общий WebRTC-коннект к комнате (как сейчас), но заменить внутренний энкодер в libwebrtc на фабрику с NVENC (сборка livekit-webrtc с кастомным encoder factory), если такая опция появится/доступна. |

**Итог по вопросу 1:** Да, архитектура реализуема. Самый предсказуемый путь для «только клиент кодирует, сервер только ретранслирует» — **WHIP + Ingress** с отдельным WHIP-клиентом в Rust, который шлёт RTP с уже закодированным H.264 (GStreamer/FFmpeg/NVENC → RTP packetizer → WHIP). Альтернатива — дождаться/добавить в LiveKit Rust SDK поддержку инжекта encoded frames или HW encoder factory в libwebrtc.

---

### 2. Узкие места и потенциальные проблемы

| Область | Риски | Рекомендации |
|--------|-------|--------------|
| **CPU (клиент)** | Конвертация RGBA→YUV и масштабирование до энкодера; при 1440p60 ~800 MB/s. | Оставить libyuv (I420 scale), переиспользование буферов; HW-энкодер принимает по возможности текстуру/буфер в VRAM (DXGI/NV12), чтобы минимизировать копии через CPU. |
| **Шина (GPU↔CPU)** | Копирование 1440p кадров в GPU для NVENC и обратно — задержка и нагрузка. | WGC уже даёт кадр в CPU (RGBA). Для NVENC выгодно: либо захват в GPU (e.g. WGC → D3D texture → NVENC input), либо один copy в NV12 на GPU и подача в NVENC без возврата в CPU. |
| **Сеть** | 1440p60 при 15–20 Мбит/с; джиттер, потери пакетов. | BWE и FEC уже в WebRTC; при WHIP — те же механизмы. Рекомендуется достаточный буфер и приоритет видео-пакетов (DSCP), особенно на облачном сервере. |
| **Синхронизация аудио/видео** | Разные пути (микрофон через Room, экран через WHIP) — риск рассинхрона. | Либо вести оба трека через один WebRTC-коннект (тогда без WHIP, но с ограничением SDK по encoded frames), либо при WHIP — один WHIP-поток с аудио+видео и едиными таймстампами (RTP clock). |
| **Потеря кадров** | Дропы при перегрузке, буфер энкодера переполнен. | Pacing по FPS (как сейчас), при опоздании — drop и следующая сетка. При HW — не ставить в очередь больше 1–2 кадров; ключевые кадры по расписанию или по запросу PLI. |
| **Сервер (облако, без GPU)** | Ingress только ретранслирует — CPU почти не нагружается. | Для WHIP без транскодинга Ingress лишь проксирует RTP; масштабирование и кодирование на сервере не нужны. |

---

### 3. Критические ограничения LiveKit Rust SDK

| Ограничение | Влияние |
|-------------|---------|
| **Только `NativeVideoSource::capture_frame(I420)`** | Нет API для передачи уже закодированных H.264 кадров. Текущий путь: I420 → внутренний энкодер (OpenH264) → RTP. Использование NVENC «в обход» без двойного кодирования в этом SDK невозможно без форка/патча. |
| **Нет публичного API для RTP packetization / encoded frame inject** | Нельзя подменить источник на «готовый RTP с H.264». Нужен либо другой транспорт (WHIP-клиент с своим RTP), либо изменения в livekit-webrtc (encoder factory, или приём encoded frames). |
| **Issue #503 (hardware encoding)** | Запрос на HW-энкодер в SDK; ясной поддержки инжекта pre-encoded или выбора внешнего энкодера пока нет. |
| **Блокировка `capture_frame`** | Известны кейсы, когда `capture_frame` может надолго блокироваться — уже учтено в проекте (отдельный encoder thread, не event-driven). |

**Вывод:** Текущий SDK **блокирует** схему «клиент кодирует H.264 один раз, сервер только ретранслирует» в рамках одной Room-сессии с публикацией через `publish_track` + `NativeVideoSource`. Обход — отдельный WHIP-клиент с собственным RTP (H.264) или доработки/форк SDK/webrtc.

---

### 4. Динамическое изменение разрешения без пересоздания комнаты

- **Комната и Ingress не пересоздаются.** Комната уже есть; WHIP Ingress создаётся один раз на сессию экрана (или переиспользуется по stream key). Смена разрешения — только на стороне клиента.

- **Механизм смены разрешения в H.264:** В середине потока энкодер (NVENC и др.) выдаёт новый **SPS/PPS** (новые width/height) и следующий кадр — **IDR (keyframe)**. Декодеры и WebRTC принимают смену разрешения in-band; **renegotiation (SDP offer/answer) не обязателен**, если кодек и профиль H.264 не меняются.

- **Что делать на клиенте:** По команде (1440→1080→720) переконфигурировать энкодер (width, height, bitrate), сбросить внутренний state, выдать ключевой кадр с новым SPS/PPS. Дальше продолжать поток с новым разрешением. Ingress и сервер просто пересылают RTP дальше.

- **Рекомендация:** При смене разрешения принудительно запрашивать keyframe (или генерировать по расписанию раз в N секунд), чтобы подписанты быстро получили новый SPS/PPS и переключились без артефактов.

---

### 5. Альтернативы, если LiveKit Ingress/WHIP недостаточны

| Подход | Плюсы | Минусы |
|--------|-------|--------|
| **Текущая схема (Room + capture_frame(I420))** | Уже работает, один коннект, простая интеграция. | Двойное кодирование при желании использовать NVENC (I420→OpenH264), ограничение по стабильности 1440p60 на CPU. |
| **WHIP + отдельный Rust WHIP-клиент** | Один раз H.264 на клиенте (NVENC), Ingress без транскодинга, смена разрешения in-band. | Нужна реализация/интеграция WHIP-клиента и RTP H.264 packetization; аудио — либо в том же WHIP-потоке, либо отдельно через Room (риск рассинхрона). |
| **RTMP Ingress** | Много инструментов (OBS, FFmpeg, GStreamer). | Ingress **всегда транскодирует** RTMP → не подходит под «сервер без кодирования». |
| **Свой SFU/реле на Go** | Полный контроль. | Высокая сложность (RTP, SRTP, DTLS, BWE, NACK, simulcast). |
| **GStreamer webrtcbin → свой signaling** | GStreamer может кодировать NVENC и отдавать в WebRTC. | Нужен отдельный signaling и стыковка с LiveKit-комнатой или замена LiveKit на свой сервер. |
| **Дождаться/контрибьютить в LiveKit** | Encoded frame API или HW encoder factory в Rust SDK — тогда одна сессия Room, один коннект, без отдельного WHIP. | Зависимость от дорожной карты и ревью. |

**Практический вывод:** Для стабильных 1440p60 и динамического разрешения при «лёгком» сервере наиболее прямой путь — **WHIP + LiveKit Ingress (transcoding off)** и клиентский пайплайн **WGC → (scale при необходимости) → NVENC → RTP H.264 → WHIP**. Текущий LiveKit Rust SDK для Room при этом можно оставить для голоса/микрофона и управления присутствием; экран — вести отдельным WHIP-потоком с единым аудио+видео в одном RTP-потоке для синхронизации, либо документировать ограничения при раздельных путях аудио/видео.
