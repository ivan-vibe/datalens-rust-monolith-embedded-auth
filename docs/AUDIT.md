# Аудит кода: результаты и исправления

Первичный аудит (clippy + ручной просмотр hot-path'ов + дифференциальные тесты) нашёл
дефекты; все пункты ниже исправлены в этом репозитории и проверены. Оставшиеся пункты —
архитектурные (размер модулей) и стилевые clippy-предупреждения, не влияющие на поведение.

## Безопасность / конфигурация — исправлено

1. **`AUTH_ENABLED` теперь fail-closed.** Не задан → считается включённым. В
   `MONOLITH`-профиле старт дополнительно проверяет конфигурацию и **падает с exit 2**,
   если auth включён, а `AUTH__TYPE!=NATIVE`, `US_MASTER_TOKEN` или `AUTH__JWT_KEY`
   пустые. Явный dev-режим — только `AUTH_ENABLED=0`.
2. **Debug-эндпоинты fail-closed.** `require_debug_admin` (`src/data_direct.rs`) при
   отсутствии in-process auth состояния отвечает `503`, если `AUTH_ENABLED` не выключен
   явно. Раньше незаданный `AUTH_ENABLED` открывал `/debug/*`.
3. **Убраны дефолтные мастер-токены.** Fallback `"us-master-token"` из
   `control_api.rs`, `engine_main.rs`, `ui_api.rs`, `meta_api.rs`, `meta_manager.rs`
   заменён на пустое значение; при включённом auth старт требует явный токен.

## Функциональные — исправлено

4. **`MOD(...)`: текст ошибки** теперь `Unknown 2-argument function or operator MOD`
   (арность считается по исходной формуле, а не по переведённому выражению) — совпадает с
   Python, включая код `ERR.DS_API.FORMULA.TRANSLATION.UNKNOWN_FUNCTION`.
5. **Валидатор формул больше не инвалидирует весь пакет `updates`.** На ошибочном пути
   каждый патч валидируется отдельно; в ответе `valid=false`/`string`-fallback получает
   только проблемное поле, здоровые поля сохраняют свои типы (совпадает с Python).
6. **`CONCAT(...)` и другие string/boolean-формулы** классифицируются как `DIMENSION`,
   а формулы со ссылками на агрегированные поля (например pivot-поле `abs-retention` со
   строковым результатом) — как `MEASURE`. Реализован рекурсивный анализ дерева формулы
   (`formula_has_aggregate`) вместо доверия полю `type` из UI.
7. **Граница API/storage-формы датасета** закреплена: API отдаёт `left/right/type`,
   `stored_dataset_data` конвертирует в US-storage `left_part/right_part/condition_type`
   и обратно. Проверено: после native-save Python-сервис читает датасет, chart parity 57/57.

## Мёртвый код / качество — исправлено

8. Удалён неиспользуемый `dataset_schema` в `pipeline.rs`.
9. Упрощены одинаковые ветки в `graph_engine.rs` (`date_axis_value`, `format_category`).
10. Убраны неиспользуемые переменные в `gravity_tree.rs` / `gravity_scatter.rs`.
11. `vendor/us-rust/src/logging.rs`: ошибка отправки в лог-канал больше не игнорируется
    (пишется предупреждение в stderr).
12. Применён `cargo clippy --fix` для machine-applicable предупреждений; оставшиеся —
    стилевые (`too_many_arguments`, `type_complexity` и т.п.), не дефекты.

## Проверка после исправлений

* `cargo test --release` — **136 passed**;
* chart payload parity против DataLens v2.9.0 — **57/57**;
* control-api parity — 15/16 (известный unordered `replacement_types`), auth parity — OK,
  editor/control-plane smoke — OK;
* дифференциальные кейсы редактора формул: `MOD`, `CONCAT`, unknown field, batch с одним
  битым полем, string-формула с агрегатом, редактирование существующего поля — ответы
  native и Python совпадают (статус, код, `data_type`, `type`, `valid`, `component_errors`);
* fail-closed старт: `MONOLITH=1` без auth-конфигурации → exit 2 с сообщением
  `fatal: AUTH_ENABLED is on but AUTH__TYPE=…`.

## Осталось (не дефекты)

* `src/pipeline.rs` (~8k строк) и `src/pivot_engine.rs` (~5k) — большие модули, кандидаты
  на разбиение по доменам; поведение стабилизировано тестами parity.
* Часть clippy-предупреждений стилевые; CI их не блокирует.

## Второй проход: авторизация (2026-11-16) — исправлено

Найдены более серьёзные дыры, чем в первом аудите:

1. **Обход прав записи в control-api.** `require_user` проверял только *наличие* заголовка
   `Authorization`/`Cookie`, а записи в US шли с service/master-токеном. Запрос с
   `Authorization: Bearer garbage` мог создать/изменить/удалить датасет или соединение.
   Исправлено: JWT валидируется (auth-rs публичный ключ), а записи идут через **публичные**
   US-маршруты `/v1/entries...` с заголовками пользователя, без master-токена — US сам
   проверяет editor-права. Проверено: viewer получает `403 ACCESS_SERVICE_PERMISSION_DENIED`,
   admin создаёт/меняет/удаляет датасет, anonymous/bogus → `401`.
2. **Сервисные `/private/*` маршруты были доступны с LAN.** В монолите `/private/entries/...`
   (US master-token) и в auth-rs `/private/v1/management/users/...` (MASTER_TOKEN) отвечали
   любому хосту сети при предсказуемых токенах — включая создание пользователей и запись
   записей US. Итоговое решение: auth-rs пускает свои `/private/v1/*` только с loopback;
   US `/private/*` защищён **случайным** мастер-токеном из `deploy/bootstrap.sh` (UI-контейнер
   обязан ходить туда из своей сети — иначе не резолвятся tenant-настройки и UI отдаёт 500),
   поэтому в compose токен не предсказуем, а для публичной экспозиции его нужно ротировать.
3. **Ошибки US 401/403 превращались в 500** при записи — теперь пробрасываются с исходным
   статусом и кодом (`ACCESS_SERVICE_PERMISSION_DENIED`).
4. **Последствие проверки:** private-POST до фикса испортил demo-датасет `cohorts`; он
   восстановлен к исходной ревизии 1.39.

Остаточные риски (документированы, не исправлены кодом): демо-логин `admin/admin`; мастер-
токены в dev-env предсказуемы (private закрыт loopback'ом — для интернет-экспозиции ротировать);
нет rate-limit на `/signin`; debug-маршруты требуют admin-роль, но доступны с LAN при её
наличии; рекомендовано ставить reverse-proxy с TLS/лимитами и менять пароли.

Проверено после исправлений: chart parity 57/57, control parity 15/16, auth parity OK,
editor/control-plane smoke OK, cargo test 136; viewer-write 403; LAN `/private/*` 403;
anonymous `/api/v1/*` 401; `/api/run` без токена → 427 с вложенным 401.

## Проверка развёртывания с нуля (2026-11-16)

`./deploy/cluster.sh up` на macOS (Apple Silicon, Docker Desktop): bootstrap секретов →
сборка образа monolith → 3 контейнера healthy (postgres, monolith, ui) →
`http://localhost:8091` 200, `https://localhost:8092` HTTP/2 200, `/signin` 200,
chart parity против Python-референса **57/57**. В ходе прогона найдены и исправлены:
отсутствующий `AUTH_TOKEN_PUBLIC_KEY` у монолита (US отвечал `Unauthorized access` на
пользовательские данные после логина) и шумный healthcheck Postgres (`pg_isready` без `-d`,
FATAL каждые 5 секунд).
