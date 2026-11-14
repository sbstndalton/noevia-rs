# Maintenance gate recordings (sbstndalton/noevia#2641)

The oracle for the front's lose-at-margin lease coverage (`model_maint::gate`, switch
`NOEVIA_RUST_LEASE_COVERAGE=node`) and its settle driver (`model_maint::recovery`): core's own
`server/inference-maintenance.cjs` `createMaintenanceGate` with a native lease grant, on a virtual
clock (its `now`/`setTimer`/`clearTimer` options and the global `setTimeout`, which
`scheduleRecovery` uses). `record-node.cjs` holds the scenarios and writes `gate.json` and
`SOURCES.sha256` (the core file it exercises, at the recorded commit). The Rust replay is the unit
test `model_maint::recovery::tests::replays_nodes_gate_recordings`.

Re-record on the N5 builder (owner decision 19) from this checkout; `<core>` is a noevia-core
checkout (only that one file and node builtins are loaded, no `npm ci`):

    timeout 900 ~/bin/n5run --with <core> --pull --timeout 600 -- 'set -e
      F=bins/noevia-server/tests/fixtures/maint-gate
      node $F/record-node.cjs <core> $F/gate.json
      node $F/record-node.cjs <core> /tmp/r6-gate-second.json
      cmp $F/gate.json /tmp/r6-gate-second.json'

Recording twice gives byte-identical files (no wall clock is read).

## Format (v1)

`{ version: 1, options: {coverageMarginMs, recoveryRetryMs, recoveryMaxDelayMs}, sourceSha256, scenarios }`;
each scenario `{ name, input, steps, events }`:

- `input.grant` `{ttlMs, renewMs, hardMs}` of every native grant; `input.renewals` the remote's renewal
  answers in call order (`true`, `false`, `null`, or `"defer"`: held open until a `resolve` step), then
  `renewDefault`; `input.settle` the settle step's outcome per recovery attempt (`ok`/`fail`, then `ok`);
  `input.grants` per acquire (`ok`/`refuse`, then `ok`); `input.steps` `{at, do, ...}` with `do` one of
  `hold`, `assert {stepMs}`, `release`, `resolve {value}`, `require {key, repair: [outcome per run]}`, `check`.
- `steps`: per input step at virtual time `t`: `result` of `assertCovered` (`{ok:true}` or
  `{ok:false, message, code, status}`), then `held()`, `recoveryPending()`, the work hold's
  `signal.aborted` and its abort reason's message.
- `events` in order: `{t, call: acquire|renew|release, token, recovery?, refused?}`, `{t, log}`,
  `{t, settle}`, `{t, repair, outcome}`.

Node's `runRecovery` releases a failed attempt's hold before it schedules the backoff, and that
release (recoveries still pending) schedules the next attempt at 0 ms: a failed settle or repair is
retried at once (scenarios "fails, backs off and then succeeds", "doubles to its cap", "repair
registered"). The Rust driver applies the backoff Node intends there; the replay names that
difference. A refused fresh hold has no release, so Node's backoff itself is recorded in "a recovery
whose fresh hold is refused".
