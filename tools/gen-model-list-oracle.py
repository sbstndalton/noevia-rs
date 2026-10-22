#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/model-list-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, middleware, router,
`api.models`, `api.overview`, `api._entry`, `helpers._badges_for_files`,
`services.snapshot_models_dir`, `services.model_shape`, `ini`, `db` and `gguf_meta`) against
synthetic model folders, models.ini files, GGUF headers and badge rows, and records the exact
status, content type and body bytes of `GET /api/v1/models` and `GET /api/v1/overview`:

    python3 -I tools/gen-model-list-oracle.py <services>/model-manager/app > fixture.json

Some fields are live state, not files, and the native routes do not re-implement them:

- `/models`: `loadedOn` (Docker discovery plus inference-server probes) and `fit` (the GPU sampler and
  host RAM). The native route asks the child for `api._loaded_map()` and
  `[services.vram_fit_chips(b) for b in sizes]` (private `POST /api/v1/internal/models-runtime`).
- `/overview`: `backends` (the services backend snapshot) and `activeDownloads` (the
  child's download table; the native table's count when the parent owns downloads), through the
  private `GET /api/v1/internal/overview-runtime`.

Here those sources are replaced by deterministic stand-ins, the real routes place them, and each
case records the body the child would answer (`*_runtime`), rendered as the routes render their
answers (pydantic's JSON mode, then FastAPI's JSONResponse). `shutil.disk_usage` is replaced by
the case's numbers. Every file name, section, badge and value is synthetic. Each case is
declarative, so the Rust tests rebuild the identical tree and store. CI re-runs this against the
pinned services ref and requires a byte-identical fixture. Run it on Linux with TZ=UTC (the
generator sets it), as CI does.
"""
from __future__ import annotations

import asyncio
import hashlib
import json
import os
import re
import shutil
import sqlite3
import struct
import sys
import tempfile
from pathlib import Path

T0 = 1_700_000_000_000_000_000  # 2023-11-14T22:13:20Z, in nanoseconds
GIB = 1024 ** 3

# --- synthetic GGUF headers ---------------------------------------------------------------------


def _s(text: str | bytes) -> bytes:
    raw = text.encode() if isinstance(text, str) else text
    return struct.pack("<Q", len(raw)) + raw


def _value(kind: str, value) -> tuple[int, bytes]:
    if kind == "str":
        return 8, _s(value)
    if kind == "u32":
        return 4, struct.pack("<I", value)
    if kind == "u64":
        return 10, struct.pack("<Q", value)
    if kind == "f32":
        return 6, struct.pack("<f", value)
    if kind == "f64":
        return 12, struct.pack("<d", value)
    if kind.startswith("arr:"):
        inner = kind[4:]
        code = _value(inner, value[0] if value else ("" if inner == "str" else 0))[0]
        body = b"".join(_value(inner, v)[1] for v in value)
        return 9, struct.pack("<IQ", code, len(value)) + body
    raise ValueError(kind)


def gguf(*kvs, version: int = 3, tensors: int = 0, magic: bytes = b"GGUF") -> str:
    out = magic + struct.pack("<IQQ", version, tensors, len(kvs))
    for key, kind, value in kvs:
        code, body = _value(kind, value)
        out += _s(key) + struct.pack("<I", code) + body
    return out.hex()


BASIC = gguf(("general.architecture", "str", "syntharch"), ("general.name", "str", "Synthetic Model"),
             ("syntharch.context_length", "u32", 8192), ("syntharch.block_count", "u32", 4),
             ("synthetic.tokens", "arr:str", [f"t{i}" for i in range(12)]))
MOE = gguf(("general.architecture", "str", "synthmoe"), ("synthmoe.expert_count", "u32", 128),
           ("synthmoe.expert_used_count", "u32", 8))
MOE_NO_USED = gguf(("general.architecture", "str", "synthsparse"), ("synthsparse.expert_count", "u64", 16))
NO_ARCH = gguf(("general.name", "str", "no architecture"))
PLAIN = gguf(("general.architecture", "str", "synthplain"), ("synthplain.block_count", "u32", 2))
STR_COUNTS = gguf(("general.architecture", "str", "syntharch"), ("syntharch.expert_count", "str", " 12 "),
                  ("syntharch.expert_used_count", "str", "x"))
FLOAT_COUNTS = gguf(("general.architecture", "str", "syntharch"), ("syntharch.expert_count", "f64", 7.9),
                    ("syntharch.expert_used_count", "f32", float("nan")))
ARCH_LIST = gguf(("general.architecture", "arr:str", ["a", "b"]), ("['a', 'b'].expert_count", "u32", 6))
KV_KEY = gguf(("general.architecture", "str", "syntharch"), ("syntharch.expert_count", "u32", 8),
              ("kv", "arr:u32", list(range(10))))
UNICODE_ARCH = gguf(("general.architecture", "str", "合成"), ("合成.expert_count", "u32", 2))
# Headers the parser refuses: model_shape() swallows the error, so the shape is null.
BAD_MAGIC = "4e4f50450300000000000000"
TRUNCATED = gguf(("general.architecture", "str", "syntharch"))[:-6]
SUMMARY_RAISES = gguf(("general.architecture", "str", "syntharch"), ("general.file_type", "arr:u32", [1, 2]))


def files(*names: str, content: str = PLAIN, mtime: int = T0) -> list:
    return [{"name": n, "hex": content, "mtime": mtime} for n in names]


# --- live-state stand-ins -----------------------------------------------------------------------

def fit_stub(size_bytes: int) -> list[dict]:
    """Deterministic chips with the float shapes the real ones carry (vram_gb from settings or the
    sampler, ratio_pct ints, a CPU chip with `is_cpu`, the machine-level `impossible` chip), plus
    non-finite values, which pydantic's JSON mode writes as null."""
    gb = size_bytes / GIB
    if size_bytes == 3 * GIB + 3:
        return [{"name": "this machine", "vram_gb": 23.5, "host_ram_gb": 0.1 + 0.2, "verdict": "impossible",
                 "ratio_pct": 412, "needs_gb": 97, "ceiling_gb": 24}]
    if size_bytes == 7:
        return [{"name": "non-finite", "vram_gb": float("nan"), "verdict": "oom", "ratio_pct": 0,
                 "host_ram_gb": float("inf")}]
    chips = [{"name": "synthetic-gpu", "vram_gb": 23.5, "verdict": "fits" if gb < 15 else "oom",
              "ratio_pct": round(gb / 23.5 * 100)},
             {"name": "synthetic-cpu", "vram_gb": 0.1 + 0.2, "verdict": "slow", "ratio_pct": 0, "is_cpu": True}]
    if size_bytes < 4096:
        chips.append({"name": "édge", "vram_gb": 1e16, "verdict": "fits", "ratio_pct": 0,
                      "host_ram_gb": 1e-05})
    return chips


def backend_objects(spec: list[dict]):
    from app import services
    return [services.LlamaBackend(**b) for b in spec]


def backends_stub(spec: list[dict]):
    async def snapshot():
        return backend_objects(spec)
    return snapshot


class _Job:
    def __init__(self, status: str):
        self.status = status


def jobs_stub(statuses: list[str]):
    return lambda: [_Job(s) for s in statuses]


def disk_stub(disk):
    def usage(_path):
        total, free = disk
        return shutil._ntuple_diskusage(total, total - free, free)
    return usage


# --- cases --------------------------------------------------------------------------------------

DEFAULT_BACKENDS = [
    {"name": "backend-a", "found": True, "status": "running", "image": "registry.invalid/synthetic/server:1",
     "short_id": "0123456789", "started_at": "2023-11-14 22:13", "uptime": "1h 2m", "uptime_s": 3720,
     "host_ports": ["8080/tcp -> 0.0.0.0:18080"], "internal_port": 8080, "loaded_model": "flat-model, short-id"},
    {"name": "backend-b", "found": True, "status": "running", "loaded_model": "short-id,,other ",
     "probe_error": None, "last_restart_error": "synthétique failure"},
    {"name": "idle", "found": True, "status": "exited", "loaded_model": None, "probe_error": "not running"},
    {"name": "gone", "found": False, "status": "not_found"},
    {"name": "backend-c", "found": True, "status": "running", "loaded_model": "  ",
     "probe_error": "not reachable from the model loader (Docker reports it healthy)",
     "unreachable_but_healthy": True},
    {"name": "backend-d", "found": True, "status": "running", "loaded_model": "Synthétique-Q4"},
]
DISK = [4_000_000_000_000, 1_234_567_890_123]
# A stored infinity (JSON cannot carry one in the case declaration; the store dump can).
INF = "@inf@"
BADGE_ROWS = [
    # alias, category, rating, note, run_id, created_at
    ["short-id", "coding", 4, "good", 3, 1_700_000_000.5],
    ["short-id", "tools", 2, "", None, 1_700_000_001.0],
    ["alpha-Q8_0", "coding", 5, "newer under the file name", None, 1_700_000_002.25],
    ["alpha-Q8_0", "vision", 3, "synthétique", 9, 1_700_000_000],
    ["persona", "coding", 1, "older", None, 1_699_999_999.0],
    ["persona", "writing", 5, "", None, 1_700_000_003.0],
    ["persona", "unknown-category", 5, "never shown", None, 1_800_000_000.0],
    ["beta-renamed", "reasoning", 3, "x" * 40, None, 1_700_000_004.0],
    ["unrelated", "coding", 5, "no file", None, 1_700_000_005.0],
]

CASES: list[dict] = [
    {"id": "flat-models-shapes-and-badges",
     "files": [{"name": "alpha-Q8_0.gguf", "hex": BASIC, "mtime": T0, "size": 1280},
               {"name": "moe-Q4.gguf", "hex": MOE, "mtime": T0 + 59_999_999_500, "size": 5 * GIB + 7},
               {"name": "moe-no-used.gguf", "hex": MOE_NO_USED, "mtime": T0 - 86_400_000_000_123, "size": 1792},
               {"name": "no-arch.gguf", "hex": NO_ARCH, "mtime": 0, "size": 1023},
               {"name": "UPPER.GGUF", "hex": PLAIN, "mtime": -2_000_000_000, "size": 1024},
               {"name": "beta.gguf", "hex": STR_COUNTS, "mtime": T0},
               {"name": "gamma.gguf", "hex": FLOAT_COUNTS, "mtime": T0},
               {"name": "arch-list.gguf", "hex": ARCH_LIST, "mtime": T0},
               {"name": "kv-key.gguf", "hex": KV_KEY, "mtime": T0},
               {"name": "unicode-合成.gguf", "hex": UNICODE_ARCH, "mtime": T0},
               {"name": "bad-magic.gguf", "hex": BAD_MAGIC, "mtime": T0},
               {"name": "empty.gguf", "hex": "", "mtime": T0},
               {"name": "truncated.gguf", "hex": TRUNCATED, "mtime": T0},
               {"name": "summary-raises.gguf", "hex": SUMMARY_RAISES, "mtime": T0},
               {"name": "tera.gguf", "hex": PLAIN, "mtime": 2_147_000_000_000_000_123, "size": 1536 * GIB}],
     "ini": {"text": "version = 1\n[DEFAULT]\nctx-size = 1\n[short-id]\nmodel = /models/alpha-Q8_0.gguf\n"
                     "[alpha-Q8_0]\nctx-size = 4096\n[persona]\nmodel = alpha-Q8_0.gguf\n"
                     "[beta-renamed]\nmodel = /beta.gguf\n[gamma]\nmodel = missing.gguf\n"
                     "[unrelated]\nmodel = elsewhere.gguf\n"},
     "badges": BADGE_ROWS},
    {"id": "subfolders-shards-companions",
     "dirs": ["sub", "solo", "deep/a/b/c", "models--org--repo/snapshots/rev1", "models--org--repo/blobs",
              "other"],
     "files": [{"name": "sub/big-00001-of-00003.gguf", "hex": MOE, "mtime": T0 + 1_000_000_000, "size": 2 * GIB},
               {"name": "sub/big-00003-of-00003.gguf", "hex": "00", "mtime": T0 + 3_000_000_000, "size": GIB},
               {"name": "sub/big-00002-of-00003.gguf", "hex": "00", "mtime": T0 + 2_000_000_000, "size": GIB},
               {"name": "sub/mmproj-big-BF16.gguf", "hex": PLAIN, "mtime": T0, "size": 700},
               {"name": "sub/mmproj-big-F32.gguf", "hex": PLAIN, "mtime": T0, "size": 1300},
               {"name": "sub/big-draft-Q4.gguf", "hex": PLAIN, "mtime": T0, "size": 300},
               {"name": "solo/mmproj-orphan.gguf", "hex": PLAIN, "mtime": T0 + 13},
               {"name": "solo/model.imatrix.gguf", "hex": "", "mtime": T0 + 14},
               {"name": "deep/a/b/c/depth-four.gguf", "hex": BASIC, "mtime": T0 + 11},
               {"name": "other/same.gguf", "hex": BASIC, "mtime": T0 + 21},
               {"name": "same.gguf", "hex": MOE, "mtime": T0 + 22},
               {"name": "big-MTP-Q8.gguf", "hex": PLAIN, "mtime": T0 + 23, "size": 3 * GIB + 3},
               {"name": "small-MTP-Q8.gguf", "hex": PLAIN, "mtime": T0 + 24, "size": 7},
               {"name": "models--org--repo/blobs/0123abcd", "hex": BASIC, "mtime": T0 + 15}],
     "links": [["models--org--repo/snapshots/rev1/hf-model-Q4.gguf", "../../blobs/0123abcd"],
               ["linked-file.gguf", "sub/mmproj-big-BF16.gguf"]],
     "ini": {"text": "[same-a]\nmodel = other/same.gguf\n[same]\nctx-size = 1\n"
                     "[big]\nmodel = sub/big-00001-of-00003.gguf\n"},
     "badges": [["same-a", "coding", 2, "subfolder copy", None, 1_700_000_000.0],
                ["same", "tools", 4, "flat copy", None, 1_700_000_000.0],
                ["big", "vision", 5, "", None, 1_700_000_000.0]]},
    {"id": "badge-comparisons",
     "files": files("one.gguf", "two.gguf", "three.gguf", "four.gguf", "five.gguf"),
     "ini": {"text": "[one]\n[one-b]\nmodel = one.gguf\n[two]\n[two-b]\nmodel = two.gguf\n[three]\n"
                     "[three-b]\nmodel = three.gguf\n[four]\n[four-b]\nmodel = four.gguf\n[five]\n"},
     "badges": [["one", "coding", 3, "equal time, first alias wins", None, 1_700_000_000.0],
                ["one-b", "coding", 4, "", None, 1_700_000_000],
                ["two", "coding", 3, "", None, 2],
                ["two-b", "coding", 4, "float beats int", None, 2.5],
                ["three", "writing", 2, "zero time", None, 0.0],
                ["three-b", "writing", 5, "empty time counts as zero", None, ""],
                ["four", "tools", 1, "", None, "abc"],
                ["four-b", "tools", 2, "text compares as text", None, "abd"],
                ["five", "reasoning", 2, "infinite time", None, INF],
                ["five", "vision", 2, "negative time", None, -1.5]]},
    {"id": "badge-mixed-types-500", "files": files("one.gguf"),
     "ini": {"text": "[one]\n[one-b]\nmodel = one.gguf\n"},
     "badges": [["one", "coding", 3, "", None, 1_700_000_000.0],
                ["one-b", "coding", 4, "", None, "text"]]},
    {"id": "badge-zero-against-text-500", "files": files("one.gguf"),
     "ini": {"text": "[one]\n[one-b]\nmodel = one.gguf\n"},
     "badges": [["one", "coding", 3, "", None, 0.0], ["one-b", "coding", 4, "", None, "later"]]},
    {"id": "badge-text-not-numbers", "files": files("one.gguf"),
     "ini": {"text": "[one]\n"},
     "badges": [["one", "coding", "4", "123", "7", "1700000000.5"],
                ["one", "tools", 4.5, "plain note", None, 1]]},
    {"id": "no-badge-table-500", "files": files("one.gguf"), "ini": {"text": "[one]\n"}, "db": "no-table"},
    {"id": "ini-unregistered-and-claims",
     "dirs": ["sub", "sub2"],
     "files": files("alpha.gguf", "Beta.gguf", "gamma.gguf", "sub/delta.gguf", "eps.gguf", "zeta.gguf",
                    "theta.gguf", "mmproj-iota.gguf", "sub2/kappa-00001-of-00002.gguf",
                    "sub2/kappa-00002-of-00002.gguf", "model.imatrix.gguf", "aaa-draft.gguf", "Zed.gguf",
                    "é.gguf")
     + [{"name": "huge-draft-Q4.gguf", "hex": PLAIN, "mtime": T0, "size": 2 * GIB + 1}],
     "ini": {"text": "[short]\nmodel = alpha.gguf\n[gamma]\nctx-size = 1\n[delta-id]\nmodel =   sub/delta.gguf  \n"
                     "[eps]\nmodel = missing.gguf\n[zeta-a]\nmodel = /models//zeta.gguf\n[mmproj-iota]\n"}},
    {"id": "ini-duplicates-and-default-model",
     "files": files("one.gguf", "two.gguf"),
     "ini": {"text": "[DEFAULT]\nmodel = two.gguf\n[one]\nctx-size = 1\n[two-a]\nx = 1\n[two-a]\ny = 2\n[one]\nz = 3\n"},
     "badges": [["one", "coding", 1, "", None, 1.0], ["two-a", "coding", 2, "", None, 1.0]]},
    {"id": "ini-crlf", "files": files("a.gguf"), "ini": {"text": "version = 1\r\n[a-id]\r\nmodel = a.gguf\r\n"}},
    {"id": "ini-missing", "files": files("a.gguf", "b.gguf")},
    {"id": "ini-parse-error-500", "files": files("a.gguf"), "ini": {"text": "[a]\nno delimiter here\n"}},
    {"id": "ini-not-utf8-500", "files": files("a.gguf"), "ini": {"hex": "5b615d0a78203d20ff0a"}},
    {"id": "ini-is-directory-500", "files": files("a.gguf"), "ini": "dir"},
    {"id": "section-path-leaving-the-folder", "files": files("a.gguf", "b.gguf"), "outside": ["outside.gguf"],
     "ini": {"text": "[b]\nmodel = ../outside.gguf\n[a-id]\nmodel = a.gguf\n"},
     # The native list cannot know which sections configure b.gguf; it refuses (the overview does not).
     "native_refuses": ["models"]},
    {"id": "models-missing", "models": "missing", "ini_outside": {"text": "[x]\n[y]\n"}},
    {"id": "models-missing-bad-ini-500", "models": "missing", "ini_outside": {"text": "[x]\nbroken\n"}},
    {"id": "models-path-is-file", "models": "file"},
    {"id": "models-folder-is-a-symlink", "models": "link", "dirs": ["sub"],
     "files": [{"name": "a.gguf", "hex": BASIC, "mtime": T0},
               {"name": "sub/b-00001-of-00002.gguf", "hex": MOE, "mtime": T0 + 1},
               {"name": "sub/b-00002-of-00002.gguf", "hex": "", "mtime": T0 + 2}],
     "ini": {"text": "[a]\nmodel = a.gguf\n"}},
    {"id": "absolute-links-inside", "dirs": ["sub"],
     "files": [{"name": "sub/x.gguf", "hex": MOE, "mtime": T0 + 7}],
     "abslinks": [["y.gguf", "sub/x.gguf"]]},
    {"id": "loaded-on-and-fit",
     "files": files("flat-model.gguf", "other.gguf", "Synthétique-Q4.gguf", content=BASIC)
     + [{"name": "seven.gguf", "hex": "00000000000000", "mtime": T0},
        {"name": "empty-size.gguf", "hex": "", "mtime": T0}],
     "ini": {"text": "[short-id]\nmodel = other.gguf\n"},
     "backends": DEFAULT_BACKENDS + [{"name": "backend-a", "found": True, "status": "running",
                                      "loaded_model": "flat-model"}]},
    {"id": "no-backends-host-path-and-jobs", "files": files("solo.gguf", content=MOE), "backends": [],
     "host_path": "/srv/synthétique models", "jobs": ["downloading", "queued", "done", "error", "queued"]},
    {"id": "empty-models-folder", "jobs": ["done"], "host_path": "", "disk": [1, 0]},
    {"id": "disk-full-and-exact-division", "files": files("a.gguf"), "disk": [3 * 10 ** 15 + 7, 1]},
    {"id": "disk-huge-numbers", "files": files("a.gguf"), "disk": [2 ** 64 - 1, 2 ** 40 + 3]},
    {"id": "disk-free-equals-total", "disk": [10 ** 12, 10 ** 12]},
    {"id": "disk-rounding-ties", "disk": [800, 799]},
    {"id": "disk-zero-total-500", "files": files("a.gguf"), "disk": [0, 0]},
]


def build(root: Path, case: dict) -> tuple[Path, Path]:
    models = root / "models"
    ini_path = models / "models.ini"
    kind = case.get("models", "dir")
    for name in case.get("outside", []):
        (root / name).write_bytes(bytes.fromhex(PLAIN))
    if "ini_outside" in case:
        ini_path = root / "models.ini"
        ini_path.write_text(case["ini_outside"]["text"])
    if kind == "file":
        models.write_bytes(b"")
    elif kind in ("dir", "link"):
        if kind == "link":
            real = root / "real-models"
            real.mkdir()
            os.symlink(real, models)
        else:
            models.mkdir()
        for d in case.get("dirs", []):
            (models / d).mkdir(parents=True, exist_ok=True)
        for f in case.get("files", []):
            path = models / f["name"]
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(bytes.fromhex(f["hex"]))
            if "size" in f:
                os.truncate(path, f["size"])
        for name, target in case.get("links", []):
            os.symlink(target, models / name)
        for name, target in case.get("abslinks", []):
            os.symlink(models.resolve() / target, models / name)
        ini = case.get("ini")
        if ini == "dir":
            ini_path.mkdir()
        elif isinstance(ini, dict):
            ini_path.write_bytes(ini["text"].encode() if "text" in ini else bytes.fromhex(ini["hex"]))
        for f in case.get("files", []):
            os.utime(models / f["name"], ns=(f["mtime"], f["mtime"]))
    return models, ini_path


def store(data: Path, case: dict) -> list[str]:
    """The case's store, as db.init() leaves it plus the badge rows, returned as SQL statements
    (sqlite3's iterdump) so the native tests rebuild exactly the same tables and stored values."""
    from app import db
    db._DB_PATH = None
    db.init()
    with sqlite3.connect(data / "model_loader.db") as c:
        if case.get("db") == "no-table":
            c.execute("DROP TABLE model_badge")
        for row in case.get("badges", []):
            c.execute("INSERT INTO model_badge(alias, category, rating, note, run_id, created_at) "
                      "VALUES(?, ?, ?, ?, ?, ?)", [float("inf") if v == INF else v for v in row])
    with sqlite3.connect(data / "model_loader.db") as c:
        # iterdump spells a stored infinity by the SQLite build: `Inf` (which SQLite does not read
        # back) or, from SQLite 3.43, `9.0e+999`. Both are the same stored value; the oracle keeps
        # one spelling, 9e999, so it does not depend on the SQLite bundled with the interpreter
        # (sbstndalton/noevia#2123).
        return [re.sub(r"(?<=[,(])(-?)(?:Inf|9\.0e\+999)(?=[,)])", r"\g<1>9e999", s) if s.startswith('INSERT INTO "model_badge"')
                else s for s in c.iterdump()]


def rendered(body: dict) -> str:
    """A `-> dict` route's answer: pydantic's JSON mode (non-finite floats become null), then
    FastAPI's JSONResponse. The child's private calls render the same way."""
    from fastapi.responses import JSONResponse
    from pydantic import TypeAdapter
    return JSONResponse(TypeAdapter(dict).dump_python(body, mode="json")).body.decode("utf-8")


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith(("NOEVIA_NATIVE_", "MODELS_", "MODEL_FILES", "GGUF_", "LLAMA_")) \
                or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR", "DOCKER_HOST", "GPU_VRAM"}:
            del os.environ[key]
    os.environ["TZ"] = "UTC"
    import time
    time.tzset()
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import api, config, downloader, gguf_meta, services  # noqa: E402
    from app.main import app  # noqa: E402

    client = TestClient(app, raise_server_exceptions=False)  # no lifespan: no sampler
    names = ("api.py", "main.py", "services.py", "ini.py", "utils.py", "gguf_meta.py", "autoconfig.py",
             "autoconfig_core.py", "discover.py", "config.py", "db.py", "helpers.py", "downloader.py")
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest() for name in names}
    services.vram_fit_chips = fit_stub
    out = []
    for case in CASES:
        backends = case.get("backends", DEFAULT_BACKENDS)
        services.snapshot_llama_backends = backends_stub(backends)
        downloader.manager.snapshot = jobs_stub(case.get("jobs", []))
        services.shutil.disk_usage = disk_stub(case.get("disk", DISK))
        with tempfile.TemporaryDirectory() as tmp:
            models, ini_path = build(Path(tmp), case)
            data = Path(tmp) / "data"
            config.settings.models_dir = models
            config.settings.models_ini_path = ini_path
            config.settings.data_dir = data
            config.settings.models_host_path = case.get("host_path", "")
            dump = store(data, case)
            gguf_meta._CACHE.clear()
            # The models folder path appears in the overview: the case's temporary folder
            # becomes @MODELS@ (the native tests substitute their own folder the same way).
            here = json.dumps(str(models))[1:-1]
            results = {}
            for route in ("models", "overview"):
                response = client.get(f"/api/v1/{route}")
                results[route] = {"status": response.status_code,
                                  "content_type": response.headers.get("content-type"),
                                  "body": response.content.decode("utf-8").replace(here, "@MODELS@")}
            # The child is asked once the snapshot has succeeded (a store error comes after it).
            try:
                snap = services.snapshot_models_dir()
            except Exception:  # noqa: BLE001 - the route answered 500 before asking the child
                snap = None
            sizes = [g.total_bytes for g in snap.ggufs if not g.is_companion] if snap else None
            results["models_runtime"] = None if sizes is None else {
                "sizes": sizes,
                "body": rendered({"loaded": asyncio.run(api._loaded_map()),
                                  "fit": [services.vram_fit_chips(b) for b in sizes]})}
            results["overview_runtime"] = rendered({
                "backends": [api._plain(b) for b in backend_objects(backends)],
                "activeDownloads": sum(1 for j in downloader.manager.snapshot()
                                       if j.status in ("queued", "downloading"))})
            out.append({**case, "store": dump, "results": results})
    json.dump({"oracle": digest, "cases": out}, sys.stdout, ensure_ascii=True, indent=0, sort_keys=True,
              allow_nan=False)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
