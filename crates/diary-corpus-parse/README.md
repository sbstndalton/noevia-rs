# Diary corpus parsing and chunk projection (#1071)

Pure foundation from noevia-services `6c1eb5891c75b6fb9f04c4484ba39da80a5b4b9e`:
`diary/agent/corpus.py::parse_diary` and
`diary/agent/retrieval.py::Retriever._parse_chunks`.
Rust base: `4d18712c75be72f209af47ea420bfeb25da3dca3`.

`parse_diary(&str)` returns source-order days, subsections, time/topic and
exchanges. The original day heading is retained after Python trailing-whitespace
trimming; valid dates are ISO strings, syntactically matched invalid dates remain
`None`. Legacy Claude and Assistant labels both populate `claude`. Hidden xid
markers are removed from visible text with the same assistant-first selection.

`parse_chunks(Option<&str>)` skips invalid dates and empty subsections and
projects the exact Markdown chunk text plus lower-case SHA256 of its UTF-8 bytes.
Source order and duplicate rows remain intact. There is no NFC/NFD normalization.
Rust strings represent valid UTF-8; Python strings containing unpaired surrogates
are outside this API (the Python chunk UTF-8 encoder would reject them).

Compatibility is pinned to the shipped Diary Python **3.12 / Unicode 15.0.0**,
not the machine's default Python. The explicit whitespace/splitlines rules include
Python's U+001C–U+001F whitespace behavior. The generated decimal table mirrors
Python regex `\d` and `int`; later Unicode digit additions deliberately do not
become day/time digits. Dates use Gregorian leap years and years 1–9999;
weekday text need not agree with the calendar.

The stdlib-only generator executes the actual corpus source and the actual
AST-extracted retrieval projection method without initializing storage or models.
It extracts the existing synthetic `test_corpus.SAMPLE`, creates deterministic
malformed/multiline/legacy/xid/empty/duplicate/Unicode/random cases, and records
source hashes and interpreter/Unicode versions. Generate with:

```text
~/bin/devrun /absolute/path/to/python3.12 crates/diary-corpus-parse/tests/generate_fixtures.py /absolute/path/to/services/diary
~/bin/devrun cargo fmt -p diary-corpus-parse
~/bin/devrun cargo test -p diary-corpus-parse -j2
```

The Rust differential test compares every parsed field, chunk string and hash.
It needs no Python installation to consume the committed oracle vectors.

This crate has **no production consumer or CLI**. Services adapters, bounded
transport, Docker/CI wiring and release pins remain separate work. Existing
editing and rendering stay in Python; this foundation does not retire them or
activate any runtime selector.
