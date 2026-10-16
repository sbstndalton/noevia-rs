#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/badges-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, middleware, router,
`api.set_badge` / `api.clear_badge` and `db.badge_set` / `badge_clear` / `badges_for`) against
synthetic SQLite stores and records the exact status, content type and body bytes of each
`PUT` / `DELETE /api/v1/badges` step:

    python3 -I tools/gen-badges-oracle.py <services>/model-manager/app > fixture.json

Every alias, note and row is synthetic. `time.time()` inside `db` is replaced by a fixed clock
(1700000000.25 + 1.5 per call) so `created_at` is reproducible; the Rust test uses the same
clock. Each case starts from a fresh store made by `db.init()` (or none / a broken one) and
runs its steps in order. CI re-runs this against the pinned services ref and requires a
byte-identical fixture. The pinned services ref carries the noevia#1905 fix, so every answer
here, including the `fixed-1905` and `integer-boundaries` cases, is what both sides must give.
"""
from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import sys
import tempfile
import types
from pathlib import Path

J = "application/json"


def put(body, raw: str | None = None, content_type: str | None = J):
    return {"method": "PUT", "body": raw if raw is not None else json.dumps(body, ensure_ascii=False),
            "content_type": content_type}


def delete(query: str):
    return {"method": "DELETE", "query": query}


def ok(alias="m", category="coding", rating=3, **extra):
    return {"alias": alias, "category": category, "rating": rating, **extra}


# id, store ("init" | "missing" | "no-table" | "locked"), seed SQL, steps
CASES = [
    ("put-then-update-and-order", "init", [], [
        put(ok(category="writing", rating=4, note="  first  ")),
        put(ok(category="coding", rating=2, runId=12)),
        put(ok(category="writing", rating=5, note="second", runId="7")),
        put(ok(alias="other", category="tools", rating=1)),
    ]),
    ("note-strip-cap-and-repr", "init", [], [
        put(ok(note=" " + "é" * 600 + " ")),
        put(ok(category="vision", note=5)),
        put(ok(category="tools", note=["a", 1, None, True, 1.5, {"k": "it's"}])),
        put(ok(category="reasoning", note="")),
    ]),
    ("alias-forms", "init", [], [
        put(ok(alias=7)),
        put(ok(alias="")),
        put({"category": "coding", "rating": 3}),
        put(ok(alias=None)),
        put(ok(alias="été \"q\" \\")),
    ]),
    ("category-errors", "init", [], [
        put(ok(category="Coding")),
        put(ok(category="it's")),
        put(ok(category="a'b\"c")),
        put(ok(category=None)),
        put(ok(category=3)),
        put(ok(category="\x01 ")),
    ]),
    ("rating-values", "init", [], [
        put(ok(rating=0)), put(ok(rating=6)), put(ok(rating=-1)), put(ok(rating="3")),
        put(ok(rating=" 4 ")), put(ok(rating="+5")), put(ok(rating=3.0)), put(ok(rating=False)),
        put(ok(rating=None)), put(ok(rating="")), put(ok(rating="1_0")),
        put(None, raw='{"alias":"m","category":"coding","rating":1e300}'),
        put(None, raw='{"alias":"m","category":"coding","rating":1000000000000000000000000000000}'),
        put(None, raw='{"alias":"","category":"coding","rating":1000000000000000000000000000000}'),
    ]),
    ("run-id-values", "init", [], [
        put(ok(runId=0)), put(ok(runId=None)), put(ok(runId=-3)), put(ok(runId=" 9 ")),
        put(ok(runId=9223372036854775807)),
    ]),
    ("body-errors", "init", [], [
        put(None, raw="{not json"),
        put(None, raw="[1, 2]"),
        put(None, raw="null"),
        put(None, raw=""),
        put(None, raw='"text"'),
        put(None, raw='{"alias":"m","category":"coding","rating":3}', content_type="text/plain"),
        put(None, raw='{"alias":"x","alias":"m","category":"coding","rating":3}'),
        put(None, raw='{"alias":"m","category":"coding","rating":3}', content_type=None),
    ]),
    ("delete-forms", "init", [], [
        put(ok(category="coding")), put(ok(category="tools")),
        delete("alias=m&category=coding"),
        delete("category=tools"),
        delete(""),
        delete("alias=&category=tools"),
        delete("alias=x&alias=m&category=nope&category=tools"),
        delete("alias=other+m&category=tools"),
        delete("alias=m&category=unknown"),
    ]),
    ("seeded-odd-rows", "init", [
        "INSERT INTO model_badge(alias, category, rating, note, run_id, created_at) VALUES('m','coding',3,'n',NULL,1.0)",
        "INSERT INTO model_badge(alias, category, rating, note, run_id, created_at) VALUES('m','zz',9,'',5,123456789012345680.0)",
        "INSERT INTO model_badge(alias, category, rating, note, run_id, created_at) VALUES('m','aa',2,'',NULL,0.0001)",
    ], [delete("alias=m&category=none")]),
    # noevia#1905 forms (answered 500 or truncated before the fix).
    ("fixed-1905", "init", [], [
        put(ok(rating="abc")),
        put(ok(rating=True)),
        put(ok(category="tools", rating=2.9)),
        put(ok(rating=[1])),
        put(ok(runId="x")),
        put(None, raw='{"alias":"m","category":"coding","rating":3,"runId":1180591620717411303424}'),
        put(ok(alias=" p ")),
        delete("alias=%20p%20&category=coding"),
        delete("alias=p&category=none"),
    ]),
    # noevia#1910: i64's edges as integers, strings and floats, for rating and runId.
    ("integer-boundaries", "init", [], [
        put(None, raw='{"alias":"m","category":"coding","rating":3,"runId":9223372036854775807}'),
        put(None, raw='{"alias":"m","category":"coding","rating":3,"runId":-9223372036854775808}'),
        put(None, raw='{"alias":"m","category":"coding","rating":3,"runId":9223372036854775808}'),
        put(None, raw='{"alias":"m","category":"coding","rating":3,"runId":-9223372036854775809}'),
        put(ok(runId="-9223372036854775808")),
        put(ok(runId="9223372036854775807")),
        put(ok(runId="9223372036854775808")),
        put(ok(runId="-9223372036854775809")),
        put(None, raw='{"alias":"m","category":"coding","rating":3,"runId":9.2e18}'),
        put(None, raw='{"alias":"m","category":"coding","rating":3,"runId":9223372036854774784.0}'),
        put(None, raw='{"alias":"m","category":"coding","rating":3,"runId":9223372036854775808.0}'),
        put(None, raw='{"alias":"m","category":"coding","rating":3,"runId":-9223372036854775808.0}'),
        put(None, raw='{"alias":"m","category":"coding","rating":3,"runId":-9.3e18}'),
        put(None, raw='{"alias":"m","category":"coding","rating":-9223372036854775809}'),
        put(ok(rating="-9223372036854775808")),
        put(None, raw='{"alias":"m","category":"coding","rating":5.0,"runId":-0.0}'),
    ]),
    ("missing-store", "missing", [], [put(ok())]),
    ("no-table", "no-table", [], [put(ok()), delete("alias=m&category=coding")]),
    ("locked-store", "locked", [], [put(ok())]),
]


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR"}:
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import config, db  # noqa: E402
    from app.main import app  # noqa: E402

    client = TestClient(app, raise_server_exceptions=False)  # no lifespan
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest()
              for name in ("api.py", "db.py", "main.py")}
    out = []
    for case_id, store, seed, steps in CASES:
        calls = [0]

        def fake_time():
            calls[0] += 1
            return 1700000000.25 + 1.5 * calls[0]

        db.time = types.SimpleNamespace(time=fake_time)
        with tempfile.TemporaryDirectory() as root:
            config.settings.data_dir = Path(root)
            db._DB_PATH = None
            path = Path(root) / "model_loader.db"
            holder = None
            if store in ("init", "locked"):
                db.init()
                c = sqlite3.connect(path, isolation_level=None)
                for statement in seed:
                    c.execute(statement)
                c.close()
            elif store == "no-table":
                c = sqlite3.connect(path, isolation_level=None)
                c.execute("PRAGMA journal_mode=WAL")
                c.execute("CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                c.close()
            if store == "locked":
                holder = sqlite3.connect(path, isolation_level=None)
                holder.execute("BEGIN EXCLUSIVE")
            answers = []
            for step in steps:
                if step["method"] == "PUT":
                    headers = {}
                    if step["content_type"]:
                        headers["content-type"] = step["content_type"]
                    response = client.put("/api/v1/badges", content=step["body"].encode("utf-8"), headers=headers)
                else:
                    url = "/api/v1/badges" + ("?" + step["query"] if step["query"] else "")
                    response = client.delete(url)
                answers.append({"status": response.status_code,
                                "content_type": response.headers.get("content-type"),
                                "body": response.content.decode("utf-8")})
            if holder is not None:
                holder.execute("ROLLBACK")
                holder.close()
            out.append({"id": case_id, "store": store, "seed": seed, "steps": steps, "answers": answers})
    json.dump({"oracle": digest, "cases": out}, sys.stdout, ensure_ascii=False, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
