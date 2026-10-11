# Native backend diagnosis

The feature `native-diagnose` includes `native-logs`. `MODEL_MANAGER_DIAGNOSE_IMPL`
defaults to `python`; `rust` owns the whole internal
`GET /api/v1/backends/{name}/diagnose`. A native refusal never delegates diagnosis
to Python. Public authentication and administrator policy remain owned by the
actual Node proxy.

Selected native diagnosis requires exact `TZ=UTC`. Missing TZ, aliases and other
zones fail configuration before bind, child spawn or store access. The default
Python selector preserves the parent's timezone. The selected native child inherits
explicit UTC and receives `NOEVIA_NATIVE_MODEL_MANAGER_DIAGNOSE_CONFIRMED=1`;
the supervised Python child receives `0` when diagnosis is off, and Python exec
scrubs this confirmation. The production services refusal guard is a separate
follow-on: public qualification uses an explicitly test-only AST shim guard.

## Acquisition and refusals

Configured names are unioned with discovery of stopped and running containers.
Discovery excludes the manager itself and uses only an image's first tag for the
llama.cpp match. Per-candidate ordinary discovery errors skip that candidate.
Unknown names return 404 before logs acquisition. Admitted names contain only
ASCII letters, digits, underscore, dot, colon or hyphen, with at most 256 bytes.
Invalid names return 400; this is a named stricter difference from Python's unknown
404 for otherwise unmatched names.

The closed Docker seam permits inventory, inspect, image inspect and current-run
logs only over a local Unix socket. Inventory admits at most 1,024 containers,
using a 1,025-entry sentinel request. Metadata is capped at 1 MiB, headers at 8 KiB
and 32 fields, configured input at 64 KiB, effective names at 2,048, and URIs at
8 KiB. Raw and replacement-decoded logs are capped at 8 MiB. No host TCP, arbitrary
operation, exec or mutation is exposed.

Logs use a canonical StartedAt converted to UTC whole seconds and fixed
`tail=40000`, stdout/stderr, non-streaming and no timestamps. Missing or unsupported
StartedAt returns empty failures, rather than Python's possible unfiltered read.
The actual Docker SDK 7.1.0 gate demonstrates that Python's naive conversion depends
on local timezone; the UTC selector gate is a stronger admission rule, not a fix to
the existing Python path. After logs, inspect of the same ID must preserve ID,
StartedAt and TTY; a restart or mismatch discards the result. This is a race check,
not an atomic Docker snapshot.

Non-TTY frames require exact eight-byte Docker headers, stream kinds 1 or 2,
zero reserved bytes and complete bounded payloads. TTY reads are raw. Ambiguous
Content-Length/Transfer-Encoding, unsupported encodings, trailers, redirects,
malformed framing and exceeded caps return fixed sanitized 503. Ordinary transport
failure or timeout returns 200 with empty failures. Native evidence always passes
the existing Redactor; redaction is a named stronger security difference.

## Ownership and deadline

One five-second arrival deadline covers inventory, inspect, logs, re-inspect,
analysis and serialization. Four HTTP admissions and two retained diagnosis jobs
use try-only admission. Cancellation or timeout can return best-effort empty
failures while cleanup retains ownership. Each connection driver handle is retained
before awaiting its operation; HTTP admission is released only after cleanup
observes all driver joins. Shutdown cancels and joins retained controllers and
drivers. The existing logs endpoint is unchanged and retains its separately
recorded driver ownership limitation.

These bounds do not establish hard process RSS, OOM, kernel resource, escaped
descendant or universal fallible-allocation guarantees. Qualification uses fresh
private synthetic state and fixed fake Unix Docker only; it makes no live runtime
or deployment claim.
