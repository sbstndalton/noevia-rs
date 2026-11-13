"""Record exchange-fixtures.json from the frozen Python Diary exchange (app._run_exchange).

Usage (CPython 3.12, the Diary requirements installed; the import writes only under a temp dir):

    <venv>/bin/python -I crates/diary-answer/tests/generate_exchange_fixtures.py <noevia-services>/diary

The real _run_exchange, ContextAssembler, LoggingPipeline and LLMClient (sha256 pinned,
noevia-services origin/main 6d0ba0e) run against a fake store, a stub retriever and two scripted
model engines on httpx.MockTransport (the companion and the aux model). Each case records the
result or the failure, every emitted event, every request to each engine, every store call and the
session afterwards. Synthetic text only.
"""

import hashlib
import json
import os
import pathlib
import random
import sys
import tempfile
import types
from datetime import datetime

sys.dont_write_bytecode = True

PINNED = {
    "agent/app.py": "ad3ff495c93765dc7b5dff1ee0231f07cd7a4bfa7c3c3efb50d4d72780748f2c",
    "agent/context.py": "b972d8d2af8ed51cf46068d5affe002d6992ac36be0791e942aa9fd9473e428f",
    "agent/pipeline.py": "c86aba1d33fbff2b2812d8aa3fe2406ba0d4828dd17ade55377717dabd746b1c",
    "agent/llm.py": "cd18536747b405221b6a3a2dba67412ee1dec172d304f9cb5bde846b36d1b3a2",
    "config/prompts/logging.md": "b28d5d7e03db5fbc9875a433c9013042ad88f7413c7d4e986a9a3d8467fcf661",
}
OUT = pathlib.Path(__file__).with_name("exchange-fixtures.json")
SYSTEM_PROMPT = "QA SYSTEM PROMPT for exchange fixtures."
TENANT = "00000000-0000-4000-8000-0000000000a9"  # synthetic prefix (shadow_compare.SYNTHETIC_PREFIX)


def load(diary_dir):
    root = pathlib.Path(diary_dir).resolve()
    for rel, want in PINNED.items():
        got = hashlib.sha256((root / rel).read_bytes()).hexdigest()
        if got != want:
            sys.exit(f"refusing {rel}: sha256 {got} is not the pinned {want}")
    tmp = tempfile.mkdtemp(prefix="qa-r9-exchange-")
    os.environ["DB_PATH"] = str(pathlib.Path(tmp) / "data" / "index.db")
    os.environ["CORPUS_LOCAL_ROOT"] = str(pathlib.Path(tmp) / "data" / "corpus")
    for key in ("DIARY_TEMPORAL_FOCUS_IMPL", "DIARY_SCORE_GAP_IMPL", "DIARY_ENTITY_FOCUS_IMPL", "DIARY_RUST_ROUTES"):
        os.environ.pop(key, None)
    sys.path.insert(0, str(root))
    import yaml
    from agent import app, context, llm, pipeline, workspace_files  # noqa: E402
    from agent.config import Config  # noqa: E402
    workspace_files.memory_text = lambda store: store.memory
    app._reindex_dirty = lambda st: None
    templates = yaml.safe_load((root / "config/prompts/logging.md").read_text(encoding="utf-8"))
    return app, context, llm, pipeline, Config, templates


class Store:
    def __init__(self, spec):
        self.spec = spec
        self.memory = spec.get("memory", "")
        self.index_enabled = spec.get("index_enabled", False)
        self.calls = []

    def require_write_transform(self):
        pass

    def get_day_text(self, day, max_chars=None):
        self.calls.append(["day", day.isoformat(), max_chars])
        text = self.spec.get("days", {}).get(day.isoformat(), "")
        return text[:max_chars] if max_chars is not None else text

    def get_standing_sections_text(self, max_chars=None):
        self.calls.append(["standing", max_chars])
        text = self.spec.get("standing", "")
        return text[:max_chars] if max_chars is not None else text

    def log_exchange(self, day, sub_header, me_text, claude_text, now=None):
        self.calls.append(["log_exchange", day.isoformat(), sub_header, me_text, claude_text])
        if self.spec.get("log") == "fail":
            raise OSError("synthetic write failure")
        return "qa-xid-0042"

    def update_standing_sections(self, open_q, timeline, today):
        self.calls.append(["update", open_q, timeline, today])
        return "qa-journal-7"


class Retriever:
    def __init__(self, rows):
        self.rows = rows
        self.calls = []

    def search(self, query, top_k=8, min_score=0.30, with_ids=False, diag=None):
        self.calls.append([query, top_k, min_score, with_ids])
        hits = [dict(r) for r in self.rows if r["score"] >= min_score][:top_k]
        if not with_ids:
            for h in hits:
                h.pop("id", None)
        return hits


def engine(httpx, responses, sent):
    queue = list(responses)

    def handler(request):
        sent.append(json.loads(request.content))
        status, ctype, body = queue.pop(0) if queue else (500, "text/plain", "no more synthetic answers")
        return httpx.Response(status, headers={"content-type": ctype}, content=body.encode("utf-8"))
    return httpx.MockTransport(handler)


def sse(chunks, finish="stop", reasoning=None):
    lines = []
    if reasoning:
        lines.append("data: " + json.dumps({"choices": [{"delta": {"reasoning_content": reasoning}}]}))
    for c in chunks:
        lines.append("data: " + json.dumps({"choices": [{"delta": {"content": c}}]}))
    lines.append("data: " + json.dumps({"choices": [{"delta": {}, "finish_reason": finish}]}))
    lines.append("data: [DONE]")
    return (200, "text/event-stream", "\n\n".join(lines) + "\n\n")


def plain(content, finish="stop"):
    return (200, "application/json", json.dumps({"choices": [{"message": {"content": content}, "finish_reason": finish}]}))


ANSWERS = [
    ["The synthetic diary says ", "you walked to the canal.", "\n[LOG: ok]"],
    ["<think>private</think>", "A short synthetic answer."],
    ["Skip this one.", "\n[LOG: skip]"],
    ["Long synthetic answer " * 40],
    [""],
    ["<think>never closed"],
    ["Answer with é and 🌿 ", "and more text.", "\n\n[LOG: ok]\n"],
]
VERDICTS = ["LOG", "SKIP", "log please", None]
SUMMARIES = ['"The user asked about the canal."', "Plain synthetic summary.", None]
ROWS = [
    {"id": 1, "day": "2031-03-02", "header": "10:00 — Canal", "score": 0.81,
     "text": "[2031-03-02] ## Day\n### 10:00 — Canal\n**Me:** Walked to the canal with Qa Orrin Vell."},
    {"id": 2, "day": "2031-03-05", "header": "18:00 — Kites", "score": 0.66,
     "text": "[2031-03-05] ## Day\n### 18:00 — Kites\n**Me:** Flew a teal kite."},
]


def case_list(rng):
    cases = []
    for i in range(90):
        streamed = rng.random() < 0.7
        answer = rng.choice(ANSWERS)
        finish = rng.choice(["stop"] * 5 + ["length"])
        if streamed:
            main = [sse(answer, finish, reasoning=rng.choice([None, "hidden reasoning"]))]
        else:
            main = [plain("".join(answer), finish)]
        if rng.random() < 0.05:
            main = [(rng.choice([404, 503]), "text/plain", "synthetic failure")]
        aux = []
        v = rng.choice(VERDICTS)
        aux.append((500, "text/plain", "aux down") if v is None else plain(v))
        s = rng.choice(SUMMARIES)
        aux.append((500, "text/plain", "aux down") if s is None else plain(s))
        aux.append(plain(rng.choice(["UPDATE", "NO"])))
        aux.append(plain('{"open_questions": [{"action": "add", "text": "QA?"}], "timeline": []}'))
        turns = []
        if rng.random() < 0.4:
            turns = [{"role": "user", "content": "What kite colour does Qa Tamsin Fennimore fly?"},
                     {"role": "companion", "content": "Teal."}]
        if rng.random() < 0.1:
            turns = [t for k in range(20) for t in ({"role": "user", "content": f"q{k}"}, {"role": "companion", "content": f"a{k}"})]
        message = rng.choice(["How was the canal walk?", "And what about Qa Tamsin Fenimore?", "Tell me about kites."])
        cases.append({
            "name": f"exchange_{i:03d}", "streamed": streamed, "message": message, "turns": turns,
            "main": main, "aux": aux, "trace": rng.random() < 0.5,
            "extra_context": rng.choice(["", "", "QA external reference " * rng.choice([1, 800])]),
            "max_answer_tokens": rng.choice([1024, 10, 200]),
            "store": {"days": {"2031-03-20": "### 09:00\n**Me:** synthetic today."}, "standing": "",
                      "memory": rng.choice(["", "QA memory"]), "index_enabled": rng.random() < 0.5,
                      "log": rng.choice(["ok"] * 8 + ["fail"])},
            "rows": rng.choice([ROWS, [], ROWS[:1]]),
        })
    return cases


def run(case, mods):
    app, context, llm, pipeline, Config, templates = mods
    import httpx
    store = Store(case["store"])
    retriever = Retriever(case["rows"])
    asm = context.ContextAssembler(store, retriever, Config({"retrieval": {}, "context": {}}))
    asm._system_prompt = lambda: SYSTEM_PROMPT
    main_sent, aux_sent = [], []
    main = llm.LLMClient("http://qa-main.invalid/v1", chat_model="qa-chat", max_retries=1, retry_backoff_s=0.0)
    main._client = httpx.Client(base_url="http://qa-main.invalid/v1", transport=engine(httpx, case["main"], main_sent),
                                follow_redirects=True)
    aux = llm.LLMClient("http://qa-aux.invalid/v1", chat_model="qa-aux", max_retries=1, retry_backoff_s=0.0)
    aux._client = httpx.Client(base_url="http://qa-aux.invalid/v1", transport=engine(httpx, case["aux"], aux_sent),
                               follow_redirects=True)
    st = types.SimpleNamespace(store=store, assembler=asm, llm_main=main,
                               pipeline=pipeline.LoggingPipeline(store, main, aux, templates),
                               max_answer_tokens=case["max_answer_tokens"])
    sess = app._session(case["name"], TENANT)
    sess["turns"][:] = [dict(t) for t in case["turns"]]
    events = []
    try:
        result = app._run_exchange(st, case["message"], case["name"], TENANT, entry_time=datetime(2031, 3, 20, 9, 30),
                                   background=False, extra_context=case["extra_context"],
                                   emit=events.append if case["streamed"] else None, retrieval_trace=case["trace"])
        outcome = {"result": result}
    except Exception as exc:  # noqa: BLE001 - the Rust must fail the same cases the same way
        outcome = {"error": type(exc).__name__}
    main.close()
    aux.close()
    return {**outcome, "events": events, "main_sent": main_sent, "aux_sent": aux_sent, "store_calls": store.calls,
            "search_calls": retriever.calls, "session": sess["turns"]}


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    if sys.version_info[:2] != (3, 12):
        sys.exit("record with CPython 3.12, the Diary image's interpreter")
    mods = load(sys.argv[1])
    rng = random.Random(1071)
    cases = case_list(rng)
    for c in cases:
        c["expected"] = run(c, mods)
    data = {"source": "noevia-services diary at origin/main 6d0ba0e", "python": sys.version.split()[0],
            "pinned_sha256": PINNED, "templates": mods[5], "system_prompt": SYSTEM_PROMPT, "cases": cases}
    OUT.write_text(json.dumps(data, ensure_ascii=False, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({len(cases)} cases)")


if __name__ == "__main__":
    main()
