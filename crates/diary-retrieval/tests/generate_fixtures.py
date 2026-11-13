"""Record retrieval-fixtures.json from the frozen Python Diary retriever and vector slots.

Usage (run from anywhere; the venv needs the Diary requirements, sqlite-vec 0.1.9 included):

    <venv>/bin/python -I crates/diary-retrieval/tests/generate_fixtures.py <noevia-services>/diary

The script refuses any other source: the sha256 of every Python file the scenarios execute must
match PINNED (noevia-services origin/main 6d0ba0e). Every corpus, query and model name here is
synthetic. Each scenario is a list of operations; the runner executes them on the Python
Retriever and stores each result next to its operation, and the Rust differential test replays
the same operations and compares.

The embedder is a fake with no model: a bag of hashed words (see `fake_vector`), implemented the
same way in the Rust test. It fails on texts containing a configured marker and can change width.
"""

import hashlib
import json
import pathlib
import re
import sqlite3
import sys
import tempfile

sys.dont_write_bytecode = True

PINNED = {
    "agent/retrieval.py": "1f68ed102044e844b066eb948c6bb968eb0eb3404a441c8aa8cb8429bb679c7b",
    "agent/vector_slots.py": "cdc78bb0df55f2743865deca4b21cf7efc6c92490b6529a32b09cd38608e234e",
    "agent/corpus.py": "b95b9fc5e6fd9758cfffa8c88f5f9d5cad042a258f193f20c6edfe4928bff2bb",
    "agent/corpus_parse.py": "939c21012f58fc16917df174f00de776502d417e8f7e23139630bc968b5a6540",
    "agent/embedding.py": "54a99e73ddc4d9987729fd245c3981c23a90227407c5e31bcf74519429093d1c",
    "agent/tenant_writer.py": "c218b1a602b720c4c3a1f2efe62b109715507a7496e9f0161745aa59823e5f3c",
}
OUT = pathlib.Path(__file__).with_name("retrieval-fixtures.json")
TABLE_NAMES = re.compile(r"^(chunks|vec_meta|dirty_documents|vec_items|vec_[0-9a-f]{16}|vecmap_[0-9a-f]{16})$")


def load(diary_dir):
    root = pathlib.Path(diary_dir).resolve()
    for rel, want in PINNED.items():
        got = hashlib.sha256((root / rel).read_bytes()).hexdigest()
        if got != want:
            sys.exit(f"refusing {rel}: sha256 {got} is not the pinned {want}")
    sys.path.insert(0, str(root))
    # sqlite-vec 0.1.9 does not enable extension loading itself; a SQLite built without it on by
    # default refuses the load. Allow it per connection only, as the Diary's tests/conftest.py does.
    import sqlite_vec
    plain_load = sqlite_vec.load

    def load(conn):
        conn.enable_load_extension(True)
        try:
            return plain_load(conn)
        finally:
            conn.enable_load_extension(False)

    sqlite_vec.load = load
    from agent import retrieval, vector_slots  # noqa: E402
    from agent.embedding import Target  # noqa: E402
    return retrieval, vector_slots, Target


def fake_vector(profile, text, dim, scale=1.0):
    """Bag of hashed lower-case ASCII words plus a small constant, so no vector is all zero."""
    if dim == 0:
        return []
    v = [0.0] * dim
    for token in re.findall(r"[a-z0-9]+", text.lower()):
        h = hashlib.sha256(f"{profile}|{token}".encode("utf-8")).digest()
        v[h[0] % dim] += 1.0 + (h[1] % 4) * 0.5
    v[dim - 1] += 0.25
    return [x * scale for x in v]


class FakeEmbedding:
    def __init__(self, target):
        self.target = target
        self.fail_marker = None
        self.dim = 16
        self.scale = 1.0
        self.calls = []

    def active(self):
        return self.target

    def embed(self, target, texts, kind="document", titles=None):
        out = []
        for text in texts:
            self.calls.append([kind, hashlib.sha256(text.encode("utf-8")).hexdigest()[:12]])
            if self.fail_marker and self.fail_marker in text:
                raise RuntimeError("synthetic embed failure")
            out.append(fake_vector(target.profile, text, self.dim, self.scale))
        return out


def day(header, subs):
    lines = [header, ""]
    for sub_header, exchanges in subs:
        lines.append(f"### {sub_header}")
        lines.append("")
        for me, reply in exchanges:
            lines.append(f"**Me:** {me}")
            lines.append("")
            if reply:
                lines.append(f"**Assistant:** {reply}")
                lines.append("")
    return "\n".join(lines)


MARCH = "\n".join([
    "# QA Diary — March 2031", "",
    day("## Monday, March 3, 2031", [
        ("08:10 — Orchard walk", [("Walked the qa orchard path before work, apple blossom early this year.",
                                   "Noted the early blossom and the walk.")]),
        ("12:40 — Lunch with Orrin", [("Lunch with Qa Orrin Vell, we planned the canal survey for April.", "")]),
    ]),
    day("## Tuesday, March 4, 2031", [
        ("07:55 — Poor sleep", [("Slept badly, third night of poor sleep this week, screens too late.",
                                 "Asked about evening screens and the sleep pattern.")]),
        ("18:20", [("Repaired the bicycle chain and oiled the gears in the shed.", "")]),
        ("21:00 — Empty", []),
    ]),
    day("## Wednesday, February 30, 2031", [
        ("09:00 — Impossible date", [("This day does not exist and is never indexed.", "")]),
    ]),
    day("## Thursday, March 6, 2031", [
        ("10:05 — Canal survey prep", [("Packed waders, the tide table and the survey notebook for the canal.",
                                        "Listed what to bring for the canal survey."),
                                       ("Also charged the camera batteries.", "")]),
    ]),
])

MARCH_EDITED = MARCH.replace(
    "Repaired the bicycle chain and oiled the gears in the shed.",
    "Replaced the bicycle chain entirely; the old one snapped near the bridge.",
).replace(
    "### 08:10 — Orchard walk\n\n**Me:** Walked the qa orchard path before work, apple blossom early this year.\n\n"
    "**Assistant:** Noted the early blossom and the walk.\n\n", "")

APRIL = "\n".join([
    "# QA Diary — April 2031", "",
    day("## Tuesday, April 1, 2031", [
        ("09:15 — Canal survey day one", [("Started the canal survey with Qa Orrin Vell, counted forty herons.",
                                           "Recorded the heron count.")]),
        ("16:30 — Garden", [("Planted beans and marigolds along the south fence.", "")]),
    ]),
    day("## Friday, April 4, 2031", [
        ("20:45 — Sleep better", [("Two good nights of sleep after moving screens out of the bedroom.",
                                   "Noted the sleep improvement.")]),
    ]),
])

FAILING = "\n".join([
    "# QA Diary — May 2031", "",
    day("## Thursday, May 1, 2031", [
        ("08:00 — Bird count", [("Counted swifts over the QA roofline, eleven in total.", "")]),
        ("09:00 — Broken", [("This chunk carries the QAFAIL marker so its embedding fails.", "")]),
    ]),
])

UNICODE = "\n".join([
    "# QA Diary — June 2031", "",
    day("## Sunday, June 1, 2031", [
        ("07:30 — Café", [("Café crème at the qa kiosk — naïve résumé review, ünïcödé text and emoji 🌿.", "")]),
        ("19:00 — Ζωή", [("Γράμματα in Greek and 漢字 in a synthetic note about the qa herb garden.", "")]),
    ]),
])

DUPLICATE = "\n".join([
    "# QA Diary — July 2031", "",
    day("## Tuesday, July 1, 2031", [
        ("06:00 — Repeated", [("The same synthetic qa line about kettles twice.", "")]),
        ("06:00 — Repeated", [("The same synthetic qa line about kettles twice.", "")]),
        ("06:30 — Other", [("A different qa line about lanterns and moths.", "")]),
    ]),
])

T_A = {"base_url": None, "model": "qa-embed-a", "profile": "nomic-v1"}
T_B = {"base_url": "http://qa-embed.invalid:8080", "model": "qa-embed-b", "profile": "embeddinggemma-2"}
T_C = {"base_url": None, "model": "qa-embed-c-modèl", "profile": "nomic-v1"}

SEARCHES = [
    {"query": "poor sleep and evening screens", "top_k": 8, "min_score": 0.30},
    {"query": "canal survey with Orrin", "top_k": 3, "min_score": 0.0},
    {"query": "bicycle chain", "top_k": 8, "min_score": 0.30},
    {"query": "something entirely unrelated zebra xylophone", "top_k": 8, "min_score": 0.30},
    {"query": "orchard blossom walk", "top_k": 1, "min_score": 0.0},
]


def searches(with_ids=True):
    return [{"op": "search", **s, "with_ids": with_ids} for s in SEARCHES]


SCENARIOS = {
    "basic_nomic": [
        {"op": "open", "target": T_A},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH},
        {"op": "dump"},
        *searches(),
        {"op": "search", "query": "poor sleep", "top_k": 8, "min_score": 0.30, "with_ids": False},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH_EDITED},
        {"op": "dump"},
        {"op": "reindex", "file": "2031-04.md", "text": APRIL},
        *searches(),
        {"op": "dirty", "document": "2031-03.md"},
        *searches(),
        {"op": "undirty", "document": "2031-03.md"},
        {"op": "stats"},
        {"op": "coverage"},
        {"op": "missing", "limit": 4, "exclude": []},
        {"op": "reindex", "file": "2031-03.md", "text": ""},
        {"op": "reindex", "file": "2031-03.md", "text": None},
        {"op": "dump"},
        {"op": "stats"},
    ],
    "two_slots": [
        {"op": "open", "target": T_A},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH},
        {"op": "set_active", "target": T_B},
        *searches(),
        {"op": "coverage"},
        {"op": "missing", "limit": 2, "exclude": []},
        {"op": "missing", "limit": 10, "exclude": [1, 3]},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH},
        {"op": "dump"},
        *searches(),
        {"op": "set_active", "target": T_A},
        {"op": "prepare", "file": "2031-03.md", "text": MARCH_EDITED, "as": "p1"},
        {"op": "set_active", "target": T_B},
        {"op": "apply", "plan": "p1"},
        {"op": "dump"},
        {"op": "coverage"},
        {"op": "missing", "limit": 4, "exclude": []},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH_EDITED},
        {"op": "set_active", "target": T_C},
        {"op": "reindex", "file": "2031-04.md", "text": APRIL},
        {"op": "dump"},
        {"op": "stats"},
        {"op": "drop_slot", "target": T_B},
        {"op": "drop_slot", "target": T_B},
        {"op": "drop_slot", "target": T_A},
        {"op": "dump"},
        {"op": "set_active", "target": T_A},
        *searches(),
        {"op": "reindex", "file": "2031-04.md", "text": APRIL},
        {"op": "dump"},
    ],
    "pending_and_failures": [
        {"op": "open", "target": T_B},
        *searches(),
        {"op": "fail_on", "marker": "QAFAIL"},
        {"op": "reindex", "file": "2031-05.md", "text": FAILING},
        {"op": "dump"},
        {"op": "fail_on", "marker": None},
        {"op": "reindex", "file": "2031-05.md", "text": FAILING},
        {"op": "dump"},
        {"op": "set_dim", "dim": 12},
        {"op": "reindex", "file": "2031-06.md", "text": UNICODE},
        {"op": "dump"},
        {"op": "search", "query": "swifts roofline", "top_k": 8, "min_score": 0.0, "with_ids": True},
        {"op": "set_dim", "dim": 16},
        {"op": "fail_on", "marker": "swifts"},
        {"op": "search", "query": "swifts roofline", "top_k": 8, "min_score": 0.0, "with_ids": True},
        {"op": "fail_on", "marker": None},
        {"op": "search", "query": "swifts roofline", "top_k": 8, "min_score": 0.0, "with_ids": True},
        {"op": "reindex", "file": "2031-06.md", "text": UNICODE},
        {"op": "search", "query": "café crème kiosk", "top_k": 8, "min_score": 0.0, "with_ids": True},
        {"op": "dump"},
        {"op": "prepare", "file": "2031-07.md", "text": MARCH, "as": "p2"},
        {"op": "reindex", "file": "2031-07.md", "text": MARCH},
        {"op": "apply", "plan": "p2"},
        {"op": "dump"},
    ],
    "duplicates_and_reused_ids": [
        {"op": "open", "target": T_A},
        {"op": "reindex", "file": "2031-07.md", "text": DUPLICATE},
        {"op": "prepare", "file": "2031-07.md", "text": DUPLICATE, "as": "p3"},
        {"op": "set_active", "target": T_B},
        {"op": "reindex", "file": "2031-07.md", "text": DUPLICATE},
        {"op": "dump"},
        {"op": "set_active", "target": T_A},
        {"op": "sql_delete_chunk", "id": 2},
        {"op": "reindex", "file": "2031-08.md", "text": APRIL},
        {"op": "dump"},
        {"op": "set_active", "target": T_B},
        {"op": "coverage"},
        {"op": "missing", "limit": 10, "exclude": []},
        {"op": "reindex", "file": "2031-07.md", "text": DUPLICATE},
        {"op": "dump"},
        *searches(),
    ],
    "no_vector_support_then_vectors": [
        {"op": "open_novec", "target": T_A},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH},
        *searches(),
        {"op": "stats"},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH_EDITED},
        {"op": "dump"},
        {"op": "open", "target": T_A},
        {"op": "coverage"},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH_EDITED},
        {"op": "dump"},
        *searches(),
    ],
    "guarded_reads_do_not_adopt": [
        {"op": "legacy_seed", "file": "2031-03.md", "text": MARCH, "dim": 16, "profile": "nomic-v1"},
        {"op": "open_guarded", "target": T_A},
        {"op": "dump"},
        *searches(),
        {"op": "stats"},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH_EDITED},
        {"op": "dump"},
        *searches(),
    ],
    "failed_apply_rolls_back": [
        {"op": "open", "target": T_A},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH},
        {"op": "set_active", "target": T_B},
        {"op": "set_dim", "dim": 0},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH_EDITED},
        {"op": "dump"},
        {"op": "set_dim", "dim": 16},
        {"op": "set_active", "target": T_A},
        {"op": "set_scale", "scale": 1e39},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH_EDITED},
        {"op": "dump"},
        {"op": "search", "query": "poor sleep", "top_k": 8, "min_score": 0.0, "with_ids": True},
        {"op": "set_scale", "scale": 1.0},
        {"op": "reindex", "file": "2031-03.md", "text": MARCH_EDITED},
        {"op": "dump"},
    ],
    "malformed_slot_records": [
        {"op": "legacy_seed", "file": "2031-03.md", "text": MARCH, "dim": 16, "profile": "nomic-v1"},
        {"op": "open_novec", "target": T_A},
        *[step for value in (" 16\n", "' 16 '||char(10)", "char(28)||'16'", "+0_16", "X'3136'", "X''", "'16.0'", "0", "-16", "'1__6'", "NULL", "'  '", "4.9")
          for step in ({"op": "sql", "statements": [f"UPDATE vec_meta SET value = {value if value.startswith(('X', 'NULL', '0', '-', '4', chr(39), 'char')) else repr(value)} WHERE key = 'dim'"]},
                       {"op": "slots_probe", "target": T_A})],
        {"op": "sql", "statements": ["UPDATE vec_meta SET value = '16' WHERE key = 'dim'"]},
        *[step for record in (
            '{"model": "m", "profile": "nomic-v1", "dim": 16, "table": "vec_items", "map": null, "id": "override"}',
            '{"model": "m", "dim": true, "table": "vec_items", "map": null}',
            '{"dim": 16, "table": "vec_items\\n", "map": null}',
            '{"dim": 16, "table": "vec_items", "map": ""}',
            '{"dim": 16.0, "table": "vec_items"}',
            '{"dim": 65537, "table": "vec_items"}',
            '[1, 2]',
            'not json',
            '{"dim": 16, "table": "vec_0123456789abcdef", "map": "vecmap_0123456789abcdef", "extra": [1]}',
          ) for step in (
            {"op": "sql", "statements": ["DELETE FROM vec_meta WHERE key LIKE 'slot:%'"],
             "params": []},
            {"op": "sql_meta", "key": "slot:" + "955edd8bb49c8295", "value": record},
            {"op": "slots_probe", "target": T_A},
            {"op": "sql_meta", "key": "SLOT:" + "955edd8bb49c8295", "value": record},
            {"op": "slots_probe", "target": T_A},
          )],
    ],
    "legacy_adoption_nomic": [
        {"op": "legacy_seed", "file": "2031-03.md", "text": MARCH, "dim": 16, "profile": "nomic-v1"},
        {"op": "open", "target": T_A},
        {"op": "dump"},
        *searches(),
        {"op": "reindex", "file": "2031-03.md", "text": MARCH_EDITED},
        {"op": "dump"},
    ],
    "legacy_kept_for_other_profile": [
        {"op": "legacy_seed", "file": "2031-03.md", "text": MARCH, "dim": 16, "profile": "nomic-v1"},
        {"op": "open", "target": T_B},
        {"op": "dump"},
        *searches(),
        {"op": "reindex", "file": "2031-03.md", "text": MARCH_EDITED},
        {"op": "dump"},
        {"op": "set_active", "target": T_C},
        {"op": "stats"},
        {"op": "reindex", "file": "2031-04.md", "text": APRIL},
        {"op": "dump"},
        *searches(),
        {"op": "drop_slot", "target": T_C},
        {"op": "dump"},
    ],
}


def legacy_seed(db_path, retrieval, file_name, text, dim, profile):
    """A pre-#1636 index: chunks plus vec_items and vec_meta 'dim', no slot record."""
    import sqlite_vec
    from agent.corpus_parse import parse
    conn = sqlite3.connect(db_path)
    sqlite_vec.load(conn)
    conn.executescript(retrieval._SCHEMA)
    conn.execute(f"CREATE VIRTUAL TABLE vec_items USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[{dim}])")
    conn.execute("INSERT INTO vec_meta (key, value) VALUES ('dim', ?)", (str(dim),))
    for day_iso, header, chunk, h in parse(text).chunks:
        cur = conn.execute("INSERT INTO chunks (day, file, header, body, content_hash) VALUES (?, ?, ?, ?, ?)",
                           (day_iso, file_name, header, chunk, h))
        conn.execute("INSERT INTO vec_items (chunk_id, embedding) VALUES (?, ?)",
                     (cur.lastrowid, retrieval._serialize_f32(fake_vector(profile, chunk, dim))))
    conn.commit()
    conn.close()


def dump(conn):
    tables = sorted(r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'")
                    if TABLE_NAMES.match(r[0]))
    out = {"tables": tables}
    out["chunks"] = [list(r) for r in conn.execute(
        "SELECT id, xid, day, file, header, body, content_hash FROM chunks ORDER BY id")]
    out["vec_meta"] = [list(r) for r in conn.execute("SELECT key, value FROM vec_meta ORDER BY key")]
    out["dirty"] = [r[0] for r in conn.execute("SELECT document FROM dirty_documents ORDER BY document")]
    vectors = {}
    for name in tables:
        if name.startswith("vecmap_"):
            vectors[name] = [list(r) for r in conn.execute(f"SELECT chunk_id, content_hash FROM {name} ORDER BY chunk_id")]
        elif name.startswith("vec_") and name != "vec_meta":
            vectors[name] = [[r[0], bytes(r[1]).hex()] for r in conn.execute(
                f"SELECT chunk_id, embedding FROM {name} ORDER BY chunk_id")]
    out["vectors"] = vectors
    return out


GUARD = "0a0a0a0a-1111-4222-8333-444444444444"


def run(name, ops, modules):
    retrieval, slots, Target = modules
    from agent import tenant_writer
    guarded = None

    def exempt():
        # Lets the fixture's own writes past the guard of the faked writer tenant; adoption on
        # reads still sees an unheld writer lock.
        import contextlib
        return tenant_writer.exempt(GUARD, "fixture") if guarded is not None else contextlib.nullcontext()
    tmp = tempfile.mkdtemp(prefix="qa-r9-retrieval-")
    db = str(pathlib.Path(tmp) / "index.db")
    r = None
    emb = None
    plans = {}
    steps = []
    for op in ops:
        kind = op["op"]
        if emb is not None:
            emb.calls = []
        result = None
        if kind == "legacy_seed":
            legacy_seed(db, retrieval, op["file"], op["text"], op["dim"], op["profile"])
        elif kind == "open":
            if r is not None:
                r.close()
            emb = FakeEmbedding(Target(**op["target"]))
            emb.calls = []
            r = retrieval.Retriever(pathlib.Path(db), None, embedding=emb)
            result = {"vec_available": r.vec_available}
        elif kind == "open_novec":
            emb = FakeEmbedding(Target(**op["target"]))
            emb.calls = []
            real = retrieval._load_vec_extension
            retrieval._load_vec_extension = lambda conn: False
            try:
                r = retrieval.Retriever(pathlib.Path(db), None, embedding=emb)
            finally:
                retrieval._load_vec_extension = real
            result = {"vec_available": r.vec_available}
        elif kind == "open_guarded":
            # A writer tenant's index opened without its writer lock: reads never adopt (D5).
            emb = FakeEmbedding(Target(**op["target"]))
            emb.calls = []
            real_of, real_writer = tenant_writer.tenant_of, tenant_writer.is_writer
            tenant_writer.tenant_of = lambda path: GUARD
            tenant_writer.is_writer = lambda uid: uid == GUARD
            try:
                r = retrieval.Retriever(pathlib.Path(db), None, embedding=emb)
            finally:
                tenant_writer.tenant_of = real_of
            guarded = real_writer
            result = {"vec_available": r.vec_available}
        elif kind == "set_scale":
            emb.scale = op["scale"]
        elif kind == "sql":
            for statement in op["statements"]:
                r._conn.execute(statement)
            r._conn.commit()
        elif kind == "sql_meta":
            r._conn.execute("INSERT OR REPLACE INTO vec_meta (key, value) VALUES (?, ?)", (op["key"], op["value"]))
            r._conn.commit()
        elif kind == "slots_probe":
            t = Target(**op["target"])
            found = slots.find_slot(r._conn, t)
            listed = slots.list_slots(r._conn)
            keys = ("id", "dim", "table", "map")
            result = {"needs": slots.needs_legacy_adoption(r._conn, t),
                      "find": None if found is None else {k: found.get(k) for k in keys},
                      "list": [{k: x.get(k) for k in keys} for x in listed]}
        elif kind == "set_active":
            emb.target = Target(**op["target"])
        elif kind == "fail_on":
            emb.fail_marker = op["marker"]
        elif kind == "set_dim":
            emb.dim = op["dim"]
        elif kind == "reindex":
            try:
                with exempt():
                    embedded = r.reindex_file(op["file"], op["text"])
                result = {"embedded": embedded, "pending": r.pending_embeddings}
            except Exception as exc:  # noqa: BLE001 - the Rust must fail the same steps
                result = {"error": type(exc).__name__}
        elif kind == "prepare":
            plan = r.prepare_reindex(op["file"], op["text"])
            plans[op["as"]] = plan
            result = {"pending": plan.pending, "embedded_hashes": sorted(plan.embeddings),
                      "rows": [list(row) for row in plan.rows]}
        elif kind == "apply":
            embedded = r.apply_reindex(plans[op["plan"]])
            result = {"embedded": embedded, "pending": r.pending_embeddings}
        elif kind == "search":
            diag = {}
            try:
                hits = r.search(op["query"], top_k=op["top_k"], min_score=op["min_score"],
                                with_ids=op["with_ids"], diag=diag)
            except Exception as exc:  # noqa: BLE001
                steps.append({**op, "expected": {"error": type(exc).__name__}, "embed_calls": list(emb.calls)})
                continue
            scores = [h["score"] for h in hits]
            if len(set(scores)) != len(scores):
                sys.exit(f"{name}: tied scores in {op['query']!r}; change the corpus so ordering is defined")
            result = {"hits": hits, "status": diag.get("status")}
        elif kind == "dirty":
            r._conn.execute("INSERT OR IGNORE INTO dirty_documents (document) VALUES (?)", (op["document"],))
            r._conn.commit()
        elif kind == "undirty":
            r._conn.execute("DELETE FROM dirty_documents WHERE document = ?", (op["document"],))
            r._conn.commit()
        elif kind == "sql_delete_chunk":
            # An older release deleting a chunk without knowing this slot leaves its vectors behind.
            r._conn.execute("DELETE FROM chunks WHERE id = ?", (op["id"],))
            r._conn.commit()
        elif kind == "stats":
            result = r.stats()
        elif kind == "coverage":
            result = slots.coverage(r._conn, slots.find_slot(r._conn, emb.active()))
        elif kind == "missing":
            result = [list(row) for row in slots.missing_chunks(
                r._conn, slots.find_slot(r._conn, emb.active()), op["limit"], op["exclude"])]
        elif kind == "drop_slot":
            with r._conn:
                result = slots.drop_slot(r._conn, Target(**op["target"]))
        elif kind == "dump":
            result = dump(r._conn)
        else:
            sys.exit(f"unknown op {kind}")
        step = dict(op)
        step["expected"] = result
        step["embed_calls"] = list(emb.calls) if emb is not None else []
        steps.append(step)
    if guarded is not None:
        tenant_writer.is_writer = guarded
    if r is not None:
        r.close()
    return steps


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    if sys.version_info[:2] != (3, 12):
        sys.exit("record with CPython 3.12, the Diary image's interpreter")
    modules = load(sys.argv[1])
    import sqlite_vec
    Target = modules[2]
    slot_ids = {t["model"]: Target(**t).slot_id for t in (T_A, T_B, T_C)}
    data = {
        "source": "noevia-services diary/agent at origin/main 6d0ba0e",
        "pinned_sha256": PINNED,
        "python": sys.version.split()[0],
        "sqlite": sqlite3.sqlite_version,
        "sqlite_vec": getattr(sqlite_vec, "__version__", "0.1.9"),
        "slot_ids": slot_ids,
        "scenarios": {name: run(name, ops, modules) for name, ops in SCENARIOS.items()},
    }
    OUT.write_text(json.dumps(data, ensure_ascii=False, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({sum(len(s) for s in data['scenarios'].values())} steps)")


if __name__ == "__main__":
    main()
