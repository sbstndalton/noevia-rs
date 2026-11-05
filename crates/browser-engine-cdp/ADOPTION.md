# Native browser adapter qualification

Experimental, unwired adapter. **The adoption gate fails. Do not integrate or
retire the retained driver.** See `tests/qualification.json` for per-case results.

## Contract and dependency

At intake the task-runtime branch had no `BrowserEngine`. This crate defines a
provisional serialized per-session async `act` / `close` trait, `Launch`, and an
async `Approval` callback. `launch` maps to executor `open`; one instance maps to a
session identifier; `act` accepts the same action JSON; `close` returns confirmed
process completion. Task ownership, approval persistence, journals and closeTask
remain caller responsibilities. No other worker's files are edited.

chromiumoxide and chromiumoxide_types are pinned to 0.9.1, MIT OR Apache-2.0.
At inspection, latest publication was 2026-02-25; upstream was unarchived with a
repository push on 2026-04-03. Sources: <https://crates.io/crates/chromiumoxide>,
<https://github.com/mattsse/chromiumoxide>.

The existing browser-policy crate judges exact action/element/page projections.
Approval retains and rechecks the remote object, facts, origin and focus. Text is
limited to 20,000 UTF-16 units and masked; screenshots are suppressed after secret
substitution. Each session owns a fresh process/profile. Numbered downloads are
saved without overwriting existing targets. Direct access is explicit for tests;
a proxy grant remains mandatory otherwise. Credential-bearing proxy URLs are
rejected before launch because proxy authentication is unimplemented.

## Launch parity

`src/launch-flags.json` records the pinned driver's common Linux/headless switch
set, including `--no-sandbox`. The root authorized that flag as parity with the
existing launch defaults; all executions here use disposable synthetic fixtures,
and no host AppArmor/sysctl settings are changed. The adapter remains unwired.
The runtime test loads the pinned driver's own switch generator and compares it
with `Browser.getBrowserCommandLine`, including headless switches and absence of
certificate-error bypass. chromiumoxide adds a fresh user-data-dir and debugging
port. A port replaces the existing driver's debugging pipe; argument order and
duplicate switches are normalized by the library.

The high-level navigation watcher exceeded the 400 ms test deadline after the
page loaded. Native navigation therefore uses chromiumoxide's raw `Connection`
with `Page.enable`, `Page.navigate` and `Page.loadEventFired`, under the action
deadline. It retains GET navigation and browser referrer defaults; it does not
navigate by assigning location in page JavaScript.

## Reproduction and comparison

Provide `BROWSER_EXECUTABLE`, `BROWSER_EVIDENCE` (owned directory), `ORACLE_CORE`,
`DAV_PARSE_WASM`, `NODE_PATH` (playwright-core 1.58.2), and optionally `BROWSER_NODE`
(Node 22). Through the approved build queue, explicitly run the ignored
`synthetic_differential` integration test. Ordinary package tests skip this test
because it requires provisioned external artifacts; skipping is not qualification.

The oracle is Core `99ebd9c951b16cb6beeeae8bd79722cf4f3368bb`. Committed
`oracle-pin.json` enforces source hashes; the policy WASM is checked by the
unchanged oracle loader. No oracle code is patched. JavaScript files here are
synthetic fixtures or scripts evaluated in Chromium; no Node runtime participates
in the native adapter.

Both engines use the same loopback HTTP/WebSocket fixture server and fake .test
hosts. The added secure-origin switch is limited to this fixture and permits a
real service-worker-registration probe over loopback HTTP. No external page is
visited. Each run gets a fresh evidence directory.

Compare outcome class, origin, masked text, screenshot omission, input/select/file
DOM effects, navigation path, and downloaded names/bytes. Driver diagnostics and
PNG bytes are normalized. DOM snapshots wait a bounded 40 ms after each action;
download contents are compared after an explicit bounded completion barrier,
because the event can arrive on either side of an action response. Raw API
responses are retained in the report. The server also counts denied fetch,
WebSocket and popup requests, so a late refusal cannot look like enforcement.

## Remaining gaps and evidence

| Area | Qualification and exact remaining gap |
| --- | --- |
| Popup interception/fallback | Allowed popup selection and close fallback pass the original matrix. A denied popup's first request reaches the fixture once before policy is installed on the new target, then the action reports blocked. Pre-resume target interception is missing. The unchanged oracle's denied-popup probe separately crashes in `Request.frame()` before the popup frame exists; run it with `BROWSER_DENIED_POPUP_PROBE=1`. That reference fault is retained as evidence, not suppressed or patched. |
| WebSockets | Allowed handshake passes. URLPattern blocking refuses denied fetches but does **not** block a denied WebSocket handshake: the server observes it and the page reports allowed. Native connection interception before the handshake is missing. The egress proxy remains required, but its presence is not accepted as proof of parity for this failed layer. |
| Service workers | Secure-context registration resolves undefined without registering, matching the reference's async no-op. Bypass is also enabled. Subframe/worker-target and popup-before-attachment coverage is not qualified. |
| Actionability | Delayed lookup, disabled hover, delayed enabled click, same-node replacement, basic click/hover and DOM effects pass. Full stable-animation checks, complete editable/fill/trusted-event semantics, option waiting, iframe targeting, and the reference locator language beyond CSS are unimplemented or unqualified. Fill currently uses DOM setters/events; no claim of trusted-event parity. |
| Proxy auth | Embedded credentials fail closed before launch; unit tested. Authenticated grant fields/challenge responses are not implemented. Anonymous proxy forwarding and all redirect hops have not been integration-tested here. Proxy loopback bypass is disabled when a proxy is configured. |
| Isolation | Two live sessions on the same origin have separate cookies/localStorage; both close with process confirmation. Cache/worker/download concurrency and per-context proxy tests remain. |
| Cancellation | Dropping an action while approval is pending poisons admission; a further action is blocked, the consequential effect does not occur, and explicit close confirms cleanup. Automatic immediate browser/context close on cancellation, release of retained objects at every cancellation await, and exhaustive setup/action/download/close cancellation injection remain unqualified. Caller must close interrupted instances. |
| Other | Request audit/state projection, concurrent download/error handling, wildcard/IDNA allowlist normalization, and full error-string parity remain. |

These are adoption blockers even when the original action matrix passes.

Alternative considered: headless_chrome 1.0.22 (MIT), which exposes synchronous
request interception and incognito APIs. Those APIs use the same protocol family;
its primary documentation does not establish pre-handshake WebSocket or
pre-resume popup enforcement. Switching libraries alone would not resolve the
measured gaps. Retain this candidate without claiming qualification. Sources:
<https://docs.rs/crate/headless_chrome/latest/source/>,
<https://github.com/rust-headless-chrome/rust-headless-chrome/blob/main/tests/simple.rs>.
