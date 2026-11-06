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

## The shared conditional-operation protocol (docs: handoff `port-d2-r3-20261105/loader-cas.md`)

Every effect is one operation of the shared shape, on the one exclusion the worker already holds
(`Maint::acquire`, J's grant), with no second lease or permit model:

1. **register**: `ini.snapshot_covered(hold.write_fence())` gives R; `hold.begin_operation("register", Some(R))`
   issues the permit; `POST /api/v1/sections/{stem}/safe-defaults` carries it as `X-Noevia-Maintenance-Fence`. The
   loader validates identity, operation and R inside its publication transaction and records the terminal state and
   the new revision there. The worker reads that ORIGINAL receipt (`wait_for_settlement`) and takes the revision
   from it, never from the answer alone (an answer that disagrees with the receipt is unproven).
2. **reload**: a covered read must still be the registration's revision; `reload_expected` takes its own covered
   read, issues a `reload` permit with that revision and sends the fenced Gate request.
3. **Refused file** (400/404/409): the loader published nothing, the permit is settled as bookkeeping and the stem
   is remembered (400/404/409) or retried (stale permit, no verdict on the file).
4. **No answer** (5xx, timeout, cut body): the permit is NOT settled and is never re-issued. The run keeps its hold
   while it waits for the original receipt. A `settled` receipt with a revision is the effect; anything else leaves the
   unsettled permit to its `Drop`, which revokes coverage and keeps J's recovery incident; the batch stops, nothing
   is reloaded and the stem is not remembered. A call that provably never left (`Sent::NotSent`) is settled as
   bookkeeping.

A late loader commit after the hold is gone is refused by the authority (`recovery` receipts do not validate), not
by a worker check. Local acquisition, the detached run and detached acquisition are J's `Reservation` and the run
owner (cancellation never frees a hold before its effect settles).

## Open (see `handoff/port-w-r3-20261105/protocol-gaps.md`)

The manager still resolves the stem to a file itself, so the validated file identity is not carried into the
publication transaction; refusals leave the permit `issued` until the worker settles it; the stale-permit refusal
is recognised by message text. Descriptor-relative probes compare identity across two looks only.

## Interfaces to switch when the neighbours merge

- **Load path (row A, `move/model-load-ac`)**: `Env::reload(&hold, revision)` (today `reload_expected`). Replace
  with A's reload under a hold.
- **Auto-tune (rows K, K5)**: `Env::busy` (today `Maint::local_held`). Replace with the native auto-tune state;
  Node's calibration and auto-tune are already visible through the exclusion being refused.
