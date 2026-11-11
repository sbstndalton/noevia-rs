# chat-mirror-sched ownership (`rust-chat-mirror-v1`)

Moved from noevia-core (`chat-change-journal.cjs`, the scheduling halves of `chat-vault-mirror.cjs` and
`chat-brain.cjs`, `chat-activity.cjs`, the nudge poller in `rust-vault-mirror-preferences.cjs`):

| piece | where it lives now |
|---|---|
| journal tail: cursor, rescan rules, revision dedupe, move-event identity, 2 s tick, 15 min active window, journal-file stamp skip, activity merge, failure isolation, forget on a removed account | `journal.rs` (`Consumer`) |
| Diary mirror schedule: 5 s debounce, 30 s max wait, per-account dirty set, one pass at a time, again flag, 60 s x 2^n backoff capped at 1 h | `mirror.rs` (`Mirror`) |
| brain schedule: 3 min per-chat idle timer, one build at a time across accounts, one retry after 15 min for `deadline`, `error`, `budget`, `invalid-json`, `schema`, mirror nudge after `built` | `brain.rs` (`Brains`) |
| preference-save nudge | in process: `server_account::vault_mirror::set_nudge_hook`, installed by the server |
| wiring: journal source, activity file, executors, Node's triggers | `bins/noevia-server/src/chat_mirror.rs`, `project_store.rs` (`/v1/chat-mirror/{schedule,touch}`) |

## REMAINING (still Node)

The work itself, behind `MirrorExecutor` and `BrainExecutor`, reached by `POST /internal/chat-mirror/sync`
and `/brain` on loopback with the per-start secret (core `rust-chat-mirror.cjs`):

- one mirror pass: Diary file read/write/move/trash through the Diary files client, the note rendering,
  the per-user index, the tombstone-driven trash pass (`chat-vault-mirror.cjs` `sync`);
- one brain build: the reasoner call, its admission and fallbacks, the per-chat brain store
  (`chat-brain.cjs` `createBrainBuilder`, `createBrainRunner`).

An unreachable or erroring Node is a failed pass (retried on the backoff, nothing written), never a success.

## Pairing

Both processes carry the capability. A front without it beside this Core leaves the journal unread; a Core
without it beside this front runs a second schedule. `build/web-supervisor.sh` and `deploy/tools/assemble-release.sh`
refuse either mix.
