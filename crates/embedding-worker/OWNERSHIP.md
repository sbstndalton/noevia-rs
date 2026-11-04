# Native embedding ownership switch

Rust packet: concrete adapter and lifecycle candidate; **not a deployed ownership
switch**. No embeddings Core worktree existed at intake, so Core remains unchanged
at b91b3f0324bfd8daea0e7c7b5f3a40ad4cde91fc. No worktree was created.

## Native side

`noevia-server --features` advertises `rust-embedding-background-v1`. Only a nonempty
MODEL_LOADER_URL constructs its background owner, at the end of startup. A lifetime
OS flock on `embedding-worker.lock` excludes a second native owner of the same
data directory. Before any service refresh or migration, the owner obtains the
Core proof described below. Old Core returns 404, so native stays idle and Node's
existing timers remain responsible. The owner refreshes every 10s, kicks saved next
after 30s, notices RAG DB/WAL changes every 2s, and accepts a start-bound explicit
kick request. It stops on shutdown, cancels its in-flight pass, then joins its task.
No target storage or credential is copied across tenants.

Native HTTP reuses reqwest and engine-admission. No proxy, no redirects, checked
private DNS answers pinned to the socket, bounded DNS/request/body sizes, sensitive
headers, zeroized retained secrets and fixed diagnostic labels. Explicit embedding
endpoints receive neither loader nor inference secrets. Model-loader GET is 6s,
served-model GET 10s/1MiB, embedding POST 60s/16MiB. A new maintenance-only embedding
ask is carried by the existing renewable inference lease client; Core must implement
the paired branch below. Empty/incomplete embedding batches fall back to singles;
UTF-16 request framing retains lone surrogates.

Native index format is Core's actual vec0 layout: `chunks`, `vec_meta`, legacy
`vec_items`, `vec_<16 hex sha256(profile NUL model)>`, and `vecmap_<same id>`.
Floats are little-endian f32 blobs. Legacy active nomic slots are adopted once;
small/direct and image chunks are excluded. Missing selection and publication are
existing-only, with only Node's four historical additive chunk columns
(version/direct/kind/meta), and no replacement index schema.
Cowork account/schema access uses server-store. Publication holds config flock,
a short cowork IMMEDIATE transaction, projects.json flock, existing outbox
IMMEDIATE transaction, then RAG IMMEDIATE transaction. It rechecks live user,
project, tombstone, config version and chunk hash/slot width. No HTTP is performed
under these locks. Missing legacy outboxes stay absent: the project lock fences
removal before tombstone creation. Jobs, attempts and generations are never
claimed, recovered, advanced or acknowledged by this worker.

## Required paired Core diff (not applied)

1. Add an unconditional compiled ownership marker, analogous to the existing
   `RUST_*_OWNED` modules, for `rust-embedding-background-v1`. The release supervisor
   must require this exact capability before starting this Core. Do not introduce
   an operator feature flag or infer ownership from a stale state file.
2. `server/index.cjs:250`: remove Core's immediate embeddingService.refresh and
   its 10000ms interval when paired ownership is active. Retain embeddingService
   construction/install: embeddingOff, deletion guards, model route service
   mutations and waitLoaded still depend on it.
3. `server/index.cjs:252-253`: replace the onIndexed callback's direct migration
   kick with a native kick signal; remove Core's saved-next 30000ms timer. Keep
   onIndexed notification. Do not stop `rag.init`, seeded-address guards, RAG
   indexing/search, chat retrieval, media indexing or `indexOutbox`'s 2s executor
   (`index.cjs:578-614,1320`). They have not been retired by this packet.
4. `server/embedding-migration.cjs:114-175`: remove background fillTarget/pass/
   running/again/retryTimer ownership in paired mode. Its public kick becomes a
   native signal writer. Keep setNext/clearNext/switchToNext/rollback/finish,
   coverage, parity and orphan cleanup; these public control APIs are not moved
   by this change. `stop` stops notifications, not an independent second pass.
   Replace status's background fields with the native snapshot, bound to start;
   keep config and coverage computed from the existing source of truth.
5. Native signal: atomically replace `UI_DATA_DIR/embedding-worker-request.json`
   with `{start:NOEVIA_START_ID,sequence:<monotonic nonzero integer>}` after each
   control operation requiring a kick and onIndexed while next exists. Do not
   write or acknowledge index-outbox jobs. Native additionally stamps DB/WAL
   changes so Node outbox completions trigger rechecks without an HTTP callback.
6. `server/embedding-service.cjs:40-62,86-100`: read the background service view
   from `embedding-worker-status.json.service`, validating protocol/current start
   and freshness. Keep serve/waitLoaded direct requests for control operations;
   their mutable view must merge with the native view using its serviceAt
   observation time, retaining the union of protected models until a post-mutation
   refresh confirms them. A stale in-flight GET must never clear protection for a
   model just added by serve. Signal a fresh native service observation after
   mutation; qualify concurrent PUT/GET/refresh and delete guards. Status
   missing/stale must remain unknown/stale rather than falsely enabled/off. The
   delete guard must fail closed until it has current model protection evidence.
7. `server/embedding-config.cjs` persistence and the configuration-changing/
   orphan-cleanup portions of embedding-migration must hold the same
   `embedding-config.json.lock` flock as native publication. Read/check/persist
   and cleanup in that hold; never hold it across model HTTP/parity checks. Repeat
   the expected-target check after acquiring it. This fences concurrent switch,
   rollback, abandonment and config persistence against a batch publication.
8. `server/inference-admission-lease.cjs:createAdmission`: recognize
   `{embedding:true,model}` ONLY from trustedFront. Take only
   `modelManager.enterInference`, not `acquireLease`, `admitEngineModel`, model
   loading, text-engine queue or memory preparation. Return the existing held
   release object/renewal lifetime and no engine headers, matching rag.cjs's
   `embedOnce` maintenance guard. Reject malformed mixed role/machine/embedding
   asks. Retain all existing chat/research lease behavior.
9. Add GET `/internal/embedding-worker-owner` before the public gate, protected by
   `trustedFront` (loopback plus x-noevia-store-token). Only the paired Core,
   after statically suppressing its background workers and confirming the front's
   required capability/start, responds 200 with:

   ```json
   {"protocol":"rust-embedding-worker-v1","start":"<current start>","nodeWorkersStopped":true}
   ```

   This route must be available before native-ready notification to avoid a
   readiness cycle; it proves static protocol ownership, not model readiness.
   An old Core has no such route and cannot authorize native work. The complete
   protocol also requires steps 6-8; do not emit this proof from a partially ported
   Core. Native status snapshots contain protocol, start, at, service, lastPass,
   nextHeld, nextServed, serviceAt and configValid, but are never used as ownership authority.

## Rollback and pairing matrix

| Core | Front | Expected behavior |
|---|---|---|
| Old | Old | Existing Node refresh/migration workers |
| Old | New | Core proof 404; native idle, existing Node workers continue |
| New | New | Node background workers statically absent; authenticated current-start proof enables one native owner |
| New | Old | Supervisor/Core capability guard refuses startup; never silently run with zero workers |

Rollback must stop/join the current front before releasing its native owner lock,
then restart the **coherent Core/front pair** with a new NOEVIA_START_ID. Never roll
back Core alone while a native batch is in flight: loss of the next ownership
proof prevents later passes, but cannot retract a request already admitted. Never
start an old Core concurrently with an already authorized new front. The lifetime
flock excludes duplicate native owners, not old Node workers that know no lock.
A paired supervisor must enforce the source-image tuple and quiescence across
restart/rollback; the handshake alone cannot enforce process orchestration.

Loss of Core/proof causes native to suspend new work; on a new paired Core it does
not start a Node fallback. This intentionally chooses no-run over uncoordinated
double-run during an outage. Readiness/health must expose unconfirmed ownership or
stale native status, so this cannot be mistaken for a healthy release. This packet
does not change existing public readiness; adding the paired health gate is part
of the Core/supervisor release diff and qualification.

## Reviewed dependency boundary

The only new unsafe module is `src/sqlite_vec_loader.rs`: one statically linked
sqlite3_vec_init -> sqlite3_auto_extension registration under OnceLock, with an
explicit SAFETY comment for each block. No runtime library path is accepted.
The Rust dependency is exactly `sqlite-vec =0.1.9`; each opened vector connection
requires `vec_version() == v0.1.9` before publication.

Version evidence supplied by root: read-only live observation at 08:1xZ on
2026-11-04 found Python Diary pip sqlite-vec 0.1.9 and SQL vec_version v0.1.9;
Node web sqlite-vec and its Linux package also 0.1.9. Diary requirements line 10
is **unpinned** (`sqlite-vec>=0.1.6`); root will file that drift risk separately.

## Remaining acceptance

The synthetic adapter tests do not establish a completed ownership switch. The
Core/supervisor diff above, exact-head independent review (especially loader FFI),
CI, real process startup/cutover/rollback qualification and full synthetic retrieval
acceptance remain. No live models, flags, production sign-in or deployment were
performed. Existing Node remains runtime owner until a coherent paired release.

The CI guard uses this explicit source-path allowlist and unsafe-block caps:

| Path | Max blocks | Change in this packet |
|---|---:|---|
| `crates/mem-return/src/lib.rs` | 1 | Untouched existing malloc_trim implementation |
| `crates/gguf-memtest/tests/hostile.rs` | 2 | Existing allocator unchanged; added contract SAFETY comments |
| `crates/embedding-worker/src/sqlite_vec_loader.rs` | 2 | New reviewed loader |

Every allowed block requires an adjacent contiguous comment containing SAFETY.
The guard parses all Rust sources under bins/crates/tools, visits unsafe blocks
and contracts, and also token-scans macro bodies; string/comment mentions do not
count as unsafe code. Any unsafe token outside the allowlist fails. Raising a
block cap or adding a path is an explicit reviewable guard edit. Workspace unsafe
lint changes from forbid to deny solely to permit the module-scoped loader allow;
no library-wide allow was added.
