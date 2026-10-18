#!/usr/bin/env python3
"""Generate crates/backend-telemetry/tests/fixtures/telemetry-python.v1.json (noevia#1318 step 3c).

Runs the ACTUAL noevia-services model-manager `telemetry.py` pure functions (`parse_log`, with
and without the stored-instance attribution, `log_span`, `_parse_ts`, `argv_to_dict`,
`differing_keys`, `_pct`, `instance_key`) and `stats_for` and `config_history` over rows handed
in through a stand-in `db` (the store is step 3d). Every log line, alias, path and figure is
synthetic; no container, model or store is touched.

    python3 -I tools/gen-telemetry-oracle.py <services>/model-manager/app > fixture.json
"""
from __future__ import annotations

import hashlib
import json
import os
import sys
from pathlib import Path


def r(x):
    """A float as Python's repr (None stays None); everything else as is."""
    if isinstance(x, float):
        return repr(x)
    if isinstance(x, dict):
        return [[k, r(v)] for k, v in x.items()]
    if isinstance(x, list):
        return [r(v) for v in x]
    return x


def sample(s):
    return {k: r(getattr(s, k)) for k in ("ts", "instance", "task", "model_path", "alias", "spec_type",
                                          "prompt_tokens", "prompt_tps", "gen_tokens", "gen_tps",
                                          "draft_acc", "draft_len")}


def config(c):
    return {"instance": c.instance, "alias": c.alias, "model_path": c.model_path, "argv": r(c.argv),
            "first_seen": r(c.first_seen)}


T = "2026-10-18T10:{:02d}:{:02d}.{}Z "


def ts(m, s, frac="123456789"):
    return T.format(m, s, frac)


def spawn(m, s, alias, port, argv, frac="000000001"):
    out = [ts(m, s, frac) + f"srv    load: spawning server instance with name={alias} on port {port}",
           ts(m, s, frac) + "srv    load: spawning server instance with args:"]
    out += [ts(m, s, frac) + f"srv    load:   {a}" for a in argv]
    return out


def load(m, s, port, path):
    return ts(m, s) + f"[{port}] 0.05.690 I srv    load_model: loading model '{path}'"


def timing(m, s, port, task, prompt=(1239.27, 8297, 6695.10), gen=(777.37, 111, 141.50), draft=None):
    out = []
    if prompt:
        out.append(ts(m, s) + f"[{port}] 0.05.690 I slot print_timing: id  0 | task {task} | prompt eval time = "
                   f"{prompt[0]} ms / {prompt[1]} tokens (0.15 ms per token, {prompt[2]} tokens per second)")
    if gen:
        out.append(ts(m, s) + f"[{port}] 0.05.690 I slot print_timing: id  0 | task {task} |        eval time =  "
                   f"{gen[0]} ms /  {gen[1]} tokens (7.07 ms per token,  {gen[2]} tokens per second)")
    if draft:
        out.append(ts(m, s) + f"[{port}] 0.05.690 I slot print_timing: id  0 | task {task} | draft acceptance = "
                   f"{draft[0]} ( {draft[1]} accepted / {draft[2]} generated), mean len = {draft[3]}")
    return out


ARGV_A = ["--model", "/models/syn/a.gguf", "--alias", "syn-a", "--port", "18001", "--ctx-size", "8192",
          "--spec-type", "draft-mtp", "--spec-draft-n-max", "4", "--spec-draft-p-min", "0.25", "-ngl", "99",
          "--jinja"]
ARGV_B = ["-m", "/models/syn/b.gguf", "--ctx-size", "4096", "-np", "2", "--flash-attn"]


def logs():
    out = {}
    # A clean run: two instances, the second respawned on the same port, timings for each.
    lines = spawn(0, 1, "syn-a", 18001, ARGV_A) + [load(0, 2, 18001, "/models/syn/a.gguf")]
    lines += timing(0, 3, 18001, 1) + timing(0, 4, 18001, 2, draft=(0.62963, 51, 81, 2.13))
    lines += spawn(1, 0, "syn-b", 18002, ARGV_B) + [load(1, 1, 18002, "/models/syn/b.gguf")]
    lines += timing(1, 2, 18002, 1, prompt=(10.5, 12, 1142.8))
    lines += spawn(2, 0, "syn-a", 18001, ARGV_A[:6]) + timing(2, 1, 18001, 1, gen=(50.0, 8, 160.0))
    out["respawn"] = "\n".join(lines) + "\n"
    # Interleaved spawns, a load line before its spawn, lines without timestamps, a task whose
    # lines straddle the start (eval only), CRLF and other line breaks, a blank argv block.
    lines = [timing(0, 0, 18003, 9, prompt=None)[0], "no timestamp here",
             "[18004] 0.01.000 I srv    load_model: loading model '/models/syn/c.gguf'"]
    lines += spawn(0, 5, "syn-c", 18004, []) + spawn(0, 6, "syn-d", 18005, ARGV_B, frac="5")
    lines += timing(0, 7, 18004, 3) + timing(0, 8, 18005, 3)
    out["interleaved"] = "\r\n".join(lines[:4]) + "\x0b" + "\x1e".join(lines[4:]) + " "
    # Timing values on the edges: integer rates, leading dots, huge digit runs, a second id.
    lines = spawn(0, 1, "syn-e", 18006, ARGV_A) + [
        ts(0, 2) + "[18006] x slot print_timing: id 12 | task 4 | prompt eval time = 5 ms / 7 tokens "
        "(1 ms per token, 1400 tokens per second)",
        ts(0, 2) + "[18006] x slot print_timing: id 12 | task 4 |  eval time = .5 ms / 0016 tokens "
        "( .1 ms per token, 3200. tokens per second)",
        ts(0, 2) + "[18006] x slot print_timing: id 12 | task 5 | draft acceptance = 1 ( 0 accepted / 0 generated), "
        "mean len = 0",
        ts(0, 3) + "[18006] x slot print_timing: id 1 | task 6 |  eval time = 1 ms / 20 tokens "
        "(1 ms per token, " + "9" * 400 + " tokens per second)"]
    out["edges"] = "\n".join(lines)
    # Timestamps: offsets, a comma fraction, no fraction, out-of-range values (0.0), a short one.
    stamps = ["2026-10-18T10:00:00Z", "2026-10-18T10:00:00.5+02:00", "2026-10-18T10:00:00,25-01:30",
              "2026-10-18T10:00Z", "2026-02-30T10:00:00Z", "2026-10-18T24:00:00Z", "2024-02-29T23:59:59.9999999Z",
              "0000-01-01T00:00:00Z", "2026-10-18T10:00:60Z", "1969-12-31T23:59:59.5Z", "2026-13-01T00:00:00+0000"]
    out["stamps"] = "\n".join(s + f" [18007] x slot print_timing: id 0 | task {i} |  eval time = 1 ms / 20 tokens "
                              f"(1 ms per token, 10.5 tokens per second)" for i, s in enumerate(stamps))
    # Whitespace Python's \\s matches and the regex crate's would not (\\x1f), and Unicode spaces.
    out["whitespace"] = (ts(0, 1) + "srv\x1fload: spawning server instance with name=syn-f on port 18008\n"
                         + ts(0, 1) + "srv　load:  --model /models/syn/f.gguf\n"
                         + ts(0, 2) + "[18008] x slot print_timing: id  0 | task 1 | eval time = 1 ms /"
                         " 20 tokens (1 ms per token, 11.0 tokens per second)\n")
    return out


RAISING = {"bad-float": ts(0, 1) + "[18009] x slot print_timing: id 0 | task 1 |  eval time = 1 ms / 20 tokens "
                        "(1 ms per token, 1.2.3 tokens per second)"}

STORED = {
    "18010": [{"instance": "18010@1792317000", "alias": "syn-g", "model_path": "/models/syn/g.gguf",
               "argv": {"--model": "/models/syn/g.gguf", "--spec-type": "ngram-simple", "--draft-max": "8"},
               "first_seen": 1792317000.0},
              {"instance": "18010@1792316000", "alias": "syn-old", "model_path": "/models/syn/old.gguf",
               "argv": {}, "first_seen": 1792316000.0}],
    "18011": "raise",
    "18012": [{"instance": "18012@1792318000", "alias": "syn-h", "model_path": "/models/syn/h.gguf", "argv": {},
               "first_seen": 1792318000.0}],
}


def attributed():
    """(text, covered_until, continuous_since): requests whose spawn scrolled out of the tail."""
    base = "\n".join(timing(0, 0, 18010, 1) + timing(5, 0, 18011, 2) + timing(9, 0, 18012, 3)
                     + [load(10, 0, 18013, "/models/syn/x.gguf")] + timing(10, 1, 18013, 4)) + "\n"
    first = 1792317600.123456
    return {"gap-free": (base, first + 1, 1792316500.0), "gap": (base, first - 1, 0.0),
            "old-span": (base, first, 1792317500.0), "no-cover": (base, 0.0, 0.0),
            "model-veto": (load(0, 0, 18010, "/models/syn/other.gguf") + "\n" + base, first + 1, 0.0)}


ARGVS = [[], ["--model"], ["-m", "/m.gguf", "--port", "1", "-p", "2", "--host", "h", "-a", "x", "--alias", "y"],
         ["pos", "--a", "1", "--a", "2", "--b", "-c", "--d", "-", "--e", "--x=y", "--"], ["--z", "", "--y"]]
KEY_SETS = [[], [{"--a": "1"}], [{"--a": "1"}, {"--a": "1"}], [{"--a": "1"}, {"--a": "2", "--b": "x"}],
            [{"--b": "—"}, {}], [{"--z": "1", "-a": "1", "--é": "1"}, {"--z": "2"}, {"--z": "2", "Z": "1"}]]
PCTS = [([], 0.5), ([3.0], 0.5), ([1.0, 2.0], 0.5), ([1.0, 2.0, 3.0, 4.0], 0.5), ([4.0, 1.0, 3.0, 2.0], 0.25),
        ([5.0, 1.0, 2.0, 3.0, 4.0, 6.0], 0.75), ([1.0, 2.0, 3.0], 0.25), (list(map(float, range(10))), 0.5),
        ([0.1, 0.2, float("inf")], 0.75)]


def row(inst, ts_, gen, prompt=0.0, acc=None, dlen=None, spec=None):
    return {"instance": inst, "ts": ts_, "gen_tps": gen, "prompt_tps": prompt, "draft_acc": acc,
            "draft_len": dlen, "spec_type": spec}


ROWS = {
    "empty": [],
    "single": [row("1@1", 9.0, 100.0, 900.0, 0.5, 2.0, "draft-mtp (max=4)")] * 3,
    "current-wins": [row("2@2", 20.0 - i, 50.0 + i, 0.0 if i % 2 else 800.0) for i in range(6)]
                    + [row("1@1", 5.0, 10.0, 100.0, spec="ngram-simple")],
    "pooled": [row("3@3", 30.0, 40.0, spec=""), row("2@2", 20.0, 41.0, acc=0.25, spec="b"),
               row("1@1", 10.0, 0.0, 700.0, dlen=3.0, spec="a")],
}
HISTORY = {
    "two-configs": ([row("b", 30.0, 140.0), row("b", 29.0, 141.5), row("b", 28.0, 150.0),
                     row("a", 20.0, 99.0, 900.0), row("a", 19.0, 98.0), row("a", 18.0, 97.0, acc=0.5),
                     row("c", 15.0, 99.5), row("d", 10.0, 1.0)],
                    {"b": {"--model": "/m.gguf", "--ctx-size": "8192"}, "a": {"-m": "/m.gguf"},
                     "c": {"-m": "/m.gguf"}, "d": {"--ctx-size": "1"}}),
    "one-config": ([row("a", 2.0, 1.0)] * 4, {"a": {"-m": "/m.gguf"}}),
    "configs-raise": ([row("a", 2.0, 1.0)] * 4, "raise"),
    "zero-best": ([row("a", 3.0, 0.0)] * 3 + [row("b", 2.0, 0.0)] * 3,
                  {"a": {"-m": "/a"}, "b": {"-m": "/b", "--jinja": "true"}}),
    "ties": ([row("x", 5.0, 10.0)] * 3 + [row("y", 5.0, 20.0)] * 3 + [row("z", 6.0, 30.0)] * 3,
             {"x": {"-m": "/x"}, "y": {"-m": "/y"}, "z": {"--model": "", "-m": "/z", "-t": "4"}}),
}


def main() -> None:
    app_dir = Path(sys.argv[1]).resolve()
    sys.path.insert(0, str(app_dir.parent))
    os.environ.setdefault("TZ", "UTC")
    from app import db, telemetry

    out = {"sources": {"telemetry.py": hashlib.sha256((app_dir / "telemetry.py").read_bytes()).hexdigest()}}

    def parsed(text, *args):
        try:
            s, c = telemetry.parse_log(text, *args)
            return {"samples": [sample(x) for x in s], "configs": [config(x) for x in c]}
        except Exception as e:  # noqa: BLE001 - recorded: the native side reports Raised
            return {"raised": type(e).__name__}

    out["logs"] = {k: {"text": v, "span": r(list(telemetry.log_span(v))), **parsed(v)} for k, v in logs().items()}
    out["raising"] = {k: {"text": v, **parsed(v)} for k, v in RAISING.items()}

    def instances_on_port(backend, port):
        rows = STORED.get(port, [])
        if rows == "raise":
            raise RuntimeError("synthetic store failure")
        assert backend == "syn-backend"
        return [dict(x) for x in rows]

    db.server_instances_on_port = instances_on_port
    out["attributed"] = {k: {"text": t, "covered_until": repr(c), "continuous_since": repr(s),
                             **parsed(t, "syn-backend", c, s)} for k, (t, c, s) in attributed().items()}
    out["stored"] = {k: (v if v == "raise" else [dict(x, argv=list(x["argv"].items()), first_seen=repr(x["first_seen"]))
                                                  for x in v]) for k, v in STORED.items()}
    out["argv"] = [{"tokens": a, "dict": list(telemetry.argv_to_dict(a).items())} for a in ARGVS]
    out["differing"] = [{"configs": [list(c.items()) for c in cs], "keys": telemetry.differing_keys(cs)}
                        for cs in KEY_SETS]
    out["pct"] = [{"values": r(v), "q": repr(q), "out": repr(telemetry._pct(v, q))} for v, q in PCTS]
    out["keys"] = [{"port": p, "ts": repr(t), "key": telemetry.instance_key(p, t)}
                   for p, t in [("8080", 0.0), ("8080", 1792317600.9), ("1", -0.5), ("1", -1.5)]]

    def stats(s):
        return {k: r(getattr(s, k)) for k in ("n", "gen_p50", "gen_p25", "gen_p75", "prompt_p50", "draft_acc_p50",
                                              "draft_len_p50", "draft_n", "last_ts", "spec_types", "single_config",
                                              "config_count")}

    state = {}
    db.recent_timings = lambda **_: [dict(x) for x in state["rows"]]
    out["stats"] = {}
    for k, rows in ROWS.items():
        state["rows"] = rows
        out["stats"][k] = {"rows": r(rows), "stats": stats(telemetry.stats_for(model_path="/m", alias="a"))}

    def configs_for(instances):
        if state["argvs"] == "raise":
            raise RuntimeError("synthetic store failure")
        return {i: dict(state["argvs"][i]) for i in instances if i in state["argvs"]}

    db.timings_by_instance = lambda **_: [dict(x) for x in state["rows"]]
    db.server_configs_for = configs_for
    out["history"] = {}
    for k, (rows, argvs) in HISTORY.items():
        state["rows"], state["argvs"] = rows, argvs
        runs = telemetry.config_history(model_path="/m", alias="a")
        out["history"][k] = {"rows": r(rows), "argvs": argvs if argvs == "raise" else [[i, list(a.items())] for i, a in argvs.items()],
                             "runs": [{f: r(getattr(x, f)) for f in ("instance", "n", "gen_p50", "gen_p25", "gen_p75",
                                                                      "prompt_p50", "draft_acc_p50", "draft_len_p50",
                                                                      "last_ts", "argv", "is_current", "diff", "rel_pct")}
                                      for x in runs]}
    json.dump(out, sys.stdout, indent=1, sort_keys=True, ensure_ascii=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
