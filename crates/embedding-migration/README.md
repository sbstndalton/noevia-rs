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
