# LIVEKIT-STATE.md

## Назначение документа

Этот файл описывает **переход с кастомного WebRTC (Pion SFU + str0m) на LiveKit**. При чтении в новом диалоге разработка должна продолжаться с **текущего шага** (см. раздел «Текущий шаг»). Выполняйте шаги строго по порядку.

---

## Цели перехода

- Заменить самописный SFU (Go/Pion) и клиентский WebRTC (Rust/str0m) на **LiveKit**.
- Сохранить: единый Docker для запуска всего окружения; сервер на любом языке (сейчас Go); клиент на **Rust**.
- Сохранить функциональность: голос, микрофон, камера, демонстрация экрана, список участников, мут/громкость, voice-grid UI.

---

## Целевая архитектура

```
┌─────────────────────────────────────────────────────────────────────────┐
│  Docker Compose                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────────────────────────┐ │
│  │ PostgreSQL   │  │ Redis        │  │ Astrix Go Server               │ │
│  │ (db)         │  │ (redis)      │  │ - REST API (auth, servers,     │ │
│  │              │  │              │  │   channels, messages, members)  │ │
│  │              │  │              │  │ - WebSocket (realtime events)  │ │
│  │              │  │              │  │ - POST /voice/token → JWT       │ │
│  │              │  │              │  │   (LiveKit access token)       │ │
│  │              │  │              │  │ - voice_presence в БД          │ │
│  │              │  │              │  │ - LiveKit webhooks:             │ │
│  │              │  │              │  │   participant_joined/left       │ │
│  └──────────────┘  └──────────────┘  └────────────────────────────────┘ │
│                                                                         │
│  ┌────────────────────────────────────────────────────────────────────┐ │
│  │ LiveKit Server (livekit/livekit)                                    │ │
│  │ - Сигналинг (WS) + медиа (UDP/TCP)                                  │ │
│  │ - Комната = channel_<channel_id>; identity = "<user_id>"            │ │
│  │ - Порты: 7880 (signal), 7881 (TCP), 40000-40100 (UDP)                 │ │
│  │ - Redis в livekit.yaml (для масштабирования; можно включить сразу)   │ │
│  └────────────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────────────┘
         │
         │ HTTP/WS (REST + WS events)
         │ LiveKit URL + token (выдаёт Go-сервер)
         ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  Rust Client (egui)                                                      │
│  - livekit crate: Room, connect(url, token), publish/subscribe tracks    │
│  - Аудио/видео через LiveKit SDK (без str0m, cpal, audiopus, vpx-encode) │
│  - UI без изменений: voice grid, участники, mic/cam/screen, громкость   │
└─────────────────────────────────────────────────────────────────────────┘
```

### Identity и синхронизация с БД

- **Room:** `channel_<channel_id>` — одна комната LiveKit = один голосовой канал Astrix.
- **Identity:** `fmt.Sprintf("%d", userID)` — **без префикса** `user_`. Просто числовой `user_id` как строка.
  - Это главный ключ участника в комнате в LiveKit.
  - Проще парсить в webhook, меньше строковых операций, identity = прямой PK в БД.
- **Name:** отображаемое имя (username) для UI в LiveKit.

### Поток «вход в голосовой канал»

1. Пользователь выбирает голосовой канал в UI.
2. Клиент вызывает **POST /voice/join** (channel_id, server_id) с JWT Astrix.
3. Go-сервер: проверяет членство в сервере, генерирует **LiveKit Access Token** (room = `channel_<channel_id>`, identity = `"<user_id>"`, name = username), шлёт WS `voice.participant_joined`, возвращает `{ "livekit_url": "...", "token": "<jwt>", "participants": [...] }`. Запись в `voice_presence` при join делается по желанию для быстрого отображения; **источник истины о присутствии — webhook** (см. ниже).
4. Клиент подключается к LiveKit по `livekit_url` с `token`, публикует аудио (и при включении — камера/экран), подписывается на удалённые треки.
5. При отключении клиент по возможности вызывает **POST /voice/leave** (мягкий запрос). **Источник истины — webhook** `participant_left`: только он гарантирует удаление из `voice_presence` и рассылку `voice.participant_left` (краш клиента, обрыв сети, leave может не вызваться → без webhook будут «зомби»).

### Webhook — источник истины о медиа-присутствии

Webhook **не вспомогательный**, а **критичный**:

- Клиент может крашнуться, сеть — оборваться, `/voice/leave` может не выполниться.
- **Правильная модель:**
  - `participant_joined` → вставить/обновить `voice_presence`, broadcast WS.
  - `participant_left` → удалить из `voice_presence`, broadcast WS.
- `/voice/leave` — только мягкий запрос (клиент сообщает о намерении выйти); фактическое удаление из присутствия и рассылка делаются по webhook.

### Speaking-индикатор (voice-grid)

В LiveKit есть `audio_level`, `is_speaking`, `active_speakers` — но это события уровня **Room**, а не твоего WS. **Speaking обрабатывать только на клиенте:** подписка на события Room, обновление индикаторов в UI. Не тащить speaking через сервер/WS/БД: ниже задержка, меньше нагрузка.

### Что убирается

- **Сервер:** пакет `internal/voice` (Pion SFU: sfu.go, state.go, signaling offer/answer/candidate). Остаются только HTTP: join (→ token), leave, state, update_state и интеграция с LiveKit (токены + webhooks).
- **Клиент:** str0m, ручной SDP/ICE, cpal/audiopus/vpx-encode/screenshots в контексте голоса — заменяются на **livekit** SDK (публикация/подписка треков). При необходимости захват экрана/камеры остаётся через текущие или альтернативные крейты, но отправка идёт через LiveKit.

### Что даёт LiveKit «из коробки»

- Simulcast, adaptive bitrate, лучший congestion control.
- Серверный SFU без своей поддержки — упрощение поддержки Discord-подобного клиента.

### Критические зоны (кратко)

- **Identity** — числовой user_id без префикса; webhook — **источник истины** о присутствии.
- **Speaking** — только на клиенте (низкая задержка, не грузить WS и БД).
- **Громкость 0–300%** — проверить в Rust SDK (RemoteAudioTrack::set_volume или post-processing).
- **Мут микрофона** — через `publication.mute()`, не unpublish (быстрее, не пересоздаёт RTP).
- **UDP** — на VPS открыть диапазон из `livekit.yaml` (на Windows в конфиге стоит 40000–40100 из‑за зарезервированных портов; на Linux — 40000–40100). Иначе медиа не пойдёт.
- **Очистка клиента** — убрать всю логику jitter/Opus/RTP/ICE; иначе конфликты со стороны LiveKit.

---

## Стек технологий после перехода

| Компонент        | Технология |
|------------------|------------|
| Сервер приложения| Go 1.22+, Chi, pgx, Redis (как сейчас) |
| Медиа-сервер     | LiveKit Server (официальный образ) |
| Генерация токенов| `github.com/livekit/server-sdk-go/v2` (auth.NewAccessToken, VideoGrant) |
| LiveKit Webhooks | HTTP endpoint в Go: participant_joined, participant_left, room_finished |
| Клиент           | Rust, egui/eframe (без изменений) |
| Клиент LiveKit   | крейт `livekit` (tokio, Room, TrackPublication, LocalTrack и т.д.) |
| Запуск           | Единый `docker-compose`: db, redis, server, **livekit** |

---

## Порядок шагов (миграция)

Нумерация фиксирована. Каждый шаг завершается тестами/проверкой перед переходом к следующему.

### Фаза 1: Инфраструктура и бэкенд

| Шаг | Описание | Детали |
|-----|----------|--------|
| **1.1** | Добавить LiveKit Server в Docker | В `docker-compose.yml` добавить сервис `livekit` (образ `livekit/livekit`), конфиг `livekit.yaml`: порты 7880, 7881, **UDP 40000-40100** (на VPS/firewall эти порты обязательно открыть — иначе сигналинг есть, аудио нет); API key/secret в env; **Redis** в конфиге LiveKit (уже есть в compose; для масштабирования нужен, лучше включить сразу). Пробросить порты. |
| **1.2** | Конфиг Go для LiveKit | В `server/internal/config`: переменные `LIVEKIT_URL`, `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET`. Читать из env, передавать в места выдачи токенов. |
| **1.3** | Эндпоинт выдачи LiveKit-токена | Handler: POST `/voice/token` или логика внутри join. Проверка членства в сервере; токен: room = `channel_<id>`, **identity = `fmt.Sprintf("%d", userID)`** (без префикса), name = username. Возврат `{ "livekit_url": "...", "token": "..." }`. Использовать `livekit/server-sdk-go` (auth.NewAccessToken, SetVideoGrant, Room, RoomJoin). |
| **1.3.1** | Тестовый токен и проверка LiveKit | До глубокой интеграции в Go: сгенерировать токен вручную (скрипт/Go one-off или livekit-cli), подключиться к LiveKit через **livekit-cli** или минимальный Rust test client. Цель: проверить Docker, порты, конфиг без UI и без Astrix — ускоряет отладку. |
| **1.4** | Адаптировать /voice/join под LiveKit | POST /voice/join: возвращает `livekit_url` + `token` вместо `ice_servers`; по желанию пишет в `voice_presence` и рассылает WS для быстрого отображения. Итоговое присутствие синхронизируется через webhook. |
| **1.5** | LiveKit Webhooks в Go (источник истины) | Роут POST /voice/webhook. Проверка подписи (LiveKit secret). **participant_joined** → вставить/обновить `voice_presence`, broadcast WS. **participant_left** → удалить из `voice_presence`, broadcast WS. Это единственный надёжный способ убрать «зомби» при краше/обрыве. room_finished — при необходимости. URL webhook в `livekit.yaml`. |
| **1.6** | Удалить Pion SFU из Go | Удалить HandleOffer, HandleAnswer, HandleCandidate; убрать sfu.go, state.go (SFU). Оставить в `internal/voice`: маршруты join, leave, state, update_state, webhook; генерация токенов; store и WS. |

### Фаза 2: Клиент Rust

| Шаг | Описание | Детали |
|-----|----------|--------|
| **2.1** | Подключить livekit SDK | В `client/Cargo.toml` добавить `livekit = "…"` (актуальная версия с crates.io). Убедиться, что tokio совместим. **Учесть:** Rust SDK менее зрелый, чем JS/Go — проверить reconnect, обработку смены сети, lifecycle фоновых задач (tokio cancellation); в egui легко получить «висящие» room-задачи. |
| **2.2** | Новый модуль voice на LiveKit | Подключение к комнате по URL + token, публикация локального аудио (микрофон), подписка на удалённые треки. Без камеры/экрана на первом этапе. **Speaking:** обрабатывать только на клиенте (audio_level / is_speaking / active_speakers из Room) — не через сервер/WS/БД: меньше задержка, не грузим WS. Интерфейс для UI: Join/Leave, участники, флаг speaking из событий Room. |
| **2.3** | Интеграция UI с новым voice | Получение из /voice/join `livekit_url` и `token` → Room::connect. Отключение → POST /voice/leave. Список участников из WS (и при необходимости из LiveKit). |
| **2.4** | Камера и экран в клиенте | Публикация видео-треков через LiveKit SDK (LocalVideoTrack; источники — nokhwa/screenshots или аналог). UI: кнопки камеры и экрана, voice-grid. |
| **2.5** | Громкость и мут | **Мут микрофона:** использовать **publication.mute()**, не unpublish — быстрее, не пересоздаёт RTP, UI не дёргается. **Локальная громкость 0–300%:** проверить в livekit crate наличие `RemoteAudioTrack::set_volume()`; если нет — post-processing (gain на аудио-фреймах или встроенный mixer). Синхронизация mic_muted/cam_enabled/streaming с сервером через POST /voice/update_state. |
| **2.6** | Удаление старого WebRTC из клиента | Удалить зависимости: str0m, cpal, audiopus, vpx-encode, screenshots (если не нужны вне голоса). **Убрать из кода полностью:** jitter buffer, Opus encoder/decoder, RTP-логику, ICE-логику — всего этого в клиенте быть не должно, LiveKit делает сам; иначе возможны конфликты и нестабильный звук. Оставить только LiveKit Room + треки. |

### Фаза 3: Финализация

| Шаг | Описание | Детали |
|-----|----------|--------|
| **3.1** | Docker и конфиг LiveKit | Итоговый `livekit.yaml`: порты 7880, 7881, UDP 40000-40100; **Redis** (уже в compose — подключить LiveKit к redis для будущего масштабирования); webhook URL на Go-сервер. В compose: livekit и server видят друг друга по имени сервиса. **Firewall:** на VPS открыть UDP 40000-40100, иначе медиа не пойдёт. |
| **3.2** | Документация и project-state | Обновить `project-state.md`: заменить раздел «Голосовые каналы» на архитектуру с LiveKit; обновить стек (LiveKit вместо Pion/str0m). Удалить или архивировать устаревшие подразделы (этапы 4.x, технические детали str0m). |
| **3.3** | Чистка | Удалить неиспользуемый код в Go (старые хендлеры offer/answer/candidate, если остались), неиспользуемые поля в ответах. Проверить, что WS-события voice.* по-прежнему используются клиентом и что webhook и join/leave согласованы. |

---

## Текущий шаг (для продолжения в новом диалоге)

- **Текущий шаг:** Миграция завершена. Фаза 3 выполнена: **3.1** — livekit.yaml (Redis, webhook, firewall-комментарий), docker-compose без legacy UDP; **3.2** — project-state.md обновлён (архитектура LiveKit, стек, backend/client/WS), устаревшие этапы 4.x и итерации удалены/архивированы; **3.3** — комментарий в ws/hub.go, удалены неиспользуемые поля конфига Go (STUN/TURN/UDP/PublicIP) и соответствующие env/порты в compose.
- **Следующий шаг:** Нет. При необходимости — обновить секцию «Текущий шаг» на «Завершено» или удалить.

**Проверка LiveKit (1.3.1):**
- Установка CLI: `go install github.com/livekit/livekit-cli/cmd/lk@latest` (команда — `lk`, не `livekit-cli`). Либо скачать бинарник с [Releases](https://github.com/livekit/livekit-cli/releases).
- Поднять стек: `docker compose up`.
- Подключиться к комнате (токен CLI генерирует сам из api-key/secret):
  - **PowerShell:** `$env:LIVEKIT_URL="http://livekit.astrix.crazedns.ru"; $env:LIVEKIT_API_KEY="astrixkey"; $env:LIVEKIT_API_SECRET="astrixsecret"; lk room join channel_1`
  - Либо сохранить проект: `lk project add` (url, api-key, api-secret), затем `lk room join channel_1`.
- Флаг `--token` у `lk room join` **нет**; передаётся только имя комнаты, токен создаётся из проекта/env.
- При тесте без канала в БД Astrix в логах сервера может появиться `voice webhook: channel 1 not found` — это нормально (webhook пришёл по комнате `channel_1`, в БД канала с id=1 нет). При входе в голос через клиент Astrix канал уже будет в БД.

---

## Справочник: ключевые файлы

| Роль | Путь |
|------|------|
| Docker | `docker-compose.yml`, `server/Dockerfile` |
| Конфиг Go | `server/internal/config/config.go` |
| Голос HTTP | `server/internal/voice/signaling.go`, `server/internal/voice/livekit.go`, `server/internal/voice/webhook.go`, `server/internal/httpserver/server.go` |
| Voice state (in-memory + WS) | `server/internal/voice/state.go` (Pion удалён, только комнаты/участники) |
| Store voice_presence | `server/internal/store/store.go` (VoiceJoin, VoiceLeave, VoiceUpdateState, VoiceListPresence) |
| WS Hub | `server/internal/ws/hub.go` |
| Клиент API | `client/src/net.rs` (voice_join, voice_leave, voice_update_state, voice_state) |
| Клиент голос | `client/src/voice.rs` |
| Клиент UI голос | `client/src/ui.rs` (MainState.voice*, voice_engine_tx, voice_video_textures) |
| Зависимости клиента | `client/Cargo.toml` |

---

## Требуемые технологии (кратко)

- **LiveKit Server**: официальный образ, конфиг YAML; порты 7880 (signal), 7881 (TCP), **40000-40100 (UDP)** — на VPS/firewall UDP обязательно открыть, иначе аудио не будет.
- **Redis для LiveKit**: в `livekit.yaml` указать Redis (из того же compose); для горизонтального масштабирования нужен; лучше включить сразу.
- **Go**: `github.com/livekit/server-sdk-go/v2` для токенов; при необходимости `protocol` для webhook payload.
- **Rust**: крейт `livekit` (клиент); tokio уже есть. **Зрелость:** Rust SDK работает, но менее зрелый, чем JS/Go — уделить внимание reconnect, смене сети, отмене tokio-задач (чтобы не оставались висящие room-задачи в egui).
- **Единый Docker**: один `docker-compose up` — db, redis, astrix-server, livekit.

После завершения всех шагов этот документ можно оставить как описание архитектуры и истории миграции; секцию «Текущий шаг» заменить на «Завершено» или удалить.
