## Astrix

**Astrix** — экспериментальное Discord‑подобное приложение с e2ee‑шифрованием:

- **Клиент A**: настольное приложение под Windows на Rust.
- **Сервер B**: высокопроизводительный backend в Docker с HTTP API и WebSocket‑шлюзом.

### Структура проекта

- `server/` — серверная часть (Auth, Users, Servers, Channels, Messages, WebSocket‑хаб).
- `client/` — настольный Rust‑клиент.
- `docker-compose.yml` — инфраструктура (Postgres, Redis, сервер B).

### Быстрый старт (план)

1. Собрать и запустить сервер B через Docker/Docker Compose.
2. Собрать и запустить клиент A (Rust).
3. Авторизоваться, создать сервер/канал и обмениваться e2ee‑сообщениями.

