# libwebrtc для Astrix (Phase 2)

Копия libwebrtc из livekit/rust-sdks webrtc-sys с патчем H.264 multithreading.

## Патчи

- `patches/h264_multithread_windows.patch` — многопоточность OpenH264 на Windows.
- `patches/h264_decoder_multithread_windows.patch` — многопоточность H.264 декодера.
- `patches/h264_encoder_params.patch` — keyframe interval 2 с, max NAL size 1200 байт (снижает stalls на IDR).

Совместим с webrtc-sdk m137_release (см. .gclient). Если `git apply` выдаёт ошибку — возможно, изменилась версия WebRTC; патч нужно адаптировать.

## Сборка

```powershell
cd client
.\scripts\build-webrtc-windows.ps1
```

Требуется: Python 3, Visual Studio 2022, ~20 GB свободного места. Первый запуск: 30–60 мин.

## После сборки

1. Раскомментировать `LK_CUSTOM_WEBRTC` в `client/.cargo/config.toml`
2. `cargo build`
