#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/backend-control-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (`api.restart_backend`, `api.backend_test`,
`services.restart_llama_backend`, `services.test_prompt`, `_resolve_internal_port`,
`_probe_loaded_model`) against a fake Docker client and a fake llama-server on 127.0.0.1, and
records every answer, the request the fake server saw, and the restart error left behind:

    python3 -I tools/gen-backend-control-oracle.py <services>/model-manager/app > fixture.json

The backend is named `127.0.0.1`, so `http://{name}:{port}` reaches the fake server. `time.time`
is pinned (1000.0, then 1000.5) around each test call so `elapsed_s` and `tokens_per_s` are
reproducible. No Docker daemon, container or model is touched.
"""
from __future__ import annotations

import hashlib
import http.server
import json
import os
import sys
import threading
import time
from pathlib import Path

NAME = "127.0.0.1"
OK_MODELS = json.dumps({"data": [{"id": "m-a", "status": {"value": "loaded"}}, {"id": "m-b", "status": {"value": "unloaded"}}]})
OK_CHAT = json.dumps({"choices": [{"message": {"content": "synthetic reply é"}}], "usage": {"completion_tokens": 7, "prompt_tokens": 3}})


def spec(ports=None, cmd=None, env=None):
    return {"ports": ports if ports is not None else {"{PORT}/tcp": None}, "cmd": cmd or [], "entrypoint": [], "env": env or []}


def step(path, body=None, llama=None, restart_error=None):
    return {"path": path, "body": None if body is None else json.dumps(body, ensure_ascii=False),
            "llama": llama or {"models": [200, OK_MODELS], "chat": [200, OK_CHAT]}, "restart_error": restart_error}


T = f"/api/v1/backends/{NAME}/test"
R = f"/api/v1/backends/{NAME}/restart"
CASES = [
    ("test-ok-and-forms", spec(), [
        step(T, {"prompt": "  hello  ", "maxTokens": 32}),
        step(T, {"prompt": "x", "maxTokens": 99999}),
        step(T, {"prompt": "x", "maxTokens": "12"}),
        step(T, {"prompt": "x", "maxTokens": 2.9}),
        step(T, {"prompt": "x", "maxTokens": 0}),
        step(T, {"prompt": "x", "maxTokens": True}),
        step(T, {"prompt": 42}),
        step(T, {"prompt": "  "}),
        step(T, {}),
        step("/api/v1/backends/other/test", {"prompt": "x"}),
    ]),
    ("probe-notes", spec(), [
        step(T, {"prompt": "x"}, {"models": [500, "x"], "chat": [200, OK_CHAT]}),
        step(T, {"prompt": "x"}, {"models": [200, "not json"], "chat": [200, OK_CHAT]}),
        step(T, {"prompt": "x"}, {"models": [200, json.dumps({"data": []})], "chat": [200, OK_CHAT]}),
        step(T, {"prompt": "x"}, {"models": [200, json.dumps({"data": [{"id": "a", "status": {"value": "x"}}]})], "chat": [200, OK_CHAT]}),
        step(T, {"prompt": "x"}, {"models": [200, json.dumps({"data": [{"id": "fixed"}]})], "chat": [200, OK_CHAT]}),
        step(T, {"prompt": "x"}, {"models": [200, json.dumps({"data": [{"id": ""}]})], "chat": [200, OK_CHAT]}),
        step(T, {"prompt": "x"}, {"models": [200, json.dumps([1])], "chat": [200, OK_CHAT]}),
    ]),
    ("chat-forms", spec(), [
        step(T, {"prompt": "x"}, {"models": [200, OK_MODELS], "chat": [503, "busy"]}),
        step(T, {"prompt": "x"}, {"models": [200, OK_MODELS], "chat": [200, json.dumps({})]}),
        step(T, {"prompt": "x"}, {"models": [200, OK_MODELS], "chat": [200, json.dumps({"choices": [], "usage": {"completion_tokens": "4"}})]}),
        step(T, {"prompt": "x"}, {"models": [200, OK_MODELS], "chat": [200, json.dumps({"choices": [None], "usage": {"completion_tokens": 0}})]}),
        step(T, {"prompt": "x"}, {"models": [200, OK_MODELS], "chat": [200, json.dumps({"choices": [{"message": {"content": ["a"]}}]})]}),
        step(T, {"prompt": "x"}, {"models": [200, OK_MODELS], "chat": [200, json.dumps({"choices": "x"})]}),
        step(T, {"prompt": "x"}, {"models": [200, OK_MODELS], "chat": [200, json.dumps({"usage": {"completion_tokens": "x"}})]}),
        step(T, {"prompt": "x"}, {"models": [200, OK_MODELS], "chat": [200, "[]"]}),
    ]),
    ("ports-from-command-and-env", spec(ports={"x/tcp": None}, cmd=["--port", "{PORT}"]), [step(T, {"prompt": "x"})]),
    ("port-from-env", spec(ports={}, env=["LLAMA_ARG_PORT= {PORT} "]), [step(T, {"prompt": "x"})]),
    ("not-found-container", None, [step(T, {"prompt": "x"}), step(R)]),
    ("restart-ok", spec(), [step(R)]),
    ("restart-fails", spec(), [step(R, restart_error="Docker refused the restart")]),
]


class Llama(http.server.BaseHTTPRequestHandler):
    current: dict = {}
    seen: list = []

    def log_message(self, *a):
        pass

    def _send(self, status, body):
        data = body.encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self._send(*Llama.current["models"])

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        Llama.seen.append(json.loads(self.rfile.read(n)))
        self._send(*Llama.current["chat"])


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {"MODEL_LOADER_TOKEN", "LLAMA_DEFAULT_PORT"}:
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from docker.errors import APIError, NotFound
    from fastapi.testclient import TestClient

    from app import services
    from app.main import app

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Llama)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    services._effective_container_names = lambda: [NAME]

    def fill(value):
        if isinstance(value, str):
            return value.replace("{PORT}", str(port))
        if isinstance(value, list):
            return [fill(v) for v in value]
        if isinstance(value, dict):
            return {fill(k): fill(v) for k, v in value.items()}
        return value

    class Container:
        def __init__(self, s, error):
            self.attrs = {"NetworkSettings": {"Ports": fill(s["ports"])},
                          "Config": {"Cmd": fill(s["cmd"]), "Entrypoint": fill(s["entrypoint"]), "Env": fill(s["env"])}}
            self.error = error

        def restart(self, timeout=None):
            if self.error:
                raise APIError(f"{self.error} (HTTP 500)")

    class Client:
        def __init__(self, s, error):
            self.s, self.error = s, error

        @property
        def containers(self):
            return self

        def get(self, name):
            if self.s is None or name != NAME:
                raise NotFound("synthetic")
            return Container(self.s, self.error)

    real_time = time.time
    client = TestClient(app, raise_server_exceptions=False)
    digest = {n: hashlib.sha256((app_dir / n).read_bytes()).hexdigest() for n in ("api.py", "services.py", "main.py")}
    out = []
    for case_id, s, steps in CASES:
        answers = []
        for st in steps:
            services._restart_errors.clear()
            services._docker_client = lambda s=s, e=st["restart_error"]: Client(s, e)
            Llama.current, Llama.seen = st["llama"], []
            ticks = iter([1000.0, 1000.5])

            def pinned(ticks=ticks):
                # Only test_prompt's own two reads are pinned; httpx and anyio keep the real clock.
                if sys._getframe(1).f_code.co_name == "test_prompt":
                    return next(ticks, 1000.5)
                return real_time()
            time.time = pinned
            body = None if st["body"] is None else st["body"].encode()
            headers = {"content-type": "application/json"} if body is not None else {}
            r = client.post(st["path"], content=body, headers=headers)
            time.time = real_time
            deadline = real_time() + 10
            while any(t.name.startswith("restart-") for t in threading.enumerate()) and real_time() < deadline:
                time.sleep(0.01)
            answers.append({"status": r.status_code, "content_type": r.headers.get("content-type"),
                            "body": r.content.decode().replace(str(port), "{PORT}"),
                            "chat_requests": Llama.seen,
                            "restart_error": services._restart_errors.get(NAME)})
        out.append({"id": case_id, "spec": s, "steps": steps, "answers": answers})
    server.shutdown()
    json.dump({"oracle": digest, "cases": out}, sys.stdout, ensure_ascii=False, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
