# Astrix: перенос на VDS

Текущий VDS IP: `193.233.251.173`

## Что уже перенастроено в проекте

- Клиент по умолчанию использует API: `http://193.233.251.173:8080`
- Клиент теперь читает `api_base` из `astrix_settings.json`, поэтому при следующем переносе можно менять адрес без пересборки
- `docker-compose.yml` теперь публикует backend наружу на `8080:8080`
- LiveKit в `docker-compose.yml` уже настроен на `ws://193.233.251.173:7880`

## Что нужно сделать на VDS

1. Установить Docker и Docker Compose plugin
2. Открыть порты в firewall/security group:
   - `8080/tcp` для backend API
   - `7880/tcp` для LiveKit WebSocket
   - `7881/tcp` для LiveKit TCP fallback
   - `50000-50100/udp` для media traffic
3. Скопировать проект на сервер
4. В `docker-compose.yml` заменить секреты:
   - `POSTGRES_PASSWORD`
   - `JWT_SECRET`
   - `LIVEKIT_API_KEY`
   - `LIVEKIT_API_SECRET`
5. Убедиться, что ключи в `livekit.yaml` совпадают с `LIVEKIT_API_KEY` и `LIVEKIT_API_SECRET`
6. Запустить:

```powershell
docker compose up -d --build
```

7. Проверить:

```powershell
docker compose ps
docker compose logs server --tail=100
docker compose logs livekit --tail=100
```

## Клиент

Клиент можно перенастроить без пересборки через `client/astrix_settings.json`:

```json
{
  "api_base": "http://193.233.251.173:8080"
}
```

Если поля `api_base` нет, клиент возьмёт адрес по умолчанию из исходников.

## Важно

- Сейчас схема временная: `HTTP + WS` по IP без домена и SSL
- Для production лучше перевести на домен, `HTTPS`, `WSS` и reverse proxy
- Так как в конфигах уже присутствуют реальные ключи, их лучше сгенерировать заново перед боевым запуском
