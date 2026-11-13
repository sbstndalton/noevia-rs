"""Record migration-fixtures.json from the frozen Python embedding migration (embedding_migration.py).

Usage (CPython 3.12, the Diary requirements installed):

    <venv>/bin/python -I crates/diary-retrieval/tests/generate_migration_fixtures.py <noevia-services>/diary

The real EmbeddingMigration, EmbeddingConfig, Retriever and vector_slots (sha256 pinned, noevia-services
origin/main 6d0ba0e) run over a temporary service index and two tenant index files built from synthetic
corpora, with a fake embedder (hashed words, as generate_fixtures.py), a fake clock, a recorded sleep and
recorded timers (threading.Timer is replaced; _run is driven synchronously). Each step records its
result, the sleeps, the timers, every embedding call and full dumps of every index file.
"""

import hashlib
import json
import pathlib
import re
import sqlite3
import sys
import tempfile
import types

sys.dont_write_bytecode = True

PINNED = {
    "agent/embedding_migration.py": "cbdc49fa956db1f006d99faf346804fc16672519ad738521268cb5820c0516a5",
    "agent/embedding.py": "54a99e73ddc4d9987729fd245c3981c23a90227407c5e31bcf74519429093d1c",
    "agent/retrieval.py": "1f68ed102044e844b066eb948c6bb968eb0eb3404a441c8aa8cb8429bb679c7b",
    "agent/vector_slots.py": "cdc78bb0df55f2743865deca4b21cf7efc6c92490b6529a32b09cd38608e234e",
}
OUT = pathlib.Path(__file__).with_name("migration-fixtures.json")
TABLE_NAMES = re.compile(r"^(chunks|vec_meta|dirty_documents|vec_items|vec_[0-9a-f]{16}|vecmap_[0-9a-f]{16})$")
TENANTS = ["00000000-0000-4000-8000-00000000a901", "00000000-0000-4000-8000-00000000a902"]


def load(diary_dir):
    root = pathlib.Path(diary_dir).resolve()
    for rel, want in PINNED.items():
        got = hashlib.sha256((root / rel).read_bytes()).hexdigest()
        if got != want:
            sys.exit(f"refusing {rel}: sha256 {got} is not the pinned {want}")
    sys.path.insert(0, str(root))
    import sqlite_vec
    plain = sqlite_vec.load

    def load_vec(conn):
        conn.enable_load_extension(True)
        try:
            return plain(conn)
        finally:
            conn.enable_load_extension(False)
    sqlite_vec.load = load_vec
    from agent import embedding_migration, embedding, retrieval  # noqa: E402
    return embedding_migration, embedding, retrieval


def fake_vector(profile, text, dim):
    v = [0.0] * dim
    for token in re.findall(r"[a-z0-9]+", text.lower()):
        h = hashlib.sha256(f"{profile}|{token}".encode("utf-8")).digest()
        v[h[0] % dim] += 1.0 + (h[1] % 4) * 0.5
    v[dim - 1] += 0.25
    return v


class Embedder:
    """embed(target, texts, kind): the fake vectors; refuses texts with a marker; a 'perturbed' base_url
    answers different vectors (for the endpoint parity check)."""

    def __init__(self):
        self.refuse = None
        self.fail_all = False
        self.calls = []
        self.hook = None  # called with the texts before answering (a test's side effect)

    def embed(self, target, texts, kind="document", titles=None):
        self.calls.append([target.model, target.base_url, kind, [hashlib.sha256(t.encode()).hexdigest()[:10] for t in texts]])
        if self.hook is not None:
            self.hook(texts)
        if self.fail_all or (self.refuse and any(self.refuse in t for t in texts)):
            raise RuntimeError("synthetic embed refusal")
        dim = 8 if target.profile == "embeddinggemma-2" else 16
        out = []
        for t in texts:
            v = fake_vector(target.profile, t, dim)
            if target.base_url and "perturbed" in target.base_url:
                v = [x + (1.5 if i % 2 else 0.0) for i, x in enumerate(v)]
            out.append(v)
        return out


def day(header, subs):
    lines = [header, ""]
    for h, me in subs:
        lines += [f"### {h}", "", f"**Me:** {me}", ""]
    return "\n".join(lines)


def corpus(n, tag):
    return "\n".join(["# QA", ""] + [day(f"## Monday, March {d + 1}, 2031", [(f"{8 + k}:00 — {tag} {d}-{k}",
            f"Synthetic {tag} entry {d} {k} about kites tides lanterns {'QAREFUSE' if (d + k) % 7 == 3 else ''}") for k in range(3)])
            for d in range(n)])


T_A = {"base_url": None, "model": "qa-embed-a", "profile": "nomic-v1"}
T_B = {"base_url": "http://qa-embed.invalid:8080", "model": "qa-embed-b", "profile": "embeddinggemma-2"}
T_A_MOVED = {"base_url": "http://qa-embed-twin.invalid", "model": "qa-embed-a", "profile": "nomic-v1"}
T_A_PERTURBED = {"base_url": "http://qa-embed-perturbed.invalid", "model": "qa-embed-a", "profile": "nomic-v1"}

# Per-scenario options (duty, retry_s); default 0.5 and 60.
OPTIONS = {"floors": {"duty": 1.0, "retry_s": 0.25},
           "options_and_skip": {"duty": "nan", "batch": 20, "skip": 0},
           "batch_floor": {"batch": 0}}

SCENARIOS = {
    "timers_fire": [
        {"op": "refuse", "marker": "QAREFUSE"}, {"op": "reset_active_vectors"}, {"op": "run"},
        {"op": "advance", "seconds": 30}, {"op": "recheck_fired"}, {"op": "run"}, {"op": "set_next", "target": T_B},
        {"op": "break_tenant"}, {"op": "run"}, {"op": "run"}, {"op": "retry_fired"}, {"op": "run"}, {"op": "recheck_fired"},
        {"op": "advance", "seconds": 600}, {"op": "run"},
    ],
    "options_and_skip": [{"op": "status"}, {"op": "set_next", "target": T_B}, {"op": "run"}, {"op": "status"}],
    "batch_floor": [{"op": "set_next", "target": T_B}, {"op": "run"}, {"op": "status"}],
    "version_change_inside_the_write": [
        {"op": "set_next", "target": T_B}, {"op": "bump_in_guard", "target": T_A_MOVED}, {"op": "run"}, {"op": "status"},
        {"op": "dump"}, {"op": "run"}, {"op": "dump"},
    ],
    "text_changes_while_embedding": [
        {"op": "set_next", "target": T_B}, {"op": "change_text_on", "marker": "service 1 1"}, {"op": "run"}, {"op": "dump"},
        {"op": "status"},
    ],
    "floors": [
        {"op": "set_next", "target": T_B}, {"op": "run"}, {"op": "status"},
        {"op": "reset_active_vectors"}, {"op": "run"}, {"op": "run"}, {"op": "status"},
    ],
    "full_migration": [
        {"op": "status"}, {"op": "set_next", "target": T_B}, {"op": "status"},
        {"op": "switch", "force": False}, {"op": "run"}, {"op": "dump"}, {"op": "run"}, {"op": "run"},
        {"op": "status"}, {"op": "switch", "force": False}, {"op": "status"}, {"op": "dump"},
        {"op": "run"}, {"op": "rollback"}, {"op": "status"}, {"op": "dump"}, {"op": "switch", "force": False},
        {"op": "set_next", "target": T_B}, {"op": "switch", "force": True}, {"op": "finish"}, {"op": "finish"},
        {"op": "dump"}, {"op": "clear_next"}, {"op": "status"},
    ],
    "refusals_and_cooldown": [
        {"op": "refuse", "marker": "QAREFUSE"}, {"op": "reset_active_vectors"}, {"op": "run"}, {"op": "status"},
        {"op": "run"}, {"op": "advance", "seconds": 700}, {"op": "run"}, {"op": "advance", "seconds": 5000},
        {"op": "run"}, {"op": "retry_missing"}, {"op": "run"}, {"op": "refuse", "marker": None}, {"op": "retry_missing"},
        {"op": "run"}, {"op": "dump"},
    ],
    "endpoint_moves": [
        {"op": "set_next", "target": T_A_MOVED}, {"op": "status"}, {"op": "switch", "force": False}, {"op": "status"},
        {"op": "set_next", "target": T_A_PERTURBED}, {"op": "switch", "force": False}, {"op": "fail_all", "on": True},
        {"op": "switch", "force": False}, {"op": "fail_all", "on": False}, {"op": "switch", "force": True},
        {"op": "status"}, {"op": "rollback"}, {"op": "rollback"}, {"op": "dump"},
    ],
    "budget_and_unavailable": [
        {"op": "set_next", "target": T_B}, {"op": "break_tenant"}, {"op": "run"}, {"op": "status"},
        {"op": "set_next", "target": T_A}, {"op": "clear_next"}, {"op": "run"}, {"op": "status"},
    ],
}


def connect_vec(path):
    import sqlite_vec
    conn = sqlite3.connect(path)
    sqlite_vec.load(conn)  # the patched loader (extension loading allowed per connection)
    return conn


def dump(conn):
    tables = sorted(r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'") if TABLE_NAMES.match(r[0]))
    out = {"tables": tables,
           "chunks": [list(r) for r in conn.execute("SELECT id, day, file, header, content_hash FROM chunks ORDER BY id")],
           "vec_meta": [list(r) for r in conn.execute("SELECT key, value FROM vec_meta ORDER BY key")]}
    vectors = {}
    for name in tables:
        if name.startswith("vecmap_"):
            vectors[name] = [list(r) for r in conn.execute(f"SELECT chunk_id, content_hash FROM {name} ORDER BY chunk_id")]
        elif name.startswith("vec_") and name != "vec_meta":
            vectors[name] = [[r[0], bytes(r[1]).hex()] for r in conn.execute(f"SELECT chunk_id, embedding FROM {name} ORDER BY chunk_id")]
    out["vectors"] = vectors
    return out


def run(name, ops, mods):
    em, emb, retrieval = mods
    tmp = pathlib.Path(tempfile.mkdtemp(prefix="qa-r9-migration-"))
    service_db = tmp / "index.db"
    files = [(service_db, corpus(4, "service")),
             (tmp / "users" / TENANTS[0] / "index.db", corpus(3, "tenantA")),
             (tmp / "users" / TENANTS[1] / "managed-index.db", corpus(2, "tenantB"))]
    (tmp / "users" / "00000000-0000-4000-8000-00000000a903").mkdir(parents=True)
    embedder = Embedder()
    seed = types.SimpleNamespace(active=lambda: emb.Target(**T_A))
    for path, text in files:
        view = types.SimpleNamespace(active=lambda: emb.Target(**T_A),
                                     embed=lambda target, texts, kind="document", titles=None: embedder.embed(target, texts, kind))
        r = retrieval.Retriever(path, None, embedding=view)
        r.reindex_file("2031-03.md", text)
        r.close()
    embedder.calls.clear()
    cfg = {"llm": {"embed_model": "qa-embed-a"}, "retrieval": {"db_path": str(service_db)}}
    from agent.config import Config
    config = emb.EmbeddingConfig(Config(cfg), state_file=tmp / "embedding-config.json")
    shared = types.SimpleNamespace(config=config, embedder=embedder)
    clock = [1000.0]

    def now():
        clock[0] += 0.5
        return clock[0]
    sleeps, timers = [], []

    class FakeTimer:
        def __init__(self, interval, fn):
            timers.append([fn.__name__.lstrip("_"), interval])
            self.daemon = True

        def start(self):
            pass

        def cancel(self):
            pass

    em.threading.Timer = FakeTimer

    def load_vec(conn):
        import sqlite_vec
        sqlite_vec.load(conn)
        return True
    opts = OPTIONS.get(name, {})
    duty = float(opts["duty"]) if isinstance(opts.get("duty"), str) else opts.get("duty", 0.5)
    skip = (lambda u: u == TENANTS[opts["skip"]]) if "skip" in opts else (lambda _u: False)
    m = em.EmbeddingMigration(shared, service_db, duty=duty, batch=opts.get("batch", 2), load_vec=load_vec,
                              skip_user=skip, sleep=sleeps.append, now=now, retry_s=opts.get("retry_s", 60.0),
                              cooldown_s=600.0, max_cooldown_s=86400.0, active_budget=6)
    m.kick = lambda: None

    def mask(status):
        if isinstance(status, dict) and isinstance(status.get("last_pass"), dict):
            status["last_pass"] = {k: v for k, v in status["last_pass"].items() if k != "at"}
        return status

    steps = []
    for op in ops:
        kind = op["op"]
        sleeps.clear(), timers.clear(), embedder.calls.clear()
        result = None
        try:
            if kind == "status":
                result = mask(m.status())
            elif kind == "run":
                m._run()
                result = {k: v for k, v in m.last_pass.items() if k != "at"}
            elif kind in ("set_next",):
                result = mask(m.set_next(op["target"]))
            elif kind == "switch":
                result = mask(m.switch(force=op["force"]))
            elif kind in ("rollback", "finish", "clear_next", "retry_missing"):
                result = mask(getattr(m, kind)())
            elif kind == "refuse":
                embedder.refuse = op["marker"]
            elif kind == "recheck_fired":
                m._recheck()
            elif kind == "retry_fired":
                m._retry()
            elif kind == "bump_in_guard":
                # The configuration changes while a write waits for its guard: the write must abort.
                import contextlib
                real = em.tenant_writer.index_publication
                fired = []

                @contextlib.contextmanager
                def bumping(path, real=real, fired=fired, target=op["target"]):
                    if not fired:
                        fired.append(True)
                        config.set_next(target)
                    with real(path):
                        yield
                em.tenant_writer.index_publication = bumping
            elif kind == "change_text_on":
                marker = op["marker"]
                done = []

                def change(texts, marker=marker, done=done):
                    for t in texts:
                        if marker in t and not done:
                            done.append(True)
                            for path, _ in files:
                                c = sqlite3.connect(path)
                                c.execute("UPDATE chunks SET content_hash = 'qa-changed' WHERE body = ?", (t,))
                                c.commit()
                                c.close()
                embedder.hook = change
            elif kind == "fail_all":
                embedder.fail_all = op["on"]
            elif kind == "advance":
                clock[0] += op["seconds"]
            elif kind == "reset_active_vectors":
                for path, _ in files:
                    conn = connect_vec(path)
                    conn.execute("DELETE FROM vec_items")
                    conn.commit()
                    conn.close()
            elif kind == "break_tenant":
                p = files[1][0]
                p.unlink()
                conn = sqlite3.connect(p)
                conn.execute("CREATE TABLE unrelated (x)")
                conn.commit()
                conn.close()
            elif kind == "dump":
                result = {}
                for path, _ in files:
                    conn = connect_vec(path)
                    try:
                        result[str(path.relative_to(tmp))] = dump(conn) if conn.execute("SELECT 1 FROM sqlite_master WHERE name='chunks'").fetchone() else None
                    finally:
                        conn.close()
            else:
                sys.exit(f"unknown op {kind}")
        except emb.EmbeddingConfigError as exc:
            result = {"error": exc.code, "status": exc.status, "message": str(exc), "coverage": getattr(exc, "coverage", None)}
        state = (tmp / "embedding-config.json")
        steps.append({**op, "expected": result, "sleeps": list(sleeps), "timers": list(timers),
                      "embed_calls": list(embedder.calls), "state_file": state.read_text() if state.exists() else None})
    return steps


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    if sys.version_info[:2] != (3, 12):
        sys.exit("record with CPython 3.12, the Diary image's interpreter")
    mods = load(sys.argv[1])
    data = {"source": "noevia-services diary/agent at origin/main 6d0ba0e", "python": sys.version.split()[0],
            "pinned_sha256": PINNED, "tenants": TENANTS, "options": OPTIONS,
            "scenarios": {name: run(name, ops, mods) for name, ops in SCENARIOS.items()}}
    OUT.write_text(json.dumps(data, ensure_ascii=False, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({sum(len(s) for s in data['scenarios'].values())} steps)")


if __name__ == "__main__":
    main()
