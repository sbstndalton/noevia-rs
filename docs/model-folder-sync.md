# Model folder sync (Core `model-folder-sync.cjs`, moved)

A GGUF that appears in the models folder becomes a usable model without anyone opening a page.
`bins/noevia-server/src/model_folder_sync` runs it; Core no longer does (`rust-model-folder-sync.cjs` is the
ownership marker, paired by the web supervisor and release assembly with the front's
`rust-model-folder-sync-v1` capability).

## Behaviour kept from Node (fixture: `tests/fixtures/folder-sync-node.json`, recorded by `tests/folder-sync-oracle.cjs`)

- First scan 20 s after start, then every 15 minutes; one run at a time.
- Skipped while calibration or auto-tune of this process runs (and, through the exclusion, while any other
  maintenance job anywhere holds the engine).
- The scan is the model management service's `GET /api/v1/models` (`unregistered`); each stem is registered
  with `POST /api/v1/sections/{stem}/safe-defaults` (the manager writes `models.ini` under its own lock).
- 400 (draft head), 409 (name or file taken) and 404 (gone) are remembered in
  `UI_DATA_DIR/model-folder-sync.json` (`{"skip":[...]}`, mode 0600, the last 500) and never retried; anything
  else is retried next tick. Busy / exclusion refused is transient and never remembered.
- One guarded reload (nothing is unloaded) when anything was registered. Same log lines.

## Stricter than Node (on purpose)

| Node | Here |
| --- | --- |
| `busy()` re-checked before every file (no hold) or a held writer | one exclusion for the batch, checked before every registration and before the reload; a lost or unconfirmable exclusion ends the batch, nothing more is written or reloaded, and it is never taken again within the run |
| scan once, outside the hold | scan again under the exclusion |
| reload whatever `models.ini` now holds | reload only when `models.ini`'s revision is the one the last registration answered with |
| registers whatever the manager lists | registers only a settled file: regular `.gguf` inside the models root (no link followed), no download temp beside it, older than 120 s, a GGUF header, unchanged across a 2 s probe |
| a gate error other than "refused" was a scan failure | any refusal defers |

Caller cancellation detaches the owned run; it keeps its exclusion and run slot until completion.
Graceful stop drains that run before ending the schedule. Acquisition also has an owning task, so
late gate/Node grants are released even if their waiter disappears. Local acquisition uses an RAII reservation.

## Remaining authority requirements

This branch still requires the shared maintenance barrier before release. An HTTP timeout is not
proof a manager write settled, and neither the manager write nor engine reload currently accepts
an authority-enforced epoch. The expected reload text is now retained across router awaits, but
checking before sending cannot serialize engine publication with another process's INI write.
Descriptor-relative file probes refuse link swaps and compare inode identity across probes; the
manager still independently resolves the stem on registration and must validate that same identity.
These require manager/engine protocol integration, rather than another worker-side check.

## Interfaces to switch when the neighbours merge

- **Exclusion (row J, maintenance lease authority)**: `Env::take` / `Hold` (`live.rs`, today
  `Maint::acquire("model folder sync", ..)`: the inference gate's hold, else a lease on Node's maintenance gate).
  Replace with J's client; keep `Hold::excluded` (lost or unconfirmed => false) and `Hold::may_start`.
- **Load path (row A, `move/model-load-ac`)**: `Env::reload(&hold)` (today `Maint::reload_inner`, the guarded
  reload under an existing hold). Replace with A's reload-under-hold.
- **Auto-tune (rows K, K5)**: `Env::busy` (today `Maint::local_held`). Replace with the native auto-tune state;
  Node's calibration and auto-tune are already visible through the exclusion being refused.
