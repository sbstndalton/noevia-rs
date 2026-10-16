# Native benchmark run read (noevia#1318)

`native-bench-runs` with `MODEL_MANAGER_BENCH_RUNS_IMPL=rust` (and `MODEL_MANAGER_IMPL=rust`) serves `GET /api/v1/benchmark/runs/{id}` from the Rust front. Default `python`.

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
- Only a canonical id (`-?[0-9]+` inside i64, leading zeros allowed) is native. Any other spelling (`%31`, `+1`, `1.0`, an id past i64) is forwarded, and Python keeps its own 422/500 for it. The Python guard compares the raw path with the same rule, so a percent-encoded id is not refused there.
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

`tests/bench_runs.rs` replays every case byte for byte against a fake child and checks the private call's query. It also covers child failures, non-canonical ids, python mode, 405, the private path being unreachable from outside, and a missing store.
