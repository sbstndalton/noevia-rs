"""Record assembler-fixtures.json from the frozen Python Diary context assembler.

Usage (CPython 3.12, the Diary image's interpreter; the Diary requirements installed):

    <venv>/bin/python -I crates/diary-core/tests/generate_assembler_fixtures.py <noevia-services>/diary

Refuses any source whose sha256 is not pinned below (noevia-services origin/main 6d0ba0e). Every
name, date and text is synthetic: names come from a seeded syllable generator, as in the Diary's own
near-name tests. A stub store and a stub retriever drive the real `ContextAssembler.build`; each case
records the messages it built, the retrieval trace, and every store and search call. The Rust test
replays each case's inputs with the same stubs.
"""

import hashlib
import json
import pathlib
import random
import sys
from datetime import date

sys.dont_write_bytecode = True

PINNED = {
    "agent/context.py": "b972d8d2af8ed51cf46068d5affe002d6992ac36be0791e942aa9fd9473e428f",
    "agent/entity_focus.py": "cde7d7863748ff00a60065f0bb3f5d3978c9002da2f0e6229a8a28512d3b6e2f",
    "agent/follow_up.py": "1ebc1f44eb7c2a987352a2f3ee15fd0754d7bdf1158dabb1a1709acc44971351",
    "agent/score_gap.py": "c74d0dfdcf5489a62ce6e0ecdc70c9306d158c1369cfdb9016e5baa8b2c5e52d",
    "agent/retrieval_trace.py": "ec4172f5135e70754aceab4a218e57e5730509dfd741851e53870a06bd1394d4",
    "agent/util.py": "f1cfedf68937724331989b9083e2fff661b0c8b09db86659f32e41803ea4dfe9",
    "agent/temporal_native.py": "5fa362e7ab2e7ee15fe9532a3ce6e1960dd8e4c402d5a0d1f59c1b9e9b2672fa",
}
OUT = pathlib.Path(__file__).with_name("assembler-fixtures.json")
SYSTEM_PROMPT = "QA SYSTEM PROMPT: synthetic rules for the fixture."


def load(diary_dir):
    root = pathlib.Path(diary_dir).resolve()
    for rel, want in PINNED.items():
        got = hashlib.sha256((root / rel).read_bytes()).hexdigest()
        if got != want:
            sys.exit(f"refusing {rel}: sha256 {got} is not the pinned {want}")
    sys.path.insert(0, str(root))
    from agent import context, workspace_files  # noqa: E402
    from agent.config import Config  # noqa: E402
    from agent.retrieval_trace import RetrievalTrace  # noqa: E402
    # memory_text walks workspace files; the stub store supplies its text directly.
    workspace_files.memory_text = lambda store: store.memory
    return context, Config, RetrievalTrace


class Store:
    def __init__(self, spec):
        self.days = spec.get("days", {})
        self.raise_days = set(spec.get("raise_days", []))
        self.ignore_max = spec.get("ignore_max", False)
        self.standing = spec.get("standing", "")
        self.memory = spec.get("memory", "")
        self.calls = []

    def get_day_text(self, day, max_chars=None):
        self.calls.append(["day", day.isoformat(), max_chars])
        if day.isoformat() in self.raise_days:
            raise OSError("synthetic read failure")
        text = self.days.get(day.isoformat(), "")
        if not self.ignore_max and max_chars is not None:
            text = text[:max_chars]
        return text

    def get_standing_sections_text(self, max_chars=None):
        self.calls.append(["standing", None, max_chars])
        text = self.standing
        if not self.ignore_max and max_chars is not None:
            text = text[:max_chars]
        return text


class Retriever:
    def __init__(self, spec):
        self.rows = spec["rows"]
        self.status = spec.get("status")
        self.fail = spec.get("raise", False)
        self.calls = []

    def search(self, query, top_k=8, min_score=0.30, with_ids=False, diag=None):
        self.calls.append([query, top_k, min_score, with_ids])
        if self.fail:
            raise RuntimeError("synthetic search failure")
        if self.status is not None and diag is not None:
            diag["status"] = self.status
        hits = [dict(r) for r in self.rows if r["score"] >= min_score][:top_k]
        if not with_ids:
            for h in hits:
                h.pop("id", None)
        return hits


# ---------------- synthetic names ----------------

_ONSETS = ["b", "d", "f", "g", "k", "l", "m", "n", "p", "r", "s", "t", "v", "br", "dr", "gr", "kl", "st", "tr"]
_VOWELS = ["a", "e", "i", "o", "u"]
_CODAS = ["", "n", "r", "l", "s", "m", "th", "ck"]


def word(rng, syllables):
    return "".join(rng.choice(_ONSETS) + rng.choice(_VOWELS) + rng.choice(_CODAS) for _ in range(syllables)).title()


def variant(rng, surname):
    for _ in range(200):
        i = rng.randrange(1, len(surname) - 1)
        pool = _VOWELS if surname[i] in _VOWELS else list("bdfgklmnprstv")
        new = surname[:i] + rng.choice([c for c in pool if c != surname[i]]) + surname[i + 1:]
        if new.lower() != surname.lower() + "s":
            return new
    return surname + "e"


def person(rng):
    while True:
        given, surname = word(rng, 2), word(rng, rng.choice([2, 3]))
        if len(given) >= 4 and len(surname) >= 6:
            return given, surname, variant(rng, surname)


def chunk(day_iso, header, body):
    return f"[{day_iso}] ## Day\n### {header}\n**Me:** {body}"


TOPICS = ["kite colour", "favourite tea", "garden plan", "bicycle route", "map projection", "violin teacher"]
VALUES = ["teal", "amber", "cobalt", "saffron", "olive", "crimson", "lilac"]
PLACES = ["the quay", "the orchard", "the canal lock", "the north market", "the library steps"]


def random_rows(rng, people, n, day0=date(2031, 3, 1)):
    rows = []
    score = round(rng.uniform(0.75, 0.95), 4)
    for i in range(n):
        given, surname, near = rng.choice(people)
        kind = rng.random()
        if kind < 0.35:
            name = f"{given} {surname}"
        elif kind < 0.5:
            name = f"{given} {near}"
        elif kind < 0.6:
            name = given
        elif kind < 0.7:
            name = surname
        else:
            name = None
        d = day0.toordinal() + rng.randrange(0, 40)
        day_iso = date.fromordinal(d).isoformat()
        topic, value, place = rng.choice(TOPICS), rng.choice(VALUES), rng.choice(PLACES)
        hh, mm = rng.randrange(6, 23), rng.choice([0, 5, 15, 30, 45])
        header = rng.choice([f"{hh:02d}:{mm:02d} — Coffee", f"{hh}:{mm:02d}", f"{hh:02d}:{mm:02d} — {topic.title()}",
                             "Evening note"])
        if name:
            body = f"Met {name} at {place}; we talked about the {topic}, which is {value}."
        else:
            body = f"Walked to {place} alone and thought about the {topic}; it is {value} now."
        if rng.random() < 0.08:
            body += " Correction: I misheard earlier, it is not " + rng.choice(VALUES) + "."
        if rng.random() < 0.1:
            body += " " + "Long synthetic filler about weather and tides. " * rng.randrange(20, 80)
        if rng.random() < 0.08:
            body += " Ünïcödé café naïve — 漢字 🌿 notes."
        rows.append({"id": 100 + i, "day": day_iso, "header": header, "text": chunk(day_iso, header, body),
                     "score": score})
        score = round(score - rng.uniform(0.001, 0.06), 4)
    return rows


def random_case(rng, i):
    people = [person(rng) for _ in range(3)]
    g, s, near = people[0]
    asked = rng.choice([f"{g} {s}", f"{g} {near}", g, s, f"Dr {s}", None])
    topic = rng.choice(TOPICS)
    templates = [
        "What {t} does {n} prefer?",
        "What did {n} say about the {t}?",
        "Tell me about the {t} as of March 20, 2031, and {n}.",
        "Before 2031-03-15, what was {n}'s {t}?",
        "Who mentioned the {t}?",
        "What happened on 2031-03-04 and 2031-03-07?",
        "How have I been sleeping?",
        "And what about {n}?",
        "Is that written in my diary?",
    ]
    tpl = rng.choice(templates)
    name = asked or f"{people[1][0]} {people[1][1]}"
    message = tpl.format(t=topic, n=name)
    turns = []
    if rng.random() < 0.5:
        prev = f"What {topic} does {people[1][0]} {people[1][1]} prefer?"
        turns = [{"role": "user", "content": prev}, {"role": "assistant", "content": f"It is {rng.choice(VALUES)}."}]
        if rng.random() < 0.3:
            turns += [{"role": "user", "content": "unanswered synthetic question"}]
    cfg = {"retrieval": {}, "context": {}}
    if rng.random() < 0.3:
        cfg["retrieval"]["score_gap"] = rng.choice([0.08, "0.05", "off", 0.2])
    if rng.random() < 0.3:
        cfg["retrieval"]["max_context_tokens"] = rng.choice([300, 800, "1500", 5200])
    if rng.random() < 0.2:
        cfg["retrieval"]["top_k"] = rng.choice([2, 4, "3"])
    if rng.random() < 0.2:
        cfg["retrieval"]["entity_focus_margin"] = rng.choice([0.02, "0.1", "nan", 2])
    if rng.random() < 0.1:
        cfg["retrieval"]["entity_focus"] = rng.choice(["false", True, "off"])
    if rng.random() < 0.2:
        cfg["context"]["direct_past_days"] = rng.choice([0, 1, 7, 12])
    if rng.random() < 0.2:
        cfg["context"]["max_direct_past_tokens"] = rng.choice([50, 300, 3000])
    n_rows = rng.choice([0, 1, 3, 8, 16, 30])
    rows = random_rows(rng, people, n_rows)
    today = "2031-03-21"
    days = {}
    for back in range(0, 9):
        d = date.fromordinal(date(2031, 3, 21).toordinal() - back).isoformat()
        if rng.random() < 0.6:
            days[d] = f"### 09:00 — Note\n**Me:** synthetic day {d} about {rng.choice(TOPICS)} with {rng.choice(people)[0]}."
    for d in ("2031-03-04", "2031-03-07"):
        if rng.random() < 0.5:
            days[d] = f"### 10:00 — Dated\n**Me:** synthetic dated entry {d}."
    store = {"days": days, "standing": rng.choice(["", "## Open questions\n- synthetic open question"]),
             "memory": rng.choice(["", "QA memory: prefers short answers."]), "ignore_max": rng.random() < 0.2}
    if rng.random() < 0.15:
        store["raise_days"] = [rng.choice(sorted(days) or [today])]
    retriever = None if rng.random() < 0.05 else {"rows": rows, "raise": rng.random() < 0.05,
                                                  "status": rng.choice([None, None, None, "embed_failed"])}
    return {"name": f"random_{i:03d}", "day": today, "message": message, "session_turns": turns,
            "today_header": rng.choice([None, "Friday, March 21, 2031"]), "cfg": cfg, "store": store,
            "retriever": retriever, "trace": rng.random() < 0.7}


def fixed_cases():
    rng = random.Random(9)
    g, s, near = person(rng)
    o_g, o_s, _ = person(rng)
    exact = chunk("2031-03-02", "10:00 — Coffee", f"Had coffee with {g} {s} by the quay; kite colour is teal.")
    near_row = chunk("2031-03-03", "11:00 — Coffee", f"Had coffee with {g} {near}; kite colour is amber.")
    other = chunk("2031-03-04", "12:00 — Lunch", f"Lunch with {o_g} {o_s}, who flies a crimson kite.")
    rows = [
        {"id": 1, "day": "2031-03-02", "header": "10:00 — Coffee", "text": exact, "score": 0.91},
        {"id": 2, "day": "2031-03-03", "header": "11:00 — Coffee", "text": near_row, "score": 0.9},
        {"id": 3, "day": "2031-03-04", "header": "12:00 — Lunch", "text": other, "score": 0.7},
        {"id": 4, "day": "2031-03-25", "header": "09:00", "text": chunk("2031-03-25", "09:00", "Later teal note."),
         "score": 0.65},
    ]
    long_text = chunk("2031-03-05", "08:00 — Long", "é" * 3000)
    base_store = {"days": {"2031-03-20": "### 09:00 — Today\n**Me:** synthetic today."}, "standing": "", "memory": ""}
    cases = [
        {"name": "plain_no_entity", "message": "How have I been sleeping?", "rows": rows},
        {"name": "exact_and_near", "message": f"What kite colour does {g} {s} fly?", "rows": rows},
        {"name": "near_only_absent", "message": f"What kite colour does {g} {near}e fly?", "rows": rows[1:2]},
        {"name": "time_anchor", "message": "What was the kite colour as of March 10, 2031?", "rows": rows},
        {"name": "follow_up_other_name", "message": f"And what about {g} {near}?",
         "turns": [{"role": "user", "content": f"What kite colour does {g} {s} fly?"},
                   {"role": "assistant", "content": "Teal."}],
         "days": {"2031-03-20": f"### 09:00\n**Me:** What kite colour does {g} {s} fly?"}, "rows": rows},
        {"name": "long_excerpt_capped", "message": "Tell me about the long note.",
         "rows": [{"id": 9, "day": "2031-03-05", "header": "08:00 — Long", "text": long_text, "score": 0.8}]},
        {"name": "budget_and_top_k", "message": "Anything about kites?", "cfg": {"retrieval": {"top_k": 2, "max_context_tokens": 200}},
         "rows": rows + [{"id": 10 + i, "day": "2031-03-06", "header": f"1{i}:00", "text": chunk("2031-03-06", f"1{i}:00", "kite " * (40 * i)),
                          "score": 0.6 - i / 100} for i in range(5)]},
        {"name": "no_retriever", "message": "How was 2031-03-18?", "retriever": None},
        {"name": "empty_query", "message": "   ", "rows": rows},
        {"name": "search_raises", "message": "How have I been?", "raise": True},
        {"name": "status_embed_failed", "message": "How have I been?", "rows": [], "status": "embed_failed"},
        {"name": "score_gap_applied", "message": "Anything about kites?", "cfg": {"retrieval": {"score_gap": 0.08}}, "rows": rows},
        {"name": "score_gap_skipped_for_entity", "message": f"What kite colour does {g} {s} fly?",
         "cfg": {"retrieval": {"score_gap": "0.05"}}, "rows": rows},
        {"name": "focus_disabled", "message": f"What kite colour does {g} {s} fly?", "cfg": {"retrieval": {"entity_focus": "false"}}, "rows": rows},
        {"name": "direct_dates_and_errors", "message": "What happened on 2031-03-04, 2031-03-19, 2031-02-30, 2031-03-04 and 2031-03-07?",
         "rows": [], "days": {"2031-03-04": "dated four " * 50, "2031-03-19": "nineteenth", "2031-03-07": "seventh",
                              "2031-03-18": "eighteenth " * 400}, "raise_days": ["2031-03-17"], "cfg": {"context": {"direct_past_days": 4}}},
        {"name": "direct_ignores_max", "message": "Recent days?", "rows": [], "ignore_max": True,
         "days": {"2031-03-19": "x" * 5000, "2031-03-18": "y" * 5000}, "cfg": {"context": {"max_direct_past_tokens": 900}}},
        {"name": "direct_zero_budget", "message": "Recent days?", "rows": [], "cfg": {"context": {"max_direct_past_tokens": 0}},
         "days": {"2031-03-19": "nineteenth"}},
        {"name": "today_truncated", "message": "Today?", "rows": [], "ignore_max": True,
         "days": {"2031-03-20": "t" * 20000}, "cfg": {"context": {"max_today_tokens": 1000}}},
        {"name": "word_char_boundaries", "message": "Dates x2031-03-19 é2031-03-18 2031-03-17é 2031-03-16_ (2031-03-15) ٣2031-03-14",
         "rows": [], "days": {d: f"entry {d}" for d in ("2031-03-19", "2031-03-18", "2031-03-17", "2031-03-16", "2031-03-15", "2031-03-14")},
         "cfg": {"context": {"direct_past_days": 0}}},
        {"name": "long_session", "message": "And then?",
         "turns": [t for i in range(25) for t in ({"role": "user", "content": f"question {i} " + "w" * (i * 300)},
                                                  {"role": "assistant", "content": f"answer {i}"})], "rows": rows},
        {"name": "string_config_values", "message": "Anything about kites?",
         "cfg": {"retrieval": {"top_k": "3", "min_score": "0.5", "max_context_tokens": "900.5"}, "context": {"max_today_tokens": "100"}},
         "rows": rows},
        {"name": "standing_and_memory", "message": "Anything?", "rows": rows, "standing": "## Timeline\n- synthetic event",
         "memory": "QA memory text", "local_reference": "QA local reference"},
        # D3: beyond the Rust entity-focus caps the Rust output must equal the focus-off twin.
        {"name": "long_session_focus_off", "message": "And then?", "cfg": {"retrieval": {"entity_focus": "false"}},
         "turns": [t for i in range(25) for t in ({"role": "user", "content": f"question {i} " + "w" * (i * 300)},
                                                  {"role": "assistant", "content": f"answer {i}"})], "rows": rows},
        {"name": "long_word_message", "message": f"What did {g} {s} say about " + "q" * 300 + "?", "rows": rows},
        {"name": "long_word_message_focus_off", "message": f"What did {g} {s} say about " + "q" * 300 + "?",
         "cfg": {"retrieval": {"entity_focus": "off"}}, "rows": rows},
        {"name": "many_words_message", "message": f"Did {g} {s} mention " + "tide " * 9000 + "?", "rows": rows},
        {"name": "many_words_message_focus_off", "message": f"Did {g} {s} mention " + "tide " * 9000 + "?",
         "cfg": {"retrieval": {"entity_focus": "no"}}, "rows": rows},
        {"name": "relational_question", "message": f"Who is {g} {s}'s sister?", "rows": rows},
        {"name": "two_entities", "message": f"Did {g} {s} meet {o_g} {o_s}?", "rows": rows},
    ]
    out = []
    for c in cases:
        store = dict(base_store)
        store["days"] = c.get("days", base_store["days"])
        for key in ("raise_days", "ignore_max", "standing", "memory"):
            if key in c:
                store[key] = c[key]
        retriever = {"rows": c.get("rows", []), "raise": c.get("raise", False), "status": c.get("status")}
        if "retriever" in c and c["retriever"] is None:
            retriever = None
        for trace in (True, False):
            out.append({"name": f"{c['name']}{'' if trace else '_untraced'}", "day": "2031-03-20", "message": c["message"],
                        "session_turns": c.get("turns", []), "today_header": None, "cfg": c.get("cfg", {}), "store": store,
                        "retriever": retriever, "trace": trace, "local_reference": c.get("local_reference")})
    return out


def run(case, modules):
    context, Config, RetrievalTrace = modules
    store = Store(case["store"])
    retriever = Retriever(case["retriever"]) if case["retriever"] is not None else None
    asm = context.ContextAssembler(store, retriever, Config(json.loads(json.dumps(case["cfg"]))))
    asm._system_prompt = lambda: SYSTEM_PROMPT
    if case.get("local_reference") is not None:
        asm.local_reference = case["local_reference"]
    trace = RetrievalTrace() if case["trace"] else None
    try:
        messages = asm.build(date.fromisoformat(case["day"]), case["message"], case["session_turns"],
                             today_header=case["today_header"], trace=trace)
        error = None
    except Exception as exc:  # noqa: BLE001 - recorded: the Rust must fail the same cases
        messages, error = None, type(exc).__name__
    if trace is not None and trace.failed:
        sys.exit(f"{case['name']}: the trace failed; fix the case")
    return {"messages": messages, "error": error, "trace": trace.data if trace is not None else None,
            "store_calls": store.calls, "search_calls": retriever.calls if retriever is not None else []}


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    if sys.version_info[:2] != (3, 12):
        sys.exit("record with CPython 3.12, the Diary image's interpreter")
    import os
    for key in ("DIARY_TEMPORAL_IMPL", "DIARY_SCORE_GAP_IMPL", "DIARY_ENTITY_FOCUS_IMPL"):
        os.environ.pop(key, None)
    modules = load(sys.argv[1])
    rng = random.Random(20311113)
    cases = fixed_cases() + [random_case(rng, i) for i in range(160)]
    for case in cases:
        case["expected"] = run(case, modules)
    data = {"source": "noevia-services diary/agent at origin/main 6d0ba0e", "python": sys.version.split()[0],
            "pinned_sha256": PINNED, "system_prompt": SYSTEM_PROMPT, "cases": cases,
            # D3: Rust runs these with the focus off (entity-focus caps); compare with the twin.
            "degraded_twins": {"long_session": "long_session_focus_off",
                               "long_session_untraced": "long_session_focus_off_untraced",
                               "long_word_message": "long_word_message_focus_off",
                               "long_word_message_untraced": "long_word_message_focus_off_untraced",
                               "many_words_message": "many_words_message_focus_off",
                               "many_words_message_untraced": "many_words_message_focus_off_untraced"}}
    OUT.write_text(json.dumps(data, ensure_ascii=False, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    errors = sum(1 for c in cases if c["expected"]["error"])
    print(f"wrote {OUT} ({len(cases)} cases, {errors} raising)")


if __name__ == "__main__":
    main()
