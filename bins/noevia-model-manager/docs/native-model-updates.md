# Native model update status read (noevia#1318)

The optional `native-model-updates` Cargo feature serves `GET /api/v1/models/updates` from Rust.
`MODEL_MANAGER_MODEL_UPDATES_IMPL` defaults to `python`. Selecting `rust` requires the compiled
feature and `MODEL_MANAGER_IMPL=rust`; any other value refuses before the legacy child starts.
This document does not authorize activation.

## Ownership

- Rust answers only `GET /api/v1/models/updates` (exact path). Other methods, the slash variant,
  `GET /api/v1/models` and `GET /api/v1/models/detail` keep their existing handling. Python stays
  the only writer of `update_check` (`POST /api/v1/models/check-updates`, downloads).
- The child gets `NOEVIA_NATIVE_MODEL_MANAGER_MODEL_UPDATES_CONFIRMED=1` only in native mode (`0`
  otherwise; a Python parent removes any inherited value). Under `1` the services middleware
  refuses `GET /api/v1/models/updates` with 503, so the route is never served twice.
- Both sides read the same `MODELS_DIR`, `MODELS_INI_PATH` and `DATA_DIR`: case-folded aliases
  refuse at startup and the child is given the three roots explicitly.

## Contract

Python (`model-manager/app/api.py`, `helpers._update_status_map`, `services.snapshot_models_dir`):
for each model in the models-folder snapshot (files ending `.gguf` up to four folders deep, hidden
and Hugging Face cache-internal folders skipped, links followed for files but not folders, shards
grouped, a projector/imatrix/draft-head companion folded into its folder's model, sorted by
folder, companion flag and name) the `update_check` row keyed by its file name becomes
`{status, remote, checked_at, delta_days}`, in Python's dict order (a name seen twice keeps its
first position and its last value).

Ported exactly and differential-tested: the walk and grouping; Python's companion file-name rule
(no new name-based logic); `st_mtime` as `sec + nsec * 1e-9`; `datetime.fromtimestamp` with
half-even microsecond rounding; CPython 3.12's C `datetime.fromisoformat` (separator by byte
position, a non-ASCII separator skipped by its lead byte, the first `Z`/`+`/`-` as the zone, a
zero offset as UTC, the `HH:MM:SS:ffffff` and `.+00:00` quirks); `float.__repr__`; FastAPI's JSON
(a non-finite `checked_at` renders `null`, a UTF-8 BLOB renders as text).

Every condition where Python's handler raises answers FastAPI's exact
`500 text/plain; charset=utf-8` `Internal Server Error`: no store or no `update_check` table
(Python's sqlite3 would also create the file), invalid UTF-8 in any stored text, a naive or
date-only remote time, a non-text truthy remote, a non-UTF-8 BLOB `checked_at`, an unreadable,
non-UTF-8 or unparseable `models.ini` (only read when the models folder exists), a lock held past
the wait, or a model mtime past year 9999 when its remote time parsed.

## Bounds and deliberate differences

- One read-only store connection per request (`SQLITE_OPEN_READ_ONLY`), inside the shared
  admission permits and the 5 s native budget; 4 s busy wait; a progress-handler deadline. The
  store is never created and never converted to WAL.
- Over 65,536 `update_check` rows, a stored value over 64 KiB, or a `models.ini` over 1 MiB
  answers 500 instead of being read.
- The walk uses the kernel-contained beneath-root opener of the native sections read, and follows
  Python where it safely can (noevia#1873): an unreadable folder is skipped as `os.walk` skips it;
  an absolute link whose fully resolved target stays inside the canonical models folder is
  followed; a symlinked models folder is canonicalised and walked. It refuses with
  `503 {"detail":"Native model update status is unavailable"}` instead of guessing for more than
  4096 model files or 16,384 directory entries, a walk over 2 s, a link whose target leaves the
  models folder, or a non-UTF-8 name.
- `models.ini` is opened non-blocking and must be a regular file; a FIFO or device answers 500
  (Python would block on it).
- A response body over 8 MiB answers 500 instead of being built.

## Oracle

`tools/gen-model-updates-oracle.py` runs the actual services FastAPI app (middleware, router,
snapshot, ini and db code) on synthetic model folders, ini files and stores, and records CPython's
`datetime.fromisoformat` for a deterministic corpus of about 3,800 strings. It writes
`tests/fixtures/model-updates-python.v1.json`. The `native-model-updates` CI job regenerates it
from the pinned services ref with Python 3.12 and requires identical bytes. Generate it on Linux: the service's glibc `strftime` does not zero-pad `%Y` before year 1000 (macOS does), and the route returns that unpadded year as Python does. The lib tests rebuild
every case through a synthetic root on any platform; `tests/model_updates.rs` runs the same cases
through the router and the real opener on Linux.
