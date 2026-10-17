# Native badge writes (noevia#1318)

`native-badges` with `MODEL_MANAGER_BADGES_IMPL=rust` (and `MODEL_MANAGER_IMPL=rust`) serves `PUT /api/v1/badges` and `DELETE /api/v1/badges` from the Rust front. Default `python`; nothing changes until the owner switches it on. `GET /api/v1/benchmark/runs/{id}` and the models list, which also read badges, stay with Python and read the same `DATA_DIR/model_loader.db`.

## Behaviour

- `PUT` reads a FastAPI `dict` body with the prompt writer's rules (content type, CPython JSON encodings, 1 MiB cap, the same 422 bodies), then: `alias` (`str(value or "")`, stripped; empty answers 400 `no model given`), `category` (one of `coding`, `writing`, `reasoning`, `tools`, `vision`, else 400 `unknown category '<repr>'`), `rating` 1-5 (else 400 `rating must be 1-5`), `note` (`str(value or "")`, stripped, first 500 code points), `runId` (0 or missing is none). It upserts one row per alias and category and answers `{"badges": [...]}` for the alias, ordered by category, every column in table order.
- `DELETE` reads the required `alias` and `category` query values (the last of each; FastAPI's 422 lists the missing ones) and answers the alias's remaining badges.
- Each request has one 4.5 s deadline, taken when it arrives. The in-process write lock is waited for only until that deadline (503 `service is busy` otherwise). The lock guard moves into the store job, so a write whose request was dropped (the front's 504) still finishes before the next write starts (noevia#1909). That job is also registered with the request's admission, so the request's permit (one of the front's 4) comes back only when the job has returned (noevia#2006).
- Store failures (no database, no table, a lock held past 4 s, a value that is not UTF-8) answer FastAPI's `500 text/plain Internal Server Error`.

## Fixed on the way (noevia#1905)

Python answered 500 for a non-numeric `rating` or `runId`, truncated `2.9` to 2, read `true` as 1, and stored a padded alias stripped but answered (and deleted) the unstripped one. Native, and Python after the services fix:

- `rating` and `runId` must be integers: a JSON integer, an integral float, or an integer string as `int()` reads it (ASCII digits). Anything else answers 400 `rating must be a number` or `runId must be a number`. A huge integer rating still answers `rating must be 1-5`.
- The alias is stripped once and used for the write, the delete and the answer.

## Ownership

- Child confirmation `NOEVIA_NATIVE_MODEL_MANAGER_BADGES_CONFIRMED=1` only in native mode, when the Python child refuses both methods with 503. In python mode an inherited value is removed. The child gets the same canonical `DATA_DIR`.
- `rust` without `MODEL_MANAGER_IMPL=rust` or the compiled feature refuses startup before the child starts.

## Differential evidence

`tools/gen-badges-oracle.py` runs the actual services FastAPI app, pinned to `noevia-services@641c9c3` (services#64, which carries the #1905 fix, noevia#1910), step by step on 13 synthetic stores with a fixed clock, covering upsert and order, note stripping, the cap and `repr`, alias and category forms, every rating and run id form, body errors, delete forms, seeded rows, and missing, table-less and locked stores. `tests/badges.rs` replays each case natively. There is an `integer-boundaries` case for i64's edges as integers, strings and floats. Bytes must match exactly, except 422 bodies, which compare as JSON because the shared validator writes keys sorted.

## Deliberate differences

- The database is never created, and its journal mode is never changed (Python sets WAL on every connection; its startup already did).
- A stored infinite float, or more than 4,096 rows for one alias, answers 500.
