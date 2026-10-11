# Private storage reads — noevia#1368

This standalone crate is **not integrated or deployable**. Its parent workspace,
lock, server, routes, auth, configuration, store and build tools are unchanged.
Base: Rust `c665b36106505419d80e01a106b0d402f5d157e2`; actual client oracle:
core `010735c7d86d30269ab3f4a351b2135faea1c8e7`. Web predecessor:
`f7d21353dee9f11be663d6a9bf3258144f11c5d5`.

## Implemented privately

- Bounded, explicit-stack JSON scanner: replacement UTF-8 decoding, JSON grammar,
  1 MiB input, depth 128, UTF-16 strings/lone surrogates, last duplicate `path`,
  number overflow and JavaScript path coercion. Only the existing number formatter
  is reused from js-json; its parser/coercion/slicing are not used.
- Canonical decoded prefix cap, qualified independently of Node chunk layout;
  UTF-16 200000-unit previews and Node-style JSON escaping/property order.
- Exact configured-origin predicate, including explicitly approved member LAN
  origins and admin LAN access; URL userinfo is refused.
- GET/PROPFIND-only protocol engine, DAV ignoring corpusRoot, S3 joining it,
  path-style S3 URL/query construction, Basic/SigV4 headers, existing Rust listing
  parsers, stable lowercase UTF-16 ordering, finite 20-page/10000-entry traversal,
  duplicate preservation and truncation before sorting.
- Conservative typed pre-header retry (one 250ms delay), fixed public failures,
  attempt deadlines and 31/41/610-second overall budget passed to the adapter.
- Nonqueueing admission: four globally, one per validated owner; RAII release.

`Transport` is a **contract, not an enforcement implementation**. The test fake
uses fixture bytes and a fixture clock. No real TLS, DNS, socket, incoming body
timer or async cancellation exists. Arbitrary implementations can violate this
contract; no production adapter is exposed. `Connection` is an internal snapshot
shape, not an authentication capability. The authenticated store/key constructor
does not yet exist. The front must perform authentication/CSRF, acquire and retain
the admission permit, check endpoint policy, select the owner row, bind SecretKeys
AAD, release the read transaction before outbound work and wrap list JSON in the
actual route response. It must never obtain credentials from request JSON.

The separate [synchronous child lifecycle proof](proofs/transport/LIFECYCLE.md)
implements finite parent IPC and owned cleanup with Linux limit/NotDumpable setup.
It has no HTTP adapter. Portable synthetic fixture modes are explicitly private;
the actual Linux limit branch remains unexecuted on this Mac. Async supervisor,
parent parser/sort bounds and network semantics remain held.

Snapshot inputs are bounded before request construction at existing library
domains: endpoint/username/root/bucket64KiB, region1MiB, secret8MiB. The fixed502
refusal of oversized legacy configuration is **pending qualification/acceptance
before integration**, not an asserted predecessor-equivalent error. This is an
internal private input contract; no stored settings are modified.

## Evidence and limitations

Private checks are offline and synthetic. `tests/node_oracle.rs` uses a tiny fixed
fetch seam calling the actual storage-client exports and checks loaded collaborator
bytes against the exact core SHA. Its 33 cases compare response bytes and request
destinations/methods, with response/destination omission negative controls. This is
not the whole route/auth/store corpus, a mutation campaign or a network test.
`tests/qualification.rs` calls actual `http.cjs.readCappedText`: 340 malformed and
boundary UTF-8 cases compare to the native decoder. Named `CAP_LAST_CHUNK` records
the accepted Node last-chunk overflow difference, separate from zero-difference
cases. Preview split-surrogate, JSON depth, paging, retry exclusions and admission
release have focused Rust coverage. STORAGE_PATH_IMPL=wasm and per-user DB/key
state proofs remain pending. Unicode lowercasing/version edge coverage and every
number coercion case are not claimed complete.

Accepted qualifications retained: readonly native storage omits Node plaintext/v1
lazy re-encryption (no global state equivalence); public WebPKI/no system proxies;
conservative retry/messages; canonical cap+lookahead; body10s/depth128 and
admission4+1; finite overall deadlines. Actual settings/save/explicit rotation stay
Node-owned. No storage credential writer or native remote writer is introduced.

## Transport allocation hold and concrete direction

The nested dependency-only proof pins and builds hyper-rustls/rustls and codecs;
official registry dependencies were fetched with the bounded Rust process seam.
No transport adapter exists. See [current complete proof status](proofs/transport/ALLOCATION.md)
for tested harness repairs (#1371), resource qualifications, exact resolver source
and the proposed owned DNS helper. Physical allocation and work bounds remain held.

Direct cached source evidence:

- hyper1.11.1 `src/proto/h1/io.rs` 23: logical default maximum 417792.
  195–212 checks it on incomplete headers. 223–243 reserves `next` but exposes the
  **whole spare BytesMut capacity** to ReadBuf; physical capacity can exceed the
  requested reserve. Thus 417792 is not proven as exact physical read/allocation.
- hyper1.11.1 `src/proto/h1/role.rs` 1026–1074 bounds parser slots by the default
  100 headers; 1087–1150 freezes the header buffer, reserves HeaderMap slots and
  slices header values from it. This supports finite *count*, not an independent
  total physical allocation proof. `io.rs`354–360 bounds each returned body slice
  by requested length, while the underlying allocation is still retained.
- bytes1.12.1 `src/bytes_mut.rs`610–680 delegates vector reserve; 697–790 includes
  capacity doubling and shared-buffer offsets. Source proof must account for
  backing allocations retained by slices and Vec's allocator behavior, not just
  the live slice length. No exact RSS/allocator-overhead guarantee is made.
- flate2 1.1.10 `src/gz/mod.rs`21–23 and263–283 bounds filename/comment at65535;
  165–185 bounds extra by u16. This is not yet the async decoder/frame proof.
- brotli-decompressor5.0.3 `src/state.rs`387–418: `new` and dictionary constructor
  enable large windows; **new_strict** disables them. `src/decode.rs`151–195
  rejects nonstandard large-window marker when strict; 1810–1860 allocates the
  ring from window bits plus slack. Ordinary constructor is not a 16MiB proof.

Approved direction: private hyper-rustls HTTP1 adapter with per-read fixed slices,
explicit max buffer and max100 parsed headers; single owned response frame at a
time; independently bounded encoded input; fixed decoded output buffers; strict
standard Brotli and fixed gzip/deflate streaming buffers. Standard allocation may
terminate on OOM; no custom budget allocator or infallible recovery is assumed.
`BR_LARGE_WINDOW_REFUSAL` must be a separate named exception with a negative
synthetic fixture, preserving ordinary br/gzip/deflate behavior where proven.
Reject unknown/chained encodings unless their bounded decoding semantics are
qualified. No allocation budget implementation may return empty memory into a
dependency that assumes successful allocation without first proving that failure
path safe. `try_reserve` is not an infallible recovery or physical heap guarantee.

Pending before transport: exact TLS/codec lock and offline/CI feature build;
bounded IO/allocator-growth and retained-frame proof; every codec allocation and
failure path; encoded/decoded/work/deadline bounds; ordinary codec oracle corpus;
large-window refusal negative fixture; cancellation/drop/header retry taxonomy.
Transport remains absent until these close.

Shared integration additionally waits the actual M6 merge/handoff and clean
rebase. Then add required read columns, owner-bound existing SecretKeys adapter,
default-off feature and dependency config, Native exact route method claims,
supervisor capability gate and exact-method Node refusal. Full actual route replay,
property mutants, different Sol6.1HIGH review and authoritative current CI are
required before any push/PR/merge. Core54 stays parked and never merges.

Excluded: config/test/MKCOL/OAuth/Drive, project uploads/persistence/edit targets,
export/import/extraction, models/chat/Coding, accounts/credentials mutations,
remote writes, live access, release pins and ledger ownership.
