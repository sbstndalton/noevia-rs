# Diary entity focus source leaf

This crate ports deterministic entity extraction, classification, and post-search candidate selection from the frozen Python 3.12 `diary/agent/entity_focus.py` source (SHA-256 `dd4312e26a22cf2132f18bbd0ca4d7fd55a885706b8d0aa8997c14e94eeb7632`). It contains no vector search, filesystem, database, HTTP, model, context-note, label, or CLI path.

`analyse` takes the caller's canonical temporal anchor. `focus_candidates` takes an exhaustive, ordered partition of original 1-based vector ranks into survivors and removed. It never parses query time or reapplies a cutoff. The caller supplies the partition from the already validated temporal path and retains the anchor as metadata. Malformed partitions and anchors are errors.

Inputs have explicit capacities: 1 MiB query or earlier message, 64 earlier messages and 4 MiB aggregate, 10,000 candidates, 256 KiB per row and 32 MiB aggregate, 128 bytes per score or margin text, four entities, and 8,192 words. Excess is an error. Python-style float text normalization handles Unicode decimal digits and underscores.

The original differential corpus contains 784 synthetic focus cases, 420 classification cases, and 13 margin cases. It is unchanged. Five additional correction-policy cases, 172 keyword/relation cases and 62 role/header cases exercise actual frozen-Python regex behavior. Independent whole-string digests check lower/key results for all 1,112,064 Unicode scalars and 9,720 contextual strings, while actual Python regex oracles check case equivalents and scalar property classes.

Generators require CPython 3.12.15 / Unicode 15.0.0 and verify the frozen source hash before writing. Use the repository's build/test wrapper and `tests/generate_fixtures.py`, `tests/generate_rules.py`, `tests/generate_unicode15.py` and `tests/generate_regex_probes.py` with `--source /path/to/frozen/entity_focus.py`. Output options support isolated reproduction. Wrong interpreter/source controls must leave outputs unchanged.

Runtime lowercasing, Final_Sigma, character predicates and NFKD use pinned static tables rather than the toolchain's newer Unicode data. Key normalization applies whole-string lower before the source fold and decomposition; filtering nonzero canonical combining classes leaves the same retained sequence as whole-string NFKD. The finite policy patterns use pinned Python word/space/decimal classes and case equivalents on original UTF-8, with scalar-safe boundary checks. This is not a general Python regex interpreter or an ASCII-only name restriction.

The crate remains a pure source component. Python retrieval, vector search, context notes and labels retain authority; no CLI or services adapter is supplied. Packaging, activation, full synthetic retrieval/model qualification and retirement of old production paths remain separate.
