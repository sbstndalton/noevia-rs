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
/api/projects/{id}/assets/{assetId}` (crates/server-projects `assets`).

`projects.json` cannot have one writer yet (Node's chat routes keep chat metas in it), so while
the switch is on both processes write it only under one lock (flock(2) LOCK_EX on a long-lived
`projects.json.lock`, crates/server-projects `lock` = core `server/rust-projects.cjs`): Rust re-reads it for every change
and writes Node's exact `atomicJson` bytes; Node re-reads it when it changed and saves by a
per-project, per-field three-way merge, so neither drops the other's change. With
`NOEVIA_FRONT=rust`, `NOEVIA_RUST_AUTH_CONFIRMED=1` and `NOEVIA_RUST_PROJECTS_CONFIRMED=1` (the
supervisor sets the last once `--features` lists `rust-projects`), Node's copies of the image
writes answer 503 `RUST_PROJECTS_OWNED`.

CI job `corpus-replay-front-rust-projects` replays the corpus through the front with both switches
(zero differences, Node-only data dir), runs the Node-vs-Rust lock race (`server-projects --test
node_lock`, with a blind-save control that must lose data), and a negative control where Node is
told the front owns the image routes but the front does not: the uploads must be refused with 503.
