# Native settings read (noevia#1318)

The optional `native-settings` Cargo feature serves `GET /api/v1/settings` from Rust.
`MODEL_MANAGER_SETTINGS_IMPL` defaults to `python`. Selecting `rust` requires the compiled
feature and `MODEL_MANAGER_IMPL=rust`; any other value refuses before the legacy child starts.
This document does not authorize activation.

## Ownership

- Rust answers only `GET /api/v1/settings` (exact path). `PUT /api/v1/settings` stays with
  Python, the single writer; other methods and `/api/v1/settings/` keep their existing handling.
- The child gets `NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_CONFIRMED=1` only in native mode (`0`
  otherwise; a Python parent removes any inherited value). Under `1` the services middleware
  refuses `GET /api/v1/settings` with 503, so the route is never served twice.
- Both sides use canonical `DATA_DIR` (case-folded aliases refuse, as for native prompts); the
  child is given the same root explicitly.

## Contract

Python (`model-manager/app/api.py`):

```python
token = db.get_setting("hf_token", "")
return {"hasToken": bool(token), "tokenHint": f"…{token[-4:]}" if len(token) > 8 else ""}
```

`len` and slicing count code points; the native side does too. The token itself is never
returned. A failure Python would raise on (missing file or `kv` table, not a database, lock held
past the wait, NULL/number/invalid-UTF-8 value) answers FastAPI's exact
`500 text/plain; charset=utf-8` `Internal Server Error`. A BLOB value reproduces Python's
`bytes` repr hint byte for byte.

## Bounds and deliberate differences

- One read-only connection per request (`SQLITE_OPEN_READ_ONLY`), inside the shared admission
  permits and the 5 s native budget; 4 s busy wait; a progress-handler deadline.
- The database file is never created and never converted to WAL (Python's connection does both).
  On a WAL database the read-only connection can still create the `-shm`/`-wal` side files,
  which SQLite needs for reading; the database file's contents are not written.
- A stored value over 64 KiB is refused (500) instead of being read.

## Oracle

`tools/gen-settings-oracle.py` runs the actual services FastAPI app (middleware, router,
`get_settings`, `db.get_setting`) on synthetic stores and writes
`tests/fixtures/settings-python.v1.json`. The `native-settings` CI job regenerates it from the
pinned services ref and requires identical bytes; `tests/settings.rs` rebuilds every store with
rusqlite and compares status, content type and body bytes.
