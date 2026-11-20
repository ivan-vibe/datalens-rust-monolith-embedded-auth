# Single-binary fork: auth вшит в монолит

Этот форк отличается от основного репозитория только одним: **сервис авторизации
(`datalens-auth-rs`) собран в тот же бинарник, что и монолит**. Отдельного контейнера,
процесса и бинарника `datalens-auth-rs` в deployment больше нет.

## Что поменялось

| | Основной репозиторий | Этот форк |
|---|---|---|
| бинарников в runtime | 2 (`monolith`, `datalens-auth-rs`) | **1 (`monolith`)** |
| контейнеров (без Postgres/UI) | 3 (postgres, auth, monolith) | **2 (postgres, monolith)** |
| auth HTTP | отдельный процесс на `:8097` | тот же процесс, внутренний listener `AUTH_BIND:AUTH_PORT` |
| код auth | отдельный crate-бинарник | тот же crate подключён как **library** (`datalens_auth_rs`) |
| API/протокол/cookies/БД | — | **не изменились** |

Auth-код переиспользуется без копирования: `auth/src/lib.rs` экспортирует
`datalens_auth_rs::serve(bind_host, shutdown)`, а `src/main.rs` монолита запускает его в
общем tokio-рантайме:

```rust
if embedded_auth {
    runtime.spawn(datalens_auth_rs::serve(&bind, std::future::pending()));
}
```

## Настройки

Добавлены переменные (у монолита):

| переменная | смысл |
|---|---|
| `AUTH_EMBEDDED` | `1`/`true` — auth внутри монолита (в форке default `1`); `0` — старое поведение |
| `AUTH_BIND` | адрес listener'а auth (default `127.0.0.1`; в compose — `0.0.0.0`, порт не публикуется наружу) |
| `AUTH_PORT` | порт auth (default `8097`) |
| `TOKEN_PRIVATE_KEY`, `TOKEN_PUBLIC_KEY` | PS256-ключи подписи/проверки |
| `AUTH_POSTGRES_DSN` | DSN `pg-auth-db` |

`bootstrap.sh` генерирует ключи и мастер-токены и кладёт их в secrets-overlay **для сервиса
monolith** (раньше — для сервиса auth). UI, как и раньше, видит auth по `AUTH_ENDPOINT`;
в compose это `http://monolith:8097`.

## Запуск (Docker)

```bash
./deploy/cluster.sh up
# UI:     http://localhost:8091   admin / admin
# HTTP/2: https://localhost:8092
# API:    http://localhost:8099/ping
```

Контейнеров четыре: `postgres`, `monolith`, `ui` (плюс сеть) — auth-контейнера нет.
Порт `8097` у монолита наружу **не публикуется**: UI ходит к нему по внутренней сети.

## Запуск (host, как в проде)

Нужен один бинарник и env для auth внутри монолита (пример — `deploy/docker-compose.yml`,
секция `monolith`). Порядок:

```bash
cargo build --release
MONOLITH=1 PUBLIC_PORT=8099 AUTH_BIND=127.0.0.1 AUTH_PORT=8097 \
POSTGRES_DSN_LIST=... META_MANAGER_PG_DSN=... \
AUTH__TYPE=NATIVE AUTH_ENABLED=true AUTH__JWT_KEY="$(cat pub.pem)" \
AUTH_TOKEN_PUBLIC_KEY="$(cat pub.pem)" \
TOKEN_PRIVATE_KEY="$(cat priv.pem)" TOKEN_PUBLIC_KEY="$(cat pub.pem)" \
AUTH_POSTGRES_DSN=postgres://pg-user:postgres@127.0.0.1:28185/pg-auth-db \
US_MASTER_TOKEN=... AUTH_MASTER_TOKEN=... \
./target/release/datalens-monolith
```

Отдельный `auth-rs` процесс больше не запускается (если запущен — освободите `:8097` или
задайте другой `AUTH_PORT`).

## Компромиссы

* **плюс:** один бинарник/процесс, меньше контейнеров и памяти, нет сетевого хопа
  UI → auth (только UI → монолит), проще деплой;
* **минус:** auth перезапускается вместе с монолитом (сессии в БД переживают рестарт,
  JWT живут до истечения), нельзя масштабировать/обновлять auth отдельно, ключи подписи
  лежат в адресном пространстве чартового движка (но наружу порт не публикуется, а
  `/private/*` auth по-прежнему отвечает только loopback-клиентам);
* для сценариев «auth как отдельный сервис с независимым жизненным циклом/скейлом» используйте
  основной репозиторий.

## Проверено

* `cargo build --release` (macOS arm64) — OK;
* embedded auth: `POST /signin` admin/admin → 200, JWT принимается монолитом/US
  (`GET /api/v1/datasets/...` → 200);
* workbook export через встроенный auth и очередь: `POST /workbooks/export` → `success`,
  результат 1 069 648 байт;
* chart payload parity против Python-референса — **57/57** (см. `docs/PARITY.md`);
* `docker compose config` (3 сервиса, без auth) — OK;
* Docker-образ `deploy/Dockerfile.monolith` собирается (scratch, **29.5 MB**, внутри один
  `/usr/local/bin/monolith` и больше никаких auth-бинарников).

Дата: 2026-11-16. Основной репозиторий: `../datalens-rust-monolith`.
