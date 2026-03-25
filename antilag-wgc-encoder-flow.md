# Текущая реализация: WGC → Encoder pipeline

Анализ для сравнения с предложенной архитектурой (OBS/Parsec style).

---

## Как сейчас устроено

### 1. WGC (capture thread)

**Триггер:** `FrameArrived` — вызывается Windows compositor, **без гарантии стабильного FPS**.

**Действия:**
- `CopyResource` в текстуру из pool (6 слотов, round-robin)
- Push `GpuSlot(slot_index)` в `gpu_ring` (lock-free SPSC)
- Если encoder держит `pool_ref` или `context_mutex` — **кадр дропается** (try_lock, не блокируем WGC)

**Итог:** WGC пушит кадры в ring при каждом FrameArrived, без контроля интервалов.

---

### 2. Encoder loop (encoder thread)

**Таймер:** `next_frame_at` — фиксированный интервал (11.11 ms для 90 fps).

**Цикл:**
```
loop {
    now = Instant::now()
    if now < next_frame_at  →  sleep до next_frame_at
    
    gpu_slot = gpu_ring.pop_skip_to_newest_if_behind()
    
    if gpu_slot.is_none() (MFT path):
        if have_last_frame:
            re-encode last NV12 texture  // static screen
        sleep до next_frame_at
        next_frame_at += interval
        continue
    
    if gpu_slot:
        encode(texture)
        push_frame(..., rtp_timestamp, capture_us, ...)
        rtp_timestamp += step
        next_frame_at += interval
}
```

**Важно:**
- Encoder **уже работает по таймеру** (next_frame_at)
- Кадр берётся из ring: при 2+ в очереди — `pop_skip_to_newest_if_behind` отдаёт **новейший**
- При 0–1 в очереди — FIFO (старейший)

---

### 3. RTP и capture_time

- **rtp_timestamp:** `+= rtp_step` (90000/fps) — фиксированный шаг
- **capture_time_us:** `stream_start_at.elapsed()` — wall-clock в момент `push_frame`

---

## Отличия от предложенной схемы

### Что уже совпадает

1. Encoder идёт по фиксированному таймеру (next_frame_at)
2. RTP timestamps монотонны с фиксированным шагом
3. При отсутствии нового кадра — re-encode последнего (static screen)

### Что отличается

| Аспект | Сейчас | OBS/Parsec style |
|--------|--------|------------------|
| **Буфер кадров** | Ring 6 слотов, FIFO или skip-to-newest | Один слот `latest_frame`, overwrite |
| **Когда encoder берёт кадр** | Pop из ring в момент тика | Читает `latest_frame` в момент тика |
| **При 1 кадре в очереди** | Берётся FIFO (единственный) | Берётся latest (тот же) |
| **При 2+ кадрах** | Skip to newest, старые дропаются | Latest уже самый новый |

---

## Потенциальные источники jitter

### 1. capture_time = wall-clock при push

`capture_time_us = stream_start_at.elapsed()` — момент **отправки**, а не реального capture.

- Кадр мог быть захвачен на 0–11 ms раньше
- При переменном времени encode (keyframe дольше) интервалы между `capture_time` нестабильны
- WebRTC может воспринимать это как jitter

### 2. Ring vs single latest

При 1 кадре в ring — берётся он. При 2+ — берётся newest. Логика близка к "latest frame", но:

- Ring даёт буфер: если encoder отстал, кадры копятся
- Single latest: каждый новый кадр перезаписывает, encoder всегда видит только последний

### 3. Дропы WGC

WGC может не вызывать FrameArrived для части кадров. Тогда:

- В ring — меньше кадров, encoder чаще re-encodes last
- При single latest — latest просто не обновляется, encoder re-encodes тот же кадр

Поведение по сути то же.

### 4. Переменное время encode

Keyframe ~15–25 ms, P-frame ~3–8 ms. `push_frame` вызывается после encode, значит `capture_time` и реальный момент отправки пакетов смещаются на время encode. Интервалы между `capture_time` перестают быть ровно 11.11 ms.

---

## Вывод

**Текущая схема уже близка к "encoder по таймеру":**
- Фиксированный интервал encoder loop
- RTP step фиксирован
- При наличии кадров — берётся newest (при 2+ в очереди)

**Возможные улучшения по предложенной архитектуре:**

1. **Single latest вместо ring** — один слот, overwrite. Упрощение и гарантия "всегда самый новый кадр".
2. **capture_time = момент capture** — если WGC/пул даёт timestamp кадра, использовать его вместо `elapsed()` при push.
3. **Учёт времени encode в capture_time** — вычитать время encode, чтобы `capture_time` лучше соответствовал моменту capture.

---

## Реализовано (OBS/Parsec style)

1. **Ring заменён на single latest** — `LatestSlot` (AtomicU8), double buffer (2 текстуры)
2. **WGC** — копирует в texture[1 - current], затем `latest_slot.store(write_slot)`
3. **Encoder** — по таймеру читает `latest_slot.load()`, кодирует texture[slot]
4. **capture_time** — оставлен `stream_start_at.elapsed()`
