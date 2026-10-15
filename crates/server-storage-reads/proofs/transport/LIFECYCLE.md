# Synchronous private child checkpoint

This is a concrete lifecycle/IPC proof, not an HTTP transport or production
adapter. `src/isolated.rs` owns a child and an admission Permit, controls bounded
binary IPC with fixed explicit capacities and4096-byte nonblocking steps, gates
stdin on exact READY, and commits body only after framing/EOF/exit/confirmed reap.
On errors it closes local pipes before cleanup. Cancellation uses a caller-owned
AtomicBool; no worker or detached cleanup is created. Cleanup retains the leader
with waitid NOWAIT, signals the group before consuming exit, polls exit and reaps.
The permit releases only on confirmed reap; failed confirmation quarantines it.
Kernel scheduling/kill/reap have no hard real-time guarantee. This synchronous
proof can wait while an uninterruptible child remains owned; async supervisor
ownership and shutdown behavior remain held. It must not run in a production
async request handler or be wrapped in unmanaged spawn_blocking.

Private limits: input16MiB, body≤4MiB, response framing20bytes total, stderr4096,
pre-READY timer1second within caller deadline. Parent preallocates exact requested
capacities before reading, so input-dependent geometric growth is avoided; actual
allocator overhead/capacity and whole parent parser/sort heap are not bounded here.
Input buffers are Zeroizing; no secret enters argv/environment/diagnostics.
Response bytes are intentionally arbitrary decoded-content bytes. Request mode
and executable identity are a trusted caller contract, not an auth capability.

Linux-only setup occurs in ordinary child main before input/READY: safe rustix
NotDumpable/readback, CORE0, AS268435456bytes soft/hard, CPU5seconds soft/6hard,
limit readbacks, bounded /proc/self/statm read and128MiB baseline qualification.
No laterexec/credentialchanges exist in the fixture. Standard OOM can terminate
the child; no unsafe pre_exec/custom allocator/recoverable-OOM claim. Unsupported
platform refuses. Linux branch has not been compiled or executed on the macOS author host;
exact-target Linux CI baseline, API feature build and limit probes remain required.

Primary Linux sources verified 2026-10-11:

* [getrlimit(2)](https://www.man7.org/linux/man-pages/man2/getrlimit.2.html): AS is
  page-rounded virtual memory bytes and affects brk/mmap/mremap; CPU soft signal
  and hard kill use seconds. Limits are not RSS or wall-clock execution bounds.
* [PR_SET_DUMPABLE](https://man7.org/linux/man-pages/man2/pr_set_dumpable.2const.html):
  dumpable0 controls core production and credential/exec operations can reset it.
* [core(5)](https://www.man7.org/linux/man-pages/man5/core.5.html): piped collectors
  can ignore CORE; NotDumpable is mandatory rather than relying on CORE0 alone.
* rustix1.1.5 process/prctl.rs:80–151 safe setter/getter; process/rlimit.rs:8–41
  safe limit types/setter; backend/linux_raw/process/types.rs:10–32 resource IDs.

`private-child-proof` is exclusively synthetic: fixed explicitly named fixture
modes bypass Linux setup for portable lifecycle controls; `linux-echo` calls the
real setup and refuses elsewhere. It performs no network, resolver or account
action. Blocking reads/writes exist only inside this owned, killable synthetic
child; parent's IPC is nonblocking. Never expose fixture modes as a capability.

New focused controls: six byte lengths across4096 boundaries preserve all256 byte
values; oversized16MiB input; response overflow; malformed READY; stderr overflow;
extra trailing byte; success frame followed by abnormal exit; already-cancelled
attempt; retained stall before reading large stdin; absent READY; admission can
reacquire only after completed cleanup. Initial three-test run passed byte and
platform checks; stderr test first raced its own early exit. Corrected fixture
holds after noise, and reran only the affected control once: PASS. This is a
fixture correction, not a deployed incident. Existing33-client/340-UTF8 and other
passing groups were not repeated.

Remaining new controls: delayed/in-flight cancellation, retained inherited pipes
on this owner, reap failure quarantine, syscall/setup/readback failures, actual
Linux AS/CPU/dumpability probes, no-secret-before-READY observer and meaningful
implementation mutants. Earlier immutable process-seam controls do not certify
this new owner automatically. No whole cancellation certification is claimed.

### Subsequent owned qualification slice

New owned_controls now proves no stdin bytes before READY (child observes its
nonblocking pipe before announcing READY); cancellation triggered after actual
stdin progress; admission held at progress and immediately before reap; inherited
same-group pipes with exited leader still observable by NOWAIT; spawn failure.
All four checks passed once. Private observation callbacks are secret-free test
contracts, not production cancellation hooks. Reap-failure quarantine and Linux
syscall/limit failures remain pending. Neither docker nor podman executable is
on this host PATH; no daemon was contacted and no Linux probe is claimed.

Rust owned_mutants copied actual lifecycle source to isolated owned temporary
crates and changed one source anchor each: READY gating bypass, cancellation
ignored, admission released at construction. All three compiled, then failed
their designated test (compile failures do not count). Fixture sources are
retained under the reported owned temporary directory. These target this finite
owner only; no full mutation campaign or network guarantee follows.

The original ae6ee mutant claims above are provisional: the generic FAILED footer
did not identify the semantic cause (#1374). The subsequent narrow repair binds
status101, exactly one selected failure/sole panic, unique policy payload and
current assertion line/column. READY requires child exit42 observed before reap,
which means actual bytes before READY; unrelated setup/protocol errors fail a
different control assertion. CANCEL requires actual progress and cleanup before
the policy deadline assertion. ADMISSION asserts an intermediate missing hold.
All three affected actual source mutations reran once with the maintained Request
copy inventory and passed exact classification; captured outputs/fixtures remain
in a private temporary directory outside the repository.
Forged process/setup/compile/deadline/cap/protocol/wrongtest/location/policy/extra
panic controls were rejected, including actual compiled output variants. The two
affected unmodified policy tests passed once. This supersedes only the earlier
generic semantic-kill claim; broader transport/production qualification is held.

request_ipc implements a bounded binary owned Request contract: GET/PROPFIND,
HTTP(S) URL≤64KiB without userinfo,100 headers/name128/value12MiB, aggregate16MiB,
body cap4MiB/800000, timeout>0≤20s encoded with exact nanoseconds, no Debug/secret
diagnostics. Caller configured-origin authorization is still mandatory. Decode
checks every length before copying and rejects malformed/trailing input. Input
and header copies are Zeroizing where defined. Roundtrip/all truncations/trailing
data/CRLF refusal passed; nanosecond preservation changed the wire format and its
affected test reran once. This request contract is not yet consumed by a network
child; bounded HTTP status/failure metadata and actual pipeline remain pending.

Still held: ordinary HTTP/TLS/codec pipeline, original-host SNI/OS resolver,
bounded encoded input, header/trailer policy and typed phase retry; full actual
oracle; parent allocation/sort; async ownership; owner/AAD/auth/CSRF/store/routes;
default-off feature/supervisor gates; workspace compatibility; independent final
Sol6.1HIGH and fresh authoritative Linux CI. No shared paths were changed.
