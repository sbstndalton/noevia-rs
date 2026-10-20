# Native model-manager: bounded read capabilities

Refs noevia#1319, #1330, #1334 and #1318. This binary owns `GET /api/v1/health` and
`GET /api/v1/backends/{name}/logs`, with separately selected host and section read
capabilities described below. Unselected endpoint/method pairs remain Python-owned.
Full S1 remains open.

The native parent is permanent (noevia#1071): there is no Python-only mode and no uvicorn
exec on the public port. `MODEL_MANAGER_IMPL` and `MODEL_FILES_IMPL` are retired and not
read (noevia#2011). The binary requires the
`native-logs` compiled feature (a build without it refuses at startup) and valid configuration
before any child starts. Rust supervises one existing Python app on fixed private loopback
port 8091; Python retains its existing DB/seed/reap/sampler/INI startup for unported work.
The native process does not duplicate that business startup. The corresponding services
patch refuses health/logs in the Python child via `NOEVIA_NATIVE_MODEL_MANAGER_CONFIRMED=1`.
Rust-owned errors never proxy to Python. The public administrator/session/tenant gate
remains in the current public server; the service independently owns its shared-token gate.
Activation and release pins belong to the owner and primary; this source change activates none.

The service reads Docker only through a Unix socket (default `/var/run/docker.sock`).
It never offers Docker writes, exec, model loads, inference, downloads or deletion.
Bollard's model/query types are reused. Its aggregate JSON reader and log codec buffer
untrusted responses/declared frames before yielding, so bounded native Hyper ingress and
explicit frame decoding replace those codecs. No extra HTTP proxy is inserted.
Only exact GET list/inspect/image-inspect/log operations are admitted. First image RepoTag
is used for discovery, including stopped containers; explicit configured names remain allowed.

Limits: four concurrent HTTP requests, five seconds per native logs request, 600 seconds
for Python-owned traffic (the existing public proxy budget), 8 KiB URI and Docker
header parser buffer, 32 Docker headers, 1 MiB Docker response before parsing, 1,024
containers (a bounded serde visitor refuses a sentinel before growing its inventory), 256-byte container names,
1 MiB log input, 2 MiB redacted output, 4,000 returned lines, and 4 KiB search text.
Python-owned request/response proxy bodies are capped at 1/16 MiB. Docker errors are
sanitized; redirects, ambiguous framing, invalid/truncated frames and cap breaches refuse.
These caps and Unix-only transport are deliberately stricter admission than Python.

Redaction precedes searching and level filtering. Regex matching is linear and uses no
lookarounds. Redaction is deliberately broader for numeric credentials, embedded token
shapes and Unicode word boundaries; established `n_*`, `tokens`, `token_count` and numeric
session counters retain their old text. Python's additional ignore-case I variants are
explicitly admitted. Non-ASCII search filters are refused because contextual Unicode
lowercasing differs; UTF-8 replacement decoding and all Python splitlines delimiters remain.
Tail accepts bounded ASCII integer syntax (including arbitrarily large magnitudes that
clamp), with zero mapped to 400; invalid syntax returns a sanitized 422 detail rather than
FastAPI's input-echoing validation object.

Qualification uses the actual pinned Python `api.py` SHA
`eb8e12f6929baf15d2b1a1998aceff9529fbe48ca8242094ac6b7526c253a91d` from services
`ff8f314d7496e83d3b2d229089f8aabb4ddf67f9`. `generate-oracle` evaluates only the actual
AST redaction/log functions with synthetic in-memory collaborators, without importing the
app or invoking startup. CI regenerates its exact fixture. Properties cover secret probes
and hostile frames; tests use only owned Unix fake peers and a test-only uvicorn executable.
Both compiled-feature variants prove selector/child behavior. No live Docker or model activity.

The services Docker build/pin change follows a reviewed, merged Rust SHA; the existing
services default Rust pin does not contain this binary. This Rust PR only references #1319;
the linked services integration must qualify before closing it.

Unicode hardening (#1325): regex word classes explicitly use Python letter/number/underscore semantics and whitespace includes U+001C–U+001F. Counter exemptions require single-word ASCII letter/digit/underscore keys. AWS access-key prefixes are scrubbed even before word-character suffixes. The bounded payload is scrubbed before Python-compatible line splitting, then each line again; this intentionally redacts more than Python when control whitespace separates keys and values across lines. Adversarial fixtures retain actual Python output for comparison and assert the stricter native no-secret output and empty secret-query result.

Host telemetry is native and has no selector: `native-routing` compiles `native-host`
(which includes `native-logs`). The former `MODEL_MANAGER_HOST_IMPL` selector is retired and
ignored. The native owner passes `NOEVIA_NATIVE_MODEL_MANAGER_HOST_CONFIRMED=1` to its
supervised child, which refuses the host endpoint and skips host sampling while retaining
backend stats.

The native sampler reads only fixed Linux `/proc/stat` (first line) and
`/proc/meminfo`, with 64 KiB input caps, at most 64 CPU fields and bounded
integers. It ticks every two seconds, retains 450 points and returns the latest
point and last 180. Its 250 ms read budget stops the sampler on a stalled read;
shutdown aborts the sampler, with at most one bounded filesystem operation
outstanding. Ordinary read/parse failures skip a point; CPU state advances before
a memory failure as in the actual Python source. Synthetic differential fixtures
are regenerated by `generate-host-oracle`; `check-host-mutations` verifies four
behavioral regressions are caught and restores each modified source before
checking the result. Run that tool only in an isolated checkout.

## Section configuration reads (#1334)

`native-routing` compiles `native-sections`, which owns exactly GET `/api/v1/sections` and
GET `/api/v1/sections/{name}`. There is no selector; the former `MODEL_MANAGER_SECTIONS_IMPL`
selector is retired and ignored. The child receives
`NOEVIA_NATIVE_MODEL_MANAGER_SECTIONS_CONFIRMED=1` and refuses only those two GET
shapes after authentication. INI writes, backup creation, actions, startup migration,
backend stats, jobs and DB ownership remain Python. The parent overwrites any inherited
confirmation value.

Reads use a single bounded INI byte snapshot for both raw text and its SHA256
revision. This intentionally strengthens consistency during replacement; there is
no snapshot guarantee across INI, model headers and backups. Ordered sections,
DEFAULT expansion, CLI flags, schema, companion discovery and metadata hints are
qualified against the actual services source using 93 complete responses and 17
FastAPI method/query/redirect responses. Rust generates cases and synthetic files;
a fixed Python loader evaluates actual source functions without app startup.

Production file access requires Linux `openat2` with BENEATH and NO_MAGICLINKS
relative to retained original configured root descriptors. Configured directory
roots and traversed directories reject final symlinks; relative in-root file
symlinks are permitted. Absolute symlinks, outside traversal, unsupported systems,
nonregular model/INI files and refused access return sanitized 503
`{"detail":"Native section configuration is unavailable"}`. Files open nonblocking
and are checked before content reads, so FIFOs/devices are refused. Hard links and
already opened inodes retain their filesystem meaning; root descriptors do not
provide cross-file snapshots or interrupt OS syscalls. Ordinary acquisition or INI
parse errors also conservatively refuse 503 rather than exposing Python errors.

Per request limits: 2 seconds of cooperative work, two retained blocking operations,
1 MiB INI, 4,096 sections, 65,536 effective keys, 64 KiB individual values,
16,384 enumerated entries counted before filtering, depth four, 4,096 GGUF files,
1,024 backups, 4 KiB paths, 256-byte names and 8 MiB JSON output. DEFAULT expansion
and JSON string reservations are checked before growing aggregates; JSON writing
is capped while serializing. Cancellation/deadline retains each admission permit
until its blocking operation finishes, preventing replacement work from exceeding
two outstanding operations. Deadlines cannot terminate an OS filesystem syscall.

Metadata reads reuse the existing GGUF parser through an additive per-call limits
API. Existing default APIs and limits are unchanged. Sections retain the 64 MiB
input read budget and tighten to 4,096 KV entries, 32,768 retained values, 64 KiB
individual string bytes, 4 KiB key bytes, and a 4 MiB conservative decoded UTF-8
string reservation including keys (three times encoded bytes). Budget refusals are
distinct from malformed metadata, which keeps the qualified ordinary hint. These
component bounds are not a 16 MiB heap claim: vector/hash capacities, value storage,
parser buffers and summary clones allocate additional bounded memory. No weight
payload reads, model loads or inference are performed.

Rope hint text reuses the existing GGUF Python text formatter through a bounded
wrapper, with preflight limits of 64 KiB text, 1,024 values and eight nesting levels.
Scalar strings retain exact Unicode text and Python control-character whitespace
is respected. Complex text admits ASCII strings only: unsupported complex Unicode
metadata returns the same sanitized 503 as other admitted-dialect refusals. This
does not claim the existing partial formatter implements all Python Unicode repr.

Reviewed compatibility repairs (#1342–#1346) keep sibling defaults and companions
on pathlib suffix semantics (bare `.gguf` excluded, uppercase extensions accepted)
while retaining discovery's separate endswith rule. Positive and negative bounded
GGUF Big integers preserve existing decimal context/default/hint semantics, including
finite f64-derived values beyond i128. Decoded route matching precedes query
validation while token authentication remains first. Capability output uses fallible
complete writes and flush; a closed stdout returns an ordinary error before startup.
Both fixed-port child lifecycle tests share one test-only admission lock.

Linux CI qualifies containment and atomic replacement races with owned synthetic
files; macOS runs semantic fixtures through a test-only adapter and explicitly
refuses production access. Properties, cancellation/admission controls, no-mutation
checks and assertion-reaching policy mutations qualify the source. No real models,
Docker services or production data are used.

## Native routes and retired selectors

`native-routing` is mandatory (noevia#1071) and compiles the public routes below. For each of them the native path is the only path. The `MODEL_MANAGER_*_IMPL` selectors that once chose Python or Rust per route are retired: the executable ignores them, hostile values included, so no environment setting sends a route back to Python. The Cargo features named here are compile-time parts of `native-routing`, not runtime switches. The service requires exact `TZ=UTC` before startup and refuses otherwise. Each linked document gives the route's contract and qualification boundary.

## Native prompt library

`native-prompts` moves whole prompt CRUD and once-only seed ownership together; the former `MODEL_MANAGER_PROMPTS_IMPL` selector is retired and ignored. See [native prompt contract and qualification boundaries](docs/native-prompts.md) for caps, actual Python oracle provenance, trusted DATA_DIR requirements and the services handoff. Public Node proxy policy remains Node-owned.

## Native backend diagnosis

`native-settings` serves the read-only `GET /api/v1/settings` (`{hasToken, tokenHint}`); the former `MODEL_MANAGER_SETTINGS_IMPL` selector is retired and ignored. The write is the `native-settings-write` route below. See [native settings read](docs/native-settings.md).

`native-model-updates` serves the read-only `GET /api/v1/models/updates` (the models list's update-status badges); the former `MODEL_MANAGER_MODEL_UPDATES_IMPL` selector is retired and ignored. Every writer of the update store stays Python. See [native model update status](docs/native-model-updates.md).

`native-model-detail` serves the read-only `GET /api/v1/models/detail`; the former `MODEL_MANAGER_MODEL_DETAIL_IMPL` selector is retired and ignored. The file fields are native; the live `loadedOn` and `fit` fields are asked of the Python child through a private call, never re-implemented. See [native model detail](docs/native-model-detail.md).

`native-model-list` serves the read-only `GET /api/v1/models` and `GET /api/v1/overview` (always native; the `MODEL_MANAGER_MODELS_IMPL` and `MODEL_MANAGER_OVERVIEW_IMPL` selectors are retired and ignored, noevia#1071). The file, ini, badge and disk fields are native; the live `loadedOn`, `fit`, `backends` and `activeDownloads` fields are asked of the Python child through private calls, never re-implemented. Settings it cannot read exactly refuse startup. See [native model list and overview](docs/native-model-list.md).

`native-section-autoconfig` serves the read-only `GET /api/v1/sections/{name}/autoconfig` (always native; the `MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL` selector is retired and ignored, noevia#1071). The query, the routing role check, the model file, its header and the whole analysis are native, and so are the live telemetry and the backend inventory; the private Python child call that carried them before noevia#2055 survives only in library tests. Settings it cannot read exactly refuse startup. With `native-telemetry` (the `MODEL_MANAGER_TELEMETRY_IMPL` selector is retired and ignored) the telemetry ingest and store are native too and no child call is made. The shipping binary starts no child (noevia#2055), so there is no child telemetry guard to check: this process is the only telemetry writer, and its lifetime writer lease on `DATA_DIR` keeps a second manager off the store. The former guard probe and its 503 went with the child (noevia-rs#256). See [native section autoconfig](docs/native-section-autoconfig.md).

`native-download-targets` serves the read-only `GET /api/v1/download-targets`; the former `MODEL_MANAGER_DOWNLOAD_TARGETS_IMPL` selector is retired and ignored. `POST /api/v1/downloads` stays Python and checks its target against the same settings. See [native download targets](docs/native-download-targets.md).

`native-badges` serves `PUT` and `DELETE /api/v1/badges`, with the noevia#1905 fixes (a non-integer rating or run id answers 400, and padded aliases are stripped once); the former `MODEL_MANAGER_BADGES_IMPL` selector is retired and ignored. See [native badge writes](docs/native-badges.md).

`native-bench-runs` serves the read-only `GET /api/v1/benchmark/runs/{id}`; the former `MODEL_MANAGER_BENCH_RUNS_IMPL` selector is retired and ignored. The store part and the charts are native; `capacity_gb` and `vram_predicted` come from the Python child's private runtime call. See [native benchmark run read](docs/native-bench-runs.md).

`native-section-writes` makes the Rust front the only models.ini writer (section save, rename, delete, safe defaults, the raw editor); the former `MODEL_MANAGER_SECTION_WRITES_IMPL` selector is retired and ignored. Safe-defaults values come from the Python child's private read-only plan call. See [native models.ini writers](docs/native-section-writes.md).

`native-settings-write` serves `PUT /api/v1/settings` (the Hugging Face token); the former `MODEL_MANAGER_SETTINGS_WRITE_IMPL` selector is retired and ignored. The token test is asked of the Python child's private call. See [native settings write](docs/native-settings-write.md).

`native-backend-control` serves `POST /api/v1/backends/{name}/restart` and `/test`; the former `MODEL_MANAGER_BACKEND_CONTROL_IMPL` selector is retired and ignored. Restart errors are kept by the native sampler, which `GET /api/v1/backends` answers. See [native backend restart and test](docs/native-backend-control.md).

`native-benchmark-overview` serves `GET /api/v1/benchmark`; the former `MODEL_MANAGER_BENCHMARK_OVERVIEW_IMPL` selector is retired and ignored. The job slot and its controls are native through `native-benchmark-runner` below. See [native benchmark overview](docs/native-benchmark-overview.md).

`native-check-updates` serves `POST /api/v1/models/check-updates`; the former `MODEL_MANAGER_CHECK_UPDATES_IMPL` selector is retired and ignored. The Hub tree listing is fetched and `update_check` is written here. See [native model update check](docs/native-check-updates.md).

`native-benchmark-runner` moves the whole benchmark job slot (it needs the native overview); the former `MODEL_MANAGER_BENCHMARK_RUNNER_IMPL` selector is retired and ignored. It covers prompt runs, llama-bench sweeps through the named sweep Docker allowlist, start/sweep/progress/cancel, and the startup cleanup of leftover sweep containers. See [native benchmark runner](docs/native-benchmark-runner.md).

`native-diagnose` serves the whole internal backend diagnosis GET; the former `MODEL_MANAGER_DIAGNOSE_IMPL` selector is retired and ignored. See [native diagnosis contract](docs/native-diagnose.md) for current-run acquisition, bounded Docker framing, retained driver joins, and the separate services handoff.

`native-downloads` owns the download job table: list, start (planned by the Python child), cancel, clear and the overview's active count; the former `MODEL_MANAGER_DOWNLOADS_IMPL` selector is retired and ignored. See [native downloads](docs/native-downloads.md).

`native-model-delete` serves `POST /api/v1/models/delete` (the container check and every deletion); the former `MODEL_MANAGER_MODEL_DELETE_IMPL` selector is retired and ignored. See [native model delete](docs/native-model-delete.md).

Benchmark chart capacity and prediction come from the native autoconfig service and sampler; see [native chart runtime](docs/native-bench-chart-runtime.md).

`native-db-init` moves full schema bootstrap onto the secure prompt-store connection; the former `MODEL_MANAGER_DB_INIT_IMPL` selector is retired and ignored. See [schema bootstrap ownership](docs/native-db-init.md).
