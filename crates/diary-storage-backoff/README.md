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
parsing covers common RFC 2822/850/asctime forms under UTC. Python's more
permissive `email.utils.parsedate_to_datetime`, local timezone interpretation
of naive dates, and unusual syntactically valid email dates are a documented
adapter gate: an explicit Rust selector should refuse unsupported forms or
add those cases before replacing the Python transport. Finite clock samples
within ±1e12 seconds are required; Python assumes real finite clocks.

Regenerate on the development host:

```sh
cd crates/diary-storage-backoff
~/bin/devrun python3.12 tests/generate_fixtures.py --source /path/to/noevia-services/diary/agent/storage_backoff.py
```
