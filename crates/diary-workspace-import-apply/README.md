# Managed Diary workspace import APPLY source half (#1071)

This opt-in library/CLI owns the transactional application of a verified portable
workspace archive to an **existing** managed SQLite corpus. It is unwired: no
services adapter, selector, CI activation, image/source pin or deployment changes.

The caller must derive the fixed database path and tenant from its managed backend,
never from an HTTP parameter. Python preview remains separate. Its SQL `db()`
context must have ended before spawn; `ManagedCorpusBackend.close()` is a no-op
and is not evidence of quiescence. Keep the store write, migration and journal
exclusion through child exit/reap. Real SQLite locking handles other connections.
Do not run the dirty-index journal transfer until the child is reaped.

## apply-v1 IPC

Invoke the fixed executable with exactly `apply-v1`. Stdin is:

1. ASCII `noevia-workspace-import-apply-v1` followed by NUL.
2. u32 little-endian metadata length, at most 1 MiB.
3. Strict UTF-8 JSON, all fields required and unknown/duplicate fields rejected:
   `{"version":1,"dbPath":"/managed/managed-diary.db","expectedTenant":"00000000-0000-0000-0000-000000000001","destination":"Imports/Copy","reviewedFingerprint":"<64 lowercase hex>","debounce":3.0}`
4. u64 little-endian original ZIP length, at most 272 MiB.
5. Exact original ZIP bytes then EOF, no trailing bytes.

The destination is already-normalized `Imports/` plus a trimmed 1–100 character
name, without slash, backslash, leading dot, NUL or ASCII controls. Unicode names
and the existing case-sensitive safe-key behavior are preserved. No extra limit
is imposed on destination-prefixed imported paths or derived directory counts.
Debounce must be finite; the managed backend's normal value is 3 seconds.
The archive is verified internally with the merged workspace-restore-verify
`verify_archive` API, preserving its entry/source/manifest limits. Python-decoded
files, caller-provided hashes or reserialized ZIPs are never trusted.
The reviewed fingerprint must equal SHA256(destination UTF-8 || NUL || original ZIP),
and is checked before opening the database.

Success is exit 0 and exactly one UTF-8 JSON line:
`{"version":1,"destination":"Imports/Copy","fileCount":2,"alreadyApplied":false}\n`.
It is emitted only after commit, including an idempotent receipt result.
Validation refusal is exit 2 with `reject: CODE\n`; unavailable is exit 3 with
`unavailable: CODE\n`. Stderr is empty. Codes never contain paths or source bytes.
Archive refusal codes are inherited from the verifier; additional codes are
`frame`, `destination`, `fingerprint`, `conflict`, `database`, `schema`,
`tenant`, `inactive`, `clock`, `generation`, `response`.
Invalid arguments use `reject: frame`. SQLite/I/O errors use static codes.
Output failure may leave incomplete/empty stdout and exits 3.

The future host adapter must cap stdout at 4096 bytes, validate the exact versioned
reply plus EOF/exit status, close stdin, enforce a 120-second total deadline and
reap the owned child on any fault. **A nonzero exit, timeout or lost reply does not
prove rollback**: the child may have committed before stdout failed. Treat the
outcome as uncertain and retry the identical archive/destination/fingerprint.
Never fall back to a second Python mutation or mint a fresh receipt.

## Database transaction and limits

SQLite opens with READ_WRITE and no CREATE/URI flags, 30-second busy timeout and
synchronous FULL. There is no create/migrate/repair/ATTACH/extension/user_version
operation. Required tables, column types/nullability/keys and BINARY primary-key
collations are checked inside BEGIN IMMEDIATE, followed by exact tenant and
active=1 checks. No new marker-existence rule is introduced; the existing outer
backend constructor retains its marker policy.

The durable fingerprint receipt is checked before destination conflicts. Conflicts
use case-sensitive equality and slash boundaries, including ancestor files.
One transaction writes original BLOBs, verified SHA-256 versions, REAL timestamps,
directories, Markdown-only dirty-index outbox, TEXT generation/due/snapshot_time,
and receipt. Settings and unrelated rows are preserved. All precommit errors
roll back. Generations increment as arbitrary decimal text with Python-compatible
sign/whitespace/underscore/Unicode-decimal parsing, without an i64 clamp.
Finite float spelling uses ryu plus Python repr layout, qualified against actual
CPython 3.12 fixtures; due is independently computed in binary64 and formatted.

Opening an existing pathname does not prove its identity cannot change between
checks, nor does it prevent a concurrent rename/unlink after SQLite opens it.
The caller still owns filesystem exclusion. Missing files are not recreated;
pre-open wrong-tenant replacements are refused. No unsupported path-race proof
is claimed.

## Synthetic qualification

`tests/generate_fixtures.py` requires CPython 3.12 and reads the actual services
Python schema/exporter. It generates 8256 raw IEEE754 repr cases, 24 independent
now/due cases, the actual managed schema and a synthetic workspace ZIP.
Rust tests compare exact float strings and persisted TEXT/REAL values.
`tests/python_interop.py SERVICES_CHECKOUT IMPORT_BINARY` creates only temporary
managed databases and compares the actual Python APPLY against Rust, then checks
transaction fault rollback, identity/schema/no-create, receipt-before-conflict,
concurrent processes/readers/writers/deletion, lost stdout, case/Unicode, long
derived paths and the uncapped derived-directory count. Run every test/build via
the workspace's devrun wrapper with targeted cargo -j2.

Production app 400/409/503 mapping and closed-preview-transaction guard require the
later isolated services adapter; this source-only crate does not claim those gates.
