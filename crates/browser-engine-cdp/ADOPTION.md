# Native browser adapter qualification

This crate is experimental and is not wired into the server. Do not retire the
existing driver based on this crate compiling. The adoption gate is the synthetic
differential suite, plus the outstanding adversarial cases below.

## Contract mapping

At implementation intake the task-runtime branch did not contain `BrowserEngine`.
This crate defines a provisional serialized, per-session async `act` / `close`
trait, a `Launch` configuration, and an async `Approval` callback. `CdpEngine::launch`
corresponds to executor `open`; ownership of one instance corresponds to a session
identifier; `act` takes the same action JSON; `close` returns confirmation. Task
ownership, approval persistence, job journaling and `closeTask` remain with the
caller. No other worker's executor files are changed.

The native policy uses the existing browser-policy crate's operations 1, 2 and 3.
DOM descriptions are the executor's self-contained function. Approval binds the
remote object, facts, origin and keyboard focus. Typed placeholders resolve only
after the substitution policy permits them. Text uses the existing UTF-16
20,000-unit limit, and screenshots are suppressed once substitution uses a secret.
The initial process-per-session implementation isolates browser state and proxy
configuration; it does not pool profiles.

## Dependencies

The adapter pins chromiumoxide and chromiumoxide_types to 0.9.1. The published
manifest licenses them MIT OR Apache-2.0. Upstream was unarchived at inspection,
with a recorded repository push on 2026-04-03 and latest published release on
2026-02-25. This is maintenance evidence, not a security or compatibility promise.

Sources: <https://crates.io/crates/chromiumoxide>,
<https://github.com/mattsse/chromiumoxide>.

## Reproducing the synthetic gate

Supply `BROWSER_EXECUTABLE`, `BROWSER_EVIDENCE` (fresh owned directory),
`ORACLE_CORE` (a checkout/export of the pinned Core main), `DAV_PARSE_WASM`,
`NODE_PATH` (playwright-core), and optionally `BROWSER_NODE` (Node 22).
Run the ignored `synthetic_differential` integration test through the approved
build queue. The test starts an HTTP server on loopback, resolves fixture.test to
it inside both browser processes, and uses no external page. Driver reasons and
PNG bytes are normalized; outcome class, origin, extracted text, omission metadata,
and numbered download bytes/names are compared. The full JSON report remains in
the evidence directory. The test is ignored in ordinary cargo tests because it
requires those explicit external artifacts; a skipped test is not a passed gate.

The oracle revision is `99ebd9c951b16cb6beeeae8bd79722cf4f3368bb`;
`tests/oracle-pin.json` verifies the imported source bytes.
The oracle executor is imported directly from the exported Core main, rather
than reimplemented. The fixture and DOM description script are the only JavaScript
in this crate. Neither participates in the native production runtime.

## Required qualification beyond the basic action matrix

- Popup attachment before the first request; newest-page selection and close fallback.
- Allowed and denied WebSocket handshakes, subresources, redirect hops and request audit.
- Same-node replacement and fact/focus mutation during approval.
- Actionability retry parity for hidden, covered, disabled and delayed controls;
  trusted input events, selection and modifier combinations.
- Proxy authentication and per-session grants; no direct production networking.
- Service worker registration refusal parity (bypass plus navigator shadowing is
  currently an approximation, not proof of parity).
- Concurrent downloads, cancellation at every setup/action/approval/close await,
  profile cleanup after failed setup, and bounded failed-close confirmation.
- Two simultaneously live sessions with cookie/storage isolation.
- Extraction boundary/secret URL encoding and error masking adversarial cases.

These gaps keep adoption closed even if basic action comparisons pass. The
remaining task-runtime owner must adopt a qualified contract before integration.

## Observed environment blocker

The first native differential attempt failed before the first action: the test
host's browser reported `No usable sandbox` under restricted unprivileged user
namespaces. No browser sandbox was disabled in the native adapter. The existing
oracle launch uses its normal launch defaults; that succeeding does not establish
that the native launch environment is qualified. A sandbox-capable test environment
or an explicitly authorized synthetic-only exception is required before native
action comparisons can execute.

Alternative considered: headless_chrome 1.0.22 (MIT), whose primary repository
shows request interception and incognito support. Its synchronous tab interception
API does not remove the Chromium sandbox requirement, and replacing the async
candidate would not resolve this measured startup failure. Retain the current
candidate pending actual runtime parity evidence. No claim is made that either
library alone supplies context-wide popup/WebSocket enforcement.
Sources: <https://docs.rs/crate/headless_chrome/latest/source/>,
<https://github.com/rust-headless-chrome/rust-headless-chrome/blob/main/tests/simple.rs>.
