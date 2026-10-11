# Phase1 Request consumption and typed response checkpoint

Implemented privately: separate M4HTTP01 response codec (18-byte metadata), finite
fragment accumulator, phase/failure combination validation, cap+one raw payload
and capped-flag consistency, empty status-error bodies, checked trailing/truncated
data. No Debug or reflected diagnostic strings. Header status metadata is fixed;
body bytes remain intended remote-content output. Parent final commit still needs
IPC EOF/exit0/confirmed reap. Frame::finish by itself does not certify these gates.

child_request consumes actual Request IPC after READY with length≤16MiB/exact
input/EOF and Zeroizing encoded bytes, then hands owned Request to child caller.
Its blocking IO is exclusively inside the owned killable child; parent owner is
unchanged and nonblocking. The Linux-required private-request-child initializes
linux_setup, current-thread Tokio/max1 blocking worker/2MiB stack and public root
configuration/no-client-auth/no-resumption/noearlydata BEFORE READY. No mode or
bypass exists. It performs no HTTP and returns fixed Other/Headers refusal. There
is no capability activation or fallback. Separate private-request-fixture accepts
only fixed synthetic modes and emits fixed GET/PROPFIND payloads; never use it as
production executable or pass real credentials. No network/socket/DNS tests ran.

Explicit nesting: inner M4HTTP01 is encoded as payload of the existing synthetic
BODY envelope for portable proof. Those versions are separately decoded. Existing
owner output cap4MiB means full4MiB+one+metadata cannot fit; full listing transport
requires a separately reviewed enlarged/versioned owner boundary. No silent
reinterpretation or lowered listing cap is introduced. Current portable proof
uses800000preview request, whose envelope fits; Linux stub error always fits.
Neither binary is an HTTP adapter.

New offline checks passed once: every two-part split of six status frames, all
truncated prefixes/trailing bytes; typed DNS/TLS/preheader/body/timeout failures;
illegal retry phase, body overflow and unknown kind/phase/code/flag refusal;
actual Request consumption for GET/PROPFIND through owned portable artifact.
Added affected-only commit-gate check passed: valid inner frame followed by
nonzero exit, extra outer byte or stall must never commit; admission reacquires
after cleanup. Existing cancellation/READY proofs are preserved, not rerun.

Private manifest adds exact Tokio1.53.2/rustls0.23.45/publicroots1.0.9 already
present in dependency proof lock; private lock adds27 packages, no shared lock.
This feature-unifies dependencies in the private crate; old semantic source bytes
remain unchanged but no new whole-corpus certificate is claimed. Maintained
mutant runner copies new response/child modules; no earlier passing mutant group
repeated and no new response semantic mutant run is claimed.

Held: phase2 plain HTTP/owned loopback request bytes/status/redirect/informational
headers/wire/logical-header/trailer limits and meaningful source-bound mutants;
full listing-cap IPC owner boundary; exact Linux setup/runtime/baseline/resource
probes; TLS/DNS/codecs actual runtime corpus; async supervisor/reap failures;
parent parser/sort memory; owner/AAD/auth/CSRF/store/routes/shared/defaultoff.
Mac compilation of startup APIs is not Linux resource execution. Standard OOM
and kernel cleanup timing caveats remain. No production or publication approval.
