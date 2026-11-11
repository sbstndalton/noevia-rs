# noevia-router: one OpenAI endpoint, one local model at a time

Refs noevia#2440. A native port of the router prototype's behaviour, built on the `model-router`
crate. It is **not** wired into production: no compose file or environment references it.

    noevia-router --config router.toml [--host H] [--port P] [--logdir DIR] [--preload MODEL]
    noevia-router --config router.toml --check     # validate; print name, engine, aliases

The request's `model` (or an alias) picks a `[[models]]` entry. When another model is loaded, the
router does the following, in order:

1. **Drain.** It waits for in-flight requests to finish.
2. **Stop, verified.** It runs the entry's `stop` command. Then it sends SIGTERM to the backend's
   process group, and SIGKILL after `stop_timeout`. Finally it runs `docker rm -f` on every
   `containers` name until `docker ps -a` no longer lists it, and checks that no process of the
   group is left (a host process can hold the GPU too). The stop command runs even if the
   backend has already exited.
3. **Start.** It runs `sh -c cmd` in a new process group, with its output appended to
   `backend-<name>.log`.
4. **Health.** It polls `health` every 250 ms until the body contains `health_expect`, the process
   exits, or `ready_timeout` passes.
5. **Proxy.** It forwards the request (path and query) and streams the response back as it
   arrives, SSE or not.

Every swap, unload, death and shutdown is appended to `<logdir>/router-events.jsonl` with its
timings (`stop_s`, `stopped_s`, `ready_s`, `total_s`, `gpu_lock_wait_s`). `GET /router/status`
shows the last 20.

Routes:
- `GET /health`
- `GET /v1/models` (every name and alias, `loaded` or `unloaded`; `unlisted` entries are hidden)
- `POST /v1/*`
- `GET /router/status`
- `POST /router/unload`

## Configuration
The TOML format is compatible with the prototype's `router.toml`: every key it uses has the same
meaning. Unknown keys are an error.

`[router]` keys:
- `host` and `port`: default `127.0.0.1:18090`. The router binds loopback only unless
  `allow_non_loopback = true` is set.
- `logdir`: default `<config dir>/logs`.
- `docker`: the CLI used to verify and remove containers.
- `max_body_mb`: default 64.
- `shutdown_grace`: seconds that in-flight requests may run after SIGTERM, default 10.
- `upstream_read_timeout`: the longest an upstream may stay silent mid-request, default 3600 s.
  When it passes, the request fails with a 502 and its hold on the backend is released, so a
  wedged engine cannot block swaps forever.
- `gpu_lock` and `gpu_lock_timeout`: see below.

`[defaults]` sets any model key for every entry.

`[[models]]` keys:
- `name`, `aliases` and `engine`. `engine` is informational.
- `cmd`, which must stay in the foreground (`exec ...`).
- `proxy`.
- `health`: default `/health`. `"none"` means ready as soon as the process is started.
- `health_expect`.
- `stop`: `{pid}` is replaced by the process group id.
- `containers`.
- `upstream_model`: rewrites the request's `model` before it is forwarded.
- `ready_timeout` (default 600 s) and `stop_timeout` (default 60 s).
- `ttl`: idle seconds before an unload. 0 means never.
- `env`, `cwd`, `enabled`, `unlisted`, `notes` and `devices`.

`{dir}` in `cmd`, `stop`, `cwd`, `logdir` and `gpu_lock` is the config file's folder. A container
name that looks like production's (`noevia-*`, `*inference*`) is a config error, because the router
never removes those.

## Scheduling
- **Arrival order.** Requests are served in the order they arrive.
- **Sharing the loaded model.** A request for the loaded model goes straight through, unless an
  older request is waiting for a different model. In that case it queues behind that request, so a
  busy model cannot starve the others.
- **Batches.** Requests that were already queued when their model finished loading are served
  together.
- **Swaps.** A swap runs in its own task, so a client that disconnects mid-swap cannot leave it
  half done.
- **Failed starts.** When a start fails, every request that was queued for that model gets the same
  503. They do not each retry the whole start.
- **Client disconnects.** If a client disconnects, its request leaves the queue. If it was
  streaming, the router drops the upstream connection, which cancels the generation, and releases
  its hold on the backend.
- **TTL.** The ttl counts from the end of the last request and is checked with the queue lock held,
  so it never unloads a model that a request is about to use.

## Refusals
**A stop that leaves a container behind refuses the swap.** The next backend is not started while
the old one may still hold the GPU. The router reports the leftover names in `stuck_containers`
and keeps the GPU lock. Every later swap first tries to remove them again. A process group with
a member that is still alive 10 s after SIGKILL blocks starts in the same way
(`stuck_process_groups`). An unload that leaves either behind answers 503.

If docker itself cannot be asked (no daemon, no permission), a container counts as present, not
gone. A model with `containers` then refuses to start until docker answers.

Before a start, the router also verifies that the target's own containers are absent, for example
a leftover from an earlier crash.

## GPU lock
`gpu_lock = "/path/to/gpu.lock"` takes an exclusive `flock` on that file just before a backend
starts. It holds the lock until the backend is stopped and its containers are verified gone. If
another holder (such as an eval queue) has it, the start waits `gpu_lock_timeout` seconds (default
30), then refuses with a 503.

Don't point it at a lock that the router's own parent already holds. `flock` would then block on
that same lock.

## Prototype behaviours fixed in the port
1. **Leftover containers.** A stop that left containers was logged, but the next backend started
   anyway. The port refuses instead.
2. **Docker errors.** A failing `docker ps` counted as "container gone". It now counts as not
   verified.
3. **Production container names.** These were silently skipped during removal, so a stop looked
   clean. They are now a config error.
4. **Backends that die while loaded.** The prototype dropped such a backend without removing its
   containers. The port runs the full stop sequence and logs a `died` event, also when the model is
   idle.
5. **Starvation.** A steady stream of requests for the loaded model starved every other model
   forever. Scheduling is now first come, first served.
6. **Repeated failed starts.** Each request queued behind a failing start retried the whole start,
   each waiting up to its own `ready_timeout`. Now they share one attempt.
7. **TTL race.** The ttl check and the unload were not atomic, so a request that arrived between
   them had its model unloaded right after it finished, although it was no longer idle.
8. **Shutdown during a start.** A backend that was still starting when SIGTERM arrived was never
   stopped, and was orphaned with its container. Shutdown now cancels the start and stops it.
9. **Query strings.** These were dropped when proxying. They are now forwarded.
10. **Process groups.** The prototype did not check that the process group was gone after SIGKILL.
    The port refuses to start while a member is alive.

Other differences from the prototype: a wrong method on a known route gets a JSON 405, not a 404,
and trailing slashes are not stripped (`/v1/models/` is a 404).

## Tests
`tests/router.rs` runs the binary against fake backends (`tests/support/fake_backend.py`) and a
fake docker CLI (`tests/support/fake-docker`). It needs no GPU and no real containers.

The first 12 tests port the prototype's suite. The tiered-engine mock test runs only when
`STRATA_PY` and `STRATA_SRV` point at that engine's checkout.

The other tests cover:
- a client abort mid-stream;
- requests that arrive during a swap, and a shared failed start;
- a backend that never becomes healthy;
- a stuck container;
- ttl only after idle;
- the GPU lock;
- SIGKILL escalation;
- starvation;
- an upstream that hangs (the read timeout frees the lease);
- SIGTERM during a slow stop, which must not start the next backend.

## Before it could replace the production model path
- Per-request auth.
- `/metrics` passthrough.
- Request queue limits and a drain deadline.
- A policy for always-resident sidecars, which must stay outside the router.
- A package (image or service unit) with docker socket access.
- A translation of the production model presets into `[[models]]` entries.
- A side-by-side replay against the current endpoint.
- Tying backends to the router's lifetime. A router killed with SIGKILL leaves its backend running
  in its own process group, and a later start could pass its health check against that orphan.
