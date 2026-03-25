# PROJECT_STATE.md

## Проект
Astrix — десктопное приложение для чата (аналог Discord).

## Цель
Самостоятельно размещаемая система чатов с поддержкой:
- серверов
- текстовых и голосовых каналов
- управления участниками
- системы сообщений
- последующего внедрения E2EE
- обновлений, близких к реальному времени

---

## Стек

### Frontend
- Rust
- egui 0.28
- eframe 0.28
- reqwest
- tokio (block_on внутри UI)
- Голос/видео: LiveKit SDK (крейт `livekit`), cpal для аудио I/O
- Захват экрана: `windows-capture` (WGC, GPU-based, Windows) / `xcap` (fallback macOS/Linux)

### Backend
- Go 1.22+
- Chi v5
- PostgreSQL (pgx v5)
- JWT-аутентификация
- Docker
- Голос: LiveKit Server (отдельный контейнер), выдача JWT и webhook в Go

---

## Архитектура

Rust-клиент (egui)
        ↓ HTTP REST
Go-сервер (Chi)
        ↓
PostgreSQL

Сообщения пока хранятся как plaintext в bytea.
В будущем — переход на E2EE.

---

## Реализовано (Backend)

### Аутентификация
- Регистрация
- Логин
- JWT middleware
- user_id и username добавляются в context
- TokenResponse: access_token, user_id, username

### Серверы
- Создание сервера
- Получение списка серверов пользователя
- Удаление сервера (только владелец)
- Выход с сервера

### Каналы
- Создание текстовых и голосовых каналов
- Получение списка каналов сервера

### Участники
- Получение списка участников сервера
- Приглашение по user_id
- AddMemberToServer (ON CONFLICT DO NOTHING)

### Сообщения
- Создание сообщения
- Получение последних 200 сообщений канала
- MessageRow:
  - id
  - channel_id
  - author_id
  - author_username
  - content
  - created_at

---

## Реализовано (Frontend)

### Экраны
- Login
- Register
- Main

### Layout (как в Discord)
Слева → колонка серверов (круги), далее колонка каналов (текстовые + голосовые).
Центр → чат (текстовый канал) или voice-grid (голосовой канал).
Справа → список участников сервера.

### Голосовой UI (принятая структура)

**Левая колонка** (после подключения к голосовому каналу):
- Кнопки: **Disconnect** (📵), **Mic on/off** (🎤), **Output mute** (🔇).
- Под списком голосовых каналов — список подключённых пользователей выбранного канала, отступ 30 px.
- ПКМ по пользователю: «Заглушить локально» / «Снять локальный мут»; **Громкость 0–300%**, по умолчанию 100%.

**Центральная область (voice-grid)** при выборе голосового канала:
- Аватары/видео в виде **скруглённых прямоугольников** (Rounding 12 px).
- **Адаптивная сетка**: 1 участник → почти весь экран; 2 → 50/50; 4 → 2×2; N → auto-grid (число столбцов по √N).
- Под сеткой кнопки: **Mic toggle**, **Camera toggle**, **Screen share**, **Disconnect** (центральный Disconnect выставляет `voice_pending_leave`, обрабатывается тем же блоком, что и кнопка в левой колонке).
- Поддержка нескольких video tracks от одного пользователя (камера + стрим): заложена при переходе на ключ `(user_id, track_id)` в `voice_video_textures`; пока один кадр на пользователя.

### Функции
- Диалог создания сервера
- Удаление сервера (ПКМ)
- Создание канала
- Диалог приглашения
- Отображение собственного ID
- Копирование ID (левый клик)
- Logout
- Переключение тёмной темы
- Отправка сообщений
- Загрузка сообщений при выборе канала
- Загрузка участников при выборе сервера
- Закреплённая снизу панель ввода
- Закреплённая панель пользователя

---

## Ограничения (текстовый чат)

- Нет WebSocket
- Нет real-time push
- Нет автообновления серверов после приглашения
- Нет группировки сообщений
- Нет восстановления выбранного сервера при повторном входе
- Нет tooltip полного названия сервера
- Нет отметки владельца сервера
- block_on() блокирует UI при запросах

---

## API Base
http://astrix.astrix.crazedns.ru

---

## Основные таблицы БД

users  
servers (owner_id)  
channels (server_id, is_voice)  
server_members (server_id, user_id)  
messages (channel_id, author_id, ciphertext bytea)

---

## Принятые решения

- Интерфейс в стиле Discord
- Immediate-mode UI (egui)
- Использование owned Vec перед замыканиями (из-за borrow checker)
- TopBottomPanel::bottom().show_inside() для закреплённых панелей
- LIMIT 200 сообщений на загрузку
- ON CONFLICT DO NOTHING для приглашений
- Голосовой UI: управление (Disconnect, Mic, Output) в левой колонке после подключения; центральный voice-grid с адаптивной сеткой и скруглёнными тайлами; кнопки Mic/Camera/Screen/Disconnect под сеткой; выход из центра задаёт `MainState.voice_pending_leave`, обрабатывается в том же блоке, что и кнопка в левой колонке
- Screen share (Windows): `windows-capture` (WGC/D3D11, GPU) вместо `xcap` (GDI/CPU) — включается через feature `wgc-capture` (default); двухпоточный pipeline: WGC callback → latest_frame slot + encoder thread с точным таймером
- YUV конверсия: libyuv называет форматы по обратному порядку байт (ABGR в libyuv = RGBA в памяти); отправитель — `abgr_to_i420`, получатель — `to_argb(VideoFormatType::ABGR)`
- Screenshare TrackPublishOptions: bitrate и fps берутся из выбранного пресета `ScreenPreset`, `simulcast: false` — переопределяет дефолтный пресет LiveKit (≤30 fps)
- Screen share quality presets: `ScreenPreset` enum в `voice.rs` — 6 вариантов (720p30, 720p60, 1080p30, 1080p60, 1440p30, 1440p60); по умолчанию 1080p30; выбирается в диалоге перед началом трансляции; хранится в `MainState.screen_preset`
- Encoder thread точный таймер (Windows): `timeBeginPeriod(1)` поднимает разрешение системного таймера до 1 мс; `coarse sleep до deadline-2ms + spin-yield` для точного попадания в кадровый бюджет; переиспользование `src I420Buffer` между кадрами чтобы не аллоцировать МБ-блоки на каждый кадр при высоких разрешениях

---

## Голосовые каналы

### Архитектура (LiveKit)

Медиа-сервер — **LiveKit Server** (отдельный контейнер). Go-бэкенд выдаёт JWT-токен доступа и синхронизирует присутствие через webhook. Клиент подключается к LiveKit по URL + токен, публикует/подписывает треки.

```
Rust Client (egui)  ──HTTP──►  Go Server (join → token, leave, update_state)
       │                              │
       │  ws + media                  │  POST /voice/webhook (participant_joined/left)
       ▼                              ▼
  LiveKit Server  ◄───────────────────  source of truth for presence
  (signal 7880, RTC UDP 40000–40100)
```

- **Комната:** `channel_<channel_id>`. **Identity:** числовой `user_id` (строка). **Name:** отображаемое имя.
- **Источник истины о присутствии:** webhook LiveKit (`participant_joined` / `participant_left`). Join/leave в Go — оптимистичное обновление; при краше клиента webhook убирает «зомби».
- **Speaking:** только на клиенте (события Room: ActiveSpeakersChanged), не через WS/БД.

### Стек голосового слоя

| Уровень | Технология |
|---|---|
| Медиа-сервер | LiveKit Server (Docker), Redis для масштабирования |
| Токены и webhook | Go, `github.com/livekit/server-sdk-go`, POST /voice/webhook |
| Клиент медиа | Rust, крейт `livekit` (Room, треки, публикация/подписка) |
| Аудио I/O клиента | `cpal` (микрофон → LiveKit, вывод из микшера удалённых треков) |
| Видео | LiveKit NativeVideoSource; захват экрана — `windows-capture` WGC (Windows) / `xcap` (fallback) |

### Сеть и Docker

- **Go server:** `HTTP_ADDR`, `LIVEKIT_URL`, `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET`.
- **LiveKit:** порты 7880 (signal), 7881 (TCP), UDP 40000–40100 (на VPS — открыть в firewall).
- В `docker-compose`: сервисы `server` и `livekit`; webhook URL: `http://server:8080/voice/webhook`.

### Backend (server/internal/voice/)

- **signaling.go** — POST /voice/join (возвращает `livekit_url`, `token`, `participants`), /voice/leave, /voice/mute (update_state), GET /voice/state.
- **livekit.go** — создание JWT (room, identity, name).
- **webhook.go** — проверка подписи, participant_joined → VoiceJoin + broadcast, participant_left → VoiceLeave + broadcast.
- **state.go** — in-memory Manager (комнаты, участники), без медиа-логики.

### Client (client/src/)

- **voice.rs** — `VoiceCmd`, `ScreenPreset`, `spawn_voice_engine()`, `VideoFrames`; движок — `voice_livekit::run_engine`.
- **voice_livekit.rs** — `Room::connect`, публикация микрофона (cpal → NativeAudioSource), подписка на удалённые треки, микшер по громкости 0–300%, камера/экран (LocalVideoTrack), мут через `publication.mute()`; encoder thread с `timeBeginPeriod(1)` и spin-yield таймером.
- **ui.rs** — voice-grid, кнопки Mic/Camera/Screen/Disconnect, список участников, ПКМ: громкость/заглушить; выбор экрана по индексу; диалог выбора источника содержит панель пресетов качества (`selectable_label`); `MainState.screen_preset` хранит выбранный пресет.

### WS-события

| Направление | Событие |
|---|---|
| Server → Client | `voice.participant_joined`, `voice.participant_left`, `voice.state_update` |
| Client → Server | только HTTP (join, leave, mute, state) |

### Демонстрация экрана (screen share)

#### Захват (отправитель)

| Платформа | Метод | Формат пикселей |
|---|---|---|
| Windows (feature `wgc-capture`) | `windows-capture` 1.5 (WGC / D3D11) | `ColorFormat::Rgba8` → байты `[R,G,B,A]` |
| macOS / Linux | `xcap` 0.0.12 (polling) | `RgbaImage` → байты `[R,G,B,A]` |

**Pipeline (Windows):**
1. WGC callback (`on_frame_arrived`) — копирует сырые пиксели в shared `latest_frame` слот (`Arc<Mutex<Option<(Vec<u8>, w, h)>>>`) без тяжёлой обработки; аллокация переиспользуется если размер кадра не изменился.
2. Encoder thread (тик ~16 мс) — забирает последний кадр, вызывает `abgr_to_i420` (libyuv SIMD, `[R,G,B,A]` = libyuv ABGR), затем `I420Buffer::scale()` до `VIDEO_WIDTH × VIDEO_HEIGHT`, публикует в `NativeVideoSource`.

Разделение на два потока позволяет WGC доставлять кадры с частотой монитора (60–144 Гц), не дожидаясь завершения конверсии.

#### Конверсия форматов (libyuv naming vs memory layout)

| libyuv имя | байты в памяти (LE) | описание |
|---|---|---|
| `ARGB` | `[B, G, R, A]` | то, что обычно называют BGRA |
| `ABGR` | `[R, G, B, A]` | то, что egui/большинство API называют RGBA ← **нужный формат** |
| `RGBA` | `[A, B, G, R]` | нестандартный порядок, не используется |
| `BGRA` | `[A, R, G, B]` | то, что обычно называют ARGB |

- **Отправитель:** `abgr_to_i420` (принимает `[R,G,B,A]` = libyuv ABGR).
- **Получатель:** `buf.to_argb(VideoFormatType::ABGR, ...)` → байты `[R,G,B,A]` для `egui::ColorImage::from_rgba_unmultiplied`.

#### Пресеты качества трансляции (ScreenPreset)

| Пресет | Разрешение | FPS | Битрейт |
|---|---|---|---|
| P720F30 | 1280×720 | 30 | 2.5 Мбит/с |
| P720F60 | 1280×720 | 60 | 4 Мбит/с |
| P1080F30 | 1920×1080 | 30 | 5 Мбит/с |
| P1080F60 | 1920×1080 | 60 | 8 Мбит/с |
| P1440F30 | 2560×1440 | 30 | 8 Мбит/с |
| P1440F60 | 2560×1440 | 60 | 14 Мбит/с |

По умолчанию — `P1080F30`. Параметры передаются в `TrackPublishOptions` при публикации; `simulcast: false` для всех пресетов.

#### Настройки публикации screenshare трека

```rust
let (_, _, fps, bitrate) = preset.params();
opts.video_encoding = Some(VideoEncoding { max_bitrate: bitrate, max_framerate: fps });
opts.simulcast = false;
```

Дефолтные screenshare пресеты LiveKit SDK ограничивают `max_framerate` до 5–30 fps. Явная установка fps/bitrate из пресета и отключение simulcast обязательны.

#### Encoder thread: точный таймер (Windows)

Проблема: `std::thread::sleep` на Windows неточен (шаг ~15.6 мс по умолчанию) — при 60 к/с систематически пропускает кадры.

Решение:
1. `timeBeginPeriod(1)` (winmm.dll) — таймер ОС переключается в 1 мс режим на время жизни encoder thread.
2. Алгоритм: `sleep(deadline - 2ms)` + `spin_loop()` на последние 2 мс — гарантирует попадание в дедлайн без дрейфа.
3. `next_frame_at += frame_interval` (абсолютная точка) вместо `now + interval` — накопленная ошибка не растёт.
4. Переиспользование `src_buf: Option<I420Buffer>` — при стабильном разрешении захвата нет аллокации МБ-блока каждый кадр.

---

Миграция с Pion/str0m на LiveKit описана в **livekit-state.md**.

Устаревшие этапы 4.x и история итераций (Pion/str0m) не перенесены; при необходимости см. историю коммитов или livekit-state.md.