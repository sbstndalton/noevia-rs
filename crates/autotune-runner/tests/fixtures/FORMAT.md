# Auto-tune runner trace format (v1)

The oracle for the native auto-tune runner (noevia-rs `crates/autotune-runner`). Node's runner
(noevia-core `server/llamacpp-full-autotune.cjs`, `createFullAutotuner`) is run inside the existing
test fixtures with a fake router; every call it makes on its injected dependencies, every call a test
makes on the actor it returns, and every state-file save is recorded in order. The Rust runner, given
a replay of the same dependencies, must make the same calls in the same order and write the same
state-file bytes at every save.

## Files
- Recorder: `noevia-rs/crates/autotune-runner/tests/fixtures/record-node.cjs <core> <out>`, which runs
  `node --require <fixtures>/trace-preload.cjs --test-reporter=tap <core>/server/llamacpp-full-autotune.test.cjs`
  in-process (TZ=UTC, `NOEVIA_AUTOTUNE_ALLOW_BELOW_Q5_KV` unset; `--baseline` runs the same without the preload). It writes one JSON file per recorded autotuner instance into
  `crates/autotune-runner/tests/fixtures/traces/`, named `<nnn>-<slug of test name>-<instance>.json`.
- Every trace file records `sourceSha256` of `server/llamacpp-full-autotune.cjs` and of the test file.

## Determinism (enforced by the preload, not by the tests)
- `crypto.randomUUID` called by the runner file → `"00000000-0000-4000-8000-" + 12-digit counter`
  (counter per trace, starting at 1). Other callers (calibrator job ids, temp names) get
  `"00000000-0000-4000-9000-" + counter` (reset at each test start) so their ids are stable too.
- The autotuner's `now` → a fake clock: `T0 = 1700000000000` plus, for every `sleep(ms)` the
  autotuner awaited, `max(1, ms)` (a 0 ms poll still advances it by 1, else a 0 ms foreign poll
  never reaches its deadline). Nothing else advances it. `Date.now` used elsewhere is not patched.
- The autotuner's `sleep(ms)` → recorded; resolves after one `setImmediate` turn (an injected
  sleep is awaited instead); advances the fake clock when it resolves.
- `readMemory`, `budgetGib`, `fileMissing`, `presets.get`, `presets.snapshot`, `presets.files` are
  **derived** reads: not recorded per call. `readMemory()`/`budgetGib()` must be constant per trace:
  the first value is in `config.readMemory` / `config.budgetGib` (absent when never read;
  `{ "throws": ... }` when it threw); a trace where one varies is kept with `replayable: false`.
  `config.fileMissing` maps each row id the runner asked about to the answer (constant per id).
  `presets.*` reads follow models.ini: `initial.ini`, then each `iniAfter`, then `ini` events.
- Runner timers (`setTimeout` in the runner file, i.e. `scheduleReload`, 1 s or less) fire after
  that many `setImmediate` turns instead of real ms (longer ones stay real). When one fires the
  trace records `{ "dep": "timer" }` and is `replayable: false`.
- The test file's own timers that race a run: while a run of the test's latest trace is in
  progress, a test `setInterval`, and a test `setTimeout` created by code running for the trace (a
  fake-engine handler inside a recorded call), run on that trace's fake clock (fired when an
  awaited runner sleep reaches them). Elsewhere they are real. One the run never sleeps up to fires
  on real time after its delay + 1 s and makes the trace `replayable: false`.
- A trace records nothing after its test ended (`afterEnd: true` says something was dropped;
  `openAtEnd: n` says n recorded calls were still in flight then).
- Recording twice gives byte-identical trace directories (checked with `diff -r`).

## Top level
```json
{
  "version": 1,
  "test": "<node:test name>", "instance": 0,
  "sourceSha256": { "runner": "<hex>", "test": "<hex>" },
  "replayable": true,
  "config": { "betweenModelsMs": 25, "idleTimeoutMs": 300000, "memoryFloorGib": 2,
    "foreignWaitMs": 900000, "foreignQuietMs": 30000, "foreignPollMs": 2000, "servicesReserveMib": 2560,
    "readMemory": 20, "budgetGib": 16, "allowBelowQ5Kv": false, "samplingFor": true, "planner": "wasm",
    "routerReload": false,
    "contention": "wasm", "servingChecks": "wasm", "loadAdvisor": "default", "longProfiles": "default",
    "contextFactory": true, "sleep": "default", "maintenance": ["<function names>"], "fileMissing": { "<id>": false } },
  "initial": { "state": "<state file text or null>", "ini": "<models.ini text or null>" },
  "strings": [ "<long string>" ],
  "events": [ ... ]
}
```
`planner`/`contention`/`servingChecks` are `"wasm"` (the runner's default, dav-parse.wasm) or
`"injected"`; `loadAdvisor`/`longProfiles` `"default"` or `"injected"` (the manager's own
`createLongProfiles()` counts as default); `sleep` `"default"` or `"injected"`; `maintenance` lists the
gate's function names (null without one). When `replayable` is false, `why` lists the reasons.
Optional: `afterEnd`, `openAtEnd`, `strings` (below).

### Size encodings (all lossless)
- `save` after the first in a trace has `delta` instead of `state` when that is shorter: a list of
  `[keep, delete, insert]` applied to the previous save's text (UTF-8 bytes; cut points are on
  character boundaries): copy `keep` bytes, skip `delete` bytes, write `insert`; after the last edit
  copy the rest. "Previous" is in event order (preorder: an event, then its `during`). `[]` = same text.
- `rawModels` / `request` `result.body` equal to that of the previous such event with a body for the same `path`
  (`rawModels` counts as one path), in event order, is written `{ "same": true }`.
- Every string of 2048+ characters that occurs more than once in a trace's event values (not save
  texts) is written once in top-level `strings` and as `{ "$string": i }` wherever it occurs
  (the batch-phase prompt mostly).
- Calibrator payloads (`context.*` results and callback args) have their own clock values
  (`startedAt`, `finishedAt`, `at`, `seconds`, `lastSave`, `etaMs`, `updatedAt`, `deadline`, numbers
  only) written `"<time>"`: the calibrator uses real `Date.now`, and the runner reads none of them.
- Any string field named `token`, `generation`, `rowToken` or `preparedToken` anywhere is `"<token>"`.
- The OS temp directory prefix in any text is written `<tmp>/` (none occur at present).

## Events (in order)
Every event is an object with `dep`. Calls that can fail have either `result` or `throws`
(`{ "message": "...", "flags": { "cancelled"?, "fatal"?, "engineFailure"?, "name"? } }`).
Values are JSON (`undefined` written as the string `"<undefined>"` only where it matters).

| dep | fields | notes |
|---|---|---|
| `rawModels` | `result` | the engine boundary's view: `{ ok, status?, body }` |
| `request` | `path`, `method` (default GET), `body` (parsed JSON or null), `timeoutMs`, `result` / `throws` | `signal` not recorded; `result.body` as the fake returned it (string for `/metrics`) |
| `applyUnlocked` | `body`, `writeOptions`, `result`, `iniAfter` | `iniAfter`: models.ini text when the call returned |
| `presets.commit` | `candidate`, `options`, `iniAfter` / `throws` | |
| `identityFor` | `model`, `result` / `throws` | |
| `planFacts` | `model`, `result` | |
| `samplingFor` | `model`, `result` | |
| `holdWhenIdle` | `reason`, `timeoutMs`, `result: "held"` / `throws` | returns a release handle |
| `release.setReason` | `reason` | |
| `release` | | the release handle called |
| `maintenance.<fn>` | `args`, `result` / `throws` | `assertCovered`, `coverageSignal` (result `{ aborted }` or null), `requireRecovery` (`key`, `message`), `recoverNow` |
| `maintenance.callback` / `maintenance.callbackDone` | `name: "recovery"`, `key`, `arg`; then `result` / `throws` | the gate ran the repair function given to `requireRecovery`; the runner's events in between |
| `invalidateBaseline` | `tag`, `result` | |
| `reloadRouter` | `result` / `throws` | the injected `reloadRouter`, only when given |
| `onResult` | `model`, `result` (the argument's `result`) | |
| `decide` | `fn`, `request`, `result` / `throws`, `injected` | a decision: `fn` is `plan` (planner.plan), `contention`, `templateCaps` (request: the template text), `servingVerdict` (request `{ status, bodyText }`), `judge` (loadAdvisor.judge, awaited), `longPairs` (longProfiles.pairsOf, request `{ rows }`), `longSection` (longProfiles.sectionFor, request `{ text, row }`). Recorded for injected and default implementations alike; `injected: false` means the production one ran, which Rust may compute natively and check. Replaces the `loadAdvisor.judge` row. |
| `context.create` | `id` (counter), `callbacks` (names given) | `contextFactory(...)` |
| `context.start` | `id`, `model`, `opts`, `result` | |
| `context.callback` | `id`, `name` (`onUpdate`/`onWrite`/`applyUnlocked`/`foreignHeld`/`foreignWait`/`contextCurrent`), `arg`, then a later `context.callbackDone` with `result`/`throws` | events the runner makes inside the callback appear between the two |
| `context.completion` | `id`, `result`/`throws` | recorded when called, like every other call; its `during` holds what happened while it was awaited (callbacks above all) |
| `context.status` | `id`, `model`, `result` | |
| `context.cancellationFacts` / `context.compareAndAbort` | `id`, `result` | |
| `context.probe` | `id`, `model`, `opts`, `result` | |
| `context.cancelProbe` | `id` | |
| `sleep` | `ms` | |
| `save` | `state` (or `delta`, see Size encodings) / `throws` | the exact text written to the state file (`throws`: the write or rename failed) |
| `ini` | `text` | models.ini changed outside a recorded write (a test edited it); placed where it was first seen |
| `driver` | `method`, `args`, `result` / `throws` | a call the test (or the manager) made on the actor: `start`, `compareAndAbort`, `prepareResume`, `compareAndResume`, `commitSettings`, `compareAndClear`, `recover`, `flushReload`, and `scanFacts` (a read, but it calls `rawModels`/`identityFor`). Other reads (`statusFacts`, `*Facts`, `completion`) are not recorded. `result` keeps tokens as `"<token>"` (they are process-salted). A token argument (`compareAndAbort`/`compareAndClear`/`prepareResume`/`compareAndResume` first, `commitSettings` third) that is a 64-hex hash is `"<token>"` when it equalled the actor's current one for that call and `"<stale-token>"` otherwise; anything else is kept. Function args are `"<function>"`, signals `"<signal>"`. |
| `driverAwait` | `method` | the run launched by that driver method (`start`, or `compareAndResume` with a launch plan) settled; recorded the moment it settled (right after the run's last save), not when the test noticed, so the replay waits for its run to finish here before going on |
| `timer` | `source?` | a runner timer fired (trace not replayable). `source: "maintenance"`: the gate ran a recovery on its own retry timer outside any recorded call (the first is recorded; at the second, `stop: true`, recording ends) |

Nesting (one rule for every event kind): each recorded call (`rawModels`, `request`, ...,
`context.*`, `decide`, `sleep`, `holdWhenIdle`, ...) is placed when it is called; every event recorded
while it is in flight goes into the `during` list of the most recently started call still in flight
(after its own arguments, before `result`). So a `driver` cancel made by the fake engine inside a
chat request, the calibrator's callbacks while `context.start`/`completion`/`probe` is awaited, and a
test timer's driver call during a `sleep` all sit in that call's `during`, and the replay runs them
before that call returns its result. Saves and `ini` events nest the same way. `driver` events are
not calls in this sense: what the runner does for a driver call follows it at the same level.

## Replay rule (Rust)
The replay engine pops events in order. A dependency call from the runner must equal the next
dependency event (dep and recorded arguments); a `save` must equal byte for byte; `driver` events
between dependency events are executed by the harness when the runner next calls a dependency (or
finishes). Mismatch = test failure with the event index and both sides.

## Recorder deviations from the first draft (R6 recorder, 2026-11-13)
- `decide` events (task brief) replace `loadAdvisor.judge`; `ini`, `maintenance.callback(Done)`,
  `save.throws`, `timer.source`, and `scanFacts` as a driver method are new: without them a replay
  cannot reproduce test edits of models.ini, gate-run recoveries, or scans that call dependencies.
- `context.completion` is recorded at the call, with a `during`, instead of when it settled: one
  nesting rule for every call is simpler to replay and loses nothing.
- Fake clock: `max(1, ms)` per sleep, and sleeps resolve after a `setImmediate` turn, not in the
  same microtask: test 115 (`foreignPollMs: 0`) never reached its deadline otherwise, and test
  polling loops need a turn to see the state between models.
- `driverAwait` is recorded when the run settles rather than when the test's `finished()` loop sees
  it: the latter raced the gate's real-time recovery retry in test 144.
- Traces whose readMemory/budgetGib vary are kept with `replayable: false` (task brief), not skipped.
- Test-file timers and runner timers are made deterministic (above); without that 5 traces
  differed between two recordings.
- Size encodings (save deltas, `same` bodies, `strings`): the raw set was 150 MB, 121 MB of it
  whole-state saves; now about 25 MB. No gzip.
