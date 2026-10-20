# Native model list and overview reads (noevia#1318)

The optional `native-model-list` Cargo feature (it includes `native-model-detail`, whose snapshot,
shape and entry fields it reuses, and `native-bench-runs`, whose SQLite row reader it reuses)
serves `GET /api/v1/models` and `GET /api/v1/overview` from Rust. Both are always native
(noevia#1071): the former selectors `MODEL_MANAGER_MODELS_IMPL` and
`MODEL_MANAGER_OVERVIEW_IMPL` are retired and ignored. Settings this side cannot read exactly as
the Python reference did no longer keep a route with Python; they refuse startup (see
"Refused settings" below). This document does not authorize activation.

## Ownership

- Rust answers only `GET /api/v1/models` and `GET /api/v1/overview` (exact paths). Other methods,
  the slash variants and every other model route keep their existing handling.
- The process sets neither `NOEVIA_NATIVE_MODEL_MANAGER_MODELS_CONFIRMED` nor
  `NOEVIA_NATIVE_MODEL_MANAGER_OVERVIEW_CONFIRMED` for any child: the routes are never selected per
  route.
- Refused settings (startup fails, never a fallback to Python): `TZ` other than exactly `UTC`
  (the native route formats `modified` in UTC); a case-folded alias of `MODELS_DIR`,
  `MODELS_INI_PATH`, `DATA_DIR` or `MODELS_HOST_PATH` (the Python settings fold names); a
  `MODELS_DIR` that is not absolute and in pathlib's normal form (it is answered as configured);
  an empty `MODELS_INI_PATH` or `DATA_DIR`; or one of these over 4096 bytes
  (`model_list_cmd::native`, unit-tested in `src/model_list_cmd.rs`).

## Which fields are native and which are delegated

Native, ported exactly and differential-tested:

- `/models`: the snapshot (walk, shard grouping, companion folding, order, ini aliases and
  `sections_by_file`), every file field of `_entry` (`modelId`, sizes, `modified`, `mtime`,
  `projector`, `file`, `path`, `sections`, `aliases`) and `services.model_shape` (header read
  through the `gguf` crate; read and parse errors give `null` as Python's swallowed exceptions do,
  and only successful reads are cached, keyed by path, mtime and size as `gguf_meta` keys them),
  `unregistered_gguf_stems`, `revision` (SHA-256 of the raw ini bytes) and the badge join of
  `_badges_for_files`, read-only from `DATA_DIR/model_loader.db` (never created). The join keeps
  Python's semantics: the newest row per category by `created_at or 0`, ties to the first, a
  comparison Python would refuse (text against a number) and a missing badge table both answer
  FastAPI's plain 500 as Python does. The disk usage is taken first, as `snapshot_models_dir()`
  does, so a zero-sized filesystem is the same 500.
- `/overview`: `modelsDir` (`path` as configured, `hostPath` or `null`, `exists`, the missing
  folder's `error` text, and `disk` with `usedPct` computed as Python's exact integer true division
  followed by `round(x, 1)`), the non-companion model count, the section count and `revision`.

**noevia#1318 step 1b: native live fields.** While this process samples the backends
(`native-backends`, the only path), both bodies are produced natively (`backend_stats/live.rs`):
`/models` gets `BackendStats::models_runtime` (the backend snapshot for `loaded`, one fit
inventory judged for every listed size), and `/overview` gets `BackendStats::overview_runtime`
(the snapshot without `stats` and `history`) when this process also owns the download table.
Without the native table, the overview still asks the child, for its download count. Before
its first badge read, the list waits once per process for the child's private `GET /api/v1/internal/startup-ready`,
because the child's startup creates that store. Until the child has answered, the list keeps
the forwarded route's 502. The private runtime handlers are then unused by native.
`tools/gen-fit-oracle.py` records the child's own answers (loaded and unloaded models, Docker
down, restart errors, CPU and GPU chips), and `backend_stats::live::tests` reproduces them byte
for byte. `tests/model_list.rs` checks the routes through the router.

Delegated, never re-implemented: live state only the child computes. Only after the file part
succeeded does the parent ask the child, under the forwarded routes' budget:

- `/models`: `POST /api/v1/internal/models-runtime` with `{"bytes":[<total per model>]}`, answered
  `{"loaded": api._loaded_map(), "fit": [services.vram_fit_chips(b) for b in bytes]}`. `loadedOn`
  is `loaded[modelId]` (or `[]`) and the chips are placed verbatim. The badge store is read after
  that call succeeds, so a 500 from the store follows the child's startup (and its `db.init()`).
- `/overview`: `GET /api/v1/internal/overview-runtime`, answered
  `{"backends": [...], "activeDownloads": <count>}`. While this process owns the download table
  (`native-downloads` selected), its own count of queued and downloading jobs replaces the child's.

The child renders both with the routes' own serialisation (pydantic's JSON mode, then the JSON
response class), so every float keeps Python's text and non-finite values are `null`. A child 500
answers FastAPI's plain 500; a refusing, missing, malformed or oversized (over 16 MiB) answer, or a
`fit` list whose length does not match, is the forwarded route's
`502 {"detail":"Python-owned endpoint unavailable"}`.

## Bounds and deliberate differences

The walk and its refusals are the native model update read's (`docs/native-model-updates.md`). In
addition a route answers `503 {"detail":"Native model list is unavailable"}` instead of guessing
for:

- the list: a section whose `model =` path has a `..` segment or leaves through a link, when that
  section could configure a listed file (the overview only counts files and is still answered);
  a header read past the reader's 256 MiB bound; more than 4096 models; or the file part
  (including waiting for the one read slot) running past 60 s;
- the overview: a disk usage query that fails (Python reports it with an errno text in `error`),
  a free size above the total, or a `MODELS_DIR` that is not UTF-8.

The shape cache holds at most 4096 entries and is cleared when full.

The blocking reads run one at a time per slot, and the deadlines are checked between reads,
so a hung filesystem read holds its slot and the request's admission permit until it returns.
The list's file part and its badge read share one slot; the overview has its own, so a list
stuck on a hung read never holds the overview up (a read hung in the models folder itself
would hold both). The badge read is bounded
(100 000 rows, 16 MiB per value, 64 MiB of text and blob values in all, 3 s busy wait,
3.5 s deadline) and answers 500 past those bounds.

## Startup `db.init`

Not ported, deliberately. The child always starts and runs `db.init()` in its startup hook before
it serves. The native list reads the badge store only after the child's runtime call succeeded,
and the native readers open the store read-only and never create it, so no native route depends on
the schema existing before the child. Moving `db.init` belongs to retiring the child.

## Oracle

`tools/gen-model-list-oracle.py` runs the actual services FastAPI app on synthetic model folders,
ini files, GGUF headers and badge stores, with the live sources (the fit sampler, backend
discovery, the download table and the disk usage) replaced by deterministic stand-ins, and
records each answer of both routes plus the runtime bodies the child renders for them. It writes
`tests/fixtures/model-list-python.v1.json`; the `native-model-list` CI job regenerates it from the
pinned services ref with Python 3.12 and requires identical bytes. The lib tests rebuild every
case through a synthetic root on any platform; `tests/model_list.rs` runs the same cases through
the router, the real opener and a child stand-in on Linux, and checks the runtime requests (path,
body and the token header). `tests/model_list_lifecycle.rs` checks the selectors at process level.

Readiness requires exact HTTP 200 `{"startup_ready":1}` from a child containing services#101. Unknown successful bodies, oversized replies and old children refuse the read. A ten-second total wait bounds startup retries; success is cached per list instance. Public child health remains 503 (#2044).
