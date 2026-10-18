# Native section autoconfig read (noevia#1318)

The optional `native-section-autoconfig` Cargo feature (it includes `native-draft-heads`, whose
contained model-file reader and draft-head search it reuses) serves
`GET /api/v1/sections/{name}/autoconfig` from Rust. Its selector is
`MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL`, `python` by default. Startup refuses a value other than
`python` or `rust`, and `rust` without the compiled feature, as for every switch. It never refuses
startup over the settings: settings this side cannot read exactly as the child does keep the route
with Python and log one line
(`native-section-autoconfig: <reason>; GET /api/v1/sections/{name}/autoconfig stays with python`).
This document does not authorize activation.

Compose must pass the selector through, never as an empty value:

```yaml
MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL: ${MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL:-python}
```

## Ownership

- Rust answers only `GET /api/v1/sections/{name}/autoconfig`, after the token check, with the
  name segment percent-decoded as the child's server decodes it. Other methods, the slash
  variants, a name that decodes to a slash and every other section route keep their existing
  handling.
- The child gets `NOEVIA_NATIVE_MODEL_MANAGER_SECTION_AUTOCONFIG_CONFIRMED`, `1` only when the
  route is native (`0` otherwise, also over an inherited value), and then the same `MODELS_DIR`
  and `MODELS_INI_PATH`. Under `1` the services middleware refuses the route with 503, so it is
  never served twice, and serves the private call below (404 otherwise). The native front never
  forwards `/api/v1/internal/*`, so that call is reachable only from the parent.
- Fallbacks (one log line, the route stays with Python, the child gets `0`): a case-folded alias
  of `MODELS_DIR`, `MODELS_INI_PATH`, `LLAMACPP_CACHE_RAM_HARD_MAX_MIB` or
  `LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB` (the child's settings fold names); a `MODELS_DIR` that
  is not absolute and in pathlib's normal form (the projector and draft-head paths are mapped
  onto it as text); a `MODELS_INI_PATH` that is not an absolute file path; a cache-ram limit that
  is not a plain integer; any of these over 4096 bytes. The prompt-cache cap is the child's
  `settings.cache_ram_limits[0]`: `min(max(cap, 0), max(hard, 0))`.

## What is native and what stays in Python

Native, ported exactly and differential-tested:

- the query: every parameter, FastAPI's lax readings of integers, floats (including `inf` and
  `nan`) and booleans, the last value of a repeated key, and the 422 body with every error in
  declaration order;
- the system routing role check: `models.roles.json` (a missing, unreadable or unusable file is no
  roles, a repeated key keeps its last value) and the deprecated name/path rule, with the route's
  own 200 answer; the deprecated rule logs once per section;
- the section's model file: the explicit `model =` path, the folder layout (its first non-projector
  `.gguf`), the flat `{name}.gguf`, the "No model file found" answer, the header through the `gguf`
  crate with the "Could not read the model file" answer, and the size (the folder's model parts
  when the file sits in one);
- `budget_backends` (the VRAM budget less the largest prompt cache, the cards in proportion), the
  whole of `autoconfig.analyze` through the `model-autoconfig` crate (preparation and its refusal
  texts, the projector and draft head beside the model, the size plan, speculative decoding
  profiles, values, presets and presentation), `ini.get_section`, the models.ini revision
  (SHA-256 of the raw bytes), and the JSON, byte for byte (Python's float text, non-finite values
  as `null`, `ensure_ascii=False`).

Delegated to the Python child, never re-implemented, through the private
`POST /api/v1/internal/section-autoconfig-runtime` with `{"section": name, "model": model_rel}`:

- the telemetry: `telemetry.ingest` over the backend logs, then the section's `stats_for` and
  `config_history` (a failure there is empty measurements, as in the route);
- the backend inventory: `helpers._backend_list()` (the GPU sampler, Docker image tags and
  commands), whose failure is the route's plain 500.

**Step 3b (noevia#1318).** While this process samples the backends
(`MODEL_MANAGER_BACKENDS_IMPL=rust`), the route uses its own `_backend_list()` instead of the
list in the child's answer.

- **The list:** the native descriptors (`fit::descriptors`), each with `baseline` as
  `parse_baseline(cmd) if cmd else {}`. The parse goes through the `model-autoconfig` crate over
  `inventory::KNOWN_KEYS` (`sorted(ini.ALL_KNOWN_KEYS)`).
- **Failures:** the child still lists the backends itself, so a failure there stays Python's 500.
  A container command this side does not render, or a baseline the rule refuses, keeps the
  child's list for that answer. Docker inventory work has a 10-second deadline; after a timeout,
  optional inventory reads on that sampler keep the child's answer for 10 seconds before retrying.
  This cooldown holds only a monotonic retry deadline, never a previous inventory. Failures are
  logged at the first, tenth, hundredth and subsequent powers-of-ten occurrence per reason.
  The authoritative native-telemetry path below still refuses on inventory failure and does not
  use this child-fallback cooldown.
- **Differential:** `tools/gen-fit-oracle.py` records Python's whole list (baseline included) and
  the known keys, and `section_autoconfig::inventory::tests` compares both.

**Step 3d (noevia#1318).** With the `native-telemetry` feature and
`MODEL_MANAGER_TELEMETRY_IMPL=rust` (default `python`), the route no longer asks the child at all:
this process ingests the backend logs, reads the telemetry tables and builds the backend list.

- **One writer per table.** The selector is decided before the child starts. Under `rust` the
  child is started with `NOEVIA_NATIVE_MODEL_MANAGER_TELEMETRY_CONFIRMED=1`, its autoconfig route
  is not forwarded to and its runtime call is never made, so `req_timing`, `server_config` and the
  `telemetry_*` keys of `kv` have this process as their only writer. Under `python` the child is
  told `0` and nothing here writes them.
- **The child's guard.** Under the confirmation the child's `telemetry.ingest`,
  `db.record_timings` and `db.record_server_configs` write nothing (services#98), and it answers
  the private `GET /api/v1/internal/telemetry-guard` with `{"telemetry_guard":1}` (services#100).
  This process ingests only after that answer. A child image without the guard answers 404
  there; this process refuses native telemetry with 503, writes none of the tables, and logs
  that once. It cannot safely fall back: services#98 already disables its writer under the
  confirmation even though it lacks the advertisement (#2041). (The child refuses
  `GET /api/v1/health`, which this process owns, so the advertisement is a private route
  rather than a health field.) The answer is kept for the life of the process: the child is
  never respawned (its exit ends this process), so its image cannot change under it. `rust` needs `MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL=rust`
  and `MODEL_MANAGER_BACKENDS_IMPL=rust`; without them, or without the compiled feature, startup
  is refused rather than downgraded.
- **Ingest semantics (`telemetry.ingest`, kept exactly).** At most once per 20 s
  (`telemetry_last_ingest`). For each effective backend name: the Docker log tail
  (`tail=20000`, stdout and stderr, timestamps; multiplexed frames demultiplexed as docker-py
  does when the container has no TTY; decoded with replacement), then `parse_log` from the
  `backend-telemetry` crate with the per-backend watermark `telemetry_log_watermark:<name>` and
  continuous-span key `telemetry_continuous_since:<name>`. The tail is gap-free only when its
  first line is at or before the watermark; only then is the watermark passed on, so a respawn on
  the same port is attributed by the previous spawn line and a gap starts a new span. Rows with
  no generated tokens or no timestamp are dropped. A backend whose read fails is skipped; a
  store write that fails (an interrupted write) skips the rest of that backend and leaves its
  keys unchanged, so the next pass re-reads the same lines and the unique indexes keep it
  idempotent. A parse that raises ends the ingest, and the route answers empty measurements, as
  Python's `except`. The SQL is `db.py`'s, each call its own transaction, WAL, 5 s busy timeout.
- **Bounds (that backend skipped, as Python skips a failed read):** a log answer over 64 MiB, a
  log read over 60 s, more than 40,000 lines or 4,096 distinct ports in one tail. Nothing of that
  backend is written and its keys stay; the other backends are ingested and the route answers.
  It is logged once per backend and reason and not read again for 10 minutes, so a huge log is
  not re-read on every request. Python would read such a tail; here it is left out of the
  measurements until it is back within the bounds.
- **Refused (503, never guessed), only where rows could be written wrong:** a stored value of a
  type Python would not have written, a log shape the parser does not support, or a backend
  list this side cannot build.
- **Rollout order.** 1. Deploy services#98 (and #100). 2. Confirm the deployed child image has
  the guard (`GET /api/v1/internal/telemetry-guard` from the parent's side answers 200 under the
  confirmation). 3. Deploy this build with `MODEL_MANAGER_TELEMETRY_IMPL` still `python`. 4. Set
  `MODEL_MANAGER_TELEMETRY_IMPL`, `MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL` and
  `MODEL_MANAGER_BACKENDS_IMPL` to `rust` together, then restart. Out of order, step 4 against
  an old child makes native telemetry refuse with 503 and no native writes. To restore
  child ingestion, select `python` and restart so its confirmation is reset to `0`.
- **Differential:** `tools/gen-telemetry-ingest-oracle.py` runs the actual `telemetry.ingest`,
  `stats_for` and `config_history` over fourteen passes (first read, the interval, repeated reads,
  a restart, port reuse, a gap, an interrupted write and its recovery, a raising parse, a
  multiplexed log with invalid UTF-8, unreadable stored keys, ports A, B, A answered from the
  stored-instance cache) and records the tables, the keys and
  the rendered measurements after each; `section_autoconfig::telemetry::tests` replays every pass
  on the same schema and compares them.

The child answers `{"measured": ..., "history": ..., "promptTps": "<repr>", "backends": "<json>"}`:
`measured` and `history` rendered as the route renders them and placed verbatim, the prompt rate
as Python's float `repr`, and the backend list as JSON text that keeps non-finite floats. The
parent asks only once the file part found a model, as Python ingests only then. A child 500 is
FastAPI's plain 500; a refusing, missing or oversized (over 16 MiB) answer is the forwarded
route's `502 {"detail":"Python-owned endpoint unavailable"}`; an answer of the wrong shape
refuses (below).

`MODEL_AUTOCONFIG` (the child's choice between its Python analysis, the default, and the
crate's binary) is not read here: the native route always answers the crate's plan. That is
justified because the crate matches Python's default analysis: the oracle records the child under
its default, and every native answer equals it byte for byte. The one behaviour that differs is
the child's `MODEL_AUTOCONFIG=rust` mode, where Python computes its own plan, has the crate confirm
it, and fails closed with an error when the two disagree; the native route has no second plan to
compare with and answers the crate's.

## Bounds and deliberate differences

The route answers `503 {"detail":"Native section autoconfig is unavailable"}` instead of guessing
for:

- a query value whose reading is uncertain here (whitespace around a number, underscores, a
  fraction on an integer, more than 18 digits);
- a model file or folder that leaves the models folder through a link (Python would follow it), a
  file that vanishes between the check and the read, or a read error (Python's error text is not
  reproduced);
- a saved projector or draft-head path that is not under `/models` (a relative `mmproj =` is
  resolved by Python against its own working directory, which this process cannot reproduce; the
  oracle pins Python's 200 and the native 503 for it);
- a header past the reader's bounds, a non-finite float in the header, or a header the summary
  step refuses;
- a roles file over 1 MiB, or a models.ini the section parser refuses;
- any input where the crate refuses but Python could still answer (the crate's own refusals);
- a child answer of the wrong shape;
- an answer over 8 MiB;
- a blocking part (including waiting for one of the two read slots) running past 60 s.

The blocking parts (the file part, then the analysis) each take one of two read slots and run
under the request's admission permit; a request dropped mid-read keeps both until the read
returns. The child call has a 120 s timeout (its ingest reads up to 20,000 log lines per backend).

Wire texts the route answers with (the system routing role answer, the backend refusals) are kept
byte for byte, including the product names they mention.

## Oracle

`tools/gen-section-autoconfig-oracle.py` runs the actual services FastAPI app on synthetic model
folders, models.ini and roles files and GGUF headers, with the live sources (the telemetry and
the backend inventory) replaced by deterministic stand-ins, and records each answer of the route
plus, for every request that reaches the analysis, the request the parent makes and the child's
real answer to it under the confirmation. It writes
`tests/fixtures/section-autoconfig-python.v1.json`; the `native-section-autoconfig` CI job
regenerates it from the pinned services ref with Python 3.12 and requires identical bytes. The lib
tests rebuild every case through a synthetic root on any platform with a loopback child stand-in;
`tests/section_autoconfig.rs` runs the same cases through the router and the real opener on Linux,
and checks each runtime request (path, body and the token header).
`tests/section_autoconfig_lifecycle.rs` checks the selector at process level.
