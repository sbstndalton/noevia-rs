"""Records the Diary's POST /v1/chat/completions (app.py `v1_chat_completions`, `_run_exchange`,
`exchange_stream`) over one synthetic managed tenant and a deterministic stub engine, for
tests/exchange_route.rs (shift 4, step 6).

Run with CPython 3.12 and the frozen noevia-services checkout (read-only):

    python3.12 -I tests/generate_exchange_route_fixtures.py <services>/diary tests/exchange-route-fixtures.json

The app is imported in a clean environment and called through FastAPI's TestClient (no
lifespan). The stub engine answers `/v1/models`, `/v1/embeddings` (a vector computed from each
input text, as the Rust test computes it) and `/v1/chat/completions` by fixed rules; every chat
request and its reply are recorded, so the Rust side replays the replies by request and so checks
that it sends the same requests. Recorded per case: the request, the status and the JSON reply
(or the stream's events), the chat requests, and the corpus afterwards. Exchange ids (random
UUIDs) and `created` are masked. Synthetic text only; no real model.
"""
import hashlib
import json
import math
import os
import re
import sqlite3
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

if sys.version_info[:2] != (3, 12):
    sys.exit("record with CPython 3.12")
diary = Path(sys.argv[1]).resolve()
out = Path(sys.argv[2]).resolve()
SOURCES = ["agent/app.py", "agent/streaming.py", "agent/pipeline.py", "agent/context.py", "agent/llm.py",
           "agent/corpus_store.py", "agent/retrieval.py", "agent/embedding.py", "config/prompts/logging.md",
           "config/prompts/system.md"]
UID = "7c25d4e5-0b93-4df5-8087-8192a3b4c5d6"
TOKEN = "qa-service-token-r9"
THINKING_HINT = ("Output must be the final answer only. Do not show reasoning; do not wrap the answer in "
                 "quotes or code fences.")
DIM = 16
scratch = Path(tempfile.mkdtemp(prefix="qa-exchange-route-"))


def vector(text):
    """Deterministic, text-dependent, unit length (tests/exchange_route.rs computes the same)."""
    v = [0.0] * DIM
    for word in text.lower().split():
        h = hashlib.sha256(word.encode()).digest()
        for i in range(DIM):
            v[i] += (h[i] - 127.5) / 127.5
    norm = math.sqrt(sum(x * x for x in v)) or 1.0
    return [x / norm for x in v]


CALLS = []


def chat_reply(payload):
    """(status, body) for one chat request, by fixed rules on its messages."""
    messages = payload.get("messages") or []
    system = messages[0].get("content", "") if messages else ""
    last = messages[-1].get("content", "") if messages else ""
    text = "\n".join(m.get("content", "") for m in messages if isinstance(m.get("content"), str))
    if system == THINKING_HINT:
        if "QA-AUXFAIL" in last:
            return 500, {"error": "synthetic aux failure"}
        if "Verdict (LOG or SKIP):" in last:
            content = "SKIP" if "QA-SKIP" in last else "LOG"
        elif "Assistant reply to convert:" in last:
            content = "The diary companion noted a synthetic QA exchange."
        else:
            content = "NO"
        return 200, {"choices": [{"message": {"content": content}, "finish_reason": "stop"}]}
    question = last
    if "QA-FAIL" in question:
        return 500, {"error": "synthetic engine failure"}
    if "QA-EMPTY" in question:
        content, finish = "", "stop"
    elif "QA-TRUNC" in question:
        content, finish = "A partial synthetic QA answer that the cap cut off", "length"
    else:
        seen = "reference" if "QA reference" in text else "no reference"
        content, finish = f"A synthetic QA reply ({seen}, {len(messages)} messages).\n[LOG: ok]", "stop"
    return 200, {"choices": [{"message": {"content": content}, "finish_reason": finish}]}


class Engine(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _send(self, status, body, content_type="application/json"):
        data = body.encode() if isinstance(body, str) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.endswith("/models"):
            self._send(200, {"data": [{"id": "qa-chat"}, {"id": "qa-aux"},
                                      {"id": "qa-embed", "status": {"value": "loaded", "args": ["--embedding"]}}]})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        if self.path.endswith("/embeddings"):
            inputs = payload["input"] if isinstance(payload["input"], list) else [payload["input"]]
            self._send(200, {"data": [{"index": i, "embedding": vector(t)} for i, t in enumerate(inputs)]})
            return
        status, body = chat_reply(payload)
        if payload.get("stream") and status == 200:
            choice = body["choices"][0]
            chunks = [{"choices": [{"delta": {"content": choice["message"]["content"]}}]},
                      {"choices": [{"delta": {}, "finish_reason": choice["finish_reason"]}]}]
            text = "".join("data: " + json.dumps(c) + "\n\n" for c in chunks) + "data: [DONE]\n\n"
            CALLS.append({"request": payload, "status": 200, "reply": text, "stream": True})
            self._send(200, text, "text/event-stream")
            return
        CALLS.append({"request": payload, "status": status, "reply": json.dumps(body), "stream": False})
        self._send(status, body)


engine = ThreadingHTTPServer(("127.0.0.1", 0), Engine)
threading.Thread(target=engine.serve_forever, daemon=True).start()
base = f"http://127.0.0.1:{engine.server_address[1]}/v1"
state_dir = scratch / "state"
state_dir.mkdir()
CONFIG = (
    "llm:\n"
    f"  base_url: {base}\n  chat_model: qa-chat\n  embed_model: qa-embed\n  timeout_s: 20\n  max_retries: 1\n"
    f"  aux:\n    base_url: {base}\n    model: qa-aux\n"
    "corpus:\n  backend: local\n  root: ''\n  monthly_prefix: ''\n  entry_layout: daily\n  entries_prefix: Entries\n"
    "  index_enabled: false\n  month_file_template: '{year}-{month02}.md'\n  index_file: INDEX.md\n"
    f"  local:\n    root: {scratch / 'default'}\n"
    f"retrieval:\n  db_path: {state_dir / 'index.db'}\n  top_k: 4\n  min_score: 0.0\n  max_context_tokens: 900\n"
    "context:\n  max_today_tokens: 600\n  max_standing_tokens: 300\n  direct_past_days: 3\n  max_direct_past_tokens: 400\n"
    f"ui:\n  auth_token: {TOKEN}\n"
)
(scratch / "config.yaml").write_text(CONFIG, encoding="utf-8")
os.environ.clear()
os.environ.update({"PATH": "/usr/bin:/bin", "HOME": str(scratch / "home"), "TZ": "UTC",
                   "DIARY_CONFIG": str(scratch / "config.yaml"), "DIARY_ALLOW_OPEN": "1",
                   "DIARY_CORPUS_WRITE_IMPL": "python"})
os.chdir(scratch)
sys.path.insert(0, str(diary))
from fastapi.testclient import TestClient  # noqa: E402
from agent import app as appmod  # noqa: E402
from agent import tenant_writer  # noqa: E402
from agent.managed_storage import ManagedCorpusBackend  # noqa: E402

CORPUS = {
    "Entries/2026/January/January 2, 2026.md":
        "## Friday, January 2, 2026\n\n### 08:30 — Garden\n\n**Me:** Synthetic QA plan: plant beans by the canal wall.\n",
    "Entries/2026/January/January 4, 2026.md":
        "## Sunday, January 4, 2026\n\n### 18:00 — Kites\n\n**Me:** Flew the teal kite with Qa Orrin Vell at the quay.\n",
    "Entries/2026/January/January 6, 2026.md":
        "## Tuesday, January 6, 2026\n\n### 07:45 — Morning\n\n**Me:** Synthetic QA walk along the canal before work.\n",
    "AI Memory/notes.md": "QA memory: prefers short answers.\n",
}
root = state_dir / "users" / UID
managed = ManagedCorpusBackend(root, UID)
with tenant_writer.hold(UID):
    managed.activate({p: t.encode() for p, t in CORPUS.items()},
                     {"entry_layout": "daily", "month_file_template": "{year}-{month02}.md", "index_enabled": False})
managed.close()
client = TestClient(appmod.app)
HEADERS = {"Authorization": f"Bearer {TOKEN}", "X-Cowork-User-ID": UID}
WHEN = "2026-01-06T09:15:00+00:00"


def body(message, **extra):
    return {"messages": [{"role": "user", "content": message}], "entryTime": WHEN, **extra}


CASES = [
    ("plain", body("What did I write about the canal?")),
    ("skip", body("QA-SKIP is the diary formatted right?")),
    ("history", {"messages": [{"role": "user", "content": "Did I fly a kite?"},
                              {"role": "assistant", "content": "Yes, the teal one."},
                              {"role": "user", "content": "With whom?"}],
                 "entryTime": WHEN, "session_id": "qa-history"}),
    ("same-session", body("And where was that?", session_id="qa-history")),
    ("extra", body("Anything to add?", extrasEnabled=True, extraContext="QA reference: a synthetic note.")),
    ("entry-day", body("What about the garden?", entryDay="2026-01-05")),
    ("bad-entry", {"messages": [{"role": "user", "content": "Naive time?"}], "entryTime": "2026-01-06T09:15:00",
                   "session_id": "qa-bad-entry"}),
    ("truncated", body("QA-TRUNC tell me everything")),
    ("empty", body("QA-EMPTY say nothing")),
    ("fail", body("QA-FAIL break")),
    ("aux-fail", body("QA-AUXFAIL keep it anyway")),
    ("bad-session-id", body("Which session?", session_id="bad id!")),
    ("stream", body("Streamed: the kite?", stream=True, diary_events=True, session_id="qa-stream")),
    ("stream-skip", body("QA-SKIP streamed formatting", stream=True, diary_events=True)),
    ("stream-fail", body("QA-FAIL streamed", stream=True, diary_events=True)),
    ("stream-bad-entry", {**body("Streamed naive", stream=True, diary_events=True), "entryTime": "2026-01-06T09:15"}),
]

xids = {}


def mask(text):
    def sub(m):
        return xids.setdefault(m.group(0), f"XID-{len(xids) + 1}")
    return re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
                  lambda m: m.group(0) if m.group(0) == UID else sub(m), text)


def masked(value):
    value = json.loads(mask(json.dumps(value, ensure_ascii=False)))
    if isinstance(value, dict) and "created" in value:
        value["created"] = 0
    return value


def corpus():
    db = sqlite3.connect(root / "managed-diary.db")
    try:
        return {p: mask(bytes(d).decode()) for p, d in db.execute("SELECT path, data FROM files ORDER BY path")}
    finally:
        db.close()


def events(text):
    out = []
    for frame in text.split("\n\n"):
        if frame.startswith("data: "):
            out.append(masked(json.loads(frame[len("data: "):])))
    return out


records = []
for label, request in CASES:
    start = len(CALLS)
    if request.get("stream") is True:
        with client.stream("POST", "/v1/chat/completions", json=request, headers=HEADERS) as r:
            text = r.read().decode()
            status = r.status_code
        reply = {"events": events(text)} if status == 200 else {"json": masked(json.loads(text))}
    else:
        r = client.post("/v1/chat/completions", json=request, headers=HEADERS)
        status, reply = r.status_code, {"json": masked(r.json())}
    chat = [{"request": c["request"], "status": c["status"], "reply": c["reply"], "stream": c["stream"]}
            for c in CALLS[start:]]
    records.append({"case": label, "request": request, "status": status, **reply,
                    "chat": chat, "corpus": corpus()})

# The tenant's state as the exchanges used it: its embedding target, prompt, cap and config.
state = next(s for s in appmod._tenant_states.values() if getattr(s, "tenant_id", None) == UID)
target = state.retrieval.embedding.active()
active = {"base_url": target.base_url, "model": target.model, "profile": target.profile}
system_prompt = state.assembler._system_prompt()
cap = state.max_answer_tokens
cfg = state.cfg
templates = __import__("yaml").safe_load((diary / "config/prompts/logging.md").read_text(encoding="utf-8"))
fixture = {
    "source": {p: hashlib.sha256((diary / p).read_bytes()).hexdigest() for p in SOURCES},
    "python": "%d.%d.%d" % sys.version_info[:3],
    "uid": UID,
    "token": TOKEN,
    "config_yaml": CONFIG.replace(base, "STUB").replace(str(scratch), "SCRATCH"),
    "embedding_target": active,
    "settings": {
        "cfg": {"context": cfg.get("context") or {}, "retrieval": {**(cfg.get("retrieval") or {}), "db_path": "",
                                                                  "embedding_state_path": ""}},
        "max_answer_tokens": cap,
        "system_prompt": system_prompt,
        "templates": templates,
    },
    "corpus_before": CORPUS,
    "cases": records,
}
out.write_text(json.dumps(fixture, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
print("recorded", len(records), "cases;", sum(len(r["chat"]) for r in records), "chat calls")
engine.shutdown()
