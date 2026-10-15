# HTTP contracts (full-Rust migration M0)

- `routes.toml`: every `/api/` path the web client (noevia-web `src/`, tests excluded, at the
  commit in `web.ref`) names, with the server that owns it. All `owner = "node"` until a Rust
  slice takes a route over. CI job `routes-check` fails when the client names a path missing here
  (`tools/contracts/routes_check.py`); paths are matched by shape, so `{id}` and `{chatId}` are the
  same parameter. Methods are not listed yet.
- `corpus/`: a synthetic recorded corpus. `exchanges/NNNNNN-METHOD-path.json` are request/response
  pairs written by noevia-core's opt-in recorder (`NOEVIA_CONTRACT_RECORD`,
  `server/contract-record.cjs`) while `tools/contract-corpus/generate.cjs` drove a server on a
  throwaway data dir with a mock model and Diary. Secrets are `<secret:N>`, ids `<id:N>`,
  timestamps `<ts>`; the same value has the same placeholder throughout. `manifest.json` names the
  inputs a replay must supply (the setup code file, the synthetic passwords). Refresh it with
  `tools/contracts/refresh-corpus.sh <noevia-core run id>`.

- `core.ref`: the noevia-core commit the CI job `corpus-replay-node` boots to prove the corpus
  replays clean against Node.

Replay against Node booted the way the corpus was recorded (noevia-core
`tools/contract-corpus/serve.cjs`: the same mock model and Diary, outbound connections refused,
`UI_DATA_DIR` = a fresh copy of the seed):

```sh
mkdir -p "$PWD/target/empty-seed"
cargo run -p replay -- run --corpus contracts/http/corpus --base http://127.0.0.1:18021 \
  --seed "$PWD/target/empty-seed" \
  --server-cmd "exec node <noevia-core>/tools/contract-corpus/serve.cjs" \
  --state-out target/node-state.json
cargo run -p replay -- coverage --corpus contracts/http/corpus --routes contracts/http/routes.toml
```

A Rust server is replayed the same way with its own `--server-cmd` (it must answer the same mock
model and Diary), and `--expect-state target/node-state.json` compares the data dirs it leaves.

## M1: the Rust front (`bins/noevia-server`)

`noevia-server` compiles `routes.toml` in (build.rs) and answers the `owner = "rust"` routes
itself: `GET /api/ready` and, through the `catch_all` entry, the built web client with its SPA
fallback (core `static-files.cjs` / `spa-routes.cjs`). Everything else, including Node's own
non-/api pages listed at the end of the file, is streamed to Node by `src/legacy_proxy.rs`. Its
tests fail when a rust route has no native handler or a node route does not reach the proxy. CI
job `corpus-replay-front` replays the corpus through the front (`tools/front/replay-front.sh`)
with zero differences and the same final data dir as a Node-direct run.

Follow-up once the front is live: delete core's `static-files.cjs`, `spa-routes.cjs` and
`createReadyRoutes` (Node then answers neither), not before.

## M3: Rust owns sign-in and the account (`NOEVIA_RUST_AUTH`)

A route may name a deployment `switch`. `owner = "rust"` with `switch = "NOEVIA_RUST_AUTH"`
takes effect only while the deployment sets `NOEVIA_RUST_AUTH=1`; without it the route is Node's,
so merging a flip changes nothing live. The switch also:

- opens server-store's `Writer` (crates/server-store `OWNED_TABLES`, `OWNED_SETTING_KEYS`,
  `OWNED_JSON_FILES`): the auth tables, with per-table access, and nothing else;
- makes the front write what Node's request gate wrote on every request (server-auth `upkeep`:
  `sessions.last_seen_at`, rejected sessions, `device_grants.last_used_at`) before it proxies;
- with `NOEVIA_FRONT=rust`, makes Node refuse to write the same tables (noevia-core
  `server/rust-auth.cjs`), so each row has one writer, and re-read the public address from
  settings on every use.

`noevia-server --features` lists `rust-auth`: the build answers the switched routes (the web
supervisor should check it before letting Node refuse the account tables). The switch is removed, with Node's copies of the routes, once Rust-owned sign-in
is proven live.

## M4: Rust owns project routes, slice by slice (`NOEVIA_RUST_PROJECTS`)

`switch = "NOEVIA_RUST_PROJECTS"` routes are Rust's only while the deployment sets
`NOEVIA_RUST_PROJECTS=1`, and the front refuses to start with it unless `NOEVIA_RUST_AUTH=1` too
(the front gates the routes it owns itself, and writes the session upkeep Node's gate wrote). The
first slice is a project's images: `POST /api/projects/{id}/assets`, `GET` and `DELETE
/api/projects/{id}/assets/{assetId}` (crates/server-projects `assets`). The second is a project's
chat list: `GET` and `POST /api/projects/{id}/chats` (crates/server-projects `chats`: core's
getProject heal and instruction-skill reconcile, chat-lists.cjs mergeChats with tombstones and the
other lists' ids, chat-framing.cjs normalizeFrame, the reconcile every save runs). The third is
the free-chat list: `GET` and `POST /api/freechats` (crates/server-projects `free_chats`: the same
mergeChats over `free-chats.json`, skipping and reporting ids a project holds). `DELETE
/api/freechats/{chatId}` stays Node's: it also removes the chat's transcript, context meter, brain
note and attachments project. JavaScript strings keep their lone surrogates (js-json `wtf16`), as
Node stores them.

Since sbstndalton/noevia#1806 a project source's text is not in projects.json: core keeps it in
`project-sources/<project id>/<sha256>.txt` beside the file, and a source row holds `textRef` (the
pointer), `chars` and, for a front-matter skill, `skillHeader`. Every Rust write keeps those fields
as read (files are generic JSON here), and the instruction-skill reconcile reads `skillHeader` for a
source whose text is in its file, as core's `looksLikeSkill` does; the text itself is never read.

`projects.json` cannot have one writer yet (Node's other project routes, the chat turn and a chat's
move or delete still change it), so while the switch is on both processes write it only under one lock (flock(2) LOCK_EX on a long-lived
`projects.json.lock`, crates/server-projects `lock` = core `server/rust-projects.cjs`): Rust re-reads it for every change
and writes Node's exact `atomicJson` bytes; Node re-reads it when it changed and saves by a
per-project, per-field three-way merge (`assets`, `retiredAssets` and `chats` per item), so neither
drops the other's change. With `NOEVIA_FRONT=rust`, `NOEVIA_RUST_AUTH_CONFIRMED=1` and
`NOEVIA_RUST_PROJECTS_CONFIRMED=1` (the supervisor sets the last once `--features` lists
`rust-projects`), Node's copies of the image writes answer 503 `RUST_PROJECTS_OWNED`; its
chat-list save does so only with `NOEVIA_RUST_PROJECT_CHATS_CONFIRMED=1` as well, which the
supervisor sets only once `--features` lists `rust-project-chats`. The front answers the chat list
only when built with the cargo feature of that name; without it, the route stays Node's under the
switch, and Node, unconfirmed, answers it (sbstndalton/noevia#1409). A request start that takes in
the front's change nudges Node's chat vault mirror, as Node's own project writes did. A front with
the chat slice needs a core that merges `chats` per item (core#75 or later): noevia's
`deploy/tools/assemble-release.sh` refuses a release whose front owns a project route its core's
`OWNED_ROUTES` does not list.

`free-chats.json` has the same arrangement under its own lock (`free-chats.json.lock`): while the
free-chat slice is on, Node's writers of the file (a chat's move into or out of a project, a free
chat's delete, the routing flags, an import, the retention sweep) take that lock, re-read the file
when it changed and save by a per-item merge of the list (core `createFreeChatsFile`), and its
own `POST /api/freechats` answers 503 `RUST_PROJECTS_OWNED`. Node does so only with
`NOEVIA_RUST_FREE_CHATS_CONFIRMED=1`, which the supervisor sets only once `--features` lists
`rust-free-chats`; the front answers the free list only when built with that cargo feature. A
move writes the list it adds to before the one it removes from, so a crash between the two leaves
a chat in both lists, never in neither. A front with the free slice needs core#76 or later.

A project write (an image upload or delete, a chat-list or free-list save) passes the gate again
right before it touches a file, since its body is read after the first pass, and the store never
creates the user's directory: a write for an account removed meanwhile answers Node's 410
`account no longer exists` and leaves nothing behind (sbstndalton/noevia#1425).

CI job `corpus-replay-front-rust-projects` regenerates the chat-list fixture at core.ref and
compares the bytes (and the free-chat fixture), replays the corpus through the front with both
switches, with both chat slices, with the project chat slice only and with neither (zero
differences, Node-only data dir), runs the Node-vs-Rust lock races (`server-projects --test
node_lock`: images, chat lists and the free list, each with a blind-save control that must lose
data, and a chat's move and a free chat's delete interleaved with the front's free-list save) and
the generated saves against core's own routes (`--test chats_property --test
free_chats_property`), and a negative control where Node is told the front owns the project
routes but the front does not: the uploads, chat-list and free-chat-list saves must be refused
with 503.

`core.sha256` records the twice-fetched, byte-identical codeload archive SHA-256 for
`core.ref` (metadata; existing replay CI consumes the single SHA in `core.ref`).
The core#61 landing provides the required native OCR front acceptance gate (#1271).

## M6: engine statistics (`NOEVIA_RUST_STATS`, sbstndalton/noevia#1650)

`GET /api/stats` (every open page polls it) is Rust's under `NOEVIA_RUST_STATS=1` in a build with
the cargo feature `rust-stats` (listed by `--features`); the front refuses the switch without
`NOEVIA_RUST_AUTH=1`. crates/server-stats ports core's handler, its 2 s shared read (concurrent
polls share one engine read; a model or role write that passes Node's gate drops it) and the engine
reads, from the same environment keys Node reads (`MODEL_MANAGER_KIND`, `MODEL_MANAGER_BASE_URL` /
`LEMONADE_BASE_URL` / `INFERENCE_BASE_URL`, the API key chain). Other methods stay Node's. With
`NOEVIA_FRONT=rust` and `NOEVIA_RUST_STATS_CONFIRMED=1` (set once `--features` lists `rust-stats`)
Node answers that GET with 503 (core `statsRustOwned`).

The reply is Node's byte for byte (`tests/oracle.cjs` drives core's own handler and adapters over
synthetic engines; `tests/differential.rs` replays every case). Deliberate differences: for
llama.cpp the reads Node makes but never uses for that kind (`health()`, `listModels()`) are not
made; the Lemonade kind is a startup refusal (its MTP last-response fallback lives in Node's
memory); an `https` engine is a startup refusal (no TLS client in the front); a value nested 64
deep that the JS would look into is a 503 rather than a guess. CI job `stats-native` (workflow
stats.yml) also replays the corpus through the front with the switch on, and once more against a
synthetic engine to show only `/api/stats` follows the front.

## M4: conversation export (`NOEVIA_RUST_EXPORT`, sbstndalton/noevia#1315)

`GET /api/export/conversations` is Rust's under `NOEVIA_RUST_EXPORT=1` in a build with the cargo
features `rust-export` and `rust-free-chats`; the front refuses the switch without
`NOEVIA_RUST_PROJECTS=1` (and so `NOEVIA_RUST_AUTH=1`) or `UI_DATA_DIR`. It reads the signed-in
user's free-chats.json, projects.json and history files fresh from disk (crates/server-export
`route`), appends the `export.conversations` audit row as Node does, and answers Node's bytes,
headers, 413 and 500. Other methods stay Node's (405). With `NOEVIA_FRONT=rust` and
`NOEVIA_RUST_EXPORT_CONFIRMED=1` Node answers that GET with 503 (core `exportRustOwned`).

Accepted difference (owner, 2026-10-12): the front exports what is on disk; a Node process's unsaved
or stale in-memory list is not seen. CI job `export-native` (workflow export.yml) regenerates the
Node oracle, runs the differential and HTTP tests, and replays the whole corpus through the front
with the switch on (exchange 182 is the export), plus a negative control once core refuses.

## M6: ChatGPT connection status (`NOEVIA_RUST_CHATGPT_STATUS`, sbstndalton/noevia#1373)

`GET /api/providers/chatgpt` is Rust's under `NOEVIA_RUST_CHATGPT_STATUS=1` in a build with the
cargo feature `rust-chatgpt-status`; the front refuses the switch without `NOEVIA_RUST_AUTH=1`,
`UI_DATA_DIR` and valid native auth, or with an invalid `NOEVIA_FEATURE_CHATGPT_OAUTH`. Every other
method and every `/api/providers/chatgpt/*` sibling (device login, poll, cancel, models) stays
Node's. With `NOEVIA_FRONT=rust` and `NOEVIA_RUST_CHATGPT_STATUS_CONFIRMED=1` Node answers that GET
with 503 (core `rustChatgptStatusOwned`, core#73).

crates/server-chatgpt-status reads the product flag first (off: Node's 404 before any OAuth row
or key), then only the signed-in user's `chatgpt_oauth_tokens` row, through the read-only store: no
DDL, no migration, no write. The secret keys are snapshotted once per process (a key file
replaced on disk needs a coordinated restart). `plan` is projected to a short identifier in Node
and Rust alike (core#135). A missing or older schema, an oversized or non-text row and unreadable
keys are a sanitized 502 (`ChatGPT sign-in failed.`). CI job `chatgpt-status-native` (workflow
chatgpt-status.yml) regenerates the Node oracle at `crates/server-chatgpt-status/tests/oracle-core.ref`,
runs the differential, bounds and HTTP tests, and replays the whole corpus through the front with
the switch on (exchange 147 is the status, flag off), plus a negative control once core passes the
switch through its corpus launcher.

### UTC calendar replay contract (#1308)

The committed manifest explicitly opts into version 1 of `calendarContract` (UTC,
365 usage days). Only successful exact GET `/api/usage`, `/api/usage/aggregate`,
and `/api/export/conversations` exchanges qualify. Replay independently captures
UTC dates immediately before and after the actual HTTP request; an endpoint's
last day / export filename date must equal one of those dates (at most a single
midnight crossing). Both recorded and actual usage windows must contain exactly
365 valid, ordered consecutive dates, `retentionDays: 365`, and `timeZone: UTC`.
Only validated expected day labels / the exact ZIP filename date are adapted.
Counters, streaks, models, sorting, all other body values and headers remain
subject to the existing strict comparison. Missing opt-in retains strict dates;
unsupported manifest contracts and invalid windows fail closed. No server clock
controls or date ignore paths are introduced.

Focused controls live in `tools/replay/src/calendar.rs`: leap/year boundaries,
midnight capture ordering, malformed/gapped/reordered windows, retention/timezone,
wrong anchors, nondate drift, full export filename format, and real TCP replay
with opt-in/off and wrong-date refusal. Qualification mutations removing window
ordering, request-anchor checks, or calendar wiring must fail these tests.

### Fixed synthetic host input (#1329)

Every required corpus replay uses the Rust-owned `corpus-launcher` against the
actual commit in `core.ref`. It adds a preload only to the literal actual corpus
server child spawn. The preload supplies `MemTotal: 8388608 kB` through the
existing `createInferenceBudget` `readMeminfo` collaborator; the actual core
module computes every public budget field. This fixture does not change core
runtime policy or read/patch the host filesystem. Existing spawn options,
authentication confirmations, signals and child cleanup remain intact.

For a fresh synthetic capture, build `cargo build --locked -p replay`, then run
`target/debug/corpus-launcher capture /absolute/actual-core /absolute/new-output`.
The capture retains the owned UTC calendar contract and writes source, runtime,
WASM checksum and host-input provenance. The refresh helper refuses raw
host-dependent CI artifacts. The checked-in corpus requires both calendar and
fixed-host provenance in focused Rust tests.

The mandatory reasoning job verifies the actual spawn and factory seam, rejects
a removed preload, and runs `host-control` with a 12 GiB input. That full replay
must reject exactly the strict `budgetGib`, `hostRamGib` and `maxGib` fields;
all other fields and statuses remain checked. The regular 8 GiB Node baseline
and native front must match all 228 exchanges and final state.
