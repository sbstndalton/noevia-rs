"""Records the Diary's POST /api/external-sources/import (app.py `api_external_sources_import`) over
one synthetic managed tenant and a synthetic source folder, for tests/external_import.rs (track R9,
shift 4).

Run with CPython 3.12 and the frozen noevia-services checkout (read-only):

    python3.12 -I tests/generate_external_import_fixtures.py <services>/diary tests/external-import-fixtures.json

The app is imported in a clean environment (TZ=UTC, DIARY_EXTERNAL_SOURCES = the synthetic folder)
and called through FastAPI's TestClient (no lifespan). The source files and their modification
times are fixed and recorded, so the Rust test recreates them. Recorded per case: the request, the
status, the JSON reply (exchange ids masked) and the corpus afterwards. Synthetic text only.
"""
import hashlib
import json
import os
import re
import sqlite3
import sys
import tempfile
from pathlib import Path

if sys.version_info[:2] != (3, 12):
    sys.exit("record with CPython 3.12")
diary = Path(sys.argv[1]).resolve()
out = Path(sys.argv[2]).resolve()
SOURCES = ["agent/app.py", "agent/external_sources.py", "agent/corpus_store.py", "agent/corpus.py"]
UID = "5a0c3e2f-9d1b-4c7a-8e6f-0b1c2d3e4f50"
TOKEN = "qa-service-token-import"
scratch = Path(tempfile.mkdtemp(prefix="qa-external-import-"))
source = scratch / "qa-source"
# name -> (text bytes, mtime): every name dated except two, whose day comes from the mtime.
NOON_2026_01_03 = 1767441600  # 2026-01-03T12:00:00Z
FILES = {
    "2026-01-05 garden.md": ("Synthetic QA garden notes: beans by the canal.\n".encode(), NOON_2026_01_03),
    "notes/2026_01_04-kites.txt": ("Flew a synthetic QA kite.\n".encode(), NOON_2026_01_03),
    "undated walk.md": ("A synthetic QA walk, date from the file time.\n".encode(), NOON_2026_01_03),
    "2026-02-30 not a date.md": ("Synthetic QA: the name's date is not a real day.\n".encode(), NOON_2026_01_03),
    "empty.md": (b"   \n", NOON_2026_01_03),
    "latin1.md": ("Synthetic QA café".encode("latin-1"), NOON_2026_01_03),
    "picture.png": (b"\x89PNG synthetic", NOON_2026_01_03),
    "big.md": (b"Q" * (2 * 1024 * 1024 + 1), NOON_2026_01_03),
}
for name, (data, mtime) in FILES.items():
    path = source / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    os.utime(path, (mtime, mtime))
state_dir = scratch / "state"
state_dir.mkdir()
CONFIG = (
    "llm:\n  base_url: http://127.0.0.1:9/v1\n  chat_model: qa-chat\n"
    "  aux:\n    base_url: http://127.0.0.1:9/v1\n    model: qa-aux\n"
    "corpus:\n  backend: local\n  root: ''\n  monthly_prefix: ''\n  entry_layout: daily\n  entries_prefix: Entries\n"
    "  index_enabled: false\n  month_file_template: '{year}-{month02}.md'\n  index_file: INDEX.md\n"
    f"  local:\n    root: {scratch / 'default'}\n"
    f"retrieval:\n  db_path: {state_dir / 'index.db'}\n"
    f"ui:\n  auth_token: {TOKEN}\n"
)
(scratch / "config.yaml").write_text(CONFIG, encoding="utf-8")
os.environ.clear()
os.environ.update({"PATH": "/usr/bin:/bin", "HOME": str(scratch / "home"), "TZ": "UTC",
                   "DIARY_CONFIG": str(scratch / "config.yaml"), "DIARY_ALLOW_OPEN": "1",
                   "DIARY_CORPUS_WRITE_IMPL": "python", "DIARY_EXTERNAL_SOURCES": str(source)})
os.chdir(scratch)
sys.path.insert(0, str(diary))
from fastapi.testclient import TestClient  # noqa: E402
from agent import app as appmod  # noqa: E402
from agent import tenant_writer  # noqa: E402
from agent.managed_storage import ManagedCorpusBackend  # noqa: E402

CORPUS = {
    "Entries/2026/January/January 2, 2026.md":
        "## Friday, January 2, 2026\n\n### 08:30 — Garden\n\n**Me:** Synthetic QA plan: plant beans.\n",
}
root = state_dir / "users" / UID
managed = ManagedCorpusBackend(root, UID)
with tenant_writer.hold(UID):
    managed.activate({p: t.encode() for p, t in CORPUS.items()},
                     {"entry_layout": "daily", "month_file_template": "{year}-{month02}.md", "index_enabled": False})
managed.close()
client = TestClient(appmod.app, raise_server_exceptions=False)
HEADERS = {"Authorization": f"Bearer {TOKEN}", "X-Cowork-User-ID": UID}
SRC = str(source)
CASES = [
    ("dated-name", {"source_path": SRC, "rel_path": "2026-01-05 garden.md"}),
    ("nested-underscore-date", {"source_path": SRC, "rel_path": "notes/2026_01_04-kites.txt"}),
    ("mtime-date", {"source_path": SRC, "rel_path": "undated walk.md"}),
    ("impossible-name-date", {"source_path": SRC, "rel_path": "2026-02-30 not a date.md"}),
    ("again", {"source_path": SRC, "rel_path": "2026-01-05 garden.md"}),
    ("not-configured", {"source_path": str(scratch), "rel_path": "qa-source/undated walk.md"}),
    ("traversal", {"source_path": SRC, "rel_path": "../config.yaml"}),
    ("absolute", {"source_path": SRC, "rel_path": str(source / "undated walk.md")}),
    ("missing", {"source_path": SRC, "rel_path": "nope.md"}),
    ("wrong-extension", {"source_path": SRC, "rel_path": "picture.png"}),
    ("empty", {"source_path": SRC, "rel_path": "empty.md"}),
    ("not-utf8", {"source_path": SRC, "rel_path": "latin1.md"}),
    ("too-big", {"source_path": SRC, "rel_path": "big.md"}),
]
xids = {}


def mask(text):
    return re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
                  lambda m: m.group(0) if m.group(0) == UID else xids.setdefault(m.group(0), f"XID-{len(xids) + 1}"),
                  text)


def corpus():
    db = sqlite3.connect(root / "managed-diary.db")
    try:
        return {p: mask(bytes(d).decode()) for p, d in db.execute("SELECT path, data FROM files ORDER BY path")}
    finally:
        db.close()


records = []
for label, request in CASES:
    r = client.post("/api/external-sources/import", json=request, headers=HEADERS)
    records.append({"case": label, "request": json.loads(json.dumps(request).replace(str(scratch), "SCRATCH")),
                    "status": r.status_code, "json": json.loads(mask(json.dumps(r.json(), ensure_ascii=False))),
                    "corpus": corpus()})
fixture = {
    "source": {p: hashlib.sha256((diary / p).read_bytes()).hexdigest() for p in SOURCES},
    "python": "%d.%d.%d" % sys.version_info[:3],
    "uid": UID,
    "token": TOKEN,
    "files": {name: {"hex": data.hex() if len(data) < 4096 else None, "size": len(data),
                     "fill": "Q" if len(data) >= 4096 else None, "mtime": mtime}
              for name, (data, mtime) in FILES.items()},
    "corpus_before": CORPUS,
    "cases": records,
}
out.write_text(json.dumps(fixture, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
print("recorded", len(records), "cases")
