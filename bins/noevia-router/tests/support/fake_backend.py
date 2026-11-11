#!/usr/bin/env python3
"""A fake OpenAI-compatible backend for the router's tests (port of the prototype's): /health is 503
for --delay seconds, then ok; /v1/chat/completions answers "<name>: <last user message>" (streamed
word by word with stream=true). --container NAME registers a fake docker container (a file in
$FAKE_DOCKER_DIR) for as long as it runs, like `docker run --rm`."""
import argparse, json, os, signal, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ap = argparse.ArgumentParser()
ap.add_argument("--port", type=int); ap.add_argument("--name"); ap.add_argument("--delay", type=float, default=0)
ap.add_argument("--exit-after", type=float, default=0, help="exit with code 3 after this many seconds (a crashing engine)")
ap.add_argument("--slow", type=float, default=0, help="seconds between streamed chunks")
ap.add_argument("--ignore-term", action="store_true", help="ignore SIGTERM (only SIGKILL stops it)")
ap.add_argument("--container", default="", help="fake docker container to register while running")
ap.add_argument("--hang", action="store_true", help="accept requests and never answer (a wedged engine)")
ap.add_argument("--abort-file", default="", help="append a line here when a client disconnects mid-stream")
a = ap.parse_args(); T0 = time.monotonic()
if a.ignore_term:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
if a.container:
    open(os.path.join(os.environ["FAKE_DOCKER_DIR"], a.container), "a").close()
if a.exit_after:
    threading.Timer(a.exit_after, lambda: os._exit(3)).start()


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *x): pass
    def _j(self, code, obj):
        b = json.dumps(obj).encode(); self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        if self.path == "/health":
            return self._j(200, {"status": "ok", "name": a.name}) if time.monotonic() - T0 >= a.delay else self._j(503, {"status": "loading"})
        self._j(404, {})
    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if a.hang:
            time.sleep(3600)
        text = f"{a.name}: {req['messages'][-1]['content']} (model={req.get('model')}) (path={self.path})"
        if not req.get("stream"):
            return self._j(200, {"object": "chat.completion", "model": a.name, "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}]})
        self.send_response(200); self.send_header("Content-Type", "text/event-stream"); self.send_header("Connection", "close"); self.end_headers()
        try:
            for w in text.split(" "):
                self.wfile.write(b"data: " + json.dumps({"object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"content": w + " "}}]}).encode() + b"\n\n"); self.wfile.flush()
                if a.slow: time.sleep(a.slow)
            self.wfile.write(b"data: [DONE]\n\n"); self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            if a.abort_file:
                with open(a.abort_file, "a") as f: f.write(f"{a.name} client gone\n")
        self.close_connection = True


ThreadingHTTPServer(("127.0.0.1", a.port), H).serve_forever()
