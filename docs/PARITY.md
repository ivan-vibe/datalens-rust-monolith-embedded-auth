# Chart payload parity

Цель проекта: **серверная часть монолита отвечает ровно так же, как Python/Node DataLens**.
Проверка — дифференциальная: один и тот же HAR-файл (запросы реального дашборда) проигрывается
в браузерном UI, запросы `/api/run` из HAR отправляются в два кластера, ответы сравниваются
побайтово (с allowlist для полей времени/латентности/размера).

## Стенд

1. Поднять эталонный Python-кластер DataLens нужной версии (release v2.9.0) + наш кластер:
   `./deploy/cluster.sh up`.
2. Снять HAR с эталонного UI (браузер → DevTools → Network → Save all as HAR), открыв
   демо-дашборд и походив по чартам.
3. Запустить сравнение:

```bash
python3 scripts/parity.py \
  --har path/to/capture.har \
  --native-run 127.0.0.1:8099 \
  --python-run 127.0.0.1:28180 \
  --native-auth 127.0.0.1:8097 \
  --python-auth 127.0.0.1:28180
```

Ожидаемый результат: `57/57 entries match`. Скрипт печатает unified diff для расхождений
(сравниваются `sources`, `extra`, ошибки и HTTP-статусы).

## Что уже совпадает

* все типы демо-чартов (line/bar/area/scatter/pie/donut/tree/table/pivot/geo);
* drill-down, фильтры, параметры, `usedParams`, `updates` (локальные поля чарта);
* ошибки data-api: `ERR.DS_API.DB.ZERO_DIVISION` → HTTP 427, envelope `{"error":...}`;
* control-api: чтение датасетов/соединений/валидация формул (см. ниже);
* auth: `/signin`, `/refresh`, профиль, cookies — побайтово (см. `README.md` и `docs/ARCHITECTURE.md`).

## Формулы и редактор датасетов

Валидация `POST /api/v1/datasets/{id}/versions/draft/validators/schema` отдаёт тот же
`dataset.result_schema` с типами и `component_errors`, что Python; `add_field`/`update_field`
в визарде и редакторе датасета работают без пересоздания чарта. Проверено на +, -, *, /,
скобках, унарном минусе, `ROUND`, `ABS`, `IF`, `POWER`, `TOTAL` и ошибках формул
(unknown field / unknown function).
