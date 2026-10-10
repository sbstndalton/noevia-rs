# Native usage reporting (M6)

`noevia-server` owns every method on the two exact usage paths when `NOEVIA_RUST_USAGE=1` and native auth is on. It runs the existing identity/upkeep policy, returns 405 after the auth gate for non-GET requests, and requires an effective admin identity for aggregate. URL parameters and headers never select a user directory. Node records usage; this crate writes no usage file or database row.

Reads use no-follow directory/file descriptors rooted in UI_DATA_DIR, regular files only, a 2 MiB limit checked both before and during reading (limit+1 sentinel), and UUID-shaped user IDs. Missing/invalid own files summarize as empty; aggregate missing files do not count unreadable, malformed/oversize/symlink/type failures do. Aggregate keeps the current account-list order (created_at, no new tie ordering), path-signature/30s caching and checkedAt semantics.

Native bounds are deliberately stricter: 1000 accounts; 32 MiB total bytes read, including malformed inputs; 65536 merged keys; four admitted reporting requests, with one aggregate reader/cache lock. One input file is retained at a time. Crossing total/key limits returns 503, not partial totals. Invalid JSON numeric ranges/depth are refused by the bounded native JSON parser. No symlink can redirect a user/file read into another tenant.

Ordered input decoding is local to this crate; serde_json workspace features are unchanged (#1284). Counters retain fractions and clamp at 2^53-1. Integer-key enumeration, combined-weight stable model ties (#1286), locale-sensitive tool ties, 24-hour counters and both streaks follow Node. chrono-tz provides civil/DST behavior with compatible gap/overlap disambiguation; ICU4X supplies native collation. `src/timezone-names.json` records Node ICU's canonical display names for all 597 supported IANA timezone spellings (generated from chrono-tz names via Node Intl, not runtime Node calls). Unsupported timezone/locale configuration fails closed rather than assuming a different clock.

`tools/gen-usage-fixtures.cjs` loads actual Node modules from immutable core acceptance head `b3ad33c9778041d21a45357c1fcdc31d2db086fc` (landed core #63), the immutable acceptance input. Its 288 synthetic cases cover UTC/New York/Oslo/Apia, English/Swedish/German, DST and skipped civil days, Unicode/canonical-equivalent names, fractional/invalid/clamped counts, retention and streaks. CI also regenerates against its explicitly pinned companion core head and compares semantic results. Recording never runs in this generator.

Focused commands:

- `cargo test -p server-usage`
- `cargo test -p noevia-server --test usage`
- `cargo test -p noevia-server --lib usage_`
- `cargo test -p passkey --test compat registrations_agree_with_simplewebauthn`
- `cargo clippy -p server-usage -p noevia-server --all-targets -- -D warnings`

Mutation evidence: restoring the old sequential-subtraction model comparator fails the fractional MAX_SAFE self-comparison (exit 101); the Node 64-model regression also fails with its old comparator (exit 1).  removing only the safe-number clamp made `mutation_control_detects_clamp_omission` fail (exit 101). Removing only the packed self-attestation algorithm equality guard made the strengthened #1258 registration test fail (exit 101), at the named ES256-key/RS256-statement mismatch which Node accepts. Original production code was restored. Valid matching algorithms remain in the same corpus. Real HTTP usage tests use a dead legacy upstream and prove positives cannot come from fallback; disabling usage ownership fails that positive condition. Authoritative full corpus replay and Node-confirmed omission control are in the core companion required workflow.
