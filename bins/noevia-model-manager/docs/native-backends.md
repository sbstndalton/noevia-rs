# Native backend sampler and `GET /api/v1/backends` (noevia#1318 slice 13)

`native-backends` with `MODEL_MANAGER_BACKENDS_IMPL=rust` moves the backend sampler and `GET /api/v1/backends` to the native parent. The default is `python`. The confirmed child (`NOEVIA_NATIVE_MODEL_MANAGER_BACKENDS_CONFIRMED=1`) then:

- refuses `GET /api/v1/backends` with 503;
- no longer samples the backends. Its sampler thread only reads the host when `MODEL_MANAGER_HOST_IMPL` is still `python`, and is not started when both are native;
- takes the native samples into its backend cache through the private `PUT /api/v1/internal/backend-stats`. Its other readers keep working unchanged: the fit chips, autoconfig, the benchmark observer, and `vram_gb_for`/`gpu_count_for`/`card_vram_gb_for`;
- runs the GPU reads that need `docker exec` when the parent asks, through the private `GET /api/v1/internal/gpu-exec-probe?name=&probe=nvidia|amd|amd-nvidia`.

The native sampler stays with Python, with one log line and no refusal to start, when:

- `MODEL_MANAGER_BACKEND_CONTROL_IMPL` is not `rust`. The `last_restart_error` the answer shows is kept by the native restart.
- `TZ` is set to a zone other than UTC. `started_at` is local time; the child is given `TZ=UTC` too.
- A case-folded spelling of `GPU_VRAM`, `LLAMA_CONTAINERS` or `LLAMA_DEFAULT_PORT` is set. The child's settings fold names.
- `GPU_VRAM` holds a value this side cannot read exactly (non-ASCII digits).

Only the exact `GET /api/v1/backends` is native. Other methods and the slash variant reach the child: FastAPI's 405 and its redirect. The per-backend routes keep their own switches.

## What runs where

| Part | Python (`hw.py`, `services.py`) | Native |
|---|---|---|
| Effective backends | `_effective_container_names`: LLAMA_CONTAINERS, then every container whose first image tag (`<none>:<none>` skipped) contains `llama.cpp`, never `model-loader` | the same, from `/containers/json` plus inspect and image inspect of each |
| Docker state | `containers.get`, `container.image`, `c.status`, `c.short_id` | `/containers/{name}/json`, `/images/{hex}/json` |
| GPU, AMD sysfs | `_read_drm` (cards mapped into the container, `/sys/class/drm`) | the same files, read by the native process (same container, same view) |
| GPU, `docker exec` | `_read_nvidia`, `_read_amd` | **asked of the child** (`hw.exec_probe`) |
| GPU_VRAM declaration | `_read_vulkan`'s last fallback | the same |
| Container figures | `container.stats(stream=False)` | `/containers/{id}/stats?stream=false` |
| History | 450 points per backend, pruned when a non-empty discovery no longer lists it | the same |
| Probes | `_probe_loaded_model` (`/v1/models`, 2 s connect, 3 s read) | the same (`backends::probe`) |

The native Docker reads are GET only, through `docker::allowed_backend_read`: `/version`, the container list, container inspect, image inspect, and one-shot stats. Each path segment is a Docker name, id or digest that starts with a letter or digit. There is no exec, create, start, stop or kill. Running `nvidia-smi`/`rocm-smi` natively would need `docker exec`, which the native allowlist forbids. That stays in Python until the owner decides otherwise (see below).

## Behaviour reproduced
- **Sampler tick (`_sample_loop`, every 2 s):**
  - For each effective name: `docker unreachable` (no `/version`), `container not found`, `APIError: <docker-py's text>`, or `container is <status>` for a stopped container. Otherwise `ok` with the GPU and container figures.
  - The vendor comes from the image tag (rocm/hip, cuda/nvidia, vulkan) and is kept per name for the life of the process, as `_VENDOR_CACHE` is.
  - History only for `ok` samples.
  - The prune removes names with history that a non-empty discovery no longer lists, and their cache entries. A name that never sampled `ok` keeps its cache entry, as in Python.
- **Numbers:**
  - `round()` rounds ties to even on the exact value, and `round(x)` gives an int (`clock_mhz`).
  - `sum()` of floats uses Python 3.12's Neumaier compensation.
  - `max()` keeps the first maximum and its int or float type.
  - int/int is true division; `int()` and `float()` follow Python's rules on the Docker stats JSON.
  - A type error Python catches gives no container figures. One it does not catch ends the sample.
- **docker-py's errors:** `"<status> Client|Server Error for http+docker://localhost/v<ApiVersion><path>: <reason>"` plus ` ("<message>")`. A 404 is `NotFound`; in the GET that also covers a missing image, which leaves `found: true`, no image and `probe_error: "port unknown"`.
- **GET:**
  - `docker unreachable` for every effective name when `/version` fails. Otherwise each backend's Docker state, `started_at` in `%Y-%m-%d %H:%M` (UTC), `uptime`/`uptime_s` while running, ports, and the probe.
  - `unreachable_but_healthy` for a refused or timed-out probe of a container Docker reports healthy.
  - Then the cached figures (`warming up…` before the first tick) and the last 120 history points.
  - The JSON is FastAPI's: compact, non-ASCII as is, Python float spelling. A non-finite float is FastAPI's 500.

## Deliberate differences
Python crashes on some Docker or sysfs data; this side does not reproduce the exception text:
- Sampler: a stats body that is not JSON, a container without `Image`, an odd `State` or `HostConfig`, sysfs text that is not UTF-8 or longer than 4 KiB. These give `sampler error: unexpected Docker or sysfs data` instead of Python's exception message.
- GET: an inspect or image answer of a shape Python would 500 on, or a `StartedAt` that is not Docker's RFC 3339 `...Z` (Python's `fromisoformat` accepts more). These answer the native 503.

Other differences:
- More than 1024 containers on the daemon: discovery lists none, as the native list's sentinel refuses (Python lists them all).
- A child that does not answer a GPU read: no GPU figures for that tick, with one log line per failing run. Python reads them itself.
- A tick is bounded to 60 s; Python's is not.

## Owner decision (not taken here)
Moving `nvidia-smi`/`rocm-smi` into the native process means allowing `docker exec` of two fixed commands into effective backends. The native Docker allowlist forbids exec today. Until the owner decides, those reads stay in the Python child, called per tick. On the owner's AMD APU with the Vulkan image, every figure comes from sysfs and no exec is needed.

## Oracle
`tools/gen-backends-oracle.py` runs the actual services app (pinned in CI) against:
- a fake Docker daemon on a Unix socket, reached through docker-py, so docker-py's object model, errors and messages are the real ones;
- fake llama-servers on loopback;
- a fake `/sys/class/drm` in a temporary folder;
- `docker exec` answered at docker-py's `Container.exec_run`;
- pinned `time.time`, `datetime.now` and `TZ=UTC`.

There are 9 cases: Vulkan on an APU from sysfs, configured names and ROCm with its sysfs fallback, NVIDIA with equal, mixed and failing cards, Vulkan fallbacks to exec and to `GPU_VRAM`, Docker errors (image 404 and 500, inspect 500, stats 500), Docker down, probe outcomes and uptimes, 125 ticks of history with the vendor kept and a prune, and container-stats edges. The fixture records each tick's cache (what the push carries), the GPU reads the child would answer, and each GET's status and body.

The lib test replays every case on every platform with a fake Docker API. The integration test replays five cases through the router and the real `UnixDocker` transport against a fake daemon on a Unix socket, and checks that only allowlisted read-only GETs reached it. CI regenerates the fixture and compares it byte for byte.

## Fit primitives (noevia#1318 step 1)

`backend_stats::fit` ports what the model reads judge a file size against, reading this
process's own sampler cache (the values it pushes to the child, which `hw.stats_for` answers
there) and the sampler's read-only Docker discovery:

| Python | Native |
| --- | --- |
| `services._fit_backends` | `Inventory::gpu`: a positive `GPU_VRAM` declaration, else the sampled `vram_total_gb` of an `ok` sample |
| `services._cpu_backends` | `Inventory::cpu`: discovered backends whose first image tag names no GPU vendor (`cpu`, `unknown`) and whose sample has no VRAM |
| `hw.host_ram_gb`, `hw.usable_ram_gb` | `fit::host_ram_gb` (`MemTotal` of `/proc/meminfo`), `fit::usable_ram_gb` less HOST_RAM_RESERVE_GB |
| `services.vram_fit_chips` | `fit::chips`: the machine-level `impossible` chip, then the VRAM chips (65 % / 90 %), then the CPU chips (100 %, 30 GiB, 75 %) |
| `helpers._backend_list` (without `baseline`) | `BackendStats::descriptors`; `fit::budget` is the search budget |

Thresholds, ordering, `round()` (ties to even, exact integers however large) and `round(x, 1)`
are Python's. Differential-tested against the pinned services app by `tools/gen-fit-oracle.py`
(`tests/fixtures/fit-python.v1.json`): fit boundaries, CPU-only, insufficient RAM, unreadable
meminfo, an impossible machine, and empty and mixed stores.

Deliberate differences: Python rediscovers the containers up to four times for one chip list
(and again for every model of a list); this side makes one Docker discovery per answer.
HOST_RAM_RESERVE_GB is read as a strict subset of what pydantic reads: a plain decimal under its
exact name. Other forms, including some pydantic accepts, leave the inventory unavailable
(`None`), so a reader refuses rather than guessing the reserve. A case-variant key (for example
`host_ram_reserve_gb`), which the child's settings would also read, makes the chips refuse too.
The oracle records pydantic's own reading of every accepted form, and the native value must
equal it. A container command holding anything but strings refuses the descriptors.
