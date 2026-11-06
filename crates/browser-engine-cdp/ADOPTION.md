# Native browser adapter qualification

Experimental, unwired adapter. **Do not integrate or retire the retained driver:**
several adoption gates remain partial. `tests/qualification.json` records the
measured scope; passing the external suite is not complete adoption approval.

## Contract

The provisional serialized async `act` / `close` trait owns one independent
browser process/profile per session. Task ownership, approval persistence,
journals and task finalization remain caller responsibilities. chromiumoxide and
chromiumoxide_types remain pinned to 0.9.1 (MIT OR Apache-2.0). No Node runtime
participates in the native path. The retained executor remains the unchanged,
hash-checked oracle identified in `tests/oracle-pin.json`.

The browser-policy crate judges real DOM facts. Approval retains and rechecks the
remote object, origin, facts and focus. Evidence is limited to 20,000 UTF-16 units,
secrets are masked, and screenshots are omitted after secret substitution.
Profiles are fresh and downloads use numbered, non-overwriting destinations.

## Network boundary

The retained executor uses **context.routeWebSocket**, before newPage, to refuse
denied page/frame sockets before connecting. A real Node 22 fixture also found a
dedicated-worker socket escaping that routing on the pinned version. Proxy-only
reliance is therefore insufficient for page/frame parity.

Native launch uses a session-owned authenticated loopback filtering proxy. It
checks the authority against the existing browser policy **before opening an
upstream socket**, including CONNECT. An independently random credential scopes
each listener to its browser session. The configured external egress proxy is
still the upstream hop; it remains mandatory outside synthetic-direct fixtures.
HTTP/HTTPS proxy transport and SOCKS5 tunneling are implemented. External proxy
credentials go only to the configured proxy, never an origin challenge. Do not
interpret untested TLS/SOCKS combinations as qualified.

Chromium receives exactly one proxy-server setting with loopback bypass disabled.
Credential-bearing proxy URLs are rejected; structured credentials never appear
in launch arguments. Header parsing is bounded; response bodies stream without
buffering or size limits. The adapter adds no response CSP and does not intercept
response bodies or headers. Request routing provides action-level refusals;
Network.webSocketCreated provides full URL audit while the proxy independently
prevents connection. CONNECT audit additionally identifies the denied authority.
Proxy tasks and connections are aborted on action cancellation, close and drop.

New targets receive ordered initialization commands before resume, including
service-worker registration blocking and bypass. Initialization replies cannot
all be awaited before resume: Page.enable can wait for the initial navigation.
Protocol errors still close the browser. The main-page initialization is repeated
when the session selects a different page.

## External reproduction

Provide BROWSER_EXECUTABLE (full Chromium), BROWSER_EVIDENCE (fresh owned
directory), ORACLE_CORE, DAV_PARSE_WASM, NODE_PATH (playwright-core 1.58.2), and
BROWSER_NODE (Node 22). Run the browser-engine-cdp package tests through the
approved Linux verification wrapper. The external tests execute when
BROWSER_EVIDENCE is present; without it they explicitly report that qualification
was not provisioned. Such a run never qualifies adoption.

Use full Chromium 157.0.8103.38 in headless mode for the secure-origin fixture.
The matching headless-shell binary ignores the synthetic secure-origin switch:
its .test page has no service-worker API. Full Chromium reports secure=true and
an API object. The fixture requires this positive capability before accepting
registration results. All pages are synthetic and loopback-backed. No host
security settings or certificate-error bypass are needed. Launch flags retain
--no-sandbox, matching the retained driver's sandbox defaults.

Compare normalized public results, real DOM effects, numbered download bytes,
origins, proxy identity, storage and close confirmation. The fixture counts
requests and upgrades reaching denied destinations and credentials reaching an
origin challenge. A late refusal cannot pass this gate. Separate tests retain
parser-dependent streaming and a 9 MiB document.

## Remaining adoption work

The current suite adds first-script popup registration, special date input fill,
WebSocket denial audit, public secret redaction, wrong-password/origin/anonymous
proxy cases, CacheStorage/IndexedDB isolation, concurrent same-name downloads,
and cancellation during a timed wait in addition to approval cancellation.

Exhaustive setup/action/download/close cancellation injection, shared-worker
and storage coverage, remaining transport errors, and live TLS/SOCKS proxy
qualification remain open. Existing
passing behavior is recorded as such; no artificial fail-before is invented for
cache isolation or cancellation cases that already matched the retained driver.

The exact contract fixture additionally compares public reason strings, typed
unknown actions, invalid selectors, missing uploads, unsupported fill/select,
read-only timeout, invalid keys, absent targets, approval-card shape, ordered
lifecycle callbacks, task state and repeated close. `launch_with_log` supplies
an optional task-scoped audit callback; ordinary `launch` uses no callback.
Transport denials from browser-background services remain diagnostics rather
than task audit entries. The owner-approved audit exception redacts the retained callback's raw
action-type field when it contains a declared secret.

The owner approved redacting declared secrets at the audit callback boundary.
That deliberate exception replaces the retained raw action-type echo. The
contract test compares every other audit field exactly and checks that no raw
sentinel reaches the callback.

Locator/frame grammar uses the retained package's unmodified, hash-pinned
browser-side DOM evaluator in isolated worlds. Rust owns traversal, CDP
connections, deadlines, remote handles, policy and input dispatch. Separate
renderer connections are scoped to each action. The frame suite compares
actual same-origin and cross-origin fill/click effects, requires a positive
iframe renderer target, checks secret binding and approval cards, and refuses
the original node if its frame navigates during approval. The input suite
covers label retargeting, contenteditable, color/range/number validation and
selecting by label using the same browser-side algorithms. Transport negatives,
remaining worker/isolation cases and exhaustive cancellation still gate wiring.

The local-only transport suite now compares eleven exact outcomes across TLS
CONNECT, encrypted HTTPS proxies and SOCKS. It covers wrong credentials, origin
challenges, both certificate rejection layers, unsupported SOCKS authentication
and refused tunnels. Proxy failure classifications are fixed and scoped to the
navigation generation and authority; upstream bodies/messages are not copied.
The fixture uses a proper ephemeral CA and server leaf. Its additional native
trust is limited to a literal-loopback HTTPS proxy with synthetic-direct enabled;
admission tests reject that option for runtime/non-loopback use. Production trust
and selector defaults remain unchanged.
