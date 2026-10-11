# Stored ZIP foundation (#1316)

`zip_store` is a functional bounded stored-ZIP writer needed by future native export
(#1315). It does not own an HTTP route, tenant/history reads, conversation formatting,
slug/collision policy, reasoning omission, or audit writes. Those policies remain Node-owned.
The workspace metadata cache boundary blocks HTTP migration until explicitly resolved.

The admitted byte format follows landed core `965598462d64dc04e1df55c3bb7a25d4343380e1`
`server/chat-export.cjs::zipStore`: caller order, UTF-8 flag, stored payload, CRC32,
classic local/central/end records, UTC DOS dates including Node's pre-1980 year clamp.
Duplicate names are retained, as in that low-level oracle. Timestamps must be
representable by chrono UTC and the encoded DOS year must not exceed 2107; pre-1980 years are clamped while preserving month/day. Other timestamps
are refused (this is stricter than Node’s low-level timestamp admission). This library
does not invent conversation paths or sanitize caller names.

Before payload traversal or output allocation it admits at most 20,000 entries,
64 MiB total payload, 65,535 bytes per UTF-8 name, and 1 MiB combined names.
The name budget is deliberately stricter than Node's low-level writer. Output is bounded
by `64 MiB + 2 MiB + 76 * 20,000 + 22` bytes (70,726,038), plus at most 20,000
small records; borrowed input is not copied until the final output. All size arithmetic
is checked. Work is linear in entry count, name bytes and payload bytes. Allocation
failure returns an error. The caller supplies an explicit millisecond timestamp; there
is no clock override or global production clock setting.

Reproduce the 15 complete-byte synthetic oracle fixtures with the repository Node runtime (providing `zlib.crc32`):

```
node tools/gen-export-zip-fixtures.cjs /path/to/clean/core965 > crates/server-export/tests/fixtures/node-zip.v1.json
cargo test -p server-export
```

The generator verifies the actual core revision and records the source SHA-256. Fixtures
cover empty/multiple/duplicate names, Unicode, binary bytes/CRC, leap days, odd seconds,
pre-1980 clamp, and the last classic DOS year. Property tests independently inspect ZIP
records, offsets, sizes, names, CRCs, payloads and order. This is library byte compatibility,
not native-front endpoint parity or an export feature activation.

## Mutation qualification

Run the focused tests after each isolated production-source mutation, then restore the
source. Each following mutation was observed to fail an actual assertion:

| Mutation | Required failing test |
| --- | --- |
| Replace `crc32fast::hash(entry.data)` with zero | `actual_node_complete_zip_bytes` |
| Replace local magic `0x04034b50` with `0x04034b51` | `actual_node_complete_zip_bytes` |
| Reverse the local-entry writing traversal | `actual_node_complete_zip_bytes` |
| Admit `MAX_ENTRIES + 1` | `limits_are_checked_before_zip_output` |

Use `cargo test -p server-export --test zip <test-name>` for each qualification.
These controls distinguish valid complete-byte output from malformed CRC/header/order
and weakened admission; they do not qualify tenant or HTTP policies that are not yet moved.

# Native Markdown foundation (#1317)

`markdown::render(&chat, &history)` implements actual core965 `chatMarkdown` policy:
truthy title/default title, optional last-updated line, exact user/assistant role filtering,
JavaScript plain-data string coercion, ECMAScript trim/empty-text marker, truthy tool-name
filter/coercion/join, exact blank lines and trailing newline. Reasoning and other roles
are never emitted. This is functional policy code, still no HTTP/cache/history/audit owner.

Inputs are borrowed `js_json::JValue` plain data; the formatter does not parse JSON.
Canonical decoded objects have unique keys. Manually supplied duplicate-key objects are
refused (`UnsupportedInput`); JSON.parse duplicate semantics are covered by the oracle
fixture decoder. Rust strings are valid Unicode scalar UTF-8; lone UTF-16 surrogates are
outside this API. Arrays stringify with comma joining/nullish empty slots; objects use
inherited string-hint Object.toString unless a non-callable own `toString` causes `Type`.
An own `valueOf` alone does **not** cause refusal. Existing Number::toString handles
scalar numbers, including explicit nonfinite JValues; its fixed-size scalar temporaries
are reused, not a general untrusted JSON parser/coercion bridge.

Date admission is explicit: falsy metadata omits the date; truthy numeric milliseconds
use ECMAScript TimeClip (truncate toward zero), with resulting UTC years 0000–9999;
`true` means 1 ms; strings must be exactly valid `YYYY-MM-DDTHH:mm:ss.sssZ` (24 ASCII
bytes), with Gregorian calendar/time validation and exact canonical roundtrip. Numeric
nonfinite/outside ±8.64e15 values yield `Date` (Node RangeError). Other date forms, valid
extended years, noncanonical strings/timezones and calendar rollover forms yield
`UnsupportedDate`; Node may accept them. This is **not** general JavaScript Date parity.
`Type` matches the admitted Node TypeError class (null chat/message, noniterable history,
truthy non-array toolCalls, failed plain-object coercion); Node-specific exception message
wording is captured in fixtures but is not exposed by the typed native error API.

Before formatting, the whole tree—including ignored reasoning/roles—is admitted within
32 child levels, 20,000 total values across chat/history, and 1 MiB combined UTF-8 string
and property-key bytes. These are stricter than the unbounded Node formatter. Siblings
are charged before any key-set reservation; duplicate checking reserves at most the
node budget across an active traversal. Output is capped at 2 MiB. Two reusable output
buffers grow geometrically from 64 bytes, capped at 2 MiB each (plus allocator rounding),
with fallible reservations and amortized linear copying. Small returned Markdown does
not retain a maximum-sized allocation. No input-sized coercion intermediates or
per-character reallocation occur. Hash-set reservations are fallible. Work is bounded
by tree values/bytes and output; property lookup/coercion does a fixed number of linear
passes, maximum recursion 32. Checked bounds return `Limit`, reservation failure
`Allocation`; allocation failure injection is not claimed.

Reproduce actual Node byte/error/refusal goldens:

```
node tools/gen-export-markdown-fixtures.cjs /path/to/clean/core965 > crates/server-export/tests/fixtures/node-markdown.v1.json
cargo test -p server-export --test markdown
```

Goldens distinguish actual Node success/error from this library's stricter admission,
retain actual Node exception names/messages, and pin revision/source digest. Properties
check text formatting/privacy and numeric UTC roundtrips; tests cover whole-tree limits,
exact admission, duplicate input, nonfinite numbers and output/arithmetic refusal.

Markdown mutation qualification (`cargo test -p server-export --test markdown <test>`):

| Production mutation | Required assertion failure |
| --- | --- |
| Emit `message.reasoning` | `private_reasoning_and_other_roles_never_render`, actual private-reasoning leak marker |
| Drop the role filter | Same test, actual private-system-content leak marker |
| Use Rust `trim()` | `actual_node_bytes_and_error_classes` |
| Refuse own `valueOf` objects | Same Node oracle test |
| Floor fractional Date numbers | Same Node oracle test (negative TimeClip) |
| Remove tool separator space | Same Node oracle test |
| Change default title | Same Node oracle test |
| Admit one byte over whole-input cap | `preflight_bounds_all_fields_before_formatting` |

Each was observed to fail an assertion independently, and source was restored. These
controls qualify native formatting/admission, not the still-held HTTP export contract.
