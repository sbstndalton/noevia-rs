# Diary INDEX.md transforms

Pure source parity for the INDEX.md helpers in the shipped Python
`diary/agent/corpus.py`: `IndexFile.render`, `parse_index`, `register_month`,
`_bullet_text`, `_bullet_match_indices`, `_check_bullet`, and
`apply_index_edits`. The crate returns transformed strings and in-memory
sections. It does not read or write files, own journal intents, authorize
edits, use a clock, invoke models, select a service, or change image pins.

The Rust API accepts typed UTF-8 strings and an explicit `today` string. The
Python helper's ambient `date.today()` fallback is intentionally replaced by
this caller-supplied value. Missing or null replacement fields retain distinct
open-question edit behavior. The caller is responsible for validating model
JSON types and ensuring the supplied date is the same date Python would have
used. No calendar parsing occurs in either helper for supplied date strings.

Input text is capped at 1 MiB and 20,000 source lines. Rendering may add
section scaffolding, so rendered output is separately capped at 24,000 lines
and 2 MiB. Sections are capped at 1,000, edit operations at 2,000, and
individual fields at 64 KiB.
Capacity errors are explicit and `apply_index_edits` leaves the caller's
in-memory value unchanged. `parse_index_bytes` also distinguishes invalid UTF-8
from a missing or empty source. Malformed but valid UTF-8 Markdown remains
leniently parsed, as in Python. A later adapter must fail its context or durable
write before mutation when a capacity error occurs. These caps are Rust
boundaries; the shipped Python helper itself had no equivalent caps.

`tests/index-fixtures.jsonl` contains 665 index transformation cases and 358
bullet helper cases generated from the shipped services source with CPython
3.12. The source file SHA-256 is asserted by the generator and the test. The
fixtures are wholly synthetic and cover section ordering, exact versus loose
bullet matches, noncanonical checkbox spacing, registration idempotence,
Unicode text and Python `splitlines` separators. They do not establish live
service, journal, persistence, or model parity. This crate only accepts the
string/null subset of edit fields used by the current caller; an adapter must
reject other JSON types instead of silently coercing them.

Regenerate on the development host using the project build wrapper:

```sh
~/bin/devrun python3.12 crates/diary-index-transform/tests/generate_fixtures.py \
  --source /path/to/noevia-services/diary/agent/corpus.py
~/bin/devrun cargo test -j2 -p diary-index-transform
```
