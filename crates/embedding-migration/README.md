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

- A TypeError Node's route throws (a truthy non-array `models` in the embedding service's status
  during `service-switch`; a Diary target address `String()` cannot convert, read by `servedSet()`)
  ends the request where it is thrown, and index.cjs's handler answers `errorResponse()`'s
  500 `{ "error": "Internal error" }`. The port answers the same at the same point
  (`routes::internal_error()`): a `service-switch` serves nothing, and the sync after a change
  serves nothing (the change and its audit record stay). The recorder answers a thrown error with
  http.cjs's own `errorResponse`.
- The endpoint parity check fails closed: unless both endpoints give one vector per sample text,
  parity is `ok: false`. Node's passes vacuously on fewer vectors from the active endpoint
  (`sims.every(Number.isFinite)` of an empty list is true, `Math.min()` is Infinity).
  Not recorded; `tests/node_replay.rs` pins it.
- A `Host` panic inside the config lock's hold ends the hold and calls `Host::unlock` once before
  the panic goes on.
