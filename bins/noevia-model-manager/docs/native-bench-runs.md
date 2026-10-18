# Native benchmark run read (noevia#1318)

`native-bench-runs` with `MODEL_MANAGER_BENCH_RUNS_IMPL=rust` serves `GET /api/v1/benchmark/runs/{id}` from the Rust front. Default `python`.

## What is native and what is asked of Python

Native: the run, its variants, results and sweeps, each variant's badges (every column, in table order), and the charts computed from them, with Python's rules:
- Only clean rows count: not cold, not contended, no error.
- Medians are taken over truthy values. `round()` is correctly rounded with ties to even, and an int stays an int.
- Depth-decay series are built from the first matching sweep.
- Measured VRAM is `max(sum(json.loads(peak_vram_json)))`, rounded to 2. Unparseable JSON is skipped and a non-summable value is a 500, as in Python.

Two chart values are live and come from the Python child's private `GET /api/v1/internal/bench-run-runtime?alias=…` call, which answers `{"capacity_gb": round(capacity, 2), "predicted": {alias: _predicted_vram_gb(alias)}}`:
- `capacity_gb`, from the GPU sampler through `helpers._backend_list`.
- `vram_predicted`, from autoconfig on the model files against those backends.

Both are placed verbatim, so the sampler and autoconfig are never re-implemented. This is the model-detail pattern. A child that cannot answer, or leaves an alias out, gives 503 `Native benchmark run is unavailable`; the child's 500 stays a 500.

## Decisions (technical, made here)
- All path and integer spellings follow the actual FastAPI ASGI contract. Decode the path once with UTF-8 replacement; encoded slashes change route shape, and malformed percent escapes stay literal. Trailing slashes redirect with 307, preserving query and URL quoting. Other methods answer 405 with `Allow: GET` before integer validation; HEAD has no body. Authentication precedes every branch.
- Integer strings accept signs, leading zeros, Unicode whitespace, valid underscores and an all-zero fractional part. Invalid values give the path `run_id` 422. Valid integers outside SQLite's signed 64-bit range give its plain 500; they are not misclassified as invalid input. Pydantic first attempts JSON integer parsing, then its cleanup step; their oversized-number errors remain distinct (`int_parsing_size` or `int_parsing`). Leading zeros do not impose a 64-character restriction.
- The existing front URI limit (8192 bytes, 414 after authentication) remains unchanged; the parser adds no smaller integer-string limit.
- The route uses the forwarded budget, not the 5 s native one, because it waits on the child, as model detail does.
- Bounds:
  - 100,000 rows per table and a 64 MiB answer
  - 1,024 aliases sent to the child, and a 1 MiB runtime answer
  - a 3 s busy wait and a 3.5 s progress deadline on the store
  - a 60 s child call
  - the store is read-only and never created

  Rows Python could not have written through the bench writer (non-text aliases, non-integral `n_depth`, NaN) answer 500.

## Differential evidence
`tools/gen-bench-runs-oracle.py` runs the actual services app (pinned `641c9c3`, the same legacy ref as the badges slice) on synthetic stores, with `_backend_list` and `_predicted_vram_gb` fixed. It records each answer and the runtime answer the child gives for the same inputs.

The cases cover:
- a full run (cold, contended and errored rows, mixed peak JSON, unicode aliases and text, sweep depths with NULLs)
- no GPU, and a sampler that raises
- leading zeros, a missing id and a negative id
- an empty run and an all-cold run
- rounding ties

`tests/bench_runs.rs` replays every case byte for byte against a fake child and checks the private call's query. It also covers child failures, Python mode, 405, the private path being unreachable from outside, and a missing store.


`tools/gen-bench-run-paths-oracle.py` separately runs the actual ASGI app pinned to services `60764e67f390ea9dad5c91c3f41aad865d757823`. It injects Uvicorn-equivalent once-decoded scope paths directly, avoiding TestClient's extra percent decoding. The fixture records FastAPI 0.115.6, Starlette 0.41.3, Pydantic 2.14.0 and pydantic-core 2.50.0; newer framework behavior is not silently substituted. Synthetic SQLite contains one empty run. The source-hashed fixture covers encoded namespaces/IDs, malformed bytes and percent escapes, encoded and double-encoded separators, signs, fractions, Unicode whitespace, long leading zeros, SQLite overflow, Pydantic size errors, absent/nested paths, all methods, redirects with query/Unicode/reserved characters and authentication precedence. The router replays the same cases; only successful reads use a fixed child chart runtime. No child lifecycle or chart selection changes are made here.
