# Private calibrator trace format (v1)

The oracle for the auto-tune's private calibrator (core `llamacpp-calibration.cjs` `createCalibrator`
as the manager's `contextFactory` builds it: no state file, a no-op hold, the runner's hooks spread
last). Two Node test files are run with `calibrator-preload.cjs`: core's own
`server/llamacpp-full-autotune.test.cjs` (traces `NNN-*.json`) and `scenarios.test.cjs` beside this
file (traces `SNNN-*.json`). Every call such a calibrator (or the contextFactory's `start`/`current()`
around it) makes on its dependencies, and every call the runner makes on the context object, is
recorded in order.

`scenarios.test.cjs` builds core's native model manager (`server/model-manager.cjs`) over a synthetic
engine (QA-named models, a temp `models.ini`), takes the manager's own `contextFactory` (through the
preload's recording factory) and drives it as the runner does, for the branches core's tests barely
reach: probe evidence and causes, `cancelProbe`, foreign clients (`foreignWait`/`foreignHeld`),
aborts during start and during a run, stale starts, refused writes and restores, the memory floor
(per-call `readMemory`), engine failures, and (R6 shift 3) a rejected long prompt's error body (cut
by its 2 s deadline, cut at the byte cap, ended normally) and the long prompt's 1.5x wall-clock stop
(paced chunks past it, exactly at it with and without engine timings). Whatever Node does is the oracle; each scenario only
asserts that it reached the branch it is about. Every scenario has a 15 s node:test timeout (a
scenario that never settles fails alone). That timeout cannot stop a loop that never yields to the
event loop: without the recorder's clock the calibrator's time is real and the scenarios' sleeps
return at once, so a scenario must keep its deadlines short or end its waits itself.

Re-record (from a noevia-core checkout at the commit in `SOURCES.sha256`, `server/wasm/dav-parse.wasm` present):

    # on the N5 builder (owner decision 19), from this checkout; <core> = the core checkout, npm ci done in <core>/server
    timeout 1700 ~/bin/n5run --with <core> --timeout 1600 -- \
      'node bins/noevia-server/tests/fixtures/private-calibrator/record-node.cjs <core> bins/noevia-server/tests/fixtures/private-calibrator/traces'
    timeout 1700 ~/bin/n5run --with <core> --timeout 1600 -- \
      'node bins/noevia-server/tests/fixtures/private-calibrator/record-node.cjs <core> --baseline'

Both print the node:test counts per file (`core`, `scenarios`); they must match. Recording twice
gives byte-identical directories. Public calibrators (with a state file) are not recorded.
`SOURCES.sha256` holds the core files at the recorded commit (paths relative to the core checkout);
each trace's `sourceSha256.test` is the hash of the test file it came from (`scenarios.test.cjs`
for `S*`).

## Top level
```json
{ "version": 1, "test": "<node:test name>", "instance": 0, "kind": "context" | "fill",
  "callbacks": ["contextCurrent", "applyUnlocked", ...],
  "sourceSha256": { "calibration", "manager", "runner", "test" },
  "replayable": true, "why": [],
  "config": { "memoryFloorGib": 2, "readMemory": 20, "timeouts": {} },
  "strings": [ ... ], "events": [ ... ] }
```
- File `NNN-<slug>-<k>.json`: NNN the test's ordinal in the file, k the calibrator's index within the test.
- `kind`: `fill` when the hooks have no `contextCurrent` (fillCheck: `applyUnlocked`, `onWrite`, a no-op
  `onUpdate`; no `foreignWait`/`foreignHeld`, so another client's model is fatal there), else `context`.
- `config.readMemory`: the value `readMemory()` returned (constant; `null` = it returned null; the
  key is ABSENT when it was never called). It is not recorded per call (the calibrator also samples it
  on a real `setInterval`). A trace where it varied has `replayable: false`, `why: ["readMemory varies"]`.
  Exception: `"calls"` when the calibrator was given `timeouts.memoryPoll >= 1e8` (its memory
  `setInterval` never fires, so every read comes from its own code): each read is then a sync
  `readMemory` event, in order, and may vary.
- `config.timeouts`: the `timeouts` the calibrator was given (merged over Node's defaults:
  load 600000, smoke 180000, long 7200000, unload 60000, poll 500, memoryPoll 1000, recover 180000).
- Optional `afterEnd: true` (something was dropped after the test ended), `openAtEnd: n` (n calls had
  not settled then; they carry `"open": true` and no result). None occur at present.

## Determinism (by the preload)
- The calibrator's `now()` is a fake clock: `1700000000000` plus `max(1, ms)` for every calibrator
  `sleep(ms)` once it resolves, plus every `clock` event's `ms`. Nothing else advances it (a calibrator
  given its own `now` would be `replayable: false`; none is: a scenario that paces a stream hands the
  calibrator its own `now` only in the baseline run, where nothing is recorded).
- The error-body deadline is a real 2 s timer: a scenario body that stalls makes the scenario wait
  for it. Its other timers (the memory watch's interval) are not recorded.
- `crypto.randomInt(min, max)` from llamacpp-calibration.cjs (the recall marker) returns
  `min + ((n * 7919) % (max - min))`, n counting from 1 per trace; `crypto.randomUUID()` (the job id)
  returns `00000000-0000-4000-a000-` + 12-digit counter per trace. Both are recorded as events.
- Any string field named `token`, `generation`, `rowToken`, `preparedToken` is `"<token>"`.
- The OS temp prefix is written `<tmp>/`.

## Size encodings (lossless)
- Every string of 200+ characters in event values is in `strings` and written `{ "$s": i }`.
- A `strings` entry containing two or more consecutive copies of the calibrator's `PAD`
  (`"This is synthetic padding for a context allocation validation.\n"`) is written
  `{ "prefix": p, "pad": n, "suffix": s }` = `p + PAD.repeat(n) + s` (the first such run only).

## Events
Each is `{ "dep": ..., <args>, "result" | "throws", "during"? }`. `throws` is
`{ "message", "flags": { cancelled?, fatal?, coverageLost?, foreignTimeout?, backend?, name? } }`
(only truthy flags; `name` only when not `Error`). Values are JSON clones taken when recorded
(undefined dropped).

| dep | fields | |
|---|---|---|
| `rawModels` | `result` | signal not recorded |
| `request` | `path`, `method` (default `GET`), `body` (parsed JSON or null), `timeoutMs` (or null), `signal` (bool), `result`/`throws` | |
| `stream` | `path`, `method`, `body` (parsed), `result: { ok, status, body: bool }` / `throws` | |
| `stream.read` | `stream` (0-based index of the `stream` event in this trace), `result: { "text": utf8 }` or `{ "done": true }` / `throws` | one per body read (async iterator `next` or `getReader().read()`); cancel/return not recorded |
| `presets.get` | `model`, `result` | sync |
| `presets.snapshot` | `result: { text, revision }` | sync |
| `presets.commit` | `candidate`, `result`/`throws` | the calibrator's restore |
| `invalidateBaseline` | `tag`, `result` | |
| `conservativeFor` / `identityFor` | `model`, `result`/`throws` | |
| `held` | `result` | the manager's `maintenance.held()` inside the contextFactory's `current()`; sync |
| `readMemory` | `result` (GiB or null) | only when `config.readMemory` is `"calls"`; sync |
| `sleep` | `ms` | |
| `tick` | `ms` | the error-body deadline (`errorBody`, ERROR_BODY_MS) fired; sync. Recorded only when it fires, so it sits in the `during` of the `stream.read` it cut, and that read's result is what the cancel gave (`{ "done": true }`). A replay must not answer that read: its own deadline (set with the same `ms`) has to end it |
| `clock` | `ms` | scenario traces only: `ms` of wall time passed while the engine produced a stream chunk (the scenario's paced body); sync, in the `during` of that read. The fake clock moves on by `ms` before the read answers |
| `randomInt` | `min`, `max`, `result` | sync |
| `randomUUID` | `result` | sync |
| `hook` | `name`, `arg` (not for `contextCurrent`), `result`/`throws` | `contextCurrent`, `onWrite` (arg revision), `onUpdate` (arg: the job or null; result null) are sync; `applyUnlocked` (arg body), `foreignHeld` (arg rows), `foreignWait` (arg: the error as `{ message, flags }`) are async |
| `driver` | `method`, `args`, `result`/`throws` | the runner calling the context object: `start` ([model, opts]), `completion`, `status` ([model]), `cancellationFacts`, `compareAndAbort` (["<token>"] when it equalled the current cancellation token at the call, else ["<stale-token>"]), `cancel`, `probe` ([model, opts]), `cancelProbe`; every function on the object is recorded the same way. Placed at the call; `result` attached when it settles |

## Nesting and order
Per trace there is a stack of in-flight async calibrator calls (`rawModels`, `request`, `stream`,
`stream.read`, `presets.commit`, `invalidateBaseline`, `conservativeFor`, `identityFor`, `sleep`, and
the async hooks). Any event recorded while one is in flight goes into the `during` of the most recently
started call still in flight, else at top level. Sync events and `driver` events are never in flight,
so what a driver call does follows it at the same level. A replay runs a call's `during` (above all the
runner's driver calls, such as a cancel) before that call returns its result.

Order quirks a replay must reproduce:
- The contextFactory reads `presets.snapshot()` once when it is created (the revision `current()`
  compares against), before any driver event.
- `current()` is `contextCurrent() === true && held() && presets.snapshot().revision === revision`,
  short-circuited: a false `contextCurrent` is followed by neither `held` nor `presets.snapshot`.
- A launch runs synchronously into the run: the job's `randomUUID`, `onUpdate(job)`, then the run's
  first `rawModels` all come before the runner's next driver call (`completion`).
- `start` can settle (abort: `{ ok:false, body:{ error:'The context worker state changed or is
  unavailable.' } }`) while its own reads go on: the trace keeps those later calls.
- `driver probe`'s saves call `onUpdate(null)` (the probe's job is never the calibrator's job).
