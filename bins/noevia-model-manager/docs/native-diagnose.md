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
scrubs this confirmation. Services d245183 ships the production refusal guard in
`_require_token`; public qualification additionally keeps its explicitly test-only AST
shim guard.

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
failure or timeout returns 200 with empty failures.

## Redaction

Python `backend_diagnose` (services #1379) redacts each failure's model name and every
evidence line with `redact_log_line` after matching. The port does the same with the
existing Redactor, after matching, so rule order and failure bookkeeping are unchanged.
The oracle is pinned to services `d890ab476d1f1a8b6de799d6428e45b6af42a473`. The pure
differential fixture records the actual `redact_log_line` output for every case and the
Rust answer must equal it exactly; the route fixture runs the actual `backend_diagnose`
with a credential-shaped model name and evidence. Two named stronger security
differences remain:

- the Redactor is broader for numeric-looking credentials (`password=12345`,
  `TOKEN=12345`), which Python's pattern deliberately leaves (the fixture asserts that
  these cases still differ, so the exception cannot go stale), and for its Unicode word
  and delimiter classes (see the README);
- cross-line contextual suppression: evidence whose joined block redacts differently
  from its separate lines is dropped entirely, where Python keeps the raw line.

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
