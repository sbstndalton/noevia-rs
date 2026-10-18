#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/telemetry-ingest-python.v1.json
(noevia#1318 step 3d).

Runs the ACTUAL noevia-services model-manager `telemetry.ingest`, `stats_for` and
`config_history` over a sequence of passes against a fresh store made by the app's own
`db.init()`. Docker is a stand-in client whose `containers.get(name).logs(...)` answers the
pass's synthetic log bytes (or raises); `time.time` is pinned per pass. After every pass the
telemetry tables and keys are dumped, with the measurements of one section as the route renders
them. The passes qualify the ingest: a first read, the 20 s interval, repeated reads of the same
tail, a restart (a respawn on the same port), port reuse with the spawn line scrolled out (with
and without a gap since the last pass), an interrupted write (a trigger that aborts the
`req_timing` insert, then removed), recovery, a parse that raises, a multiplexed log with a
truncated frame and invalid UTF-8. Every name, path and figure is synthetic.

    python3 -I tools/gen-telemetry-ingest-oracle.py <services>/model-manager/app > fixture.json
"""
from __future__ import annotations

import hashlib
import json
import os
import shutil
import sqlite3
import struct
import sys
import tempfile
import time
from pathlib import Path

T0 = 1792317600  # 2026-10-18T10:00:00Z


def stamp(sec, frac="123456789"):
    t = time.gmtime(T0 + sec)
    return time.strftime("%Y-%m-%dT%H:%M:%S", t) + f".{frac}Z "


def spawn(sec, alias, port, argv):
    out = [stamp(sec, "000000001") + f"srv    load: spawning server instance with name={alias} on port {port}",
           stamp(sec, "000000001") + "srv    load: spawning server instance with args:"]
    return out + [stamp(sec, "000000001") + f"srv    load:   {a}" for a in argv]


def load(sec, port, path):
    return stamp(sec) + f"[{port}] 0.05.690 I srv    load_model: loading model '{path}'"


def req(sec, port, task, gen=111, rate=141.5, prompt_rate=6695.1, draft=None):
    out = [stamp(sec) + f"[{port}] x slot print_timing: id  0 | task {task} | prompt eval time = 1239.27 ms / 8297 "
           f"tokens (0.15 ms per token, {prompt_rate} tokens per second)",
           stamp(sec) + f"[{port}] x slot print_timing: id  0 | task {task} |        eval time =  777.37 ms /  {gen} "
           f"tokens (7.07 ms per token,  {rate} tokens per second)"]
    if draft:
        out.append(stamp(sec) + f"[{port}] x slot print_timing: id  0 | task {task} | draft acceptance = {draft} "
                   f"( 51 accepted / 81 generated), mean len = 2.13")
    return out


ARGV_1 = ["--model", "/models/syn/a.gguf", "--alias", "syn-a", "--port", "18001", "--ctx-size", "8192",
          "--spec-type", "draft-mtp", "--spec-draft-n-max", "4"]
ARGV_2 = ["--model", "/models/syn/a.gguf", "--alias", "syn-a", "--port", "18001", "--ctx-size", "16384",
          "--jinja", "--name", "été \"q\" ☃ \U0001f600 \x7f"]


def text(lines):
    return ("\n".join(lines) + "\n").encode()


def frames(chunks, truncate=0):
    out = b"".join(struct.pack(">BxxxL", kind, len(data)) + data for kind, data in chunks)
    return out[:len(out) - truncate] if truncate else out


def passes():
    a1 = spawn(1, "syn-a", 18001, ARGV_1) + [load(2, 18001, "/models/syn/a.gguf")]
    a1 += sum((req(3 + i, 18001, i, rate=140.0 + i, draft=0.5 + i / 100) for i in range(6)), [])
    a1_more = a1 + sum((req(20 + i, 18001, 10 + i, rate=150.0 + i) for i in range(2)), [])
    a2 = a1_more + spawn(200, "syn-a", 18001, ARGV_2) + [load(201, 18001, "/models/syn/a.gguf")]
    a2 += sum((req(202 + i, 18001, i, rate=90.0 + i) for i in range(5)), [])
    # The spawn scrolled out: the tail starts inside what the last pass read (no gap), with
    # requests of the respawned instance already held, then new ones: these are attributed to the
    # stored instance.
    scrolled = sum((req(202 + i, 18001, i, rate=90.0 + i) for i in range(2, 5)), [])
    scrolled += sum((req(260 + i, 18001, 20 + i, rate=95.0 + i) for i in range(4)), [])
    gap = sum((req(900 + i, 18001, 30 + i, rate=80.0 + i) for i in range(3)), [])
    raising = scrolled + [stamp(270) + "[18001] x slot print_timing: id  0 | task 99 |        eval time =  1 ms / "
                                       "20 tokens (1 ms per token,  1.2.3 tokens per second)"]
    b = spawn(5, "syn-b", 18002, ["-m", "/models/syn/b.gguf", "-c", "4096"]) + req(6, 18002, 1, rate=33.0)
    c_chunks = [(1, text(spawn(7, "syn-c", 18003, ["-m", "/models/syn/c.gguf"]))),
                (2, b"\xff\xfe broken \xe2\x82 bytes\n"), (1, text(req(8, 18003, 1, rate=12.5)))]
    return [
        {"id": "first", "now": T0 + 100.5, "logs": {"syn-a-backend": text(a1), "syn-b-backend": "raise"}},
        {"id": "interval", "now": T0 + 110.5, "logs": {"syn-a-backend": text(a1_more)}},
        {"id": "repeat", "now": T0 + 130.5, "logs": {"syn-a-backend": text(a1_more), "syn-b-backend": text(b)}},
        {"id": "repeat-again", "now": T0 + 151.0, "logs": {"syn-a-backend": text(a1_more), "syn-b-backend": text(b)}},
        {"id": "restart", "now": T0 + 230.0, "logs": {"syn-a-backend": text(a2)}},
        {"id": "port-reuse", "now": T0 + 280.0, "logs": {"syn-a-backend": text(scrolled)}},
        {"id": "gap", "now": T0 + 1000.0, "logs": {"syn-a-backend": text(gap)}},
        {"id": "interrupted", "now": T0 + 1100.0, "trigger": True,
         "logs": {"syn-a-backend": text(gap + sum((req(1050 + i, 18001, 40 + i) for i in range(2)), []))}},
        {"id": "recovered", "now": T0 + 1200.0,
         "logs": {"syn-a-backend": text(gap + sum((req(1050 + i, 18001, 40 + i) for i in range(2)), []))}},
        {"id": "raises", "now": T0 + 1300.0, "logs": {"syn-a-backend": text(raising)}},
        {"id": "multiplexed", "now": T0 + 1400.0, "tty": {"syn-c-backend": False},
         "logs": {"syn-c-backend": frames(c_chunks, truncate=3)}},
        {"id": "bad-keys", "now": T0 + 1500.0, "kv": {"telemetry_log_watermark:syn-b-backend": " nan ",
                                                     "telemetry_continuous_since:syn-b-backend": "x"},
         "logs": {"syn-b-backend": text(b)}},
    ]


NAMES = ["syn-a-backend", "syn-b-backend", "syn-c-backend"]
TRIGGER = ("CREATE TRIGGER synthetic_interrupt BEFORE INSERT ON req_timing "
           "BEGIN SELECT RAISE(ABORT, 'synthetic interrupted write'); END")


def dump(path):
    c = sqlite3.connect(path)
    out = {
        "req_timing": [[repr(v) if isinstance(v, float) else v for v in r] for r in c.execute(
            "SELECT id, ts, backend, instance, task, model_path, alias, spec_type, prompt_tokens, prompt_tps, "
            "gen_tokens, gen_tps, draft_acc, draft_len FROM req_timing ORDER BY id")],
        "server_config": [[repr(v) if isinstance(v, float) else v for v in r] for r in c.execute(
            "SELECT backend, instance, alias, model_path, argv_json, first_seen FROM server_config "
            "ORDER BY backend, instance")],
        "kv": [list(r) for r in c.execute("SELECT key, value FROM kv WHERE key LIKE 'telemetry%' ORDER BY key")],
    }
    c.close()
    return out


def main() -> None:
    app_dir = Path(sys.argv[1]).resolve()
    root = Path(tempfile.mkdtemp(prefix="gen-telemetry-ingest-"))
    os.environ.update(TZ="UTC", DATA_DIR=str(root), MODELS_DIR=str(root / "models"),
                      MODELS_INI_PATH=str(root / "models" / "models.ini"), MODEL_LOADER_TOKEN="")
    (root / "models").mkdir()
    sys.path.insert(0, str(app_dir.parent))
    from pydantic import TypeAdapter

    from app import db, services, telemetry
    from app.api import _plain

    db.init()
    schema = [r[0] for r in sqlite3.connect(root / "model_loader.db").execute(
        "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")]
    state = {}
    asked = []

    class Container:
        def __init__(self, name):
            self.name = name
            self.id = hashlib.sha256(name.encode()).hexdigest()

        def logs(self, **kw):
            asked.append([self.name, sorted(kw.items())])
            data = state["pass"]["logs"].get(self.name)
            if data is None or data == "raise":
                raise RuntimeError("synthetic docker failure")
            tty = state["pass"].get("tty", {}).get(self.name, True)
            if tty:
                return data
            # docker-py's own frame handling, as `Container.logs` applies it without a TTY.
            from docker.api.client import APIClient

            class R:
                content = data
                status_code = 200

                def raise_for_status(self):
                    pass
            client = APIClient.__new__(APIClient)
            client._raise_for_status = lambda r: None
            client._result = lambda r, json=False, binary=False: r.content
            return b"".join(client._multiplexed_buffer_helper(R()))

    class Containers:
        def get(self, name):
            return Container(name)

    class Client:
        containers = Containers()

    services._docker_client = lambda: Client()
    out_passes = []
    db_path = root / "model_loader.db"
    for p in passes():
        state["pass"] = p
        c = sqlite3.connect(db_path)
        for k, v in (p.get("kv") or {}).items():
            c.execute("INSERT INTO kv(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                      (k, v))
        if p.get("trigger"):
            c.execute(TRIGGER)
        c.commit()
        c.close()
        real = time.time
        time.time = lambda: p["now"]
        try:
            telemetry.ingest(NAMES)
            ingest = "ok"
        except Exception as e:  # noqa: BLE001 - the route's except: empty measurements
            ingest = type(e).__name__
        finally:
            time.time = real
        c = sqlite3.connect(db_path)
        c.execute("DROP TRIGGER IF EXISTS synthetic_interrupt")
        c.commit()
        c.close()
        measured = telemetry.stats_for(model_path="/models/syn/a.gguf", alias="syn-a")
        history = telemetry.config_history(model_path="/models/syn/a.gguf", alias="syn-a")
        shown = TypeAdapter(dict).dump_python({"measured": _plain(measured), "history": _plain(history)}, mode="json")
        logs = {}
        for k, v in p["logs"].items():
            logs[k] = v if v == "raise" else v.hex()
        out_passes.append({"id": p["id"], "now": repr(p["now"]), "logs": logs, "tty": p.get("tty", {}),
                           "kv": p.get("kv") or {}, "trigger": bool(p.get("trigger")), "ingest": ingest,
                           "store": dump(db_path),
                           "measured": json.dumps(shown["measured"], ensure_ascii=False, separators=(",", ":")),
                           "history": json.dumps(shown["history"], ensure_ascii=False, separators=(",", ":"))})
    shutil.rmtree(root, ignore_errors=True)
    sources = {n: hashlib.sha256((app_dir / n).read_bytes()).hexdigest() for n in ("telemetry.py", "db.py", "api.py")}
    json.dump({"sources": sources, "schema": schema, "names": NAMES, "trigger": TRIGGER,
               "asked": sorted({json.dumps(a[1]) for a in asked}), "passes": out_passes},
              sys.stdout, indent=1, sort_keys=True, ensure_ascii=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
