#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/check-updates-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, `api.check_updates`,
`hf.check_updates_for`, `hf.repo_file_mtimes`, `db` and `helpers._update_status_map`) against
synthetic stores and model folders, with the Hub replaced by a local fake on 127.0.0.1 and
`time.time()` inside `hf` pinned, and records for each case:

  * the exact status, content type and body of `POST /api/v1/models/check-updates`,
  * every request the fake Hub received (method, path and query, Authorization header),
  * the `update_check` table afterwards.

    python3 -I tools/gen-check-updates-oracle.py <services>/model-manager/app > fixture.json

Every name, repository, token and date is synthetic; nothing reaches the real Hub.
"""
from __future__ import annotations

import hashlib
import http.server
import json
import os
import sqlite3
import sys
import tempfile
import threading
import types
from pathlib import Path

NOW = 1700000500.75
MTIME = 1_700_000_000  # 2023-11-14T22:13:20Z
TOKEN = "hf_synthetic_token_0123456789"


def done(filename: str, repo: str, status: str = "done") -> str:
    return (f"INSERT INTO download_history(repo_id, filename, dest_path, total_bytes, status, started_at)"
            f" VALUES({repo}, '{filename}', '/models/{filename}', 1, '{status}', 1.0)")


def token(value: str) -> str:
    return f"INSERT INTO kv(key, value) VALUES('hf_token', {value})"


def tree(*entries) -> dict:
    return {"status": 200, "body": json.dumps(list(entries)).encode().hex()}


def entry(path, date, kind="file"):
    return {"type": kind, "path": path, "lastCommit": {"date": date, "title": "synthetic commit"}}


def raw(body: bytes, status: int = 200) -> dict:
    return {"status": status, "body": body.hex()}


TREE_A = tree(
    entry("a.gguf", "2024-01-02T03:04:05.000Z"),
    entry("b.gguf", "2020-01-01T00:00:00Z"),
    entry("sub", "2024-01-01T00:00:00Z", kind="directory"),
    {"type": "file", "path": "nocommit.gguf"},
    {"type": "file", "path": "nullcommit.gguf", "lastCommit": None},
    entry("emptydate.gguf", ""),
    entry("a.gguf", "2025-06-07T08:09:10.123456Z"),  # a later duplicate wins
)
BASE = [
    done("a.gguf", "'owner/repo-a'"),
    done("a.gguf", "'owner/repo-a'"),
    done("b.gguf", "'owner/repo-a'"),
    done("missing.gguf", "'owner/repo-a'"),
    done("0-url.gguf", "'example.com'"),
    done("c.gguf", "'owner/repo-b'"),
    done("d.gguf", "'owner/repo-c'", status="error"),
    "INSERT INTO update_check VALUES('c.gguf', 'owner/repo-b', '2019-01-01T00:00:00Z', 5.0)",
    "INSERT INTO update_check VALUES('kept.gguf', 'owner/old', '2030-01-01T00:00:00Z', 6.0)",
]
MODELS = ["a.gguf", "b.gguf", "c.gguf", "0-url.gguf", "kept.gguf", "nocommit.gguf"]
HUB = {"/api/models/owner/repo-a/tree/main": TREE_A,
       "/api/models/owner/repo-b/tree/main": raw(b'{"error":"Repository not found"}', 404)}


def one_repo(response: dict, date_file: str = "a.gguf") -> tuple[list, dict]:
    return ([done(date_file, "'owner/repo-a'"), token(f"'{TOKEN}'")],
            {"/api/models/owner/repo-a/tree/main": response})


CASES = [
    ("full", BASE + [token(f"'{TOKEN}'")], HUB),
    ("no-records", ["INSERT INTO update_check VALUES('a.gguf', 'owner/repo-a', '2024-01-02T00:00:00Z', 1.0)"], HUB),
    ("no-token", BASE, HUB),
    ("empty-token", BASE + [token("''")], HUB),
    ("token-trailing-space", BASE + [token("'abc '")], HUB),
    ("token-newline", BASE + [token("'a' || char(10) || 'b'")], HUB),
    ("token-non-ascii", BASE + [token("'tökén'")], HUB),
    ("token-blob", BASE + [token("X'6162275c00'")], HUB),
    ("repo-blob", BASE + [done("zz.gguf", "X'6f2f72'"), token(f"'{TOKEN}'")], HUB),
    ("hub-500", *one_repo(raw(b"oops", 500))),
    ("invalid-json", *one_repo(raw(b"{not json"))),
    ("utf16-body", *one_repo(raw(json.dumps([entry("a.gguf", "2024-02-03T00:00:00Z")]).encode("utf-16")))),
    ("empty-object", *one_repo(raw(b"{}"))),
    ("object", *one_repo(raw(b'{"a": 1}'))),
    ("string-body", *one_repo(raw(b'"abc"'))),
    ("number-body", *one_repo(raw(b"5"))),
    ("entry-not-object", *one_repo(raw(b'[{"type": "dir"}, 5]'))),
    ("lastcommit-string", *one_repo(raw(b'[{"type": "file", "path": "a.gguf", "lastCommit": "x"}]'))),
    ("date-number", *one_repo(raw(b'[{"type": "file", "path": "a.gguf", "lastCommit": {"date": 5}}]'))),
    ("date-float", *one_repo(raw(b'[{"type": "file", "path": "a.gguf", "lastCommit": {"date": 1.5}}]'))),
    ("date-true", *one_repo(raw(b'[{"type": "file", "path": "a.gguf", "lastCommit": {"date": true}}]'))),
    ("date-nan", *one_repo(raw(b'[{"type": "file", "path": "a.gguf", "lastCommit": {"date": NaN}}]'))),
    ("date-list", *one_repo(raw(b'[{"type": "file", "path": "a.gguf", "lastCommit": {"date": [1]}}]'))),
    ("date-list-other-file", *one_repo(raw(b'[{"type": "file", "path": "x.gguf", "lastCommit": {"date": [1]}}]'))),
    ("date-huge-int", *one_repo(raw(b'[{"type": "file", "path": "a.gguf", "lastCommit": {"date": 1180591620717411303424}}]'))),
    ("path-number", *one_repo(raw(b'[{"type": "file", "path": 5, "lastCommit": {"date": "2024-01-01"}}]'))),
    ("path-list", *one_repo(raw(b'[{"type": "file", "path": [1], "lastCommit": {"date": "2024-01-01"}}]'))),
]


class Hub(http.server.BaseHTTPRequestHandler):
    responses: dict = {}
    seen: list = []

    def do_GET(self):  # noqa: N802
        path, _, query = self.path.partition("?")
        Hub.seen.append({"method": "GET", "path": path, "query": query,
                         "authorization": self.headers.get("Authorization")})
        answer = Hub.responses.get(path, {"status": 404, "body": b'{"error":"not found"}'.hex()})
        body = bytes.fromhex(answer["body"])
        self.send_response(answer["status"])
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {
                "MODEL_LOADER_TOKEN", "DATA_DIR", "MODELS_DIR", "MODELS_INI_PATH"} or "PROXY" in key.upper():
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import config, db, hf  # noqa: E402
    from app.main import app  # noqa: E402

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Hub)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    hf.HF_API = f"http://127.0.0.1:{server.server_address[1]}/api"
    hf.time = types.SimpleNamespace(time=lambda: NOW)

    client = TestClient(app, raise_server_exceptions=False)
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest()
              for name in ("api.py", "db.py", "helpers.py", "hf.py", "services.py", "main.py")}
    out = []
    schema = None
    for case_id, seed, hub in CASES:
        Hub.responses = hub
        Hub.seen = []
        with tempfile.TemporaryDirectory() as root:
            config.settings.data_dir = Path(root)
            models = Path(root) / "models"
            models.mkdir()
            for name in MODELS:
                f = models / name
                f.write_bytes(b"GGUF")
                os.utime(f, (MTIME, MTIME))
            config.settings.models_dir = models
            config.settings.models_ini_path = models / "models.ini"
            db._DB_PATH = None
            db.init()
            c = sqlite3.connect(Path(root) / "model_loader.db", isolation_level=None)
            if schema is None:
                schema = [r[0] for r in c.execute(
                    "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")]
            c.execute("DELETE FROM kv")
            for statement in seed:
                c.execute(statement)
            c.close()
            response = client.post("/api/v1/models/check-updates")
            c = sqlite3.connect(Path(root) / "model_loader.db", isolation_level=None)
            # SQL literals: exact values and storage classes, comparable byte for byte.
            table = [list(r) for r in c.execute(
                "SELECT quote(filename), quote(repo_id), quote(hf_last_modified), quote(checked_at)"
                " FROM update_check ORDER BY filename")]
            c.close()
            out.append({"id": case_id, "seed": seed, "models": MODELS, "mtime": MTIME, "hub": hub,
                        "status": response.status_code, "content_type": response.headers.get("content-type"),
                        "body": response.content.decode("utf-8"), "requests": Hub.seen, "update_check": table})
    server.shutdown()
    json.dump({"oracle": digest, "schema": schema, "now": NOW, "cases": out}, sys.stdout, ensure_ascii=False,
              indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
