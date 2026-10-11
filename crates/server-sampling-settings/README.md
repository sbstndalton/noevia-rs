# Native sampling catalogue and global default

This slice owns only `/api/sampling-settings` and `auto_sampling_presets_enabled`.
Task classification, chat selection, providers and model routing remain outside this crate.

`NOEVIA_RUST_SAMPLING_SETTINGS=1` requires native auth, advertises `rust-sampling-settings`,
and grants one fixed uncached SQLite setting statement. The M3 setting allowlist is unchanged.
The setting and authenticated audit append commit together. Node's dark fallback has the same
transaction after core#65 / noevia#1298; confirmed Node ownership refuses the route and writes.

## Bounds and evidence

Four requests are admitted, with no wait queue. Each retains at most 1 MiB + 1 streamed body
bytes and has a ten-second body deadline. Parsing is linear: lossy UTF-8 replacement (at most
three times the body size), bounded raw JSON validation, and a scan retaining only the last
`enabled` value. Nesting above 128 fails closed with `invalid JSON`; this is explicitly stricter
than Node, and is not claimed as an equal-contract case. No generic object map or quadratic
key lookup is used. Missing/NULL settings and every value except exact SQLite text `false`
remain enabled; the GET SQL returns only a Boolean rather than materializing a large value.

`tools/gen-sampling-fixtures.cjs <clean core> <output>` executes actual Node route/body/JSON/auth
SQLite/audit collaborators with isolated synthetic data. The committed fixture's immutable
core oracle SHA is landed core#66 `c613f4595b3b91d8acc173e402ea55953f3a4506`, matching
`contracts/http/core.ref` and its independently verified archive checksum.
All 76 module cases match status/body/setting/audit. Real HTTP tests replay 74 of those cases
with a dead Node upstream; the two throwing-Readable cases are module-only transport tests.
Native body tests separately cover stream errors, actual byte overflow, deadlines and admission
saturation. HTTP HEAD has no body, and only audit timestamp is checked against the actual
request interval; actor, target, action, detail and setting rows remain exact.

The omission control restores a valid session, proves native `/api/auth/session` succeeds,
then requires exactly 502 from sampling with the switch absent and dead legacy. The required
core front workflow must also exercise full recorded HTTP corpus replay plus a confirmed Node
503 omission control. Helper tests alone do not satisfy this gate.

Mutation checks remove the admin check, audit append, default-on rule, fixed-key isolation and
native handler. Each must fail a real contract assertion, not merely fail to start or compile.
