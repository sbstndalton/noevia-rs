#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/section-writes-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, `api.save_section`,
`replace_models_ini`, `rename_section`, `delete_section`, `register_safe_defaults` and every
`ini.py` helper they use) step by step against synthetic models folders and records each answer
plus the files left behind (models.ini bytes, models.roles.json, the directory listing):

    MODEL_FILES_BIN=<model-files> python3 -I tools/gen-section-writes-oracle.py <services>/model-manager/app > fixture.json

`time.strftime` inside `ini` is pinned to one UTC instant so backup names are reproducible; the
Rust test pins its clock to the same instant. `{REV}` in a step is replaced by the sha256 of
models.ini as it is at that step. For safe defaults, the answer of the child's private
safe-defaults-plan call for the same state is recorded too (the native writer asks for it), then
the Python route itself runs. `MODEL_FILES_IMPL=rust` cases use the real `model-files` binary.
Every file is synthetic; the GGUF files are tiny headers with no tensors.
"""
from __future__ import annotations

import calendar
import hashlib
import json
import os
import struct
import sys
import tempfile
import types
from pathlib import Path

STAMP = "20261016-120000"
EPOCH = calendar.timegm((2026, 10, 16, 12, 0, 0, 0, 0, 0))


def gguf(arch: str = "llama", ctx: int = 32768, extra: list | None = None) -> str:
    """A GGUF v3 header with no tensors, as hex."""
    kv = [("general.architecture", 8, arch), (f"{arch}.context_length", 4, ctx)] + (extra or [])
    out = b"GGUF" + struct.pack("<IQQ", 3, 0, len(kv))
    for key, kind, value in kv:
        k = key.encode()
        out += struct.pack("<Q", len(k)) + k + struct.pack("<I", kind)
        if kind == 8:
            v = value.encode()
            out += struct.pack("<Q", len(v)) + v
        elif kind == 4:
            out += struct.pack("<I", value)
    return out.hex()


def put(path, body, ct="application/json"):
    return {"method": "PUT", "path": path, "body": body if isinstance(body, str) else json.dumps(body, ensure_ascii=False), "content_type": ct}


def post(path, body=None):
    return {"method": "POST", "path": path, "body": None if body is None else (body if isinstance(body, str) else json.dumps(body, ensure_ascii=False)), "content_type": "application/json"}


def delete(path):
    return {"method": "DELETE", "path": path, "body": None, "content_type": None}


R = "{REV}"
BASE = ("version = 1\n\n# top comment\n[*]\ncache-ram = 512\n\n[a]\n; note about a\nmodel = /models/a.gguf\n"
        "ctx-size = 2048\ntemp = 0.7\n\n[b]\nmodel = /models/b.gguf\nmulti = one\n  two\n\n")
DEFAULTS = "[DEFAULT]\nthreads = 4\ncache-ram = -1\n\n[a]\nmodel = /models/a.gguf\n"


def case(cid, steps, ini=None, roles=None, files=(), impl="python", hard=2048, cap=1024):
    return {"id": cid, "ini": ini, "roles": roles, "files": list(files), "impl": impl,
            "hard": hard, "cap": cap, "steps": steps}


CASES = [
    case("save-new-file", [
        put("/api/v1/sections/a", {"baseRevision": R, "values": {"model": "/models/a.gguf", "ctx-size": " 4096 ", "jinja": True,
                                                                "swa-full": "on", "no-host": 1, "check-tensors": "yes", "cpu-moe": 1.0},
                                   "extras": "temp = 0.7\n# c\n; d\nfoo=bar\nbad line\n=x\n  spaced  =  value  \nfoo = baz"}),
    ]),
    case("save-preserving", [
        put("/api/v1/sections/a", {"baseRevision": R, "values": {"ctx-size": "8192", "n-gpu-layers": 99}, "extras": "temp = 0.7"}),
        put("/api/v1/sections/new-one", {"baseRevision": R, "values": {"model": "/models/n.gguf"}}),
        put("/api/v1/sections/b", {"baseRevision": R, "values": {"model": "/models/b.gguf"}, "extras": "multi = one"}),
    ], ini=BASE),
    case("save-errors", [
        put("/api/v1/sections/a%20b", {"baseRevision": R}),
        put("/api/v1/sections/a", {"values": {}}),
        put("/api/v1/sections/a", {"baseRevision": "stale", "values": {}}),
        put("/api/v1/sections/a", {"baseRevision": 5, "values": {}}),
        put("/api/v1/sections/a", {"baseRevision": R, "values": {"model": "x\ny"}}),
        put("/api/v1/sections/a", {"baseRevision": R, "values": {}, "extras": "ok = 1\n  [x]"}),
        put("/api/v1/sections/a", {"baseRevision": R, "values": ["not", "a", "dict"]}),
        put("/api/v1/sections/a", {"baseRevision": R, "values": "text"}),
        put("/api/v1/sections/DEFAULT", {"baseRevision": R, "values": {}}),
        put("/api/v1/sections/a", "[1]"),
        put("/api/v1/sections/a", "{bad"),
        put("/api/v1/sections/a", {"baseRevision": R, "values": {"ctx-size": 12, "temp": 1.5, "model": None, "top-k": False,
                                                                "min-p": [1, "x"], "seed": {"k": "v"}}}),
    ], ini=BASE),
    case("save-cache-ram", [
        put("/api/v1/sections/my-embed", {"baseRevision": R, "values": {"model": "/models/e.gguf"}}),
        put("/api/v1/sections/flagged", {"baseRevision": R, "values": {"model": "/models/f.gguf"}, "extras": "reranking = ON"}),
        put("/api/v1/sections/neg", {"baseRevision": R, "values": {}, "extras": "cram = -1"}),
        put("/api/v1/sections/big", {"baseRevision": R, "values": {}, "extras": "LLAMA_ARG_CACHE_RAM = 99999\n--cache-ram = 10"}),
        put("/api/v1/sections/text", {"baseRevision": R, "values": {}, "extras": "cache-ram = lots"}),
        put("/api/v1/sections/zero", {"baseRevision": R, "values": {}, "extras": "cache-ram = 007"}),
        put("/api/v1/sections/*", {"baseRevision": R, "values": {}, "extras": "cache-ram = 99999"}),
        put("/api/v1/sections/plain", {"baseRevision": R, "values": {"model": "/models/p.gguf"}}),
        put("/api/v1/sections/*", {"baseRevision": R, "values": {}}),
        put("/api/v1/sections/plain2", {"baseRevision": R, "values": {}}),
    ], ini="[*]\ncache-ram = 300\n", hard=4096, cap=512),
    case("save-defaults-section", [
        put("/api/v1/sections/a", {"baseRevision": R, "values": {"ctx-size": "1"}}),
        put("/api/v1/sections/c", {"baseRevision": R, "values": {}}),
    ], ini=DEFAULTS),
    case("duplicates-refused", [
        put("/api/v1/sections/a", {"baseRevision": R, "values": {}}),
        delete(f"/api/v1/sections/a?baseRevision={R}"),
        post("/api/v1/sections/a/rename", {"baseRevision": R, "newName": "z"}),
    ], ini="[a]\nx = 1\n[b]\ny = 2\n[a]\nz = 3\n[b]\n"),
    case("unparseable-file", [
        put("/api/v1/sections/a", {"baseRevision": R, "values": {}}),
    ], ini="[a]\ngarbage line\n"),
    case("odd-headers-full-render", [
        put("/api/v1/sections/a", {"baseRevision": R, "values": {"ctx-size": "1"}}),
    ], ini="[a] trailing\nmodel = m\n[b];c\nk : v\n"),
    case("crlf-and-no-final-newline", [
        put("/api/v1/sections/a", {"baseRevision": R, "values": {"model": "m", "ctx-size": "2"}}),
    ], ini="[a]\r\nmodel = m\r\n[b]\r\nk = v"),
    case("models-ini-replace", [
        put("/api/v1/models-ini", {"text": "[x]\r\nk = v\n", "baseRevision": R}),
        put("/api/v1/models-ini", {"text": "[y]\nk = v\n", "baseRevision": R, "backup": False}),
        put("/api/v1/models-ini", {"text": "[z]\n", "baseRevision": R, "backup": None}),
        put("/api/v1/models-ini", {"text": "   ", "baseRevision": R}),
        put("/api/v1/models-ini", {"text": 5, "baseRevision": R}),
        put("/api/v1/models-ini", {"text": "[a]\n[a]\n", "baseRevision": R}),
        put("/api/v1/models-ini", {"text": "no section here", "baseRevision": R}),
        put("/api/v1/models-ini", {"text": "[a]\nbad\n", "baseRevision": R}),
        put("/api/v1/models-ini", {"text": "[a]\n" + "k = v\n" * 180000, "baseRevision": R}),
        put("/api/v1/models-ini", {"text": "[a]\n", "baseRevision": "0" * 64}),
        put("/api/v1/models-ini", {"text": "[a]\n"}),
    ], ini=BASE),
    case("models-ini-rust-backups", [
        put("/api/v1/models-ini", {"text": "[x]\nk = v\n", "baseRevision": R}),
        put("/api/v1/models-ini", {"text": "[y]\nk = v\n", "baseRevision": R, "backup": False}),
        put("/api/v1/sections/x", {"baseRevision": R, "values": {}}),
    ], ini=BASE, impl="rust", files=[
        ["models.ini.bak-20200101-000000", "old", 1600000000],
        ["models.ini.bak-before-upgrade", "operator", 1600000000],
    ] + [[f"models.ini.noevia-backup-{i:064x}", "rev", 1600000000 + i] for i in range(11)]),
    case("rotating-prune-python", [
        put("/api/v1/sections/a", {"baseRevision": R, "values": {}}),
    ], ini=BASE, files=[[f"models.ini.bak-20200101-0000{i:02d}", "old", 1600000000] for i in range(12)]
       + [["models.ini.bak-20261016-120000", "taken", 1600000000], ["models.ini.bak-keep-me", "op", 1600000000]]),
    case("rename", [
        post("/api/v1/sections/a/rename", {"baseRevision": R, "newName": " c "}),
        post("/api/v1/sections/b/rename", {"baseRevision": R, "newName": "c"}),
        post("/api/v1/sections/zz/rename", {"baseRevision": R, "newName": "q"}),
        post("/api/v1/sections/b/rename", {"baseRevision": R, "newName": "bad name"}),
        post("/api/v1/sections/b/rename", {"baseRevision": "stale", "newName": "q"}),
        post("/api/v1/sections/b/rename", {"newName": "q"}),
        post("/api/v1/sections/b/rename", {"baseRevision": R, "newName": 7}),
    ], ini=BASE, roles='{"a": " Laya ", "b": 5, "x": "chat", "é": "rôle"}'),
    case("rename-fallback-and-header-comment", [
        post("/api/v1/sections/a/rename", {"baseRevision": R, "newName": "a2"}),
        post("/api/v1/sections/b/rename", {"baseRevision": R, "newName": "b2"}),
    ], ini="[DEFAULT]\nd = 1\n[a] ; comment\nk = v\n[b] junk\nk = v\n"),
    case("delete", [
        delete(f"/api/v1/sections/b?baseRevision={R}"),
        delete("/api/v1/sections/a?baseRevision="),
        delete("/api/v1/sections/a"),
        delete("/api/v1/sections/zz?baseRevision={REV}"),
        delete("/api/v1/sections/a?baseRevision=stale&baseRevision={REV}"),
        delete("/api/v1/sections/*?baseRevision={REV}"),
    ], ini=BASE, roles='{"a": "laya", "b": "x"}'),
    case("delete-middle", [
        delete(f"/api/v1/sections/a?baseRevision={R}"),
    ], ini="[x]\nk = 1\n\n[a]\nm = 2\n\n\n[y]\nk = 3\n"),
    case("safe-defaults", [
        post("/api/v1/sections/llama-small/safe-defaults"),
        post("/api/v1/sections/llama-small/safe-defaults"),
        post("/api/v1/sections/folder-model/safe-defaults"),
        post("/api/v1/sections/folder-model-Q4_K_M/safe-defaults"),
        post("/api/v1/sections/missing/safe-defaults"),
        post("/api/v1/sections/x-mmproj-f16/safe-defaults"),
        post("/api/v1/sections/bad%20name/safe-defaults"),
        post("/api/v1/sections/tiny-draft/safe-defaults"),
        post("/api/v1/sections/llama-small/safe-defaults", {"ignored": True}),
    ], ini="[*]\ncache-ram = 100\n", files=[
        ["llama-small.gguf", {"gguf": gguf(ctx=4096)}, 1600000000],
        ["folder-model/folder-model-Q4_K_M.gguf", {"gguf": gguf(ctx=65536)}, 1600000000],
        ["folder-model/folder-model-draft-Q8_0.gguf", {"gguf": gguf(ctx=4096)}, 1600000000],
        ["tiny-draft.gguf", {"gguf": gguf()}, 1600000000],
    ]),
]


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR", "MODELS_DIR", "MODELS_INI_PATH"}:
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    import time as real_time

    from fastapi.testclient import TestClient  # noqa: E402

    from app import config, db, ini  # noqa: E402
    from app.main import app  # noqa: E402

    ini.time = types.SimpleNamespace(strftime=lambda fmt, *a: STAMP if fmt == "%Y%m%d-%H%M%S" else real_time.strftime(fmt, *a),
                                     monotonic_ns=real_time.monotonic_ns)
    client = TestClient(app, raise_server_exceptions=False)
    digest = {n: hashlib.sha256((app_dir / n).read_bytes()).hexdigest() for n in ("api.py", "ini.py", "main.py", "helpers.py")}
    out = []
    for c in CASES:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(os.path.realpath(tmp))
            models = root / "models"
            models.mkdir()
            path = models / "models.ini"
            if c["ini"] is not None:
                path.write_bytes(c["ini"].encode())
            if c["roles"] is not None:
                (models / "models.roles.json").write_text(c["roles"], encoding="utf-8")
            for name, content, mtime in c["files"]:
                f = models / name
                f.parent.mkdir(parents=True, exist_ok=True)
                f.write_bytes(bytes.fromhex(content["gguf"]) if isinstance(content, dict) else content.encode())
                os.utime(f, ns=(mtime * 10**9, mtime * 10**9))
            s = config.settings
            s.models_dir, s.models_ini_path, s.data_dir = models, path, root / "data"
            s.llamacpp_cache_ram_hard_max_mib, s.llamacpp_autoconfig_cache_ram_max_mib = c["hard"], c["cap"]
            s.model_files_impl = c["impl"]
            db._DB_PATH = None
            answers = []
            for step in c["steps"]:
                rev = hashlib.sha256(path.read_bytes() if path.exists() else b"").hexdigest()
                url = step["path"].replace("{REV}", rev)
                body = None if step["body"] is None else step["body"].replace("{REV}", rev).encode()
                headers = {"content-type": step["content_type"]} if step["content_type"] and body is not None else {}
                plan = None
                if url.endswith("/safe-defaults"):
                    name = url.split("/")[4]
                    os.environ["NOEVIA_NATIVE_MODEL_MANAGER_SECTION_WRITES_CONFIRMED"] = "1"
                    from urllib.parse import unquote
                    p = client.get("/api/v1/internal/safe-defaults-plan", params={"name": unquote(name)})
                    del os.environ["NOEVIA_NATIVE_MODEL_MANAGER_SECTION_WRITES_CONFIRMED"]
                    plan = {"status": p.status_code, "body": p.content.decode()}
                r = client.request(step["method"], url, content=body, headers=headers)
                answers.append({"status": r.status_code, "content_type": r.headers.get("content-type"),
                                "body": r.content.decode(), "plan": plan})
            listing = sorted(str(p.relative_to(models)) for p in models.rglob("*") if p.is_file() and ".tmp-" not in p.name)
            roles = models / "models.roles.json"
            out.append({**c, "answers": answers, "final": {
                "ini": path.read_bytes().decode() if path.exists() else None,
                "roles": roles.read_text(encoding="utf-8") if roles.exists() else None,
                "listing": listing}})
    json.dump({"oracle": digest, "stamp": STAMP, "epoch": EPOCH, "cases": out}, sys.stdout, ensure_ascii=False, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
