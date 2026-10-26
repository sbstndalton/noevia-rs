# Diary workspace export authoring (#1071)

This library/CLI builds the real portable `noevia-workspace-v1` ZIP from an
in-memory snapshot. It never reads files, opens databases, connects to services,
verifies imports, or performs model calls. Python retains the SQLite read
transaction, tenant/auth checks, write/migration locks and legacy export path.

## IPC v1

Run `diary-workspace-export export` with exactly one stdin frame then EOF:

1. ASCII magic `NOEVIA-WORKSPACE-EXPORT-1\n`.
2. Four-byte unsigned big-endian JSON header length, at most 16 MiB.
3. Exactly that many bytes of UTF-8 JSON:
   `{"settings":<JSON value>,"directories":["empty"],"files":[{"path":"a.md","bytes":3}]}`
4. Raw file bytes concatenated in header order. No delimiter or trailing bytes.

All header fields are required and unknown fields refused. File lengths are
nonnegative u64 integers. Settings retain their JSON value, including integers
larger than u64 (preserved as raw validated JSON, without enabling global arbitrary-precision parsing); non-JSON NaN/Infinity and serde_json's
excessive nesting are invalid frames. Header JSON parsing has the standard
serde_json recursion limit. The host must serialize interoperable JSON.
Paths follow Diary safe_key: relative slash-separated nonempty segments, no
dot/dot-dot segments, backslash or NUL. A path's UTF-8 encoding must fit 65524
bytes to allow ZIP names. Duplicate file declarations are refused; duplicate
explicit directories are normalized like Python.

At most 5000 files and 5000 explicit/inferred directories, 64 MiB/file,
256 MiB total source, 8 MiB serialized manifest and 272 MiB complete ZIP.
All declarations are validated before source allocation. Source bytes and
settings are never coerced. Parents and explicit empty directories are included.
Files/directories are lexically sorted, SHA-256 digests lowercase, file totals
are exact. Members are ZIP_STORED with UTF-8 names and fixed 1980-01-01 timestamps.
Semantic parity with Python is required; ZIP/JSON byte identity is not.

Success: exit 0, empty stderr. Stdout is exactly:
ASCII `NOEVIA-WORKSPACE-EXPORT-OK-1\n` (29 bytes), u64 big-endian ZIP length,
raw 32-byte SHA-256 of the ZIP, exact ZIP bytes, ASCII
`NOEVIA-WORKSPACE-EXPORT-END-1\n` (30 bytes), then EOF.
The fixed transport overhead is 99 bytes. The host checks length, digest,
end marker, EOF and exit status before returning downloadable bytes.
Refusal: exit 2, stdout exactly `reject: CODE\n`, empty stderr.
Codes: `frame`, `counts`, `path`, `source_limits`, `directories`,
`overlap`, `manifest`, `archive`. No path/source text appears in errors.
Bad arguments: exit 1, empty stdout, fixed usage on stderr.
Output failure: exit 1, stdout potentially incomplete. Hosts must discard any
nonzero result except a recognized exit-2 refusal, and never consume partial ZIPs.

The host must close stdin and enforce a 120-second process deadline, cap stdout
at 272 MiB plus 100 bytes and stderr at a small fixed budget, and terminate the process
on overflow/timeout. The CLI intentionally waits for EOF and has no clock or
background threads. Assembly completes before the first stdout write; peak
memory includes source, manifest and ZIP (bounded by input/format limits).

The Rust verifier is a test-only interoperability oracle, not an implementation
dependency. `tests/python_interop.py` compares actual Python exports from a
provided services checkout with this binary using synthetic temporary databases.
