"""Regenerate retrieval-trace-fixtures.json from the frozen Python retrieval_trace module.

Usage (stdlib only, isolated mode so no site or cwd module can shadow the stdlib):

    python3 -I crates/diary-core/tests/generate_fixtures.py <path-to>/diary/agent/retrieval_trace.py

The script refuses any other source file: its sha256 must match PINNED_SHA256, which is the
diary/agent/retrieval_trace.py of noevia-services origin/main at the time of the R9 head start.
All rows are synthetic. Entry text, headers and ids here are invented for the fixtures.
"""

import hashlib
import importlib.util
import json
import pathlib
import sys

PINNED_SHA256 = "ec4172f5135e70754aceab4a218e57e5730509dfd741851e53870a06bd1394d4"
OUT = pathlib.Path(__file__).with_name("retrieval-trace-fixtures.json")


def load(path):
    data = pathlib.Path(path).read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    if digest != PINNED_SHA256:
        sys.exit(f"refusing {path}: sha256 {digest} is not the pinned {PINNED_SHA256}")
    spec = importlib.util.spec_from_file_location("retrieval_trace_frozen", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def row(id_, day, header, score, **extra):
    base = {"id": id_, "day": day, "header": header, "score": score, "text": "SECRET-ENTRY-TEXT", "embedding": [0.1]}
    base.update(extra)
    return base


CLOCK_HEADERS = [
    "09:30 Morning walk",
    "9:05",
    "12:00",
    "  7:45 leading spaces",
    "\t\t3:10 tab indent",
    "12:345 too many minutes",
    "123:45 too many hours",
    "12:3 short minutes",
    "1:23_ underscore boundary",
    "10:15é accented boundary",
    "10:15 ",
    "10:15",
    "10:15-note",
    "Morning 09:30",
    "",
    "no time here",
    ":30 missing hour",
    "09:",
    "x",
    "١٠:١٥ arabic digits",
]

# Python \d matches these Unicode decimal digits, the Rust port matches ASCII only (see spec).
KNOWN_DEVIATION_PREFIX = "١"


def clock_cases(module):
    cases = []
    for header in CLOCK_HEADERS:
        expected = module.RetrievalTrace._ref({"id": 1, "day": "2026-08-01", "header": header})
        cases.append({
            "header": header,
            "expected": expected,
            "known_deviation": header.startswith(KNOWN_DEVIATION_PREFIX),
        })
    # Non-string headers: str(value) in Python never matches the time pattern.
    for header in [None, 0, 12, 12.5, True, ["09:30"], {"t": "09:30"}]:
        expected = module.RetrievalTrace._ref({"id": 2, "day": "2026-08-02", "header": header})
        cases.append({"raw_header": header, "expected": expected, "known_deviation": False})
    # Missing fields become null.
    cases.append({"raw_row": {"header": "08:00 x"}, "expected": module.RetrievalTrace._ref({"header": "08:00 x"}),
                  "known_deviation": False})
    return cases


def run_ops(module, ops):
    trace = module.RetrievalTrace()
    items = []
    for op in ops:
        kind = op["op"]
        if kind == "status":
            trace.set_status(op["value"])
        elif kind == "params":
            trace.set_params(**op["value"])
        elif kind == "budget":
            trace.set_budget(**op["value"])
        elif kind == "candidate":
            item = trace.add_candidate(op["rank"], op["row"], op.get("vector_rank"))
            items.append(item)
            item["loaded"] = op["loaded"]
            item["dropped"] = op["dropped"]
        elif kind == "score_gap":
            removed = [(rank, r) for rank, r in op["removed"]]
            trace.set_score_gap(op["gap"], op["applied"], op["best"], removed,
                                diagnostics=op.get("diagnostics"))
        elif kind == "loaded":
            trace.add_loaded(op["row"], op["tokens"], op["chars"])
        elif kind == "finish":
            trace.finish(op["used_chars"])
        elif kind == "direct_fallback":
            trace.set_direct_fallback(op["found"], op["tokens"])
        elif kind == "fail":
            trace.failed = True
        else:
            raise SystemExit(f"unknown op {kind}")
    result = module.attach({"answer": "synthetic"}, trace)
    return {"data": trace.data, "attached": result}


def scenarios():
    r1 = row(101, "2026-08-01", "09:30 Morning walk", 0.91)
    r2 = row(102, "2026-08-02", "14:05 Coffee", 0.88)
    r3 = row(103, "2026-08-03", "no time", 0.70)
    r4 = row(104, "2026-08-04", "23:59 late", 0.65)
    gap_removed = row(105, "2026-08-05", "08:15 far", 0.20)
    return [
        {"name": "empty_trace", "ops": []},
        {"name": "status_only_no_retriever", "ops": [{"op": "status", "value": "no_retriever"}]},
        {"name": "full_ok_with_budget_and_top_k",
         "ops": [
             {"op": "status", "value": "ok"},
             {"op": "params", "value": {"top_k": 2, "candidate_k": 4, "min_score": 0.3}},
             {"op": "budget", "value": {"max_context_tokens": 5200}},
             {"op": "candidate", "rank": 0, "row": r1, "loaded": True, "dropped": None},
             {"op": "candidate", "rank": 1, "row": r2, "loaded": True, "dropped": None},
             {"op": "candidate", "rank": 2, "row": r3, "loaded": False, "dropped": "budget"},
             {"op": "candidate", "rank": 3, "row": r4, "loaded": False, "dropped": "top_k"},
             {"op": "loaded", "row": r1, "tokens": 12, "chars": 40},
             {"op": "loaded", "row": r2, "tokens": 9, "chars": 31},
             {"op": "finish", "used_chars": 71},
         ]},
        {"name": "vector_rank_defaults_to_rank_and_explicit_value_kept",
         "ops": [
             {"op": "candidate", "rank": 5, "row": r1, "vector_rank": None, "loaded": False, "dropped": None},
             {"op": "candidate", "rank": 1, "row": r2, "vector_rank": 7, "loaded": False, "dropped": None},
         ]},
        {"name": "score_gap_with_diagnostics",
         "ops": [
             {"op": "status", "value": "ok"},
             {"op": "score_gap", "gap": 0.25, "applied": True, "best": 0.91,
              "removed": [[4, gap_removed]], "diagnostics": {"kept": 3, "note": "n"}},
             {"op": "score_gap", "gap": None, "applied": False, "best": None, "removed": []},
         ]},
        {"name": "direct_fallback_found_and_empty",
         "ops": [{"op": "direct_fallback", "found": True, "tokens": 33}]},
        {"name": "direct_fallback_empty", "ops": [{"op": "direct_fallback", "found": False, "tokens": 0}]},
        {"name": "failed_trace_is_not_attached",
         "ops": [{"op": "status", "value": "ok"}, {"op": "fail"}]},
        {"name": "hostile_row_with_no_fields", "ops": [
            {"op": "candidate", "rank": 0, "row": {}, "loaded": False, "dropped": None},
            {"op": "candidate", "rank": 1, "row": {"header": 12, "score": None}, "loaded": True, "dropped": None},
            {"op": "loaded", "row": {}, "tokens": 0, "chars": 0},
            {"op": "finish", "used_chars": 0},
        ]},
    ]


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    module = load(sys.argv[1])
    out = {
        "source_sha256": PINNED_SHA256,
        "schema_version": module.SCHEMA_VERSION,
        "clock": clock_cases(module),
        "traces": [{"name": s["name"], "ops": s["ops"], **run_ops(module, s["ops"])} for s in scenarios()],
    }
    OUT.write_text(json.dumps(out, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({len(out['clock'])} clock cases, {len(out['traces'])} traces)")


if __name__ == "__main__":
    main()
