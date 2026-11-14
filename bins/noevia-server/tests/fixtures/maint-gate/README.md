# Maintenance gate recordings (sbstndalton/noevia#2641)

The oracle for the front's lose-at-margin lease coverage (`model_maint::gate`, switch
`NOEVIA_RUST_LEASE_COVERAGE=node`) and its settle driver (`model_maint::recovery`): core's own
`server/inference-maintenance.cjs` `createMaintenanceGate` with a native lease grant, on a virtual
clock (its `now`/`setTimer`/`clearTimer` options and the global `setTimeout`, which
`scheduleRecovery` uses). `record-node.cjs` holds the scenarios and writes `gate.json` and
`SOURCES.sha256` (the core file it exercises, and the recorder itself; `gate.json` pins both and the
core commit). The Rust replay is the unit test `model_maint::gate::replay::replays_nodes_gate_recordings`
(src/model_maint/gate_replay.rs), which also checks that the committed recorder's sha256 is the one
that wrote the fixture.

The replay runs a real `Hold` of a real `Maint` with the switch on: the authority's grant and the
recovery `start_release` requires, the hold's decisions at Node's virtual times (`tick_at`,
`renewed_at`, `assert_covered_at`), its release (`start_release` then `finish`), `held` from
`Maint::maintenance_held`, `aborted` from `Hold::lost_wait`, and the settle driver's `attempt`, backoff
and kick with the real admission fence and discharge check. Not driven by the replay (covered over
HTTP by tests/lease_coverage.rs): the renewal loop's own timing and Node calls (the harness keeps
Node's tick schedule and calls the same decisions, and `start_release` at a loss as the loop does),
`start_release`'s barrier/DELETE task, and the front's settle step against the router (scripted).

Named difference, tested (`the_in_flight_wait_loses_at_the_margin_point_itself`): the front's
`lost_wait` checks coverage at the margin point itself (confirmation + TTL − margin, or the hard limit
− margin), so a call in flight ends there; Node's `dispatchSignal` aborts only when its next renewal
tick or an `assertCovered` call loses. Node cannot record that. The hold keeps its times on the std
clock; under a paused tokio test clock `lost_wait` ends on the loss only (no spin), so switch-on tests
run on the real clock.

Re-record on the N5 builder (owner decision 19) from this checkout; `<core>` is a noevia-core
checkout (only that one file and node builtins are loaded, no `npm ci`):

    C=<core>; H=$(git -C $C rev-parse HEAD)
    timeout 900 ~/bin/n5run --with $C --pull --timeout 600 -- "set -e
      F=bins/noevia-server/tests/fixtures/maint-gate
      node \$F/record-node.cjs $C \$F/gate.json $H
      node \$F/record-node.cjs $C /tmp/r6-gate-second.json $H
      cmp \$F/gate.json /tmp/r6-gate-second.json"

Recording twice gives byte-identical files (no wall clock is read).

## Format (v1)

`{ version: 1, coreCommit, recorderSha256, options: {coverageMarginMs, recoveryRetryMs, recoveryMaxDelayMs}, sourceSha256, scenarios }`;
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
retried at once (sbstndalton/noevia#2660; scenarios "fails, backs off and then succeeds", "doubles
to its cap", "repair registered"). The Rust driver applies the backoff Node intends there; the replay names that
difference. A refused fresh hold has no release, so Node's backoff itself is recorded in "a recovery
whose fresh hold is refused".
