# Roadmap:

## Контекст

**Цель:** включить многопоточность в H.264 энкодере (I420 → H.264 при publish), чтобы снизить нагрузку на CPU и стабилизировать FPS при screen share 1080p60/1440p60.

**Проблема:** В libwebrtc (Chromium WebRTC) функция `NumberOfThreads()` в `h264_encoder_impl.cc` всегда возвращает `1`. Многопоточность отключена из‑за sandbox на macOS (crbug.com/583348). OpenH264 поддерживает `iMultipleThreadIdc`, но получает только 1 поток.

**Целевая платформа:** только Windows (sandbox не применяется, многопоточность должна работать).

**Стек:** Astrix → livekit 0.7 → libwebrtc 0.3 → libwebrtc C++ (Chromium WebRTC).

---
Перед каждой фазой запрашивать подтверждение на продолжение.
Ключевые изменения вносить в этот файл в зависимости от фазы.
---


## Phase 0: Подготовка и исследование

- [x] **0.1** Убедиться, что LiveKit/libwebrtc использует стандартный `h264_encoder_impl.cc` (не кастомную сборку)
- [x] **0.2** Найти исходник в используемой версии: `modules/video_coding/codecs/h264/h264_encoder_impl.cc`
- [x] **0.3** Проверить `arcas-io/libwebrtc` и `livekit/rust-sdks`: откуда берётся libwebrtc C++ (prebuilt или сборка из Chromium)
- [x] **0.4** Изучить `SEncParamExt.iMultipleThreadIdc` и `sSliceArgument.uiSliceNum` в OpenH264 — связь потоков и слайсов

**Результат:** понимание, где и как менять код.

---

### Результаты Phase 0 (выполнено)

**0.1 — Стандартный h264_encoder_impl.cc:** Да. LiveKit использует prebuilt libwebrtc из Chromium WebRTC. В `NumberOfThreads()` — стандартная реализация: `return 1` (многопоточность закомментирована из‑за crbug.com/583348).

**0.2 — Путь к исходнику:**
- Файл: `modules/video_coding/codecs/h264/h264_encoder_impl.cc`
- Репозиторий: https://chromium.googlesource.com/external/webrtc/ (или webrtc.googlesource.com/src/)
- В `CreateEncoderParams()` вызывается `NumberOfThreads()` → результат передаётся в `encoder_params.iMultipleThreadIdc`

**0.3 — Источник libwebrtc C++:**
- **НЕ arcas-io.** LiveKit/rust-sdks использует свой стек: `livekit` → `libwebrtc` (crate) → `webrtc-sys` → `webrtc-sys-build`
- **Prebuilt:** скачивается с https://github.com/livekit/rust-sdks/releases/download/webrtc-0001d84-2/webrtc-{triple}.zip (для Windows: `webrtc-win-x64-release.zip`)
- **LK_CUSTOM_WEBRTC:** env-переменная для пути к кастомной сборке (см. issue #531)
- Локальный путь prebuilt: `{scratch}/livekit_webrtc/livekit/win-x64-release-webrtc-0001d84-2/win-x64-release/`

**0.4 — OpenH264 параметры:**
- `iMultipleThreadIdc`: 0=auto, 1=один поток, >1=число потоков. Сейчас всегда 1 (из `NumberOfThreads()`)
- `sSliceArgument.uiSliceNum`: при `SM_FIXEDSLCNUM_SLICE` — число слайсов. Сейчас =1. TODO в коде: «Set to 0 when we understand why the rate controller borks when uiSliceNum > 1»
- **Связь:** при многопоточности OpenH264 делит кадр на слайсы; `uiSliceNum` должен соответствовать числу потоков или 0 (auto)

---

## Phase 1: Форк и сборка

- [x] **1.1** Определить источник libwebrtc C++: Chromium depot_tools, LiveKit webrtc-xcframework, или другой
- [x] **1.2** Форкнуть или получить локальную копию libwebrtc, совместимую с `arcas-io/libwebrtc` / `livekit`
- [x] **1.3** Настроить `LK_CUSTOM_WEBRTC` (см. livekit/rust-sdks issue #531) для сборки с модифицированным libwebrtc
- [x] **1.4** Собрать Astrix с форком без изменений — убедиться, что всё работает

**Результат:** воспроизводимая сборка с кастомным libwebrtc.

---

### Результаты Phase 1 (выполнено)

**1.1 — Источник libwebrtc C++:**
- **Prebuilt:** LiveKit GitHub releases — `webrtc-0001d84-2`, zip ~89 MB
- **Сборка из исходников:** `webrtc-sys/libwebrtc/build_windows.cmd` — depot_tools, gn, ninja, MSVC 2022

**1.2 — Локальная копия:**
- Скрипт `client/scripts/fetch-webrtc-prebuilt.ps1` — скачивает и распаковывает prebuilt
- Папка `client/webrtc-prebuilt/win-x64-release/` (добавлена в .gitignore)
- ⚠️ **Проверка:** распакованный zip может не содержать `lib/` — при ошибке сборки перескачать или собирать из Chromium

**1.3 — LK_CUSTOM_WEBRTC:**
- В `client/.cargo/config.toml` добавлена переменная (закомментирована — см. ниже)
- Формат: `{ value = "webrtc-prebuilt/win-x64-release", relative = true }`

**1.4 — Сборка:**
- Сборка **без** LK_CUSTOM_WEBRTC: ✅ успешна (default prebuilt)
- Сборка **с** LK_CUSTOM_WEBRTC: требует полный prebuilt (include/, lib/webrtc.lib, webrtc.ninja, desktop_capture.ninja). При отсутствии lib/ в распакованном zip — использовать сборку из Chromium (Phase 2).

**Для Phase 2:** сборка из исходников — `client/vendor/` или клон `livekit/rust-sdks`, затем `webrtc-sys/libwebrtc/build_windows.cmd`. После сборки указать LK_CUSTOM_WEBRTC на `win-x64-release/`.

---

## Phase 2: Включение многопоточности в H.264 encoder

- [x] **2.1** Открыть `modules/video_coding/codecs/h264/h264_encoder_impl.cc`
- [x] **2.2** Изменить `NumberOfThreads()`: вариант C — `#if defined(WEBRTC_WIN)` (2–8 потоков на Windows)
- [x] **2.3** `sSliceArgument.uiSliceNum` — Astrix: multi-slice по разрешению (720p→2-3, 1080p→4, 1440p→6), capped by iMultipleThreadIdc. bEnableAdaptiveQuant=false при uiSliceNum>1 (workaround cisco/openh264#2591).
- [x] **2.4** Патч и скрипт сборки добавлены
- [x] **2.5** Собрать libwebrtc, собрать Astrix — ✅ успешно

**Результат:** OpenH264 использует несколько потоков на Windows.

---

### Результаты Phase 2 (выполнено)

**Патч:** `client/vendor/libwebrtc/patches/h264_multithread_windows.patch`
- Включает многопоточность только на Windows (`#if defined(WEBRTC_WIN)`)
- 8 потоков: 1080p+ при >8 ядрах
- 3 потока: 1080p при ≥6 ядрах
- 2 потока: qHD/HD при ≥3 ядрах

**Рекомендуемая сборка:** `client/scripts/build-webrtc-via-livekit.ps1`
- Клонирует livekit/rust-sdks, добавляет наш патч, собирает из их дерева
- Патчи livekit применяются корректно (vendor-подход может давать `patch failed`)
- Результат: `client/webrtc-prebuilt/win-x64-release/`

**Альтернатива:** `client/scripts/build-webrtc-windows.ps1` — сборка из `vendor/libwebrtc` (требует совместимости патчей с webrtc-sdk)

**Финальные шаги:** раскомментировать `LK_CUSTOM_WEBRTC = { value = "webrtc-prebuilt/win-x64-release", relative = true }` в `client/.cargo/config.toml` → `cargo build` ✅

---

### Troubleshooting сборки libwebrtc

**1. Патчи не применяются** (`patch failed`, `patch does not apply`):
- Патчи livekit (add_licenses, add_deps, windows_silence_warnings, ssl_verify_callback) рассчитаны на другую версию webrtc-sdk. Решение: клонировать [livekit/rust-sdks](https://github.com/livekit/rust-sdks), выполнить сборку из их `webrtc-sys/libwebrtc/` и добавить только наш `h264_multithread_windows.patch` в их `build_windows.cmd`.

**2. dbghelp.dll / Windows SDK 10.0.26100.0**:
- Visual Studio Installer → Modify → Individual components → найти «Windows 10 SDK» (10.0.26100 или новее) и «Debugging Tools for Windows» → установить.
- Альтернатива: `scripts/build-webrtc-via-livekit.ps1` — клонирует livekit/rust-sdks и собирает из их дерева (патчи должны применяться).

**3. ninja не найден**:
- Появляется, если `gn gen` падает (например, из‑за SDK). Сначала устранить ошибки `gn gen`.

**4. mkdir «уже существует»**:
- Удалить `client/vendor/libwebrtc/win-x64-release` и `client/vendor/libwebrtc/src` перед повторной сборкой.

---

## Phase 3: Валидация и настройка

- [ ] **3.1** Сравнить CPU load: до и после (Task Manager, один участник, screen share)
- [ ] **3.2** Проверить стабильность FPS, отсутствие артефактов, задержку
- [ ] **3.3** При необходимости подобрать число потоков (2/4/6/8) под типичные конфигурации
- [ ] **3.4** Документировать изменения (patch, diff) для воспроизводимости

**Результат:** подтверждённый выигрыш по CPU/FPS без регрессий.

---

## Phase 4: Поддержка (опционально)

- [ ] **4.1** Стратегия обновлений: патч при каждом обновлении libwebrtc или скрипт автоматического применения
- [ ] **4.2** Рассмотреть PR в Chromium/WebRTC: многопоточность только для Windows (маловероятно принять, но можно попробовать)
- [ ] **4.3** Добавить в документацию Astrix: «требуется кастомная сборка libwebrtc с многопоточным H.264»

---

## Ключевые файлы и ссылки

| Что | Где |
|-----|-----|
| `NumberOfThreads()` | `modules/video_coding/codecs/h264/h264_encoder_impl.cc` |
| OpenH264 `iMultipleThreadIdc` | `SEncParamExt`, передаётся в `InitializeExt()` |
| `sSliceArgument.uiSliceNum` | `CreateEncoderParams()`, слой 0, оба режима packetization |

### Slice mode (h264_encoder_params.patch)

**Режим:** SM_FIXEDSLCNUM_SLICE, uiSliceNum по разрешению и fps:
- 720p30 → 2, 720p60 → 3
- 1080p30 → 4, 1080p60 → 4
- 1440p30 → 4, 1440p60 → 6
- Capped by iMultipleThreadIdc (не больше потоков)

**Workaround rate controller:** При uiSliceNum>1 ставим `bEnableAdaptiveQuant = false` — иначе OpenH264 неправильно считает fps (cisco/openh264#2591, 2016). Качество может немного снизиться, но RC стабилен.

**Риски:** При проблемах с битрейтом/качеством — откатить на uiSliceNum=1.
| LK_CUSTOM_WEBRTC | env var для webrtc-sys-build, путь к кастомному libwebrtc |
| Сборка (рекомендуется) | `client/scripts/build-webrtc-via-livekit.ps1` |
| Prebuilt download | `https://github.com/livekit/rust-sdks/releases/download/webrtc-0001d84-2/webrtc-{triple}.zip` |
| crbug sandbox | crbug.com/583348 (Mac) |

---

## Риски

| Риск | Митигация |
|------|-----------|
| arcas-io/libwebrtc использует prebuilt, без исходников | Проверить webrtc-sys, возможно нужна полная сборка из Chromium |
| Сборка libwebrtc сложна (depot_tools, GN, etc.) | Использовать инструкции LiveKit/arcas для custom build |
| Регрессия качества при многопоточности | Замерить bitrate/качество; при необходимости ограничить 2–4 потоками |

---

## Оценка трудозатрат

| Phase | Оценка |
|-------|--------|
| 0 | 0.5–1 день |
| 1 | 1–2 дня (если сборка уже настроена — быстрее) |
| 2 | 0.5–1 день |
| 3 | 0.5–1 день |
| 4 | по необходимости |

**Итого:** ~2–4 дня при наличии опыта со сборкой WebRTC.

---

## Следующие шаги (для продолжения в новом диалоге)

1. ~~Phase 0–2~~ — выполнено. Astrix собран с многопоточным H.264.
2. **Phase 3** — валидация: screen share 1080p60/1440p60, замерить CPU и FPS в Task Manager.
3. **Phase 4** (опционально) — стратегия обновлений, документация.

---

### Дополнение: окно статистики — потоки энкодера и декодера

В окно статистики добавлено отображение **потоков H.264-энкодера** (публикующий) и **потоков H.264-декодера** (смотрящий):

- **encoder_threads** — число потоков OpenH264 (CPU path). `None` при GPU или без трансляции.
- **decoder_threads** — число потоков FFmpeg (CPU path, смотрящий). `None` при GPU или без видео.
- В UI: «Потоки энкодера:» и «Потоки декодера:».

---

### Дополнение: многопоточный H.264 декодер (смотрящий)

**Патч:** `client/vendor/libwebrtc/patches/h264_decoder_multithread_windows.patch`

- В `h264_decoder_impl.cc` (FFmpeg) вместо `thread_count = 1` — 2–4 потока на Windows по разрешению и числу ядер.
- 1080p+ и ≥6 ядер → 4 потока; 720p+ и ≥4 ядер → 2 потока; иначе 1.
- Применяется вместе с encoder patch при сборке libwebrtc (`build_windows.cmd`, `build-webrtc-via-livekit.ps1`).
