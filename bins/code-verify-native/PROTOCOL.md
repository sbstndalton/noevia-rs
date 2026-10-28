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
