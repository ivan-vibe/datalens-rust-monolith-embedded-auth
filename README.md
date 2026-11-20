# DataLens Rust Monolith — single-binary fork (embedded auth)

Это **форк** основного репозитория
[datalens-rust-monolith](https://github.com/ivan-vibe/datalens-rust-monolith):
код авторизации `datalens-auth-rs` собран **в тот же бинарник**, что и монолит.
В deployment нет отдельного auth-контейнера, процесса и бинарника — только один
`monolith`.

* **один бинарник** (~18 MB, без V8/Node/Python на сервере);
* заменяет `us` + `data-api` + `ui-api` (charts engine) + `control-api` +
  `meta-manager` + `auth`;
* база эквивалентности — **DataLens v2.9.0** (`ui 0.3831.0`, `us 1.39.0`,
  backend `0.2457.0`, auth `0.27.0`);
* chart payload parity с оригиналом — **57/57** запросов реального дашборда
  (см. [docs/PARITY.md](docs/PARITY.md));
* **коннекторы: только PostgreSQL и ClickHouse.** Остальные не перенесены.
* подробности отличий и компромиссов — [FORK.md](FORK.md).

## Что внутри

```
src/                     монолит: US + data-api + ui-api + control-api + meta-manager
auth/                    datalens-auth-rs: login/JWT/users; в форке вшит в монолит
deploy/                  docker-compose (postgres + monolith + ui), Dockerfile, seed БД
docs/                    архитектура, parity, память, auth, аудит
scripts/                 сборка и дифф-тест против оригинального стека
```

## Быстрый старт (Docker)

Нужен Docker + docker compose v2, ~4 GB RAM, порты `8091`, `8099`.

```bash
git clone https://github.com/ivan-vibe/datalens-rust-monolith-embedded-auth.git
cd datalens-rust-monolith-embedded-auth
docker compose up -d --build      # или ./deploy/cluster.sh up
```

Один проект, три сервиса: `postgres` (официальный образ DataLens), `monolith` и `ui`.
**Auth живёт внутри монолита** на внутреннем `:8097` — отдельного контейнера нет.

Первый запуск:

1. официальный `ghcr.io/datalens-tech/datalens-postgres:16-1.3.0` создаёт базы `pg-*`;
2. монолит накатывает US-миграции **и сам создаёт схему `pg-auth-db` с первым
   пользователем** `admin/admin` (`AUTH_ADMIN_LOGIN` / `AUTH_ADMIN_PASSWORD`);
3. монолит сам заливает официальные демо-данные (они встроены в бинарник) при первом
   старте, если в `pg-us-db` ещё нет воркбуков — отдельного seed-контейнера нет;
4. поднимается официальный `datalens-ui`.

| URL | Что |
|---|---|
| `http://localhost:8091` | UI |
| `http://localhost:8099/ping` | API монолита |

Логин: **admin / admin**.

### Готовые бинарники

В git не хранятся: `cargo build --release` → один бинарник (~18 MB), либо
[GitHub Releases](../../releases) — `linux-x86_64`, `windows-x86_64`,
`macos-x86_64`, `macos-arm64`. Каждый бинарник — весь сервер: US, data-api,
charts engine, control-api, meta-manager **и auth**.

### Ключи по умолчанию

В `deploy/docker-compose.yml` стоят публичные демо-ключи открытого DataLens (пара PS256,
`US_MASTER_TOKEN=us-master-token`, `AUTH_MASTER_TOKEN=auth-master-token`, fernet-ключ).
Это не секреты — они опубликованы upstream. Для реального стенда переопределите их
окружением; `./deploy/bootstrap.sh` умеет сгенерировать свои в `deploy/secrets/`.

## Сборка без Docker

MSRV: Rust **1.85+**. В форке достаточно одного бинарника:

```bash
cargo build --release      # → target/release/datalens-monolith (auth внутри)
```

Standalone-бинарник auth (`cd auth && cargo build --release`) остаётся только для
отладки/разработки крейта; в deployment он не нужен.

## Конфигурация auth (внутри монолита)

| переменная | значение |
|---|---|
| `AUTH_EMBEDDED` | `1` — auth внутри монолита (default в форке); `0` — отключить |
| `AUTH_BIND` | адрес listener'а auth (default `127.0.0.1`; compose — `0.0.0.0` внутри контейнера) |
| `AUTH_PORT` | порт auth (default `8097`) |
| `TOKEN_PRIVATE_KEY`, `TOKEN_PUBLIC_KEY` | PS256-ключи (PEM) |
| `AUTH_POSTGRES_DSN` | DSN `pg-auth-db` |
| `AUTH__JWT_KEY`, `AUTH_TOKEN_PUBLIC_KEY` | публичный ключ для проверки JWT |

Остальные переменные — как в основном репозитории: `POSTGRES_DSN_LIST`,
`META_MANAGER_PG_DSN`, `DL_CONNECTION_HOST_MAP`, `US_MASTER_TOKEN` и т.д.
Полный набор — в `deploy/docker-compose.yml`.

## Потоковая отдача и производительность

Пачку чартов можно получить двумя способами: `POST /api/run/batch` — один JSON со всеми
ответами, или `POST /api/run/batch/stream` — NDJSON, строка на чарт сразу после готовности.
Разница в TTFB, памяти и отмене при обрыве клиента, а не в пропускной способности; ручки
(`DL_BATCH_CONCURRENCY`, `DL_STREAM_ITEM_TIMEOUT_SECS`, `DL_RUN_TIMEOUT_SECS`,
`DL_ZSTD_STREAM_LEVEL`, `DL_PG_*`) и замеры — в [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md).

## Чего честно ждать

* ранняя версия: parity покрывает демо-дашборд и основной control-api, не все
  экзотические сценарии; редкие формулы/ошибки могут расходиться;
* single-node, без extracts и части RLS-сценариев;
* нет UI-патча для Gravity-чарта (в консоли может быть
  `It seems you are trying to get non-existing category value` — баг самого UI);
* демо-учётка admin/admin; мастер-токены генерируются `deploy/bootstrap.sh`;
* **минус форка:** auth перезапускается вместе с монолитом, нельзя масштабировать
  auth отдельно; ключи подписи живут в адресном пространстве чартового движка
  (порт наружу не публикуется). Для отдельного auth-сервиса — основной репозиторий.

## Лицензия

Apache License 2.0 — [LICENSE](LICENSE), производная от DataLens (см. [NOTICE](NOTICE)).

**Дисклеймер:** это неофициальный проект, не связанный с DataLens, его мейнтейнерами или
Яндексом и не одобренный ими. DataLens и связанные логотипы — товарные знаки
соответствующих правообладателей.

