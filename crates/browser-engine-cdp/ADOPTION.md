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

## Header-only response policy and current adoption blockers

Response policy uses `Fetch.continueResponse` with the original status, status
text and response headers plus the additional CSP header. It never requests,
re-encodes, limits or replaces a response body. Chromium retains streaming and
framing/compression ownership. `tests/streaming.rs` differentially exercises a
parser-dependent streaming document and a 9 MiB document against the pinned
retained executor, as well as declared-secret redaction in protocol diagnostics.
Like the larger differential, this external test must be explicitly enabled.

**WebSocket enforcement fails on the qualified Chromium build.** Browser-level
Fetch does not emit pause events for WebSocket upgrades. A focused Chromium
157.0.8103.38 probe measured zero page/worker/subframe upgrades with an original
server CSP header, but two upgrades (page and subframe) after modifying the CSP
using `Fetch.continueResponse`; the worker remained blocked. The same failure
occurred with page-level Fetch. Successful protocol acknowledgments do not prove
CSP enforcement. No buffering fallback is accepted or implemented. This needs a
verified header-only enforcement solution before integration.

The earlier 54-action qualification is superseded: its main-page network fixture
referenced undefined driver variables inside page JavaScript and did not run its
requests. The fixture is corrected. Previously measured zero handshake counters
must not be used to qualify the current header-only implementation.

Service-worker probes on fake HTTP `.test` origins also require positive proof
that the service-worker API exists. On this build it remained unavailable despite
the synthetic secure-origin switch; matching empty/pending results do not establish
registration blocking. No worker/subframe adoption closure is claimed.

| # | Area | Current result / remaining gate |
| --- | --- | --- |
| 1 | Popup | Earlier denied first-request regression retained; allowed selection/fallback requires the current full differential. |
| 2 | WebSockets | BLOCKED: Fetch misses upgrades and continued CSP is not enforced equivalently on page/subframe documents. |
| 3 | Workers/subframes | PARTIAL: earlier dedicated-worker/iframe HTTP coverage; positive service-worker capability and popup/subframe registration qualification remain. |
| 4 | Actionability | PARTIAL: full locator language, frame targeting and all fill input types remain. |
| 5 | Proxy auth | PARTIAL: wrong-password/origin-challenge/anonymous-proxy negatives remain. |
| 6 | Isolation | PARTIAL: cache, worker storage and concurrent-download qualification remain. |
| 7 | Cancellation | PARTIAL: exhaustive setup/action/download/close cancellation injection remains. |
| 8 | Audit/errors | PARTIAL: protocol diagnostics no longer retain raw hosts, including secret-bearing hostnames. Complete denial audit/public-error parity remain. |

Do not adopt or retire the retained driver until every gate is closed. No runtime
ownership wiring is part of this change. Do not turn a failing external differential
into a passing gate by excluding the failing network assertions.
