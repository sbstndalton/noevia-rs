# Diary immutable backup-v1 format foundation

Pure, source-only library for `noevia-diary-backup-v1`. It is **unwired**. It does
not replace the managed backup worker or the restore operator and does not open
files, SQLite, WebDAV, network clients, clocks or subprocesses. It is separate
from the portable `noevia-workspace-v1` ZIP and from whole-application backup.

Reference: noevia-services `6c1eb5891c75b6fb9f04c4484ba39da80a5b4b9e`,
`diary/agent/managed_storage.py:283,291–319` and
`diary/agent/backup_restore.py:16–65`; migration tracking noevia#1071.

## API and authority

- `plan(PlanInput, Limits)` sorts snapshot paths/directories as the Python SQL
  queries do, builds immutable object keys, emits canonical manifest bytes and
  hashes **those exact bytes** for the manifest key. It validates source paths,
  checksums and namespace overlap; it does not check source payload bytes.
- Objects remain `<corpusRoot>/noevia-backups/<tenant>/objects/<sha256>/<basename>`;
  manifest keys remain `<base>/manifests/<sha256(exact emitted bytes)>.json`.
  Empty roots and leading/trailing root slashes follow the Python writer.
- `Manifest::canonical_bytes` is the low-level serializer, not a replacement for
  `plan` validation. `PythonJson` represents arbitrary decimal integers and raw
  IEEE754 floats without changing serde_json features workspace-wide.
- `read(bytes, immutable_stem, policy, limits)` checks the immutable filename
  against ORIGINAL supplied bytes, including any whitespace. It never hashes a
  reconstructed manifest. Unknown fields and absent generation/savedAt are
  tolerated because the existing restore reader ignores them. Duplicate JSON
  object keys retain the last value, as CPython does.
- `verify_payloads` optionally checks supplied ordered object byte slices against
  their manifest checksums and a cumulative byte budget. The library performs no
  retrieval, filesystem containment/symlink validation, writes or activation.

A plan is not a snapshot or permission to upload. Existing tenant authorization,
SQLite generation/version snapshot checks, transactional saves, debounce/retry,
locks, GET/create-only PUT/reread verification and manifest publication ordering
remain the application's responsibility. Never publish a manifest before all
its referenced payloads are verified. No fallback, overwrite, truncation or
remote latest-manifest selection is implemented here.

## Canonical Python bytes and float evidence

Serialization matches `json.dumps(manifest, sort_keys=True,
ensure_ascii=False).encode()` using `, ` / `: ` spacing, lexically sorted Unicode
scalar keys, UTF-8 strings, Python control-character escaping and unbounded
integer text (subject to caller budgets). Array order is preserved by the
serializer; the planner sorts the input snapshot lists.

The committed fixture generator uses actual CPython `json.dumps`, not an assumed
Rust-equivalent serializer. It writes raw IEEE754 bit cases and complete synthetic
manifest bytes, their SHA256 and immutable keys. Coverage includes deterministic
random float bit patterns, adjacent exponent-boundary values, subnormal/max
finite numbers, signed zero, nonfinite values, Unicode key ordering/control
characters, nested settings and integers beyond u64. The half-tie regression
`42ec8b01f8b6d744` requires `251066872870586.12` (Rust Display gives `.13`).

`python_float` uses ryu's shortest decimal digits and Python's fixed/scientific
notation cutoffs, explicit sign and minimum two exponent digits, `.0`, and signed
zero. Fixtures gate byte/hash parity; a mismatch blocks adoption. It preserves
float values, not the spelling of incoming JSON numeric tokens.

CPython's default JSON accepts/emits `NaN`, `Infinity`, `-Infinity`; these are
explicitly supported and tested rather than converted to null or finite values.
This is CPython-compatible JSON, not strict RFC JSON. Python `str(float)` uses
lowercase `nan`/`inf`, so the nonfinite `python_float` result is specifically the
JSON spelling, not a general Python str replacement.

Regenerate through the development wrapper:

```sh
devrun python3 crates/diary-backup-format/tests/generate_python_fixtures.py
devrun cargo test -p diary-backup-format --locked -j2
devrun cargo clippy -p diary-backup-format --all-targets --locked -j2 -- -D warnings
```

## Reader policy and compatibility scope

`LegacyRead` checks safe path components plus the contiguous components
`noevia-backups/<normalized declared tenant>/objects` anywhere in the object key,
with a nonempty safe suffix. Arbitrary safe prefix and suffix components remain
accepted. This matches the old substring condition AFTER its safe_key check; it
is not a proof of canonical writer layout or of the caller's expected tenant.

`StrictWriterLayout { expected_tenant }` is a separate, explicit opt-in. It checks
that expectation even for empty manifests and requires the exact canonical
five-component suffix, checksum directory and source basename. It is never a
silent replacement for LegacyRead or a production default.

The compatibility target is manifests the current Python writer can emit. This
is **not a complete replacement for every handcrafted input the Python operator
can parse**. Explicit reader-only exclusions:

- Escaped lone UTF-16 surrogates can survive Python json.loads in settings, but
  cannot be UTF-8 emitted by the writer's ensure_ascii=False + encode. They return
  typed `UnsupportedInput`; they are never replaced by another character. Valid
  escaped surrogate pairs and literal backslash-u text are supported.
- Canonical, uppercase, compact hex, ordinary braced/URN UUID spellings and
  nonstandard hyphen placement are accepted. Exotic Python int(hex) coercions
  such as embedded underscores are refused with `Tenant`.
- UTF-8 BOM-prefixed handcrafted manifests are not accepted. The writer emits
  UTF-8 without a BOM. Explicit caller limits can refuse otherwise readable data.

These gaps must be reviewed before any operator-reader activation. Path syntax
checks here do not replace the operator's resolved-root containment and
new-directory-only filesystem policy.

## Explicit budgets, no production cap change

There is no `Limits::default` and no application selector. Every caller supplies
manifest bytes, file/directory count, JSON depth/node/string budgets and cumulative
payload bytes. Overlimits return `Limit`, never a shortened plan. Tests exercise
each budget. In-memory typed trees remain caller-owned inputs; callers should
choose conservative depth limits.

Before a future switch, evaluate the old reader's existing 8 MiB manifest,
50,000-file and 1 GiB payload limits as possible compatibility budgets. A proposed
50,000-directory / 128-depth / 500,000-node budget would be NEW policy, requiring
explicit approval and synthetic boundary evidence. The managed writer currently
has no corresponding count/manifest caps; this library does not impose them on
production. Inventory compatibility and decide writer/refusal behavior before
wiring any quantitative limits into that worker.
