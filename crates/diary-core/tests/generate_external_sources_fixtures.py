"""Record external-sources-fixtures.json from the frozen Python module diary/agent/external_sources.py.

Usage (CPython 3.12 only, isolated mode):

    <venv>/bin/python -I crates/diary-core/tests/generate_external_sources_fixtures.py <noevia-services>/diary

The real module (sha256 pinned, noevia-services origin/main 6d0ba0e) is loaded from its file and run
against synthetic data only: file names made up here, files of "x" bytes, trees built in a temporary
directory. Nothing is read from a real Diary. Records:

- infer_date for 170+ synthetic names (mtimes set with os.utime to noon UTC of a fixed day, so the
  local date is the same in any timezone from UTC-11 to UTC+11);
- the microsecond rounding of datetime.fromtimestamp on real st_mtime floats;
- the Unicode 15.0 decimal digit table (every code point `\\d` matches, with its int() value);
- scan_source and resolve_import_target on synthetic trees (spec: names, sizes, mtimes, symlinks),
  paths relative to the root, the temporary directory written as "{base}";
- external_source_paths for 40+ raw strings with fixed HOME values and recorded passwd lookups.
"""

import calendar
import hashlib
import importlib.util
import json
import os
import pathlib
import pwd
import random
import re
import shutil
import sys
import tempfile
import unicodedata
from datetime import datetime, timezone

sys.dont_write_bytecode = True

PINNED = {"agent/external_sources.py": "e1b8e730004e3bed84cee3928552bd64ccec0d39205a66868320e352f9dea1d7"}
OUT = pathlib.Path(__file__).with_name("external-sources-fixtures.json")

NOON = calendar.timegm((2025, 7, 15, 12, 0, 0))  # fixed default mtime, noon UTC
DAY = 86400


def noon(y, m, d):
    return calendar.timegm((y, m, d, 12, 0, 0))


def load(diary_dir):
    root = pathlib.Path(diary_dir).resolve()
    for rel, want in PINNED.items():
        got = hashlib.sha256((root / rel).read_bytes()).hexdigest()
        if got != want:
            sys.exit(f"refusing {rel}: sha256 {got} is not the pinned {want}")
    spec = importlib.util.spec_from_file_location("external_sources_under_test", root / "agent/external_sources.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def raised(exc):
    return {"error": type(exc).__name__}


# ---------------------------------------------------------------------------------------------
# infer_date
# ---------------------------------------------------------------------------------------------

def infer_names():
    names = []
    add = names.append
    # pattern 1, valid, many shapes
    for n in ["2026-03-04.md", "2026-3-4.md", "2026-03-4.md", "2026-3-04.md", "a2026-03-04.md",
              "journal 2026-03-04 evening.md", "2026-03-04", "x_2026-03-04_y.txt", "2026-03-04-2026-03-05.md",
              "2026-03-04.txt", "2026-3-4-5.md", "2026-12-31.markdown", "2026-1-1.md", "(2026-03-04).md",
              "a2026-03-04b.md", "2026-03-04b.md", "b2026-03-04.md", "entry.2026-03-04.md",
              "20260304-2026-05-06.md", "2026-05-06-20260304.md"]:
        add(n)
    # pattern 1 with impossible dates: fall to pattern 2, then mtime
    for n in ["2026-02-30.md", "2026-13-01.md", "2026-00-10.md", "2026-01-00.md", "2025-02-29.md",
              "1900-02-29.md", "0000-01-01.md", "2026-04-31.md", "2026-06-31.md", "2026-11-31.md",
              "2026-02-29.md", "0000-12-31.md", "2026-99-99.md", "2026-02-30-2026-03-01.md",
              "2026-02-30_20260301.md", "2026-13-45 and 20260102.md", "0000-01-01_00010101.md"]:
        add(n)
    # leap years and boundaries that are valid
    for n in ["2024-02-29.md", "2000-02-29.md", "0001-01-01.md", "9999-12-31.md", "0400-02-29.md"]:
        add(n)
    # pattern 2
    for n in ["20260304.md", "x20260304y.md", "journal_20260304.txt", "20260304", "20260304-notes.md",
              "20261301.md", "20260230.md", "00000101.md", "20260000.md", "20260100.md", "99991231.md",
              "20240229.md", "20250229.md", "00010101.md", "20260431.md", "(20260304).md"]:
        add(n)
    # 8-digit runs inside longer digit runs, and digits touching the pattern
    for n in ["120260304.md", "202603041.md", "2026030.md", "2026030400.md", "1234567890123.md",
              "12026-03-04.md", "2026-03-045.md", "2026-003-04.md", "2026-03-004.md", "20260304.5.md",
              "2026-03-04.5.md", "9-20260304-9.md", "123-2026-03-04-123.md", "2026--03-04.md",
              "2026-03--04.md", "2026-03-04--.md", "202603-04.md", "2026-0304.md", "20260-304.md"]:
        add(n)
    # unicode digits
    arabic = "٠١٢٣٤٥٦٧٨٩"
    fullwidth = "０１２３４５６７８９"
    deva = "०१२३४५६७८९"
    mathdbl = "".join(chr(0x1D7D8 + i) for i in range(10))
    for table in (arabic, fullwidth, deva, mathdbl):
        tr = {ord("0123456789"[i]): table[i] for i in range(10)}
        for base in ["2026-03-04.md", "20260304.md", "2026-3-4.md", "2026-02-30.md", "0000-01-01.md", "2026-13-01.md"]:
            add(base.translate(tr))
    for n in ["2026-٠٣-04.md", "٢٠٢٦-03-٠٤.md", "٢٠٢٦-٠٣-٠٤٠.md", "x٢٠٢٦٠٣٠٤.md", "٢٠٢٦٠٣٠٤٠٥.md",
              "²⁰²⁶-03-04.md", "①②③④-03-04.md", "2026-03-04٠.md", "٠" + "2026-03-04.md",
              "𝟚𝟘𝟚𝟞-𝟘𝟛-𝟘𝟜.md", "2026-0²3-04.md", "2026-٣-04.md"]:
        add(n)
    # newlines and other non-digits around a date
    for n in ["2026-03-04\n.md", "\n2026-03-04.md", "x\n20260304\n", "20260304\n", "a\n2026-3-4\n",
              "é2026-03-04é.md", "\U0001f6002026-03-04.md", "2026-03-04�.md"]:
        add(n)
    # no date at all: mtime
    for n in ["notes.md", "readme", "1.md", ".md", "a-b-c", "2026-03.md", "2026.md", "03-04.md",
              "2026_03_04.md", "2026.03.04.md", "26-03-04.md", "my 2026 notes 03 04", "Jan 5 2026.md",
              "2026-3.md", "20260.md", "2026 03 04.md", "-2026-03-04.md".replace("2026", "20x6"),
              "café.md", "١٢٣.md", "", ]:
        if n:
            add(n)
    # a systematic sweep of valid and invalid (y, m, d) in both patterns
    triples = [(2026, 3, 4), (2026, 12, 31), (2026, 1, 1), (2026, 2, 28), (2024, 2, 29), (2025, 2, 29),
               (2026, 4, 30), (2026, 4, 31), (2026, 0, 1), (2026, 1, 0), (2026, 13, 1), (2026, 2, 30),
               (1, 1, 1), (0, 1, 1), (9999, 12, 31), (1900, 2, 29), (2100, 2, 29), (2000, 2, 29), (1999, 12, 31)]
    for y, m, d in triples:
        add(f"{y:04d}-{m:02d}-{d:02d}.md")
        add(f"{y:04d}{m:02d}{d:02d}.md")
        add(f"log {y:04d}-{m}-{d} end.txt")
    out, seen = [], set()
    for n in names:
        if n not in seen:
            seen.add(n)
            out.append(n)
    return out


MTIMES = [NOON, noon(2024, 2, 29), noon(1999, 12, 31), noon(1970, 1, 2), noon(2038, 1, 19), noon(2100, 3, 1),
          noon(1969, 12, 31), noon(1901, 6, 15), noon(2026, 11, 13), noon(2262, 4, 10)]


def infer_cases(mod):
    base = pathlib.Path(tempfile.mkdtemp(prefix="r9-ext-infer-")).resolve()
    try:
        cases = []
        for i, name in enumerate(infer_names()):
            mt = MTIMES[i % len(MTIMES)]
            full = base / f"d{i}" / name
            case = {"name": name, "mtime": mt, "create": True}
            if "\x00" in name:
                case["create"] = False
            else:
                full.parent.mkdir()
                full.write_bytes(b"x")
                os.utime(full, (mt, mt))
            cases.append((case, full))
        # a name that does not exist and has no date: unavailable
        for name in ["missing-no-date.md", "missing.txt", "20261301-missing.md", "2026-13-01-missing"]:
            cases.append(({"name": name, "mtime": 0, "create": False}, base / "nonexistent-dir" / name))
        # NUL in the name: the stat raises ValueError (Rust reports "unavailable")
        cases.append(({"name": "no\x00date.md", "mtime": 0, "create": False}, pathlib.Path(str(base) + "/nul/no\x00date.md")))
        # mtimes Python cannot turn into a local date in any timezone (ValueError; Rust reports "unavailable")
        for tag, mt in (("year10000", 253402300800 + 2 * DAY), ("year0", -62135596800 - 2 * DAY), ("far-future", 2**40)):
            d = base / f"extreme-{tag}"
            d.mkdir()
            f = d / f"extreme-{tag}.md"
            f.write_bytes(b"x")
            os.utime(f, (mt, mt))
            cases.append(({"name": f.name, "mtime": mt, "create": True, "dir": d.name}, f))
        out = []
        for case, full in cases:
            try:
                date, src = mod.infer_date(full)
                case["expected"] = [date.isoformat() if date else None, src]
            except Exception as exc:  # noqa: BLE001
                case["expected"] = raised(exc)
            out.append(case)
        return out
    finally:
        shutil.rmtree(base)


def carry_cases():
    """The float st_mtime and datetime.fromtimestamp's microsecond rounding, on a real file."""
    base = pathlib.Path(tempfile.mkdtemp(prefix="r9-ext-carry-")).resolve()
    try:
        f = base / "t.md"
        f.write_bytes(b"x")
        rng = random.Random(9091)
        pairs = [(86399, 999999600), (86399, 999999499), (86399, 999999500), (86399, 999999999),
                 (86399, 999999501), (86399, 999999000), (0, 0), (0, 499), (0, 500), (0, 501), (1, 500),
                 (2, 1500), (1, 999999500), (-1, 999999500), (-1, 999999499), (-1, 0), (-2, 500000000),
                 (-86400, 999999600), (1752580800, 999999500), (1752580799, 999999999), (1752580800, 499),
                 (1752580800, 500), (1752580800, 1500), (1752580800, 2500), (4102444799, 999999999)]
        for _ in range(60):
            sec = rng.choice([rng.randrange(0, 4_000_000_000), rng.randrange(-2_000_000_000, 0)])
            nsec = rng.choice([rng.randrange(0, 1_000_000_000), 999_999_000 + rng.randrange(0, 1000),
                               rng.randrange(0, 2000)])
            pairs.append((sec, nsec))
        out = []
        for sec, nsec in pairs:
            os.utime(f, ns=(sec * 10**9 + nsec, sec * 10**9 + nsec))
            st = os.stat(f)
            if st.st_mtime_ns != sec * 10**9 + nsec:
                continue  # filesystem did not keep the nanoseconds
            if st.st_mtime != sec + 1e-9 * nsec:
                sys.exit(f"st_mtime is not sec + 1e-9*nsec for {sec},{nsec}")
            dt = datetime.fromtimestamp(st.st_mtime, timezone.utc)
            out.append({"sec": sec, "nsec": nsec, "epoch": calendar.timegm(dt.utctimetuple())})
        return out
    finally:
        shutil.rmtree(base)


def digit_table():
    rx = re.compile(r"\d")
    table = []
    for cp in range(0x110000):
        c = chr(cp)
        m = rx.fullmatch(c) is not None
        if m != (unicodedata.category(c) == "Nd"):
            sys.exit(f"\\d and Nd disagree at {cp:#x}")
        if m:
            table.append([cp, int(c)])
    return table


# ---------------------------------------------------------------------------------------------
# synthetic trees
# ---------------------------------------------------------------------------------------------

def expand(nodes):
    out = []
    for n in nodes:
        if n[0] == "gen":  # ["gen", dir, prefix, suffix, count, width, size, mtime0]
            _, d, prefix, suffix, count, width, size, mt = n
            for i in range(count):
                name = f"{prefix}{i:0{width}d}{suffix}"
                out.append(["file", f"{d}/{name}" if d else name, size, mt + DAY * (i % 9)])
        elif n[0] == "genlink":  # ["genlink", dir, prefix, suffix, count, width, target]
            _, d, prefix, suffix, count, width, target = n
            for i in range(count):
                name = f"{prefix}{i:0{width}d}{suffix}"
                out.append(["symlink", f"{d}/{name}" if d else name, target])
        else:
            out.append(n)
    return out


def node_path(base, n):
    raw = bytes.fromhex(n[1]) if n[0].endswith("_b") else os.fsencode(n[1])
    return os.path.join(os.fsencode(str(base)), raw)


def build(base, nodes, mtime_default=NOON):
    for n in expand(nodes):
        kind = n[0]
        p = node_path(base, n)
        if kind in ("dir", "dir_b"):
            os.makedirs(p, exist_ok=True)
        elif kind in ("file", "file_b"):
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "wb") as fh:
                fh.write(b"x" * n[2])
            os.utime(p, (n[3], n[3]))
        elif kind == "symlink":
            os.makedirs(os.path.dirname(p), exist_ok=True)
            os.symlink(n[2].replace("{base}", str(base)), p)
        else:
            raise SystemExit(f"unknown node {n}")


def lossy(s):
    return os.fsencode(s).decode("utf-8", "replace")


def clean_scan(result, base):
    out = dict(result)
    out["path"] = out["path"].replace(str(base), "{base}")
    files = []
    for f in out["files"]:
        files.append({"name": lossy(f["name"]), "rel_path": lossy(f["rel_path"]), "size": f["size"],
                      "date": f["date"], "date_source": f["date_source"]})
    out["files"] = files
    return out


def rel_to_base(p, base):
    s = os.path.relpath(str(p), str(base))
    if s.startswith(".."):
        sys.exit(f"resolved path {p} left the base {base}")
    return s


def run_tree(mod, name, nodes, scan_sources, resolves):
    base = pathlib.Path(tempfile.mkdtemp(prefix="r9-ext-tree-")).resolve()
    try:
        build(base, nodes)
        scans = []
        for src in scan_sources:
            real = src.replace("{base}", str(base))
            try:
                scans.append({"source": src, "expected": clean_scan(mod.scan_source(real), base)})
            except Exception as exc:  # noqa: BLE001
                scans.append({"source": src, "expected": raised(exc)})
        res = []
        for src, rel in resolves:
            real = src.replace("{base}", str(base))
            rel_real = rel.replace("{base}", str(base))
            try:
                got = mod.resolve_import_target(real, rel_real)
                expected = {"path": rel_to_base(got, base)} if got is not None else None
            except Exception as exc:  # noqa: BLE001
                expected = raised(exc)
            res.append({"source": src, "rel": rel, "expected": expected})
        return {"name": name, "nodes": nodes, "scans": scans, "resolves": res}
    finally:
        shutil.rmtree(base)


def main_tree(mod):
    d = lambda y, m, dd: noon(y, m, dd)  # noqa: E731
    nodes = [
        ["dir", "root"], ["dir", "outside"],
        ["file", "outside/secret.md", 33, d(2025, 5, 5)],
        ["file", "outside/other.txt", 7, d(2025, 5, 6)],
        ["file", "root/2026-03-04.md", 120, d(2025, 6, 1)],
        ["file", "root/2026-3-5.txt", 5, d(2025, 6, 2)],
        ["file", "root/20260306.markdown", 6, d(2025, 6, 3)],
        ["file", "root/note-2026-02-30.md", 7, d(2025, 6, 4)],
        ["file", "root/20261301.md", 8, d(2025, 6, 5)],
        ["file", "root/UPPER-2026-04-01.MD", 9, d(2025, 6, 6)],
        ["file", "root/Mixed.TxT", 10, d(2025, 6, 7)],
        ["file", "root/x.MARKDOWN", 11, d(2025, 6, 8)],
        ["file", "root/a.marKdown", 12, d(2025, 6, 9)],
        ["file", "root/a.marKDOWN", 13, d(2025, 6, 9)],
        ["file", "root/.hidden.md", 14, d(2025, 6, 10)],
        ["file", "root/.md", 15, d(2025, 6, 11)],
        ["file", "root/.txt", 16, d(2025, 6, 11)],
        ["file", "root/.gitignore", 17, d(2025, 6, 11)],
        ["file", "root/..md", 18, d(2025, 6, 12)],
        ["file", "root/noext", 19, d(2025, 6, 13)],
        ["file", "root/trailing.", 20, d(2025, 6, 13)],
        ["file", "root/x.md.", 21, d(2025, 6, 13)],
        ["file", "root/x.txt.bak", 22, d(2025, 6, 13)],
        ["file", "root/x.md ", 23, d(2025, 6, 13)],
        ["file", "root/bin.dat", 24, d(2025, 6, 14)],
        ["file", "root/archive.tar.md", 25, d(2025, 6, 15)],
        ["file", "root/café-2026-05-05.md", 26, d(2025, 6, 16)],
        ["file", "root/٢٠٢٦-٠٣-٠٤.md", 27, d(2025, 6, 17)],
        ["file", "root/bad\nname-2026-03-04.md", 28, d(2025, 6, 18)],
        ["file", "root/a b.md", 29, d(2025, 6, 19)],
        ["file", "root/Z.md", 1, d(2025, 6, 20)],
        ["file", "root/z.md", 2, d(2025, 6, 20)],
        ["file", "root/_u.md", 3, d(2025, 6, 20)],
        ["file", "root/é.md", 4, d(2025, 6, 20)],
        ["file", "root/é.md", 5, d(2025, 6, 20)],
        ["file", "root/.md", 6, d(2025, 6, 20)],
        ["file", "root/\U0001f600.md", 7, d(2025, 6, 20)],
        ["file", "root/퟿.md", 8, d(2025, 6, 20)],
        ["file", "root/￮.md", 9, d(2025, 6, 20)],
        ["file", "root/0empty.md", 0, d(2025, 6, 20)],
        ["file", "root/2025/jan/deep/2025-01-05.md", 40, d(2025, 1, 6)],
        ["file", "root/2025/jan/x.md", 41, d(2025, 1, 7)],
        ["file", "root/2025/jan/20250109.txt", 42, d(2025, 1, 8)],
        ["file", "root/2025/readme.markdown", 43, d(2025, 2, 8)],
        ["dir", "root/empty"],
        ["file", "root/only-bin/x.bin", 3, d(2025, 6, 6)],
        ["file", "root/.git/notes.md", 4, d(2025, 3, 3)],
        ["file", "root/zeta/z1.md", 5, d(2025, 3, 4)],
        ["file", "root/Alpha/a1.md", 6, d(2025, 3, 5)],
        ["file", "root/sub dir with spaces/a b.md", 7, d(2025, 3, 6)],
        ["dir", "root/dir.md"],
        ["file", "root/dir.md/inside.md", 8, d(2025, 3, 7)],
        ["symlink", "root/link-in.md", "2026-03-04.md"],
        ["symlink", "root/link-dir", "2025"],
        ["symlink", "root/link-dir.md", "2025"],
        ["symlink", "root/broken.md", "nonexistent.md"],
        ["symlink", "root/escape.md", "../outside/secret.md"],
        ["symlink", "root/abs-escape.md", "{base}/outside/secret.md"],
        ["symlink", "root/escape-dir", "../outside"],
        ["symlink", "root/loop-a.md", "loop-b.md"],
        ["symlink", "root/loop-b.md", "loop-a.md"],
        ["symlink", "root/selfloop", "selfloop"],
        ["symlink", "root/link-bin.md", "bin.dat"],
        ["symlink", "root/link-to-md", "2026-03-04.md"],
        ["symlink", "root/link-chain.md", "link-in.md"],
        ["symlink", "root/up-and-back.md", "2025/../2026-3-5.txt"],
        ["symlink", "root/2025/jan/rel-up.md", "../../2026-03-04.md"],
        ["symlink", "rootlink", "root"],
        ["symlink", "root-abs-link", "{base}/root"],
    ]
    scans = ["{base}/root", "{base}/root/", "{base}/./root", "{base}/root/../root", "{base}//root",
             "/{base}/root", "{base}/rootlink", "{base}/root-abs-link", "{base}/root/2025",
             "{base}/root/2026-03-04.md", "{base}/nonexistent", "{base}/root/loop-a.md",
             "{base}/root/broken.md", "{base}/root/link-dir", "{base}/root/2026-03-04.md/x",
             "{base}/root/empty", "{base}/root/only-bin", "{base}/root\x00", "{base}/root/dir.md",
             "{base}/outside", "{base}/root/zeta/..", "{base}/root/selfloop"]
    rels = ["2026-03-04.md", "2026-3-5.txt", "20260306.markdown", "UPPER-2026-04-01.MD", "Mixed.TxT",
            "x.MARKDOWN", "a.marKdown", ".hidden.md", ".md", ".txt", ".gitignore", "..md", "noext",
            "trailing.", "x.md.", "x.txt.bak", "x.md ", "bin.dat", "archive.tar.md",
            "café-2026-05-05.md", "bad\nname-2026-03-04.md", "a b.md", "2025/jan/deep/2025-01-05.md",
            "2025/jan/x.md", "2025/jan/20250109.txt", "2025", "2025/", "empty", "only-bin/x.bin",
            ".git/notes.md", "sub dir with spaces/a b.md", "dir.md", "dir.md/inside.md",
            "link-in.md", "link-dir", "link-dir.md", "link-dir/jan/x.md", "link-dir.md/jan/x.md",
            "broken.md", "escape.md", "abs-escape.md", "escape-dir", "escape-dir/secret.md",
            "loop-a.md", "loop-b.md", "loop-a.md/x.md", "selfloop", "selfloop/x.md", "link-bin.md",
            "link-to-md", "link-chain.md", "up-and-back.md", "2025/jan/rel-up.md",
            "nonexistent.md", "nonexistent/x.md", "2026-03-04.md/x.md",
            # shapes Path() normalises
            "./2026-03-04.md", ".//2026-03-04.md", "2025//jan/./x.md", "2026-03-04.md/",
            "2026-03-04.md/.", "2025/./jan/x.md", "./", ".",
            # refused outright
            "", "..", "../outside/secret.md", "2025/../2026-03-04.md", "link-dir/../2026-03-04.md",
            "2025/jan/../jan/x.md", "/etc/passwd", "/{base}/root/2026-03-04.md", "//2026-03-04.md",
            "a\x00.md", "2026-03-04.md\x00", "..hidden", "...", "2025/.../x.md"]
    resolves = [("{base}/root", r) for r in rels]
    for src in ["{base}/rootlink", "{base}/root/", "{base}/root-abs-link", "{base}/root/2025",
                "{base}/root/../root", "{base}/root/link-dir", "{base}/./root//", "{base}/outside",
                "{base}/root/2025/jan", "{base}/nonexistent", "{base}/root/loop-a.md", "{base}/root\x00"]:
        for r in ["2026-03-04.md", "x.md", "jan/x.md", "link-in.md", "escape.md", "secret.md", "../2026-03-04.md",
                  "rel-up.md", "link-dir/jan/x.md", "deep/2025-01-05.md", "nonexistent.md", "a/b"]:
            resolves.append((src, r))
    resolves += [("", "2026-03-04.md"), ("{base}/root", ""), ("", ""), ("root-relative-that-does-not-exist", "a.md"),
                 ("{base}/root/2026-03-04.md", "x.md"), ("{base}/root/2026-03-04.md", "."),
                 ("//{base}/root", "2026-03-04.md")]
    return run_tree(mod, "main", nodes, scans, resolves)


def cap_trees(mod):
    bins_top = [["gen", "root", "zz", ".bin", 10, 3, 1, NOON]]
    trees = []

    def tree(name, nodes, resolve=()):
        trees.append(run_tree(mod, name, [["dir", "root"]] + nodes, ["{base}/root"], list(resolve)))

    # 300 top files then one sub directory: cut inside "sub" after 200 of 250 (sorted by name)
    tree("cap-sub-cut", [["gen", "root", "n", ".md", 300, 3, 10, NOON], ["gen", "root/sub", "s", ".txt", 250, 3, 11, NOON + DAY * 3],
                         ["gen", "root/sub", "t", ".bin", 5, 2, 1, NOON]] + bins_top)
    # exactly 500 allowed files plus other extensions: not truncated
    tree("cap-exact-500", [["gen", "root", "a", ".md", 500, 3, 5, NOON]] + bins_top)
    # 500 allowed files and a sub directory that holds nothing allowed: not truncated
    tree("cap-500-then-bin-dir", [["gen", "root", "a", ".md", 500, 3, 5, NOON], ["gen", "root/sub", "b", ".bin", 4, 2, 1, NOON],
                                  ["dir", "root/sub/deeper"]])
    # 500 allowed files and one allowed file in a sub directory: truncated
    tree("cap-500-then-one-in-sub", [["gen", "root", "a", ".md", 500, 3, 5, NOON], ["file", "root/sub/late.md", 5, NOON]])
    # 501 allowed files: the last name in sort order is dropped
    tree("cap-501", [["gen", "root", "e", ".md", 501, 3, 6, NOON], ["gen", "root", "e", ".bin", 20, 3, 1, NOON]])
    # mixed case and extension spellings, 501 files
    tree("cap-501-mixed", [["gen", "root", "m", ".MD", 200, 3, 6, NOON], ["gen", "root", "n", ".Txt", 200, 3, 6, NOON + DAY],
                           ["gen", "root", "o", ".markdown", 101, 3, 6, NOON + 2 * DAY]])
    # a chain of single directories, the cut lands in the deepest one
    tree("cap-chain", [["gen", "root", "t", ".md", 100, 3, 3, NOON], ["gen", "root/p", "p", ".md", 100, 3, 4, NOON + DAY],
                       ["gen", "root/p/q", "q", ".md", 400, 3, 5, NOON + 2 * DAY]])
    # broken links sorted first are skipped by the stat and do not count: 500 files, not truncated
    tree("cap-broken-links-first", [["genlink", "root", "0-broken", ".md", 5, 2, "nonexistent-target.md"],
                                    ["gen", "root", "a", ".md", 500, 3, 5, NOON]])
    # a broken link sorted AFTER 500 good files still sets truncated: the cap is checked before the stat
    tree("cap-broken-link-last", [["gen", "root", "a", ".md", 500, 3, 5, NOON], ["symlink", "root/zz-broken.md", "nowhere.md"]])
    # links to directories (named like notes) are directories for os.walk and never count
    tree("cap-dir-links-last", [["gen", "root", "a", ".md", 500, 3, 5, NOON], ["dir", "root/real"],
                                ["symlink", "root/zz-dirlink.md", "real"], ["symlink", "root/zz-dirlink2", "real"]])
    # links to files count like files
    tree("cap-file-links", [["gen", "root", "a", ".md", 499, 3, 5, NOON], ["symlink", "root/b-link.md", "a000.md"],
                            ["symlink", "root/c-link.md", "a001.md"]])
    return trees


def unicode_tree(mod):
    same = noon(2025, 8, 8)
    names = ["é.md", "é.md", "z.md", "Z.md", "_u.md", "0.md", ".md", "\U0001f600.md",
             "￮.md", "퟿.md", "a b.md", "a-b.md", "a.b.md", "a\nb.md", "K.md", "K.md", "k.md"]
    nodes = [["dir", "root"]]
    for n in names:
        nodes.append(["file", f"root/{n}", 1, same])
    for dn in ["édir", "Zdir", "zdir", "\U0001f600dir", "dir", "퟿dir"]:
        nodes.append(["file", f"root/{dn}/x.md", 2, same])
        nodes.append(["file", f"root/{dn}/y.txt", 2, same])
    # file and directory names that are not UTF-8 (Python's surrogateescape): sort as U+DC80..U+DCFF
    nodes.append(["file_b", ("bad" + "ÿ").encode("latin-1").hex() + (b".md").hex(), 3, same])
    nodes.append(["file_b", b"\xff\xfe.txt".hex(), 3, same])
    nodes.append(["file_b", b"\x80.md".hex(), 3, same])
    nodes.append(["file_b", (b"dir\x80/inner.md").hex(), 3, same])
    nodes.append(["file_b", (b"\xffdir/inner.md").hex(), 3, same])
    return run_tree(mod, "unicode-order", nodes, ["{base}/root"], [("{base}/root", n) for n in names[:6]])


# ---------------------------------------------------------------------------------------------
# external_source_paths
# ---------------------------------------------------------------------------------------------

def source_path_cases(mod):
    raws = ["", " ", ",", ",,", "/a", "/a,/b", " /a , /b ", "/a,/a", "/a,/a/", "/a/,/a", "/a//b,/a/b",
            "/a/./b", "~", "~/x", "~/", "~/x/", " ~/x ", "~root", "~root/y", "~nosuchuser-qa", "~nosuchuser-qa/x",
            "./~x", "~x/../y", "rel/path", "./rel", "rel/../x", "/", "//", "///x", "//x", "/x/..",
            "\t/a\n", " /a ", "　/a", "\x1c/a\x1f", "​/a", "/a,,/b", "~,~", "~/a,~/a/",
            "/a,~/b,/a", " /a ", "\x85/a\x0b", "﻿/a", "/a᠎", "~root/", "x/~y", "/~z", "~ ",
            "/é/中,/É", "a,b,c", "a/,a"]
    users = {}
    for u in ("root",):
        try:
            users[u] = pwd.getpwnam(u).pw_dir
        except KeyError:
            pass
    try:
        current = pwd.getpwuid(os.getuid()).pw_dir
    except KeyError:
        current = None
    homes = ["/home/qa", "/home/qa/", "", "/", "//", "//h", "///h", "rel/home", ".", "~foo", "/h/./x", None]
    cases = []
    for raw in raws:
        for home in (["/home/qa"] if raw not in ("~", "~/x", "~/", "~,~", "~/a,~/a/", "/a,~/b,/a", "~ ") else homes):
            if home is None:
                os.environ.pop("HOME", None)
            else:
                os.environ["HOME"] = home
            try:
                got = mod.external_source_paths({"DIARY_EXTERNAL_SOURCES": raw})
                expected = got
            except Exception as exc:  # noqa: BLE001
                expected = raised(exc)
            cases.append({"raw": raw, "home": home, "expected": expected})
    return {"users": users, "current_home": current, "cases": cases}


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    if sys.version_info[:2] != (3, 12):
        sys.exit("record with CPython 3.12, the Diary image's interpreter")
    if unicodedata.unidata_version != "15.0.0":
        sys.exit("record with Unicode 15.0.0")
    if sys.getfilesystemencoding().lower() != "utf-8" or sys.getfilesystemencodeerrors() != "surrogateescape":
        sys.exit("need a UTF-8 surrogateescape filesystem encoding")
    mod = load(sys.argv[1])
    infer = infer_cases(mod)
    if len(infer) < 150:
        sys.exit(f"only {len(infer)} infer_date cases")
    trees = [main_tree(mod)] + cap_trees(mod) + [unicode_tree(mod)]
    data = {
        "source": "noevia-services diary/agent/external_sources.py at origin/main 6d0ba0e",
        "python": sys.version.split()[0],
        "pinned_sha256": PINNED,
        "digits": digit_table(),
        "infer_date": infer,
        "carry": carry_cases(),
        "trees": trees,
        "source_paths": source_path_cases(mod),
    }
    OUT.write_text(json.dumps(data, ensure_ascii=True, separators=(",", ":"), sort_keys=True) + "\n", encoding="utf-8")
    n_scan = sum(len(t["scans"]) for t in trees)
    n_res = sum(len(t["resolves"]) for t in trees)
    print(f"wrote {OUT}: {len(infer)} infer_date, {len(data['carry'])} carry, {len(trees)} trees, "
          f"{n_scan} scans, {n_res} resolves, {len(data['source_paths']['cases'])} source_paths")


if __name__ == "__main__":
    main()
