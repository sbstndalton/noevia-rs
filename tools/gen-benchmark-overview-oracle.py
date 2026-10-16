#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/benchmark-overview-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app,
`api.benchmark_overview`, `ini.section_names`, `bench.sweep_args_for_section`, `db.list_prompts`,
`db.bench_runs` and `services._effective_container_names`) against a synthetic models.ini, a
synthetic SQLite store and a fake Docker client, and records the exact status, content type and
body of `GET /api/v1/benchmark`:

    python3 -I tools/gen-benchmark-overview-oracle.py <services>/model-manager/app > fixture.json

The runner's live state is a fixed `bench.JobState` per case. Each case also records `job`: what
the child's private bench-job call answers for that state (`api._bench_job()` rendered by FastAPI's
JSONResponse), which the Rust test's fake child serves. No model, container or GPU is touched.
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

PROMPT = "INSERT INTO prompts(id, name, body, created_at) VALUES"
RUN = "INSERT INTO bench_run(id, backend, status, started_at, finished_at, reps, max_tokens, note) VALUES"
VAR = "INSERT INTO bench_variant(id, run_id, alias, argv_json, load_ms) VALUES"
RES = ("INSERT INTO bench_result(variant_id, prompt_name, rep, cold, total_ms, peak_vram_json, err, response_text)"
       " VALUES")

STORE = [
    f"{PROMPT}(1, 'beta', 'Write a sort.', 1700000000.5)",
    f"{PROMPT}(2, 'Alpha', 'Prove it.', 1.0)",
    f"{PROMPT}(3, 'éclair', 'Unicode \U0001F600 body\nwith a newline and \"quotes\"', 2.25)",
    f"{PROMPT}(4, 'alpha2', '', 0)",
    f"{RUN}(1, 'llama', 'done', 1700000000.5, 1700000100.25, 3, 256, 'synthetic run')",
    f"{RUN}(2, 'llama', 'running', 1.0, NULL, 1, 3072, '')",
    f"{RUN}(3, 'é-backend', 'error', 1e20, 0.1, 10, 8192, 'could not start: boom')",
    f"{VAR}(1, 1, 'm1', '{{}}', 12.5)",
    f"{VAR}(2, 1, 'm2', '{{}}', NULL)",
    f"{VAR}(3, 3, 'm3', '{{}}', NULL)",
    f"{RES}(1, 'p', 0, 1, 10.5, '[]', '', 'ok')",
    f"{RES}(1, 'p', 1, 0, 11.5, '[]', '', 'ok')",
    f"{RES}(2, 'p', 0, 1, 12.5, '[]', 'boom', '')",
]
MANY = [f"{RUN}({i}, 'b{i}', 'done', {i}.5, {i + 1}.0, 1, 16, 'n{i}')" for i in range(1, 31)]

INI_FULL = """version = 1

[DEFAULT]
flash-attn = on
ngl = 99

[chat-model]
model = /models/chat.gguf
cache-type-k = q8_0
cache-type-v =  q8_0\x20\x20
n-gpu-layers = 50
tensor-split = 3,1
split-mode = layer
batch-size = 2048

[éclair-7b]
flash-attn =
main-gpu = 1
ubatch-size = 512
n-cpu-moe = 4
extra = ignored

[bare]
"""
INI_DUPLICATES = """[a]
ngl = 1
[b]
ngl = 2
[a]
ngl = 3
tensor-split = 1,1,2
"""
INI_CONTINUATION = """[cont]
cache-type-k = q4_0
  continued
"""

CONTAINERS = [("llama", "ghcr.io/ggml-org/llama.cpp:server-cuda"),
              ("model-loader", "ghcr.io/ggml-org/llama.cpp:server"),
              ("Other-LLAMA", "local/LLaMA.CPP:rocm"),
              ("db", "postgres:16")]

IDLE = {}
FINISHED = {"run_id": 7, "status": "done", "backend": "llama", "total": 12, "done": 12,
            "current": "", "started_at": 1700000000.25, "finished_at": 1700000090.75, "error": "",
            "unit": "requests", "lines": ["m1: starting", "m1: done — ok \U0001F600"]}
CANCELLED = {"run_id": 9, "status": "cancelled", "backend": "é", "total": 3, "done": 1,
             "started_at": 10.0, "finished_at": 13.5, "unit": "models", "lines": []}

# id, ini text (None: no file; bytes: raw), store seed, configured names, containers (None: docker
# unreachable), job state
CASES = [
    ("full", INI_FULL, STORE, ["llama", "manual"], CONTAINERS, FINISHED),
    ("idle-no-ini", None, STORE, [], CONTAINERS, IDLE),
    ("empty-store", "", [], [], [], IDLE),
    ("run-limit-25", INI_FULL, MANY, [], [], CANCELLED),
    ("docker-unreachable", INI_DUPLICATES, STORE, ["llama"], None, IDLE),
    ("duplicate-sections", INI_DUPLICATES, [], [], CONTAINERS, IDLE),
    ("continuation-value", INI_CONTINUATION, [], [], [], IDLE),
    ("ini-parse-error", "no section header here\n", STORE, [], [], IDLE),
    ("ini-not-utf8", b"[a]\nngl = \xff\n", STORE, [], [], IDLE),
    ("inf-in-run", INI_FULL, [f"{RUN}(1, 'b', 'done', 1.0, 2.0, 1, 16, '')",
                              "UPDATE bench_run SET finished_at = 1e999 WHERE id = 1"], [], [], IDLE),
]


class _Image:
    def __init__(self, tag: str) -> None:
        self.tags = [tag]
        self.short_id = "sha256:0"


class _Container:
    def __init__(self, name: str, tag: str) -> None:
        self.name = name
        self.image = _Image(tag)


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR",
                                                                         "MODELS_INI_PATH", "LLAMA_CONTAINERS"}:
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.responses import JSONResponse  # noqa: E402
    from fastapi.testclient import TestClient  # noqa: E402

    from app import api, bench, config, db, services  # noqa: E402
    from app.main import app  # noqa: E402

    client = TestClient(app, raise_server_exceptions=False)
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest()
              for name in ("api.py", "bench.py", "db.py", "ini.py", "services.py", "main.py")}
    out = []
    schema = None
    for case_id, ini_text, seed, configured, containers, job in CASES:
        if containers is None:
            services._docker_client = lambda: None
        else:
            listed = [_Container(n, t) for n, t in containers]
            fake = types.SimpleNamespace(containers=types.SimpleNamespace(list=lambda all=False, c=listed: c))
            services._docker_client = lambda f=fake: f
        config.settings.llama_containers = ",".join(configured)
        bench._STATE = bench.JobState(**job)
        with tempfile.TemporaryDirectory() as root:
            config.settings.data_dir = Path(root)
            ini_path = Path(root) / "models.ini"
            config.settings.models_ini_path = ini_path
            if isinstance(ini_text, bytes):
                ini_path.write_bytes(ini_text)
            elif ini_text is not None:
                ini_path.write_text(ini_text, encoding="utf-8")
            db._DB_PATH = None
            db.init()
            c = sqlite3.connect(Path(root) / "model_loader.db", isolation_level=None)
            if schema is None:
                # db.init()'s tables, so the Rust test builds the identical store.
                schema = [r[0] for r in c.execute(
                    "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")]
            c.execute("DELETE FROM prompts")
            for statement in seed:
                c.execute(statement)
            c.close()
            response = client.get("/api/v1/benchmark")
            job_body = JSONResponse(api._bench_job()).body.decode("utf-8")
            out.append({"id": case_id,
                        "ini": ini_text.decode("latin-1") if isinstance(ini_text, bytes) else ini_text,
                        "ini_bytes": isinstance(ini_text, bytes), "seed": seed, "configured": configured,
                        "containers": None if containers is None else [list(x) for x in containers],
                        "job": job_body, "status": response.status_code,
                        "content_type": response.headers.get("content-type"),
                        "body": response.content.decode("utf-8")})
    json.dump({"oracle": digest, "schema": schema, "cases": out}, sys.stdout, ensure_ascii=False, indent=1,
              sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
