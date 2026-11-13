"""Record llm-fixtures.json from the frozen Python Diary model client (diary/agent/llm.py).

Usage (CPython 3.12, the Diary image's interpreter, with the Diary requirements installed):

    <venv>/bin/python -I crates/diary-core/tests/generate_llm_fixtures.py <noevia-services>/diary

Refuses any source whose sha256 is not pinned below (noevia-services origin/main 6d0ba0e). No model
or network is involved: the real `LLMClient` talks to an `httpx.MockTransport` that answers with
synthetic bodies (seeded generator), and the text helpers run on synthetic strings. Each case
records what the client sent, every event it emitted and what it returned or raised.
"""

import hashlib
import json
import pathlib
import random
import re
import sys

sys.dont_write_bytecode = True

PINNED = {
    "agent/llm.py": "cd18536747b405221b6a3a2dba67412ee1dec172d304f9cb5bde846b36d1b3a2",
    "agent/util.py": "f1cfedf68937724331989b9083e2fff661b0c8b09db86659f32e41803ea4dfe9",
}
OUT = pathlib.Path(__file__).with_name("llm-fixtures.json")


def load(diary_dir):
    root = pathlib.Path(diary_dir).resolve()
    for rel, want in PINNED.items():
        got = hashlib.sha256((root / rel).read_bytes()).hexdigest()
        if got != want:
            sys.exit(f"refusing {rel}: sha256 {got} is not the pinned {want}")
    sys.path.insert(0, str(root))
    from agent import llm  # noqa: E402
    return llm


# ---------------- text helpers ----------------

def text_cases(rng):
    base = [
        "", " ", "plain answer", "<think>hidden</think>answer", "  <THINK>x</THINK>  answer",
        "<thinking>a</thinking><think>b</think>tail", "<think>never closed", "<think>a</thinking>mismatch",
        "answer <think>later tag stays</think>", "<thinking>multi\nline\nreasoning</thinking>\n\nanswer",
        "<think></think>", "<Think>x</tHiNk>ok", "<think>x</think>  <think>y</think>z", "<thi", "<THINKI", "  <thinking",
        "\n\n<think>x</think>", "<think>a</think><think>unclosed", "<thinkx>no</thinkx>", "<think >sp</think>",
        "answer\n[LOG: ok]", "answer\n\n[LOG: skip]\n", "answer [LOG: ok]", "[LOG: ok] answer", "answer\n[LOG:ok]  ",
        "answer\n[LOG: OK]", "answer\n[LOG: maybe]", "　answer [LOG: ok] ", "a\n[LOG: ok]\n\n",
        "<think>r</think>visible\n[LOG: skip]", "x" * 50 + "\n[LOG: ok]",
        "<İhink>dotted</İhink>after", "<thınK>dotless kelvin</thinK>after",
        "<think>a</thİnk>after", "<thınk>x</thınk>y", "<ſhink>s</ſhink>z", "<think>x</İHINK>y",
        "\x1c<think>a</think>b", "\x85<think>a</think>b", "​<think>a</think>b",
        "café — 漢字 🌿 <think>x</think>", "<think>" + "r" * 300 + "</think>" + "é" * 20,
    ]
    pieces = ["<think>", "</think>", "<thinking>", "</thinking>", "<THINK>", "answer ", "é", "\n", "  ",
              "[LOG: ok]", "[LOG: skip]", "\n[LOG: ok]", "text", "🌿", "\t", "<", "think", ">"]
    for _ in range(160):
        base.append("".join(rng.choice(pieces) for _ in range(rng.randrange(0, 9))))
    return base


def record_text(llm, rng):
    out = []
    for text in text_cases(rng):
        row = {"text": text,
               "strip_final": llm.strip_reasoning(text, True),
               "strip_partial": llm.strip_reasoning(text, False),
               "log_marker": list(llm.LLMClient.strip_log_marker(text)),
               "shortened": llm.with_shortened_note(text),
               "cap": {}}
        for cap in (None, 0, 1, 2, 5, 20):
            got, cut = llm.cap_visible(text, cap)
            row["cap"][str(cap)] = [got, cut]
        out.append(row)
    return out


def record_case_folding():
    """Which characters Python's re.IGNORECASE lets each letter of the reasoning tags match, and
    which pairs of them a backreference treats as equal."""
    letters = "thinkg"
    classes = {}
    for letter in letters:
        pat = re.compile(re.escape(letter), re.IGNORECASE)
        classes[letter] = [cp for cp in range(0x110000) if not 0xD800 <= cp <= 0xDFFF and pat.fullmatch(chr(cp))]
    members = sorted({cp for v in classes.values() for cp in v})
    backref = re.compile(r"(.)\1", re.IGNORECASE | re.DOTALL)
    pairs = [[a, b] for a in members for b in members if backref.fullmatch(chr(a) + chr(b))]
    # \s under re (str pattern) for the leading/trailing whitespace of the tags.
    space = re.compile(r"\s")
    spaces = [cp for cp in range(0x110000) if not 0xD800 <= cp <= 0xDFFF and space.fullmatch(chr(cp))]
    return {"classes": classes, "backref_pairs": pairs, "spaces": spaces}


# ---------------- served models ----------------

def record_served(llm):
    bodies = [
        {"data": [{"id": "qa-chat"}, {"id": "qa-embed", "status": {"value": "loaded", "args": ["--embedding"]}}]},
        {"data": [{"id": "qa-chat", "status": {"value": "unloaded", "args": ["-m", "x"]}},
                  {"id": "qa-alt", "status": {"value": "loaded"}}, {"id": ""}, {"id": 5}, "junk",
                  {"id": "qa-rerank", "status": {"args": ["--reranking", 3]}}]},
        {"data": []}, {"data": "nope"}, {"models": []}, [], {"data": [{"id": "qa-x", "status": "loaded"}]},
        {"data": [{"id": "qa-y", "status": {"value": "loaded", "args": "--embedding"}}]},
    ]
    entries = []
    for body in bodies:
        try:
            entries.append({"body": body, "entries": llm._served_entries(body)})
        except Exception as exc:  # noqa: BLE001
            entries.append({"body": body, "error": type(exc).__name__})
    choices = []
    listing = [{"id": "qa-chat", "loaded": True, "chat": True}, {"id": "qa-fallback", "loaded": False, "chat": True},
               {"id": "qa-embed", "loaded": True, "chat": False}]
    for chat_model, fallback, partner in [("qa-chat", "", None), ("qa-missing", "qa-fallback", None),
                                          ("qa-missing", "", "qa-chat"), ("qa-missing", "qa-gone", None),
                                          ("qa-embed", "", None), ("qa-missing", "", "qa-embed"), ("", "", None)]:
        for lst in (listing, [listing[2]]):
            client = llm.LLMClient("http://qa-engine.invalid/v1", chat_model=chat_model, fallback_model=fallback,
                                   setting_key="LLM_AUX_MODEL", fallback_setting_key="LLM_AUX_FALLBACK_MODEL")
            if partner is not None:
                p = llm.LLMClient("http://qa-engine.invalid/v1", chat_model=partner)
                client._prefer_from = p
            try:
                choices.append({"chat_model": chat_model, "fallback": fallback, "partner": partner, "entries": lst,
                                "choice": list(client._choose(lst))})
            except llm.ModelNotServed as exc:
                choices.append({"chat_model": chat_model, "fallback": fallback, "partner": partner, "entries": lst,
                                "error": "ModelNotServed", "setting": exc.setting, "message": str(exc)})
            except llm.LLMError as exc:
                choices.append({"chat_model": chat_model, "fallback": fallback, "partner": partner, "entries": lst,
                                "error": "LLMError", "message": str(exc)})
            client.close()
    return {"entries": entries, "choices": choices}


def record_too_large(llm):
    import httpx
    out = []
    for status, body in [(400, "input (812 tokens) is too large to process. increase the physical batch size (current batch size: 512)"),
                         (500, '{"error":{"code":400,"message":"exceeds the available context size","n_prompt_tokens": 9000,"n_ctx":8192}}'),
                         (400, "input is larger than the max context size"), (413, "EXCEED_CONTEXT_SIZE"),
                         (200, "too large to process"), (400, "other failure"), (503, "x" * 5000 + "too large to process"),
                         (400, "too large to process" + "y" * 400), (400, "input (12 tokens) too large to process")]:
        resp = httpx.Response(status, text=body)
        got = llm.too_large_from_response(resp)
        out.append({"status": status, "body": body,
                    "result": None if got is None else {"message": str(got), "tokens": got.tokens, "limit": got.limit}})
    return out


# ---------------- streamed and plain completions ----------------

def sse(rng):
    """A synthetic provider answer as (content-type, body text)."""
    if rng.random() < 0.12:
        content = rng.choice(["plain JSON answer", "<think>r</think>json answer\n[LOG: ok]", "", None, 5,
                              "x" * 300 + "\n[LOG: skip]"])
        message = {"content": content}
        if rng.random() < 0.5:
            message[rng.choice(["reasoning_content", "reasoning"])] = rng.choice(["thoughts", "", 7])
        body = {"choices": [{"message": message, "finish_reason": rng.choice(["stop", "length", None])}],
                "model": "qa-served"}
        if rng.random() < 0.3:
            body["timings"] = {"draft_n": 10, "draft_n_accepted": 7}
        return rng.choice(["application/json", "application/json; charset=utf-8", "text/plain"]), json.dumps(body)
    words = ["Hello", " there", ",", " é", " 漢字", " 🌿", "\n", " answer", " [LOG:", " ok]", "\n[LOG: skip]",
             "<think>", "reasoning", "</think>", "<thinking>", "</thinking>", " more", " text" * 5, "  ",
             " Synthetic QA sentence about tides and kites.", " Another synthetic line, with é and 🌿 inside it."]
    lines = []
    if rng.random() < 0.2:
        lines.append(": keep-alive")
    n = rng.randrange(0, 70)
    for i in range(n):
        delta = {}
        r = rng.random()
        if r < 0.15:
            delta[rng.choice(["reasoning_content", "reasoning"])] = rng.choice(["think ", "", None, 3])
        if r > 0.1:
            delta["content"] = rng.choice(words + [None, ""])
        chunk = {"choices": [{"delta": delta, "index": 0}]}
        if rng.random() < 0.05:
            chunk = {"choices": []}
        if rng.random() < 0.03:
            chunk = {"choices": [{"index": 0}]}
        if rng.random() < 0.08:
            chunk["timings"] = {"draft_n": i, "draft_n_accepted": i // 2}
            if rng.random() < 0.5:
                chunk["model"] = "qa-served-model"
        if rng.random() < 0.06:
            lines.append(rng.choice(["event: message", "id: 3", "", "data:", "data:   "]))
        lines.append("data: " + json.dumps(chunk, ensure_ascii=rng.random() < 0.5))
    end = rng.random()
    if end < 0.55:
        lines.append("data: " + json.dumps({"choices": [{"delta": {}, "finish_reason": rng.choice(["stop", "length"])}]}))
        if rng.random() < 0.7:
            lines.append("data: [DONE]")
    elif end < 0.7:
        lines.append("data: [DONE]")
    elif end < 0.78:
        lines.append("data: " + json.dumps({"error": {"message": "synthetic provider failure"}}))
    elif end < 0.84:
        lines.append("data: {not json")
    elif end < 0.88:
        lines.append("data: [1, 2]")
    sep = rng.choice(["\n", "\n\n", "\r\n", "\r\n\r\n", "\r"])
    body = sep.join(lines) + (sep if rng.random() < 0.8 else "")
    return rng.choice(["text/event-stream", "text/event-stream; charset=utf-8"]), body


class Alive:
    def __init__(self):
        self.calls = 0
        self.abandoned = False

    def __call__(self):
        self.calls += 1

    def on_abandon(self, fn):
        return lambda: None


def run_completion(llm, httpx, case):
    sent = []
    responses = list(case["responses"])

    def handler(request):
        sent.append(json.loads(request.content))
        status, ctype, body = responses.pop(0) if responses else (500, "text/plain", "no more synthetic answers")
        return httpx.Response(status, headers={"content-type": ctype}, content=body.encode("utf-8"))

    client = llm.LLMClient("http://qa-engine.invalid/v1", chat_model="qa-chat", max_retries=case["retries"],
                           retry_backoff_s=0.5)
    sleeps = []
    real_sleep = llm.time.sleep
    llm.time.sleep = sleeps.append  # the retry waits are recorded, not slept
    client._client = httpx.Client(base_url="http://qa-engine.invalid/v1", transport=httpx.MockTransport(handler))
    if case.get("reasoning_tokens") is not None:
        client.reasoning_tokens = case["reasoning_tokens"]
    events = []
    alive = Alive()
    mode = case["mode"]
    try:
        if mode == "stream":
            reply = client.chat_stream([{"role": "user", "content": "QA question"}], events.append,
                                       temperature=0.7, alive=alive, max_tokens=case["max_tokens"])
        elif mode == "cancellable":
            reply = client.chat([{"role": "user", "content": "QA question"}], temperature=0.2,
                                max_tokens=case["max_tokens"], stop=case.get("stop"), alive=alive)
        else:
            reply = client.chat([{"role": "user", "content": "QA question"}], temperature=0.2,
                                max_tokens=case["max_tokens"], stop=case.get("stop"))
        result = {"content": str(reply), "reasoning": reply.reasoning, "truncated": reply.truncated}
    except Exception as exc:  # noqa: BLE001
        result = {"error": type(exc).__name__, "message": str(exc)[:300]}
    llm.time.sleep = real_sleep
    client.close()
    return {"sent": sent, "events": events, "result": result, "alive_calls": alive.calls, "sleeps": sleeps}


def completion_cases(rng):
    cases = []
    for i in range(260):
        mode = rng.choice(["stream", "stream", "cancellable", "plain"])
        responses = []
        if rng.random() < 0.12:
            responses.append((rng.choice([500, 503, 404, 429]), "text/plain", "synthetic upstream failure"))
        ctype, body = sse(rng)
        if mode == "plain" and "event-stream" in ctype:
            ctype, body = "application/json", json.dumps(
                {"choices": [{"message": {"content": rng.choice(["plain", "<think>x</think>y\n[LOG: ok]", "z" * 200])},
                              "finish_reason": rng.choice(["stop", "length"])}]})
        responses.append((200, ctype, body))
        cases.append({"name": f"completion_{i:03d}", "mode": mode, "responses": responses,
                      "max_tokens": rng.choice([None, None, 1, 5, 12, 40]),
                      "retries": rng.choice([1, 2, 3]),
                      "reasoning_tokens": rng.choice([None, 0, 16]),
                      "stop": rng.choice([None, ["\n\n"]])})
    return cases


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    if sys.version_info[:2] != (3, 12):
        sys.exit("record with CPython 3.12, the Diary image's interpreter")
    llm = load(sys.argv[1])
    import httpx
    rng = random.Random(1505)
    cases = completion_cases(rng)
    for case in cases:
        case["expected"] = run_completion(llm, httpx, case)
    data = {
        "source": "noevia-services diary/agent at origin/main 6d0ba0e",
        "python": sys.version.split()[0],
        "httpx": httpx.__version__,
        "pinned_sha256": PINNED,
        "text": record_text(llm, rng),
        "case_folding": record_case_folding(),
        "served": record_served(llm),
        "too_large": record_too_large(llm),
        "completions": cases,
    }
    OUT.write_text(json.dumps(data, ensure_ascii=False, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({len(data['text'])} texts, {len(cases)} completions)")


if __name__ == "__main__":
    main()
