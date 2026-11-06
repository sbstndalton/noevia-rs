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
a proxy grant remains mandatory otherwise. Credential-bearing proxy URLs are rejected before launch. Structured
`proxy_credentials` answer only a matching configured proxy challenge, once per
request; origin challenges never receive proxy credentials.

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

## Round 3 security qualification and remaining gates

The 54-action synthetic matrix passes. Server counters for denied popup first
requests, fetches, and WebSocket handshakes are zero, including iframe and
worker probes. The unmodified retained driver leaks a dedicated-worker WebSocket;
the oracle preserves that raw result while the native safety assertion requires
zero handshakes. Counters reset between engines. This explicit security improvement
is the only normalized outcome difference allowed by the differential.

A dedicated browser connection enables browser-level Fetch before creating the
first page. Auto-attachment pauses new page/worker targets before execution and
recursively attaches descendants before resuming them. Loss of the policy
connection closes the browser. Request interception judges the browser-policy
crate's result. Chromium does not intercept WebSocket upgrades through Fetch;
response fulfilment installs a native CSP connect-src header on documents and
worker scripts. Original CSP headers are retained. Removing page JavaScript
cannot remove this response policy. Scheme/host normalization uses the same policy
verdict. Body framing/compression headers are removed when fulfilling decoded
bodies. Documents and scripts over 8 MiB fail closed; this buffering/limit needs
integration-owner acceptance before adoption. Streaming document parity is not
claimed. Blob/data policy inheritance still needs adversarial qualification.

On this Chromium build, continueResponse acknowledges modified CSP headers but
still sends denied WebSocket handshakes. Reproduction probes retain that failure.
Fulfilled synthetic documents need explicit local-network permissions to contact
the fake .test loopback fixture. Both engines receive those permissions only in
disposable fixture browsers; the helper refuses non-synthetic/non-.test use.
No host permissions or production settings change. The identical fixture action
timeout is 1200 ms for both engines to allow shared build-host scheduling.

| # | Area | Current result / remaining gate |
| --- | --- | --- |
| 1 | Popup | First denied request blocked before network; allowed selection/fallback passes. |
| 2 | WebSockets | Page/iframe/worker denied handshake counters zero; allowed socket passes. CSP buffering/inheritance acceptance remains above. |
| 3 | Workers/subframes | Dedicated-worker and iframe network probes pass. Pre-resume service-worker-registration parity on popups/subframes remains unqualified. |
| 4 | Actionability | Trusted text input/clear, delayed option selection, and sampled pointer stability pass. Full locator language, frame targeting and all fill input types remain unqualified. |
| 5 | Proxy auth | Structured credentials, real 407 challenge, redirect forwarding and two identities pass. Wrong-password/origin-challenge/anonymous-proxy negatives remain unqualified. |
| 6 | Isolation | Independent cookie/localStorage sessions and proxy identities pass. Cache, worker storage and concurrent-download qualification remains. |
| 7 | Cancellation | Dropped approval automatically closes browser and destroys remote objects; explicit close reaps it. Exhaustive setup/action/download/close cancellation injection remains. |
| 8 | Audit/errors | Bounded masked Fetch-denial state and fixed connection-failure class implemented. CSP-denial audit completeness and full public-error parity remain. |

Do not adopt or retire the retained driver until every remaining gate is closed.
The external differential remains ignored in ordinary package checks. The recorded
explicit run used identical source with only the ignore attribute removed in the
owned scratch checkout, through the canonical build wrapper; restore the attribute
for ordinary checks. No adapter/runtime ownership wiring is part of this change.
