#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/draft-heads-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, middleware and router,
`api.section_draft_heads`, `api._builtin_mtp_layers`, `helpers._resolve_section_gguf`, `ini`,
`autoconfig._find_mtp` with `autoconfig_core.pick_file`, `gguf_meta`, `db.repo_for_file`,
`hf.repo_detail` and `model_files` through the real model-files binary) against synthetic model
folders, models.ini files and stores, with the Hub replaced by a local fake on 127.0.0.1, and
records for each request:

  * the exact status, content type and body of `GET /api/v1/sections/{name}/draft-heads`,
  * every request the fake Hub received (path and query, Authorization header).

    MODEL_FILES_BIN=<model-files> python3 -I tools/gen-draft-heads-oracle.py \\
        <services>/model-manager/app > fixture.json

Every file name, section, repository and token is synthetic; GGUF files are tiny synthetic
headers and other files are zeros of a given size (sparse). Nothing reaches the real Hub. The
fake Hub's own address is written as @HUB@ in the Link headers it sends (the native test serves
the same answers from its own fake).
"""
from __future__ import annotations

import hashlib
import http.server
import json
import os
import sqlite3
import struct
import sys
import tempfile
import threading
from pathlib import Path

TOKEN = "hf_synthetic_draft_token_0123456789"
GIB = 1024 ** 3


def gguf(arch: str = "llama", nextn=None, extra: dict | None = None) -> dict:
    kv = {"general.architecture": ["str", arch]}
    if nextn is not None:
        kv[f"{arch}.nextn_predict_layers"] = nextn
    kv.update(extra or {})
    return {"gguf": kv}


def f(name: str, size: int = 16, **spec) -> dict:
    return {"name": name, "size": size, **spec}


def g(name: str, **kw) -> dict:
    return {"name": name, **gguf(**kw)}


def build_gguf(kv: dict) -> bytes:
    out = bytearray(b"GGUF")
    out += struct.pack("<IQQ", 3, 0, len(kv))
    for key, (kind, value) in kv.items():
        k = key.encode()
        out += struct.pack("<Q", len(k)) + k
        if kind == "str":
            v = value.encode()
            out += struct.pack("<IQ", 8, len(v)) + v
        elif kind == "u32":
            out += struct.pack("<II", 4, value)
        elif kind == "i32":
            out += struct.pack("<Ii", 5, value)
        elif kind == "f32":
            out += struct.pack("<If", 6, value)
        elif kind == "bool":
            out += struct.pack("<IB", 7, 1 if value else 0)
        elif kind == "u64":
            out += struct.pack("<IQ", 10, value)
        else:
            raise ValueError(kind)
    return bytes(out)


def done(filename: str, repo: str, status: str = "done") -> str:
    return (f"INSERT INTO download_history(repo_id, filename, dest_path, total_bytes, status, started_at)"
            f" VALUES({repo}, '{filename}', '/models/{filename}', 1, '{status}', 1.0)")


def token(value: str) -> str:
    return f"INSERT INTO kv(key, value) VALUES('hf_token', {value})"


def entry(path, size=None, kind="file", lfs=None):
    e = {"type": kind, "oid": "0" * 40, "path": path}
    if size is not None:
        e["size"] = size
    if lfs is not None:
        e["lfs"] = lfs
    return e


def page(entries, status=200, link=None, raw: bytes | None = None, content_type="application/json"):
    body = raw if raw is not None else json.dumps(entries).encode()
    headers = [["content-type", content_type]]
    if link is not None:
        headers.append(["link", link])
    return {"status": status, "headers": headers, "body": body.hex()}


TREE = "/api/models/{repo}/tree/main?recursive=true&expand=true"


def tree(repo: str) -> str:
    return TREE.format(repo=repo)


REMOTE_HEADS = [
    entry("README.md", 100),
    entry("m-Q4_K_M.gguf", 5 * GIB, lfs={"size": 5 * GIB, "sha256": "0" * 64}),
    entry("MTP", kind="directory"),
    entry("MTP/m-mtp-BF16.gguf", 300, lfs={"size": 300}),
    entry("MTP/m-mtp-Q8_0.gguf", 0, lfs={"size": 160}),
    entry("m-mtp-F16.gguf", 300),
    entry("m-draft-Q4_0.gguf", 90),
    entry("mmproj-m-mtp-F16.gguf", 50),
    entry("m-noMTP-Q4_K_M.gguf", 40),
    entry("m-mtp-zero.gguf", 0),
    entry("m-mtp-huge.gguf", 2 * GIB + 1),
    entry("m-mtp-edge.gguf", 2 * GIB),
    entry("draft-tiny.GGUF", 7),
    entry("notes-mtp-.txt", 5),
]

HUB_TWO_PAGES = {
    tree("acme/m-GGUF"): page(REMOTE_HEADS[:6],
                              link='<@HUB@/api/models/acme/m-GGUF/tree/main?recursive=true&expand=true&cursor=p2>;'
                                   ' rel="next"'),
    tree("acme/m-GGUF") + "&cursor=p2": page(REMOTE_HEADS[6:]),
}

SUB = [g("org/m-Q4_K_M.gguf", nextn=None)]
SUB_SEED = [done("org/m-Q4_K_M.gguf", "'acme/m-GGUF'")]

CASES: list[dict] = [
    {"id": "no-file", "files": [f("other.txt")], "names": ["missing", "m"]},
    {"id": "flat-no-repo", "files": [g("m.gguf")], "names": ["m"]},
    {"id": "flat-head-beside", "files": [g("m-Q4.gguf"), g("m-Q4-mtp-Q8_0.gguf"), f("m-Q4-mtp-BF16.gguf", 64),
                                         f("other-mtp-Q8_0.gguf", 4), f("mmproj-m-Q4-mtp.gguf", 2)],
     "names": ["m-Q4"]},
    {"id": "subdir-mtp-folder", "files": [g("org/m-Q4_K_M.gguf"), f("org/MTP/m-mtp-BF16.gguf", 300),
                                          f("org/MTP/m-mtp-Q8_0.gguf", 160), f("org/MTP/m-mtp-Q4_0.gguf", 160),
                                          f("org/MTP/mmproj-mtp.gguf", 10), f("org/m-mtp-Q2.gguf", 50)],
     "names": ["m-Q4_K_M"]},
    {"id": "subdir-beside", "files": [g("org/m-Q4_K_M.gguf"), f("org/m-Q4_K_M-mtp-Q8_0.gguf", 50),
                                      f("org/m-Q4_K_M-mtp-huge.gguf", 2 * GIB + 1),
                                      f("org/m-Q4_K_M-mtp-edge.gguf", 2 * GIB)],
     "names": ["m-Q4_K_M"]},
    {"id": "builtin-layers", "files": [g("nx/n-Q4.gguf", nextn=["u32", 1]), g("fl.gguf", nextn=["u32", 3]),
                                       g("zero.gguf", nextn=["u32", 0]), g("neg.gguf", nextn=["i32", -2]),
                                       g("big.gguf", nextn=["u64", 2 ** 40]), g("flt.gguf", nextn=["f32", 2.75])],
     "names": ["n-Q4", "fl", "zero", "neg", "big", "flt"]},
    {"id": "bad-header", "files": [f("broken.gguf", 64), f("sub/broken2.gguf", 3)], "names": ["broken", "broken2"]},
    {"id": "explicit-model-in-shared-folder",
     "files": [g("sub/a.gguf", nextn=["u32", 0]), g("sub/b.gguf", nextn=["u32", 2]), f("sub/mmproj-b.gguf", 8)],
     "ini": "[b]\nmodel = /models/sub/b.gguf\n\n[c]\nmodel = sub/b.gguf\n\n[d]\nmodel = /models/sub/missing.gguf\n",
     "names": ["b", "c", "d", "a"]},
    {"id": "first-sorted-in-folder", "files": [g("sub/b.gguf", nextn=["u32", 4]), g("sub/a.gguf", nextn=["u32", 0])],
     "names": ["b"]},
    {"id": "default-section-model", "files": [g("x.gguf", nextn=["u32", 5]), g("y.gguf")],
     "ini": "[DEFAULT]\nmodel = /models/x.gguf\n\n[y]\nctx-size = 4096\n", "names": ["y", "x", "DEFAULT"]},
    {"id": "ini-preamble-only", "files": [g("m.gguf")], "ini": "no header line\n", "names": ["m"]},
    {"id": "ini-duplicate-option", "files": [g("m.gguf")], "ini": "[m]\nk = 1\nk = 2\n", "names": ["m"]},
    {"id": "ini-duplicate-section", "files": [g("m.gguf", nextn=["u32", 1]), g("n.gguf")],
     "ini": "[m]\nmodel = /models/n.gguf\n\n[m]\nctx-size = 1\n", "names": ["m"]},
    {"id": "explicit-model-not-a-file", "files": [g("m.gguf", nextn=["u32", 2]), f("dir.gguf/inner.txt", 1)],
     "ini": "[m]\nmodel = /models/dir.gguf\n\n[n]\nmodel = /models/m.gguf\n", "names": ["m", "n"]},
    {"id": "sharded", "files": [g("sh/s-00001-of-00002.gguf", nextn=["u32", 1]), f("sh/s-00002-of-00002.gguf", 8),
                                f("sh/MTP/s-mtp-Q8_0.gguf", 12)],
     "names": ["s"]},
    {"id": "mtp-folder-other-entries", "files": [g("o/m.gguf"), f("o/MTP/sub-mtp-x.gguf/inner", 1),
                                                 f("o/MTP/m-mtp-a.gguf", 30)],
     "links": [["o/MTP/m-mtp-b.gguf", "missing-target"], ["o/MTP/m-mtp-c.gguf", "m-mtp-a.gguf"]],
     "names": ["m"]},
    {"id": "odd-names", "files": [g("a b.gguf"), g("ü-mtp-x.gguf"), g("q.gguf")],
     "names": ["a%20b", "%C3%BC", "q"]},
    {"id": "repo-remote-two-pages", "files": SUB, "seed": SUB_SEED + [token("'" + TOKEN + "'")],
     "hub": HUB_TWO_PAGES, "names": ["m-Q4_K_M"]},
    {"id": "repo-like-and-status", "files": [g("org/m-Q4_K_M.gguf"), g("n.gguf")],
     "seed": [done("deep/org/m-Q4_K_M.gguf", "'acme/old-GGUF'"), done("org/m-Q4_K_M.gguf", "'acme/m-GGUF'"),
              done("org/m-Q4_K_M.gguf", "'acme/err-GGUF'", status="error"), done("org/m-Q4_K_M.gguf", "'url.example'"),
              done("N.GGUF", "'acme/n'"), done("xn.gguf", "'acme/xn'")],
     "hub": {tree("acme/m-GGUF"): page([entry("m-mtp-Q8.gguf", 10)]),
             tree("acme/n"): page([])},
     "names": ["m-Q4_K_M", "n"]},
    {"id": "sibling-mtp-build", "files": SUB, "seed": SUB_SEED,
     "hub": {tree("acme/m-GGUF"): page([entry("m-Q4_K_M.gguf", 100)]),
             tree("acme/m-MTP-GGUF"): page([entry("m-MTP-Q4_K_M.gguf", 100)])},
     "names": ["m-Q4_K_M"]},
    {"id": "sibling-missing", "files": SUB, "seed": SUB_SEED,
     "hub": {tree("acme/m-GGUF"): page([])}, "names": ["m-Q4_K_M"]},
    {"id": "sibling-not-asked", "files": [g("org/m-Q4_K_M.gguf", nextn=["u32", 1]), g("p.gguf"), g("q.gguf")],
     "seed": SUB_SEED + [done("p.gguf", "'acme/p-MTP-GGUF'"), done("q.gguf", "'acme/q-gguf'")],
     "hub": {tree("acme/m-GGUF"): page([]), tree("acme/p-MTP-GGUF"): page([]),
             tree("acme/q-gguf"): page([]), tree("acme/q-MTP-GGUF"): page([], status=404)},
     "names": ["m-Q4_K_M", "p", "q"]},
    {"id": "hub-errors", "files": [g("e1.gguf"), g("e2.gguf"), g("e3.gguf"), g("e4.gguf"), g("e5.gguf")],
     "seed": [done("e1.gguf", "'acme/e1'"), done("e2.gguf", "'acme/e2'"), done("e3.gguf", "'acme/e3'"),
              done("e4.gguf", "'acme/e4'"), done("e5.gguf", "'acme/e5'")],
     "hub": {tree("acme/e1"): page([], status=500),
             tree("acme/e2"): page(None, raw=b"not json"),
             tree("acme/e3"): page(None, raw=b'{"error": "a dict, not a list"}'),
             tree("acme/e4"): page([{"type": "file", "path": ["not", "a", "str"]}]),
             tree("acme/e5"): page([entry("e5-mtp-x.gguf", 3)],
                                   link='<@HUB@/api/models/acme/e5/tree/main?recursive=true&expand=true&cursor=gone>;'
                                        ' rel="next"')},
     "names": ["e1", "e2", "e3", "e4", "e5"]},
    {"id": "token-int", "files": [g("t.gguf")], "seed": [done("t.gguf", "'acme/t'"), token("7")],
     "hub": {tree("acme/t"): page([entry("t-draft-x.gguf", 2)])}, "names": ["t"]},
]


class Hub(http.server.BaseHTTPRequestHandler):
    responses: dict = {}
    seen: list = []
    base = ""

    def log_message(self, *args):  # noqa: D401 - silence
        pass

    def do_GET(self):  # noqa: N802
        Hub.seen.append({"path": self.path, "authorization": self.headers.get("authorization")})
        answer = Hub.responses.get(self.path)
        if answer is None:
            body = b'{"error":"Repository not found"}'
            self.send_response(404)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        body = bytes.fromhex(answer["body"])
        self.send_response(answer["status"])
        for k, v in answer["headers"]:
            self.send_header(k, v.replace("@HUB@", Hub.base))
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def build(models: Path, case: dict) -> None:
    for spec in case["files"]:
        path = models / spec["name"]
        path.parent.mkdir(parents=True, exist_ok=True)
        if "gguf" in spec:
            path.write_bytes(build_gguf(spec["gguf"]))
        else:
            with open(path, "wb") as handle:
                handle.truncate(spec["size"])
    for name, target in case.get("links", []):
        os.symlink(target, models / name)
    if case.get("ini") is not None:
        (models / "models.ini").write_text(case["ini"], encoding="utf-8")


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith(("NOEVIA_", "GGUF_")) or key.upper() in {
                "MODEL_LOADER_TOKEN", "DATA_DIR", "MODELS_DIR", "MODELS_INI_PATH"} or "PROXY" in key.upper():
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import config, db, hf  # noqa: E402
    from app.main import app  # noqa: E402

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Hub)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    Hub.base = f"http://127.0.0.1:{server.server_address[1]}"
    hf.HF_API = Hub.base + "/api"

    client = TestClient(app, raise_server_exceptions=False)  # no lifespan: no sampler, no startup
    names = ("api.py", "helpers.py", "ini.py", "autoconfig.py", "autoconfig_core.py", "gguf_meta.py", "db.py",
             "hf.py", "model_files.py", "utils.py", "config.py", "main.py")
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest() for name in names}
    out = []
    schema = None
    for case in CASES:
        Hub.responses = case.get("hub", {})
        with tempfile.TemporaryDirectory() as root:
            config.settings.data_dir = Path(root)
            models = Path(root) / "models"
            models.mkdir()
            build(models, case)
            config.settings.models_dir = models
            config.settings.models_ini_path = models / "models.ini"
            db._DB_PATH = None
            db.init()
            c = sqlite3.connect(Path(root) / "model_loader.db", isolation_level=None)
            if schema is None:
                schema = [r[0] for r in c.execute(
                    "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")]
            c.execute("DELETE FROM kv")
            for statement in case.get("seed", []):
                c.execute(statement)
            c.close()
            results = []
            for name in case["names"]:
                Hub.seen = []
                response = client.get(f"/api/v1/sections/{name}/draft-heads")
                results.append({"name": name, "status": response.status_code,
                                "content_type": response.headers.get("content-type"),
                                "body": response.content.decode("utf-8"), "requests": Hub.seen})
            out.append({**case, "results": results})
    server.shutdown()
    json.dump({"oracle": digest, "schema": schema, "cases": out}, sys.stdout, ensure_ascii=True, indent=1,
              sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
