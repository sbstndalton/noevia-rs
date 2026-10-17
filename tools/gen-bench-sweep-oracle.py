#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/bench-sweep-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services sweep runner (`bench.start_sweep`, `_run_sweep`,
`run_sweep_once`, `_sweep_runtime`, `_free_gpu`, `reap_orphans`, `_watch_orphans`,
`db.bench_add_sweep`) behind the real FastAPI routes (`POST /api/v1/benchmark/sweep|start|cancel`,
`GET /api/v1/benchmark/progress`) against:

  * a fake llama-server on 127.0.0.1 answering `/v1/models` (and recording each request),
  * a fake Docker client scripted per case: backend containers (image tag, mounts, GPU request,
    port), the sweep containers `containers.run` hands out (create/start failures, status
    sequence, stdout/stderr) and the label-filtered list startup recovery sees,
  * `services.restart_llama_backend` recorded instead of restarting anything,
  * `time.time` in `bench` replaced by a fixed-step counter and `time.sleep` by a no-op,
  * worker threads held back until a scripted `run` step.

    python3 -I tools/gen-bench-sweep-oracle.py <services>/model-manager/app > fixture.json

No model, container, GPU or network beyond 127.0.0.1 is touched.
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

TIME0, TIME_STEP = 1700000000.0, 1.5
B = "127.0.0.1"
MODELS_LOADED = {"status": 200, "body": json.dumps({"data": [{"id": "m1", "status": {"value": "loaded"}}]})}
MODELS_NONE = {"status": 200, "body": json.dumps({"data": []})}

INI = """[m1]
model = /models/m1.gguf
ngl = 99
n-gpu-layers = 5
cache-type-k = q8_0
tensor-split = 1,2
flash-attn = on

[m2]
model = sub/m2.gguf

[nomodel]
ngl = 1
"""

IMAGE_ID = "sha256:" + "ab" * 32
BACKENDS = {
    B: {"tags": ["ghcr.io/example/llama:full"], "mounts": [["/mnt/user/models", "/models"],
                                                         ["/mnt/user/cache", "/root/.cache"]],
        "gpu": True, "workdir": "/models"},
    "notag": {"tags": [], "mounts": [], "gpu": False, "workdir": ""},
}


def cid(n: int) -> str:
    return f"{n:064x}"


ENTRY = {"build_commit": "abc", "model_type": "qwen3 8B Q4_K - Médium", "n_prompt": 256, "n_gen": 0,
         "n_depth": 512, "avg_ts": 1234.5, "stddev_ts": 0.1, "n_gpu_layers": 99, "n_cpu_moe": 0,
         "n_ubatch": 512, "type_k": "q8_0", "type_v": "f16", "flash_attn": True, "split_mode": "layer",
         "samples_ts": [1, 2.5], "devices": None}
ENTRY_TG = {"n_prompt": 0, "n_gen": 64, "n_depth": 0, "avg_ts": 50.25, "stddev_ts": None, "flash_attn": 1}


def ok(n, stdout, statuses=("running", "running", "exited"), stderr=""):
    return {"id": cid(n), "statuses": list(statuses), "stdout": stdout, "stderr": stderr}


def sweep(**body) -> list:
    return ["sweep", body]


GOOD = "ggml_cuda_init: found 1 CUDA devices\n" + json.dumps([ENTRY, ENTRY_TG], indent=2) + "\n"
FAILED = {"id": cid(2), "statuses": ["exited"], "stdout": "[\n",
          "stderr": "load_backend: loaded CUDA\nmain: error: failed to load model '/models/sub/m2.gguf'\nllama_bench: done\n"}

PROMPT = "INSERT INTO prompts(id, name, body, created_at) VALUES(1, 'alpha', 'Say hi.', 1.0)"

# id, models answer, sweep containers handed out by run(), label-filtered list, steps
CASES = [
    ("sweep", MODELS_LOADED, [ok(1, GOOD), FAILED], [],
     [sweep(backend=B, aliases=["m1", "", "m2"], nPrompt=256, nGen=64, depths=" 0,512 ", reps=2),
      ["progress"], ["run"], ["progress"]]),
    ("refusals", MODELS_NONE, [ok(3, "[]", statuses=("exited",))], [],
     [sweep(backend="", aliases=["m1"]), sweep(backend=" ", aliases=["m1"]), sweep(backend=B, aliases=[""]),
      sweep(backend=B), sweep(backend=B, aliases=["nomodel"]), sweep(backend=B, aliases=["m1", "absent"]),
      ["progress"],
      sweep(backend=B, aliases=["m1"]), sweep(backend=B, aliases=["m1"]),
      ["start", {"backend": B, "aliases": ["m1"], "promptIds": [1]}],
      ["run"], ["progress"]]),
    ("conversions", MODELS_NONE, [ok(4, "", statuses=("dead",), stderr="")], [],
     [sweep(backend=B, aliases=["m1"], nPrompt="abc"),
      sweep(backend=B, aliases=["m1"], reps=2 ** 70),
      sweep(backend=B, aliases=["m1"], nGen=2 ** 70),
      sweep(backend=B, aliases=["m1"], depths=["a", 1]),
      sweep(backend=B, aliases="m1", nPrompt=True, nGen=" 7 ", reps=-3.9, depths=0),
      ["progress"], ["run"], ["progress"]]),
    ("docker-errors", MODELS_NONE, [], [],
     [sweep(backend="notag", aliases=["m1", "m2"]), ["run"], ["progress"],
      sweep(backend="missing", aliases=["m1"]), ["run"], ["progress"]]),
    ("create-and-start-fail", MODELS_NONE,
     [{"id": cid(5), "create_error": "create boom"}, {"id": cid(6), "start_error": "start boom"}], [],
     [sweep(backend=B, aliases=["m1", "m2"]), ["run"], ["progress"]]),
    ("timeout", MODELS_NONE, [{"id": cid(7), "statuses": ["created"], "repeat": "running", "stdout": "", "stderr": ""}],
     [], [sweep(backend=B, aliases=["m1"]), ["run"], ["progress"]]),
    ("cancel", MODELS_LOADED, [], [],
     [sweep(backend=B, aliases=["m1", "m2"]), ["cancel"], ["progress"], ["run"], ["progress"], ["cancel"]]),
    ("bad-entries", MODELS_NONE,
     [ok(8, "[1]"), ok(9, '[{"n_prompt": "x"}]'), ok(10, '[{"n_prompt": 1, "avg_ts": [1]}]'),
      ok(11, '[{"n_prompt": 1180591620717411303424}]'), ok(12, '[{"n_gen": 2.9, "n_depth": "  1_0 "}] trailing'),
      ok(13, '[{"n_gen": 2.9, "n_depth": "  1_0 ", "model_type": 0, "type_k": [], "split_mode": 3}]'),
      ok(14, "x\xff[\"\xe9\", {\"a\": NaN, \"b\": -Infinity, \"c\": 1e400}]")],
     [],
     [sweep(backend=B, aliases=["m1"]), ["run"], ["progress"],
      sweep(backend=B, aliases=["m1"]), ["run"], ["progress"],
      sweep(backend=B, aliases=["m1"]), ["run"], ["progress"],
      sweep(backend=B, aliases=["m1"]), ["run"], ["progress"],
      sweep(backend=B, aliases=["m1"]), ["run"], ["progress"],
      sweep(backend=B, aliases=["m1"]), ["run"], ["progress"],
      sweep(backend=B, aliases=["m1"]), ["run"], ["progress"]]),
    ("reap", MODELS_NONE, [],
     [{"id": cid(20), "removes": ["ok"]}, {"id": cid(21), "removes": ["notfound"]},
      {"id": cid(22), "removes": ["error:conflict", "error:conflict", "ok"]}],
     [["reap"], ["progress"], ["sweep", {"backend": B, "aliases": ["m1"]}], ["run"], ["progress"]]),
    ("reap-none", MODELS_NONE, [], [], [["reap"], ["progress"]]),
]


class Llama(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    models: dict = {}
    seen: list = []

    def do_GET(self):  # noqa: N802
        Llama.seen.append({"method": "GET", "path": self.path})
        body = Llama.models["body"].encode("utf-8")
        self.send_response(Llama.models["status"])
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def log_message(self, *args):
        pass


class NotFound(Exception):
    pass


class APIError(Exception):
    pass


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR",
                                                                      "MODELS_INI_PATH"} \
                or "PROXY" in key.upper():
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import bench, config, db, services  # noqa: E402
    from app.main import app  # noqa: E402

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Llama)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_address[1]

    docker_log: list = []
    restarts: list = []
    script: dict = {"run": [], "listed": [], "leftover": []}

    class Sweep:
        def __init__(self, spec):
            self.id = spec["id"]
            self.statuses = list(spec.get("statuses") or [])
            self.repeat = spec.get("repeat")
            self.spec = spec
            self.status = "created"
            self.removes = list(spec.get("removes") or [])

        def reload(self):
            if self.statuses:
                self.status = self.statuses.pop(0)
            elif self.repeat:
                self.status = self.repeat
            else:
                self.status = "exited"

        def kill(self):
            docker_log.append(["kill", self.id])

        def logs(self, stdout=True, stderr=True):
            return (self.spec.get("stdout") if stdout else self.spec.get("stderr") or "").encode("utf-8",
                                                                                                "surrogateescape")

        def remove(self, force=False):
            docker_log.append(["remove", self.id])
            what = self.removes.pop(0) if self.removes else "ok"
            if what == "notfound":
                raise NotFound(f"No such container: {self.id}")
            if what.startswith("error:"):
                raise APIError(what[6:])

    def get(name):
        if name in BACKENDS:
            spec = BACKENDS[name]
            attrs = {"NetworkSettings": {"Ports": {f"{port}/tcp": None}},
                     "Mounts": [{"Source": s, "Destination": d} for s, d in spec["mounts"]],
                     "HostConfig": {"DeviceRequests": [{"Driver": "nvidia", "Count": -1, "DeviceIDs": None,
                                                        "Capabilities": [["gpu"]], "Options": {}}]
                                    if spec["gpu"] else None},
                     "Config": {"WorkingDir": spec["workdir"]}}
            return types.SimpleNamespace(attrs=attrs, image=types.SimpleNamespace(tags=spec["tags"]))
        raise NotFound(f"No such container: {name}")

    def run(**kwargs):
        entry = {"image": kwargs["image"], "cmd": kwargs["command"], "entrypoint": kwargs["entrypoint"],
                 "binds": sorted(f"{s}:{v['bind']}:{v['mode']}" for s, v in kwargs["volumes"].items()),
                 "network": kwargs["network_mode"], "labels": kwargs["labels"],
                 "gpu": bool(kwargs.get("device_requests")), "remove": kwargs["remove"],
                 "detach": kwargs["detach"]}
        docker_log.append(["create", entry])
        spec = script["run"].pop(0)
        if spec.get("create_error"):
            raise APIError(spec["create_error"])
        if spec.get("start_error"):
            script["leftover"].append(Sweep(spec))
            raise APIError(spec["start_error"])
        return Sweep(spec)

    def listing(all=False, filters=None):
        assert all and filters.get("label") == f"{bench.SWEEP_LABEL}=1", filters
        if filters.get("status") == "created":
            out, script["leftover"] = script["leftover"], []
            return out
        return [Sweep(s) for s in script["listed"]]

    services._docker_client = lambda: types.SimpleNamespace(
        containers=types.SimpleNamespace(get=get, run=run, list=listing))
    services.restart_llama_backend = lambda name: restarts.append(name) or (True, "restart queued")

    client = TestClient(app, raise_server_exceptions=False)
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest()
              for name in ("api.py", "bench.py", "db.py", "ini.py", "main.py")}
    out = []
    schema = None
    for case_id, models, runs, listed, steps in CASES:
        Llama.models, Llama.seen = models, []
        docker_log.clear()
        restarts.clear()
        script["run"], script["listed"], script["leftover"] = [dict(r) for r in runs], listed, []
        wall = iter(TIME0 + TIME_STEP * k for k in range(100000))
        bench.time = types.SimpleNamespace(time=lambda: next(wall), perf_counter=lambda: 0.0, sleep=lambda s: None)
        held: list = []

        class Held:
            def __init__(self, target, name=None, args=(), daemon=None):
                self.target, self.args = target, args

            def start(self):
                held.append(self)
        bench.threading = types.SimpleNamespace(Thread=Held, Lock=threading.Lock, Event=threading.Event)
        bench._STATE = bench.JobState()
        bench._CANCEL.clear()
        answers = []
        with tempfile.TemporaryDirectory() as root:
            config.settings.data_dir = Path(root)
            ini_path = Path(root) / "models.ini"
            ini_path.write_text(INI, encoding="utf-8")
            config.settings.models_ini_path = ini_path
            db._DB_PATH = None
            db.init()
            c = sqlite3.connect(Path(root) / "model_loader.db", isolation_level=None)
            if schema is None:
                schema = [r[0] for r in c.execute(
                    "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")]
            c.execute("DELETE FROM prompts")
            c.execute(PROMPT)
            c.close()
            for step in steps:
                if step[0] == "sweep":
                    r = client.post("/api/v1/benchmark/sweep", json=step[1])
                elif step[0] == "start":
                    r = client.post("/api/v1/benchmark/start", json=step[1])
                elif step[0] == "progress":
                    r = client.get("/api/v1/benchmark/progress")
                elif step[0] == "cancel":
                    r = client.post("/api/v1/benchmark/cancel")
                elif step[0] == "reap":
                    answers.append(bench.reap_orphans())
                    continue
                else:
                    while held:
                        t = held.pop(0)
                        t.target(*t.args)
                    answers.append(None)
                    continue
                answers.append({"status": r.status_code, "content_type": r.headers.get("content-type"),
                                "body": r.content.decode("utf-8")})
            c = sqlite3.connect(Path(root) / "model_loader.db", isolation_level=None)
            tables = {t: [list(r) for r in c.execute(f"SELECT {cols} FROM {t} ORDER BY id")] for t, cols in (
                ("bench_run", "quote(id), quote(backend), quote(status), quote(started_at), quote(finished_at),"
                              " quote(reps), quote(max_tokens), quote(note)"),
                ("bench_sweep", "quote(run_id), quote(alias), quote(model_type), quote(test), quote(n_prompt),"
                                " quote(n_gen), quote(n_depth), quote(avg_ts), quote(stddev_ts),"
                                " quote(n_gpu_layers), quote(n_cpu_moe), quote(n_ubatch), quote(type_k),"
                                " quote(type_v), quote(flash_attn), quote(split_mode), quote(raw_json)"))}
            c.close()
        out.append({"id": case_id, "models": models, "runs": runs, "listed": listed, "steps": steps,
                    "answers": answers, "llama": Llama.seen, "docker": list(docker_log),
                    "restarts": list(restarts), "tables": tables})
    server.shutdown()
    json.dump({"oracle": digest, "schema": schema, "ini": INI, "backends": BACKENDS, "image_id": IMAGE_ID,
               "time": [TIME0, TIME_STEP], "cases": out},
              sys.stdout, ensure_ascii=False, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
