# Native embedding ownership switch

Rust packet: concrete adapter/lifecycle candidate. A separate Core/Top packet
implements the counterpart; this branch edits neither Core nor deployment code.
Core file references below identify b91b3f03, the behavior being ported.

## Native side after independent-review remediation

`noevia-server --features` advertises `rust-embedding-background-v1`. Migration
construction is **independent of MODEL_LOADER_URL**; only the ten-second service
refresh needs that setting. Missing prerequisites, damaged configuration, unknown
seed profiles and a draining owner's lock never exit the front. Invalid saved
configuration is re-read, and lock contenders retry every two seconds without
writing the current owner's snapshot. Stop is permanent only for shutdown.

A lifetime flock on `embedding-worker.lock` excludes duplicate native owners.
Authenticated current-start Core proof gates all embedding work. A failed probe
suspends a pass; the next paired probe resumes and kicks it. Snapshots expose
`ownershipUnconfirmedSince` (epoch milliseconds or null), `ownershipStatus`,
`configValid` and `configInvalid`. Unpaired ownership is reported even while the
front continues serving sign-in/chat. Old Core returns no proof and keeps its
existing Node workers. No uncoordinated Node fallback is started by Rust.

The owner refreshes every ten seconds when model-loader is configured, kicks
saved/seeded next after thirty seconds, observes RAG DB/WAL changes every two
seconds, and accepts the unchanged start-bound explicit request signal. Shutdown
cancels admission, HTTP and pacing waits and joins the owner. The duty clock sums
embedding work after lease grants; admission queue waits never inflate its pause.
`Ask::Embedding` is paired-only: old Core would misinterpret it as text-engine work.

HTTP uses reqwest/engine-admission with no proxy or redirects, private DNS answers
pinned to sockets, bounded DNS/request/body sizes, sensitive headers, zeroized
retained secrets and fixed diagnostic labels. Explicit embedding endpoints receive
neither loader nor inference secrets. Fresh proof is checked before publication,
between provider fallback singles and again for each per-row retry. A proof older
than 250ms after fence acquisition or before commit suspends/rolls back publication;
networking is never performed while the account/outbox write fences are held.

Index coverage matches Node: every existing non-hidden .db in shared RAG_DIR
(default data/rag), and each live account's rag directory, including catalogue-free
or orphan indexes. No projects.json membership/readability requirement remains.
Tenant liveness and project tombstones fence access; index files are never created.

The stored format remains Node's `chunks`, `vec_meta`, legacy `vec_items`,
`vec_<sha256(profile NUL model)[:16]>` and `vecmap_<same id>`, with little-endian
f32 blobs. Existing orphan slot tables are reused with IF NOT EXISTS. Integer-valued
JSON indexes such as 1.0 sort as Node does. Older chunk columns are projected as
NULL during reads; only publication applies Node's four additive columns and
legacy metadata adoption.

Selection and remaining/coverage run through read-only/deferred RAG connections,
with non-locking server-store account and outbox tombstone checks before/after.
Only publication takes cowork.db's IMMEDIATE account-deletion fence. It then holds
config/project flocks, an existing outbox IMMEDIATE transaction and the RAG
transaction, rechecking config version, chunk hash, width, cancellation and proof
freshness. Slot cleanup does not take the global account write fence. No HTTP
is performed under these locks. Jobs, generations and attempts are never claimed,
recovered, advanced or acknowledged by this worker.

## Required paired Core diff (not applied)

1. Add an unconditional compiled ownership marker, analogous to the existing
   `RUST_*_OWNED` modules, for `rust-embedding-background-v1`. The release supervisor
   must require this exact capability before starting this Core. Do not introduce
   an operator feature flag or infer ownership from a stale state file. Migration
   ownership must remain native even with MODEL_LOADER_URL absent.
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
   nextHeld, nextServed, serviceAt, configValid/configInvalid and ownership state, but are never used as ownership authority.

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

Loss of Core/proof suspends native work and records the unconfirmed-since time;
paired proof resumes it. It never starts a Node fallback. This intentionally chooses no-run over uncoordinated
double-run during an outage. Readiness/health must expose unconfirmed ownership or
stale native status, so this cannot be mistaken for a healthy release. This packet
does not change existing public readiness; adding the paired health gate is part
of the Core/supervisor release diff and qualification.

## Reviewed dependency boundary

The only new unsafe boundary is the one-file crate
`crates/sqlite-vec-loader/src/lib.rs` (re-exported at the old Rust API path): one statically linked
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

The CI guard now budgets blocks, contracts, raw unsafe tokens, lint escapes and
ABI attributes. Workspace `unsafe_code = "forbid"` is restored; the loader follows
the existing mem-return isolated-crate pattern, with an allow on its one inline
FFI module only. Explicit existing/new path budgets are:

| Path | Blocks | Contracts | Lint escapes | ABI attributes |
|---|---:|---:|---:|---:|
| `crates/mem-return/src/lib.rs` | 1 | 0 | 1 | 0 |
| `crates/gguf-memtest/tests/hostile.rs` | 2 | 3 | 1 | 0 |
| `crates/sqlite-vec-loader/src/lib.rs` | 2 | 0 | 1 | 0 |
| `bins/dav-parse-wasm/src/lib.rs` | 0 | 0 | 49 | 50 |
| `bins/sandbox-bridge-wasm/src/lib.rs` | 0 | 0 | 10 | 10 |

Existing allocator/malloc_trim/export code is unchanged. Every allowed unsafe
block requires an adjacent SAFETY comment. Other files cannot add unsafe contracts,
lint escapes or ABI attributes. Literal Rust includes/path references must resolve
inside the scanned tree; non-Rust/unknown includes are rejected. The sole generated
routes include is explicitly recognized, and remains protected by workspace forbid.

The fixture qualification uses Core's unmodified vector-slots.cjs and actual
rag.cjs embedRowsInto function with synthetic embeddings. It proves Node KNN over
Rust-written slots, Python sqlite3 vec0 ABI compatibility, concurrent Node row/vector
replacement versus native publication, and a real owned-process SIGKILL after
embedding and before publication, followed by recovery. It does not claim the full
Diary application or whole indexProjectFile/outbox/browser workflow was retired.

The release kit/supervisor **must** enforce and test front stop/join before a Core
rollback, coherent source tuples and a new start ID. Fresh-proof checks only narrow
an overlap window; they cannot make an uncoordinated legacy-Core restart safe.
This requirement was forwarded to the separately owned Core/Top packet. Root must
verify its release-kit evidence before promotion. External config repair also
requires Core's cached damaged/fallback configuration to refresh or stay explicitly
refused; native repair recovery must not silently diverge from chat's active target.
