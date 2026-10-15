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
32 child levels, 1,000,000 total values across chat/history, and 16 MiB combined UTF-8 string
and property-key bytes (raised from 20,000 / 1 MiB for the route, #1315, #1824). These are stricter than the unbounded Node formatter. Siblings
are charged before any key-set reservation; duplicate checking reserves at most the
node budget across an active traversal. Output is capped at 32 MiB. Two reusable output
buffers grow geometrically from 64 bytes, capped at 32 MiB each (plus allocator rounding),
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

# Export assembly (#1315)

`assemble::build_export` is core `server/chat-export.cjs` `buildExport` (core
`22dcb9d387a30fca090d970a2662268abac22c64`), ported as behaviour on top of `zip_store` and
`markdown::render`: entry names (NFKD slug, id part, `-2`, `-3`, … collision suffixes, resumed
per base name so ten thousand identical titles stay linear), project folders, `README.md`, and
`conversations.json` byte for byte (`JSON.stringify(…, null, 2)` indented four spaces, with
`reasoning`, `answers`, `answer`, `branches`, `branch` left out of every history entry). The byte
and entry limits are checked in Node's order, so an input is refused with Node's class
(`TooLarge` = 413, with `too_large_message`; `Type` = Node's TypeError).

It still owns no route, tenant read, clock, audit write or switch: the caller hands it the free
chats and projects and reads each history when asked, in Node's order. Narrower admission, as
refusals (`Unsupported`), never different bytes: a truthy list that is not an array, a `now`
outside years 0–9999, and the Markdown and ZIP foundations' own bounds.

Reproduce the 34 synthetic Node cases (18 complete ZIPs, the rest error classes and messages):

```
node tools/gen-export-assembly-fixtures.cjs /path/to/core > crates/server-export/tests/fixtures/node-assembly.v1.json
cargo test -p server-export --test assemble
```

Since #1315 a lone UTF-16 surrogate (held as js-json `wtf16` maps it) is exported as Node exports
it: `\udxxx` in `conversations.json`, U+FFFD (EF BF BD) in the Markdown.

# The route (#1315)

`route::export(data_dir, user_id, now)` is what `GET /api/export/conversations` answers, read from
the signed-in user's directory only (owner decision 2026-10-12: export what is on disk): the free
chats, the projects without the Diary-internal ones, and each chat's history file, with Node's
coercions and fallbacks (see the module docs). bins/noevia-server `export_routes.rs` runs Node's
gate first, writes the `export.conversations` audit row before sending, and answers with Node's
headers; the switch is `NOEVIA_RUST_EXPORT=1` (needs `NOEVIA_RUST_PROJECTS=1` and a build with
`rust-export` and `rust-free-chats`).

Narrower than Node, as refusals: an unparsable free-chats.json or projects.json, a `projects` that
is not a list, a value js-json cannot hold, or a symlinked or non-regular (FIFO) file is a 500 (Node
would export less, its in-memory copy, or follow the link); a history file over 16 MiB or over
500,000 values / depth 40, more than 128 MiB of history in one export, or a chat over the Markdown
bounds above is Node's 413 (#1824).

Memory (#1824): values are counted on the text before any tree is built, the file text is moved
rather than copied, and the ZIP is built as chunks (`ZipWriter`, the same bytes as `zip_store`)
that the front sends in order, so the payload is held once. `tests/peak_memory.rs` exports a
61.2 MiB ZIP (three ~9 MiB messages and one history at the value cap): peak RSS 122.5 MiB on
macOS (`/usr/bin/time -l`, whole test process); on Linux the test asserts `VmHWM` < 150 MiB.

Reproduce the Node oracle (core's own workspace store, readHistory, route and http.cjs; flock(1)
on PATH, as the M4 switches need) and the differential test:

```
node crates/server-export/tests/route-oracle.cjs /path/to/core crates/server-export/tests/fixtures/node-export-route.v1.json
cargo test -p server-export --test route
```

Mutations observed to fail `every_node_case_is_answered_as_node_answers_it`: the non-lossless
JSON stringify (lone surrogates), Markdown without U+FFFD, the chat-context project not filtered,
and a non-list free-chats.json refused instead of read as `[]`.
