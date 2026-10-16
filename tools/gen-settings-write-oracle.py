#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/settings-write-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (`api.put_settings`, `db.set_setting`,
`get_settings`) step by step on synthetic stores built by Python's own `db.init()` (WAL, every
connection closed between steps) and records each answer and the stored token after it:

    python3 -I tools/gen-settings-write-oracle.py <services>/model-manager/app > fixture.json

`hf.validate_token` (a Hugging Face call) is replaced by a fixed synthetic check. For each step
that asks for the test, the answer of the child's private hf-token-test call for the same stored
token is recorded too; the Rust test's fake child serves it. Tokens are synthetic.
"""
from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import sys
import tempfile
from pathlib import Path


def put(body):
    return body if isinstance(body, str) else json.dumps(body, ensure_ascii=False)


CASES = [
    ("set-read-test", None, [
        put({"hfToken": "  hf_good_synthetic_0001  "}),
        put({"test": True}),
        put({"hfToken": "hf_bad_synthetic_0002", "test": 1}),
        put({}),
        put({"hfToken": None}),
        put({"hfToken": "", "test": True}),
    ]),
    ("value-forms", "hf_existing_9999", [
        put({"hfToken": 123456789}),
        put({"hfToken": ["a", 1]}),
        put({"hfToken": False}),
        put({"hfToken": "tökén-\U0001F600-xyz"}),
        put('{"hfToken":"x","hfToken":"hf_good_last_wins"}'),
        put({"hfToken": "boom", "test": True}),
        put({"other": "ignored", "test": 0}),
    ]),
    ("body-errors", None, ["[1]", "{bad", "null", ""]),
]


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR"}:
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import config, db, hf  # noqa: E402
    from app.main import app  # noqa: E402

    async def fake(token):
        if token == "boom":
            raise RuntimeError("synthetic failure")
        if not token:
            return True, "anonymous (no token)"
        return token.startswith("hf_good"), f"synthetic check of {len(token)} chars"
    hf.validate_token = fake
    client = TestClient(app, raise_server_exceptions=False)
    digest = {n: hashlib.sha256((app_dir / n).read_bytes()).hexdigest() for n in ("api.py", "db.py", "main.py")}
    out = []
    schema = None
    for case_id, initial, steps in CASES:
        with tempfile.TemporaryDirectory() as root:
            config.settings.data_dir = Path(root)
            db._DB_PATH = None
            db.init()
            path = Path(root) / "model_loader.db"
            c = sqlite3.connect(path)
            if schema is None:
                schema = [r[0] for r in c.execute(
                    "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")]
            if initial is not None:
                c.execute("INSERT INTO kv(key, value) VALUES('hf_token', ?)", (initial,))
                c.commit()
            c.close()
            answers = []
            for body in steps:
                r = client.put("/api/v1/settings", content=body.encode(), headers={"content-type": "application/json"})
                stored = sqlite3.connect(path).execute("SELECT value FROM kv WHERE key='hf_token'").fetchone()
                runtime = None
                if r.status_code == 200 and "test" in r.json():
                    os.environ["NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_WRITE_CONFIRMED"] = "1"
                    runtime = client.get("/api/v1/internal/hf-token-test").content.decode()
                    del os.environ["NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_WRITE_CONFIRMED"]
                answers.append({"status": r.status_code, "content_type": r.headers.get("content-type"),
                                "body": r.content.decode(), "stored": None if stored is None else stored[0],
                                "runtime": runtime})
            out.append({"id": case_id, "initial": initial, "steps": steps, "answers": answers})
    json.dump({"oracle": digest, "schema": schema, "cases": out}, sys.stdout, ensure_ascii=False, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
