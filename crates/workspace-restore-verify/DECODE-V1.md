# Workspace decode-v1 (private IPC)

This command supplies checked source bytes and settings so a future Diary adapter can stop parsing ZIP files in Python. It does not restore files, activate a corpus, switch a production selector, import into a database, or call any model/service. The existing `verify` digest/refusal and `self-test` contracts are unchanged.

## API and validation

`verify_archive(&[u8]) -> Result<VerifiedArchive, Reject>` uses the **same validation path** as `verify`. No archive object is returned until all entries, types, manifest records, overlaps, checksums, totals and stricter ZIP-container checks succeed. It exposes immutable `listing()`, `settings()`, `fingerprint()` and sorted `files()` records with their raw bytes. No caller needs another ZIP parser. Settings preserve the accepted JSON value (including null or non-object values); an absent key yields an empty object, matching the old caller default.

The digest-only `verify` path does not retain every file. The content path retains checked bytes once, never re-inflates a member or base64-encodes source data. Input, inflated data and parsed metadata are memory-resident: existing archive272MiB, source256MiB, per-file64MiB, expanded264MiB, manifest8MiB and5000file/5000directory limits remain. Validation of a refused archive may transiently retain up to the existing expanded-size cap before the final source-total refusal. Do not launch unrestricted concurrent decoders outside an application's resource budget.

## Command

`workspace-restore-verify decode-v1 < archive.zip`

One input archive on stdin. There are no path arguments, filesystem writes, network calls or environment-dependent archive decisions.

- Success: binary frame below, flushed, exit0.
- Archive refusal: only the existing text `reject: <code>\n`, exit1. No binary prefix or content is written.
- Bad arguments: empty stdout, exit2.
- Input/output failure (including a broken output pipe or final flush): exit3, with a generic stderr error. A failed output can contain a partial frame; it is **never success**.

Archive validation and bounded metadata serialization finish before the first binary output byte. This does not make a pipe transactional: the consumer must stage received content privately and discard it on any incomplete framing, timeout, signal or nonzero child exit. No database write, extraction, preview success, receipt or published file is allowed before the complete frame, exact EOF **and exit0**. Parent must drain stdout concurrently with feeding large stdin, enforce total timeout/output caps and reap its exact child. The old256-byte verdict collector is incompatible with decode-v1.

## Wire layout (all integer fields unsigned little endian)

1. Exact magic bytes `noevia-workspace-decode-v1\0`.
2. u32 metadata byte length, at most `MAX_DECODE_METADATA = 32 * 1024 * 1024`.
3. That many bytes of UTF-8 JSON. Object keys:
   - `version`: integer1;
   - `fingerprint`: lowercase hex SHA256 of the exact original archive;
   - `listingSha256`: the existing canonical listing digest;
   - `files`: array of `{path, bytes, sha256}`, sorted by path in the existing Unicode scalar order, unique safe relative paths;
   - `directories`: sorted unique safe relative paths, disjoint from files and their descendants as required by verifier;
   - `bytes`: total checked source bytes;
   - `settings`: the accepted JSON settings value, or empty object when absent.
4. For each file in metadata order: u64 raw payload length (must equal that record's `bytes`), then exactly that many **raw bytes**. This includes zero-length files. No base64, second ZIP, path or per-file JSON appears here.
5. Exact marker bytes `\0noevia-workspace-decode-v1-end\0`.
6. EOF with no extra bytes; exact child exit0.

Maximum frame size is magic length +4+32MiB+8*5000+256MiB+end-marker length. Metadata is separately capped before output; source bytes are never folded into JSON. JSON object-key order is not contractual. Unknown protocol versions are refused; a future incompatible format needs another command/magic version. The frame completion marker is structural, not authentication.

The future receiving adapter must cap every allocation before reading, enforce file/directory counts and total/per-file bytes, safe unique ordered names and nonoverlap, metadata type/version and lowercase digest shapes, record length equality, source checksums and canonical listing digest, expected original-archive fingerprint, marker, exact EOF and child exit. Refusal text must be parsed only with exit1 and a known stable code. Never guess success from a prefix, accepted files so far, an EOF alone, the final marker alone, or a zero exit alone. The current PR includes a **test-only** reference framing consumer and negative stream cases; it does not install the future services adapter.

## Compatibility and follow-up

Frozen differential ZIP fixtures and refusal classes are unchanged. Rust's intentionally stricter ZIP/JSON refusals remain. No file content/path is printed by legacy `verify` or error logs; decode-v1 stdout intentionally carries private content and must never be logged as diagnostic output.

A paired services adapter and native isolated operator restore are separate changes requiring their own review, exact binary/image pins, fail-closed absence handling, tenant/import receipt tests and deployment approval. Python still runs today; this API/protocol alone does not finish the Diary port or qualify a release.

