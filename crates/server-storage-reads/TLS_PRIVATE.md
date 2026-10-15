# Literal-IP TLS private checkpoint

This is an inactive private crate, not a route adapter or publication approval.
The predecessor is dc75c609eae5f72ac8c37bbbfbffc925923d81c6 on Rust base
c665b36106505419d80e01a106b0d402f5d157e2. The approved design is
held privately outside the repository.

## Production boundary

`ProductionTls` has a private config field and only a fixed public-root constructor.
It uses cached rustls 0.23.45/ring, safe protocol versions, immutable webpki-roots
1.0.9, no client auth, disabled resumption and early data, HTTP/1.1 ALPN.
The Linux-required child completes existing limits/dumpability setup, current-thread
runtime, and this config before READY and request input. No trust flags, arbitrary
public config constructor, environment trust lookup, certificate file input, or
fixture feature exist in that executable. Hostnames fail with the typed DNS phase
before resolution/connect; this phase implements numeric IPs only.

Certificate identity comes from the original URI authority, independently of an
explicit HTTP Host header. IP identity has no DNS SNI. A separate portable fixture
artifact includes the actual transport source, accepts only its closed synthetic
modes, and restricts all connections to loopback. Its localhost mapping is fixed
and does not resolve DNS. Its clock shifted by two days is only an expiry witness.
It is not a production bypass or Node expiry parity claim.

One generic HTTP exchange handles plain TCP and TLS. The original Request, NET
envelope, reply codec, admission and cleanup sources remain unchanged. The old
plain execute wrapper remains available. No automatic retry, proxy, connection
pool, cookie store, redirect, or DNS task was added. TLS handshake errors become
fixed nonretryable TLS failures; timeout retains the same absolute attempt deadline.
Hyper's spawned connection task has an abort-on-drop guard and the current-thread
runtime is dropped before child exit. There is no blocking resolver to outlive it.

## Resource qualifications

Each underlying read/write call uses at most 4096 bytes. An independent encrypted
64 MiB shared read/write counter sits below TLS; the existing plaintext 64 MiB
counter remains above TLS. The encrypted cap is an additional named restriction,
not legacy parity. A read uses one-byte lookahead and refuses before copying an
over-cap byte. The exact counter boundary was tested with a fixed virtual source,
not with a 64 MiB adversarial TLS session. TLS `set_buffer_limit(65536)` bounds
rustls's two logical outgoing buffers; it does not establish a physical allocation,
incoming-buffer, RSS, or process-wide 64 KiB bound. The approved Linux address-space
and CPU limits remain required, with standard allocation/OS termination caveats.
They were not executed on this macOS host.

Existing postparse 64 KiB logical header/trailer checks, dependency parser limits,
body cap + lookahead, typed metadata validation, and parent EOF/exit-zero/confirmed
reap conditions remain. Admission is retained through cleanup/reap. Kernel reap is
not claimed hard real time. Async supervisor ownership and parent parsing/sort heap
are still separate holds.

## Synthetic evidence and provenance

The generator invokes fixed /usr/bin/openssl commands through the existing bounded
Rust process seam: empty environment, no shell, owned 0700 directory, precreated
0600 files, 30 seconds per command within 120 seconds total, capped output, group
cleanup/reap. Files are opened once NOFOLLOW/NONBLOCK, regular/nlink-one/0600 checked,
and read in 4096-byte chunks with a 64 KiB cap plus lookahead before extension.
Only synthetic certs/keys are generated; keys are not printed. Local filesystem
operations and kernel reap do not acquire a hard real-time guarantee.

Installed LibreSSL 3.3.6 SHA256 is
(private evidence held outside the repository).
The initial shared prompt=no request config gave CA and leaf identical subjects
despite distinct `-subj` arguments. Actual Node X509Certificate observations showed
leafSelfIssued=true/issuerMatchesCA=true and DEPTH_ZERO_SELF_SIGNED_CERT. Separate
fixed CA/leaf request configs corrected the fixture. This is observed installed
binary behavior, not a claim of reviewed LibreSSL implementation-source precedence.
All failed logs are retained. Only affected new TLS controls were rerun.

Final affected controls cover TLS1.2/1.3, original localhost SNI distinct from Host,
numeric IP no-SNI, cap/cap+1 for both reply caps, public-root refusal, wrong SAN,
fixture-clock expiry, handshake timeout, truncated body, and body cancellation with
held admission. Production hostname refusal and fixed counter controls are separate.
Node22.23.3 builtin fetch reports process.versions.undici=6.28.1: trusted owned GET
(with/without synthetic Host override) and PROPFIND succeed; untrusted leaf refuses
before HTTP input. This is a narrow TLS observation, not route/output/Host-byte or
npm Undici parity. Executable SHA256 is
(private evidence held outside the repository).

Two actual source mutations were executed once (wrong original identity and wrong
handshake phase), with exact sole-test/status101/panic-location/unique-policy-ID/
complete-libtest-stdout/terminal-Cargo-stderr classification and forged/tail negatives.
Captures retain the pre-correction synthetic PKI and pre-lint/cfg-test snapshot.
Final policy baselines passed with corrected PKI. Production mutation anchors are
unchanged; this is source-lineage qualification, not fresh final-tree mutation
execution. Old HTTP and six-owner mutant groups were not rerun.

## Remaining holds

Linux limits/baseline probe; IPv6 TLS execution; full encrypted-session boundary and
handshake-cancellation lab; DNS/OS address order/resolver environment; ordinary
codecs and actual builtin-fetch corpus; parent heap/sort; async supervisor; auth/AAD/
read-only store adapter/configured LAN; shared integration/current CI; original
#1368 baseline cause; independent final review and root approval. No shared files,
storage writes, activation, push, PR, merge, or external endpoint actions occurred.
