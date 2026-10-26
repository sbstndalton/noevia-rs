"""Regenerate synthetic parity cases from shipped CPython 3.12 storage_backoff.py.

From this crate directory, via the development wrapper:
  ~/bin/devrun python3.12 tests/generate_fixtures.py --source /path/to/noevia-services/diary/agent/storage_backoff.py
No HTTP request, credential, DB, filesystem storage, or real clock is used.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import locale
import os
import random
import sys
import types
import unicodedata
from pathlib import Path


def main() -> None:
    argp = argparse.ArgumentParser()
    argp.add_argument("--source", required=True, type=Path)
    args = argp.parse_args()
    os.environ["TZ"] = "UTC"
    import time
    time.tzset()
    locale.setlocale(locale.LC_ALL, "C")
    # Only the pure parser and gate are exercised. A tiny import stub avoids
    # installing or invoking httpx while loading the shipped module.
    httpx = types.ModuleType("httpx")
    httpx.BaseTransport = object
    sys.modules["httpx"] = httpx
    spec = importlib.util.spec_from_file_location("storage_backoff_oracle", args.source)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load oracle")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    source = args.source.read_bytes()

    parse_cases: list[dict] = []
    wall = 1445412420.0  # fixed 2015 UTC epoch input, not host time
    values = [
        None, "", " ", "0", "-1", "1", "1.1", "2.001", "2.999", "59.5",
        "3600", "3601", "+2", "1.", ".5", "1e-2", "1e5", "1e999",
        "-1e999", "inf", "Infinity", "-inf", "NaN", "nan", "1_2",
        "1.2_3", "1e1_0", "1_.2", "_12", "12_", "1__2", "1,2",
        "  5  ", "\u00a0٣.٥\u2003", "１２", "\x1f7\x1f", "−2", "bad",
        "Wed, 21 Oct 2015 07:28:00 GMT", "Wed, 21 Oct 2015 07:25:00 GMT",
        "Sun, 06 Nov 1994 08:49:37 GMT",
        "Sunday, 06-Nov-94 08:49:37 GMT",
        "Sun Nov  6 08:49:37 1994",
        "Wed, 21 Oct 2015 08:28:00 +0100",
        "Mon, 00 Jan 2000 00:00:00 GMT",
        "Wed, 21 Oct 2015 07:28:00 UTC",
        "x" * 8193,
    ]
    rng = random.Random(11661519)
    for _ in range(120):
        sign = rng.choice(["", "+", "-"])
        integer = str(rng.randrange(0, 10000))
        fractional = rng.choice(["", "." + str(rng.randrange(0, 10000))])
        exponent = rng.choice(["", "e" + str(rng.randrange(-5, 6))])
        values.append(sign + integer + fractional + exponent)
    original_time = module.time.time
    try:
        module.time.time = lambda: wall
        for value in values:
            for default in [60.0, 7.25]:
                try:
                    expected = module.parse_retry_after(value, default)
                except Exception as exc:
                    raise RuntimeError(f"oracle rejected {value!r}") from exc
                parse_cases.append({"value": value, "default": default, "wall": wall, "expected": expected})
    finally:
        module.time.time = original_time

    sequences: list[dict] = []

    def run_sequence(name: str, actions: list[dict]) -> None:
        clock = [0.0]
        gate = module.StorageGate(clock=lambda: clock[0])
        steps: list[dict] = []
        original_time = module.time.time
        try:
            for action in actions:
                clock[0] = action["mono"]
                module.time.time = lambda a=action: a["wall"]
                result = None
                if action["op"] == "record":
                    gate.record(action["status"], action.get("header"), action.get("root", False))
                elif action["op"] == "reset":
                    result = gate.reset_login()
                elif action["op"] != "sample":
                    raise ValueError(action["op"])
                state = gate.state()
                try:
                    gate.check()
                    blocked = None
                except module.StorageBackoff as exc:
                    blocked = {"kind": exc.kind, "retry_after": exc.retry_after}
                steps.append({"action": action, "reset_result": result, "state": list(state), "blocked": blocked})
        finally:
            module.time.time = original_time
        sequences.append({"name": name, "steps": steps})

    run_sequence("login reset and rate limit", [
        {"op": "record", "mono": 1000.0, "wall": wall, "status": 401, "root": False},
        {"op": "sample", "mono": 1000.1, "wall": wall},
        {"op": "reset", "mono": 1001.0, "wall": wall},
        {"op": "record", "mono": 1002.0, "wall": wall, "status": 401},
        {"op": "reset", "mono": 1002.0, "wall": wall},
        {"op": "sample", "mono": 1030.0, "wall": wall},
        {"op": "reset", "mono": 1031.0, "wall": wall},
        {"op": "record", "mono": 1032.0, "wall": wall, "status": 401},
        {"op": "record", "mono": 1033.0, "wall": wall, "status": 429, "header": "3600"},
        {"op": "record", "mono": 1034.0, "wall": wall, "status": 200},
        {"op": "sample", "mono": 1400.0, "wall": wall},
    ])
    run_sequence("root versus path forbidden", [
        {"op": "record", "mono": 1.0, "wall": wall, "status": 403, "root": False},
        {"op": "record", "mono": 2.0, "wall": wall, "status": 403, "root": True},
        {"op": "record", "mono": 3.0, "wall": wall, "status": 404},
        {"op": "record", "mono": 4.0, "wall": wall, "status": 204},
        {"op": "sample", "mono": 5.0, "wall": wall},
    ])
    run_sequence("throttle extension and preemption", [
        {"op": "record", "mono": 20.0, "wall": wall, "status": 429, "header": "120"},
        {"op": "record", "mono": 30.0, "wall": wall, "status": 429, "header": "10"},
        {"op": "record", "mono": 40.0, "wall": wall, "status": 429, "header": "3601"},
        {"op": "reset", "mono": 41.0, "wall": wall},
        {"op": "record", "mono": 42.0, "wall": wall, "status": 401},
        {"op": "record", "mono": 43.0, "wall": wall, "status": 429, "header": "3600"},
        {"op": "sample", "mono": 342.0, "wall": wall},
        {"op": "record", "mono": 343.0, "wall": wall, "status": 429, "header": "2.5"},
        {"op": "record", "mono": 344.0, "wall": wall, "status": 500},
        {"op": "sample", "mono": 346.0, "wall": wall},
    ])
    run_sequence("negative fake clock and expiry", [
        {"op": "record", "mono": -100.0, "wall": wall, "status": 401},
        {"op": "reset", "mono": -99.0, "wall": wall},
        {"op": "sample", "mono": 200.0, "wall": wall},
        {"op": "reset", "mono": 200.0, "wall": wall},
        {"op": "record", "mono": 201.0, "wall": wall, "status": 200},
    ])
    for seq in range(12):
        mono = 1000.0
        actions = []
        for _ in range(30):
            mono = round(mono + rng.choice([0.0, 0.1, 1.0, 5.0, 29.0, 30.0, 301.0]), 1)
            op = rng.choice(["record", "record", "record", "sample", "reset"])
            action = {"op": op, "mono": mono, "wall": wall + mono}
            if op == "record":
                action["status"] = rng.choice([200, 201, 204, 301, 400, 401, 403, 404, 429, 500, 503])
                action["root"] = rng.choice([True, False])
                action["header"] = rng.choice([None, "", "bad", "0", "2.001", "120", "3601", "inf", "Wed, 21 Oct 2015 07:28:00 GMT"])
            actions.append(action)
        run_sequence(f"seeded sequence {seq}", actions)

    data = {
        "oracle": "shipped noevia-services diary.agent.storage_backoff CPython 3.12; stubbed httpx import; UTC/C locale",
        "python": sys.version.split()[0],
        "unicode": unicodedata.unidata_version,
        "source_sha256": hashlib.sha256(source).hexdigest(),
        "parse": parse_cases,
        "sequences": sequences,
    }
    output = Path(__file__).with_name("storage-backoff-fixtures.json")
    output.write_text(json.dumps(data, ensure_ascii=False, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {len(parse_cases)} parse cases and {sum(len(s['steps']) for s in sequences)} gate steps")


if __name__ == "__main__":
    main()
