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
`getStorage` upgrades a legacy v1 ciphertext in place when it reads it; the front's reads never write
`storage_connections`, so that upgrade still happens only on Node's own reads (since #2190 round 4 a
compare-and-swap reseal of the same plaintext, the one Node write of the table its guard allows). (5) A sidecar reply that
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
| `/api/models/estimate` (GET) | rust | moved in #2189 (below); other methods keep Node's admin check and 405 |
| `/api/models/evidence` | node | fills Node's in-memory identity cache that evidence writes use |
| `/api/models/evidence/import` | node | write (M2) |
| `/api/models/evidence/recheck` | node | write that may load a model (M2) |
| `/api/models/inference-budget` | node | budget store, boot-time budget, Node's shared installed list; PUT writes |
| `/api/models/installed` | node | Node's shared (cached, invalidated by its writes) installed list |
| `/api/models/load` | node | write through Node's lease and maintenance gate (M2) |
| `/api/models/machines` | node | machine health probe and tune caches in Node memory; POST writes |
| `/api/models/presets/reload` | node | write under the maintenance hold (M2) |
| `/api/models/preset/suggest` (GET) | rust | moved in #2189 (below); other methods keep Node's admin check and 405 |
| `/api/models/{*}` | node | preset, idle-unload, assignments, reranker, prune, mtp-artifact, pull, downloads: mixed reads of Node state and writes |

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

## sbstndalton/noevia#2191 (group rust-project-reads): a project source's stored reads

Three GETs that read projects.json and a content-addressed file Node writes once and never changes
in place move to the front (crates/server-projects `sources` and `originals`, served by
`src/project_routes.rs`); core refuses those GETs with 503 `RUST_PROJECT_READS_OWNED`
(`server/rust-project-reads.cjs`) and `--features` lists `rust-project-reads`. Each looks the
project up as `getProject` does (the context self-heal and the instruction-skills reconcile are
saved under the projects.json lock), opens every component below the user directory with
`O_NOFOLLOW` (a symlink is never followed out of the tenant directory) and serves only a regular
file whose name is a validated hex hash, never a request value.

| route | owner | why |
|---|---|---|
| `/api/projects/{id}/sources/text` (GET) | rust | inline text, or `project-sources/<safe id>/<textRef>.txt`; a missing text is 404 `SOURCE_TEXT_MISSING`, the pointer kept |
| `/api/projects/{id}/documents/original` (GET) | rust | `project-documents/<sha256 id>/<byteHash>.pdf`; unreadable is Node's 422 |
| `/api/projects/{id}/{*}/original` (GET) | rust | `uploads/original` only (`project-uploads/<sha256 id>/<attachment.id>`, #786's 404); any other segment is Node's |
| `/api/projects/{id}/documents/pages` (GET) | rust | round 4, below |
| `/api/projects` (POST), `/api/projects/{projectId}` (DELETE) | rust | round 2, below |
| `/api/projects/{projectId}/config` | node | its model check reads Node's served-model catalogue and provider registry (live state, not persisted), and a `files` patch re-indexes through Node's RAG writer; next after the provider/model catalogue is published like the toolbox catalogue |
| `/api/projects/{id}/documents` (POST) | rust | round 5, below: the front's Docling pipeline and durable extraction queue |
| `/api/projects/{id}/upload`, `/sources/sync`, `/folder-import/plan` (POST, with or without `?background=1`), `/api/projects/{id}/source-jobs/{jobId}` (GET) | rust | rounds 6 and 8, below: the front owns the source-job store and runs the operations itself (streamed storage reads, Docling, the OCR worker's `/reduce-pdf` and `/extract-docx`, the storage latch, the folder import's tickets); Node has no worker call and no copy of any of it |
| `/api/projects/{projectId}/instruction-skills`, `.../manifests`, `/api/projects/{project}/skills/install` | node | the known-toolbox list (plugins, MCP) is Node's runtime catalogue; install fetches a published skill through Node's plugin directory |
| `/api/projects/{projectId}/browser`, `/code`, `/research` | node | they drive a browser session, the code harness or research jobs |
| `/api/code/active` | node | the code harness service's live task list |
| `/api/workspace` | node | its load runs the retention sweep and the orphaned attachments-project reaper, whose delete (RAG index, uploads) is Node's |
| `/api/import/conversations` (POST) | rust | round 4, below |

## sbstndalton/noevia#2191 round 2 (group rust-project-lifecycle): a project's create and delete

`POST /api/projects` and `DELETE /api/projects/{projectId}` are the front's (crates/server-projects
`lifecycle`, served by `src/project_routes.rs`); core refuses both with 503
`RUST_PROJECT_LIFECYCLE_OWNED` (`server/rust-project-lifecycle.cjs`) and `--features` lists
`rust-project-lifecycle`. Every other method of both paths is Node's.

- Create: Node's validation order and messages, Node's keys in Node's order, the folder reserved
  (#589) as `reserveProjectFolder` names it under the projects.json lock, create-time texts written to
  `project-sources/<id>/<sha256>.txt` before the projects.json that points at them (#1806), first in
  the list, every project's skills reconciled. The toolbox ids a project may select (MCP boxes
  included) are core's runtime state: core publishes them with the default list and the icon ids as
  `UI_DATA_DIR/project-catalogue.json` (core `project-catalogue.cjs`, written on change at startup
  and at each request start); the create answers 503 until it exists.
- Delete: the record removed under the lock (a failed save removes nothing, #1914), then the
  project's upload originals, document pages, images, RAG index, empty directories (never through
  a symlink, never recursive), its chats (the store's `Unlisted` delete) and their attachments
  projects.
- The index outbox (crates/server-projects `outbox`, `users/<id>/index-outbox.db`, SQLite WAL): the
  RAG index of create-time files and the storage folder sweep run where their engines are (core's
  sqlite-vec index, core's WebDAV client), so the front records them in the same request:
  idempotent job keys (`["index",project,file]`, `["sweep",project]`; a pending key is re-queued at
  a new generation, never duplicated), claim/acknowledge at a generation (work asked again while a
  job runs runs again), a `running` row taken back at core's next start (at least once; every job
  is idempotent), 5 attempts then `failed`. A delete writes the project's tombstone and drops its
  pending index jobs in one transaction before removing the RAG files; core's index run checks the
  tombstone against the generation it was queued at on every step and closes its handle (core
  `rust-index-outbox.cjs`, `rust-generation.cjs`, rag.cjs lifecycle guard).
- #687's storage step and the sweep moved to the front in round 3 (below).

## sbstndalton/noevia#2191 round 3 (group rust-project-storage): the front's storage writes

No route moves. The front gains a storage write client (crates/server-storage-reads `writes`:
core storage-client.cjs `createFolder`, `writeFile` with `If-None-Match: *` / `If-Match`,
`discardWritten`, `deleteFile`, `removeEmptyFolder`, `fileVersion`; same `safeRelativePath` and
`davUrl` encoding, same answers, `withRetry`'s one repeat of a pre-header transport failure, never
for a conditional write; per-method deadlines PUT 60 s, DELETE 30 s, MKCOL/PROPFIND 15 s; S3 refused
where core refuses it; core has no 428 retry for storage, nor does this). Every attempt runs in the
owned storage child (request frame `M4REQ002`: the method byte names PUT/MKCOL/DELETE and a body
follows the headers, a PUT's at most 25 MiB; a write's reply payload is its ETag, never the
response body; redirects refused, no proxy, no cookies), after the browse's endpoint policy, with
its own admission (four at once, one per account). With it:

- a create with files stores its storable text files in the project's folder (core
  `storeCreateFiles`, #687: the folder allocated as `allocateFolder` does and attached as a source,
  each file written create-only to `<folder>/Text/<name>`, its original kept in `project-uploads/`;
  a file storage refuses stays noevia's plain-named copy); its index jobs name the stored files;
- the `sweep` jobs of the index outbox (a delete's empty-folder sweep, `sweepRemote`) are the
  front's: run after the delete (or create) answers, claimed and acknowledged with the executor
  contract, `running` rows of that kind taken back once per process. Core claims, recovers and
  counts only `index` jobs (core `rust-project-storage.cjs` `CORE_KINDS`), and `--features` lists
  `rust-project-storage`; the web supervisor and assemble-release pair the two.

## sbstndalton/noevia#2191 round 3 (group rust-project-files): a source file's delete

`DELETE /api/projects/{id}/files` is the front's (crates/server-projects `files`); core refuses it
with 503 `RUST_PROJECT_FILES_OWNED` (`server/rust-project-files.cjs`) and `--features` lists
`rust-project-files`. Node's order and answers: 404 `no such project`, the body as `readJson`
reads it (empty is `{}`; 400 `invalid JSON`; 413 past 1 MiB), 400 not an object, 400 `a path is
required`, `ownsFile`'s 400, 400 `no browsable storage connected`, then `deleteFile` through the
round-3 write client (a folder refused, conditional on the file's ETag) with
`replyStorageFailure`'s answers (`storageLoginRejected`, `storageForbidden`, `storageThrottled`,
`storageTimeout`, `storageUnreachable`) or the error's own status and message. The file leaves the
list first (#1914); then its document pages, upload original and images go (an image a chat still
names is retired), and its RAG rows are an `unindex` job in the index outbox, which core runs
(rag.cjs `deleteProjectFile`) unless a file of that name is listed again by then; a delete's
tombstone drops it. Stricter than Node: the storage endpoint policy (as the browse's) is applied,
and a pruned directory entry is never followed through a symlink. Not serialized with core's
in-process source lock (a concurrent core upload of the same project is merged through
projects.json's version, not queued behind the delete). Every other method of the path is Node's.
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
grant); Node reads that row on every use (`getStorage`). Round 4 moves the remaining save
(`PUT /api/integrations/storage`), so Rust is that row's only writer (below). The storage latch stays in Node with its
automatic callers (the project refresh, the Diary backup): a folder or test call that works with
the saved login writes `storage-login-proofs.json` (`{version:1,proofs:{<userId>:<ms>}}`, no
credential), which Node's latch re-reads by its file signal (core shared-reread.cjs) and which
ends a hold set before it.

### Round 4: one writer for the storage connection

| route | owner | capability / Core refusal | notes |
|---|---|---|---|
| `/api/integrations/storage` (PUT) | rust | `rust-storage-connection` / `RUST_STORAGE_CONNECTION_OWNED` | GET stays the metadata worker's; other methods stay Node's. Session gate (device tokens refused), the JSON-object body (1 MiB), then core's order: the fields, the member-origin rule, the region, the WebDAV/Nextcloud login check (PROPFIND Depth 0, 10 s, no redirect, no environment proxy; a 401 keeps the old row), the save and its `storage.update` audit row in one transaction, and the login proof when the checked login is the saved one |
| `/api/providers/chatgpt/device`, `/device/poll`, `/device/cancel` (POST), `/api/providers/chatgpt/models` (GET) | rust | `rust-chatgpt-oauth` / `RUST_CHATGPT_OAUTH_OWNED` | the token broker (chatgpt_oauth_routes.rs): session gate, the `chatgptOAuth` flag (off: 404), then core's device sign-in (5 starts per 10 minutes, 3 pending per account, 15-minute TTL, the poll interval and the cancel/disconnect race), the token store (`chatgpt_oauth_tokens`, `enc:v2` bound to the owner) and the catalogue cache (5 minutes, failures 1 minute, a 10-second refresh gap, one fetch in flight per account). Outbound to Node's hosts only (a loopback QA server may replace them), no redirect, no environment proxy, 15 s, replies capped. Node's chat asks the internal store for access tokens (`/v1/chatgpt/session`, refresh-ahead and single-flight refresh per account, compare-and-swap writes) and never sees a refresh token; core's database guard refuses every Node write of the table but the rotation's reseal. DELETE `/api/providers/chatgpt` was Node's in round 5 and is the provider registry's since round 6 (below), which deletes the tokens, the provider row and the projects' fallbacks (the broker's first use after a start still sweeps the token rows of accounts that no longer exist, for the account deletion Node runs). The exchange, a refresh and the catalogue fetch run to completion even if the caller disconnects mid-request |
| `/api/providers/{providerId}/models{*?}` (every method) | rust | `rust-provider-models` / `RUST_PROVIDER_MODELS_OWNED` | provider_models_routes.rs: session gate, then on the exact `/api/providers/<id>/models` (other shapes and `<id>` = `chatgpt` go to Node) core's 405, `decodeURIComponent`, default-row and unknown-row refusals; the row read per request from `shared-providers.json` and `users/<id>/providers.json` (`enc:v1`); a ChatGPT row lists the broker's catalogue (with no stored row yet, the broker's token row stands for it); other rows `GET <base>/v1/models` as the probe (no redirect, no environment proxy, 8 s, 8 MiB) behind the member-origin rule, cached per (account, row, address, key digest) for ten minutes with core's ten-second refresh throttle and shared in-flight fetches |
| `/api/providers/test` (POST) | rust | `rust-provider-test` / `RUST_PROVIDER_TEST_OWNED` | session gate, the body (1 MiB; empty is `{}`; a `null` body is no longer Node's 500), core's address, length and member-origin rules, then `GET <base>/v1/models` with no redirect, no environment proxy, 8 s for the whole exchange and an 8 MiB reply cap; a stored key is reused only for a row the caller may edit, with a real key, at the same origin, read from `shared-providers.json` and `users/<id>/providers.json` per request (`enc:v1`, golden vector shared with core); other methods stay Node's |

Every Node writer of `storage_connections` is gone: the PUT; `saveStorage` (deleted from core
auth.cjs); and core index.cjs's legacy WebDAV setup (an administrator's legacy workspace with
`CORPUS_BACKEND=webdav`), which now asks the internal store (`POST /v1/storage/legacy-webdav
{ user, baseUrl, username, secret, corpusRoot }` -> `{ saved }`, saved only while the account has
no row or a `local` one, checked inside the write). Core's database guard (rust-auth.cjs
`FRONT_TABLES`) refuses every Node INSERT, UPDATE or DELETE of the table with
`RUST_STORAGE_CONNECTION_OWNED`, even inside `exempt()`, except one statement: `UPDATE
storage_connections SET secret=? WHERE user_id=? AND secret=?`, the same plaintext resealed under
the current key (`getStorage`'s legacy upgrade, `secrets:rotate`), which changes nothing if Rust
saved the row in between. The seal is `enc:v2` bound to the account (the round-3 golden vector).

Left on Node in this batch, and why (each resolves Node memory, a Node-only cache or an in-process
job that chat or another Node route reads):

| route | why it stays |
|---|---|
| `/api/connectors`, `/api/connectors/nextcloud/policy` | the Nextcloud half lists the live MCP servers' tools and which write (round 7 moved the Google Drive writes and the off-site backup, below) |
| `/api/admin/users/{id}` (DELETE) | the clean-up spans Node's workspace cache, MCP OAuth and directory MCP keys in memory (the ChatGPT tokens through the broker's `/v1/chatgpt/forget`) |
| `/api/auto-roles` | GET reads the served catalogue and machine models; an admin PUT warms the roles in Node's engine (`ensureRolesLoaded`) |
| `/api/health` | `ragAvailable` and the `retrieval` tri-state are Node's cached RAG status (Node-live); the default provider row is now env-derived in the front (provider_routes.rs `Deployment`), so only that status needs publishing (round 6 below) |

## sbstndalton/noevia#2190 round 6: the provider registry (group B2)

| route | owner | capability / Core refusal | notes |
|---|---|---|---|
| `GET`, `POST /api/providers`; `PUT`, `DELETE /api/providers/{id}` (a `test` id included); `DELETE /api/providers/chatgpt` | rust | `rust-providers` / `RUST_PROVIDERS_OWNED` | provider_routes.rs: the front is the only writer of `users/<id>/providers.json` and `shared-providers.json`. Other methods on those paths (a PUT on `/api/providers/chatgpt`, a GET of one id) stay Node's |

The listing is the workspace's merged view as core showed it: the env-derived default row first
(`DEFAULT_PROVIDER_ID`, `DEFAULT_PROVIDER_LABEL`, `INFERENCE_BASE_URL`/`LEMONADE_BASE_URL`,
`INFERENCE_API_KEY`/`LEMONADE_API_KEY`; `managed` is `MODEL_MANAGER_KIND` being `lemonade` or
`llamacpp`, whose engine capabilities the row carries), the shared rows, then the account's own (a
`lemonade` row read as the default, the first row of an id, shared-flagged private rows hidden).
Each row is core's `listRow`: the key masked to its last four characters (never the key), the
context window and capabilities (a row without its own gets the OpenAI preset for that address),
`keyUnreadable`, `external` for a trial-terms host (crates/provider-egress), and for the ChatGPT row
its `kind` and the token broker's `connection`. ChatGPT rows are listed only while the
`chatgptOAuth` flag is on. A token row without a provider row gets its row at the listing, and the
broker writes the row in the same step that saves a sign-in (inside its connect fence), so a
disconnect either wins before it (no row) or removes tokens, then the row.

Writes are locked read-modify-writes of the file as it is now (crates/server-projects
`provider_rows`), so two requests cannot lose each other's rows; a row nobody edits is written back
as it was (an unreadable key keeps its ciphertext, #782), a plaintext or retired-key key is sealed
`enc:v1` under the current key at every save, and a damaged file is never overwritten (500). The
checks and their order are core's (the POST: JSON object, shared rule, label, context window, address
and member-origin rule, capabilities; the PUT: the default, an unknown or sign-in-managed row and a
member's edit of a shared row are refused before the body is read; a stored key never follows a row
to another origin). The DELETE of the ChatGPT row (either path) deletes the broker's tokens first (a
failure leaves the row), then the row, then clears `provider` and `model` of the account's projects
that used it (a locked edit of projects.json). Every change runs as a spawned task the handler
awaits, so a client that leaves cannot strand the tokens/row/fallbacks steps.

Node keeps no copy: `privateProviders` is a re-read of the file by its change signal
(shared-reread.cjs; a file that cannot be read means no private providers), the shared rows are read
on every access, and its one write left is sealing a plaintext key at load, through the project store
(`providers.json` is a store file: `/v1/read` and `/v1/write`, compare-and-swap). Node bugs fixed on
the way: a `%`-escape `decodeURIComponent` refuses in the id threw (500; now `400 no such provider`);
a save could drop a concurrent writer's rows (Node rewrote its whole cached list); the
`/api/providers/chatgpt` disconnect and the row delete share one path, so a disconnect cannot leave
tokens without a row or a re-added row.

### Round 6 review: what the follow-up changed

- **Key rotation goes through the front** (`rust-providers-reseal`; core `rust-providers-reseal.cjs`):
  the internal store answers `POST /v1/providers/reseal { scope: "user" | "shared" | "all", user? }`
  (provider_reseal.rs) with one pass per file under the file's own lock (`providers.json.lock`,
  `shared-providers.json.lock`) and the current and previous key: a key under the previous key is
  `rotated`, a plaintext one `upgraded`, one already `enc:v1` under the current key `current`; a key
  no available key opens, an `enc:v2` one or a non-text one is a `failed` row left exactly as it was.
  The answer is `{ ok, tables, failures }` (`shared_providers`, `user_providers:<id>` to
  `{ current, rotated, upgraded, empty, failed }`; failures name the table and the row's position,
  never a key); without credential keys it is 503 `credential store unavailable` and nothing is
  touched. Core's rotation awaits it before its own table pass and no longer writes either file (it
  wrote each key back by row position, so a delete in between could put one row's key on another).
  The shell form of the rotation cannot reach the store (its secret is the web process's alone), so
  it refuses and says to rotate through the running app (`POST /api/admin/secrets/rotate`). The pair
  is checked like `rust-providers`: a Core that rotates through the store needs a front with the
  capability and the reverse.
- A row in `shared-providers.json` is shared whatever its `shared` flag says (the write goes to that
  file, so the administrator check follows it there): a member's edit or removal of a flagless row
  there is 403, and the listing shows `shared: true` for it.
- A removal clears the caller's project fallbacks whether or not the row was still there, so a
  removal that went through but could not update the projects (`503 Removed; its projects could not
  be updated, remove again to finish`) is finished by removing again (the 404 of a gone row now
  clears them first).
- Every change makes the account's directory first, with the legacy claim and the second gate the
  project routes use (`project_routes::ensure_account`): a new account's first provider write no
  longer answers 503.
- The listing's heal of a ChatGPT row whose sign-in was saved without it re-reads the token row and
  writes the row in one step under the broker's fence, so a disconnect that landed in between is
  not undone.
- While the credential keys cannot be loaded, creating, editing or removing a row that holds (or
  would hold) a stored key answers 503 `The credential store is unavailable. Try again shortly.`;
  an edit used to read the stored key as empty and could move the row to another address with its
  ciphertext.

### Round 6: still on Node, and the plan for each

| route(s) | why it stays | plan |
|---|---|---|
| `/api/connectors*` (Nextcloud policy and the list's Nextcloud half) | the Nextcloud half lists the live MCP servers' tools and which of them write | Google Drive's half moved in round 7 (below): the Drive device sign-in, the sealed refresh tokens and the access-token refresh are the front's, and Node's chat tools ask it over `/v1/gdrive/*` |
| `/api/health` | **moved in the close-out (below)** | ~~Node writes `rag-status.json` (`{ version, ragAvailable, retrieval, at }`, atomically, on a timer and when the status changes); the front reads it per request, probes the default provider and the Diary itself, and reports a snapshot older than a bound as `retrieval: "unavailable"`~~ |
| `/api/auto-roles` | GET needs the served catalogue (names, `missingFile`) and the machine models, both Node-live; PUT warms the roles in the engine | publish the catalogue and machine-model names in the same snapshot; GET moves with it; PUT stays Node's (chat-live) unless the warm-up is split from the save. **GET moved in the close-out (below)** |

### Round 7: Google Drive's token authority and the off-site backup

| route | owner | capability / Core refusal | notes |
|---|---|---|---|
| `/api/connectors/gdrive/connect`, `/disconnect` (POST), `/policy`, `/backup-copy` (PUT) | rust | `rust-google-drive` / `RUST_GOOGLE_DRIVE_OWNED` | connector_routes.rs: any signed-in account, its own connection only (`ownsBackup`: the backup slot doubles as the administrator's Drive); answers core's connector object; other methods stay Node's 404 |
| `/api/admin/offsite-backup`, `/run`, `/verify`, `/copy`, `/google/connect`, `/google/disconnect`, `/new-store`, `/recovery-key` (every method) | rust | `rust-offsite-backup` / `RUST_OFFSITE_BACKUP_OWNED` | offsite_routes.rs: administrators only (403 `Administrator required`), 405 for the wrong method; `POST /new-store` is the review's "this is a new store" action (below), behind the CSRF check like every POST |

Node's `server/gdrive.cjs`, `drive-accounts.cjs`, `offsite-*.cjs` and `routes/offsite-backup.cjs` are deleted.

**The broker** (gdrive.rs) owns all Google OAuth state: each account's device sign-in and sealed refresh token
(`google-drive-users/<id>.sealed`, key `secrets.cjs derive('google-drive-user')`, the retired key still read
until a rotation re-seals it) and the backup slot (`google-drive.sealed`, key `OFFSITE_BACKUP_KEY_FILE`). The
on-disk format is unchanged (crates/google-token-seal: HKDF-SHA256 file key, AES-256-GCM, base64 of
iv|tag|body), so a Node build reads what Rust saved; the golden vector is in both repos. The refresh is single
flight per slot; a disconnect never waits for it and a refresh that finishes after a disconnect or a newer
sign-in is discarded (the sign-in's identity is a digest of the refresh token and `connectedAt`, never the
token); every state-changing request runs as a spawned task, so a dropped request strands nothing. Google
endpoints other than a loopback QA server are not accepted (`GOOGLE_OAUTH_BASE_URL`, `GOOGLE_DRIVE_API_BASE_URL`,
`GOOGLE_DRIVE_UPLOAD_BASE_URL`; core took any URL).

**Node's chat tools** ask the broker over the internal store (`POST /v1/gdrive/<op>`, loopback, per-start secret):
`session {user, admin, rejected?}` -> `{accessToken, expiresAt, generation}` or `{error, code, status}` with
`disconnected`, `reconnect` or `transient`; `reject {user, admin, generation}`; `disconnect`; `forget {user}`
(account deletion); and three more than the plan named: `view` (the Google Drive half of `GET /api/connectors`, whose
Nextcloud half is Node's), `policy {user, tools, writes, mode}` (the Nextcloud permissions, written by the front
because it is `tool_policies`' only writer) and `reseal {}` -> `{current, rotated, empty, failed}` (counts only):
`secrets:rotate` re-seals the accounts' token files under the current `secrets.key` here, under the same fence as
every other change to them, loading the keys from disk at that moment; Node's rotation awaits it first and then only
checks that every file opens under the current key (it writes none, so a rotation can never resurrect a disconnected
account or drop a remembered folder). `admin: true` in a request is checked against `users.role`; a claim the table
does not confirm is a member, and a table that cannot be read is a 503, not a member. A refresh token never reaches Node. Requests have two time budgets: 30 s for a token,
form or JSON exchange and 300 s for an upload. The front also removes the connection files (revoked at Google first) of
accounts the `users` table no longer has, once at start (never when the table is unreadable or empty), and checks the
state file once a minute, rewriting it only when what it says changed (the backup key file appearing or going flips
`backup.usable`); writes of it are serialized. The broker rewrites
`gdrive-connections.json` after each change (no secret, no email: the accounts that have a connection file and its
state, and the backup slot's `{usable, state, owner, expiresAt}`; at start only when there is a connection file or an older
state file to correct, because Node reads a missing file as "nobody is connected" and the corpus replay compares the data
directory; the front makes `google-drive-users/` (0700) the first time an account's Drive is asked for, as core's `forUser` did); Node's sync `connected(user)` and its
60-second access-token cache follow that file's signal, and the shared cases in `gdrive-owns-backup.v1.json`
prove Node and the front agree on `ownsBackup`. `tool_policies`: a per-statement grant (server-store
`RustToolPolicies`, `upsert_tool_policy`, never a delete); core's guard refuses every Node write even inside
`exempt()`; Node reads it per call and its permitted-tools cache empties itself when the table's content
fingerprint or the state file moves.

**The off-site backup** (offsite_*.rs): the format is core's `noevia-offsite-v1` byte for byte (crates/offsite-backup-format;
a store Node wrote is read by Rust, tests/fixtures/offsite-node-store and offsite-node-store-first-release, the latter
written by the first released engine, core commit d4e31d7, whose manifests have no `incomplete`). A manifest missing
`root`, `size`, `mode`, `mtime` or `time` still lists and still protects its chunks (core checked only `format` and
`files`); a missing time is the snapshot id's. The reusable crates:
`secret-envelope`'s `enc:v2` is the credential seal, not this format; `diary-backup-format` and
`workspace-restore-verify` are the Diary workspace archive's and do not fit; `s3-sign` and `dav-parse` (the XML scan)
do. Differences from core: a live SQLite file is copied with `VACUUM INTO` on a read-only connection (core used the
online backup API), a file that cannot be copied consistently is skipped and the manifest flagged `incomplete`.

*What a run guarantees.* A backed-up folder that is missing or unreadable fails the run before anything is written.
Every other skip (a file that vanished or cannot be opened or read, a name that is not UTF-8, an unreadable subfolder, a
database without a consistent copy) is counted, flags the manifest `incomplete` and shows as `lastBackup.incomplete` and
`lastBackup.skipped` in the status (the web page does not show them yet: a follow-up in noevia-web). A destination that has
no `config` is not initialised once `lastBackup` exists for the same destination (`backupDestination` in the status
file): an unmounted NAS is an empty directory, and a fresh store there would start a second chain and a `noevia-offsite (2)`
folder on Drive; the error says to check the mount, and names the remedy for a destination that really is new (a replaced
disk, a recreated bucket at the same path or name, which no digest tells from an unmounted one): an administrator confirms
it with `POST /api/admin/offsite-backup/new-store` (administrators only, CSRF-gated, no body, 409 while a job runs or when
another process holds the job lock). That clears `lastBackup`, `backupDestination`, `backupRetry`, `snapshots` and
`unreadableSnapshots` from the status file, and `lastError` when it is the empty-destination refusal (any other failure
stays), so the next backup initialises the destination and records it again. It refuses (409, nothing changed) when the
destination already holds a store (`config`): that is not a new store, and the status of a working one is not forgotten.
*A status file from before this release* (it has `lastBackup` and no `backupDestination`) counts its last backup as made to
the current destination: the digest is only compared when one was recorded, so an empty destination is refused after the
upgrade too, until a good backup stamps the digest, and the same action applies. The snapshot is recorded (`lastBackup`) as soon as it is stored; a
retention failure after it is recorded on its own (`lastError` "saved, but applying retention failed") and is not a failed
backup. Retention is 7 daily, 4 weekly, 6 monthly, always the newest snapshot and the newest complete one, and within a
bucket the complete one; chunks are pruned only when every snapshot the store lists was read (a flaky or damaged manifest
read must not turn its chunks into garbage), the snapshot just written is passed in and kept, and a chunk younger than
24 hours is never pruned. The restore test reads the newest listed snapshot back chunk by chunk without writing it
anywhere and fails when that snapshot cannot be read, instead of passing an older one. A manifest with an entry that
names no `chunks` array still lists, restores and is kept, but what it protects is unknown, so it counts as unreadable
for the prune (nothing is pruned while it is there; `unreadableSnapshots` counts it). A restore test that passes clears
`lastError` only when that error is its own (`during` "restore test"), not a backup's.
`<data dir>/offsite-backup.lock` (an exclusive `flock`) holds the one-job-at-a-time rule across processes. It lives in the
UI data directory (production: btrfs, where `flock` works), never in the destination (production `/offsite` is a FUSE
mount), and is left out of the backup. A job that finds it held is refused with a 409 that is nobody's failure: it writes
neither `backupRetry` nor `lastError`. A `flock` that fails for any other reason (a file system without locks) logs a
warning and the job runs unlocked: the in-process busy mark still holds. A job ends at the latest after 6 hours
(`JOB_DEADLINE`): the busy mark and the lock are released and the failure recorded. "Run now" copies the stored snapshot
to Drive even when the retention after it hit that deadline.
Leftovers of a killed run (`.tmp-` files in the destination older than 2 hours, `noevia-sqlite-*` and
`noevia-restore-test-*` directories older than 7 hours in the system temp directory) are swept; a full destination has its
own message.

*Schedule.* Every 15 minutes (a slow check delays the next, it is not followed by a burst) a run starts when the clock
is within six hours after `OFFSITE_BACKUP_HOUR` (core: only inside that hour), the last backup is over 20 hours old, no
quiet window is open and the last failure's backoff is over: 15 minutes doubling to 2 hours, at most three scheduled
attempts per window (`backupRetry` in the status file; `lastError.failures` shows the count). Three zones are named and none is guessed.
The hour is read on the process clock, exactly as core's `new Date().getHours()` did: the `TZ` the process runs with, and
UTC without one (the production web container sets no `TZ` and its `/etc/localtime` is `Etc/UTC`, so there Node's `getHours()` and this agree on UTC; the status shows `scheduleZone`). The quiet
windows are read in explicit zones: the NAS window 02:20-04:30 always in America/New_York, the appdata backup 04:05-04:25 in
the host's zone, `OFFSITE_BACKUP_TIMEZONE`, default America/New_York (the Unraid host runs on Eastern time). Known names
are `America/New_York` (also `US/Eastern`, `EST5EDT`, `America/Detroit`; the US daylight-saving rule) and `UTC`/`Etc/UTC`/
`GMT`; an unknown `TZ` or `OFFSITE_BACKUP_TIMEZONE` is warned about at start (the unknown `TZ` is read as UTC, as Node did;
an unknown host zone is replaced by the default, never by UTC). The start also logs the zones it uses. On the two daylight-
saving days the windows follow the wall clock (the NAS window opens at 03:00 on the spring day, the repeated 01:00 hour of
the autumn day is the same night). A scheduled run pauses between files, never inside one, before it touches the
destination and between the backup and its retention; "Run now" does not pause, and a manual run asked while a scheduled one
waits for a window is told so (409) while the status shows `waitingForQuietWindow`.
Never in a response or a log: the recovery key (only its download), a token, the S3 secret. The mirror's folder rules
(`noeviaStoreId`, `noevia-offsite (2)`, the 500 and quarter prune guards, same-name-other-size refusal) are core's.

Behaviour changes (each a deliberate edit): the OAuth and Drive base URLs must be loopback; a sign-in whose token
could not be saved says so instead of staying pending; an invalid connectors body is `400 invalid JSON` (core
answered 400 `Google Drive could not be reached.`, or 502 for a non-object body); a missed nightly hour is caught up
within six hours; the status gains `waitingForQuietWindow` while a scheduled run waits, `scheduleZone` (once the
feature is on) and `unreadableSnapshots` (once a retention counted), none of them while the feature is off, so the
recorded corpus's answer of a disabled backup replays byte for byte; `lastBackup` gains `incomplete` and `skipped`; `/api/admin/offsite-backup/<other>`
is Node's generic 404.

## sbstndalton/noevia#2189 (batch M2-A): the read-only model sizing reads

`GET /api/models/estimate?model=` (the "Will it fit?" inputs) and `GET /api/models/preset/suggest?model=`
(the size-based preset suggestion) are the front's (`src/model_sizing.rs`), permanently and GET only. Core
refuses both GETs before authentication with 503 `RUST_MODEL_SIZING_OWNED` (`server/rust-model-sizing.cjs`,
required from `index.cjs`) and `--features` lists `rust-model-sizing`; the web supervisor and the release
assembly pair the two. Every other method stays Node's (an administrator check, then 405; corpus 000174).

| route | owner | reads |
|---|---|---|
| `/api/models/estimate` (GET) | rust | `LLAMACPP_PRESET_PATH` (models.ini, read only), `LLAMACPP_MODELS_PATH`/`LLAMACPP_CACHE_PATH` (the GGUF header and file sizes), the router's `GET /models` (the model's argv), the saved inference budget (`settings` key `inference:memory-budget`, `INFERENCE_MEMORY_BUDGET_GIB`, host RAM) lowered only by `LLAMACPP_AUTOCONFIG_MEMORY_GIB` |
| `/api/models/preset/suggest` (GET) | rust | the same, and the prompt-cache cap (`LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB`, `LLAMACPP_CACHE_RAM_HARD_MAX_MIB`) |

The sizing is crates/llamacpp-autoconfig's strict operations 1 and 2 and the header summary is crates/gguf's,
the same code Core ran through dav-parse.wasm; Core's answer was its JS estimator's confirmed by that port,
and the recorded answers are the port's text in every case. `tests/model_sizing.rs` replays 178 answers Core
gave for synthetic fixtures (`tests/fixtures/model-sizing/record-node.cjs`, recorded at Core `fbac2e4`) byte
for byte, and proves nothing is written and the router is asked only for its listing. Nothing is cached.

Kept on Node, and why: `GET /api/models/evidence` computes the model's identity from router args, the
engine's `/props`, a file fingerprint and the preset hash, and keeps it in a 30 s cache the evidence writers
(calibration, auto-tune, recheck, import) read and clear; serving it from a second process would need that
cache or a second identity writer, so it moves with the evidence writers (batch B). `inference-budget` (its
PUT and the boot-time saved value), `installed`, load/unload, delete, calibration, auto-tune, machines, preset
GET/PUT and `/api/model-manager` stay too.

Not Node's answer, each unreachable in a working deployment: Core always has an inference budget of at least
2 GiB, so the boot-time `LLAMACPP_MEMORY_LIMIT` fault and the "no budget" 501 never show (both kept or
documented in model_sizing.rs); a settings store the front cannot read answers 503 (Node holds the value in
memory); GGUF metadata with NaN, Infinity or -0 in a raw field (float-typed headers only) asks the port with
`null`/`0` where the JS estimator saw the number; a router listing nested 64 deep at the fields read counts as
no listing.


## sbstndalton/noevia#2191 round 4 (group rust-project-document-pages): a stored document's pages

`GET /api/projects/{id}/documents/pages` is the front's (crates/server-projects `document_pages`);
core refuses the GET with 503 `RUST_PROJECT_DOCUMENT_PAGES_OWNED`
(`server/rust-project-document-pages.cjs`) and `--features` lists `rust-project-document-pages`.
core routes/projects.cjs `docRead` with document-sources.cjs `readPages`, answer for answer: the id
decoded (a bad escape 500), `getProject` (its self-heal saved), the file named by the first `name`
query value with a `document`, else 404 `no such document`; `startPage`/`endPage`/`offset` as JS
`Number(...)` (an empty value is the default); 1 to 5 whole pages and a whole non-negative offset,
else 400; no `availableVersion` 422; past the document 400; a page the extraction did not keep 422;
any other failure with the stored `project-documents/<sha256 id>/<version>.json` (missing, not
JSON, a leading BOM, a shape the JS throws on) Node's 422 `Saved document data is unavailable;
upload or refresh again.` The text is cut at the UTF-16 offset to 8 000 units (`nextOffset`), with
the document's `notice` (quoted name, state, page outcomes and OCR-retry notes, versions, index).
Stricter than Node: the result is opened with `O_NOFOLLOW` and read only up to 256 MiB. The file is
written once per version by core's extraction (an atomic rename); reading it never takes the
large-document lock (Node's reader did not either). Every other method of the path is Node's.

## sbstndalton/noevia#2191 round 4 (group rust-conversation-import): Settings → Data → Import conversations

`POST /api/import/conversations` is the front's (crates/server-projects `import`); core refuses the
POST with 503 `RUST_CONVERSATION_IMPORT_OWNED` (`server/rust-conversation-import.cjs`), keeps its
405 for every other method, and `--features` lists `rust-conversation-import`. core routes/import.cjs
with conversation-import.cjs and chat-import.cjs, answer for answer: the body read up to 64 MiB
(413 `That file is over 64 MB…`; unreadable or not JSON 400 `Choose a conversations.json…`),
`planImport`'s refusals (400 with their words) before anything is written, then Node's recovery
journal, file for file (`conversation-imports/<sha256 of JSON.stringify(body)>/` with `<n>.json`
stages and `manifest.json`, `atomicJson` 0600), so a journal Node left is finished here: every
pending journal is applied, audited (`import.conversations`, `{imported, skipped, projectsCreated}`)
and removed first, and the same file's finished journal answers its own result. Projects are
matched by name or created through the round-2 create (`{name, toolboxes: []}`, its storage step
included); transcripts are published only where none is (origin `import`, a taken id gets a new
one), lists merged as `mergeChats` merges them; a chat the list cap dropped is `capped` and taken
back (origin `remove`) only while it is still the imported transcript. Any other failure is Node's
503 `Import was interrupted…`, the journal kept for the next import. Imports of one account run one
at a time (Node: per process; the front is the only process answering it now).

## sbstndalton/noevia#2191 round 5 (groups rust-document-extraction, rust-project-documents): Docling is the extraction engine

`--features` lists `rust-document-extraction` (the front's extraction pipeline and its worker) and
`rust-project-documents`: `POST /api/projects/{id}/documents` is the front's
(crates/server-projects `project_documents`), core refuses the POST (any query, `?background=1`
included) with 503 `RUST_PROJECT_DOCUMENTS_OWNED` (`server/rust-project-documents.cjs`).

**Behaviour change (the 2026-10-30 decision).** Docling, with the OCR worker for the per-page retry
of pages Docling left incomplete, is THE extraction engine for new project documents. The front has
no in-process PDF reader: core's pdf.js walk (`documents-native.cjs`) is not ported. Where core
would have used it (Docling not configured, or a PDF over 25 MiB whose worker does not read page
ranges), and whenever Docling is unreachable, busy or answers a transient error, the document is
stored exactly as core stored an unreachable worker's result: the original, a result record with
`retryable: true`, the source record with `document.state: "failed"` and the worker's own words in
`document.error` (`fetch failed`, `Document extraction is busy; refresh to retry.`, `Document
extraction unavailable (HTTP n); refresh to retry.`, `Document extraction is not configured.`),
`indexing: "unavailable"`, or the last readable text kept (`stale: true`) when the upload replaced a
readable document. One field is added, the smallest marker of the new state:
`document.extraction: "queued"`, present while the document's job waits and gone once it is read.
Without Docling, `isDocument` accepts a PDF only (it is queued); with Docling, its formats. A refusal
(415, 422) is final and never queued, as in core. Documents already stored are never extracted again
(a stored readable record is reused as it is; a record without the completion marker gets that pass).

**Pipeline** (crates/server-projects `extract`, `document_ingest`, `extract_queue`; bins/noevia-server
`extraction`): core documents.cjs / document-sources.cjs / docling.cjs / ocr.cjs on the Docling path,
answer for answer. Routing: plain-text names are never documents. First pass: one whole-document
request (`POST {DOCLING}/extract`, `application/octet-stream`, `X-Document-Name` the last path
segment, Latin-1 or percent-encoded, `X-Document-Figures` with multimodal RAG, a deadline of 65
minutes), or the first range (`X-Document-Pages`, 2 min + 1 min a page) of a PDF over 25 MiB when
`/health` says `ranges` (and the size is within `rangedMaxBytes`). Completion pass
(`ranges-ocr-retry-v1`): the pages past the first window in ranges through Docling up to
`PROJECT_DOCUMENT_PAGE_LIMIT`/`_TEXT_LIMIT`/`_RANGE_PAGES`, then each incomplete page retried alone
through OCR (`POST {OCR}/extract`, `X-OCR-Pages`, `X-OCR-DPI: 400`, at most 20 pages, 3 per request,
330 to 610 s), with the busy waits (5 s, 15 s) and the 3-attempt bound; a stop for a passing reason
leaves the marker off and the job queued. Reply caps 32 MiB (Docling), 16 MiB (OCR), 64 KiB
(`/health`, kept 5 minutes). Large-document lock: one document over 25 MiB at a time in the front's
process, a waiter busy after `LARGE_DOCUMENT_ACQUIRE_WAIT_MS` (2 min). Workers are where
document-workers.cjs finds them: the machine Settings → Inference machines assigned (cowork.db
`inference:machines`, read at call time; connected to only when every address of its name is on
this network), else `DOCLING_BASE_URL` / `OCR_BASE_URL`; no redirect, no credential. Files, in core's
formats: `project-documents/<sha256 id>/<byteHash>.pdf` (written once), `<version>.json` with
`version = sha256(byteHash:docling-pages-v1:<docling VERSION>[:figures-v1])` (fsynced temp, rename,
folder fsync, 0600/0700), `<version>.fig<n>.<ext>` figures; the source text in
`project-sources/<id>/<textRef>.txt`; then an `index` job in the index outbox (core indexes).

**Queue** (`users/<id>/extraction-jobs.db`): key `["extract", project, file]`, target `byte_hash`
and `version`; queuing a key again replaces its target (idempotent; a newer upload supersedes).
Claimed `running`, deleted only at the version claimed; a `running` row left by a crash or restart
is pending again at the next start (at least once: a run re-reads the stored original and the
source as listed, and results are content-addressed). A passing failure waits
`min(30 s x 2^(attempts-1), 30 min)`; there is no last attempt. A source gone, renamed or holding
other bytes by then drops its job. The worker looks for due jobs every 10 s, one extraction at a time.

**Route** (core routes/projects.cjs `projDocs`, answer for answer): id decoded (bad escape 500),
`getProject`, 404 `no such project`, body up to `ceil(25 MiB/3)*4 + 512 KiB` (413), `JSON.parse`
(400 `invalid JSON`; `null` the TypeError's 500), `String(body.name || '').slice(0, 200)` and
`isDocument` (400 `<name> is not a supported document (PDF).`), base64 data (empty 400, over 25 MiB
413 with its size), then one source operation per project in the front's process: the 20-upload cap
(400), an existing upload without `replace: true` (409 `sourceExists`), the ingest, the project and
replaced upload unchanged (409 `Project changed; retry.`), the file listed last, `indexing` set as
`indexSource` sets it, the save (texts first), the document folder pruned against the saved list,
the `index` job, 200 `{ name, pages, characters, truncated, document }`. `?background=1` is answered
the same way, synchronously (no client sends it for this route). Not serialized against a core
source operation on the same project (core's source lock is in-process): the save is a
read-modify-write of projects.json under its lock, as round 3's delete.

## sbstndalton/noevia#2191 round 6 (group rust-source-jobs): the front owns the source jobs

`--features` lists `rust-source-jobs` and no longer lists `noevia-source-jobs-store/1`. The front is
the one owning process of the source-job store (crates/server-projects `source_jobs`, the journals
`users/<id>/jobs/<uuid>.jsonl`; `source_work` for the route side). It answers
`POST /api/projects/{id}/upload?background=1`, `POST /api/projects/{id}/sources/sync?background=1`
(`url.searchParams.get('background') === '1'`: the first `background`, form-decoded) and
`GET /api/projects/{id}/source-jobs/{jobId}`. Core refuses those three before authentication with 503
`RUST_SOURCE_JOBS_OWNED` (`server/rust-source-jobs.cjs`); its `source-jobs.cjs`, the store client's
`sourceJobsFor`, `SOURCE_JOBS_CLIENT_MARKER` and `workspace.sourceJobs` are deleted, and the internal
`/v1/source-jobs/*` store routes are gone from the front. Two processes can no longer own the same
account's journals, which was what kept these routes on Node (the store's `recover(owner)` admits one
owner per account).

**Start** (core routes/projects.cjs's background wrapper and source-jobs.cjs `start`, answer for
answer): the session gate, the body up to the legacy proxy's cap (80 MiB + 512 KiB for the upload,
33 MiB for the sync), the session gate again, the id decoded (bad escape 500), `getProject` (404
`project not found`), the body cap (413 `Request exceeds size limit`), this process registered as
the account's owner on its first use (earlier owners' active jobs interrupted: "The server restarted
before this finished."), then the job persisted created+started within the two-active cap (429
`Source processing is busy; wait for the current operation and retry.`; 409 for a newer owner, 410
for a removed account, 404 for a project removed meanwhile, any other store failure 503 `Internal
error`, as core's `sourceCall` and `errorResponse` worded them). The operation is then sent to Node
as the synchronous route (the same path, no query) with the client's own headers (its session and
CSRF pass Node's gate as before), the body as received and `X-Noevia-Source-Work: 1`. Node answers
NDJSON: `{"accepted":true}` once it has taken the request, one `{"stage":...}` per progress step,
then `{"done":{"status","body"}}` (the route's answer, any status) or `{"failed":{"status"}}` (it
threw). An answer before `accepted` is a plain JSON refusal (Node's 404, the storage hold of an
automatic sync, `invalid JSON`), which the front passes on unchanged while it abandons the job (the
request key tombstoned, the job interrupted, its slot freed). After `accepted`, the client gets core's
202 `{ "poll": "/api/projects/<encodeURIComponent(id)>/source-jobs/<job>" }`, and a task of the front
writes each progress step (`n` from 1) and the terminal row, whether or not the client is still
there: `job.completed` with `{status, body}`; `job.failed` with `{status}` and core's default
message for a thrown operation, or 503 when a progress write failed (no clean completion after it,
as core); a worker stream that ends before its result fails the job with 503 and core's restart
wording. A terminal write the store refuses for a passing reason is retried (5 s apart, 12 times),
then the job is abandoned so its slot is never held for good. A synchronous upload or sync a client
sends is still proxied to Node, with any `X-Noevia-Source-Work` it carries dropped.

**Poll** (source-jobs.cjs `read`): the gate, the id decoded, `getProject` (404 `project not
found`), the owner registered, the job read (the job id as the path carries it; another project's or
an expired job is 404 `Processing status expired or the server restarted; refresh or re-upload to
retry.`): completed `{done:true,status,body}` (an absent key left out), failed
`{done:true,status:result.status||500,body:{error}}`, interrupted or cancelled
`{done:true,status:503,body:{error:"The server restarted before processing finished; refresh or
re-upload to retry."}}`, otherwise `{done:false}` with `stage` when there is one. Keys in core's
order; JSON as JSON.stringify writes it (lone surrogates stay escaped).

**#1441 (a chat turn waits for a file it was just sent).** The operation still runs in Node, so
Node's in-process registry of running file operations (core `source-work.cjs`, names memory-only)
is what a chat turn waits on, as before; it no longer reads a journal.

**Behaviour changes.** (1) Order: the body is read before the project lookup (core looked the
project up first); the project 404 still wins over the 413, but a sync's storage hold and a
malformed body are now answered after the job slot check (core: before it), so with two jobs
running a held or malformed request is a 429 rather than its hold or 400. (2) A job whose Node
worker restarts mid-operation is failed by the front at once (503, the restart wording) instead of
at the next Node start. (3) A job whose front restarts mid-operation is interrupted at the front's
next use of that account's store, as a Node restart did before; Node may still finish the operation
(as when the old Node's journal writes failed). (4) The front's restart, not Node's, now interrupts
active jobs.


## sbstndalton/noevia#2191 round 7: review fixes for rounds 4+5

- **The extraction job is written before the record that says "queued".** `POST /api/projects/{id}/documents`
  wrote `document.extraction: "queued"` to projects.json and then the job to `extraction-jobs.db`; a
  queue write that failed was logged and answered 200, so no worker ever read the document. The job is
  now written first. A queue that cannot be written answers **503** `Document extraction could not be
  queued; nothing was changed. Try again shortly.`, and nothing is saved (the stored original is
  pruned). A save that fails after the job was written puts the name's earlier job back (or removes
  the new one); a job whose record is never saved is dropped by the worker anyway (`find_source`
  matches the byte hash).
- **One large document on the host, not per process (#697).** The large-document lock (documents over
  25 MiB) was a Mutex in the front and an in-process queue in core, so the two could hold a large
  document each. Both now also take `UI_DATA_DIR/large-document.lock`: an SQLite file whose holder
  keeps a `BEGIN IMMEDIATE` transaction open (one holder across processes; a holder that dies frees
  it), polled 200 ms apart with no busy timeout, inside the same two-minute acquire wait. A waiter
  past it is "busy" as before (`Large-document processing is busy; refresh to retry.`, a passing
  failure). Core's side is `server/large-document-file-lock.cjs`, taken in large-document-lock.cjs
  `acquire()`. It stays even once core extracts nothing large, so a rollback to an older front or
  core still pairs safely.

## sbstndalton/noevia#2202 round 1: MCP keys and sign-ins (capability `rust-mcp-credentials`)

The front is the broker of every MCP server credential an account holds, ported from core
`mcp-oauth.cjs`, `directory-mcp.cjs` (the per-account keys) and `routes/mcp-directory.cjs`
(`mcp_broker.rs`, `mcp_egress.rs`, `mcp_routes.rs`; core's `rust-mcp-credentials.cjs` refuses the
routes with `RUST_MCP_CREDENTIALS_OWNED`, and the web supervisor and the release assembly pair the two).
Node keeps the MCP client (a chat's live session) and the administrator's directory routes
(`/api/admin/mcp-directory*`, the registry, the shared key); the sign-ins, the registered apps and every
account's key are the front's.

| route | owner | notes |
| --- | --- | --- |
| `GET /api/mcp-keys/servers`, `PUT`/`DELETE /api/mcp-keys/{id}` | rust | the servers whose key each person supplies and whether this account has one; a key is proven against the server (a short MCP session: `initialize`, `initialized`, `tools/list` to its end, close) before it is kept, sealed `enc:v2` for its owner in `directory_mcp_user_keys` |
| `GET /api/mcp-oauth/servers`, `POST /api/mcp-oauth/{id}/connect`, `DELETE /api/mcp-oauth/{id}` | rust | the sign-in servers and this account's state (`connected`, `needsReauth`); discovery (RFC 9728, RFC 8414), registration (RFC 7591), PKCE S256 and a resource indicator; the disconnect deletes the row and asks the service to revoke it (RFC 7009, best effort) |
| `GET /api/mcp-oauth/callback` | rust (`client = false`) | the service's browser return: the state is bound to the account and server that started it and spent once; the code is exchanged with the verifier and the redirect address of the start; the tokens are sealed `enc:v2` for the account in `mcp_oauth_tokens` |

Internal store ops for Node (`POST /v1/mcp/{session,key,reject,start,client,client-info,forget,key-set,reseal}`,
loopback, the per-start secret): `session` hands one call's access token (never a refresh token) with a
generation digest; `key` one call's key headers; `reject` marks the sign-in a server twice refused;
`start`/`client`/`client-info`/`forget`/`key-set` are the administrator's directory routes' needs; `reseal`
re-seals the three tables for a secrets rotation (core's `runRotation` awaits it, like the providers' pass).
Node holds neither past the request.

What the front publishes: `mcp-credential-state.json` (0600, atomic, rewritten only when its content
changes): `{ version, oauth: { user: { server: connected|reauth } }, keys: { user: [server] } }`, no secret.
Node re-reads it with `createReread` for "is this account ready for this server?" (the permitted-tools view,
the chat turn's box filter) and for the permitted-tools cache key; a file it cannot read means nobody is ready.

Locking: one async mutex per (account, server) serializes the single-flight refresh, the re-seal and the
disconnect (a waiter re-reads the row, so a rotated refresh token is never spent twice), every write is a
compare-and-swap on the ciphertext it started from, one mutex per server serializes the app registration.
Work a client could abandon (the token exchange, a refresh, a key check and store, a registration) runs on a
spawned task the handler awaits; the revoke after a disconnect is detached.

Outbound rules (`mcp_egress.rs`, the `ssrf-policy` crate): `https://` with no placeholder or whitespace and no
credentials in the URL (a QA run may use `http://127.0.0.1:<port>/…` with `NOEVIA_QA_ALLOW_LOOPBACK_MCP=1`);
a public address, judged for a literal and for EVERY answer of a name, before the request and again at connect
time by the client's own resolver (DNS rebinding); no redirect is ever followed, no environment proxy, 8 s for
metadata and registration, 10 s for a token, 30 s per MCP step, replies capped (256 KiB OAuth, 8 MiB MCP).

Behaviour changes (each a Node bug or gap, with a regression in `tests/mcp_credentials.rs`):

- A grant the service reports gone (`invalid_grant`) now needs a new sign-in: the refresh token is dropped and
  the row expired, so the list says `needsReauth` instead of `connected`, and a dead sign-in is not tried again.
  A flaky service (5xx, timeout) never costs the sign-in, and a token still inside its life is kept; Node
  returned "no token" for any refresh failure.
- A server that refuses the token it was just given gets one retry with a token the broker issued past it
  (`rejected`); a second refusal marks that sign-in by its generation. Node had no retry.
- The sign-in service's issuer must be the one asked (RFC 8414 §3.3), the resource document must name the
  server asked about (RFC 9728 §3.3), and a callback that names its issuer (`iss`, RFC 9207) must name the
  one asked; Node checked none. A callback without a code is refused before anything is spent.
- A registered app is authenticated the way its registration said (`token_endpoint_auth_method`); core read a
  field only hand-registered apps had, so an app handed a secret was always sent Basic. `expires_in: null`
  is no expiry (core read it as already expired).
- Disconnect asks the service to revoke the grant (RFC 7009) when its metadata names a revocation endpoint.
- Round 1 review fixes: the resource a service describes may be the site or a parent of the server's path
  (RFC 9728 §3.3: same origin, a path on a segment boundary), never another origin; `invalid_client` and
  `unauthorized_client` on a refresh (a 400 or 401) end the sign-in like `invalid_grant` and forget a registration
  the broker made (a hand-registered app stays), while the same body on a 5xx is a bad moment; a service that
  advertises `authorization_response_iss_parameter_supported` must send `iss` (remembered with the app as
  `issParameterSupported`, an addition core ignores); a directory-server table that exists but cannot be read is an
  outage (503), not "no servers"; a refreshed sign-in is sealed and written twice before giving up (the service has
  already rotated the token); a token the MCP server refused that cannot be refreshed (no refresh token, no app)
  is marked for a new sign-in by the broker itself; the disconnect deletes the very row it read and, for an app an
  older core registered, finds the revocation address lazily (5 s, only a service of the issuer the app was
  registered with); a hand-registered app waits for a registration under way; one account runs at most four key
  checks at once (429); every internal-store refusal carries its `status`.
- A sign-in that finishes for a server removed meanwhile is not kept; pending sign-ins are capped globally
  (4000) as well as per account (20); registrations for one server are serialized.
- The start sweep removes the sign-ins and keys of accounts and servers that no longer exist; the rotation
  re-seals a sign-in under its refresh lock (a refresh during a rotation no longer loses the rotated token).
- URLs with credentials, names with a trailing dot or an internationalized label are refused for the MCP
  server and its OAuth service (the `ssrf-policy` rules; the JS check resolved them).
- The key check uses the front's own short MCP session; it needs the server to answer `initialize` and
  `tools/list`, and no longer converts the tools (a server whose tools noevia cannot convert still takes a key).

Left on Node (chat-bound): the MCP client itself, discovery of a server's tools, the approval gate for MCP tool
calls (Allow once, Decline, Allow for this chat), and the administrator's add/replace/remove of directory servers.

### #2202 round 1: what the broker unblocks, and what stays on Node

`GET /api/toolboxes/permitted` is still Node's. Its rules are Rust (the `toolboxes-permitted` crate, run in
Node's wasm); its inputs are the project's selection (Rust's `projects.json`), the account's connections
(Rust's storage row; Drive), the MCP readiness (this round: the broker's published file), the tool permission
store, ENABLED_TOOLBOXES / Kiwix / CODE_REPOS (environment) and the discovered MCP catalogue. Three of those
are still Node-live: the discovered boxes and their tool schemas (live MCP sessions), the per-user tool
permission store (its reads are Node's; `tool_policies` has had the front as its only writer since settings round 7, #2200), and the
Drive state, which the front's Drive broker now answers per call (`/v1/gdrive/view`) but which this view does not
yet read as a file. What this round did for it: the
readiness input is a file, and core's cache key now carries the project's toolbox selection, the connected
connectors, the readiness and the last discovery (it omitted all of them, so a project edit or a new Nextcloud
connection by the front showed up to 30 s late). The move itself needs Node to publish its offered catalogue
(box ids, tool names, write flags) after each discovery as a signal-checked file and the permission store to get a
Rust writer; then the view is a pure function of files the front already reads.

`GET /api/connectors` is still Node's: the Drive half is the Drive broker's view (the front, `/v1/gdrive/view`, since
settings round 7), and the Nextcloud half is the storage connection (Rust) joined with the tool permission
store, whose reads are Node's (it knows which live MCP tools write; the front is the table's only writer).
Both halves publish together or not at all.

Chat-bound, by design: `POST /api/tool-approvals` (the parked approval promise lives in the live chat loop; the
three actions - Allow once, Decline, Allow for this chat - and their tenant scope stay in Node) and `GET
/api/toolboxes` (prefill samples come from the chat stream, the MCP block from live discovery state).

## sbstndalton/noevia#2191 round 8 (group rust-project-source-work): the front runs the source operations

`--features` lists `rust-project-source-work`. `POST /api/projects/{id}/upload`, `POST .../sources/sync` and
`POST .../folder-import/plan` are the front's whole, in the foreground and (round 6) as source jobs
(`?background=1`); core refuses the three POSTs, whatever the query, with 503
`RUST_PROJECT_SOURCE_WORK_OWNED` (`server/rust-project-source-work.cjs`, before authentication), and
`server/rust-source-jobs.cjs` now refuses only the poll. There is no `X-Noevia-Source-Work` worker call, no
NDJSON answer, and core's `source-work.cjs`, `folder-import.cjs`, `pdf-reduce.cjs` and the routes' copies of the
upload, the sync and the plan are deleted. The web supervisor and assemble-release.sh refuse a mixed pair (they
also refuse a pair whose two halves name different storage-latch or file-operation files, or change the latch other
than with `BEGIN IMMEDIATE`); old/old stays valid.

**Streamed binary storage read** (crates/server-storage-reads `stream`). The storage child's sandbox is unchanged
(RLIMIT_AS 256 MiB, the 64 MiB wire cap, 25 MiB `WRITE_BYTES`). A file is read as ranged GETs, each one attempt in
a fresh child, and a reply carries at most 8 MiB (`CHUNK_BYTES`) behind a summary of the response's headers
(`CHUNK_PREFIX`: the `Content-Range` total, first and last byte, and the ETag; a content coding is refused). The
chunk's body has the whole attempt's time (20 s), not 10 s. The whole file
is capped at 96 MiB (`FILE_CAP`) on the size the first reply declares, before the caller's hook runs and before a
byte reaches the sink, and again on the bytes received. A 206 must carry exactly the range asked for and the same
declared total throughout, else the read is refused mid-stream; a server that ignores `Range` (a 200) is served only
when the whole file fits one chunk. The first reply's strong ETag is sent as `If-Range` on every later chunk and
every reply must carry it: a file replaced by one of the same size mid-read is a 502 "the file changed in storage
while it was being read", never a file made of two (a server with no ETag, or only a weak one, leaves only the size
checks). A `416` after the first chunk of a file whose size was declared is that same 502 (the file shrank); it is
the end of the file only when no size was declared. The front writes the chunks to a temp file under `users/<id>/source-staging/`,
hashing and fsyncing, and renames it to its hash (`staging`); a failed read leaves nothing, and a file a dead process
left is swept. The large-document lock is taken from the declared size before the body (`beforeBody`), or first when
the listed size is large or unknown (#1872), and is re-entrant within the thread that holds it (the sync's read and
its extraction).

**Behaviour changes.**
1. A file larger than 96 MiB is refused (413) on its declared size, whatever `PROJECT_DOCUMENT_READ_BYTES` says (core
   would read up to that setting); the default is 96 MiB.
2. A storage server that does not honour `Range` can still serve a file that fits one 8 MiB chunk, and no larger one.
3. Docling is the only extraction engine: there is no pdf.js fallback, and a document whose Docling is down is
   queued (`document.extraction: "queued"`, its job in `extraction-jobs.db` written before the record that says so;
   a queue that cannot be written is a 503 and nothing is saved), in the sync and the uploads alike. A stored document
   is never extracted again or reopened (there is no `reopenStored`). A document the sync's OCR allowance
   (`PROJECT_SYNC_OCR_PAGES`) left open gets no job: it stays open and the next sync continues it, as core's did; only
   a document whose Docling is down is queued.
4. The storage latch is no longer Node's memory: it is `UI_DATA_DIR/storage-latch.db`, one row per account, changed
   by both processes with the same transitions under `BEGIN IMMEDIATE` (core `storage-latch.cjs` for the Diary
   backup worker, `storage_latch.rs` for the sync), so a refused login held by one holds the other's automatic calls.
   The latch is durable: a hold survives a restart of either process (core's was Node's memory and did not). Holds
   are dated by the moment of the refusal, not of the request. **A latch that cannot be read counts as held**: an
   automatic sync is answered as throttled for 60 s (an explicit press, `X-Cowork-Storage-Retry: 1`, still runs),
   because the latch is what keeps a refused login from being fed to the storage server's brute-force protection;
   core's Diary worker keeps its own behaviour on an unreadable latch. A listing in which a subfolder was refused
   the login leaves the hold set (core cleared it). Both sides compute the connection fingerprint the same way (a
   fixed table of rows is checked in both repos). A 429's `Retry-After` is not surfaced by the child, so a throttle
   holds for the 60 s default.
5. The running file operations (#1441, #1815) are `users/<id>/source-file-ops.json`, written by the front and
   re-read by core (`rust-source-ops.cjs`): a chat turn waits for the project's running named operations, and the
   source-text collector skips a workspace while a sync or folder import runs. An operation is listed before the
   client is told it was accepted and is stamped when it begins and ends. Core's reader fails closed for the
   collector: an unreadable list (anything but a missing file) counts as an operation in flight, so nothing is
   swept; a chat's wait still waits for nothing it cannot read.
6. The order of a background start is core's again: the project (404), the storage hold of an automatic sync, then
   the body (413, 400), and only then the job slot (429); round 6's reordering is gone.
7. The folder-import tickets are in the front's memory with core's semantics (4 per person, 1024 overall, an hour); a
   front restart ends them as a Node restart did.
8. The storage endpoint policy (an admin any http(s) origin, a member only `MEMBER_OUTBOUND_ORIGINS`) now applies to
   a sync and an upload as it does to the browse; the sync's admission is per storage operation (four at once, one per
   account, 30 s wait), not per sync, so a long sync does not refuse the account's other storage writes; a streamed
   read takes its slot for each ranged request, not across the read. A saved storage login that cannot be read is a
   503, not "no storage".
9. **Bounds.** The operation runs in a thread that cannot be stopped, so it is bounded from outside: past 2 hours
   (a sync) or 30 minutes (an upload) its job ends as failed 504 and its listing as failed, whether or not the thread
   has returned (the removed worker's 5 minute accept bound is gone; the store's 6 hour stale bound is the backstop).
   A hung thread keeps the project's source lock; an upload, a sync or a `POST .../documents` that cannot take it
   within 30 s (a request) or within half its bound (a job, so the operation keeps the other half) is answered 429.
   An operation that waited for the lock dates what it writes by the clock after the wait. A synchronous upload or
   sync runs in a task of its own: a client that disconnects does not end the operation or mark its listing failed. S3 listings are cut at 10,000 entries or 20 pages as before, and the cut
   is now logged.
10. **Account removal.** A source operation never makes `users/<id>` again: the staging folder, the stored original,
   the result record and the image assets are made one level at a time below an existing user folder, and an account
   removed meanwhile ends the operation 410 `account no longer exists` (core's `assertActive`); a project's source
   texts (`project-sources/`) are made the same way, so a create or an upload that meets a removed account is 410
   too.

**Not ported, by decision.** The OCR budget applies to the Docling completion pass only (the pdf.js walk's budgeted
OCR is gone with pdf.js).

## sbstndalton/noevia#2190 and #2191 close-out: `GET /api/health`, `GET /api/auto-roles`, `GET /api/workspace`

| route | owner | capability / Core refusal | notes |
|---|---|---|---|
| `GET /api/health` | rust | `rust-health` / `RUST_HEALTH_OWNED` | health_routes.rs. Authenticated exactly as Node had it (a session or a device token; the gate's 401/403). Not `/api/ready`, which stays the unauthenticated process check |
| `GET /api/auto-roles` | rust | `rust-auto-roles` / `RUST_AUTO_ROLES_OWNED` | auto_roles_routes.rs. **The PUT stays Node's**: a save warms the roles in the engine (`ensureRolesLoaded`), which is chat-live; Node remains the writer of `users/<id>/auto-roles.json` |
| `GET /api/workspace` | rust | `rust-workspace` / `RUST_WORKSPACE_OWNED` | project_routes.rs `Route::Workspace`, crates/server-projects `workspace.rs`: the list, the hourly delete-old-chats sweep, the orphan chat-context cleanup and the text-missing scan |

Each is permanent with no switch. Core refuses exactly the GET before authentication; the web supervisor and
the release assembler pair each marker with its capability in both directions (old/old stays valid). The
node-route floor in `tests/front.rs` is 49 (44 after round 9, below).

**Snapshots (what only Node knows).** `GET /api/health` needs `ragAvailable` and the retrieval tri-state
(rag.cjs: the native index dependencies, the embedding probe and its 45 s cache, the administrator's embedding
switch); `GET /api/auto-roles` needs the served model catalogue, the roles other machines serve and their
models. Node publishes each as a file in the data directory (core `published-snapshot.cjs`): `rag-status.json`
every 10 s (`RAG_STATUS_PUBLISH_MS`) and `auto-roles-snapshot.json` every 15 s (`AUTO_ROLES_PUBLISH_MS`), and
at once after the embedding service refreshes or a model write drops the shared model reads, **but only while
someone is asking** (next paragraph). A file is
written atomically (temporary file in the directory, fsync, rename, directory fsync, mode 0600) and holds
`{ version: 1, kind, writtenAt, everyMs, ...state }`. The front (snapshot.rs) reads it on every request and
trusts it only when it is a regular file under 4 MiB of that version and kind with `writtenAt` within two
`everyMs` of its own clock (and not more than 5 s ahead of it); **a missing, damaged, wrong-version, stale or
future file is "unavailable", never old data**: health answers `ragAvailable: false, retrieval: "unavailable"`,
and auto-roles answers as core does for an engine it cannot read (the catalogue is unknown, so nothing is
reported missing and no machine models are listed). A publisher whose build fails writes nothing, so the
file ages out. `auto-roles-snapshot.json` carries `catalogue` (`null` or `[{ name, missingFile }]`),
`machineRoles` (the roles another machine serves, which are not checked against this machine's catalogue,
#1840) and `machineModels` (their models the local catalogue does not list, #1849/#1855, filtered by Node).

**Publishing is demand-driven.** The publishers probe the embedder (a real embeddings request, cached 45 s) and
the model engines' catalogue, which an idle installation must not have probed around the clock (an embedder on
another machine would never reach its idle unload). So on every signed-in `GET /api/health` and `GET
/api/auto-roles` (after the gate: a refused request asks nothing of Node) the front touches a **demand
marker**, `snapshot-demand` in the data directory: a tiny file replaced atomically (temporary file in the
directory, rename; mode 0600) and rewritten at most every 5 s, so a busy page costs a `stat`. Node publishes
only while the marker's mtime is under **60 s** old: it watches the data directory (`fs.watch`, not keeping the
process alive) with a 1 s `stat` poll behind it (`SNAPSHOT_DEMAND_POLL_MS`), starts publishing at once when the
marker appears and stops its timers when it ages out; `changed()` republishes only while it is live. When a GET
finds its snapshot unusable and the marker **was idle** (missing, or 60 s or older), the front waits up to **2 s**
for a fresh one, looking every 100 ms; the wait is a task of its own that stops when the request does, and a
request that arrives during an open wait joins it. If none arrives the answer is the unusable-snapshot one
(health `retrieval: "unavailable"`; auto-roles without `machineModels` and with nothing reported missing). A
snapshot that is unusable while the marker is live (Core is down or stuck) is answered at once. Stale still means
unavailable: an old value is never republished or shown. The marker, the two snapshots and their temporary files
are state of the running process, so the off-site backup leaves them out at the top of a backed-up folder
(offsite_engine.rs `walk`, `snapshot::is_published_state`).

**Health's probes.** The front probes the default provider itself (the env-derived row: `INFERENCE_BASE_URL`,
else `LEMONADE_BASE_URL`, else core's default address; the key as a bearer unless empty or `local`;
`GET <base without trailing slashes or /v1>/v1/models`) and, for an account with the Diary add-on, the
sidecar's `GET /api/health` through server-diary (the bearer and the person's tenant headers). Core's bounds:
5 s each, the reply body counted in the bound and capped at 8 MiB, a redirect is a failure, 2xx is up, and no
cache (core cached only the retrieval probe, which stays Node's). `inferenceUp` and `lemonadeUp` are one
answer. Each probe runs in a task of its own that the handler awaits, so a client that hangs up leaves
nothing behind but a probe that ends at its own deadline (a debug build counts them; tests/health.rs).

**Workspace.** The list is core's, answer for answer: the account's projects without the internal ones, a
source as metadata (`chars` for an inline text, the pointer for an external one, never the text), chat metas
sanitized, and the load view core's workspace gave every route: a null or primitive `files` entry dropped
(#1236), the retired `lemonade` provider id read as the default's, and the `modes` migration (#1576), none
written back by a read. It first runs the delete-old-chats sweep (hourly at most, through the native tombstoned
delete) and the cleanup of the attachments projects of chats deleted before #788: only a tombstoned chat that no
list holds, only the account's own, ten per list, through the project delete without its chats
(`lifecycle::remove`, `purgeChats: false`). `textMissing: true` (#1815/#1818) comes from the same scan core ran:
a list never touches the disk per source; the first list of an account flags nothing and starts a background
scan of the sources it showed (at most every 3 minutes, a thread of its own); a flagged source is looked at
again by every list, so a text written back clears at once. A source's text is opened without following a
symlink, so a symlink counts as gone. Nothing in the list is chat-live, so no part of it stays on Node.

**Behaviour changes and Node bugs fixed.**
1. Node answered `/api/workspace` and `/api/health` for **every method**; the workspace's sweep and cleanup
   delete data, so a `POST`/`DELETE /api/workspace` ran them. Only the GET is the front's now and Node no
   longer has either route (the other methods are its 404). No client used them.
2. A delete-old-chats sweep that fails (a damaged setting, a busy lock) no longer fails the list: it is logged
   and the list is answered, as core did (the crate's earlier function returned the failure).
3. Retrieval and the catalogue are published on a timer, but **only while a signed-in page has asked in the
   last 60 s** (demand-driven publishing, above), so an idle installation does not probe the embedder or the
   engines; they previously ran only when a page asked, and still do. The first request after an idle spell
   may wait up to 2 s for Node to publish; if it does not, it reads "unavailable".
4. `retrieval` and the auto-roles `missing` can be up to one publishing interval old (10 s and 15 s, plus the
   snapshot's two-interval bound before they read as unavailable). A model write or an embedding refresh
   through the routes republishes at once (while the marker is live); a load the engine does by itself shows
   at the next tick.
5. The text-missing flag is the front's scan, not Node's memory: a read failure in Node's chat no longer flags
   a source; the scan finds it within 3 minutes. The flag survives a front restart no better than before
   (it is in memory; the next list starts a scan).
6. **With Core down and the front up, `GET /api/health` answers 200** with `retrieval: "unavailable"` (and the
   provider and Diary probes the front makes itself). Node's route used to answer it, so the same condition
   was a 502 from the proxy. `GET /api/auto-roles` likewise answers from the roles file, **leaving out
   `machineModels`** (and reporting nothing missing) whenever the snapshot is unusable.
7. A first request for an account now creates its directory from `/api/auto-roles` and `/api/workspace`, as
   Node's workspace load did; `/api/health` does not (Node's health did not read the workspace).

## sbstndalton/noevia#2191 round 9 (capability `rust-project-config`) and #2190 (capability `rust-admin-users`)

Two groups move, each as one owner (Core's refusal and the front's capability of the same name; the web supervisor
and assemble-release.sh refuse a mixed pair, old/old stays valid). The node-route floor in `tests/front.rs` is 44.

| Route | Owner | Core marker / capability | Where |
|---|---|---|---|
| `POST /api/projects/{projectId}/config` | rust | `rust-project-config.cjs` / `RUST_PROJECT_CONFIG_OWNED` | crates/server-projects `config.rs`, bins/noevia-server `project_config_routes.rs` |
| `GET`, `PUT /api/projects/{projectId}/instruction-skills` | rust | same | `skill_routes.rs`, `instruction_skills.rs` |
| `GET /api/projects/{projectId}/instruction-skills/manifests` and `.../manifests/{skillId}/content` | rust | same | same (the content route was Node's beside the manifests but is not named by the web client: `client = false`) |
| `POST /api/projects/{project}/skills/install` | rust | same | `skill_routes.rs`, `plugin_directory_routes.rs` `fetch_skill` |
| `DELETE /api/admin/users/{id}` | rust | `rust-admin-users.cjs` / `RUST_ADMIN_USERS_OWNED` | bins/noevia-server `admin_user_routes.rs`, crates/server-account `delete.rs` |

**Kept on Node, and why.** `/api/projects/{projectId}/browser`, `/code`, `/research` and `GET /api/code/active` are
chat-bound (the tool loop, the harness and the research jobs run in the chat process), so they stay Node's; core's
`routes/projects.cjs` is gone with the last of its handlers.

**The configuration.** Core's order and answers, field for field (`config.rs` header): the id decoded (URIError 500),
the body (413 past 1 MiB, `invalid JSON`), `getProject` (its heal saved, 404 `no such project`), then the patch, the
first refusal winning. A patch that leaves every field as stored is a read (`{ok: true, unchanged: true}`, no write, no
timestamp). Otherwise: `updatedAt` moves, every project is reconciled, the new upload texts are written before the
projects.json that points at them, and the RAG index follows through the user's index outbox (an `index` job per new or
changed upload, an `unindex` job per removed one; core's executor re-reads the list, so a source listed again by then
keeps its rows) and the removed sources' documents and originals are pruned once the list is on disk. Core re-reads
projects.json at every request start, so a chat turn sees the change at once; the permitted-tools cache already keys on
the project's toolbox selection. Two facts come from outside the store: whether a model can answer a chat (the engine's
`/models` listing for the one model the patch names, `model_kind.rs`: an embedding or reranking flag, the `embed|rerank`
name rule, `models.roles.json` and the Laya name rule; an unreadable listing judges by name, as core does with an
unreadable catalogue) and whether a provider is registered (the shared rows and the account's own). Core publishes the
icon ids, the selectable toolboxes and, since this round, every toolbox it knows (`known`, the connector boxes
included) in `project-catalogue.json`; a patch that needs the catalogue (an icon, toolboxes) and the manifests answer
503 while it is unpublished.

**Instruction skills.** `instruction_skills.rs` is core's `inspect`, `list`, `listForClient`, `manifests`, `resolve`,
`setSelection` and the metadata-index limit (32 entries, 6,144 characters); chat keeps core's `instruction-skills.cjs`
(`inspect`, `manifests`, `resolvePinned`, `reconcile`, `snapshot`), which reads the same project data. Core's
`setSelection`, `listForClient` and `resolve` are deleted. Texts are read from their own files (a missing file reads as
empty, as core's accessor does), hashed and measured as core does (a lone surrogate counts as U+FFFD).

**Skill install.** One published SKILL.md (never scripts or assets): the name must match `[a-z0-9][a-z0-9-]{0,79}`
(checked before anything is fetched), the host is the fixed `raw.githubusercontent.com` over https with no redirect
(`redirect: 'error'`), an 8 s deadline for the whole exchange and at most 32 KiB; the host's addresses are looked up
and must all be public (ssrf-policy) and the request is pinned to them, so a poisoned answer cannot lead the install
into the private network. The `NOEVIA_QA_SKILLS_RAW` base (unset in production) points synthetic runs at a loopback
host and skips that lookup. The project is checked first (404), then the body, the name, the fetch, and one save under
the store's lock; a source limit is a 409 with its code. The request runs as spawned work: a client that goes away
does not strand the fetch or the save.

**Account deletion.** In order: the gate and the administrator check; the id and the body (core's messages, 400);
one transaction (`delete.rs`) that checks the typed username (exact), refuses the last active administrator
(`cannot delete the last active administrator`, checked inside the transaction, where core counted before it opened
one), revokes the invitations and recovery links the account created, deletes the `users` row (its sessions,
passkeys, device grants, app passwords, features, appearance, storage connection, tool policies, sharing opt-in and
Diary connectors go by the foreign keys, through the one-statement grant `RustAccountDeletion` in server-store) and
writes the `user.delete` audit row; the acting administrator is re-checked inside the same transaction. Then, as work
the handler awaits: the account's folder (`users/<id>`, retried; a link in its place is removed, never followed), the
Google Drive connection (revoked at Google, then forgotten), the MCP sign-ins and keys, the ChatGPT tokens and the
Diary tenant (`DELETE /api/internal/tenant`, bearer + tenant id + assertion only, 15 s), each broker asked up to three
times; the starts' orphan sweeps remove what still fails. The answer is `{ok: true}`; a folder that could not be removed
is the one failure reported (500, fixed text). Core has no way to delete an account any more: `deleteUser`, the route's
cleanup and the `forgetUser`/`removeUser`/`*Forget` broker clients are deleted, and `users` is refused to every Node
statement, inside `exempt()` too. What Node held for the account follows the folder: a workspace held for a folder that
is gone is revoked (a turn in flight stops writing and nothing makes the folder again), and none is made for a user
the database no longer has. The Drive and ChatGPT session caches are keyed to the brokers' state files, which the
brokers rewrite when they forget the account. Core has no role-change route, so there is no role cache to invalidate.

**Behaviour changes.**
1. `POST .../config` with `Auto` routing no longer warms the role models in the engine (`ensureRolesLoaded`); with the
   native router, the only one deployed, that call returned at once. The models load on the first turn.
2. A body of JSON `null` on `PUT .../instruction-skills` is `File and enabled boolean required.` (400); core answered the
   TypeError's own text (400).
3. A skill install's network failure (a refused connection, the deadline) answers the fixed 502 text; core answered the
   low-level error's message.
4. The last-administrator rule is checked in the deletion's transaction (core counted before opening one).
5. An account deletion that cannot remove the folder is a 500 after the rows are gone; core answered 400 with the
   file system's message (a path), and its row was gone too.
6. The deleted account's Drive, MCP and ChatGPT credentials are forgotten by the front itself (in process), no longer
   by Node asking the internal store over HTTP; the internal `forget` operations stay in the store for old/old pairs.

