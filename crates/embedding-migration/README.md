# embedding-migration

noevia-core's embedding model switch (`embedding-config.cjs`, `embedding-profile.cjs`,
`embedding-migration.cjs` in its native-worker mode, `embedding-served.cjs`'s verdict and the
orchestration in `routes/embedding.cjs`) as pure Rust over a `Host` trait the front implements.
Request reading is `embedding-admin`'s. Nothing calls this crate yet.

## Recording

The fixture `tests/fixtures/node-embedding-migration.v1.json` is written by `tests/record.cjs`
from the pinned core modules (sha256 checked by the script):

```
NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
  node crates/embedding-migration/tests/record.cjs \
  > crates/embedding-migration/tests/fixtures/node-embedding-migration.v1.json
```

On MS02 run it on the builder:
`~/bin/n5run --with <core checkout under wt/> --pull -- 'cd <this worktree> && NOEVIA_CORE_CHECKOUT=<core checkout> node crates/embedding-migration/tests/record.cjs > crates/embedding-migration/tests/fixtures/node-embedding-migration.v1.json'`.
`tests/node_replay.rs` replays every case: answers, host calls in order and the saved file.

## Where Node throws, and where the port differs

- A TypeError Node's route throws ends the request where it is thrown, and index.cjs's handler
  answers `errorResponse()`'s 500 `{ "error": "Internal error" }`. The port answers the same
  (`routes::internal_error()`) after the same host calls for the throws that are recorded:
  - a request value `String()` or a template literal cannot convert (an object with its own
    `toString`, also inside an array): the action and scope (read by `embedding-admin`), a
    `set-next` target's `baseUrl` / `diaryBaseUrl` (before that address is checked) and model
    (after the address checks, before any served-model probe), a `service-switch` model (before
    `resolveFile`) and profile (from off: after `serve` and `waitLoaded`, with no change, Diary call
    or audit record);
  - a Diary target address that cannot be converted, read by `service.isAt` (production's
    `sameAddress`, once the service has an address) or by `servedSet()`'s Diary-default check: a
    `service-switch` serves nothing, and the sync after a change serves nothing (the change and its
    audit record stay);
  - a truthy non-array `models` in the embedding service's status during `service-switch`.

  The `Host` methods cannot throw: `service_is_at`'s throw is answered by the port from
  `service_base_url`, and the recorder's stubs convert values as production's do
  (`sameAddress` for `isAt`, `===` for `fileOf`). A throw inside a real host's own I/O is not
  covered. A value that converts (an own `valueOf` alone gives `[object Object]`) goes on as in
  Node. The recorder answers a thrown error with http.cjs's own `errorResponse`.
- The endpoint parity check fails closed: unless both endpoints give one vector per sample text,
  parity is `ok: false`. Node's passes vacuously on fewer vectors from the active endpoint
  (`sims.every(Number.isFinite)` of an empty list is true, `Math.min()` is Infinity).
  Not recorded; `tests/node_replay.rs` pins it.
- A `Host` panic inside the config lock's hold ends the hold and calls `Host::unlock` once before
  the panic goes on.
- A locked configuration (a seeded active address refused at start-up) refuses `set-next` with
  its own 409 `badState` and the refusal reason, as `clear-next` does. Core's `setNext` takes its
  refusal shortcut outside the hold, where `change()` checks the publication lock first, so it
  answers 500 with the text "Embedding configuration requires the publication lock"
  (sbstndalton/noevia#2696). The host calls and the file are the same; the replay checks the
  three recorded steps for the 409 instead.
