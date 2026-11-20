# Тесты: методология и набор

Документ фиксирует, **как** мы проверяли починку редактора датасетов (вторая таблица, связь,
refresh схемы) и **какие тесты** теперь лежат в репозитории. Он же — инструкция, чтобы
повторить прогон с нуля.

## Принцип: тест из реального клиента, а не из головы

Баг проявлялся только в связке «UI → gateway → control-api → data-api», поэтому тесты
строились так:

1. **Снять реальный запрос.** Открыть редактор в headless Chrome, перетащить первую и вторую
   таблицу, сохранить точные тела запросов (`dataset` + `updates`) и ответы.
2. **Зафиксировать payload как фикстуру.** `scripts/fixtures/dataset_editor/drop2-request.json`
   — это настоящий запрос второго drop'а (`add_source`, `add_source_avatar`,
   `add_avatar_relation`), а не синтетический.
3. **Проверять инварианты, а не только HTTP-код.** Ответ `200 OK` с потерянным источником уже
   был источником бага. Проверяются ссылки `field.avatar_id → avatar.id`,
   `avatar.source_id → source.id`, уникальность `title`, наличие relation.
4. **Зафиксировать нормальное поведение отдельным тестом.** Пока связь не настроена, preview
   честно отвечает `ERR.DS_API.REQUEST: the request touches 2 sources; no avatar relation joins
   them together` — это ожидаемо, тест это утверждает (а не считает ошибкой).
5. **Негативный и позитивный сценарий рядом.** Тот же payload: без join'а → 400 с конкретным
   сообщением; после `update_avatar_relation` → 200 и строки preview.
6. **Проверить сохранение и refresh.** `create` → `versions/draft/validators/schema` с
   `refresh_source` → `versions/draft/preview`; инварианты те же.

## Уровни тестов

### 1. Юнит-тесты (Rust, без БД)

`cargo test --offline --bin datalens-monolith` (в крейтете нет `lib`, поэтому именно `--bin`).
Сейчас: **152 passed**. Новые тесты в `src/control_schemas.rs`:

| тест | что фиксирует |
|---|---|
| `duplicate_source_updates_are_idempotent` | повторное применение `add_source`/`add_source_avatar` не плодит дубли |
| `reconcile_drops_orphan_avatars_and_their_direct_fields` | аватар без источника и его direct-поля удаляются, formula-поля остаются |
| `reconcile_keeps_a_single_id_source_and_its_fields` | legacy-форма, где `avatar.id == source.id`, чинится, а не выкидывается |
| `reconcile_creates_an_avatar_for_a_source_without_one` | источник без аватара получает аватар, поля по `source_id` выживают |
| `reconcile_drops_dangling_and_duplicate_relations` | связи с несуществующим концом и дубли удаляются |
| `relations_from_updates_are_upserted` | `add_avatar_relation` + `update_avatar_relation` дают одну связь, update применяется |
| `refresh_source_ids_are_collected_once` | `refresh_source` собирается по id без дублей |

Плюс существующие тесты control-api (формы payload'ов, stored/editor-преобразования) — они
ловят регрессии в сериализации.

### 2. Протокольный replay (Python, живой монолит)

`scripts/dataset_editor_smoke.py` — основной регресс на баг. Что делает:

1. логинится в auth (`admin/admin`) и берёт access token из cookie;
2. шлёт фикстуру второго drop'а в `POST /api/v1/datasets/validators/dataset`;
3. проверяет инварианты ответа: 2 источника, 2 аватара, 1 связь, 23 поля, все ссылки валидны,
   `title` уникальны;
4. **утверждает нормальный 400** на preview без join'а и текст про relation;
5. настраивает join по общей колонке (`category_name_level_1`) через
   `update_avatar_relation` и получает **preview 200** со строками;
6. создаёт датасет, гоняет `refresh_source` на сохранённом (versioned validator), проверяет
   инварианты и versioned preview, затем удаляет датасет.

Запуск (нужен поднятый стек и демо-данные):

```bash
make smoke-dataset
# или
python3 scripts/dataset_editor_smoke.py --base-url http://127.0.0.1:8099 \
    --auth-url http://127.0.0.1:8097 --login admin --password admin
```

Ожидаемый хвост:

```
  ok second drop: sources=2 avatars=2 relations=1 fields=23
  ok preview before join: HTTP 400 'no avatar relation joins them together' (expected)
  ok join update: ...
  ok preview after join: HTTP 200, 10 rows
  ok refresh_source: schema re-read, content consistent
  ok saved dataset preview: HTTP 200
PASS dataset editor smoke
```

### 3. Браузерный E2E (Node + Chrome DevTools Protocol)

`scripts/ui_dataset_editor_e2e.mjs` повторяет то, что делает человек:

- открывает `/workbooks/<id>/datasets/new` (при необходимости логинится);
- нажимает `+ Add`, выбирает соединение `pg`;
- синтетическими HTML5 drag-событиями (react-dnd) тащит `public.orders` на плейсхолдер, затем
  `public.managers` на `.relations-map`;
- проверяет, что на канвасе два аватара, все `validateDataset` = 200, в последнем ответе
  2 источника / 2 аватара / 1 связь и валидные ссылки, а `getPreview` до join'а = 400;
- сохраняет скриншот и `report.json` (сеть + консоль).

Запуск (нужен Chrome с `--remote-debugging-port=9224`):

```bash
make e2e-ui
# или
node scripts/ui_dataset_editor_e2e.mjs --out /tmp/e2e --keep-page
```

Вспомогательный драйвер: `scripts/cdp.mjs` (без зависимостей, Node 22, глобальный
`WebSocket`). Умеет `--url`, `--eval`, `--wait`, `--shot`, `--net`, `--console`, `--mouse`,
`--drag`, `--click-text`; используется и как CLI для ручной отладки.

### 4. Живой смоук дашборда и сборки

После каждого рестарта/сборки:

```bash
curl -s http://127.0.0.1:8099/ping
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:8091/
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:8092/
```

Дашборд (16+ запросов `/api/run` должны быть 200, процесс не падает):

```bash
node scripts/cdp.mjs --url http://localhost:8091/qvnkqzm0wstyf-datalens-demo \
    --wait 8000 --net | grep -c '^200 POST .*api/run'
```

Этот смоук поймал отдельный дефект: быстрая сборка без fat LTO падала по `stack overflow` на
чартовом пути. Лечится `TOKIO_STACK_MB` (по умолчанию 16 МиБ) в `src/main.rs`; тест — «после
загрузки дашборда процесс отвечает на ping».

## Нормальное поведение, которое не считается багом

- После drop'а второй таблицы редактор создаёт связь **с пустыми `conditions`**. Пока поля
  связи не выбраны, preview отвечает:
  `ERR.DS_API.REQUEST — the request touches 2 sources; no avatar relation joins them together`.
  Это защита data-api, а не регрессия.
- Связь настраивается в UI (клик по линии/диалог связи) и приходит как
  `add_avatar_relation`/`update_avatar_relation`; сервер теперь их принимает.
- `refresh_source` перечитывает схему из коннектора: если в таблице появилась/исчезла колонка,
  поля обновятся/удалятся; пользовательские `cast` и переименования сохраняются.

## Фикстуры

`scripts/fixtures/dataset_editor/drop2-request.json` — тело запроса UI-шлюза
(`{datasetId, workbookId, data:{dataset, updates}}`), снятое с демо-стенда. Внутри —
connection id демо-подключения `gz2tnvb9zdxm5` и таблицы `public.orders` / `public.managers`.
Если демо-данные пересозданы с другими id, фикстуру нужно переснять (см. «Как снять payload»
ниже); тест упадёт с понятным сообщением про отсутствующий источник/схему.

## Как снять новый payload (если UI поменялся)

1. Chrome: `--remote-debugging-port=9224`.
2. `node scripts/cdp.mjs --url http://localhost:8091/workbooks/<id>/datasets/new --wait 4000`.
3. Повторить действие руками/скриптом и снять сеть:
   `node scripts/cdp.mjs --dump-net /tmp/net.json` (CLI-версия) или добавить хук на `fetch`/XHR, как в
   `ui_dataset_editor_e2e.mjs`.
4. Взять тело `validateDataset` из лога, положить в фикстуру, прогнать
   `scripts/dataset_editor_smoke.py` и посмотреть, какие инварианты ломаются.

## Быстрая сборка для итераций

```bash
/Users/ivan/datalens2026/.native-target/quick-build.sh
```

Профиль: release без fat LTO (`opt-level=3`, `codegen-units=16`, `incremental`), полная сборка
~60 с, инкремент после правки одного файла ~3–7 с. Бинарник:
`.native-target/quick/release/datalens-monolith`. `target/release/datalens-monolith` (fat LTO)
не трогаем при итерациях; для продакшена его нужно пересобрать отдельно.

## Ограничения

- Браузерный тест завязан на классы UI (`avatar__avatar-title`, `.relations-map`) и координаты
  drag'а; при обновлении UI его нужно править. Протокольный replay от UI не зависит.
- Тесты требуют живого Postgres с демо-данными; это интеграционные, не unit-тесты.
- CDP-тест не входит в `cargo test` и не запускается в CI без браузера; запускается локально
  как `make e2e-ui`.
