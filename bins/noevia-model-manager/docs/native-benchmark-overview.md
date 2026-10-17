# Native benchmark overview (noevia#1318)

`native-benchmark-overview` with `MODEL_MANAGER_BENCHMARK_OVERVIEW_IMPL=rust` (and `MODEL_MANAGER_IMPL=rust`) serves `GET /api/v1/benchmark`. Default `python`. Any other method on that path, the trailing-slash form and every job control route (`/benchmark/start`, `/sweep`, `/progress`, `/cancel`) stay with Python.

## What is read here (`api.benchmark_overview`)
- **`sections` and `sweepArgs`:** one read of models.ini through the same configparser emulation the section writers use (`parse_ini_text(allow_duplicates=True)`). A missing file means no sections, and a file that is not UTF-8 answers 500. `sweepArgs` is `bench.sweep_args_for_section`: DEFAULT keys plus the section's own keys, in `_SWEEP_FROM_INI` order, first flag wins, `-ts` commas become slashes.
- **`prompts` and `runs`:** `db.list_prompts()` and `db.bench_runs(limit=25)`, the same SQL, opened through `sqlite_read::open` (noevia#1922).
- **`backends`:** `services._effective_container_names()`. Any Docker failure means no discovered containers, as in `discover_llama_containers`.
- **Fixed values:** `maxTokensDefault` 3072, `maxTokensCeiling` 8192, and `categories` from `db.BADGE_CATEGORIES`.

## What is asked of the child
`job` is the runner's live state. It lives in the Python process with the runner thread, so it comes from the child's private `GET /api/v1/internal/bench-job` (`api._bench_job()`, served only under `NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_OVERVIEW_CONFIRMED=1`) and is spliced in byte for byte. A child that cannot answer gives 502 `Python-owned endpoint unavailable`. A 500 or an answer that is not a JSON object gives FastAPI's 500.

## Fixed on the way
- noevia#1935: FastAPI writes non-finite floats in a `-> dict` answer as `null` (pydantic JSON mode). The shared row renderer refused them, so the native benchmark run read answered 500. Both reads now write `null`.

## A dropped request (noevia#2006, #2015)
The models.ini read and the store reads run on one blocking task that is registered with the request's admission. A request that is dropped (the client went away) or that runs past its budget keeps its permit (one of the front's 4) until that task has returned, and a request that timed out sends its 504 only then. The store reads have a deadline. The models.ini read does not, and a filesystem call that has started cannot be interrupted. So on a hung mount neither the 504 nor the permit comes back until that read returns, and there is no bound on that. After four such requests, every admitted route answers 503 `service is busy`.

## Decisions (technical)
- **Job controls stay in Python for now.** `start`, `sweep`, `progress` and `cancel` all act on the one runner thread and its in-process state. That runner also does the streamed measurements, starts llama-bench sweep containers, frees the GPU and reaps orphans at startup. Moving only some of these routes would give the job slot two owners, so they move together with a native runner in a later slice.
- **`MODELS_INI_PATH` spelling:** the path must have one canonical spelling (pydantic reads the variable case-insensitively) and be absolute and normal. Otherwise the switch falls back to `python` with a log line. The child gets the same `DATA_DIR` and `MODELS_INI_PATH`.

## Differential evidence
`tools/gen-benchmark-overview-oracle.py` runs the actual services app (pinned `d475f42`) on a synthetic models.ini, a synthetic store, a fake Docker client and a fixed `bench.JobState`.

The cases cover:
- a full file with DEFAULT keys and preamble
- no file
- duplicate sections
- a continuation value
- a non-UTF-8 file
- a store with more than 25 runs
- Docker unreachable
- an infinite stored float

`tests/benchmark_overview.rs` replays every case with a fake child serving the recorded job, and checks the private request, the omission and the routes that stay forwarded.
