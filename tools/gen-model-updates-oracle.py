#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/model-updates-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, middleware, router,
`update_status`, `helpers._update_status_map`, `services.snapshot_models_dir`, `ini` and `db`)
against synthetic model folders, models.ini files and SQLite stores, and records the exact status,
content type and body bytes of `GET /api/v1/models/updates`:

    python3 -I tools/gen-model-updates-oracle.py <services>/model-manager/app > fixture.json

Run it on Linux, as CI does: the route formats dates with `strftime("%Y-%m-%d")`, and glibc does
not zero-pad `%Y` for years before 1000 where macOS does. The committed fixture is the Linux answer.

It also records CPython's own `datetime.fromisoformat` (the C implementation production runs)
for a deterministic corpus of timestamp strings, so the native port of that parser is checked
far beyond what one HTTP case can hold.

Every file name, repository and timestamp is synthetic. Each case is declarative (empty files
with exact nanosecond mtimes, symlinks, directories, ini text or bytes, SQL statements) so the
Rust test rebuilds the identical tree and store. CI re-runs this against the pinned services ref
and requires a byte-identical fixture.
"""
from __future__ import annotations

import hashlib
import json
import os
import random
import sqlite3
import sys
import tempfile
from datetime import datetime
from pathlib import Path

SCHEMA = ("CREATE TABLE update_check (filename TEXT PRIMARY KEY, repo_id TEXT NOT NULL, "
          "hf_last_modified TEXT, checked_at REAL NOT NULL)")
# No column affinity and no key: lets a hand-edited store hold any type and duplicate names.
LOOSE = "CREATE TABLE update_check (filename, repo_id, hf_last_modified, checked_at)"
T0 = 1_700_000_000_000_000_000  # 2023-11-14T22:13:20Z, in nanoseconds


def lit(value: str) -> str:
    """SQL text literal; NUL is spliced in with char(0)."""
    return "||char(0)||".join("'" + part.replace("'", "''") + "'" for part in value.split("\0"))


def row(filename: str, remote, checked="1700000000.5", repo="'synthetic/repo'") -> str:
    remote_sql = "NULL" if remote is None else lit(remote) if isinstance(remote, str) else remote["sql"]
    return f"INSERT INTO update_check VALUES({lit(filename)}, {repo}, {remote_sql}, {checked})"


def sql(*rows: str, schema: str = SCHEMA) -> dict:
    return {"journal": "wal", "sql": [schema, *rows]}


def files(*names: str, mtime: int = T0) -> list:
    return [[name, mtime] for name in names]


# The remote strings the route must treat exactly as Python does (also folded into the corpus).
REMOTES = [
    "2024-05-01T12:34:56.000Z", "2024-05-01T12:34:56Z", "2024-05-01T12:34:56.123456Z",
    "2024-05-01T12:34:56.1234567890Z", "2024-05-01T12:34:56.5", "2024-05-01T12:34:56.",
    "2024-05-01T12:34:56.Z", "2024-05-01T12:34:56.ZZZ", "2024-05-01T12:34:56.1Z2",
    "2024-05-01T12:34:56+02:00", "2024-05-01T12:34:56-07:30", "2024-05-01T12:34:56+00:00",
    "2024-05-01T12:34:56.25+02:00", "2024-05-01", "2024-05-01T12", "2024-05-01T12Z",
    "2024-05-01 12:34:56Z", "2024-05-01T12:34:56", "20240501T123456Z", "2024-W18-3T10Z",
    "2024W183T10Z", "2024-W53-1Z", "2020-W53-7T00:00Z", "2024-02-30T00:00Z", "2023-02-29T00:00Z",
    "2024-02-29T23:59:59.999999Z", "0001-01-01T00:00:00Z", "0005-06-07T00:00:00Z",
    "9999-12-31T23:59:59.999999Z", "2024-05-01T24:00:00Z", "2024-05-01T12:60Z",
    "2024-05-01T12:34:60Z", "2024-13-01T00:00Z", "2024-00-10T00:00Z", "0000-01-01T00:00Z",
    "2024-05-01T12:34:56+24:00", "2024-05-01T12:34:56+23:59:59.999999", "2024-05-01T12:34:56+00:99",
    "2024-05-01T12:34:56-00:00:00.5", "2024-05-01T12:34:56+0530", "2024-05-01T12:34:56+05",
    "2024-05-01T12:34:56+5", "2024-05-01T12:34:56+053", "2024-05-01T12:34:56Z ", " 2024-05-01T12Z",
    "2024-05-01T12:34:56:789Z", "2024-05-01T123456789Z", "2024-05-01T12:34:56,5Z",
    "2024-05-01é12:34Z", "2024-05-01€12:34Z", "2024-05-01\U0001F600T12:34Z",
    "2024-05-01T12:34éZ", "2024-05-01T", "2024-05-0", "short", "Z", ".", ".Z", "x.y",
    "2024-05-01T12:34:56.1é", "2024-05-01T12:34:56.١٢", "٢024-05-01T12Z",
    "2024-05-01T12:34:56.000Z\0", "2024-05-01T12\0Z", "2024-05-01T12:34:56-05:00Z",
    "2024-05-01T12:34:56Z+01:00", "2024-05-01T12:34:56.000+01:00.5", "2024-11-14T22:13:20Z",
    "2023-11-14T22:13:20.000001Z", "2023-11-14T22:13:19.999999Z", "2023-11-13T22:13:20.000001Z",
    "2023-11-13T22:13:20Z", "1969-12-31T23:59:59.5Z", "2024-05-01T12:34:56-23:59",
]


def corpus() -> list[str]:
    """Deterministic fuzz corpus for datetime.fromisoformat: mutations of valid strings."""
    rng = random.Random(1318)
    seeds = [r for r in REMOTES if r] + [
        "2024-05-01T12:34:56.123456+05:30", "2024W18", "2024W183", "2024-W18", "20240501",
        "2024-05-01T12:34:56.123+0530", "2024-05-01T1234", "2024-05-01T12:34:56.1234567+00:00:00.1"]
    alphabet = "0123456789-+:.,TWZ Zé€\0x"
    out: list[str] = list(seeds)
    while len(out) < 4000:
        s = list(rng.choice(seeds))
        for _ in range(rng.randint(1, 3)):
            op = rng.random()
            if op < 0.4 and s:
                s[rng.randrange(len(s))] = rng.choice(alphabet)
            elif op < 0.7:
                s.insert(rng.randint(0, len(s)), rng.choice(alphabet))
            elif s:
                del s[rng.randrange(len(s))]
        out.append("".join(s))
    return out


def parsed(value: str):
    try:
        d = datetime.fromisoformat(value)
    except ValueError:
        return None
    offset = d.utcoffset()
    off = None if offset is None else (offset.days * 86400 + offset.seconds) * 1_000_000 + offset.microseconds
    return [d.year, d.month, d.day, d.hour, d.minute, d.second, d.microsecond, off]


def naive(remote: str) -> bool:
    """True when the route would subtract a naive datetime (TypeError, a whole-response 500)."""
    core, _, frac = remote.partition(".")
    try:
        if frac:
            return False
        return datetime.fromisoformat(core.replace("Z", "+00:00")).tzinfo is None
    except ValueError:
        return False


def remote_files(start: int = 0) -> tuple[list, list]:
    names, rows = [], []
    for i, remote in enumerate(r for r in REMOTES if not naive(r)):
        name = f"r{start + i:04d}.gguf"
        names.append([name, T0 + i * 3_600_000_000_123])
        rows.append(row(name, remote, checked=repr(1_700_000_000 + i / 8)))
    return names, rows


_rf, _rr = remote_files()

CASES: list[dict] = [
    {"id": "models-missing-store-ok", "models": "missing", "store": sql(row("a.gguf", "2024-01-01T00:00:00Z"))},
    {"id": "models-missing-no-table", "models": "missing", "store": {"journal": "wal", "sql": ["CREATE TABLE other (x)"]}},
    {"id": "models-missing-ini-invalid-not-read", "models": "missing", "ini": {"text": "garbage without a header\n[x\n"},
     "store": sql()},
    {"id": "models-path-is-file", "models": "file", "ini": {"text": "[broken\nno delimiter\n"}, "store": sql()},
    {"id": "store-missing", "files": files("a.gguf"), "store": None},
    {"id": "store-not-a-database", "files": files("a.gguf"),
     "store": {"bytes": ("synthetic not a database " * 64).encode().hex()}},
    {"id": "store-no-table", "files": files("a.gguf"), "store": {"journal": "wal", "sql": ["CREATE TABLE kv (k)"]}},
    {"id": "empty-everything", "store": sql()},
    {"id": "rows-without-files", "store": sql(row("ghost.gguf", "2024-01-01T00:00:00Z"))},
    {"id": "files-without-rows", "files": files("a.gguf", "b.gguf"), "store": sql()},
    {"id": "layout-grouping-folding-order",
     "dirs": ["sub", "Sub2", "deep/a/b/c/d", "deep/a/b/c", ".hidden", "blobs", "refs", "__pycache__", ".cache",
              "solo-companions", "models--org--repo/snapshots/rev1", "models--org--repo/blobs", "empty"],
     "files": [
         ["Zeta-Q4_K_M.gguf", T0], ["alpha-Q8_0.gguf", T0 + 86_400_000_000_000],
         ["UPPER.GGUF", T0 - 86_400_000_000_000], ["notes.txt", T0], ["mmproj-top-F16.gguf", T0 + 5],
         ["sub/big-00001-of-00003.gguf", T0 + 1_000_000_000], ["sub/big-00003-of-00003.gguf", T0 + 3_000_000_000],
         ["sub/big-00002-of-00003.gguf", T0 + 2_000_000_000], ["sub/mmproj-big-BF16.gguf", T0 + 9_000_000_000],
         ["sub/mmproj-big-F32.gguf", T0], ["sub/big-draft-Q4.gguf", T0],
         ["Sub2/alpha-Q8_0.gguf", T0 + 7 * 86_400_000_000_000],
         ["deep/a/b/c/d/too-deep.gguf", T0], ["deep/a/b/c/depth-four.gguf", T0 + 11],
         [".hidden/h.gguf", T0], ["blobs/b.gguf", T0], ["refs/r.gguf", T0], ["__pycache__/p.gguf", T0],
         [".cache/c.gguf", T0], [".dotfile.gguf", T0 + 12],
         ["solo-companions/mmproj-orphan.gguf", T0 + 13], ["solo-companions/model.imatrix.gguf", T0 + 14],
         ["models--org--repo/blobs/0123abcd", T0 + 15],
     ],
     "links": [["models--org--repo/snapshots/rev1/hf-model-Q4.gguf", "../../blobs/0123abcd"],
               ["linked-file.gguf", "alpha-Q8_0.gguf"], ["dangling.gguf", "nowhere.gguf"],
               ["linked-dir", "sub"]],
     "ini": {"text": "version = 1\n[short-id]\nmodel = /models/sub/big-00001-of-00003.gguf\n[alpha-Q8_0]\nctx-size = 4096\n"},
     "store": sql(*[row(n, f"2023-11-1{i % 10}T00:00:00.{i:03d}Z", checked=f"{1_700_000_000 + i}.25")
                    for i, n in enumerate(["Zeta-Q4_K_M.gguf", "alpha-Q8_0.gguf", "UPPER.GGUF", "mmproj-top-F16.gguf",
                                           "big.gguf", "big-00001-of-00003.gguf", "mmproj-big-BF16.gguf",
                                           "too-deep.gguf", "depth-four.gguf", "h.gguf", "b.gguf", ".dotfile.gguf",
                                           "mmproj-orphan.gguf", "model.imatrix.gguf", "hf-model-Q4.gguf",
                                           "linked-file.gguf", "dangling.gguf", "big-draft-Q4.gguf"])])},
    {"id": "unicode-order-and-companions",
     "dirs": ["Étage", "etage", "İdir"],
     "files": [["é-model.gguf", T0], ["E-model.gguf", T0 + 1], ["ΣΟΣ.gguf", T0 + 2],
               ["imatrıx-dotless.gguf", T0 + 3], ["IMATRIX-upper.gguf", T0 + 4], ["MMPROJ-Upper.gguf", T0 + 5],
               ["Étage/x.gguf", T0 + 6], ["etage/x.gguf", T0 + 7], ["İdir/mtp-head.gguf", T0 + 8],
               ["İdir/main.gguf", T0 + 9], ["nomtp-draft.gguf", T0 + 10], ["model-MTP.gguf", T0 + 11],
               ["kKelvin-mmproj.gguf", T0 + 12]],
     "store": sql(*[row(n, "2023-11-15T00:00:00Z") for n in
                    ["é-model.gguf", "E-model.gguf", "ΣΟΣ.gguf", "imatrıx-dotless.gguf",
                     "IMATRIX-upper.gguf", "MMPROJ-Upper.gguf", "x.gguf", "mtp-head.gguf", "main.gguf",
                     "nomtp-draft.gguf", "model-MTP.gguf", "kKelvin-mmproj.gguf"]])},
    {"id": "mtime-rounding",
     "files": [["half-even-down.gguf", T0 + 500], ["half-even-up.gguf", T0 + 1_500], ["just-under.gguf", T0 + 999_999_499],
               ["round-up-second.gguf", T0 + 999_999_999], ["pre-epoch.gguf", -86_400_000_000_123],
               ["pre-epoch-half.gguf", -1_500], ["epoch.gguf", 0], ["far-future.gguf", 2_147_000_000_000_000_123],
               ["y1902.gguf", -2_100_000_000_000_000_000]],
     "store": sql(*[row(n, r) for n, r in [
         ("half-even-down.gguf", "2023-11-14T22:13:20Z"), ("half-even-up.gguf", "2023-11-14T22:13:20.000001Z"),
         ("just-under.gguf", "2023-11-14T22:13:21Z"), ("round-up-second.gguf", "2023-11-14T22:13:21Z"),
         ("pre-epoch.gguf", "1969-12-30T23:59:59Z"), ("pre-epoch-half.gguf", "1970-01-01T00:00:00Z"),
         ("epoch.gguf", "1970-01-01T00:00:00Z"), ("far-future.gguf", "2024-01-01T00:00:00Z"),
         ("y1902.gguf", "2024-01-01T00:00:00Z")]])},
    {"id": "remote-strings", "files": _rf, "store": sql(*_rr)},
    {"id": "remote-naive-500", "files": files("a.gguf"), "store": sql(row("a.gguf", "2024-05-01T12:34:56"))},
    {"id": "remote-date-only-500", "files": files("a.gguf"), "store": sql(row("a.gguf", "2024-05-01"))},
    {"id": "remote-naive-unrelated-row", "files": files("a.gguf"),
     "store": sql(row("a.gguf", "2024-05-01T00:00:00Z"), row("other.gguf", "2024-05-01T12:34:56"))},
    {"id": "remote-typed-null-empty", "files": files("n.gguf", "e.gguf"),
     "store": sql(row("n.gguf", None), row("e.gguf", ""))},
    {"id": "remote-int-zero", "files": files("a.gguf"), "store": sql(row("a.gguf", {"sql": "0"}), schema=LOOSE)},
    {"id": "remote-real-zero", "files": files("a.gguf"), "store": sql(row("a.gguf", {"sql": "0.0"}), schema=LOOSE)},
    {"id": "remote-int-500", "files": files("a.gguf"), "store": sql(row("a.gguf", {"sql": "20240501"}), schema=LOOSE)},
    {"id": "remote-real-500", "files": files("a.gguf"), "store": sql(row("a.gguf", {"sql": "1.5"}), schema=LOOSE)},
    {"id": "remote-int-text-affinity", "files": files("a.gguf"), "store": sql(row("a.gguf", {"sql": "20240501"}))},
    {"id": "remote-blob-empty", "files": files("a.gguf"), "store": sql(row("a.gguf", {"sql": "x''"}), schema=LOOSE)},
    {"id": "remote-blob-500", "files": files("a.gguf"),
     "store": sql(row("a.gguf", {"sql": "x'323032342d30352d30315a'"}), schema=LOOSE)},
    {"id": "remote-invalid-utf8-500", "files": files("a.gguf"),
     "store": sql(row("a.gguf", {"sql": "CAST(x'32303234ff' AS TEXT)"}))},
    {"id": "repo-invalid-utf8-500", "files": files("a.gguf"),
     "store": sql(row("a.gguf", "2024-05-01T00:00:00Z", repo="CAST(x'73796eff' AS TEXT)"))},
    {"id": "unrelated-filename-invalid-utf8-500", "files": files("a.gguf"),
     "store": sql(row("a.gguf", "2024-05-01T00:00:00Z"),
                  "INSERT INTO update_check VALUES(CAST(x'6eff' AS TEXT), 'r', NULL, 1)")},
    {"id": "filename-blob-and-int-keys", "files": files("a.gguf", "5"),
     "store": sql("INSERT INTO update_check VALUES(x'612e67677566', 'r', '2024-05-01Z', 1)",
                  "INSERT INTO update_check VALUES(5, 'r', '2024-05-01Z', 1)", schema=LOOSE)},
    {"id": "duplicate-rows-last-wins", "files": files("a.gguf"),
     "store": sql(row("a.gguf", "2020-01-01T00:00:00Z", checked="1"), row("a.gguf", "2030-01-01T00:00:00Z", checked="2"),
                  row("a.gguf", "2025-01-01T00:00:00+05:00", checked="3"), schema=LOOSE)},
    {"id": "checked-at-shapes", "files": files(*[f"c{i}.gguf" for i in range(12)]),
     "store": sql(*[row(f"c{i}.gguf", "2024-05-01T00:00:00Z", checked=c) for i, c in enumerate(
         ["0", "-0.0", "1e16", "1.5e-5", "123456789012345678", "0.1", "1e-7", "12345678901234567.0", "'text'",
          "NULL", "x'7574663862'", "9.999999999999999e15"])], schema=LOOSE)},
    {"id": "checked-at-inf", "files": files("a.gguf"), "store": sql(row("a.gguf", "2024-05-01T00:00:00Z", checked="9e999"))},
    {"id": "checked-at-blob-invalid", "files": files("a.gguf"),
     "store": sql(row("a.gguf", "2024-05-01T00:00:00Z", checked="x'ff'"), schema=LOOSE)},
    {"id": "checked-at-bad-unrelated", "files": files("a.gguf"),
     "store": sql(row("a.gguf", "2024-05-01T00:00:00Z"), row("b.gguf", "2024-05-01T00:00:00Z", checked="x'ff'"), schema=LOOSE)},
    {"id": "ini-missing", "files": files("a.gguf"), "store": sql(row("a.gguf", "2024-05-01T00:00:00Z"))},
    {"id": "ini-duplicate-sections-ok", "files": files("a.gguf"), "ini": {"text": "[a]\nx = 1\n[a]\ny = 2\n"},
     "store": sql(row("a.gguf", "2024-05-01T00:00:00Z"))},
    {"id": "ini-crlf-preamble-ok", "files": files("a.gguf"), "ini": {"text": "version = 1\r\n\r\n[a]\r\nmodel = a.gguf\r\n"},
     "store": sql(row("a.gguf", "2024-05-01T00:00:00Z"))},
    {"id": "ini-parse-error-500", "files": files("a.gguf"), "ini": {"text": "[a]\nno delimiter here\n"},
     "store": sql(row("a.gguf", "2024-05-01T00:00:00Z"))},
    {"id": "ini-missing-header-after-preamble", "files": files("a.gguf"), "ini": {"text": "[\nx = 1\n"},
     "store": sql(row("a.gguf", "2024-05-01T00:00:00Z"))},
    {"id": "ini-not-utf8-500", "files": files("a.gguf"), "ini": {"hex": "5b615d0a78203d20ff0a"},
     "store": sql(row("a.gguf", "2024-05-01T00:00:00Z"))},
    {"id": "ini-is-directory-500", "files": files("a.gguf"), "ini": "dir", "store": sql(row("a.gguf", "2024-05-01T00:00:00Z"))},
    # noevia#1873: Python skips an unreadable folder, follows absolute links that stay inside the
    # models folder, and walks a symlinked models folder.
    {"id": "unreadable-subfolder-skipped", "dirs": ["open", "locked", "locked-deeper/inner"],
     "files": files("open/a.gguf", "locked/b.gguf", "locked-deeper/inner/c.gguf", "top.gguf"),
     "modes": [["locked", 0], ["locked-deeper/inner", 0o300]],
     "store": sql(*[row(n, "2024-05-01T00:00:00Z") for n in ("a.gguf", "b.gguf", "c.gguf", "top.gguf")])},
    {"id": "absolute-links-inside", "dirs": ["sub", "subdir-target"],
     "files": [["sub/x.gguf", T0 + 7], ["sub/old.gguf", T0 - 9]],
     "abslinks": [["y.gguf", "sub/x.gguf"], ["sub/rel-abs.gguf", "sub/old.gguf"], ["gone.gguf", "sub/nothing.gguf"],
                  ["dir-link.gguf", "subdir-target"]],
     "store": sql(*[row(n, "2023-11-14T22:13:20Z") for n in ("x.gguf", "y.gguf", "old.gguf", "rel-abs.gguf",
                                                             "gone.gguf", "dir-link.gguf")])},
    {"id": "models-folder-is-a-symlink", "models": "link", "dirs": ["sub"],
     "files": [["a.gguf", T0], ["sub/b-00001-of-00002.gguf", T0 + 1], ["sub/b-00002-of-00002.gguf", T0 + 2]],
     "ini": {"text": "[a]\nmodel = a.gguf\n"},
     "store": sql(row("a.gguf", "2024-05-01T00:00:00Z"), row("b.gguf", "2020-05-01T00:00:00Z"))},
    {"id": "store-rollback-journal", "files": files("a.gguf"),
     "store": {"journal": "delete", "sql": [SCHEMA, row("a.gguf", "2024-05-01T00:00:00Z")]}},
]


def build(root: Path, case: dict) -> tuple[Path, Path, Path]:
    models = root / "models"
    ini_path = models / "models.ini"
    data = root / "data"
    kind = case.get("models", "dir")
    if kind == "file":
        models.write_bytes(b"")
    elif kind in ("dir", "link"):
        if kind == "link":
            real = root / "real-models"
            real.mkdir()
            os.symlink(real, models)
        else:
            models.mkdir()
        for d in case.get("dirs", []):
            (models / d).mkdir(parents=True, exist_ok=True)
        for name, _ in case.get("files", []):
            (models / name).parent.mkdir(parents=True, exist_ok=True)
            (models / name).write_bytes(b"")
        for name, target in case.get("links", []):
            os.symlink(target, models / name)
        for name, target in case.get("abslinks", []):
            os.symlink(models.resolve() / target, models / name)
        ini = case.get("ini")
        if ini == "dir":
            ini_path.mkdir()
        elif isinstance(ini, dict):
            ini_path.write_bytes(ini["text"].encode() if "text" in ini else bytes.fromhex(ini["hex"]))
        for name, ns in case.get("files", []):
            os.utime(models / name, ns=(ns, ns))
        for name, mode in case.get("modes", []):
            os.chmod(models / name, mode)
    store = case.get("store")
    if store is not None:
        data.mkdir()
        db_path = data / "model_loader.db"
        if "bytes" in store:
            db_path.write_bytes(bytes.fromhex(store["bytes"]))
        else:
            c = sqlite3.connect(db_path, isolation_level=None)
            c.execute(f"PRAGMA journal_mode={store['journal'].upper()}")
            for statement in store["sql"]:
                c.execute(statement)
            c.close()
    return models, ini_path, data


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith(("NOEVIA_NATIVE_", "MODELS_", "MODEL_FILES", "GGUF_")) \
                or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR"}:
            del os.environ[key]
    os.environ["TZ"] = "UTC"
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import config, db  # noqa: E402
    from app.main import app  # noqa: E402

    client = TestClient(app, raise_server_exceptions=False)  # no lifespan: db.init never runs
    names = ("api.py", "db.py", "main.py", "helpers.py", "services.py", "ini.py", "utils.py",
             "autoconfig_core.py", "discover.py")
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest() for name in names}
    out = []
    for case in CASES:
        with tempfile.TemporaryDirectory() as tmp:
            models, ini_path, data = build(Path(tmp), case)
            config.settings.models_dir = models
            config.settings.models_ini_path = ini_path
            config.settings.data_dir = data
            db._DB_PATH = None
            response = client.get("/api/v1/models/updates")
            for name, _ in case.get("modes", []):
                os.chmod(models / name, 0o700)
            out.append({**case, "status": response.status_code,
                        "content_type": response.headers.get("content-type"),
                        "body": response.content.decode("utf-8")})
    iso = [{"input": s, "parsed": parsed(s)} for s in dict.fromkeys(REMOTES + corpus())]
    json.dump({"oracle": digest, "cases": out, "fromisoformat": iso}, sys.stdout, ensure_ascii=True,
              indent=0, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
