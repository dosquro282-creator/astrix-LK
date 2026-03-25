# Roadmap: захват экрана на GPU → I420 (D3D11)

Документ описывает архитектуру экранного захвата Astrix, план перехода на GPU-путь (D3D11) и все внесённые доработки.

**Как продолжить в новом диалоге:** «Продолжаем по @d3dcap.md — [фаза или задача]».

---

# Часть I. Обзор и архитектура

## 1. Описание проекта

**Astrix** — десктоп-клиент (Rust) для голосовой/видеосвязи с демонстрацией экрана.

| Компонент | Технология |
|-----------|------------|
| UI | eframe/egui |
| Сеть, async | tokio |
| Комната, треки | LiveKit Rust SDK |
| Микрофон | cpal |
| Захват экрана (Windows) | `windows-capture` 1.5 (WGC), feature `wgc-capture` (default) |
| Захват (fallback) | xcap (поллинг) |
| Кодирование видео | WebRTC (OpenH264) → I420; абстракция в `screen_encoder` |

**Пресеты экрана** (`voice::ScreenPreset`, `voice.rs`):

| Пресет   | width | height | max_fps | max_bitrate_bps |
|----------|-------|--------|---------|-----------------|
| P720F30  | 1280  | 720    | 30      | 6_000_000       |
| P720F60  | 1280  | 720    | 60      | 12_000_000      |
| P1080F30 | 1920  | 1080   | 30      | 16_000_000      |
| P1080F60 | 1920  | 1080   | 60      | 30_000_000      |
| P1440F30 | 2560  | 1440   | 30      | 24_000_000      |
| P1440F60 | 2560  | 1440   | 60      | 40_000_000      |

---

## 2. Текущая архитектура захвата

### 2.1 Потоки

1. **Поток WGC** (`livekit-screen-wgc`)
   - Запуск из `voice_livekit::start_screen_capture()` при `VoiceCmd::StartScreen`.
   - Коллбек `on_frame_arrived`: получает `Frame`, копирует кадр в GPU-пул (CopyResource) или в CPU ring, пушит в lock-free кольцо.
   - Остановка по `stop_flag: Arc<AtomicBool>`.

2. **Поток энкодера** (`livekit-screen-enc`)
   - Читает кадры из ring (GPU-слот или CPU RawFrame), дроссель по FPS пресета, warmup, затем полный FPS.
   - Конвертация (GPU или CPU) → I420 → `capture_frame` в LiveKit.

### 2.2 Модули

| Файл | Назначение |
|------|------------|
| `client/src/voice_livekit.rs` | LiveKit-сессия, `start_screen_capture()`, WGC handler, ring, потоки WGC и энкодера |
| `client/src/voice.rs` | `VoiceCmd`, `ScreenPreset`, `VideoFrames` |
| `client/src/screen_encoder.rs` | `RawFrame`, `VideoEncoder`, CPU/GPU пути |
| `client/vendor/windows-capture` | WGC API, коллбек FrameArrived |

### 2.3 Обмен данными

- **GPU-путь:** WGC → CopyResource в текстуру пула → `gpu_ring.push(slot)` → энкодер: convert (D3D11) → I420 → LiveKit.
- **CPU-путь:** WGC → `frame.buffer()` → RawFrame → ring → энкодер: scale + I420 → LiveKit.
- Ring: lock-free, несколько слотов; при переполнении старый кадр дропается.

### 2.4 Цель GPU-пути

Убрать полное копирование RGBA с GPU в CPU: кадр остаётся на GPU, конвертация RGBA→I420 на GPU, readback только I420. Ключевое: у `Frame` есть `as_raw_texture()` → `ID3D11Texture2D`; через него получаем device/context без изменений в крейте.

---

# Часть II. План фаз и состояние

## 3. Текущее состояние реализации

- [x] **Фаза 0** — зависимости D3D11, план обмена GPU-слотами.
- [x] **Фаза 1** — доступ к текстуре и device/context в коллбеке.
- [x] **Фаза 2** — пул текстур, CopyResource в коллбеке, GpuSlotRing.
- [x] **Фаза 3** — D3D11 compute RGBA→I420 (HLSL, BT.601).
- [x] **Фаза 4** — readback I420, подача в LiveKit.
- [x] **Фаза 5** — интеграция, fallback CPU при ошибке GPU, `ASTRIX_SCREEN_CAPTURE_PATH=cpu|gpu|auto`.
- [x] **Фаза 6** — GPU downscale в шейдере, тайминги, смена разрешения.

**Проверка путей:** `ASTRIX_SCREEN_CAPTURE_PATH=cpu|gpu|auto` (по умолчанию `auto`).

### 3.1 Известная проблема: Map E_INVALIDARG (решена)

На части конфигураций `Map` для staging возвращал E_INVALIDARG. **Причина:** константный буфер `cb_params` создавался с `D3D11_USAGE_DEFAULT`, а в `convert()` вызывался `Map(WRITE_DISCARD)`, что требует `D3D11_USAGE_DYNAMIC` + `D3D11_CPU_ACCESS_WRITE`. **Исправление:** в `d3d11_i420.rs` для `cb_params` заданы `D3D11_USAGE_DYNAMIC` и `D3D11_CPU_ACCESS_WRITE`.

---

## 4. Фазы (кратко)

- **Фаза 0:** Зависимости D3D11, план обмена GPU-слотами, пресеты и формат WGC (RGBA8).
- **Фаза 1:** В коллбеке — `frame.as_raw_texture()`, GetDevice/GetImmediateContext, GetDesc.
- **Фаза 2:** Пул из 3 `ID3D11Texture2D`, CopyResource в коллбеке, push индекса слота в ring.
- **Фаза 3:** Compute shader RGBA→I420, SRV/UAV, буферы Y/U/V, модуль `d3d11_i420.rs`.
- **Фаза 4:** Readback в staging, Map, заполнение I420Buffer, capture_frame.
- **Фаза 5:** Auto/CPU/GPU, fallback при ошибке, drain при останове.
- **Фаза 6:** Шейдер с downscale (`D3d11RgbaToI420Scaled`), тайминги, пересоздание конвертера при смене размера.

---

# Часть III. Доработки (хронология и темы)

## 5. Доработки по темам

### Цвета, FPS, мерцание (Доработки 1, 5, 6)

- **1:** Убран `swap(0,2)` в шейдере I420→RGBA. Добавлен `gpu_encode_active`: при активном GPU-пути WGC не заполняет CPU ring — нет мерцания.
- **5:** Двойное `+= interval` при опоздании исправлено на `next_frame_at = Instant::now() + interval`. Порог дропа: «> 2 интервала» (GPU convert ~16 ms).
- **6:** В ring `assert!(old.is_null())` заменён на тихий drop. WGC не держит `pool_ref` при ожидании `context_mutex`: клонируем данные из pool, освобождаем `pool_ref`, затем берём `context_mutex`.

### Map и D3D11 (Доработки 2, 3, 4, 8, 9)

- **2:** `cb_params` — `D3D11_USAGE_DYNAMIC` + `D3D11_CPU_ACCESS_WRITE` (исправление E_INVALIDARG).
- **3:** Энкодер не держит `pool_ref` на время `convert()`; клонируем device/context/texture, затем convert. Добавлен `Flush()` между проходами в scaled shader.
- **4:** `context_mutex` защищает Immediate Context; таймаут GetData 200 мс, при зависании — переход на CPU.
- **8:** `yield_now()` заменён на `sleep(1ms)` в GetData; убран лишний `Flush()`; `I420Planes` с реиспользуемыми буферами.
- **9:** Readback через `RWByteAddressBuffer`, прямой `memcpy` при Map (экономия CPU bandwidth).

### Стабильность и зрители (Доработки 7, 11, 12)

- **7:** Ring 6 слотов, warmup 30 кадров, первый I420 отправляется 3× (force keyframe).
- **11:** Исправлено дублирование timestamp после блока 3× IDR; исправлен обмен буферов в CPU-пути (`cpu_returned_buffer`/`cpu_other_buffer`).
- **12:** Рекомендации по плавности при 54–56 fps (sleep до `next_frame_at`, порог дропа, jitter buffer у зрителя).

### 30 FPS и WGC (Доработки 14, 15, 16)

- **14:** В CPU-пути порог дропа выровнен с GPU («> 2 интервала»). MinUpdateInterval: `Duration::from_millis(1)` для 60 fps. Диагностика `capture_frame rate`.
- **15:** В коллбеке WGC добавлен подсчёт и лог `WGC on_frame_arrived rate: X fps (source)`. MinUpdateInterval в логе (µs). FramePool в патче: 3 буфера (1→3 в vendor).
- **16 (исправление 30 FPS WGC):** См. раздел 6 ниже — try_lock в коллбеке, Recreate после StartCapture, минимальный коллбек.
- **17 (регрессия 30 FPS после newLK):** См. раздел 6.1 ниже — при фоновом создании пула текстур CPU fallback блокировал WGC.
- **18 (постоянный FPS при статичной картинке):** См. раздел 6.2 ниже — повторная отправка последнего кадра при отсутствии новых от WGC.

### Прочее (Доработки 10, 13)

- **10:** CPU scale через libyuv (`I420Buffer::scale`), персистентные I420-буферы (два слота, без аллокации на кадр после первого).
- **13:** Режимы 720p/1080p 120 fps — возможность и ограничения (трафик, энкодер, WGC ~8.33 ms/кадр).

---

## 6. Доработка 16 — устранение 30 FPS (WGC throttling)

**Проблема:** При блокировке коллбека FrameArrived дольше ~16 ms WGC переводит доставку в half-rate (30 FPS) и не возвращается к 60 FPS. Дополнительно: порядок CreateFramePool → StartCapture на части систем даёт фиксированные 30 FPS.

**Внесённые изменения:**

1. **`voice_livekit.rs` — коллбек `on_frame_arrived`:**
   - **`context_mutex.try_lock()`** вместо `lock()`: при занятом контексте (энкодер ещё в convert) кадр пропускается, коллбек не блокируется.
   - **`wgc_log_state.try_lock()`** для диагностики FPS: при занятости лог пропускается, блокировок нет.
   - Итог: поток WGC никогда не ждёт энкодер; при перегрузке дропаются отдельные кадры, WGC не переходит в half-rate.

2. **`client/vendor/windows-capture/src/graphics_capture_api.rs`:**
   - В структуру `GraphicsCaptureApi` добавлено поле `_pool_pixel_format`.
   - В `start_capture()` сразу после `StartCapture()` вызывается **`frame_pool.Recreate(&device, pixel_format, 3, item.Size()?)`** — обход бага «FramePool создан до StartCapture».

**Задокументировано:** правильный pipeline (минимальный коллбек), тест с пустым коллбеком, использование lock-free очередей (раздел 7).

---

## 6.1 Доработка 17 — регрессия 30 FPS после newLK (фоновое создание пула)

**Проблема:** После переноса создания GPU-пула текстур (6× `CreateTexture2D`) в фоновый поток (newLK.md: избежание 50–100 ms блокировки WGC), возникло окно в ~50–100 ms (3–6 кадров при 60 FPS), пока пул ещё не создан. В это время коллбек `on_frame_arrived` проваливался в CPU fallback — `frame.buffer()` выполняет GPU→CPU readback всего RGBA-кадра (14.7 МБ при 1440p), блокируя WGC-поток на 10–20+ ms. Этого достаточно, чтобы WGC перешёл в half-rate (30 FPS) без возврата.

**Причина:** Логика коллбека:
1. Создание пула запущено в фоновом потоке → `return Ok(())` (быстро).
2. Следующие кадры: пул = None, GPU-путь пропущен, `gpu_encode_active` = false → проваливается в CPU fallback.
3. CPU fallback: `frame.buffer()` блокирует >16 ms → WGC half-rate.

**Исправление:**
1. Добавлен флаг `pool_creation_failed: Arc<AtomicBool>` — фоновый поток ставит `true` при ошибке `CreateTexture2D`.
2. Перед CPU fallback: если `pool_creation_started && !pool_creation_failed` → `return Ok(())` (пропускаем кадр, не блокируем WGC).
3. При ошибке создания пула: `pool_creation_failed = true` → CPU fallback разблокирован, работает как раньше.

**Итог:** Во время создания пула коллбек возвращается мгновенно; WGC сохраняет 60 FPS. Как только пул готов — GPU-путь работает в штатном режиме.

---

## 6.2 Доработка 18 — постоянный FPS при статичной картинке

**Проблема:** WGC доставляет кадры только при изменении содержимого экрана. Когда экран статичен (например, открыт документ без анимации), WGC перестаёт вызывать `FrameArrived` → `gpu_ring` и `cpu_ring` пусты → энкодер не отправляет кадры → FPS падает до 0–5 вместо 30/60 по пресету. Зритель видит резкое занижение FPS.

**Исправление:**

1. Добавлен флаг `have_last_frame: bool` — ставится `true` после первого успешно сконвертированного I420-кадра (и GPU, и CPU пути). `planes_buf` хранит последние Y/U/V данные.

2. **GPU-only пустой ring** (когда `gpu_ring.pop() = None`): если `have_last_frame && frame_count >= WARMUP_FRAMES` — создаём `I420Buffer` из `planes_buf` и отправляем через `capture_frame` с обновлённым `timestamp_us`.

3. **CPU/Auto пустой ring** (когда `ring_enc.pop() = None`): аналогично — повторная отправка последнего кадра.

4. **CPU-путь сохранение**: после успешного `EncoderOutput::RawI420` данные I420 копируются в `planes_buf` (`ensure_size` + `copy_from_slice`), чтобы повторная отправка работала и при CPU fallback.

5. FPS-статистика (`fps_frame_count`, `stats_enc.stream_fps`) обновляется и при повторных отправках.

**Итог:** FPS стабильно 30 или 60 в зависимости от пресета — независимо от того, меняется экран или нет.

---

# Часть IV. WGC — известные баги и правильный pipeline

## 7. Известные баги WGC

### 7.1 Half-rate (30 FPS) при блокировке обработчика

- **Суть:** Если обработчик FrameArrived блокируется > ~16 ms (достаточно одного раза), Windows переводит захват в half-rate и может не вернуться к 60 FPS.
- **Причина:** При 60 Hz frame_time ≈ 16.6 ms; при обработке > 16 ms WGC считает consumer медленным и переключается на доставку каждого второго кадра (30 FPS). Поведение задокументировано.
- **Исправление (реализовано):** В коллбеке только `try_lock` по `context_mutex` и по диагностике; CopyResource + Flush + push в `gpu_ring`; при занятом контексте кадр дропается.

### 7.2 Порядок CreateFramePool и StartCapture

- **Суть:** При порядке CreateFramePool → StartCapture захват иногда фиксируется на 30 FPS.
- **Рабочий вариант:** CreateCaptureSession → StartCapture → CreateFramePool, либо **пересоздание FramePool после StartCapture** (Recreate с теми же параметрами).
- **Исправление (реализовано):** В `start_capture()` после `StartCapture()` вызывается `frame_pool.Recreate(...)` (в структуре сохранены `_pool_pixel_format` и доступ к `item.Size()`).

### 7.3 Правильный pipeline (минимальный коллбек)

**Целевая схема:**

```
FrameArrived → TryGetNextFrame → CopyTexture → push в lock-free очередь → return.
Отдельный worker: convert → encode → send.
```

В коллбеке не должно быть: блокирующего lock, encode, network.

**У нас:** Крейт держит `callback.lock()` на время `on_frame_arrived`, поэтому коллбек должен возвращаться быстро. Внутри: только `try_lock`, CopyResource, Flush, push в `gpu_ring`, диагностика через `try_lock`. Тяжёлая работа — в потоке энкодера.

### 7.4 Фрагмент обработчика FrameArrived (windows-capture)

Файл: `client/vendor/windows-capture/src/graphics_capture_api.rs`.

```rust
move |frame, _| {
    if halt_frame_pool.load(atomic::Ordering::Relaxed) { return Ok(()); }
    let frame = frame.as_ref().expect("...").TryGetNextFrame()?;
    // ... timestamp, content size, surface, texture, GetDesc ...
    // при смене размера: frame_pool_recreate.Recreate(...);
    let mut frame = Frame::new(..., frame_texture, ...);
    let result = callback_frame_pool.lock().on_frame_arrived(&mut frame, ...);
    // обработка stop/result
    Result::Ok(())
}
```

Вся тяжёлая работа — внутри `on_frame_arrived`; там должны быть только быстрые операции и `try_lock`.

### 7.5 Простой тест

Временно заменить тело коллбека на минимум:

```rust
move |frame, _| {
    let _ = frame.as_ref().and_then(|p| p.TryGetNextFrame().ok());
    Ok(())
}
```

Если после этого в логе `WGC on_frame_arrived rate: 60 fps` — причина 30 FPS была в обработке внутри коллбека.

### 7.6 FramePool и патч

В крейте по умолчанию пул с 1 буфером; при медленном коллбеке пул переполняется. В проекте **патч** в `client/vendor/windows-capture`: в `Create` и `Recreate` используется **3** буфера. В `Cargo.toml`: `[patch.crates-io]` → `windows-capture = { path = "vendor/windows-capture" }`.

---

# Часть V. Справочное

## 8. Зависимости между фазами

```
Фаза 0 → Фаза 1 → Фаза 2 → Фаза 3 → Фаза 4 → Фаза 5
                    └──────────────┴──────────────┘
                         (при необходимости — Фаза 6)
```

## 9. Риски и ограничения

| Риск | Митигация |
|------|-----------|
| Device из GetDevice() принадлежит WGC | Не вызывать Release на device/context из текстуры; использовать для создания ресурсов и копирования. |
| Порядок каналов RGBA/BGRA | Сверить с ColorFormat; в шейдере задать нужный порядок. |
| Compute shader на старых GPU | Проверка при инициализации; fallback на CPU. |
| Латентность Map | Двойная буферизация staging, освобождение mutex до долгого GetData. |

## 10. Ссылки по коду

- **WGC и ring:** `client/src/voice_livekit.rs` — `start_screen_capture`, `ScreenHandler`, ring, потоки WGC и энкодера.
- **Энкодер:** `client/src/screen_encoder.rs` — `RawFrame`, `VideoEncoder`, CPU/GPU пути.
- **Пресеты:** `client/src/voice.rs` — `ScreenPreset`, `VoiceCmd::StartScreen`.
- **windows-capture:** `Frame::as_raw_texture()`, коллбек в `graphics_capture_api.rs`.
- **D3D11:** `ID3D11Texture2D`, GetDevice, GetImmediateContext, CopyResource.

## 11. TODO и выполненные оптимизации

- **TODO-5 (выполнено):** Прямой memcpy при readback — переход на `RWByteAddressBuffer`, экономия CPU bandwidth (Доработка 9).
- **TODO-3/6 (выполнено):** Упрощение первого кадра в `convert()` — pre-signal query в `new()`, убран sentinel FIRST_FRAME (`d3d11_i420.rs`).
