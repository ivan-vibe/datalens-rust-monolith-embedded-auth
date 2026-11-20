# Native process memory

Measured on the current host with `vmmap -summary` (`Physical footprint`, the resident
footprint; virtual reservation is not the memory cost). The monolith was exercised through
`/api/run` with the real dashboard bodies (`bench/real/body`), so US/data-api/cache paths are
active.

| process | state | physical footprint |
|---|---|---:|
| monolith | idle | 122–124 MiB |
| monolith | real charts, 32 clients | 162–168 MiB (peak 169 MiB) |
| monolith | real charts, 64 clients | 188–202 MiB (peak 213 MiB) |
| auth-rs | idle/after signin+profile | 3.3–19.3 MiB (peak 19.3 MiB) |
| mono-ui container | loaded dashboard | 174.3 MiB (`docker stats`) |

The monolith keeps allocated memory after a burst (c64 settled at ~197 MiB); this is the
mimalloc/Rust allocator retaining capacity for the next request, not a leak. The chart path
reached ~346 RPS at 32/64 clients in these runs with 0 errors.

Reproduce: start the cluster, get an admin token from mono-ui, run
`arlab load --url http://127.0.0.1:8099/api/run --body-dir bench/real/body --connections 32
--duration 10`, and sample `vmmap -summary <pid>`.

## v265-mem profile (2026-11-14)

Триггер: cold idle уже маленький (26 MB), но тёплый процесс держал 110–147 MB. Прогнали
6+ вариантов (interleaved, по 2–3 прогона, медианы; нагрузка — реальные тела дашборда,
c18 × 20 s, `bench/rail/mem_probe.py`, сырые результаты `bench/rail/results/mem.jsonl`):

| конфиг | idle 60 s | peak c18 | RPS | p50 | p95 |
|---|---:|---:|---:|---:|---:|
| baseline (v265) | 134 MB | 170 MB | 239 | 45.1 ms | 182 ms |
| только `MIMALLOC_PURGE_DELAY=0` | 119 MB | 142 MB | 243 | 44.0 ms | 178 ms |
| только пул 8 + TTL 30 | 114–130 MB | 145 MB | 239 | 45.4 ms | 165 ms |
| только лимиты кэшей | 113 MB | 166 MB | 271 | 37.7 ms | 167 ms |
| purge + лимиты + pool 12 | 97 MB | 137 MB | 226 | 49.5 ms | 190 ms |
| **purge + soft-лимиты + pool 12 (выбран)** | **89 MB** | **141 MB** | **263** | **41.2 ms** | **156 ms** |
| агрессивный tight | 90 MB | 102 MB | 184 | 48.6 ms | 279 ms |
| без кэшей (диагностика) | 81 MB | 106 MB | 142 | 71.7 ms | 472 ms |

Выбранный профиль (`deploy/native-shared-pg.env`, env-переключатели):
`MIMALLOC_PURGE_DELAY=0`, `DL_DB_POOL_SIZE=12`, `US_ENTRY_CACHE_MAX=256` (TTL 60),
`CHART_EXTRA_CACHE_MAX=256`, `PARSED_SHARED_CACHE_MAX=128` (TTL 900), `TABLE_REQUEST_CACHE_MAX=128`.
Контрольный прогон production-профиля: cold **26.5 MB**, load peak **137.6 MB**, idle 60 s
**87.1 MB**, RPS 266.6, p50 40.5 ms, p95 154 ms.

Гейты после переключения: chart parity 57/57, cargo test 136, control parity 15/16,
auth parity OK, control-plane/editor smoke OK. `debug/cache`: entry cache 17 записей,
hits 15954 / misses 34 — кэши работают, просто ограничены.

Почему не ниже: «пол» ~80–90 MB — это аллокатор (mimalloc арены + транзиентные буферы
сжатия/HTTP) и соединения пула, а не кэши. Дальнейшие шаги (не реализованы): периодический
`mi_collect(true)` по простою ≥60 s (ожидание 40–55 MB idle), переиспользование
zstd-контекстов и response-буферов. Откат профиля — удалить блок `Memory profile` в env
или переопределить переменные.

Binary: 18239296 bytes, sha256 `64ca58f954eeb29a1e0ce096922a3ae3e3aaead647f26cf035bfe805aa60b0f4`.

## Phase 3: idle trimmer (2026-11-14) — реализовано, по умолчанию ВЫКЛ

Добавлен `src/memory.rs`: при `MEMORY_IDLE_TRIM_SECS=<n>` процесс раз в 5 s проверяет
активность последнего чарта и после `n` секунд тишины сбрасывает render-кэши
(`clear_idle_caches`: chart extra / parsed shared / table request) и US entry cache, затем
вызывает `mi_collect(true)` (FFI к libmimalloc, без новой зависимости — `cty` недоступен
offline). `0`/отсутствие переменной = выключено. Ручки кэшей: `CHART_EXTRA_CACHE_MAX`,
`PARSED_SHARED_CACHE_MAX/TTL_SECS`, `TABLE_REQUEST_CACHE_MAX` (все уже в env).

Замер `MEMORY_IDLE_TRIM_SECS=20` (c18×20 s, реальные тела):

| фаза | footprint |
|---|---:|
| peak нагрузки | 142.9 MB |
| idle 10 s | 111.5 MB |
| idle 35–55 s (после trim) | 104.9 MB |
| первый чарт после trim | 34.8 ms (без регрессии) |

Логи подтверждают срабатывание (`idle memory trim: caches cleared, mimalloc collected`,
render_caches/entry_cache=0 — кэши уже пусты к этому моменту). Сброс кэшей даёт лишь
~6–7 MB, `mi_collect(true)` не уменьшает остаток: 90–105 MB сидят в категории
`IOAccelerator` (в этом окружении она растёт с трафиком 24 → 90 MB и не отдаётся),
а не в кэшах. Поэтому trimmer **не включён в проде** (env без `MEMORY_IDLE_TRIM_SECS`) —
эффект в пределах шума, а риск лишних действий в рантайме не оправдан.

Вывод фазы 3: остаточные ~90 MB — это не «кэши», а аллокатор/VM-учёт; следующий шаг требует
in-process инструментов (`task_info(TASK_VM_INFO)` + счётчики байт по кэшам), A/B системного
аллокатора против mimalloc и разбора происхождения `IOAccelerator`-регионов. Только после
этого имеет смысл трогать лимиты дальше.

Текущий прод-профиль: idle после трафика ~85–105 MB (медиана 87–90 в чистых прогонах),
peak c18 ~137–141 MB, cold ~26–35 MB; parity 57/57, cargo 136, control 15/16, auth OK,
smoke OK. Binary: 18255872 bytes, sha256 `627c3ab39d60ac817326b2e58c4c89f93c305d086e10b0a42b44bf9caca81b3a`.

## Phase 4: allocator A/B — mimalloc vs system (2026-11-15)

Собран второй бинарник с системным аллокатором: `cargo build --release --features system-alloc`
→ `bin/monolith-v265-systemalloc` (18 054 464 B; mimalloc не линкуется). Фича оставлена как
opt-in, дефолт — mimalloc. Замер: `mem_probe.py --bin ...`, реальные тела, 2 прогона на режим.

| режим | cold | peak c18 | idle 10/60 s | peak c64 | idle c64 | RPS c18 | p50 c18 | p50 c64 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| mimalloc (прод) | 43 MB | 158–162 | 128–159 | 219 MB | 120 MB | 247 / 289 | 44.7 / 39.3 | 144 ms |
| system malloc | 50 MB | **99.8** | **91–99** | **188 MB** | **86 MB** | 255 / 286 | 43.5 / 40.1 | 163 ms |

Повторный A/B с **многопроцессным клиентом** (`--processes 4 --conc 16`, 4×16=64 клиента,
15 s, по 2 прогона) снял подозрение на регрессию — оно было артефактом GIL одного
Python-процесса:

| режим | peak mp64 | idle10 mp64 | RPS mp64 | p50 mp64 | p95 mp64 |
|---|---:|---:|---:|---:|---:|
| mimalloc | 198–207 MB | 122–138 MB | 223 / 231 | 153–161 ms | 740–761 ms |
| system malloc | **177–188 MB** | **90–99 MB** | **224 / 253** | 144–164 ms | 754–801 ms |

**Решение: на macOS дефолтом стал системный malloc** — память ниже на 20–40 MB, перф на
64 клиентах не хуже (медианы RPS 239 vs 227, p50 154 vs 157). mimalloc остался дефолтом
для не-macOS таргетов и доступен на macOS через `--features mimalloc-alloc`
(старый бинарник сохранён: `bin/monolith-v265-mimalloc`). Фичи: `system-alloc` /
`mimalloc-alloc` для принудительного выбора.

Финальные замеры на системном аллокаторе: c18 — peak 109–128 MB, idle 96–104 MB,
RPS 252, p50 45.2 ms; mp64 — peak 194 MB, idle 100 MB, RPS 277.7, p50 135 ms, p95 690 ms.
Гейты: parity 57/57, cargo 136, control 15/16, auth OK, smoke OK.

Итоговый прод-профиль (mimalloc + memory env): cold 26–43 MB, idle после трафика ~87–130 MB
(сильно зависит от хоста), peak c18 ~137–160 MB, peak c64 ~200–220 MB; parity 57/57,
cargo 136, control 15/16, auth OK, smoke OK.


### Итог по памяти (после всех фаз)

| состояние | было (v265 без профиля) | стало |
|---|---:|---:|
| cold idle | 26–43 MB | 50–61 MB (системный аллокатор берёт больше на старте) |
| idle после трафика | 134–160 MB | **90–105 MB** |
| peak c18 | 147–174 MB | **110–128 MB** |
| peak 64 клиента (4×16) | 198–220 MB | **177–194 MB** |

Binary: 18054464 bytes, sha256 `c8b86bdbedfecc49578a1f089809e770ce5740e27d27fadb9c40d0cee269efa6` (macOS, системный аллокатор).
Резервные бинарники: `bin/monolith-v265-mimalloc` (mimalloc), `bin/monolith-v265-systemalloc`
(явный system-alloc).
