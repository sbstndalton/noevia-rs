# Native verifier authority (unpublished implementation work)

This package is being built as the complete one-shot isolated verifier, with
paired caller, image and Compose adoption. The current local executable draft implements the Git/copy/test flow, namespace-init
collector and private helper, but is **not qualified**, a passing measurement,
or a deployable port.
No partial library publication or production fallback is intended.

Frozen baseline: Core commit `3cbc9f9ee16e7dff4ce9680c46d47828d89d69b2`,
`code-sandbox/verifier.cjs`, copied only to `tests/fixtures/node-verifier-oracle.cjs`.
The generator checks SHA256
`bc763d89b7a0ed27ecc0230cd803f3992a1f22d2193b1553fb4a411449b2025b`
before using its pure exports. It never calls its server/process methods. The
generator also evaluates only the frozen private positive-option declaration
in an empty JavaScript VM context. The oracle is test-only; it must not be installed as a production fallback.

`CODE_VERIFY` remains operator-only, first valid repository entry wins. Names
are ASCII 1–64 `[A-Za-z0-9_.-]`; command trimming follows ECMAScript whitespace
(including FEFF), with a 4096 UTF-16-code-unit bound. A second pipe belongs to
the command. Requests cannot supply a command.

Accepted defensive framing amendments are explicit: a request is one UTF-8 JSON
line capped at 64 KiB **bytes including its newline**, with exactly `noevia`,
`repo`, `source`, `headSha`, `nonce`. Unknown/duplicate fields and malformed UTF-8
refuse. The Node baseline used a JavaScript string-length cap and ignored
additional fields; these are intentional stricter malformed-input refusals,
not claimed byte-for-byte parity. Current caller requests are unchanged.

The output tail retains bytes before UTF-8 replacement decoding. `tailBytes`
counts retained input bytes; downstream display sanitization is still the
caller's job. The cumulative counter saturates at the largest safe JavaScript
integer rather than overflowing. No tail or agent-provided JSON can establish
a test verdict. The caller's existing original-object measured-report trust
boundary remains separate and must be preserved.

The same production Git/copy/execution flow is exercised with a private portable
runner over disposable repositories; that runner is compiled only in tests and
does not establish identity or descendant isolation. Production admission
requires the dedicated Linux PID-namespace-init and identity/capability
contract; no same-UID operator mode. Cleanup must account for all child statuses
and use the all-child `waitpid(-1, WNOHANG | __WALL)` ECHILD barrier, never an
empty proc listing, a single PID wait, WNOHANG zero, or process-group-only proof.
No UID-wide signals, extra capabilities or privileged host tests are permitted.
Actual isolated Linux execution and paired image/caller qualification remain
required before authority/retirement credit.

The native reader walks source descriptors without following links and accepts
only a regular single-link config file of at most 1 MiB. These are explicit
defensive source-config restrictions beyond the baseline readFile behavior;
they prevent the root reader from following a source-controlled config link.
Git stdout capture is bounded at 1 MiB; operator output tails remain at most
256 KiB. Finite operator numeric coercions preserve ECMAScript trim/radix and
floor semantics, with an explicit largest-safe-integer native duration bound.

The Linux draft uses an authenticated root-init socketpair bootstrap, closes its
bootstrap authority before untrusted exec, clears supplementary groups and
real/effective/saved credentials/capabilities, and accounts for statuses with
admission-generation tickets. A private signalfd handles idle/active termination;
blocked supervisor signals do not leak into repository exec. The terminal
collector has a per-poll bound so an exiting fork tree cannot starve deadlines.
A negative PID-keyed measurement control fails the actual ticket regression.

Remaining proof includes actual Linux helper/channel/UID/capability/descriptor
semantics, malformed/partial/disconnected framing, copy and command deadlines,
escaped descendants and clone-child/all-child drain, shutdown, forced cleanup
uncertainty, immutable measured Core caller, and paired image/Compose adoption.
Nothing in the portable tests or Linux cross-target checks supplies that proof.
No source push, partial PR, runtime activation or release pin update is approved.

The first whole-source review held the draft for three concrete issues. The
local repairs reject worker GID0 before socket/admission and use a fresh session
for every untrusted exec. The worker parser is the same pure policy used by the
production profile and authenticated helper; a root-only signalling helper is
not a worker role. An owned ordinary-user child tests the actual session entry
function; it does not establish Linux cross-identity signalling proof.

Production admission now requires a sealed, unexpired original-budget token.
The bounded helper envelope carries an absolute Linux monotonic deadline,
sampled conservatively without refreshing the original remaining duration;
it is checked on receipt and again immediately before exec. A terminal status
first observed after expiry is timed out even if it is exit0, and late drain
cannot hide expiry. Separate cleanup grace never authorizes another normal
phase. Deterministic tests exercise these exact production decision functions;
actual isolated Linux deadline/framing/identity fault evidence remains pending.

The protected socket directory also contains one bounded root-owned admission
receipt, `.noevia-native-admission.json` (regular, single-link, mode0600). It is
reset for the accepted nonce and records successful creation of the test Exec
helper immediately after admission tracking and before bootstrap/payload progress.
A test helper that fails before exec or is killed before JavaScript still counts.
Git, cleanup and signalling helpers are not test-Exec admissions. The one-shot
flow permits at most one test helper; unexpected additional admission fails closed.
The final receipt is sealed only after the final all-child ECHILD/status barrier;
receipt IO failure refuses a response. It contains no paths, commands or output.

Readers need the protected file's ownership/type/link/mode/bound, a final flag,
and the exact nonce from their correlated response. Missing, stale, unfinished or
unreadable evidence is uncertainty, never proof of zero admission. This metadata
is an always-on bounded internal receipt, not a test bypass/public helper mode,
new selector or unbounded audit log. Its descriptor is CLOEXEC; workers cannot
enter the root-owned socket directory. The ordinary Core caller remains unchanged.
Actual Linux proof includes a successful helper followed by `/bin/false` before
any payload marker, which must still produce a final count1; the original-budget
copy refusal must have count0. Payload-marker absence alone is not admission proof.
