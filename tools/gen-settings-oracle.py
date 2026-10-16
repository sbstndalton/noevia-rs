#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/settings-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, middleware, router,
`get_settings` and `db.get_setting`) against synthetic SQLite stores and records the exact
status, content type and body bytes of `GET /api/v1/settings`:

    PYTHON=... python3 tools/gen-settings-oracle.py <services>/model-manager/app > fixture.json

Every token is synthetic. The store for each case is described declaratively (SQL statements,
raw bytes or a held lock) so the Rust test can build the identical store with rusqlite.
CI re-runs this against the pinned services ref and requires a byte-identical fixture.
"""
from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import sys
import tempfile
from pathlib import Path

SCHEMA = "CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL)"


def put(value_sql: str) -> list[str]:
    return [SCHEMA, f"INSERT INTO kv(key, value) VALUES('hf_token', {value_sql})"]


def text(value: str) -> list[str]:
    return put("'" + value.replace("'", "''") + "'")


# id, journal mode, store, lock. `store` is a list of SQL statements, {"bytes": hex} or None
# (no file). Python's own connection switches the file to WAL; "delete" keeps a rollback
# journal so an exclusive lock really blocks readers.
CASES = [
    ("no-token-row", "wal", [SCHEMA, "INSERT INTO kv VALUES('other', 'x')"], None),
    ("empty-table", "wal", [SCHEMA], None),
    ("empty-token", "wal", text(""), None),
    ("short-3", "wal", text("abc"), None),
    ("boundary-8", "wal", text("abcdefgh"), None),
    ("boundary-9", "wal", text("abcdefghi"), None),
    ("long-synthetic", "wal", text("hf_SYNTHETICsyntheticSYNTHETIC0000wxyz"), None),
    ("whitespace-9", "wal", text(" " * 9), None),
    ("non-ascii", "wal", text("tökén-ünïcødé-çà"), None),
    ("non-ascii-8-codepoints", "wal", text("äöüßéèàç"), None),
    ("emoji-8-codepoints", "wal", text("\U0001F600" * 8), None),
    ("emoji-9-codepoints", "wal", text("a" + "\U0001F600\U0001F601\U0001F602\U0001F603" * 2), None),
    ("emoji-zwj-tail", "wal", text("synthetic-\U0001F468‍\U0001F469‍\U0001F467"), None),
    ("combining-tail", "wal", text("synthetic-éé"), None),
    ("json-escapes", "wal", text('synthetic-"\\\n\t\u0001\u007f '), None),
    ("integer-affinity", "wal", put("123456789012"), None),
    ("real-affinity", "wal", put("1.5e300"), None),
    ("rollback-journal-unlocked", "delete", text("synthetic-rollback-token"), None),
    ("wal-uncommitted-writer", "wal", text("synthetic-committed-token"), "wal-writer"),
    ("locked-exclusive", "delete", text("synthetic-locked-token"), "exclusive"),
    ("missing-db", None, None, None),
    ("no-kv-table", "wal", ["CREATE TABLE other (x)"], None),
    ("not-a-database", None, {"bytes": ("synthetic not a database " * 64).encode().hex()}, None),
    ("corrupt-null-value", "wal", ["CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT)",
                                   "INSERT INTO kv VALUES('hf_token', NULL)"], None),
    ("corrupt-invalid-utf8", "wal", put("CAST(x'73796e746865746963ff' AS TEXT)"), None),
    ("corrupt-blob-short", "wal", put("x'00010203'"), None),
    ("corrupt-blob-long", "wal", put("x'73796e7468657469632d626c6f62'"), None),
    ("corrupt-blob-quote-escapes", "wal", put("x'73796e74686574696300275c0a'"), None),
    ("corrupt-blob-both-quotes", "wal", put("x'73796e7468657469632722ff09'"), None),
    ("corrupt-blob-empty", "wal", put("x''"), None),
]


def build(path: Path, journal: str | None, store) -> None:
    if store is None:
        return
    if isinstance(store, dict):
        path.write_bytes(bytes.fromhex(store["bytes"]))
        return
    c = sqlite3.connect(path, isolation_level=None)
    c.execute(f"PRAGMA journal_mode={journal.upper()}")
    for statement in store:
        c.execute(statement)
    c.close()


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR"}:
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import config, db  # noqa: E402
    from app.main import app  # noqa: E402

    client = TestClient(app, raise_server_exceptions=False)  # no lifespan: db.init never runs
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest()
              for name in ("api.py", "db.py", "main.py")}
    out = []
    # noevia#1922: one store with Python's own `db.init()` schema (its CREATE TABLE statements,
    # comments included, are longer than 1 KiB), in WAL with every connection closed.
    with tempfile.TemporaryDirectory() as root:
        config.settings.data_dir = Path(root)
        db._DB_PATH = None
        db.init()
        c = sqlite3.connect(Path(root) / "model_loader.db")
        schema = [r[0] for r in c.execute(
            "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")]
        c.close()
    cases = CASES + [("python-init-schema-wal", "wal",
                      schema + ["INSERT INTO kv(key, value) VALUES('hf_token', 'hf_SYNTHETICinitSchema9876')"], None)]
    for case_id, journal, store, lock in cases:
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "model_loader.db"
            build(path, journal, store)
            holder = None
            if lock == "exclusive":
                holder = sqlite3.connect(path, isolation_level=None)
                holder.execute("BEGIN EXCLUSIVE")
            elif lock == "wal-writer":
                holder = sqlite3.connect(path, isolation_level=None)
                holder.execute("BEGIN IMMEDIATE")
                holder.execute("UPDATE kv SET value='synthetic-uncommitted-token' WHERE key='hf_token'")
            config.settings.data_dir = Path(root)
            db._DB_PATH = None
            response = client.get("/api/v1/settings")
            if holder is not None:
                holder.execute("ROLLBACK")
                holder.close()
            out.append({
                "id": case_id,
                "journal": journal,
                "store": store,
                "lock": lock,
                "status": response.status_code,
                "content_type": response.headers.get("content-type"),
                "body": response.content.decode("utf-8"),
            })
    json.dump({"oracle": digest, "cases": out}, sys.stdout, ensure_ascii=False, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
