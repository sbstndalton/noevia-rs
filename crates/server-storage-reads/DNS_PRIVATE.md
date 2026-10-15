# Private owned main-thread DNS slice

This standalone crate has no production route, auth/store adapter, shared workspace
integration or activation. The production child remains Linux-required. This is
finite private source and synthetic evidence; root and independent final review
remain required. #1368's original metadata baseline cause remains open.

## Boundary

`prepare_owned_dns` is synchronous. The fixed production child calls it after the
bounded Request receive and before `runtime.block_on`; it calls the fixed
`std::net::ToSocketAddrs` on that child main thread. Production roots, ring provider,
safe TLS versions, no client auth, no resumption, no early data, DNS SNI and HTTP/1
ALPN still complete before READY. There is no public config/resolver constructor,
environment trust switch or fixture mode in the production binary.

The opaque attempt owns its prepared outgoing request, original URL identity,
fixed address slots, selected cap, trust Arc and absolute std Instant. Execution
accepts only this attempt. It wraps the stored Instant with Tokio `from_std`,
without renewing the budget. DNS preparation checks the same deadline before and
after resolution, before retaining each address, after collection and at execution.
TCP/TLS/HTTP/body use this one expiration; body retains its additional ten-second
clip. A blocked synchronous call cannot check a child timer or cancellation.

`attempt_dns[_observed]` delegates to the existing owned NET pump with the minimum
of outer remaining and Request attempt timeout. Zero budget refuses before spawn.
The timer begins inside that unchanged pump before spawn, after serialization.
Setup, READY, IPC, DNS and transport therefore share the enclosing process budget.
The old `attempt_http` independently supplied duration and the literal-only
`execute`/`execute_tls` signatures and hostname refusal remain available. A future
production adapter must bind the fixed DNS executable to `attempt_dns`.

## Named restrictions

- `DNS_HOST253_ASCII_CANONICAL`: original URL canonical Domain is nonempty ASCII,
  <=253 bytes including a trailing dot, with no NUL. URI scheme/host/port must
  match; raw percent/Unicode parser behavior remains qualified. New owned name and
  ServerName copies follow this guard. The URL parser can allocate inside the
  already bounded 64KiB URL domain. Explicit Host never selects DNS/SNI/TLS identity.
- `DNS_CANONICAL_ALIAS_REFUSAL`: nonliteral raw authorities canonicalizing to IP
  refuse; the existing strict numeric fast path never calls the resolver.
- `DNS_RESULTS32_REFUSE33`: exactly 32 inline Option<SocketAddr> slots. At most 33
  iterator results; item 33 refuses the whole attempt before a retained copy or
  socket. Empty/error refuse as Dns. Drop the iterator before dialing.
- `DNS_SERIAL_OS_ORDER_COMMON_DEADLINE`: preserve iterator order, families, IPv6
  representation and duplicates; dial SocketAddr serially. Only finite TCP errors
  proceed to the next address, with no timer reset. First TCP success wins; TLS
  happens once with the original URL identity. No later address fallback after
  TLS/HTTP begins. Final TCP failure uses the last bounded typed category.
- `DNS_ENV_CLEAR_LOCALDOMAIN_RES_OPTIONS`: parent env_clear retains its intentional
  resolver-environment restriction. System resolver/NSS configuration still applies.
- `DNS_OWNED_ATTEMPT_INCLUDES_SETUP_IPC`: parent clamp is conservative; encoding
  remains outside the per-child timer. No cross-process Instant is serialized.

These are selected Rust restrictions, not Node resolver/address-selection,
resolver environment or retry parity. Actual pinned Node storage reads have whole
15s/20s fetch budgets, not a separate four-second DNS stage. There is no stage
marker, separate DNS timeout, resolver worker, detached task or spawn_blocking.

## New finite evidence

Closed `private-dns-fixture` includes the actual module but permits only fixed
localhost or loopback literals, fixed loopback result modes and corrected synthetic
PKI. It never calls actual DNS. Resolver callbacks require no entered runtime and
no LOCALDOMAIN/RES_OPTIONS despite synthetic values supplied to the parent command.

New focused controls cover original DNS certificate/SNI versus forged Host, wrong
SAN, public-root refusal, IPv4 and actual IPv6 TLS sessions, IP/no resolver, 32/33,
empty/error, pure collector duplicate/order/IPv6 flow/scope preservation, first
TCP refusal then second-family success, both-family accepting first-order witnesses,
no fallback after first TCP/TLS refusal, host253/254 and canonical alias/header/URL
refusals, synchronous delay exhausting the child budget, and parent cancellation
and both outer/request clamps during a fixed main-thread resolver stall. BeforeReap
holds the lease; confirmed cleanup releases it and exact leader wait returns ECHILD.
No failed-reap hook was authorized or added.

Four new actual source mutants execute once: original identity replaced by selected
IP, result33 truncated to32, reversed stored order, and post-DNS timer renewal.
Strict unchanged classifier binds exact assertion source/line/column, selected test,
status101, sole panic, complete stdout grammar and unique terminal Cargo footer.
Raw captures, source copies, forgery/tail negatives and failures are retained in the
external revision-bound root packet. No old HTTP/TLS/owner/Node groups were rerun.

## Holds

The application-retained fixed32 array does not bound std/libc/getaddrinfo internal
allocation, copies, NSS helpers or system-service memory. No exact installed std
resolver implementation, Node OS order, production DNS call or Linux execution was
certified here. Existing Linux AS/CPU/core/dumpability/readback and 128MiB baseline/
READY qualifications persist. Hard RSS, recoverable OOM, timer preemption, kernel
exit/reap and uninterruptible blocking behavior are unproven.

Ordinary codecs/strict Brotli/full builtin Node corpus, actual full encrypted-session
limits, adversarial TLS handshakes, parent transient heap/sort, async supervisor,
auth/CSRF/account/AAD/read-only store constructor/transaction/configured member/admin
LAN authority, route claims, shared dependencies/default-off integration, current
CI/M6 handoff and original #1368 baseline cause remain held. Readonly lazy
reencryption omission and no stored credential writer remain qualified. Synthetic
IPv6 success does not close these wider TLS/production/integration holds.
