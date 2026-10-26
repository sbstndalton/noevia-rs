# Diary corpus write transforms

Pure source parity for the format helpers in `diary/agent/corpus.py`: canonical
headers, escaped prose, exchange/subsection rendering, marker inspection,
exchange text/hash/replacement, and month-text insertion. The crate does not
read/write files or databases, perform journal deduplication, authorize edits,
invoke models, or route a production service. It changes no selector or image.

`tests/corpus-write-fixtures.json` is generated from the shipped services source
by `tests/generate_fixtures.py` with CPython 3.12 and Unicode 15.0. All entries
are synthetic. The generator fixes the C locale and explicit dates/times.
Python `strftime` is locale-sensitive in other environments; this crate emits
canonical English day/month names and does not claim parity for other locales.
The caller supplies a valid Python-range date (year 1 through 9999) and fixed
time; no ambient clock or timezone is read by this crate.

To regenerate on the development host from the shipped Python source:

```sh
cd crates/diary-corpus-write
~/bin/devrun python3.12 tests/generate_fixtures.py --source /path/to/noevia-services/diary/agent/corpus.py
```
