# Native benchmark runner (#1318, slice 9a)

`native-benchmark-runner` ports `bench.start`, `bench._run`, `bench.measure` and the routes
`POST /api/v1/benchmark/start`, `GET /api/v1/benchmark/progress` and
`POST /api/v1/benchmark/cancel` (prompt suite only).

## Status

The runner is compiled and checked against the actual Python app, but `main` does not build it yet
(`bench_runner: None`). The job slot is shared by prompt runs and llama-bench sweeps, so it can have
only one owner. Slice 9b adds the sweeps, GPU release, the startup cleanup of stale sweep
containers, the `MODEL_MANAGER_BENCHMARK_RUNNER_IMPL` switch and the Python guard. Then all four
routes move together.

## What stays in Python

The GPU sampler and backend log parsing stay in the child. For each request, the runner asks the
private `GET /api/v1/internal/bench-observe` (`backend`, `alias`, `aliases`, `since`, `cold`) for the
peak VRAM list, the contention flag and the cold load time. The front never forwards that path.
If the child can't answer, the runner records `[]`, not contended, and no load time.

The backend address comes from an injected resolver, which matches `_resolve_endpoint`, including
its `"{type}: {message}"` and `no internal port exposed` refusals.

## Differential

`tools/gen-bench-runner-oracle.py <services>/model-manager/app` drives the real routes against:

- a fake llama-server
- fake Docker
- fixed observations
- fixed-step `perf_counter` / `time` clocks
- workers held until a scripted `run` step

`tests/bench_runner.rs` replays the 14 cases and requires the native runner to produce the same:

- answer bytes
- llama-server request bodies
- observation calls
- quoted `bench_run` / `bench_variant` / `bench_result` rows

Python exceptions inside the worker are reproduced as `"{type}: {message}"` in the right order. They
include binding errors for list values, SQLite integer overflow, `int()` and `str.join` failures, and
non-object chunks.

One difference is intentional. The native runner asks for the cold load time in the same read-only
observation call, while Python asks after the insert. Whether the load time is used shows up in
`bench_variant.load_ms`, which the store comparison checks.

CI regenerates the fixture from pinned services and compares it byte for byte. No test runs a real
benchmark, loads a model or touches a container.
