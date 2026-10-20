# Native benchmark runner (#1318, slices 9a and 9b)

`native-benchmark-runner` takes over the one benchmark job slot, and it is the only path; the former `MODEL_MANAGER_BENCHMARK_RUNNER_IMPL` selector is retired and ignored. It serves `POST /api/v1/benchmark/start`, `POST /api/v1/benchmark/sweep`, `GET /api/v1/benchmark/progress` and `POST /api/v1/benchmark/cancel`, and runs the startup cleanup of leftover sweep containers.

It also needs `MODEL_MANAGER_BENCHMARK_OVERVIEW_IMPL=rust`, because the overview then reads `job` from the runner. Without that, the runner stays with Python and one line is logged.

With the switch on, the parent sets exact `NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_RUNNER_CONFIRMED=1`. The child then refuses the four routes, skips `reap_orphans`, and serves the private observation route. Only one owner of the slot exists either way.

## What stays in Python

The GPU sampler and backend log parsing stay in the child. For each prompt-suite request, the runner asks the private `GET /api/v1/internal/bench-observe` (`backend`, `alias`, `aliases`, `since`, `cold`) for:

- the peak VRAM list,
- the contention flag,
- the cold load time.

The front never forwards that path. If the child cannot answer, the runner records `[]`, not contended, and no load time.

## Docker

Sweeps go through `docker::sweep::allowed_sweep`, a named allowlist that sits next to the existing read allowlist:

- **Create:** only `POST /containers/create`, with a body built from the typed `CreateBody`. That type has only these fields: `Image`, `Cmd`, `Entrypoint`, `Labels`, `HostConfig.Binds`, `HostConfig.NetworkMode`, `HostConfig.DeviceRequests`, `HostConfig.Devices` and `HostConfig.GroupAdd`. There is no env, and no ports, privileges, capabilities, namespaces, security options or volumes-from. Devices and groups can only be the backend's own.
- **Checks before every create (`check_create`):**
  - exactly the sweep label;
  - the backend container's image id (`sha256:…`), read live by inspect, so a re-pulled tag can't change what runs;
  - entrypoint `/app/llama`;
  - network `none`;
  - the backend's GPU request, copied;
  - the backend's own `Devices` and `GroupAdd`, copied exactly: never more, never other, and only device nodes under `/dev/`;
  - only backend mounts inside `MODELS_DIR` that hold the model, all `:ro`.
- **start / kill / inspect / logs / `DELETE ?force=true`:** only for a full 64-hex id that this process created, or saw in the label-filtered list (with the label checked again). Never a name, prefix or wildcard.
- **List:** only with the sweep label filter, capped at 64.
- **Backend reads:** `_sweep_runtime` and `_resolve_endpoint` use the existing read allowlist, and only for an effective backend (configured, or a discovered llama.cpp container), by name. An all-hex name is refused, because Docker would take it as an id prefix. Anything else gets `container not in configured list` before any Docker call. The GPU-freeing restart uses the existing backend restart, limited to effective backends.
- **Deadlines:** every sweep Docker call is bounded at 30 s, and the backend restart at 60 s. A timed-out call is a `ReadTimeout:` error. A cancel that arrives while Docker stalls still ends the job as cancelled, and the container is removed.

## Startup cleanup

At startup, the runner lists containers by label and force-removes each one. A container that is already gone counts as removed. This step is bounded at 60 s.

A container that can't be removed keeps the job `running`/`models`, as Python does. A watcher retries every 5 s. Unlike Python, it gives up after 720 tries (one hour) and reports an error.

## The job slot

The slot is claimed before the backend lookup (prompt suite) or the `models.ini` read (sweep). If the request is dropped before the worker takes over, the slot goes back to idle. This happens on a client disconnect or when the request budget runs out. A `bench_run` row already written is then finished as `error`. The start's store reads and writes, and that finish, are registered with the request's admission, so a dropped start keeps its request permit until they have returned; the worker's own store writes run outside any request (noevia#2006). The cancel flag is reset inside the claim, so a cancel during `starting` is kept and the worker ends the run as cancelled. Python clears it after the lookup.

## Differences from Python, on purpose

- A sweep mounts only the models mounts that hold the model. Python mounts every backend mount read-only.
- The backend's GPU request is copied. Python asks for all GPUs whenever the backend has any request.
- The backend's `Devices` and `GroupAdd` are copied (noevia#1947). Python's sweep copies neither. On an iGPU host like DaServer, the llama.cpp backend gets its GPU through compose `devices:` (render and card nodes, no `group_add` today) and has no `DeviceRequests`. So a Python sweep there runs llama-bench without the GPU. A native sweep gets the same device nodes as the backend.
- The sweep runs the backend's image id, not `tags[0]`. Python's refusal of an untagged image (`cannot determine backend image`) is kept.
- Only effective backends are inspected for sweeps and endpoints. Python inspects any named container.
- Docker calls have deadlines (see above). Python relies on docker-py's 60 s default.
- `_free_gpu` waits for the backend restart before polling `/v1/models`, and reports a refused restart instead of polling past it (noevia#1940).
- Docker error texts are `"{NotFound|APIError}: {Docker's message}"`. They don't include docker-py's URL prefix.
- The cold load time is asked for in the same read-only observation call as the other observations.

## Differentials

Two generator scripts run the real Python routes against the same fakes as the Rust tests:

- `tools/gen-bench-runner-oracle.py` covers the prompt suite (14 cases). `tests/bench_runner.rs` replays them.
- `tools/gen-bench-sweep-oracle.py` covers sweeps and startup cleanup (10 cases). It runs against a scripted fake Docker client, a fake llama-server and a fixed-step clock. `tests/bench_sweep.rs` replays the cases through the real `UnixSweepDocker`, served by a fake Docker daemon on a Unix socket.

The replays require the same:

- answers;
- llama-server requests;
- backend restarts;
- containers created, killed and removed;
- stored rows.

The cases include refusals, conversions, sqlite binding errors, timeouts, create and start failures, and cancel. Further tests cover:

- a cancel while llama-bench is running (kill, then remove);
- a refused restart;
- the bounded watcher;
- that every verb on a container that isn't ours is refused before it reaches Docker;
- that a non-backend name or hex prefix is refused before any Docker call;
- that listed entries without exactly the sweep label, or with a short id, are left alone, and that a list of more than 64 removes nothing;
- that a stalled daemon can't pin the job, with and without a cancel;
- an iGPU backend's devices and groups, copied exactly;
- a dropped start, and a cancel while starting, for both the prompt suite and sweeps.

No test runs a real benchmark, loads a model or touches a real container.
