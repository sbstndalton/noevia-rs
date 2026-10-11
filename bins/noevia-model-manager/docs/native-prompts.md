# Native prompt library (#1356)

The optional `native-prompts` Cargo feature moves the complete prompt CRUD and once-only startup seed writer together. `MODEL_MANAGER_PROMPTS_IMPL` defaults to `python`. Selecting `rust` requires the compiled feature and `MODEL_MANAGER_IMPL=rust`. Invalid selection refuses before database access or child spawn. This document describes implementation and qualification boundaries; it does not authorize activation.

The native pairs are GET and POST `/api/v1/prompts`, and DELETE `/api/v1/prompts/{pid}` with one decoded segment. Unsupported methods on these known paths return 405. Unknown prompt subpaths return 404. Token authentication precedes path, method, body and store work. Native failures never fall back to Python. The public Node proxy retains session, CSRF, admin, no-store and normalization policy; owning these service endpoints does not move the public route.

Python exec removes inherited prompt confirmation. A native parent with the prompt selector off replaces it with `0`. Only successful native store/seed readiness permits the supervised loopback child to receive exact `NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED=1`. The services handoff must guard direct add/delete/seed and only the fixed seed kv write, suppress only startup seed, and authenticate before refusing the three HTTP pairs. Python retains other tables, kv keys and indirect prompt reads.

## Shared store and lifetime

The file remains `DATA_DIR/model_loader.db`, WAL enabled. The Rust worker verifies/creates only prompts and kv, preserving other tables, columns, keys and sqlite_sequence. Inserts/deletes commit before a separate list readback. Readback failure may therefore leave a committed mutation. Seed takes an initial exact-name snapshot, commits each missing literal independently, and commits `bench_prompts_seeded=1` independently last. A nonempty existing flag, including `0`, suppresses seed. Restart fills missing exact names after partial seed; case-different names remain distinct. Seed bodies use literal bytes without POST stripping.

One native parent per store acquires a fixed lifetime advisory writer lock. Supported deployment requires a trusted, stable, configured absolute DATA_DIR on the supported local Linux filesystem set (ext, XFS, Btrfs, tmpfs or overlay). No client chooses this root. Native mode rejects noncanonical case-folded DATA_DIR environment aliases; it passes canonical DATA_DIR explicitly to the child (#1362).

Symlink ancestry and final database/sidecar/lock files, unsupported file types, hardlinks and untrusted writable ancestry are refused. A root-owned sticky temporary ancestor is allowed only when the next directory is private, owned and mode 0700. Root/ancestor, main database and native writer-lock device/inode identities remain pinned for the store lifetime. SQLite-managed WAL/SHM companions may disappear after a reader checkpoints, or be recreated with different identities (#1376). Their identities are not permanently pinned: whenever present, they must remain regular files with one link, root/current ownership and no group/other write permission. These checks run before store operations and after readback or successful startup seed; a failure after a committed mutation does not undo it. SQLite canonicalizes its filename; these checks do **not** anchor all SQLite opens to a directory descriptor or provide an adversarial rename guarantee (#1357). The configured root and ancestors must remain trusted and stable throughout the deployment; companion lifecycle compatibility does not admit hostile same-UID path substitution within that deployment contract.

The parent retains its store Arc and writer lease until the child is confirmed reaped, including kill failure, wait error and timeout paths (#1369). Cancellation is observed before writes and on each SQLite VM operation, including short INSERT/DELETE resumed after busy waits (#1364). There is no cancellation rollback guarantee after a completed commit. Filesystem calls, SQLite I/O, OS process reap and worker Drop/join have no guaranteed hard elapsed bound; the cooperative deadlines do not establish OS syscall cancellation.

Raw nofollow verification of the main database finishes and closes its descriptor before SQLite opens (#1376). Closing another descriptor for the same database inode while SQLite is alive can cancel all of the process's POSIX SQLite locks; metadata checks during the store lifetime use pathname metadata without opening or closing raw database descriptors. The dedicated writer lock is a separate inode and remains held for the store lifetime. Qualification requires sequential and concurrent actual Python reader visibility plus a Linux cross-process main-database lock probe; Mac reader evidence alone does not certify Linux behavior.

## Fixed bounds and explicit restrictions

| Resource | Bound |
| --- | ---: |
| Encoded request / decoded JSON | 1,048,576 bytes each |
| URI / Content-Type | 8192 bytes each |
| Configured database path including filename | 4096 bytes |
| JSON depth / nodes | 32 / 65,536 |
| JSON string bytes / characters | 1,048,576 each |
| JSON number token | 4096 bytes |
| Normalized name / body | 4096 / 262,144 UTF-8 bytes |
| Full list rows | 4096 |
| Encoded response | 16,777,216 bytes |
| Outstanding worker jobs | 4 |
| Request / startup cooperative deadline | 5 seconds |
| SQLite busy wait | 250 milliseconds |
| SQLite VM operations per job | 1,000,000 |
| Schema SQL / columns | 8192 bytes / 256 |
| Decoded ID | 64 bytes, signed SQLite i64 |

Existing oversize rows, invalid SQLite dynamic types, unencodable text and nonfinite timestamps refuse with 503; data is not truncated, coerced, dropped or renumbered. Request overflow is 413. Text normalization overflow is 400. Saturation is 503. Malformed percent escapes, invalid path UTF-8 and encoded slashes in the established prompt namespace refuse with 400. Unrelated malformed paths retain their dispatcher owner and status; namespace ownership is established before decoding the entire path (#1372). Out-of-i64 IDs refuse with 422, where Python can reach a binding overflow and 500. These are bounded native restrictions.

Python truthiness, Unicode strip, integer and finite/nonfinite object-value formatting, nested repr quoting, insertion order and duplicate-key overwrite behavior are implemented in Rust. Ordinary Unicode strings remain accepted. CPython byte detection supports valid UTF-8, UTF-8 BOM, UTF-16 LE/BE and UTF-32 LE/BE with and without BOM. Absent Content-Type, application/json and application/*+json use JSON; other media types retain FastAPI dictionary validation for representable UTF-8 bytes.

Explicit raw-input restrictions are qualified against real FastAPI: lone-surrogate JSON escapes or encoded surrogate values are refused, even where Python nested repr can store escaped text; nonfinite non-object validation bodies are refused with 400 where Python response encoding reaches 500; non-JSON undecodable bytes are refused with 400 where Python error encoding reaches 500; encoded bodies within the byte cap whose decoded UTF-8 exceeds its cap are refused before normalization. Nonfinite values inside valid object fields remain accepted and use Python `nan`/`inf` formatting. These restrictions have named fixture evidence and assert unchanged store contents. They do not establish parity for all malformed byte streams or all framework extensions.

## Actual oracle and provenance

The immutable predecessor is services `2360cb51f6475122ba0aa3d32272849a63028d66`, with Git blobs:

| File | Blob |
| --- | --- |
| db.py | b6fdb19a39a76355aced71288b004e9982a86815 |
| api.py | c624647e3e23870f9abfad41d78797056ac21417 |
| main.py | b206d1919a9e8e6aed7247e45a60eec30bfe3e63 |
| config.py | 914c6c17275e8418568e662c4d570cdb19dc3c7e |

Rust verifies those blobs before executing a fixed Python invocation. Actual db functions and actual prompt API functions are registered on a real FastAPI TestClient. Rust owns synthetic requests, comparison, failures, clocks, mutation controls and provenance. The Python seam dispatches actual functions; it does not implement replacement prompt policy. The whole-app services guard/startup harness imports actual app.main and replaces only synthetic sampler/recovery/Docker seams.

The checked fixture records CPython 3.12.15, Unicode 15.0.0, FastAPI 0.115.6, Pydantic 2.14.0/core 2.50.0, Starlette 0.41.3 and SQLite 3.53.1 in the local resolved environment. The predecessor requirements pin FastAPI and Pydantic-settings but do not historically lock every transitive dependency or runtime image. `image_digest:null` is deliberate: local qualification is not a historical-image claim. CI regenerates real cases and retains its actual Python/SQLite versions; full cases, seed, framework, generator and private process-helper provenance must match. Real services image qualification follows the merged Rust pin.

Named selectors `request.name=json-diagnostic` and `raw-encoding` identify raw diagnostic/encoding checks. `request.op=restriction` contains explicit request, Python expected response and native refusal. Numeric case IDs are sequence indices, not stable names. Coercion cases, exact four literals/Unicode printability ranges, 216 ID grammar combinations, seed variants/index ties, indirect Python reads, schema token negatives (#1367), commit/readback failures, resource/cancellation controls and deliberate mutations are separately exercised.

The private oracle process helper derives from the reviewed M6 nonblocking pump and owns bounded fair IO, a process group, NOWAIT observation before signal/reap, caps and deadline. It is copied privately from reviewed M6 `2986dc92f586ea9246853c41fbd28bd150810d77`, helper blob `7b7d0f5c80f3574a157d10635188675d359c6c2f`. Descendants escaping the owned group are outside cleanup authority, and OS reap is not a hard deadline guarantee.

The public-front Rust harness binds actual core `010735c7d86d30269ab3f4a351b2135faea1c8e7` and runtime services `6c233f4a05730ed5dccd7997ae601e9999eaa6d7`, comparing relevant files to their immutable Git contents before launch. The launcher preserves all previous defaults and adds only fixed synthetic service URL/token values and an explicit loopback-port allowance. Named negative modes omit service configuration, use a wrong fixed token, or omit the port allowance. Synthetic native accounts use the actual public front and Node proxy; no model engine runs. The harness compares finite runtime timestamps structurally while the SQLite oracle checks exact fixed clock values. Local `--python-only` evidence is baseline-only; full native HTTP requires the Linux CI mode. The core's always-required WASM is independently checksum-pinned; a missing local copy was a harness prerequisite correction, not a production failure.

Public qualification also requires `NOEVIA_PROMPTS_DEPENDENCIES` naming a private owned external directory prepared by `npm ci` from byte-verified immutable core `server/package.json` and `server/package-lock.json`. CI creates that directory under `RUNNER_TEMP` and refuses a preexisting directory. Dependencies are installed outside the core/runtime checkouts, so the unchanged tracked/untracked source checks remain strict. Both qualification tools verify the copied manifests through capped nofollow/nonblocking regular-file descriptors and validate a single canonical external `node_modules` path. The literal actual server-spawn seam adds that verified path as `NODE_PATH`, including negative prompt modes; it preserves other options, arguments and the actual network guard. The artifact records manifest hashes and installation provenance separately from source provenance. It does not claim independent integrity verification of every installed dependency file.

The public harness caps synthetic HTTP responses at 128 KiB, each child output stream at 64 KiB and the final artifact at 1 MiB. It prebuilds the complete capped request and sends at most 4096 bytes per write, applying the remaining shared monotonic five-second send/read budget before every step (#1375). New indirect-reader and prompt lifecycle launches reuse the unchanged private process pump: stdin/stdout 16 MiB, stderr 64 KiB, twelve-second call-site deadline and group signaling before leader reap. Retained predecessor lifecycle launch sites are outside this new prompt qualification repair. OS calls and cleanup remain subject to the earlier stated kernel limits.

Mutation acceptance binds each mutant to explicit assertion source anchors, exact file/line, selected single test identity and an assertion failure payload (#1374). Compiler, dependency/setup/unwrap, cap/deadline/spawn, wrong assertion/test and unrelated panic failures do not count as killed mutations. Synthetic classifier negatives and actual Rust assertion-output controls qualify that distinction. Previous local killed-mutant history is retained separately; fresh final-source Linux CI must run the complete required matrix.

## Required final gates

Authoritative current-head Rust CI includes the additive native-prompts job and all previous jobs. Current services CI must preserve all existing jobs and execute guards/startup plus the real image probe with default Python, native prompt mode, native parent with prompts off, capability omission and invalid/mismatched selection. Current core/publicfront replay must prove session/admin/CSRF/token gating and actual native HTTP within the documented compatibility domain. Linux/image/publicfront gates, merged source pins and archives must be reported as pending until their actual results exist. No publication, activation or deployment is implied by local passing controls.
