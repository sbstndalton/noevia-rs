# Native backend restart and test (noevia#1318)

`native-backend-control` serves `POST /api/v1/backends/{name}/restart` and `POST /api/v1/backends/{name}/test` for plain ASCII names (no `%`). It is the only path; the former `MODEL_MANAGER_BACKEND_CONTROL_IMPL` selector is retired and ignored.

## Restart (`services.restart_llama_backend`)
- **Lookup:** `name` must be an effective backend (LLAMA_CONTAINERS plus discovered llama.cpp containers), else 404 `unknown backend`.
- **Refusals:** `docker unreachable` and `container not found` answer `{ok: false, message}`.
- **Queued restart:** the backend's last restart error is cleared, `POST /containers/{name}/restart?t=15` runs in the background, and the answer is `{ok: true, message: "restart queued"}`.
- **Failures:** a failure is recorded as `last_restart_error` in the native sampler's restart-error store. That store is the only copy and the source of truth: the restart clears the entry before it is queued, a failed restart sets it, and the native `GET /api/v1/backends` and the overview's backend list read it from there. The shipping binary has no child, so nothing is sent to one (the child's private `PUT /api/v1/internal/backend-restart-error` is gone, in release and test builds). `tests/backend_control.rs` checks that both routes report the store's error after a failed restart and report none after a successful one.
- **Docker policy:** `docker::allowed_backend_restart` allows only `restart?t=15`, by name, never an all-hex id, and the caller has already checked the name is an effective backend.

## Test (`services.test_prompt`)
- **Body:** `prompt` is `str(value or "").strip()`, and an empty prompt answers 400. `maxTokens` is `int(value or 256)` clamped to 1..4096.
- **Port:** resolved as `_resolve_internal_port`:
  - Docker's sorted ports map, last key that parses
  - else `--port N` in Cmd/Entrypoint
  - else `LLAMA_ARG_PORT`
  - else `LLAMA_DEFAULT_PORT` (8080)
- **Probe:** `/v1/models` is probed with Python's notes (`HTTP n`, `timeout`, `connection refused`, `request failed`, `invalid response`, `no models configured`, `n configured, none loaded`).
- **Chat:** one non-streaming chat completion is sent. The answer is `{ok, model, reply, completion_tokens, prompt_tokens, elapsed_s, tokens_per_s}`, with Python's rounding. Shapes Python fails on (non-dict JSON, non-integer usage) stay a 500.

## Fixed on the way
- noevia#1928: a non-integer `maxTokens` answered 500. It answers 400 `maxTokens must be a number` here. Python keeps the old behaviour until its handler is deleted.

## Decisions (technical)
- **Error texts:** Docker errors are the native transport's own texts (`docker error: <reason>`, `APIError: Docker refused the restart (HTTP n)`), not the Docker SDK's. Chat transport errors use httpx's wording for status errors (byte-identical) and short fixed texts for connect/timeout errors.
- **Timeouts:** the background restart is bounded at 60 s. Probe timeouts are 2 s to connect and 5 s total; chat timeouts are 10 s to connect and 190 s total. Answers are capped at 8 MiB.
- **Name shapes:** escaped or non-ASCII names stay with Python, and the Python guard uses the same raw-path rule.

## Differential evidence
`tools/gen-backend-control-oracle.py` runs the actual services app (pinned `8ae55c9`) with a fake Docker client and a fake llama-server on 127.0.0.1, with `time.time` pinned inside `test_prompt`. It records each answer, each chat request body and the restart error left behind.

The cases cover body forms, probe notes, chat answer forms (status error, empty, odd choices and usage), ports from the command and from env, a container that is not found, and restarts that succeed and fail. `tests/backend_control.rs` replays everything with a fake `DockerRead`, a fake server and a fake child, and requires the same answers, chat requests and restart-error state.
