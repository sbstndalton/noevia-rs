# Diary storage backoff kernel

A pure decision kernel for the shipped Python `storage_backoff.py` Retry-After
parser and one `StorageGate`'s cooldown transitions. The caller supplies both
wall-clock and monotonic samples and serializes a gate shared per credential.
This crate makes no HTTP requests, stores no credentials, creates no gate
registry, handles no auth/DB/files, and does not choose a service implementation.
It does not change a production selector or image.

`tests/storage-backoff-fixtures.json` is regenerated directly from the shipped
services module with CPython 3.12.15/Unicode 15, UTC timezone and C locale,
using a stub `httpx` class only to import the module. Synthetic cases cover
numeric/HTTP-date parsing, retry rounding, 401/403/429 distinctions, throttle
extension, in-flight success, active-login precedence, expiry and reset rate
limiting. No real storage server or clock is consulted.

The production HTTP `Retry-After` field is bounded to 8192 UTF-8 bytes here.
Longer headers return the supplied default; Python has no such cap. Date
parsing follows the shipped CPython email-date token grammar, including
weekday-independent dates, full month names, reordered asctime fields,
two-digit years, dot-separated times, named/numeric offsets and Unicode
integer syntax. Gregorian date and offset validity match Python datetime.
The adapted grammar and Python license are retained in this package.

`parse_retry_after` and `StorageGate::record` are UTC conveniences for naive
or unknown-zone dates. For a non-UTC caller, use
`parse_retry_after_with_local_date` and `record_with_local_date`: the supplied
resolver receives the parsed `chrono::NaiveDateTime` and must reproduce the
host Python timezone, DST/fold and timestamp-refusal behavior. Explicit zone
or offset dates never invoke it. Resolution failure retains the supplied
retry default. This pure kernel reads no timezone or clock; service adoption
must supply and qualify the same host-time contract rather than silently
assume UTC. Finite clock samples within ±1e12 seconds are required.

Additional source-derived edge cases live in `tests/oracles/parser-edges.json`.
They cover permissive date syntax, invalid dates/offsets, Unicode numeric and
zone forms, expiry equality and neighboring floating-point samples, negative
and large clocks, reset equality and in-flight completion. The new generators
require CPython 3.12.15 / Unicode 15.0.0 and the pinned shipped-source hash;
the original frozen fixture/generator remain unchanged.

Regenerate on the development host:

```sh
cd crates/diary-storage-backoff
~/bin/devrun python3.12 tests/generate_fixtures.py --source /path/to/noevia-services/diary/agent/storage_backoff.py
```
