#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/bench-runner-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services benchmark runner (`bench.start`, `bench._run`, `bench.measure`,
`bench.loaded_aliases`, `bench.cancel`, `db.bench_*`) behind the real FastAPI routes
(`POST /api/v1/benchmark/start|cancel`, `GET /api/v1/benchmark/progress`) against:

  * a fake llama-server on 127.0.0.1 that answers `/v1/models` and scripted streamed chat
    completions, and records each request body,
  * a fake Docker client whose only container is that server,
  * the three observations the native runner asks the child for (`_peak_vram`, `_other_traffic`,
    `_read_load_ms`) replaced by recorded fixed answers,
  * `time.perf_counter` / `time.time` in `bench` replaced by fixed-step counters,
  * the worker thread held back until a scripted `run` step, so every answer is deterministic.

    python3 -I tools/gen-bench-runner-oracle.py <services>/model-manager/app > fixture.json

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

PERF0, PERF_STEP = 100.0, 0.125
TIME0, TIME_STEP = 1700000000.0, 1.5
PEAK = [1.5, 2.25]
LOAD_MS = 1234.5


def sse(*events: object, done: bool = True, sep: str = "\n\n") -> str:
    out = "".join("data: " + (e if isinstance(e, str) else json.dumps(e)) + sep for e in events)
    return out + ("data: [DONE]" + sep if done else "")


def delta(content=None, reasoning=None, finish=None) -> dict:
    d = {}
    if content is not None:
        d["content"] = content
    if reasoning is not None:
        d["reasoning_content"] = reasoning
    ch = {"index": 0, "delta": d}
    if finish is not None:
        ch["finish_reason"] = finish
    return {"choices": [ch]}


TIMINGS = {"timings": {"prompt_n": 12, "prompt_per_second": 250.5, "predicted_n": 40,
                       "predicted_per_second": 33.25, "draft_n": 8, "draft_n_accepted": 5}, "choices": []}
OK = {"status": 200, "body": sse({"choices": [{"delta": {"role": "assistant"}}]}, delta(reasoning="thinking"),
                                 delta(content="Hello"), delta(content=" world", finish="stop"), TIMINGS)}
LENGTH = {"status": 200, "body": sse(delta(content="cut"), delta(finish="length"),
                                     {"timings": {"prompt_n": "7", "predicted_n": 3.9}}, sep="\r\n\r\n")}
NO_DONE = {"status": 200, "body": sse(delta(content="é😀"), "not json", ": comment", done=False, sep="\n")}
HTTP_503 = {"status": 503, "body": "model is loading " + "x" * 300}
EMPTY = {"status": 200, "body": ""}
CHUNK_LIST = {"status": 200, "body": sse([1, 2])}
TPS_LIST = {"status": 200, "body": sse(delta(content="a"), {"timings": {"prompt_per_second": [1]}})}
BIG_N = {"status": 200, "body": sse(delta(content="a"), {"timings": {"prompt_n": 2 ** 70}})}
BAD_N = {"status": 200, "body": sse(delta(content="a"), {"timings": {"predicted_n": "many"}})}
CONTENT_INT = {"status": 200, "body": sse(delta(content=5))}

MODELS = {"status": 200, "body": json.dumps({"data": [
    {"id": "m2", "status": {"value": "loaded"}}, {"id": "m1", "status": {"value": "unloaded"}},
    {"id": "m3"}, {"id": "", "status": {"value": "loaded"}}]})}
MODELS_LIST = {"status": 200, "body": "[]"}
MODELS_500 = {"status": 500, "body": "{}"}

PROMPTS = [
    "INSERT INTO prompts(id, name, body, created_at) VALUES(1, 'alpha', 'Say hi.', 1.0)",
    "INSERT INTO prompts(id, name, body, created_at) VALUES(2, 'beta', 'Écris \"ceci\".', 2.0)",
    "INSERT INTO prompts(id, name, body, created_at) VALUES(3, 'Gamma', 'Third', 3.0)",
]
B = "127.0.0.1"


def start(**body) -> list:
    return ["start", body]


# id, seed, models answer, chat answers (in order), contended answer, steps
CASES = [
    ("suite", PROMPTS, MODELS, [OK, LENGTH, NO_DONE, HTTP_503, OK, EMPTY, OK, OK], True,
     [start(backend=B, aliases=["m1", "m2"], promptIds=[2, 1], reps=2, maxTokens=100),
      ["progress"], ["run"], ["progress"]]),
    ("refusals", PROMPTS, MODELS, [OK], False,
     [start(backend="", aliases=["m1"], promptIds=[1]),
      start(backend="  ", aliases=["m1"], promptIds=[1]),
      start(backend=B, aliases=["", ""], promptIds=[1]),
      start(backend=B, aliases=[], promptIds=[1]),
      start(backend=B, aliases=["m1"], promptIds=[99]),
      start(backend=B, aliases=["m1"]),
      start(backend="missing", aliases=["m1"], promptIds=[1]),
      start(backend="noport", aliases=["m1"], promptIds=[1]),
      ["progress"],
      start(backend=B, aliases=["", None, 0, False], promptIds=[1]),
      ["progress"]]),
    ("clamps-and-conversions", PROMPTS, MODELS, [OK, OK], False,
     [start(backend=B, aliases="m1", promptIds=["1", 3.7, True], reps=0, maxTokens=99999),
      ["run"], ["progress"]]),
    ("clamp-low", PROMPTS, MODELS, [OK, OK, OK, OK, OK, OK, OK, OK, OK, OK], False,
     [start(backend=B, aliases=["m1"], promptIds=[1], reps=" 12 ", maxTokens="-5"), ["run"], ["progress"]]),
    ("already-running", PROMPTS, MODELS, [OK], False,
     [start(backend=B, aliases=["m1"], promptIds=[1], reps=1),
      start(backend=B, aliases=["m1"], promptIds=[1], reps=1),
      ["run"], ["progress"]]),
    ("cancel-before-run", PROMPTS, MODELS, [OK], False,
     [["cancel"], start(backend=B, aliases=["m1", "m2"], promptIds=[1], reps=1),
      ["cancel"], ["progress"], ["run"], ["progress"], ["cancel"]]),
    ("bad-conversions", PROMPTS, MODELS, [], False,
     [start(backend=B, aliases=["m1"], promptIds=["x"]),
      start(backend=B, aliases=["m1"], promptIds=[1], reps="abc"),
      start(backend=B, aliases=5, promptIds=[1]),
      start(backend=B, aliases=["m1"], promptIds=[2 ** 70]),
      start(backend=B, aliases=["m1"], promptIds=[1], reps=[1]),
      ["progress"]]),
    ("chunk-not-object", PROMPTS, MODELS, [CHUNK_LIST], False,
     [start(backend=B, aliases=["m1"], promptIds=[1], reps=1), ["run"], ["progress"]]),
    ("tps-list", PROMPTS, MODELS, [TPS_LIST], False,
     [start(backend=B, aliases=["m1"], promptIds=[1], reps=1), ["run"], ["progress"]]),
    ("count-past-64-bits", PROMPTS, MODELS, [BIG_N], False,
     [start(backend=B, aliases=["m1"], promptIds=[1], reps=1), ["run"], ["progress"]]),
    ("count-not-a-number", PROMPTS, MODELS, [BAD_N], False,
     [start(backend=B, aliases=["m1"], promptIds=[1], reps=1), ["run"], ["progress"]]),
    ("content-not-text", PROMPTS, MODELS, [CONTENT_INT], False,
     [start(backend=B, aliases=["m1"], promptIds=[1], reps=1), ["run"], ["progress"]]),
    ("models-not-object", PROMPTS, MODELS_LIST, [OK], False,
     [start(backend=B, aliases=["m1"], promptIds=[1], reps=1), ["run"], ["progress"]]),
    ("models-error", PROMPTS, MODELS_500, [OK, OK], True,
     [start(backend=B, aliases=["m1", "m1"], promptIds=[1], reps=1), ["run"], ["progress"]]),
]


class Llama(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    models: dict = {}
    chats: list = []
    seen: list = []

    def answer(self, a: dict) -> None:
        body = a["body"].encode("utf-8")
        self.send_response(a["status"])
        self.send_header("content-type", "text/event-stream" if a["status"] == 200 else "text/plain")
        self.send_header("content-length", str(len(body)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def do_GET(self):  # noqa: N802
        Llama.seen.append({"method": "GET", "path": self.path, "body": None})
        self.answer(Llama.models)

    def do_POST(self):  # noqa: N802
        n = int(self.headers.get("content-length") or 0)
        Llama.seen.append({"method": "POST", "path": self.path, "body": self.rfile.read(n).decode("utf-8")})
        self.answer(Llama.chats.pop(0) if Llama.chats else {"status": 500, "body": "no scripted answer"})

    def log_message(self, *args):
        pass


class NotFound(Exception):
    pass


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR"} \
                or "PROXY" in key.upper():
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import bench, config, db, services  # noqa: E402
    from app.main import app  # noqa: E402

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Llama)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_address[1]

    def get(name):
        if name == B:
            return types.SimpleNamespace(attrs={"NetworkSettings": {"Ports": {"x/tcp": None, f"{port}/tcp": None}}})
        if name == "noport":
            return types.SimpleNamespace(attrs={"NetworkSettings": {"Ports": {}}})
        raise NotFound(f"No such container: {name}")
    services._docker_client = lambda: types.SimpleNamespace(containers=types.SimpleNamespace(get=get))

    client = TestClient(app, raise_server_exceptions=False)
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest()
              for name in ("api.py", "bench.py", "db.py", "main.py")}
    out = []
    schema = None
    for case_id, seed, models, chats, contended, steps in CASES:
        Llama.models, Llama.chats, Llama.seen = models, list(chats), []
        observed: list = []
        perf = iter(PERF0 + PERF_STEP * k for k in range(100000))
        wall = iter(TIME0 + TIME_STEP * k for k in range(100000))
        bench.time = types.SimpleNamespace(perf_counter=lambda: next(perf), time=lambda: next(wall),
                                           sleep=lambda s: None)
        bench._peak_vram = lambda backend: observed.append({"peak": backend}) or list(PEAK)
        bench._other_traffic = lambda c, aliases, since, k=contended: observed.append(
            {"other": c, "aliases": sorted(aliases), "since": since}) or k
        bench._read_load_ms = lambda c, alias: observed.append({"load": c, "alias": alias}) or LOAD_MS
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
            db._DB_PATH = None
            db.init()
            c = sqlite3.connect(Path(root) / "model_loader.db", isolation_level=None)
            if schema is None:
                schema = [r[0] for r in c.execute(
                    "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")]
            c.execute("DELETE FROM prompts")
            for statement in seed:
                c.execute(statement)
            c.close()
            for step in steps:
                if step[0] == "start":
                    r = client.post("/api/v1/benchmark/start", json=step[1])
                elif step[0] == "progress":
                    r = client.get("/api/v1/benchmark/progress")
                elif step[0] == "cancel":
                    r = client.post("/api/v1/benchmark/cancel")
                else:
                    while held:
                        t = held.pop(0)
                        t.target(*t.args)
                    answers.append(None)
                    continue
                answers.append({"status": r.status_code, "content_type": r.headers.get("content-type"),
                                "body": r.content.decode("utf-8")})
            c = sqlite3.connect(Path(root) / "model_loader.db", isolation_level=None)
            tables = {t: [list(r) for r in c.execute(
                f"SELECT {cols} FROM {t} ORDER BY id")] for t, cols in (
                ("bench_run", "quote(id), quote(backend), quote(status), quote(started_at), quote(finished_at),"
                              " quote(reps), quote(max_tokens), quote(note)"),
                ("bench_variant", "quote(id), quote(run_id), quote(alias), quote(argv_json), quote(load_ms)"),
                ("bench_result", "quote(variant_id), quote(prompt_name), quote(rep), quote(cold), quote(ttft_ms),"
                                 " quote(ttft_answer_ms), quote(truncated), quote(total_ms), quote(prompt_n),"
                                 " quote(prompt_tps), quote(gen_n), quote(gen_tps), quote(draft_n),"
                                 " quote(draft_acc), quote(peak_vram_json), quote(contended), quote(err),"
                                 " quote(response_text)"))}
            c.close()
        out.append({"id": case_id, "seed": seed, "models": models, "chats": chats, "contended": contended,
                    "steps": steps, "answers": answers, "llama": Llama.seen, "observed": observed,
                    "tables": tables})
    server.shutdown()
    json.dump({"oracle": digest, "schema": schema, "port_placeholder": "PORT", "peak": PEAK, "load_ms": LOAD_MS,
               "perf": [PERF0, PERF_STEP], "time": [TIME0, TIME_STEP], "cases": out},
              sys.stdout, ensure_ascii=False, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
