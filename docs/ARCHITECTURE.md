# Архитектура

```
browser ──HTTP/2/TLS──▶ datalens-ui (nginx :8443 + node :3030) ──HTTP──▶ monolith :8099
                              │                                          │
                              ├── /api/run(-batch) ───────────────────────┤
                              ├── /api/v1/* (control-api) ────────────────┤
                              ├── /api/data/v2/* (data-api) ──────────────┤
                              ├── /us /private/* (united storage) ────────┤
                              └── /meta/* (meta-manager) ─────────────────┤
                                                                         │
auth :8097 (внутри процесса монолита) ◀── /signin, JWT (PS256) ── browser ──┘
                                                                         │
                                                          PostgreSQL 16 (pg-* databases)
```

## Один процесс вместо пяти сервисов

Оригинальный DataLens запускает `us`, `data-api`, `ui-api`, `control-api` и `meta-manager`
как отдельные Python/Node-процессы и общается между ними по HTTP. В монолите:

* те же HTTP-контракты наружу, но внутренние вызовы `us ↔ data-api ↔ ui-api ↔ control-api`
  идут через **in-process** маршруты (`local_http`, `local_us`) — без сети, сериализации
  и повторной валидации JWT;
* United Storage (knex-миграции Python) заменён Rust-портом `us-rs` (`vendor/us-rust`),
  читающим те же таблицы `pg-us-db` (revisions/entries/workbooks и т.д.);
* data-api — Rust-порт `vendor/data-api` (compiler + planner: SQL для PostgreSQL/ClickHouse/
  MySQL, сериализация ответов, preflight-валидация датасетов);
* charts-engine (`ui-api`) портирован в `src/pipeline.rs` (+ `gravity_*`, `d3_*`, `pivot_engine`,
  `graph_engine`, `geo_engine`): chart config → data-api запросы → сборка payload'а, включая
  Gravity/D3-форматы, таблицы, пивоты, гео;
* control-api (датасеты/соединения/валидаторы) — `src/control_api.rs`, meta-manager —
  `src/meta_manager.rs` + `meta_api.rs`, US-фасад — `src/us_direct.rs`/`us_main.rs`;
* auth — крейт `auth/`, подключённый как **library** и запущенный в том же процессе
  (внутренний listener `AUTH_BIND:AUTH_PORT`); в основном репозитории он вынесен в
  отдельный сервис, здесь — вшит в бинарник ради single-binary deployment.

## Поток рендера чарта

1. UI отправляет `/api/run` (запрос содержит chart id, параметры, локальные `updates`).
2. Монолит читает запись чарта из US (`us_direct`, revision-validated кэш), строит extra
   (поля, форматирование, палитра; кэшируется по ревизии).
3. Пайплайн собирает запросы к data-api для каждого источника (dataset/QL/combined),
   выполняет их через `data_direct` (in-process) и собирает финальный payload.
4. Ответ сжимается (`zstd`/`gzip`) и отдаётся клиенту; ошибки data-api заворачиваются в
   контракт UI (`427` + `{"error":{code,details:{sources...}}}`).

## Кэши и память

* `US entry cache` — parsed entry + typed dataset, TTL 60 s, ревизия проверяется на каждом
  чтении (свежесть важнее TTL);
* render-кэши: `extra` (поля/формат/палитра), parsed `shared`, table request — с лимитами
  и TTL (env);
* fields/tables кэшируются на уровне data-api; пул соединений `DL_DB_POOL_SIZE`;
* allocator: mimalloc (систем на macOS) + `MIMALLOC_PURGE_DELAY` для возврата страниц.
  Замеры и профиль — [MEMORY.md](MEMORY.md).

## Ключевые инварианты parity

* JSON-числа сериализуются как JS (int/float различаются), порядок ключей и полей сохраняется;
* SQLSTATE и тексты ошибок data-api пере‑маппятся в коды/сообщения Python (`ERR.DS_API.*`);
* `updates` (add/update/delete field) применяются к датасету до компиляции запроса;
  formula-поля без типа (локальные, ещё не сохранённые) компилируются inference-проходом;
* API-форма и storage-форма датасета различаются (`left/right/type` ↔
  `left_part/right_part/condition_type`) — конвертация выполняется на границе.
