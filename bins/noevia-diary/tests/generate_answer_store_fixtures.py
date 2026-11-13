"""Record answer-store-fixtures.json from the frozen Python Diary store (the corpus side of the answering exchange).

Usage (CPython 3.12 only, the Diary requirements installed; the import writes only under a temp dir):

    <venv>/bin/python -I bins/noevia-diary/tests/generate_answer_store_fixtures.py <noevia-services>/diary

The real CorpusStore (get_day_text, log_exchange, apply_pending), ManagedCorpusBackend, Journal and
workspace_files.memory_text (sha256 pinned, noevia-services origin/main 6d0ba0e) run over synthetic managed
tenants in a temp dir. Each case records what the Python code returned or left behind:

- day_text: get_day_text(day, max_chars, include_markers=False) and, for the unchanged /api/day form,
  include_markers=True, for a grid of days x caps;
- memory_text: workspace_files.memory_text(store) over synthetic workspace trees;
- log_exchange: the xid, every stored file and every journal row after CorpusStore.log_exchange on a fresh
  tenant (scaffold) and on existing ones, plus transient failures that leave the row pending.

Synthetic text only. uuid.uuid4 is replaced by a counter so the xids are reproducible; the Rust test maps its
own random xid onto the recorded one.
"""

import base64
import hashlib
import json
import os
import pathlib
import sqlite3
import sys
import tempfile
import uuid
from datetime import date, datetime

if sys.version_info[:2] != (3, 12):
    sys.exit("refusing: CPython 3.12 only, got %d.%d" % sys.version_info[:2])
sys.dont_write_bytecode = True

PINNED = {
    "agent/__init__.py": "afa92626fa9d1039db1839868f63ef800585753b890630396696852a68f2f26c",
    "agent/backup_effect.py": "357d252405882a808de5bb802491a583df9b1517c87d38e95f086602f673a2d9",
    "agent/config.py": "9953bcd8ca05a41c0554559f332af0b07e93816262d4f93aeb8f70e4ec994877",
    "agent/corpus.py": "b95b9fc5e6fd9758cfffa8c88f5f9d5cad042a258f193f20c6edfe4928bff2bb",
    "agent/corpus_parse.py": "939c21012f58fc16917df174f00de776502d417e8f7e23139630bc968b5a6540",
    "agent/corpus_store.py": "d486238d18a2d1a1baf231a4de097224e440cb7d8484f432d857409706686464",
    "agent/corpus_write_native.py": "b478fe16d04765b2112560e02488aae74e9f83ef7e342eee96c7d4d1c1b9a114",
    "agent/dedicated_storage.py": "f9d5fd3f25c28fa0c7db009245f9eea37fd8fa4d96a83da9c2dc5e2e416f8a67",
    "agent/journal.py": "9cfc69c14a95421874c00b1af9303c469a5ac71dbb94bb6086b6cf56d6139bdf",
    "agent/managed_storage.py": "3d02f83d2d4659f947cd02b1dfa00355fb6cb5b7d2581093a5cb41aec0a1575b",
    "agent/storage.py": "27be5fee7d73167c339b67ed72ead172fb427451f797d716cd718ec548d160b3",
    "agent/storage_backoff.py": "dbbd51182c6d00b3416274f6461e227c2de424cc4b37673bd833b9c8a9574655",
    "agent/tenant_writer.py": "c218b1a602b720c4c3a1f2efe62b109715507a7496e9f0161745aa59823e5f3c",
    "agent/workspace_files.py": "243767a922b73d0308aab17dd41eca30beb5e1c50a1f70a40e940f027030ef07",
}
OUT = pathlib.Path(__file__).with_name("answer-store-fixtures.json")
TENANT = "00000000-0000-4000-8000-0000000000b7"  # synthetic
FIXED_UUID = "00000000-0000-4000-8000-%012d"


def check_loaded(root):
    """Every executed file of the agent package is pinned: nothing outside the list may have been loaded."""
    loaded = sorted(
        str(pathlib.Path(m.__file__).resolve().relative_to(root))
        for m in list(sys.modules.values())
        if getattr(m, "__file__", None) and str(pathlib.Path(m.__file__).resolve()).startswith(str(root) + os.sep)
    )
    unpinned = [rel for rel in loaded if rel not in PINNED]
    if unpinned:
        sys.exit("refusing: executed but not pinned: %s" % unpinned)


def load(diary_dir):
    root = pathlib.Path(diary_dir).resolve()
    for rel, want in PINNED.items():
        got = hashlib.sha256((root / rel).read_bytes()).hexdigest()
        if got != want:
            sys.exit(f"refusing {rel}: sha256 {got} is not the pinned {want}")
    tmp = tempfile.mkdtemp(prefix="qa-r9-answer-store-")
    os.environ["DB_PATH"] = str(pathlib.Path(tmp) / "data" / "index.db")
    os.environ["CORPUS_LOCAL_ROOT"] = str(pathlib.Path(tmp) / "data" / "corpus")
    for key in ("DIARY_CORPUS_PARSE_IMPL", "DIARY_CORPUS_WRITE_IMPL", "DIARY_RUST_ROUTES", "DIARY_RUST_WRITES"):
        os.environ.pop(key, None)
    sys.path.insert(0, str(root))
    from agent import corpus as fmt  # noqa: E402
    from agent import corpus_store, journal, managed_storage, workspace_files  # noqa: E402
    from agent.config import Config  # noqa: E402
    check_loaded(root)
    return pathlib.Path(tmp), fmt, corpus_store, journal, managed_storage, workspace_files, Config


# ---------------------------------------------------------------- spec helpers


def data_of(spec):
    """A file's bytes from its fixture form: a string, {"repeat": [unit, count]} or {"b64": ...}."""
    if isinstance(spec, str):
        return spec.encode("utf-8")
    if "repeat" in spec:
        unit, count = spec["repeat"]
        return (unit * count).encode("utf-8")
    return base64.b64decode(spec["b64"])


def text_of(data):
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        return {"b64": base64.b64encode(data).decode("ascii")}


class Counter:
    n = 0


def counted_uuid4():
    Counter.n += 1
    return uuid.UUID(FIXED_UUID % Counter.n)


class Run:
    """One synthetic managed tenant under the real Python store."""

    def __init__(self, mods, tmp, name, settings, tree, journal_rows=()):
        self.tmp, self.fmt, corpus_store, journal, managed_storage, self.workspace_files, Config = mods
        self.root = tmp / name / "users" / TENANT
        self.managed = managed_storage.ManagedCorpusBackend(self.root, TENANT)
        self.db = self.root / "managed-diary.db"
        for path, spec in tree.get("files", []):
            ok = self.managed.put(path, data_of(spec))[0]
            assert ok, path
        raw = sqlite3.connect(self.db)
        try:
            for path, spec in tree.get("raw", []):
                data = data_of(spec)
                raw.execute("INSERT OR REPLACE INTO files VALUES (?,?,?,?)",
                            (path, data, hashlib.sha256(data).hexdigest(), 1.0))
            for path in tree.get("dirs", []):
                raw.execute("INSERT INTO directories VALUES (?)", (path,))
            raw.commit()
        finally:
            raw.close()
        self.index_db = self.root / "managed-index.db"
        self.journal = journal.Journal(self.index_db)
        for kind, payload, applied in journal_rows:
            jid = self.journal.enqueue(kind, payload)
            if applied:
                self.journal.mark_applied(jid)
        corpus = {"root": ""}
        corpus.update(settings)
        self.store = corpus_store.CorpusStore(Config({"corpus": corpus}), self.managed, self.journal)

    def break_files_table(self):
        raw = sqlite3.connect(self.db)
        try:
            raw.execute("ALTER TABLE files RENAME TO files_gone")
            raw.commit()
        finally:
            raw.close()

    def files_after(self):
        raw = sqlite3.connect(self.db)
        try:
            rows = raw.execute("SELECT path, data FROM files ORDER BY path").fetchall()
        finally:
            raw.close()
        return {path: text_of(bytes(data)) for path, data in rows}

    def journal_after(self):
        raw = sqlite3.connect(self.index_db)
        try:
            rows = raw.execute("SELECT kind, payload, applied, attempts, last_error FROM journal ORDER BY rowid").fetchall()
        finally:
            raw.close()
        return [{"kind": k, "payload": p, "applied": a, "attempts": n, "last_error": e} for k, p, a, n, e in rows]

    def close(self):
        self.journal.close()
        self.managed.close()


# ---------------------------------------------------------------- (a) get_day_text

A_SUB = "### 09:05 — Morning walk"
MONTH_JAN = "\n".join([
    "# January 2030 (synthetic)",
    "",
    "stray text before the first day is ignored",
    "",
    "## Sunday, January 6, 2030",
    "",
    "### 09:05 — Morning walk",
    "",
    "**Me:** I walked to the canal and watched the barges.",
    "",
    "**Assistant:** The user walked to the canal and watched the barges.",
    "",
    "<!-- xid:00000000-0000-4000-8000-00000000a001 -->",
    "",
    "### 14:32",
    "",
    "**Me:** Only a note from me, no reply was saved.",
    "",
    "<!-- xid:00000000-0000-4000-8000-00000000a002 -->",
    "",
    "**Me:** A second question with ünïcode, an emoji \U0001F33F and a multi-line",
    "body that keeps going.",
    "",
    "**Claude:** Legacy label for the reply.",
    "",
    "<!--   xid:00000000-0000-4000-8000-00000000a003   -->",
    "",
    "stray tail attached to the last exchange",
    "",
    "### 18:00 — Evening",
    "",
    "**Assistant:** A reply with no question.",
    "",
    "**Me:** Question without a marker.",
    "",
    "**Assistant:** Answer without a marker.",
    "",
    "## Monday, January 7, 2030",
    "",
    "### 07:10 — Next day",
    "",
    "**Me:** Monday note.",
    "",
    "**Assistant:** Monday reply.",
    "",
    "<!-- xid:00000000-0000-4000-8000-00000000a004 -->",
    "",
    "## Tuesday, January 8, 2030",
    "",
    "## Wednesday, January 9, 2030",
    "",
    "### 10:00 — Empty subsection",
    "",
    "## Sunday, January 6, 2030",
    "",
    "### 23:59 — Duplicate day header (the first one wins)",
    "",
    "**Me:** Never shown.",
    "",
])
LONG_DAY = "\n".join(
    ["## Thursday, January 3, 2030", ""]
    + [
        line
        for i in range(30)
        for line in (
            f"### {8 + i % 12:02d}:{i:02d} — Topic {i} \U0001F33F",
            "",
            f"**Me:** Question {i} " + "éö" * 20 + " \U0001F33F" * 5,
            "",
            f"**Assistant:** Answer {i} " + "word " * 25,
            "",
            f"<!-- xid:00000000-0000-4000-8000-0000000b{i:04d} -->",
            "",
        )
    ]
)
MONTH_JAN_LONG = MONTH_JAN + "\n" + LONG_DAY
CRLF_MONTH = "## Friday, January 4, 2030\r\n\r\n### 10:00 — Windows\r\n\r\n**Me:** CRLF question\r\n\r\n**Assistant:** CRLF answer\r\n\r\n<!-- xid:00000000-0000-4000-8000-00000000a005 -->\r\n"
MONTH_NODOW = "## January 5, 2030\n\n### 10:00 — Header without a weekday\n\n**Me:** Not the canonical header.\n"
MONTH_NOSUB = "## Saturday, January 5, 2030\n\nloose text under a day with no subsection\n"
CAPS = [0, 1, 7, 60, 200, 800, 12000, 1000000, -1, -7, -100000]
DAYS = ["2030-01-06", "2030-01-07", "2030-01-08", "2030-01-09", "2030-01-05", "2030-01-04", "2030-01-03", "2030-02-01"]


def daily_files(month_text, layout_dir="Entries"):
    """The same text stored as each day's own document (the daily layout reads a day's document)."""
    files = []
    for day in DAYS:
        d = date.fromisoformat(day)
        files.append((f"{layout_dir}/{d.year}/{d.strftime('%B')}/{d.strftime('%B')} {d.day}, {d.year}.md", month_text))
    return files


def day_cases(mods, tmp):
    fmt = mods[1]
    layouts = [
        ("monthly", {"entry_layout": "monthly", "index_enabled": False, "month_file_template": "{year}-{month02}.md"},
         lambda text: [("2030-01.md", text)]),
        ("monthly-named-template",
         {"entry_layout": "monthly", "index_enabled": False, "monthly_prefix": "Journal", "month_file_template": "Diary - {month_name} {year}.md"},
         lambda text: [("Journal/Diary - January 2030.md", text)]),
        ("daily", {"entry_layout": "daily", "index_enabled": False, "entries_prefix": "Logs"},
         lambda text: daily_files(text, "Logs")),
    ]
    months = [
        ("month-with-markers", MONTH_JAN),
        ("month-long-day", MONTH_JAN_LONG),
        ("crlf", CRLF_MONTH),
        ("no-weekday-header", MONTH_NODOW),
        ("day-without-subsection", MONTH_NOSUB),
        ("empty-file", ""),
        ("whitespace-file", "  \n\n"),
    ]
    cases = []
    for layout, settings, place in layouts:
        for label, text in months:
            tree = {"files": place(text)}
            run = Run(mods, tmp, f"day-{layout}-{label}", settings, tree)
            queries = []
            # The full grid for the first layout and the two main months; a thinner one elsewhere.
            full = layout == "monthly" and label in ("month-with-markers", "month-long-day")
            days = DAYS if full else ["2030-01-06", "2030-01-05", "2030-01-04", "2030-01-03", "2030-02-01"]
            caps = CAPS if full else [0, 7, 60, 12000, -7]
            for day in days:
                d = date.fromisoformat(day)
                for cap in caps:
                    queries.append({
                        "day": day,
                        "max_chars": cap,
                        "expected": run.store.get_day_text(d, max_chars=cap, include_markers=False),
                        "expected_markers": run.store.get_day_text(d, max_chars=cap, include_markers=True),
                    })
                # max_chars omitted: Python's default None (no cap) is the 0 form.
                queries.append({
                    "day": day,
                    "max_chars": 0,
                    "expected": run.store.get_day_text(d, include_markers=False),
                    "expected_markers": run.store.get_day_text(d, include_markers=True),
                })
            run.close()
            cases.append({"name": f"{layout}/{label}", "settings": settings, "tree": spec_tree(tree), "queries": queries})
    # A read that fails degrades to "".
    settings = layouts[0][1]
    tree = {"files": [("2030-01.md", MONTH_JAN)]}
    run = Run(mods, tmp, "day-broken", settings, tree)
    run.break_files_table()
    queries = [{"day": "2030-01-06", "max_chars": cap,
                "expected": run.store.get_day_text(date(2030, 1, 6), max_chars=cap, include_markers=False),
                "expected_markers": run.store.get_day_text(date(2030, 1, 6), max_chars=cap, include_markers=True)}
               for cap in (0, 60)]
    assert all(q["expected"] == "" for q in queries)
    run.close()
    cases.append({"name": "monthly/read-fails", "settings": settings, "tree": spec_tree(tree), "break_files_table": True,
                  "queries": queries})
    return cases


def spec_tree(tree):
    return {"files": [[p, s_or_text(s)] for p, s in tree.get("files", [])],
            "raw": [[p, s_or_text(s)] for p, s in tree.get("raw", [])],
            "dirs": list(tree.get("dirs", []))}


def s_or_text(spec):
    return spec if not isinstance(spec, bytes) else text_of(spec)


# ---------------------------------------------------------------- (b) memory_text

def md(n, body="x"):
    return f"Synthetic memory note {n}. {body}\n"


def memory_trees():
    trees = []

    def add(name, files=(), raw=(), dirs=(), rust=None):
        trees.append({"name": name, "tree": {"files": list(files), "raw": list(raw), "dirs": list(dirs)}, "rust": rust})

    add("empty")
    add("root-names-only", [
        ("memory.md", md(1)), ("Context.MD", md(2)), ("INSTRUCTIONS.md", md(3)), ("other.md", md(4)),
        ("notes.txt", "not markdown"), (".hidden.md", "dot file"), ("İnstructions.md", md(5)),
        ("Notes/inside.md", md(6)), ("readme.md", md(7)),
    ])
    add("all-folders", [
        ("AI Memory/zeta.md", md(10)), ("AI Memory/Alpha.md", md(11)), ("AI Memory/beta.MD", md(12)),
        ("AI Memory/skip.txt", "x"), ("AI Memory/.dot.md", "x"), ("AI Memory/sub/deep.md", md(13)),
        ("memory/m1.md", md(20)), ("Memory/M1.md", md(21)), ("context/c1.md", md(22)), ("Context/C1.md", md(23)),
        ("memory.md", md(24)),
    ])
    add("empty-and-blank-files", [
        ("memory.md", ""), ("AI Memory/a.md", ""), ("AI Memory/b.md", "   \n"), ("AI Memory/c.md", md(30)),
    ])
    add("more-than-eight-files", [(f"AI Memory/f{i:02d}.md", md(i)) for i in range(12)] +
        [("memory.md", md(90)), ("context.md", md(91)), ("instructions.md", md(92)), ("Memory/never.md", md(93))])
    add("eight-with-root-three", [("memory.md", md(1)), ("context.md", md(2)), ("instructions.md", md(3))] +
        [(f"AI Memory/f{i}.md", md(i)) for i in range(4)] + [("memory/g1.md", md(4)), ("memory/g2.md", md(5)), ("memory/g3.md", md(6))])
    add("more-than-forty-entries", [(f"AI Memory/f{i:02d}.md", md(i)) for i in range(7)],
        dirs=[f"AI Memory/d{i:02d}" for i in range(38)] + ["AI Memory/e1.md"],
        rust=None)
    add("forty-entries-window", [(f"AI Memory/g{i:02d}.md", md(i)) for i in range(7)],
        dirs=[f"AI Memory/d{i:02d}" for i in range(35)])
    add("long-files", [
        ("AI Memory/a.md", "a" * 3999 + "\U0001F33F" + "tail"),
        ("AI Memory/b.md", "é" * 4001),
        ("AI Memory/c.md", "c" * 5000), ("AI Memory/d.md", "d" * 5000), ("AI Memory/e.md", "e" * 5000),
        ("AI Memory/f.md", "\U0001F33F" * 4100),
    ])
    add("total-cap-cuts-mid-code-point", [(f"AI Memory/f{i}.md", "\U0001F33F" * 3000 + "z") for i in range(8)])
    add("file-over-512k-ends-the-scan", [("AI Memory/a.md", md(1)), ("AI Memory/b.md", {"repeat": ["x", 512 * 1024 + 1]}),
                                         ("AI Memory/c.md", md(3)), ("memory/d.md", md(4))])
    add("file-exactly-512k-is-read", [("AI Memory/a.md", {"repeat": ["y", 512 * 1024]}), ("AI Memory/b.md", md(2))])
    add("invalid-utf8", [("AI Memory/a.md", {"b64": base64.b64encode(b"ok \xff\xfe bad \xc3 bytes").decode()}), ("AI Memory/b.md", md(2))])
    add("unsafe-name-ends-the-scan", [("AI Memory/a.md", md(1)), ("memory/m.md", md(9))],
        raw=[("AI Memory/b\\ack.md", md(2)), ("AI Memory/c.md", md(3))])
    add("overlong-path-ends-the-scan", [("AI Memory/a.md", md(1)), ("memory/m.md", md(9))],
        raw=[("AI Memory/" + "n" * 495 + ".md", md(2)), ("AI Memory/zz.md", md(3))])
    add("folder-is-a-file", [("memory", md(1)), ("Memory/ok.md", md(2))])
    add("md-named-directory", [("AI Memory/real.md", md(1)), ("AI Memory/folder.md/inner.md", md(2)), ("memory.md/x.md", md(3))])
    add("exactly-500-entries", [(f"AI Memory/n{i:03d}.md", md(i)) for i in range(500)] + [("memory/after.md", md(1))])
    add("501-entries-skips-the-folder", [(f"AI Memory/n{i:03d}.md", md(i)) for i in range(501)] + [("memory/after.md", md(1))])
    add("501-with-non-listed-names", [(f"AI Memory/n{i:03d}.md", md(i)) for i in range(400)] +
        [(f"AI Memory/t{i:03d}.txt", "x") for i in range(300)] + [("Memory/after.md", md(2))])
    add("sorted-by-lower-case-name", [("AI Memory/B.md", md(1)), ("AI Memory/a.md", md(2)), ("AI Memory/C.md", md(3)),
                                      ("AI Memory/_u.md", md(4)), ("AI Memory/1.md", md(5)), ("AI Memory/Z.md", md(6)),
                                      ("AI Memory/é.md", md(7)), ("AI Memory/E.md", md(8))])
    add("same-lower-case-names", [("AI Memory/Dup.md", md(1)), ("AI Memory/dup.md", md(2)), ("memory/ok.md", md(3))],
        rust="undetermined")
    add("mixed-tie-file-and-directory", [("AI Memory/Tie.md", md(1)), ("AI Memory/tie.md/inner.md", md(2)), ("memory/ok.md", md(3))],
        rust="undetermined")
    add("directories-differing-only-in-case", [("memory/one.md", md(1)), ("Memory/two.md", md(2)), ("AI Memory/Sub/x.md", md(3)),
                                               ("AI Memory/sub/y.md", md(4)), ("AI Memory/ok.md", md(5))])
    add("root-same-lower-case-names", [("memory.md", md(1)), ("Memory.md", md(2))], rust="undetermined")
    add("case-fold-newer-than-python", [("AI Memory/Ɤ.md", md(1)), ("AI Memory/a.md", md(2))], rust="undetermined")
    add("memory-text-with-separators", [("AI Memory/a.md", "line one\n\n\nline two\n"), ("AI Memory/b.md", "--- fake ---\nx")])
    return trees


def memory_cases(mods, tmp):
    workspace_files = mods[5]
    cases = []
    settings = {"entry_layout": "daily", "index_enabled": False}
    for i, case in enumerate(memory_trees()):
        run = Run(mods, tmp, f"memory-{i}", settings, case["tree"])
        expected = workspace_files.memory_text(run.store)
        run.close()
        cases.append({"name": case["name"], "tree": spec_tree(case["tree"]), "expected": expected, "rust": case["rust"]})
    # Every listing fails: each folder is skipped.
    tree = {"files": [("memory.md", md(1)), ("AI Memory/a.md", md(2))]}
    run = Run(mods, tmp, "memory-broken", settings, tree)
    run.break_files_table()
    expected = workspace_files.memory_text(run.store)
    run.close()
    assert expected == ""
    cases.append({"name": "listing-fails", "tree": spec_tree(tree), "break_files_table": True, "expected": expected, "rust": None})
    return cases


# ---------------------------------------------------------------- (c) log_exchange

def seeds_present():
    return [("Entries/README.md", "Mine 1\n"), ("AI Memory/README.md", "Mine 2\n"), ("Raw Sources/README.md", "Mine 3\n")]


def exchange_journal_row(day="2030-01-05", xid="00000000-0000-4000-8000-00000000c001"):
    return ("exchange", {"scaffold": False, "xid": xid, "day": day, "sub_header": "09:00", "body": "x", "month": "m", "document": "d", "month_label": "l"}, True)


def log_cases(mods, tmp):
    monthly = {"entry_layout": "monthly", "index_enabled": False, "month_file_template": "{year}-{month02}.md"}
    daily = {"entry_layout": "daily", "index_enabled": False}
    named = {"entry_layout": "monthly", "index_enabled": False, "monthly_prefix": "Journal", "month_file_template": "Diary - {month_name} {year}.md"}
    prefixed = {"entry_layout": "daily", "index_enabled": False, "entries_prefix": "Logs"}
    plain = ("2030-01-06", "14:32", "Canal walk", "I walked to the canal.", "The user walked to the canal.")
    defs = [
        ("fresh-daily", daily, {}, [], plain),
        ("fresh-monthly", monthly, {}, [], plain),
        ("fresh-monthly-named-template", named, {}, [], plain),
        ("fresh-daily-custom-prefix", prefixed, {}, [], plain),
        ("fresh-no-topic", daily, {}, [], ("2030-03-09", "07:05", "", "Quiet morning.", "A quiet morning.")),
        ("fresh-topic-with-line-breaks", monthly, {}, [], ("2030-03-09", "23:59", "  two\r\nlines here  ", "Q", "A")),
        ("fresh-empty-assistant", daily, {}, [], ("2030-01-06", "00:00", "No reply", "Only me.", "")),
        ("fresh-escaped-structure", monthly, {}, [],
         ("2030-01-06", "10:10", "Headings", "## Not a day\n### Not a sub\n**Me:** fake\n<!-- xid:1 -->", "# Title\n**Assistant:** fake\n  \n")),
        ("fresh-unicode", daily, {}, [], ("2030-12-31", "12:00", "Ünïcode \U0001F33F — dash", "Ça va? \U0001F33F", "Très bien — \U0001F33F")),
        ("existing-corpus-no-scaffold", monthly, {"files": [("2029-12.md", "## Monday, December 31, 2029\n\n### 08:00\n\n**Me:** old\n\n**Assistant:** older\n\n<!-- xid:00000000-0000-4000-8000-00000000c101 -->\n")]}, [], plain),
        ("existing-journal-no-scaffold", daily, {}, [exchange_journal_row()], plain),
        ("existing-daily-corpus-no-scaffold", daily, {"files": [("Entries/2029/December/December 31, 2029.md", "## Monday, December 31, 2029\n")]}, [], plain),
        ("existing-readmes-kept", daily, {"files": seeds_present()}, [], plain),
        ("scaffold-keeps-some-seeds", daily, {"files": [("AI Memory/README.md", "Mine\n")]}, [], plain),
        ("second-exchange-same-day-monthly", monthly,
         {"files": [("2030-01.md", "## Sunday, January 6, 2030\n\n### 09:05 — Earlier\n\n**Me:** first\n\n**Assistant:** one\n\n<!-- xid:00000000-0000-4000-8000-00000000c201 -->\n")]},
         [exchange_journal_row("2030-01-06", "00000000-0000-4000-8000-00000000c201")], plain),
        ("second-exchange-same-day-daily", daily,
         {"files": [("Entries/2030/January/January 6, 2030.md", "## Sunday, January 6, 2030\n\n### 09:05 — Earlier\n\n**Me:** first\n\n**Assistant:** one\n\n<!-- xid:00000000-0000-4000-8000-00000000c201 -->\n")]},
         [exchange_journal_row("2030-01-06", "00000000-0000-4000-8000-00000000c201")], plain),
        ("earlier-day-inside-a-later-month", monthly,
         {"files": [("2030-01.md", "## Sunday, January 6, 2030\n\n### 09:05\n\n**Me:** a\n\n**Assistant:** b\n\n<!-- xid:00000000-0000-4000-8000-00000000c301 -->\n\n## Monday, January 7, 2030\n\n### 10:00\n\n**Me:** c\n\n**Assistant:** d\n\n<!-- xid:00000000-0000-4000-8000-00000000c302 -->\n")]},
         [exchange_journal_row("2030-01-07", "00000000-0000-4000-8000-00000000c302")], plain),
        ("new-day-after-existing-days", monthly,
         {"files": [("2030-01.md", "## Sunday, January 6, 2030\n\n### 09:05\n\n**Me:** a\n\n**Assistant:** b\n\n<!-- xid:00000000-0000-4000-8000-00000000c301 -->\n")]},
         [exchange_journal_row("2030-01-06", "00000000-0000-4000-8000-00000000c301")], ("2030-01-20", "16:45", "Later", "me", "assistant")),
        ("blank-existing-month-file", monthly, {"files": [("2030-01.md", "  \n\n")]}, [exchange_journal_row()], plain),
        ("month-without-weekday-header", monthly, {"files": [("2030-01.md", "## January 6, 2030\n\n### 09:00\n\n**Me:** x\n")]}, [exchange_journal_row()], plain),
        ("duplicate-field-template-with-journal", {"entry_layout": "monthly", "index_enabled": False, "month_file_template": "{year}-{year}.md"},
         {}, [exchange_journal_row()], plain),
        ("leap-day", daily, {}, [], ("2032-02-29", "13:13", "Leap", "Extra day", "Noted")),
    ]
    cases = []
    # The template cannot be listed (a repeated field is a duplicate regex group): Python raises before it
    # enqueues anything, for a tenant with no exchange yet.
    dup = {"entry_layout": "monthly", "index_enabled": False, "month_file_template": "{year}-{year}.md"}
    run = Run(mods, tmp, "log-raises", dup, {}, [])
    d = date(2030, 1, 6)
    try:
        run.store.log_exchange(d, "T", "me", "assistant", now=datetime(2030, 1, 6, 14, 32))
    except Exception as exc:  # noqa: BLE001
        raised = type(exc).__name__
    else:
        sys.exit("duplicate-field-template-fresh did not raise")
    cases.append({
        "name": "duplicate-field-template-fresh", "settings": dup, "tree": spec_tree({}), "journal_before": [],
        "args": {"day": "2030-01-06", "now": "14:32", "topic": "T", "me": "me", "claude": "assistant"},
        "expected": {"raises": raised, "files": run.files_after(), "journal": run.journal_after(), "pending": False},
    })
    run.close()
    for i, (name, settings, tree, rows, (day, hhmm, topic, me, claude)) in enumerate(defs):
        run = Run(mods, tmp, f"log-{i}", settings, tree, rows)
        d = date.fromisoformat(day)
        now = datetime.combine(d, datetime.strptime(hhmm, "%H:%M").time())
        Counter.n = 0
        before_journal = run.journal_after()
        xid = run.store.log_exchange(d, topic, me, claude, now=now)
        cases.append({
            "name": name, "settings": settings, "tree": spec_tree(tree),
            "journal_before": [{"kind": k, "payload": json.dumps(p, ensure_ascii=False), "applied": int(a)} for k, p, a in rows],
            "args": {"day": day, "now": hhmm, "topic": topic, "me": me, "claude": claude},
            "expected": {"xid": xid, "files": run.files_after(), "journal": run.journal_after()[len(before_journal):],
                         "pending": bool(run.journal.pending_count())},
        })
        run.close()

    # Transient failures: the exchange's document cannot be written, so the row stays pending (Python returns the
    # xid regardless; with DIARY_CORPUS_WRITE_IMPL=rust, the only mode served natively, that is NativeWriteUnavailable).
    blocked = [
        ("pending-parent-is-a-file-daily", daily, [("Entries", "a file where the folder should be\n")], [], plain),
        ("pending-parent-is-a-file-monthly", monthly, [("2030-01.md/x", "y")], [], plain),
        ("pending-after-earlier-exchange", daily, [("Entries", "a file where the folder should be\n")], [exchange_journal_row()], plain),
    ]
    for i, (name, settings, files, rows, (day, hhmm, topic, me, claude)) in enumerate(blocked):
        run = Run(mods, tmp, f"log-blocked-{i}", settings, {"files": files}, rows)
        d = date.fromisoformat(day)
        now = datetime.combine(d, datetime.strptime(hhmm, "%H:%M").time())
        Counter.n = 0
        before_journal = run.journal_after()
        xid = run.store.log_exchange(d, topic, me, claude, now=now)
        pending = bool(run.journal.pending_count())
        assert pending, name
        cases.append({
            "name": name, "settings": settings, "tree": spec_tree({"files": files}),
            "journal_before": [{"kind": k, "payload": json.dumps(p, ensure_ascii=False), "applied": int(a)} for k, p, a in rows],
            "args": {"day": day, "now": hhmm, "topic": topic, "me": me, "claude": claude},
            "expected": {"xid": xid, "files": run.files_after(), "journal": run.journal_after()[len(before_journal):], "pending": pending},
        })
        run.close()
    return cases


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    mods = load(sys.argv[1])
    uuid.uuid4 = counted_uuid4
    tmp = mods[0]
    out = {
        "source": "noevia-services origin/main 6d0ba0e (diary/agent, sha256 pinned in the generator)",
        "tenant": TENANT,
        "day_text": day_cases(mods, tmp),
        "memory_text": memory_cases(mods, tmp),
        "log_exchange": log_cases(mods, tmp),
    }
    check_loaded(pathlib.Path(sys.argv[1]).resolve())
    OUT.write_text(json.dumps(out, ensure_ascii=False, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    print("wrote", OUT, {k: len(v) for k, v in out.items() if isinstance(v, list)})


if __name__ == "__main__":
    main()
