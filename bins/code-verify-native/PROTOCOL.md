# Native verifier authority (unpublished implementation work)

This package is being built as the complete one-shot isolated verifier, with
paired caller, image and Compose adoption. The current local boundary routines
are **not an executable verifier**, a passing measurement, or a deployable port.
No partial library publication or production fallback is intended.

Frozen baseline: Core commit `3cbc9f9ee16e7dff4ce9680c46d47828d89d69b2`,
`code-sandbox/verifier.cjs`, copied only to `tests/fixtures/node-verifier-oracle.cjs`.
The generator checks SHA256
`bc763d89b7a0ed27ecc0230cd803f3992a1f22d2193b1553fb4a411449b2025b`
before using its pure exports. It never calls its server/process methods. The
oracle is test-only; it must not be installed as a production fallback.

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

Next implementation work is the real Git copy/execution/owned-process lifecycle
and one-shot server, not another generic protocol layer. Production admission
requires the dedicated Linux PID-namespace-init and identity/capability
contract; no same-UID operator mode. Cleanup must account for all child statuses
and use the all-child `waitpid(-1, WNOHANG | __WALL)` ECHILD barrier, never an
empty proc listing, a single PID wait, WNOHANG zero, or process-group-only proof.
No UID-wide signals, extra capabilities or privileged host tests are permitted.
Actual isolated Linux execution and paired image/caller qualification remain
required before authority/retirement credit.
