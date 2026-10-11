# Private proof checkpoint — transport remains absent

Base c665b36106505419d80e01a106b0d402f5d157e2. Private semantic commit
62ee0f594754950e081ca079ed2db710a0e8af88 precedes this repair. No shared file is
owned by this checkpoint. This report is not a complete allocation proof or
publication clearance; unresolved bounds below prevent adapter implementation.

## Proven private controls

All actual-client Git and Node invocations use the immutable M6 process seam
`tools/qualification/process.rs`, blob 7b7d0f5c80f3574a157d10635188675d359c6c2f.
Input/stdout limits are 8 MiB, stderr 64 KiB; fair nonblocking 4096-byte steps,
deadline, retained leader and owned group cleanup precede wait. No pipe worker
joins exist. Kernel scheduling/reaping is not claimed to have a hard real-time
bound. Synthetic stall, both noisy streams, early stdin exit, retained leader,
inherited and escaped pipes, and serialization refusal passed once. Escaped
fixture descendants are explicitly cleaned by their owner.

Affected checks passed once: actual storage-client 33 cases with comparator
negative controls; four qualification checks (including 340 malformed/boundary
UTF-8 cases and named CAP_LAST_CHUNK); two legacy resource refusal comparisons;
six snapshot-field refusals in one focused private check. Snapshot refusals return
fixed small error JSON and skip transport. No full Node suite or live request ran.

LIST_OUTPUT_16M compares actual legacy acceptance of 25000 synthetic DAV entries
and response greater than 16 MiB with native fixed 502 refusal. Native computes
exact escaped output bytes before sorting/reserving. LIST_ENTRY_BYTES is 16 MiB;
preflight also caps aggregate DAV block plus target lengths at 16 MiB and XML
candidate counts (DAV 100000; S3 each kind 200000). These are additional resource
restrictions, not universal predecessor equivalence. SNAPSHOT_ENDPOINT_64K
compares actual admin policy acceptance against native refusal. Other field
message parity, especially existing region WASM normalization, remains pending.

## Pinned source evidence and unresolved physical bounds

Paths below are relative to registry root
`/Users/sebastiandalton/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f`.
The nested manifest/lock compile only dependencies, never open sockets.

* hyper-1.11.1/src/proto/h1/io.rs:23,195–243: default logical threshold
  417792; reserve exposes full BytesMut spare capacity. It is not a physical
  allocation cap. Proposed configured 65536 threshold plus underlying read slice
  at most 4096 yields a candidate live incomplete-header bound 69631, subject to
  complete source tracing. bytes-1.12.1/src/bytes_mut.rs:610–790 reserve, doubling
  and shared offsets still require retained backing-allocation accounting.
* hyper-1.11.1/src/proto/h1/role.rs:1026–1150 bounds parser slot count and freezes
  backing bytes into header values. Proposed maximum 100 headers. Body frames
  are sliced by MemRead at io.rs:354–360, not inherently fixed 8 KiB frames.
  decode.rs:17–25 limits whole-body chunk extensions and trailers each to 16384;
  trailer append/check and HeaderMap allocation require final tracing.
* rustls-0.23.45/src/msgs/deframer/buffers.rs:218 onward bounds buffer length by
  handshake 65535 or wire-record 18437, and reads exact live slices in 4096 steps.
  Vec capacity growth, extend callers and retained handshake spans remain held.
  common_state.rs:86–88,1064–1065 sets plaintext 16384 and TLS send queue 65536,
  but vecbuf.rs append does not itself enforce these limits: packet overshoot and
  handshake writes must be proved. No final aggregate TLS allocation is claimed.
* rustls-webpki-0.103.15/src/verify_cert.rs:290–345 budgets 100 signature checks,
  200000 chain calls, 250000 name constraints; line847 bounds subordinate CA
  count at six. These are finite counters, not a wall-clock or crypto heap proof.
* brotli-decompressor-5.0.3/src/state.rs:387–418 distinguishes ordinary large
  window constructors from new_strict; decode.rs:151–195 rejects the large-window
  marker in strict mode. BR_LARGE_WINDOW_REFUSAL passed with fixed [17,30,0,0],
  no decoded output and zero ring size. decode.rs:1810–1860 bounds standard ring
  by 2^24 plus dictionary slack. Candidate Huffman groups: three times 256 times
  1080 codes, plus tables/context maps. Simultaneous lifetime and every allocation
  remain unclosed; no aggregate codec cap is asserted. Standard infallible
  allocation has an explicit OOM termination caveat; no unsafe custom budget,
  empty allocation cell or panic recovery assumption is used.
* flate2-1.1.10/src/gz/mod.rs:21–23,165–185,263–283 limits extra/name/comment
  individually to 65535. Complete miniz working memory, ordinary codec parity and
  per-call work/deadline tracing remain pending.

Proposed encoded input limit 64 MiB, decoded body cap plus one-byte lookahead,
fixed output slice 4096 and deadline checks after bounded decoder steps require
named extra refusal fixtures and proof of bounded work within each step. They are
not implemented. Existing 31/41/610-second engine budgets do not prove a transport
deadline. Standard allocator capacity/overhead is not exact RSS or recoverable OOM.

Existing parsers can transiently materialize complete pages before 10000-entry
truncation. Preflight pointers, records, duplicate fields, URL temporaries,
Unicode lowercase expansion and cached sort keys still need a quantified
simultaneous bound. Output 16 MiB and aggregate owned fields 16 MiB checks alone
do not prove the complete parser/sort heap bound. This explicitly holds transport
and integration rather than silently changing predecessor whole-page ordering.

## DNS evidence and narrow helper proposal (approval pending)

Exact source: hyper-util-0.1.21/src/client/legacy/connect/dns.rs:108–125;
call at117, spawn_blocking at118, OS ToSocketAddrs at119–121, retained JoinHandle
at124. Dropping the future is not an owned resolver cleanup proof. This is a
prospective dependency gap, not evidence of a deployed incident.

Propose a fixed Rust helper process for one URL-canonical ASCII hostname at most
253 bytes and a u16 port, bounded request 512 bytes. Literal IPs bypass DNS.
No URL, credential, HTTP header or secret enters the helper. Preserve OS resolver
order and approved LAN access; do not introduce a public-IP policy. Bound results
to 32 addresses and 4096 response bytes, refuse overflow rather than truncate;
stderr at most4096, deadline min(4 seconds, remaining attempt). TLS SNI remains
the original host. Extra hostname/result restrictions need named qualifications.

Use the proven process lifecycle (nonblocking fair IO, retained leader, owned
group kill, pipe close, reap), with an async event-driven wrapper rather than an
unmanaged spawn_blocking resolver. Admission stays held through helper cleanup
including client cancellation; at most four helpers, no queue. Production Linux
helper should set reviewed resource limits before OS resolution (candidate
address-space128 MiB and CPU2 seconds), avoiding unbounded libc result allocation.
Those platform/resource-limit semantics require actual Linux CI proof; macOS
memory guarantees are not assumed. Clear unrelated environment; explicitly
qualify LOCALDOMAIN/RES_OPTIONS differences if clearing them. Helpers may abort
on OOM; no infallible recovery claim.

Private controls to implement after this narrow direction is accepted: sanitized
host/port refusal; exact IPv4/IPv6 ordered decoding; 33-address and4097-byte refusal;
stall/noisy/early exit; inherited pipes and retained leader; client cancellation
cleanup; no credential/env leakage; Linux resource-limit refusal. Synthetic helper
modes only, no live DNS queries. No production adapter is authorized by this
proposal. Root controls labeling and specific shared integration handoff.
