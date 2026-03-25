# Anti-lag: RTP, capture_time и низкая задержка screen streaming (WebRTC / LiveKit)

Документ — сжатое резюме итераций по снижению задержки и рывков. Исходная проблема: latency росла со временем, падала на keyframe, снова росла; далее — STALL на viewer, burst-доставка, UI/рендер, backpressure на приёме.

---

## Pipeline

```
capture (WGC) → convert (BGRA→NV12) → encode (MFT H.264) → push_frame → RTP → SFU → RTP → decode → NV12→RGBA → UI
```

---

## Текущие инварианты (что считать «истиной» в коде)

| Область | Правило |
|--------|---------|
| **RTP timestamp** | Накопительный `+= 90000/fps` после каждого отправленного кадра. Не `index * step` (иначе drift при дропах). При смене пресета/FPS — новый random старт + пересчёт step. |
| **capture_time в `push_frame`** | **`ts_us` / `running_ts_us`** — шаг совпадает с RTP clock (мкс). Так устранён jitter в delta относительно RTP после экспериментов с wall-clock и «синтетиком». |
| **MFT `SetSampleTime` (`ts_us`)** | Монотонный аккумулятор `running_ts_us += interval * frames_per_tick` — не `frame_count * interval` (регресс при adaptive FPS). |
| **Playout / jitter buffer** | `EncodedImage::SetPlayoutDelay(Minimal())` в `video_track.cpp` + `livekit.yaml`: `room.playout_delay` min/max **0**. |
| **Каденция sender** | `next_frame_at` только инкрементально (`while now >= slot { slot += interval }`), не `now + interval` после overshoot. Сон: `(remaining - PRE_BUFFER_MS)` + spin до слота. Windows: `timeBeginPeriod(1)`, `PRE_BUFFER_MS` снижен (см. код). |
| **Async MFT** | Очередь метаданных `VecDeque<FrameMeta>` (rtp/capture/submit_time на кадр). Паттерн **submit → collect_blocking → push** без предварительного drain `collect()` (иначе burst в один тик). `ASTRIX_MFT_PIPELINED=0` отключает pipelined путь. NeedInput: буферизация в `need_input_buffered` (баг «съели NeedInput» исправлен). |
| **Приём** | Отдельный **OS thread converter**: `stream.next()` в tokio task (lean) → канал → drain to latest → `video_frame_to_rgba` + pacing + `vf.insert`. Иначе backpressure на WebRTC и ложные «сетевые» 200ms+ в метриках. |
| **Перезапуск трека** | `video_stream_tasks` + `abort` старого таска — иначе `recv_fps` раздувается (N× реальный FPS). |

**API:**

```rust
enc_src.push_frame(&data, rtp_timestamp, capture_time_us, key_frame);
// capture_time_us = ts_us (RTP-aligned), не wall после encode
```

**Таблица RTP step (частые FPS):** 30→3000, 60→1500, 90→1000, 120→750 тиков при clock 90000 Hz.

---

## Файлы (одна сводка)

| Файл | Роль |
|------|------|
| `client/src/voice_livekit.rs` | WGC, MFT/I420 пути, adaptive FPS, pipelined/sync MFT, pacing, recv/converter split, `cap_now`/rtp/meta, encoder thread priority, SEND-лог |
| `client/src/mft_encoder.rs` | MFT: low-latency, GOP, CBR vs VBR по fps, codec API (QualityVsSpeed, CABAC off, workers), GIR + периодический IDR, submit/collect, `FrameMeta` queue |
| `client/vendor/rust-sdks/webrtc-sys/src/video_track.cpp` | `SetPlayoutDelay(Minimal())` в `push_encoded_frame` |
| `client/src/telemetry.rs` | Sender/receiver метрики, batched print, `OnceLock` для флагов, recv_wait, encode_peak/avg, coalesce/conv_iter |
| `client/src/main.rs` | `vsync` по умолчанию off, `ASTRIX_VSYNC=1` |
| `client/src/ui.rs` | Render FPS overlay, rate-limited `request_repaint` |
| `livekit.yaml` | `room.playout_delay`, `rtc.packet_buffer_size_video`, при необходимости `congestion_control`, `logging`, `prometheus_port` |
| `docker-compose.yml` | Напр. `GOGC=200`, `GODEBUG=gctrace=1` для LiveKit |

---

## MFT / encoder (кратко)

- Sync/async: по возможности **pipelined** для HW (меньше блокировок на GPU); sync path для SW MFT.
- Тюнинг: QualityVsSpeed=100, CABAC off, worker threads (если поддерживается); **CBR при fps ≥ 60** для сглаживания пиков; **GIR** + редкий forced IDR (~12s).
- **Adaptive FPS** (лестница, гистерезис): при стабильном encode > budget — снижение fps; восстановление медленнее.
- Пул WGC: по возможности избегать лишнего CopyResource (см. `d3d11_nv12.rs` / pool flags — пункт 4.8 в старой версии).
- Опционально: `mem::take` для буфера вывода вместо лишних `clone` на keyframe.

---

## Receiver / UI

- **Метрики:** `recv_wait` / `recv_wait_pk` — чистое ожидание `stream.next()`; `network_*` / `expected` / `stall_count` — см. телеметрию. Поля coalesce / `conv_iter_pk` / `pacing_sleep_sum` отделяют пачки на клиенте и долгий конвертер от дыр в WebRTC.
- **Pacing:** `ASTRIX_RECV_PACING=1` — слот по EMA `timestamp_us`; при drain >1 кадра sleep пропускается (не копить лаг). По умолчанию обычно выкл.; тестировать vs burst.
- **UI:** vsync off по умолчанию; телеметрия одним `eprint!`; env через `OnceLock`; conditional repaint (избежать 100% CPU без vsync).
- **Render FPS** в оверлее ≠ `recv_fps` — показывает реально отрисованное.

---

## Переменные окружения (сводка)

| Переменная | Назначение |
|------------|------------|
| `ASTRIX_TELEMETRY=1` | Телеметрия sender/receiver + SEND-лог (заменяет старый `ASTRIX_DEBUG_RTP_TS`) |
| `ASTRIX_VSYNC=1` | Включить vsync (по умолчанию off) |
| `ASTRIX_RECV_PACING=1` | Пейсинг на converter |
| `ASTRIX_RECV_EXPECTED_US` | Fallback ожидаемого шага (мкс), кэшируется в `OnceLock` |
| `ASTRIX_DECODE_RGBA_CPU` | Форс CPU RGBA (кэш OnceLock) |
| `ASTRIX_MFT_PIPELINED=0` | Отключить pipelined MFT |

---

## livekit.yaml — типовой блок

```yaml
room:
  playout_delay:
    enabled: true
    min: 0
    max: 0

rtc:
  packet_buffer_size_video: 100   # меньше default — меньше батчинг; риск при congestion
  # congestion_control.enabled: false  # только для диагностики localhost/LAN
```

---

## Диагностика и интерпретация

- **SEND:** `rtp`, `capture` (= ts на кадр), `delta` между кадрами, `now`. При RTP-aligned capture ожидается стабильный `delta` ≈ `1_000_000/fps` мкс.
- **STALL / LONG_STALL:** смотреть `recv_wait` vs `conv_iter_pk` / `coalesce_pk` / `pacing_sleep_sum` — дыра **до** конвертера или **в** GPU/UI/pacing.
- **Burst на localhost:** часто норма стека (UDP, SFU, батчи); `stall_count=0` и приемлемый `recv_fps` — уже хорошо. Отличать от багов sender (например, несколько push за один такт — см. историю pipelined/collect).
- **Логи LiveKit:** осторожно с `findstr` — SDP содержит подстроки `nack`, `pli`. Коррелировать STALL с GC (`gctrace`), DTLS/data channel отдельно от видео.
- **Docker vs нативный LiveKit:** полезный A/B; на части железа улучшения не было — тогда узкое место не обязательно сеть контейнера.

---

## Decoder vs encoder

Наши тюнинги MFT encoder / приоритет потока / pipelined submit **не дублируются** на decoder: декод внутри libwebrtc, приёмный цикл — async tokio + отдельный converter. Узкие места чаще оказывались **UI и блокировкой приёма**, а не «вторым набором» codec API на декодере.

---

## Эволюция capture_time (одна строка на эпоху)

Чтобы не путать при чтении старых веток: **encode-based счётчик** → **wall-clock** (после playout_delay=0) → **синтетический шаг = RTP** (устранение STALL из-за рассинхрона с RTP) → **wall перед push** (jitter в delta от encode) → **финал: `ts_us` / RTP-aligned** + аккуратный MFT monotonic `running_ts_us` и фиксы async meta/cadence.

---

## Открытые темы и следующие шаги

1. **Прогрев ~30 с** после старта (WGC rate, два клиента на одном GPU, pacing) — отдельный таймлайн-замер.
2. **SFU / Pion:** `logging.level`, точечный `PION_LOG_*`, Prometheus, A/B viewer на втором ПК; при доказанном узле — узкий патч или issue upstream вместо полного форка.
3. **Идеи:** ring buffer вместо только latest-wins; keyframe stutter — GOP / intra-refresh; при лаге ренера — vsync, время NV12→RGBA, полный cost egui frame.

---

## Новый чат в Cursor

В первом сообщении приложить `@antilag.md`, кратко описать симптом и пресет; при узкой задаче — `@client/src/voice_livekit.rs`, `@client/src/telemetry.rs`, `@livekit.yaml`. Старый чат в контекст не переносится — этот файл заменяет резюме.
