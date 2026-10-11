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
