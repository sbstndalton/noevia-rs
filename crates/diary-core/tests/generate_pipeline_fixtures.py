"""Record pipeline-fixtures.json from the frozen Python log pipeline (diary/agent/pipeline.py).

Usage (CPython 3.12, the Diary requirements installed):

    <venv>/bin/python -I crates/diary-core/tests/generate_pipeline_fixtures.py <noevia-services>/diary

The real LoggingPipeline and the real config/prompts/logging.md templates (both sha256 pinned,
noevia-services origin/main 6d0ba0e) run against a scripted fake aux model and a fake store. Each
case records the outcome, the progress events, every aux prompt and every store call. Synthetic text.
"""

import hashlib
import json
import pathlib
import random
import sys
from datetime import date, datetime

sys.dont_write_bytecode = True

PINNED = {
    "agent/pipeline.py": "c86aba1d33fbff2b2812d8aa3fe2406ba0d4828dd17ade55377717dabd746b1c",
    "config/prompts/logging.md": "b28d5d7e03db5fbc9875a433c9013042ad88f7413c7d4e986a9a3d8467fcf661",
}
OUT = pathlib.Path(__file__).with_name("pipeline-fixtures.json")


def load(diary_dir):
    root = pathlib.Path(diary_dir).resolve()
    for rel, want in PINNED.items():
        got = hashlib.sha256((root / rel).read_bytes()).hexdigest()
        if got != want:
            sys.exit(f"refusing {rel}: sha256 {got} is not the pinned {want}")
    sys.path.insert(0, str(root))
    import yaml
    from agent import pipeline, corpus_write_native  # noqa: E402
    templates = yaml.safe_load((root / "config/prompts/logging.md").read_text(encoding="utf-8"))
    return pipeline, corpus_write_native, templates


class Aux:
    def __init__(self, script):
        self.script = list(script)
        self.calls = []

    def chat(self, messages, temperature, max_tokens, stop, alive):
        self.calls.append({"system": messages[0]["content"], "prompt": messages[1]["content"],
                           "temperature": temperature, "max_tokens": max_tokens, "stop": stop})
        reply = self.script.pop(0) if self.script else {"raise": True}
        if reply.get("raise"):
            raise RuntimeError("synthetic aux failure")
        return reply["text"]


class Store:
    def __init__(self, spec, native):
        self.spec = spec
        self.native = native
        self.index_enabled = spec.get("index_enabled", True)
        self.calls = []

    def log_exchange(self, day, sub_header, me_text, claude_text, now=None):
        self.calls.append(["log_exchange", day.isoformat(), sub_header, me_text, claude_text])
        mode = self.spec.get("log", "ok")
        if mode == "native":
            raise self.native.NativeWriteUnavailable("synthetic")
        if mode == "fail":
            raise OSError("synthetic write failure")
        return "qa-xid-0001"

    def get_standing_sections_text(self, max_chars=None):
        self.calls.append(["standing", max_chars])
        if self.spec.get("standing_raise"):
            raise OSError("synthetic")
        return self.spec.get("standing", "## Open questions\n- synthetic")

    def update_standing_sections(self, open_q, timeline, today):
        self.calls.append(["update", open_q, timeline, today])
        mode = self.spec.get("update", "changed")
        if mode == "raise":
            raise OSError("synthetic")
        return "qa-journal-1" if mode == "changed" else None


VERDICTS = ["LOG", "SKIP", "skip", "  SKIP.", "SKIP: meta", "I would not skip this", "", "\n\nSKIP\nLOG", "Skip!",
            "ß", "LOG\nSKIP", "\x1cSKIP", "SKIPPED", "s k i p", "ſKIP", "\t skip ,"]
SUMMARIES = ['"Quoted summary."', '  plain summary  ', '"', '""', '" padded "', 'no quote"', '"é \U0001f33f"', ""]
GATES = ["UPDATE", "update needed", " Update.", "NO", "", "üPDATE", "UPDATES", "upDATE now"]
EDITS = ['{"open_questions": [{"action": "add", "text": "QA question?"}], "timeline": []}',
         '```json\n{"timeline": [{"action": "add", "date": "2031-03-20", "text": "QA event"}]}\n```',
         'Here you go: {"open_questions": null} thanks', "no json", '{"broken": ', "[1, 2]", '```JSON{"timeline": [1]}```',
         '{"open_questions": [], "timeline": []}', "} {"]


def cases(rng):
    out = []
    for i in range(140):
        script = [{"raise": True} if rng.random() < 0.1 else {"text": rng.choice(VERDICTS)},
                  {"raise": True} if rng.random() < 0.1 else {"text": rng.choice(SUMMARIES)},
                  {"raise": True} if rng.random() < 0.1 else {"text": rng.choice(GATES)},
                  {"raise": True} if rng.random() < 0.1 else {"text": rng.choice(EDITS)}]
        store = {"index_enabled": rng.random() < 0.8, "log": rng.choice(["ok"] * 6 + ["fail", "native"]),
                 "standing_raise": rng.random() < 0.05, "update": rng.choice(["changed", "unchanged", "raise"])}
        out.append({"name": f"pipeline_{i:03d}", "script": script, "store": store,
                    "user": rng.choice(["QA question about kites {braces}?", "Tell me é \U0001f33f", "x"]),
                    "assistant": rng.choice(["  Synthetic answer.  ", "Answer with {braces} and {{doubles}}", ""]),
                    "topic": rng.choice(["", "Kites"]), "progress": rng.random() < 0.7})
    return out


def run(case, pipeline, native, templates):
    aux = Aux(case["script"])
    store = Store(case["store"], native)
    p = pipeline.LoggingPipeline(store, None, aux, templates)
    events = []
    kwargs = {"progress": events.append} if case["progress"] else {}
    try:
        o = p.log_exchange(case["user"], case["assistant"], topic=case["topic"], now=datetime(2031, 3, 20, 9, 30),
                           day=date(2031, 3, 20), **kwargs)
        result = {"decision": o.decision, "xid": o.xid, "reason": o.reason}
    except Exception as exc:  # noqa: BLE001
        result = {"error": type(exc).__name__}
    return {"result": result, "events": events, "aux_calls": aux.calls, "store_calls": store.calls}


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    if sys.version_info[:2] != (3, 12):
        sys.exit("record with CPython 3.12, the Diary image's interpreter")
    pipeline, native, templates = load(sys.argv[1])
    rng = random.Random(2031)
    cs = cases(rng)
    for c in cs:
        c["expected"] = run(c, pipeline, native, templates)
    helpers = {"json_extract": []}
    for text in EDITS + ["  ```json {\"a\": 1} ```  ", "```{x}```", "{only open", "}{", "```jsonX{\"b\": 2}"]:
        try:
            helpers["json_extract"].append([text, pipeline._json_extract(text)])
        except ValueError:
            helpers["json_extract"].append([text, None])
    data = {"source": "noevia-services diary/agent at origin/main 6d0ba0e", "python": sys.version.split()[0],
            "pinned_sha256": PINNED, "templates": templates, "hint": pipeline.THINKING_MODEL_HINT,
            "json_extract": helpers["json_extract"], "cases": cs}
    OUT.write_text(json.dumps(data, ensure_ascii=False, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({len(cs)} cases)")


if __name__ == "__main__":
    main()
