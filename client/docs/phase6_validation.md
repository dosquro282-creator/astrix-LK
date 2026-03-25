# Phase 6: Валидация Zero-CPU-Readback GPU H.264 Pipeline

Чеклист для проверки MFT GPU пути screen share. Большинство пунктов — ручная проверка.

---

## Статус автоматической проверки

```
cargo run --example validate_mft_path
```

**Результат на NVIDIA GeForce RTX 3080 (Windows 11):**
```
  D3d11BgraToNv12::new ... OK
  MftH264Encoder::new (hardware) ... OK (NVIDIA H.264 Encoder MFT hardware)
  OK: D3d11BgraToNv12 + MftH264Encoder (NVIDIA H.264 Encoder MFT hardware)
```

> **Важно:** `validate_mft_path` проверяет только инициализацию объектов, но не реальную конвертацию кадров. При реальной трансляции `VideoProcessorBlt` возвращает E_INVALIDARG на RTX 3080 и RTX 5080 → MFT путь падает на первом кадре → fallback на CPU/I420 (OpenH264). Фактически MFT zero-copy путь не работает ни на одном из протестированных GPU. См. «Известная проблема» ниже.

**Исправленные баги при отладке (Phase 6):**
- `MF_MT_FRAME_RATE` кодировался неправильно: `(1<<32)|fps` вместо `(fps<<32)|1` — software MFT возвращал «Frame rate out of range»
- `MF_MT_FRAME_SIZE` кодировался неправильно: `(height<<32)|width` вместо `(width<<32)|height`
- Hardware MFT выбирался неправильно: `MFTEnumEx` возвращает `AMDh264Encoder` первым, но он не принимает `SET_D3D_MANAGER`; теперь перебираем все и выбираем первый принявший
- Device manager создавался после `create_mft` — NVIDIA NVENC требует один device manager и один вызов `SET_D3D_MANAGER`; теперь создаётся до и передаётся внутрь
- `SetOutputType` с ручным типом не работает на NVIDIA — нужно использовать `GetOutputAvailableType` как базу
- NVIDIA NVENC не принимает разрешение 128×128 — нужно использовать реальные размеры (≥ 320×240)
- **[Исправлено в 6.1]** При `auto` режиме и fallback на CPU — смотрящий не видел картинку: трек был опубликован как `NativeEncodedVideoSource` (H.264), но при `D3d11BgraToNv12::convert` E_INVALIDARG код переходил на I420 путь, у которого не было `NativeVideoSource`. Исправлено: добавлен `oneshot` канал — при `mft_path_failed` encoder thread создаёт новый Native трек и отправляет его в async контекст, который делает `unpublish` + `publish` нового трека.

**Известная проблема: `NativeEncodedVideoSource::push_frame` — no-op stub**

`push_frame()` (Phase 1 stub) не передаёт H.264 данные в WebRTC. Phases 1.3 (C++ glue: `rtc_create_encoded_video_source()`) и 1.4 (FFI bindings) отложены. Даже при успешном MFT кодировании, зритель не получит видео.

- **Следствие:** `auto` режим переключён на CPU путь (`EncodePath::Cpu`) до реализации push_frame.
- **Для тестирования MFT encoder:** `$env:ASTRIX_SCREEN_CAPTURE_PATH="mft"` — проверяет GPU encode, но зрителю видео не доставляется.

**Исправлено: `VideoProcessorBlt` E_INVALIDARG (0x80070057)**

- **Причина:** NV12 output texture создавалась с `D3D11_BIND_RENDER_TARGET`, который не поддерживается для NV12 на NVIDIA. Также `D3D11_RESOURCE_MISC_SHARED` и потенциально наследуемые от WGC `MiscFlags` (KEYED_MUTEX) на pool текстурах.
- **Исправление:**
  1. NV12 texture: `D3D11_BIND_RENDER_TARGET` → `D3D11_BIND_DECODER` (0x200), fallback на BindFlags=0
  2. `D3D11_RESOURCE_MISC_SHARED` убран (не нужен для same-device)
  3. Pool texture `MiscFlags` явно обнулён (не наследуется от WGC frame)
  4. Добавлено пошаговое логирование в CS fallback для диагностики

---

## Подготовка

Перед валидацией убедитесь:
- Astrix собран: `cargo build --release -p astrix-client`
- LiveKit room доступен (для 6.1, 6.4)
- Второй участник для проверки картинки (6.1)

---

## 6.1 H.264 поток в LiveKit

**Цель:** Убедиться, что H.264 от MFT декодируется корректно, другой участник видит картинку без артефактов.

**Шаги:**
1. Запустить Astrix с MFT путём (по умолчанию `auto`):
   ```
   cargo run --release -p astrix-client
   ```
2. Подключиться к room, включить screen share (720p60 или 1080p60).
3. На втором устройстве/браузере подключиться к тому же room.
4. Проверить: экран отображается без артефактов, зелёных полос, размытия, лагов.

**Ожидаемый результат:** Чистая картинка, сопоставимая с CPU путём.

**Логи:** При старте screen share должно быть:
   ```
   [voice][screen] encode path: MFT (ASTRIX_SCREEN_CAPTURE_PATH=mft or auto)
   Screen capture: MFT GPU (NVIDIA GeForce RTX ..., hardware encoder)
   ```

---

## 6.2 CPU load

**Цель:** Сравнить нагрузку на CPU при MFT vs OpenH264.

**Шаги:**
1. Запустить Task Manager → вкладка «Производительность» → CPU.
2. **CPU путь:** `$env:ASTRIX_SCREEN_CAPTURE_PATH="cpu"`; запустить Astrix, включить screen share 1080p60.
3. Записать средний CPU % (например, 25–40% на 8-ядерном).
4. **MFT путь:** `$env:ASTRIX_SCREEN_CAPTURE_PATH="mft"` (или `auto`); перезапустить, тот же preset.
5. Записать CPU % (ожидается ниже, т.к. H.264 на GPU).

**Ожидаемый результат:** MFT путь даёт заметно меньшую нагрузку на CPU при 1080p60/1440p60.

---

## 6.3 GPU load

**Цель:** Убедиться, что Video Encode используется на GPU.

**Шаги:**
1. Task Manager → «Производительность» → GPU.
2. Включить «Видео Encode» в списке движков (если скрыт — «Просмотр» → добавить).
3. Запустить screen share с MFT путём.
4. Наблюдать: «Video Encode» должен показывать активность (не 0%).

**Ожидаемый результат:** Video Encode > 0% во время screen share.

---

## 6.4 Latency

**Цель:** Проверить задержку (субъективно и через stats).

**Шаги:**
1. Screen share с MFT.
2. Двигать окно/курсор — второй участник должен видеть с минимальной задержкой.
3. В Astrix: окно статистики (если есть) — проверить `latency_rtt_ms`, `stream_fps`.

**Ожидаемый результат:** Задержка сопоставима с CPU путём или меньше.

---

## 6.5 Стресс-тест (30 минут)

**Цель:** Проверить утечки памяти и текстур.

**Шаги:**
1. Запустить screen share с MFT, оставить на 30 минут.
2. Task Manager → «Процессы» → память Astrix.
3. До и после: записать потребление памяти.

**Ожидаемый результат:** Память стабильна или растёт незначительно (без утечек).

---

## 6.6 Fallback: CPU путь

**Цель:** Убедиться, что `ASTRIX_SCREEN_CAPTURE_PATH=cpu` работает.

**Шаги:**
1. `$env:ASTRIX_SCREEN_CAPTURE_PATH="cpu"`
2. Запустить Astrix, включить screen share.
3. В логах: `[voice][screen] encode path: CPU/OpenH264 (ASTRIX_SCREEN_CAPTURE_PATH=cpu)`
4. Проверить: картинка идёт, SessionStats показывает `OpenH264 (N threads, GPU capture)`.

**Ожидаемый результат:** Старый CPU путь работает без регрессий.

---

## 6.7 iGPU + dGPU: выбор дискретной

**Цель:** На системе с двумя GPU должна выбираться дискретная.

**Шаги:**
1. Запустить `cargo run --example gpu_device_select`.
2. Проверить: выбран адаптер с наибольшим `DedicatedVideoMemory` (дискретная).
3. Запустить screen share, в SessionStats: `MFT GPU (NVIDIA GeForce ..., hardware)`.
4. Опционально: `$env:ASTRIX_GPU_ADAPTER="1"` — принудительно другой адаптер для отладки.

**Ожидаемый результат:** Выбрана дискретная GPU, имя в stats совпадает.

---

## 6.8 Только iGPU

**Цель:** На системе только с интегрированной GPU — MFT через iGPU.

**Шаги:**
1. Запустить на машине без дискретной GPU.
2. `cargo run --example gpu_device_select` — должен быть один адаптер (Intel/AMD iGPU).
3. Screen share — MFT hardware или software в зависимости от драйверов.

**Ожидаемый результат:** Screen share работает через iGPU MFT.

---

## 6.9 Виртуалка (нет hardware MFT)

**Цель:** На VM без hardware MFT должен активироваться software MFT fallback.

**Шаги:**
1. Запустить в виртуалке (VMware, Hyper-V, etc.).
2. `cargo run --example mft_enum` — обычно только software MFT.
3. Screen share — должен использоваться software MFT или fallback на OpenH264.
4. В логах: `MFT software` или `OpenH264` при fallback.

**Ожидаемый результат:** Fallback срабатывает, screen share работает.

---

## 6.10 Изменение разрешения (resize)

**Цель:** Проверить `D3d11BgraToNv12::resize()` и пересоздание MftH264Encoder при смене preset.

**Шаги:**
1. Запустить screen share 720p60.
2. Сменить preset на 1080p60 (или 1440p60) через UI.
3. Проверить: картинка переключается без падений, артефактов.

**Ожидаемый результат:** Resize работает, encoder пересоздаётся при смене разрешения.

---

## Быстрая проверка окружения

Перед полной валидацией запустите (из `client/`):

```powershell
# Комплексная проверка (GPU, MFT, path selection)
cargo run --example validate_mft_path

# Или через скрипт:
.\scripts\validate-phase6.ps1
```

Ожидаемый вывод:
```
--- GPU Adapters ---
  [N] NVIDIA GeForce RTX ... | XXXX MB VRAM | discrete
  ...
Selected: NVIDIA GeForce RTX ... (idx=N, discrete=true)

--- MFT H.264 Encoders (NV12→H.264) ---
  Hardware: 3  → MFT hardware path available
  Software: 1  → MFT software fallback available

--- Quick Init Check ---
  D3d11BgraToNv12::new ... OK
  MftH264Encoder::new (hardware) ... OK (NVIDIA H.264 Encoder MFT hardware)
  OK: D3d11BgraToNv12 + MftH264Encoder (NVIDIA H.264 Encoder MFT hardware)
```

---

## Результаты (заполнить после проверки)

| # | Пункт | Статус | Примечания |
|---|-------|--------|------------|
| авто | `validate_mft_path` | ✅ | NVIDIA H.264 Encoder MFT hardware |
| 6.1 | H.264 в LiveKit | ☐ | |
| 6.2 | CPU load | ☐ | |
| 6.3 | GPU load | ☐ | |
| 6.4 | Latency | ☐ | |
| 6.5 | Стресс 30 мин | ☐ | |
| 6.6 | Fallback CPU | ☐ | |
| 6.7 | iGPU+dGPU | ☐ | |
| 6.8 | Только iGPU | ☐ | |
| 6.9 | Виртуалка | ☐ | |
| 6.10 | Resize | ☐ | |
