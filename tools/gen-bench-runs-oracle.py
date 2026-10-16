#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/bench-runs-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, `api.benchmark_run`,
`helpers._bench_charts` and `db.bench_*` / `db.badges_for`) against synthetic SQLite stores and
records the exact status, content type and body of `GET /api/v1/benchmark/runs/{run_id}`:

    python3 -I tools/gen-bench-runs-oracle.py <services>/model-manager/app > fixture.json

The two live inputs of the charts are replaced by fixed synthetic values: `helpers._backend_list`
(the GPU sampler; it gives `capacity_gb`) and `helpers._predicted_vram_gb` (autoconfig on model
files). Each case also records `runtime`: what the child's private bench-run-runtime call answers
for the same values (rendered by FastAPI's JSONResponse), which the Rust test's fake child serves.
No model file, container or GPU is touched.
"""
from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import sys
import tempfile
from pathlib import Path

RUN = "INSERT INTO bench_run(id, backend, status, started_at, finished_at, reps, max_tokens, note) VALUES"
VAR = "INSERT INTO bench_variant(id, run_id, alias, argv_json, load_ms) VALUES"
RES = ("INSERT INTO bench_result(variant_id, prompt_name, rep, cold, ttft_ms, ttft_answer_ms, truncated, total_ms,"
       " prompt_n, prompt_tps, gen_n, gen_tps, draft_n, draft_acc, peak_vram_json, contended, err, response_text) VALUES")
SWP = ("INSERT INTO bench_sweep(run_id, alias, model_type, test, n_prompt, n_gen, n_depth, avg_ts, stddev_ts,"
       " n_gpu_layers, n_cpu_moe, n_ubatch, type_k, type_v, flash_attn, split_mode, raw_json) VALUES")
BADGE = "INSERT INTO model_badge(alias, category, rating, note, run_id, created_at) VALUES"


def res(v, prompt, rep, ttft, gen, peak="'[]'", cold=0, contended=0, err="''", text="'ok'"):
    return (f"{RES}({v}, '{prompt}', {rep}, {cold}, {ttft}, NULL, 0, 1000.5, 10, 50.25, 20, {gen}, NULL, NULL,"
            f" {peak}, {contended}, {err}, {text})")


def swp(run, alias, n_gen, depth, avg):
    return (f"{SWP}({run}, '{alias}', 'synthetic', 'tg', 512, {n_gen}, {depth}, {avg}, 0.5, 99, 0, 512,"
            f" 'f16', 'f16', 1, 'layer', '{{}}')")


FULL = [
    f"{RUN}(1, 'llama', 'done', 1700000000.5, 1700000100.25, 3, 256, 'synthetic run')",
    f"{VAR}(1, 1, 'beta-model', '{{\"ctx\": 4096}}', 1234.5)",
    f"{VAR}(2, 1, 'Alpha-model', '{{}}', NULL)",
    f"{VAR}(3, 1, 'é-model', '{{}}', 10.0)",
    res(1, 'coding', 0, 120.0, 40.25, "'[1.5, 2.25]'", cold=1),
    res(1, 'coding', 1, 100.0, 42.125, "'[1.5, 2.5]'"),
    res(1, 'coding', 2, 102.5, 41.0, "'[1, 2]'"),
    res(1, 'writing', 0, 99.5, 0.25, "'[3, 4]'"),
    res(1, 'writing', 1, 98.5, 0.35, "'not json'"),
    res(2, 'coding', 0, 2.5, 30.0, "'[]'", contended=1),
    res(2, 'coding', 1, 3.5, None if False else 'NULL', "''"),
    res(2, 'writing', 0, 0.125, 0.0, "'[2, 2.0]'", err="'boom'"),
    res(2, 'writing', 1, 1.5, 0.05, "'[true, 1]'"),
    res(3, 'reasoning', 0, 1e20, 123.456, "'[\"0\"]'".replace("[\"0\"]", "[0.1, 0.2]"), text="'réponse \U0001F600'"),
    swp(1, 'beta-model', 128, 0, 30.55),
    swp(1, 'beta-model', 0, 0, 900.25),
    swp(1, 'beta-model', 128, 4096, 25.05),
    swp(1, 'beta-model', 128, 4096, 99.0),
    swp(1, 'beta-model', 0, 'NULL', 'NULL'),
    swp(1, 'Alpha-model', 128, 16384, 0.0),
    swp(1, 'Alpha-model', 0, 16384, 'NULL'),
    swp(2, 'other-run', 128, 0, 1.0),
    f"{BADGE}('beta-model', 'coding', 4, 'n', 1, 1700000000.25)",
    f"{BADGE}('beta-model', 'writing', 2, '', NULL, 1.0)",
    f"{BADGE}('other', 'coding', 5, '', NULL, 1.0)",
]
EMPTY = [f"{RUN}(5, 'llama', 'running', 1.0, NULL, 1, 256, '')"]
ALL_COLD = [
    f"{RUN}(6, 'b', 'error', 1.0, 2.0, 1, 1, '')",
    f"{VAR}(9, 6, 'm', '{{}}', NULL)",
    res(9, 'p', 0, 1.0, 2.0, cold=1),
]
TIES = [
    f"{RUN}(7, 'b', 'done', 1.0, 2.0, 1, 1, '')",
    f"{VAR}(10, 7, 'm', '{{}}', NULL)",
    res(10, 'p', 0, 2.5, 0.25, "'[0.125]'"),
    res(10, 'q', 0, 3.5, 0.35, "'[0.375]'"),
    res(10, 'q', 1, 4.5, 0.45, "'[2.675]'"),
]
INFINITE = [
    f"{RUN}(8, 'b', 'done', 1.0, 1e999, 1, 1, '')",
    f"{VAR}(11, 8, 'm', '{{}}', -1e999)",
    res(11, 'p', 0, '1e999', '1e999', "'[1e999]'"),
    res(11, 'p', 1, 2.0, 3.0, "'[1.0]'"),
    swp(8, 'm', 128, 0, '1e999'),
]

# id, seed, path segment, backends ("ok" | "empty" | "raises"), predictions
CASES = [
    ("full-run", FULL, "1", "ok", {"beta-model": 12.35, "Alpha-model": None, "é-model": 0.5}),
    ("full-run-no-gpu", FULL, "1", "empty", {"beta-model": None, "Alpha-model": None, "é-model": None}),
    ("full-run-sampler-raises", FULL, "1", "raises", {"beta-model": 1.0, "Alpha-model": 2.0, "é-model": 3.0}),
    ("leading-zeros", FULL, "0001", "ok", {"beta-model": 12.35, "Alpha-model": None, "é-model": 0.5}),
    ("not-found", FULL, "2", "ok", {}),
    ("negative-not-found", FULL, "-1", "ok", {}),
    ("empty-run", EMPTY, "5", "ok", {}),
    ("all-cold", ALL_COLD, "6", "ok", {}),
    ("rounding-ties", TIES, "7", "ok", {"m": 7.125}),
    ("infinite-values", INFINITE, "8", "ok", {"m": None}),
]


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR"}:
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.responses import JSONResponse  # noqa: E402
    from fastapi.testclient import TestClient  # noqa: E402

    from app import config, db, helpers  # noqa: E402
    from app.main import app  # noqa: E402

    client = TestClient(app, raise_server_exceptions=False)
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest()
              for name in ("api.py", "db.py", "helpers.py", "main.py")}
    out = []
    schema = None
    for case_id, seed, segment, backends, predictions in CASES:
        def backend_list(kind=backends):
            if kind == "raises":
                raise RuntimeError("synthetic sampler failure")
            return [{"vram_gb": 23.987}, {"vram_gb": 11}] if kind == "ok" else []
        helpers._backend_list = backend_list
        helpers._predicted_vram_gb = lambda a, p=predictions: p[a]
        with tempfile.TemporaryDirectory() as root:
            config.settings.data_dir = Path(root)
            db._DB_PATH = None
            db.init()
            c = sqlite3.connect(Path(root) / "model_loader.db", isolation_level=None)
            if schema is None:
                # db.init()'s tables, so the Rust test builds the identical store.
                schema = [r[0] for r in c.execute(
                    "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")]
            for statement in seed:
                c.execute(statement)
            c.close()
            response = client.get(f"/api/v1/benchmark/runs/{segment}")
            body = response.content.decode("utf-8")
            runtime = None
            if response.status_code == 200:
                labels = json.loads(body)["charts"]["vram_labels"]
                try:
                    capacity = max([0.0] + [float(b.get("vram_gb") or 0) for b in backend_list()])
                except Exception:  # noqa: BLE001
                    capacity = 0.0
                runtime = JSONResponse({"capacity_gb": round(capacity, 2),
                                        "predicted": {a: predictions[a] for a in labels}}).body.decode("utf-8")
            out.append({"id": case_id, "seed": seed, "segment": segment, "status": response.status_code,
                        "content_type": response.headers.get("content-type"), "body": body, "runtime": runtime})
    json.dump({"oracle": digest, "schema": schema, "cases": out}, sys.stdout, ensure_ascii=False, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
