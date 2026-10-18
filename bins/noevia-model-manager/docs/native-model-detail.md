# Native model detail read (noevia#1318)

The optional `native-model-detail` Cargo feature (it includes `native-model-updates`, whose
models-folder walk it reuses) serves `GET /api/v1/models/detail` from Rust.
`MODEL_MANAGER_MODEL_DETAIL_IMPL` defaults to `python`. Selecting `rust` requires the compiled
feature and `MODEL_MANAGER_IMPL=rust` (otherwise startup refuses, as for every switch). It never
refuses startup over the zone or roots (noevia#1889): with `TZ` unset or `UTC` and an absolute,
normalised `MODELS_DIR` the native read is used; any other zone, or roots it cannot use, keeps
Python serving the read and logs one line. This document does not authorize activation.

## Ownership

- Rust answers only `GET /api/v1/models/detail` (exact path). Other methods, the slash variant and
  every other model route keep their existing handling.
- The child gets `NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DETAIL_CONFIRMED=1` only in native mode (`0`
  otherwise; a Python parent removes any inherited value). Under `1` the services middleware
  refuses `GET /api/v1/models/detail` with 503, so the route is never served twice, and the
  private `GET /api/v1/internal/model-detail-runtime` call is served (it is 404 otherwise). The
  native front never forwards that path, so it is reachable only from the parent.
- In native mode the child is given `TZ=UTC` (the native route formats UTC) and the same `MODELS_DIR` and `MODELS_INI_PATH`.

## Which fields are native and which are delegated

Native, ported exactly and differential-tested: the `key` query (FastAPI's 422 when it is missing,
the last value when repeated), `_find_entry`'s 400 and 404, the snapshot (walk, shard grouping,
companion folding into the last model of a folder, order), the ini aliases (`[]` when a section
header repeats) and `sections_by_file` (a section's `model =` path, else the stem of a
non-companion file, a draft-named file over 2 GiB counting as a model), `modelId`, sizes
(`human_bytes`), `modified` and `mtime`, `projector`, `file`, `path`, `services.model_shape`
(Python's `str()` of the architecture and `int()` of the expert counts) and the GGUF summary
through the `gguf` crate with `chat_template` removed (422 with Python's message for a header the
parser rejects, 500 when the summary raises).

**noevia#1318 step 1b: native live fields.** While this process samples the backends
(`MODEL_MANAGER_BACKENDS_IMPL=rust`, the `native-backends` sampler running), the same
`{"loaded": ..., "fit": ...}` body is produced natively by `BackendStats::detail_runtime`
(`backend_stats/live.rs`): the backend snapshot `GET /api/v1/backends` uses for `loaded`, and
`backend_stats::fit` over the sampler cache, the read-only Docker discovery and `/proc/meminfo`
for `fit`. It is spliced exactly as the child's was, and the child is not asked at all. A snapshot
Python would have ended with an exception is the plain 500; a Docker answer the native reader
cannot follow, or a HOST_RAM_RESERVE_GB it does not read, is the native 503. The child's
`/api/v1/internal/model-detail-runtime` handler is then unused by native and is kept only for a
process whose sampler stays with Python. `tools/gen-fit-oracle.py` records that handler's answers
for loaded, unloaded and Docker-less worlds; `backend_stats::live::tests` reproduces them byte for
byte.

Delegated: `loadedOn` (Docker discovery plus llama-server probes) and `fit` (the GPU sampler and
host RAM) are live state. Only after the file part succeeded does the parent call the child's
`GET /api/v1/internal/model-detail-runtime?bytes=<total>&fit=<0|1>`, which answers
`{"loaded": api._loaded_map(), "fit": services.vram_fit_chips(bytes) or []}` rendered by the same
JSON response class. `loadedOn` is `loaded[modelId]` (or `[]`); the chips are placed verbatim, so
every float keeps Python's text. A child 500 answers FastAPI's plain 500; a refusing, missing,
malformed or oversized (over 1 MiB) answer is the forwarded route's
`502 {"detail":"Python-owned endpoint unavailable"}`. The route never fills these fields itself.

## Bounds and deliberate differences

The walk and its refusals are the native model update read's (`docs/native-model-updates.md`). In
addition the route answers `503 {"detail":"Native model detail is unavailable"}` instead of
guessing for: a section whose `model =` path has a `..` segment or leaves through a link, but
only when that section could configure the requested file (it falls back to that file's stem);
every other model is still answered (noevia#1889). Also for: an I/O error opening or reading the model file (Python's text names an errno), a
file that vanished after the walk, a header read past the reader's 256 MiB bound, an expert count Python would print past
128 bits or written in non-ASCII digits, an architecture value the bounded formatter cannot print,
or the file part (including waiting for one of the two concurrent read slots, since each may hold
a header of up to 256 MiB) running past 4.5 s. The detail route waits for the child under the forwarded
routes' budget, not the 5 s native budget.

## Oracle

`tools/gen-model-detail-oracle.py` runs the actual services FastAPI app on synthetic model folders,
ini files and GGUF headers, with the two live sources replaced by deterministic stand-ins, and
records each answer plus the runtime body the child renders for it. It writes
`tests/fixtures/model-detail-python.v1.json`; the `native-model-detail` CI job regenerates it from
the pinned services ref with Python 3.12 and requires identical bytes. The lib tests rebuild every
case through a synthetic root on any platform; `tests/model_detail.rs` runs the same cases through
the router, the real opener and a child stand-in on Linux, and checks the runtime request (path,
`bytes`, `fit` and the token header).
