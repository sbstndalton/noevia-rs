# HTTP contracts (full-Rust migration M0)

- `routes.toml`: every `/api/` path the web client (noevia-web `src/`, tests excluded, at the
  commit in `web.ref`) names, with the server that owns it: `owner = "rust"` for the routes a
  Rust slice has taken over, `owner = "node"` for the rest. A rust route lists the `methods` it
  answers (all methods when absent); every other method goes to Node. `switch` names the deployment
  switch a route still depends on (only `NOEVIA_RUST_PROJECT_WRITES` now). CI job `routes-check`
  fails when the client names a path missing here (`tools/contracts/routes_check.py`); paths are
  matched by shape, so `{id}` and `{chatId}` are the same parameter.
- `corpus/`: a synthetic recorded corpus. `exchanges/NNNNNN-METHOD-path.json` are request/response
  pairs written by noevia-core's opt-in recorder (`NOEVIA_CONTRACT_RECORD`,
  `server/contract-record.cjs`) while `tools/contract-corpus/generate.cjs` drove a server on a
  throwaway data dir with a mock model and Diary. Secrets are `<secret:N>`, ids `<id:N>`,
  timestamps `<ts>`; the same value has the same placeholder throughout. `manifest.json` names the
  inputs a replay must supply (the setup code file, the synthetic passwords). Refresh it with
  `tools/contracts/refresh-corpus.sh <noevia-core run id>`.

- `core.ref`: the noevia-core commit the CI job `corpus-replay-node` boots to prove the corpus
  replays clean against Node.

- `store-guard-core.ref`: the exact reviewed noevia-core ownership counterpart used only by
  `server-store`'s `node_guard` tests. It includes the new address-setting ownership contract,
  so the tests require exact table and settings equality without pending-owner allowances.
  This is separate from the frozen behavior oracle above: replay still boots `core.ref`.
  The counterpart is a published, reviewed source commit; it is not a selected release pin,
  assembled-release qualification, or deployment approval. The paired Core integration and
  release capability guards still have to pass before activation.

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

## M3: Rust owns sign-in and the account (permanent)

Since sbstndalton/noevia#1071 the sign-in and account routes are `owner = "rust"` with no switch:
the deployment switch `NOEVIA_RUST_AUTH` is retired (a leftover value only earns a startup
warning), core no longer has the Node handlers, and the front refuses to start without
`UI_DATA_DIR` or with an invalid auth environment. Rust's ownership (`RustAuth::owned`):

- opens server-store's `Writer` (crates/server-store `OWNED_TABLES`, `OWNED_SETTING_KEYS`,
  `OWNED_JSON_FILES`): the auth tables, with per-table access, and nothing else;
- makes the front write what Node's request gate wrote on every request (server-auth `upkeep`:
  `sessions.last_seen_at`, rejected sessions, `device_grants.last_used_at`) before it proxies;
- is matched by Node always refusing to write the same tables (noevia-core
  `server/rust-auth.cjs`), so each row has one writer, and re-reading the public address from
  settings on every use.

`noevia-server --features` lists `rust-auth`; the web supervisor refuses to start without it.
The corpus oracle at `core.ref` predates the removal: tools/front/replay-front.sh always tells it
the front owns sign-in (`NOEVIA_RUST_AUTH=1` with its confirmation, on the Node side only).

## M4: Rust owns project routes (permanent since sbstndalton/noevia#1071)

Since sbstndalton/noevia#1071 the M4 project routes, the conversation export and the project store
have no switch: `NOEVIA_RUST_PROJECTS` is retired (a leftover value only earns a startup warning;
routes.toml no longer names it and build.rs refuses it), Node refuses those routes
unconditionally and no longer writes projects.json or free-chats.json itself (it saves through the
project store, #1264), and the front refuses to start without the cargo features `rust-projects`,
`rust-project-chats`, `rust-free-chats` and `rust-export` (the default features); the web
supervisor checks the same in `--features`. The `*_CONFIRMED` handshakes below were how the
slices rolled out; the corpus oracle at `core.ref` predates the retirement, so
tools/front/replay-front.sh still tells it `NOEVIA_RUST_PROJECTS=1` and every confirmation. The
rest of this section describes the slices as they were introduced.

The front gates the routes it owns itself, and writes the session upkeep Node's gate wrote. The
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
drops the other's change. With `NOEVIA_FRONT=rust` and
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
free_chats_property`). Its negative control (a front started without the switch) left with the
switch (#1071): core's own tests prove Node refuses these routes unconditionally.

`core.sha256` records the twice-fetched, byte-identical codeload archive SHA-256 for
`core.ref` (metadata; existing replay CI consumes the single SHA in `core.ref`).
The core#61 landing provides the required native OCR front acceptance gate (#1271).

## sbstndalton/noevia#1071: a chat's context meter

`GET /api/chats/{chatId}/context-window` (core routes/chat-lists.cjs) is Rust's with no switch
(crates/server-projects `context_window`): a read of the user's `context-<sha256>.json` behind the
chat tombstones, answer for answer, except that a state file holding `null` answers
`{ meter: null }` where Node answered 500. Core refuses it (rust-projects.cjs `OWNED_ROUTES`) once
its handler is deleted. The rest of the chat-list routes stay Node's for now: `DELETE
/api/freechats/{id}` removes the chat's attachments project (a whole project delete with its RAG
index and storage sweep) and brain note; `POST /api/chats/{chatId}/move` revokes the chat's
"Allow for this chat" grant, which lives in Node's memory (approvals.cjs), so each needs a
Node-to-front channel first. The history GET has moved (next section).

## sbstndalton/noevia#1071: a chat's history (GET)

`GET /api/chats/{chatId}/history` (core routes/chat-lists.cjs) is Rust's with no switch
(crates/server-projects `history`): the user's `history-<safeChatId>.json` as `history`
(`.history || []`), `revision` = sha256 hex of `JSON.stringify(history)` byte for byte (Node's
`POST` compares a save's `baseRevision` with it, so the two must agree), and the chat's saved
partial reply as `partial` when the reply that wrote it is no longer running. Node decided that
with a set in its own memory; the front reads the `running: { start }` marker core chat-partial.cjs
writes into every save of a running reply's partial (noevia-core#245): a marker naming this
container start's `NOEVIA_START_ID` is still running (hidden), any other or none was cut off by a
crash or restart (offered, without the marker). This fixes sbstndalton/noevia#2144 (Node's set was
shared by every account and cleared by the first of two concurrent replies to end). A deleted
(tombstoned) chat answers `{ history: [] }` with no partial, as one that never existed
(sbstndalton/noevia#2147; Node served whatever was left). A transcript this port cannot hold is a
500; such a partial is left out. The pinned core must write the marker and replace it when a
finished reply's partial cannot be removed (sbstndalton/noevia#2146), which noevia's
assemble-release checks; core refuses the GET (rust-projects.cjs `OWNED_ROUTES`) once its handler is
deleted. `POST` (the save) stays Node's until the transcript store ops move (slice 5).

## sbstndalton/noevia#1071: the retention preview

`GET /api/account/retention` (core routes/account.cjs, Settings -> Data -> Delete old chats) is
Rust's with no switch (crates/server-projects `retention`, routed to the project handler although
the path is under `/api/account/`): `chat-retention.json`'s `days` and, for each period, how many
chats chat-retention.cjs `expired()` would delete, over `free-chats.json` and `projects.json` read
fresh from disk without the Diary-internal projects. It is a pure function of those files and the
clock: Node's view of them is only a cache of the same files (Rust is their only writer), so no
Node process state is needed. Accepted difference: a Node save still queued for the store is
counted once it is written. `tests/retention_property.rs` runs generated cases through core's own
`expired()` (`tools/retention-expired-oracle.cjs`, CI sets `NOEVIA_CORE_CHECKOUT`). `PUT` stays
Node's: it deletes chats through Node's delete cascade, and recounts what is due itself, refusing
(409, with the preview) more than `confirmDeletes`, so a preview here never becomes a delete. Core
refuses the GET (rust-auth.cjs `OWNED_ROUTES`) once its handler is deleted.

## sbstndalton/noevia#2185: the Diary sidecar client, and the Diary routes that are only a proxy

`crates/server-diary` is the front's Diary client: what core `diary.cjs` adds before it proxies to the
Python Diary sidecar (bearer `DIARY_AUTH_TOKEN`, `X-Cowork-User-ID`, `X-Cowork-Legacy-Owner` when the
workspace holds `migration.json`, the base64url storage descriptor, the v2 assertion signed by
`crates/tenant-assertion` with `DIARY_TENANT_KEY`, and the once-only 428 retry that re-signs and adds
the secret), read from the same environment (`DIARY_BASE_URL`, `DIARY_AUTH_TOKEN`, `DIARY_TENANT_KEY`,
`DIARY_SOURCE`, `MEMBER_OUTBOUND_ORIGINS` per call, `SECRETS_KEY_PREVIOUS`; a `DIARY_SOURCE` other than
`sidecar` stops startup as it stops Node). The storage connection comes from the same owner-bound
projection the storage list/read routes use (`server-storage-reads` `owner_secret`), so a person only ever
speaks with their own row. `--features` lists `rust-diary`; core refuses the moved routes with 503
`RUST_DIARY_OWNED` (core `server/rust-diary.cjs`). The sidecar stays the single writer: the front passes
on the signed-in person's request, signed for that person, and writes nothing itself.

| route | owner | why (core file:line at 94953d0) |
|---|---|---|
| `/api/diary/source` (any method) | rust | `routes/diary.cjs` `corpusSource.listMonths()` over `GET /api/months`: a proxy and a filter; a failure is an empty list |
| `/api/diary/today` (any method) | rust | `routes/diary.cjs` `corpusSource.readMonth()` over `GET /api/day`: a failure is core's generic 500 |
| `/api/diary/storage-import` (POST) | rust | `routes/diary.cjs`: 4 KiB body, 300 s, the sidecar's `detail` relayed |
| `/api/diary/workspace-export` (GET) | rust | `workspace-export.cjs`: 300 s, 272 MiB, streamed, cut short rather than completed |
| `/api/diary/workspace-import` (POST) | rust | `workspace-import.cjs`: 32 MiB body, 120 s, 24 MiB reply (#920), 428 resend of the buffered body |
| `/api/diary/context` | node | `routes/projects.cjs:185` creates the Diary project in `PROJECTS`, Node's in-memory project list, through `diaryExtras.newProject` |
| `/api/diary/exchanges` | node | `routes/diary.cjs` lists and acknowledges through `diary-jobs.cjs`, whose `active` set of running jobs is Node memory, written by the chat turn |
| `/api/diary/local-exchange` | node | `routes/diary.cjs` takes the model throttle shared with chat (`llmRateLimited`) and, via `index.cjs handleRequest`, the inference maintenance gate; both are Node memory |
| `/api/diary/storage-status` | node | `routes/diary.cjs` adds `automaticStorage` from `storage-latch.cjs`, an in-memory latch the backup worker and the source refresh write |
| `/api/diary-connector` | node | `diary-connectors.cjs`: credential table, in-memory throttles (`connectorRate`), audit rows, and `operate()` |
| `/api/profile/diary-connectors[/{id}]` | node | the same `diary_connectors` table (Node creates it and its tokens); no sidecar call at all |
| `/api/diary` | node | no handler: Node's generic not-found |

Not in this table and so still Node's: `/api/diary/history` (the same read as `today` under a name the
web client does not use), `files`, `file`, `workspace-trash`, `external-sources`, `entries/edit`.

Judgement calls, each testable: (1) core took the inference maintenance gate for every non-GET
`/api/diary/*`. The front takes no gate for the two moved POSTs, and a direct request to Node no longer
holds one either. `workspace-import` opens the tenant without recovery and makes no model call.
`storage-import` opens it with recovery, so on a cold tenant state it can reindex dirty files (an
embedding call, failures caught) before it snapshots and activates. Node's ungated GET reads already
reach that same recovery path, and activation only schedules work, so the gate guarded nothing the
reads do not already do. (2) An upload whose body is still arriving at the 120 s
deadline is answered 408; core destroyed the socket. (3) A request body
of at most 4 KiB gets a 120 s read deadline (core had only the server-wide 20 minutes). (4) Core's
`getStorage` upgrades a legacy v1 ciphertext in place when it reads it; the front reads and never writes
`storage_connections`, so that upgrade still happens only on Node's own reads. (5) A sidecar reply that
`JSON.parse` accepts but the port cannot hold (a number outside f64, a character in the reserved private
range) is treated as a failed call (`js_json::wtf16`'s `Unsupported`).

## sbstndalton/noevia#1071: Settings -> Sharing, and the small routes that stay Node's

`GET` and `PUT /api/profile/sharing` (core routes/auth.cjs over dav-settings.cjs) are Rust's with
no switch (crates/server-account `sharing`): every input is persisted or configured
(`dav_settings`, `user_features.diary_enabled`, `storage_connections.kind`, COWORK_DAV_*). Rust is
the table's only writer: server-store `OWNED_TABLES` grants `dav_settings` insert/update, core
rust-auth.cjs refuses Node's writes to it and the routes (503 RUST_AUTH_OWNED); core still creates
the table at boot and its DAV listener only reads the opt-in. Answer for answer with Node, except:
the public address a `public` endpoint without COWORK_DAV_ORIGIN reports is read per request (Node
fixed it at boot, sbstndalton/noevia#2143), a failed save is a 500 (Node answered 400 with the
database's message), the row and its audit event share one transaction (sbstndalton/noevia#2142),
and an oversized body keeps its 413.

These stay Node's because they read Node in-process state:

- `GET /api/health`: `ragAvailable` is whether Node's own process loaded its native index
  modules (rag.cjs `loadDeps`), `retrieval` comes from Node's in-memory probe cache, in-flight
  probe and model-maintenance gate, and `inferenceUp` probes Node's provider registry.
- `GET /api/features`: the flags of restart-only features are the values Node read at its boot
  (features.cjs `state[].boot`), and several flags are gated by availability callbacks over Node's
  decision settings.

## sbstndalton/noevia#1071: the front read routes are permanent

The usage, sampling and reasoning settings, routing mode and routing default, engine statistics,
ChatGPT status and connected-storage metadata/list/read routes below are always Rust's (no
routes.toml `switch`; `NOEVIA_RUST_AUTH` itself is retired as well), and so is the conversation
export (`NOEVIA_RUST_PROJECTS` is retired too). Their own switches (`NOEVIA_RUST_USAGE`,
`_SAMPLING_SETTINGS`, `_REASONING_SETTINGS`, `_ROUTING_MODE`, `_ROUTING_DEFAULT`, `_STATS`,
`_CHATGPT_STATUS`, `_STORAGE_READS`, `_STORAGE_BROWSE`, `_EXPORT`) are retired: a leftover value
only earns a startup warning. Core no longer has the Node handlers, so the front refuses to start without every cargo feature they need (the default features), `UI_DATA_DIR`,
a valid auth environment or a stats engine it can read; the web supervisor checks the same features
in `--features` before starting either process. The sections below describe each slice as it was
introduced; their Node oracles and fixtures stay pinned at `core.ref` and are not regenerated, and
the "negative control" replays (front started without a switch, confirmed Node refuses) left with
the switches: core's own tests now prove Node refuses these routes unconditionally.

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
with the switch on (exchange 182 is the export), plus a negative control: Node, confirmed, must
refuse it with 503 (never skipped).

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
the switch on (exchange 147 is the status, flag off), plus a negative control: Node, confirmed,
must refuse it with 503 (never skipped).

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

## sbstndalton/noevia#2187 (batch M1): one maintenance authority, and the configuration-only model reads

The inference gate (`bins/noevia-inference-gate`) is the one maintenance authority. A preset write, a
calibration, an auto-tune model or a throughput sweep takes the gate's maintenance hold
(`POST /gate/v1/maintenance`, admin token only); every client of the router then respects the same
hold, and core's in-process gate (`server/inference-maintenance.cjs`) is only the fallback when no gate
with the hold API is configured (no `INFERENCE_GATE_ADMIN_TOKEN`, or a 404 from the engine address).
The hold is refused (409 `engine_busy`) while a request is in flight, a lease is live, anyone is queued,
a switch, unload or reload runs, or a detached session generation is held; `wait_ms` (at most 10 min)
waits for that to end. While it is held, requests and leases without it wait for its end (bounded by
their own wait limit, then 503 `maintenance_held`); unloads (but an admin force) and preset reloads
without it are refused `engine_busy`; exempt work and `autoload=false` reads, which never take the
chat slot, run as they did under core's in-process hold. The
holder's requests carry it in `X-Noevia-Maintenance`. It ends by `DELETE /gate/v1/maintenance/{hold}`,
or when its holder has neither renewed (`POST /gate/v1/maintenance/{hold}/renew`) nor used it for its
idle limit (default 60 s, 1 s to 10 min on request): a holder that died pauses the engine that long at
most; there is no hard limit, a calibration may run for hours while it renews. `GET /gate/v1/status`
shows `maintenance: {held, reason, held_ms, idle_ttl_ms}` (the holder's text, never request content).

Two of the 22 model routes are configuration only and move to the front, GET only
(`src/model_routes.rs`); core refuses those GETs with 503 `RUST_MODELS_OWNED` (`server/rust-models.cjs`)
and `--features` lists `rust-model-reads`. The rest wait for batch M2 (the job runners and writes).

| route | owner | why |
|---|---|---|
| `/api/models/capabilities` (GET) | rust | the adapter kind, `LLAMACPP_PRESET_PATH` and `MODEL_LOADER_URL` only |
| `/api/models/hardware` (GET) | rust | no model manager: 404; llama.cpp cannot discover hardware: `{supported:false}`; other methods keep Node's 405 |
| `/api/model-manager` | node | proxy whose `GET models` serves Node's in-memory folder scan cache (and writes it); sweep hold, delete cleanup |
| `/api/models/autotune` | node | GET is the auto-tune job runner's in-memory job state; POST starts it (M2) |
| `/api/models/autotune/cancel` | node | write to the running job (M2) |
| `/api/models/autotune/clear` | node | write to the job state (M2) |
| `/api/models/autotune/resume` | node | starts the job runner (M2) |
| `/api/models/autotune/settings` | node | write to the tune history the runner keeps (M2) |
| `/api/models/autotune/untuned` | node | reads the runner's tune history and identity state |
| `/api/models/calibration` | node | GET is the calibration job's in-memory state; POST starts it (M2) |
| `/api/models/calibration/cancel` | node | write to the running job (M2) |
| `/api/models/delete` | node | write: unload, delete, roles, preset section (M2) |
| `/api/models/estimate` | node | sizes with the boot-time budget fault and the inference budget store held in Node |
| `/api/models/evidence` | node | fills Node's in-memory identity cache that evidence writes use |
| `/api/models/evidence/import` | node | write (M2) |
| `/api/models/evidence/recheck` | node | write that may load a model (M2) |
| `/api/models/inference-budget` | node | budget store, boot-time budget, Node's shared installed list; PUT writes |
| `/api/models/installed` | node | Node's shared (cached, invalidated by its writes) installed list |
| `/api/models/load` | node | write through Node's lease and maintenance gate (M2) |
| `/api/models/machines` | node | machine health probe and tune caches in Node memory; POST writes |
| `/api/models/presets/reload` | node | write under the maintenance hold (M2) |
| `/api/models/{*}` | node | preset, suggest, idle-unload, assignments, reranker, prune, mtp-artifact, pull, downloads: mixed reads of Node state and writes |

### PR notes (#2187)

- sbstndalton/noevia#2138: the model-manager sidecar's throughput sweep reaches the router outside the
  gate today (web holds the gate's maintenance hold for it, core `benchmark-sweep-guard.cjs`). If the
  sweep is later routed through the gate, its requests must carry the hold (`X-Noevia-Maintenance`),
  or they wait for the very hold that protects them.
- A web restart during a sweep: core gives its hold back at shutdown and the new process adopts the
  running sweep, asking for the hold again (with a growing delay) until the old one is gone.
- A hold the gate forgot mid-job (a gate restart) is taken again with a bounded wait; if that fails
  the job's requests stop carrying the dead token and run without the gate's hold (logged).
- Requests and leases waiting out a hold count against the queued body share and the waiter limit
  (`gate_busy` past them), so they cannot starve the holder's own requests. A request is admitted
  under a hold only if the hold it names is still the one held (the key, not a flag).
- Exempt work in flight does not keep a hold from being taken (it runs while one is held).

## sbstndalton/noevia#2190: the settings and integrations singles that need nothing of Node's memory

Two groups move to the front, each with its own Core marker and front capability so either can be
rolled back alone (old/old pairs stay valid; the web supervisor and release assembly refuse mixed
pairs):

| route | owner | capability / Core refusal | notes |
|---|---|---|---|
| `/about`, `/privacy` (GET, HEAD) | rust | `rust-public-pages` / `RUST_PUBLIC_PAGES_OWNED` | Node's exact bytes and headers (`src/pages/`), no session; other methods stay Node's |
| `/device` (all methods) | rust | `rust-public-pages` / `RUST_PUBLIC_PAGES_OWNED` | JSON 404 while `nativeClientAuth` is off (read per request); the bundle's page when on |
| `/api/plugins/directory` (all methods) | rust | `rust-plugin-directory` / `RUST_PLUGIN_DIRECTORY_OWNED` | fixed hosts, no redirects, 8 s, 4 MiB, hour cache, starters; 405 for all but GET after the gate |

The replay runs the front with `NOEVIA_QA_MCP_REGISTRY` and `NOEVIA_QA_SKILLS_DIRECTORY` on a
closed loopback port (`tools/front/replay-front.sh`), the counterpart of the corpus net guard Node
runs under: exchange 000141 is the recorded "could not be reached" 502 on both sides.

### Round 2: the decision settings (Rust single writer, Node re-reads)

| route | owner | capability / Core refusal | notes |
|---|---|---|---|
| `/api/admin/decision-settings` (all methods), `/api/admin/decision-settings/test` (all methods) | rust | `rust-decision-settings` / `RUST_DECISION_SETTINGS_OWNED` | admin only; GET/PUT/DELETE and `PUT {reset:true}`, POST `/test`, 405 otherwise; the save or reset and its `decision.*` audit row in one transaction |

Rust is the only writer of the `decision:configuration` settings row (core rust-auth.cjs refuses
the key on Node's connection with the same code). Node keeps the decision path in process and
re-reads the row on every use by its content fingerprint (core feature-setting-source.cjs
`readFact().source`, through core shared-reread.cjs): a save here is in force for the next
decision with no restart, and the per-URL decision backends (their lanes) stay Node caches keyed
by what it re-read. The view's Inference machine address comes from the `inference:machines`
row, which stays Node's to write. The connection test resolves a name and refuses it unless every
answer is on this network, then checks `/health` through a resolver that applies the same rule
at connect time, no redirect, no environment proxy, 3 s, 4 KiB.

### Round 3: the storage integrations (Rust owns the Nextcloud sign-in)

| route | owner | capability / Core refusal | notes |
|---|---|---|---|
| `/api/integrations/storage/folder`, `/test`, `/nextcloud/start`, `/nextcloud/poll`, `/nextcloud/cancel` (POST) | rust | `rust-storage-integrations` / `RUST_STORAGE_INTEGRATIONS_OWNED` | session gate, then the JSON-object body (1 MiB); the member-origin rule on every target, no redirect, no environment proxy, Node's deadlines and the 64 KiB Nextcloud reply cap; other methods stay Node's |
| `/api/chat-vault-mirror/preferences` (all methods) | rust | `rust-vault-mirror-preferences` / `RUST_VAULT_MIRROR_PREFERENCES_OWNED` | Rust is the only writer of `users/<id>/chat-vault-mirror.json` (Node's mirror re-reads it per use); a PUT that turns it on while available writes `chat-vault-mirror-nudges.json` (`{version:1,nudges:{<userId>:<ms>}}`), which core polls by file signal to run that account's mirror at once |

The pending Nextcloud sign-ins move wholesale: the same sealed file
(`nextcloud-login-flows.json`, `enc:v2:` bound to the account, 10 minutes, at most 100, one per
account, a nonce per flow so a cancelled flow's in-flight poll revokes the app password). A
completed sign-in saves the `storage_connections` row with core `saveStorage`'s exact upsert and
its `storage.update` audit row in one transaction (server-store `save_storage_connection`, its own
grant); Node reads that row on every use (`getStorage`), and still saves it from
`PUT /api/integrations/storage`, which stays Node's. The storage latch stays in Node with its
automatic callers (the project refresh, the Diary backup): a folder or test call that works with
the saved login writes `storage-login-proofs.json` (`{version:1,proofs:{<userId>:<ms>}}`, no
credential), which Node's latch re-reads by its file signal (core shared-reread.cjs) and which
ends a hold set before it.

Left on Node in this batch, and why (each resolves Node memory, a Node-only cache or an in-process
job that chat or another Node route reads):

| route | why it stays |
|---|---|
| `/api/providers`, `/api/providers/{id}`, `/api/providers/test`, `/api/providers/{providerId}/models{*?}` | the provider rows live on the in-memory workspace (`ws.providers`, `PROVIDERS`) that every chat turn reads in-process; a Rust writer would need Node to re-read the workspace per turn (follow-up) |
| `/api/providers/chatgpt/device`, `/device/poll`, `/device/cancel`, `/api/providers/chatgpt/models` | chatgpt-oauth.cjs keeps the pending logins, the catalogue cache, its in-flight fetch and the token refresh lock in memory; chat uses the same tokens |
| `/api/connectors`, `/api/connectors/gdrive/*`, `/api/connectors/nextcloud/policy` | drive-accounts.cjs holds personal Drive tokens in memory and the chat tools read the policies in-process |
| `/api/admin/offsite-backup` and its six children | offsite-service.cjs runs the backup, copy and verify jobs and their busy lock and 15-minute timer in-process |
| `/api/admin/users/{id}` (DELETE) | the clean-up spans Node's workspace cache, MCP OAuth, directory MCP keys and ChatGPT tokens in memory |
| `/api/auto-roles` | GET reads the served catalogue and machine models; an admin PUT warms the roles in Node's engine (`ensureRolesLoaded`) |
| `/api/health` | the retrieval probe is Node's cached RAG status, and the default provider is Node's in-memory registry |
