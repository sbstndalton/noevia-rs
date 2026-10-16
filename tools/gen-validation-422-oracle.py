#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/validation-422-python.v1.json (noevia#1908).

Runs the ACTUAL noevia-services model-manager FastAPI app and records the raw bytes of its 422
validation answers (dict bodies, path ints, query bools), so the native bodies can be compared
byte for byte, key order included:

    python3 -I tools/gen-validation-422-oracle.py <services>/model-manager/app > fixture.json

Validation runs before every handler, so no store or model file is touched.
"""
from __future__ import annotations

import hashlib
import json
import os
import sys
from pathlib import Path

J = "application/json"
BODIES = [
    ("", J), ("null", J), ("[]", J), ("[1, 2.5, 1e300, -0.0, 100000000000000000000000]", J),
    ('"text"', J), ("true", J), ("3", J), ("NaN", J), ("[Infinity, -Infinity]", J),
    ('[{"b": 1, "a": 2, "b": 3}]', J), ('[{"z": {"y": 1, "x": [true, null]}}]', J),
    ('["é\U0001F600", "q\\"\\\\\\n\\t\\u0001\u007f"]', J),
    ("{not json", J), ('{"a" 1}', J), ('{"a": 1,}', J), ("[1, 2", J), ('{"a": 1} x', J),
    ('"unterminated', J), ('"bad \\q"', J), ('"\\u12"', J), ("", None),
    ('{"name": "n", "body": "b"}', "text/plain"), ("plain text é", "text/plain"),
]
PATHS = ["abc", "1.5", "%20"]
QUERIES = ["maybe", "", "2"]


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() == "MODEL_LOADER_TOKEN":
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app.main import app  # noqa: E402

    client = TestClient(app, raise_server_exceptions=False)
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest() for name in ("api.py", "main.py")}
    cases = []
    for body, content_type in BODIES:
        headers = {"content-type": content_type} if content_type else {}
        r = client.post("/api/v1/prompts", content=body.encode("utf-8"), headers=headers)
        cases.append({"kind": "body", "body": body, "content_type": content_type,
                      "status": r.status_code, "answer": r.content.decode("utf-8")})
    for raw in PATHS:
        r = client.delete(f"/api/v1/prompts/{raw}")
        cases.append({"kind": "pid", "raw": raw, "status": r.status_code, "answer": r.content.decode("utf-8")})
    for raw in QUERIES:
        r = client.get("/api/v1/sections/synthetic", params={"defaults": raw})
        cases.append({"kind": "defaults", "raw": raw, "status": r.status_code, "answer": r.content.decode("utf-8")})
    json.dump({"oracle": digest, "cases": cases}, sys.stdout, ensure_ascii=False, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
