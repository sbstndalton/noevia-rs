#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/section-autoconfig-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, middleware and router,
`api.section_autoconfig`, `api.budget_backends`, `helpers._resolve_section_gguf`, `ini` with the
roles file, `gguf_meta`, `autoconfig.analyze` with `autoconfig_core`) against synthetic model
folders and models.ini files, and records for each request:

  * the exact status, content type and body of `GET /api/v1/sections/{name}/autoconfig`,
  * the request the native parent then makes of the child (`{"section", "model"}`, or none when
    the route answers before reading any live state), and the child's real answer to it from the
    private `POST /api/v1/internal/section-autoconfig-runtime` (with the confirmation set).

The live state is a stand-in: the backend inventory (`helpers._backend_list`) and the telemetry
(`telemetry.ingest`, `stats_for`, `config_history`) give each case's synthetic values, the same in
both calls. Every file name, section and backend is synthetic; GGUF files are tiny synthetic
headers (recorded as `header`, hex) and other files are zeros of a given size (sparse).

    python3 -I tools/gen-section-autoconfig-oracle.py <services>/model-manager/app > fixture.json
"""
from __future__ import annotations

import hashlib
import json
import os
import struct
import sys
import tempfile
from pathlib import Path

GIB = 1024 ** 3
ROLE = "laya"  # the system routing role's value (a services identifier)
TOKEN = "synthetic-section-autoconfig-oracle-token-0123456789"
ENV = "NOEVIA_NATIVE_MODEL_MANAGER_SECTION_AUTOCONFIG_CONFIRMED"
TYPES = {"u8": (0, "<B"), "i8": (1, "<b"), "u16": (2, "<H"), "i16": (3, "<h"), "u32": (4, "<I"),
         "i32": (5, "<i"), "f32": (6, "<f"), "bool": (7, "<B"), "u64": (10, "<Q"), "i64": (11, "<q"),
         "f64": (12, "<d")}


def _scalar(kind: str, value) -> bytes:
    if kind == "str":
        v = value.encode()
        return struct.pack("<Q", len(v)) + v
    return struct.pack(TYPES[kind][1], value)


def build_gguf(kv: dict) -> bytes:
    """GGUF v3 with the metadata `kv` ({key: [kind, value]}, kind "arr:<kind>" for an array)."""
    out = bytearray(b"GGUF")
    out += struct.pack("<IQQ", 3, 0, len(kv))
    for key, (kind, value) in kv.items():
        k = key.encode()
        out += struct.pack("<Q", len(k)) + k
        if kind.startswith("arr:"):
            inner = kind[4:]
            code = 8 if inner == "str" else TYPES[inner][0]
            out += struct.pack("<IIQ", 9, code, len(value))
            for item in value:
                out += _scalar(inner, item)
        elif kind == "str":
            out += struct.pack("<I", 8) + _scalar("str", value)
        else:
            out += struct.pack("<I", TYPES[kind][0]) + _scalar(kind, value)
    return bytes(out)


def model(arch="synth", layers=32, ctx=32768, embed=4096, heads=32, kv=8, extra=None, template=True,
          title=None) -> dict:
    kv_meta = {"general.architecture": ["str", arch], "general.file_type": ["u32", 15],
               f"{arch}.block_count": ["u32", layers], f"{arch}.context_length": ["u32", ctx],
               f"{arch}.embedding_length": ["u32", embed], f"{arch}.attention.head_count": ["u32", heads]}
    if kv is not None:
        kv_meta[f"{arch}.attention.head_count_kv"] = ["u32", kv] if isinstance(kv, int) else kv
    if title:
        kv_meta["general.name"] = ["str", title]
    if template:
        kv_meta["tokenizer.chat_template"] = ["str", template if isinstance(template, str)
                                              else "{% for m in messages %}{{ m.content }}{% endfor %}"]
    kv_meta.update(extra or {})
    return {"gguf": kv_meta}


def g(name: str, size: int | None = None, **kw) -> dict:
    """A model file: a synthetic header, padded (sparse) to `size` bytes when given."""
    return {"name": name, "size": size, **model(**kw)}


def f(name: str, size: int = 16) -> dict:
    return {"name": name, "size": size}


def gpu(name="synthetic-gpu", vram=24.0, cards=None, count=1, ram=64.0, vendor="cuda", baseline=None,
        args=None) -> dict:
    return {"name": name, "vendor": vendor, "vram_gb": vram, "gpu_count": count,
            "card_vram_gb": cards if cards is not None else [vram], "host_ram_gb": ram,
            "baseline": baseline or {}, "baseline_args": args or []}


MEASURED = {"n": 9, "gen_p50": 141.5, "gen_p25": 120.25, "gen_p75": 150.0, "prompt_p50": 900.0,
            "draft_acc_p50": 0.62963, "draft_len_p50": 2.13, "draft_n": 3, "last_ts": 1700000000.5,
            "spec_types": ["draft-mtp (max=4)"], "single_config": False, "config_count": 2}
HISTORY = [{"instance": "8080@1700000000", "n": 5, "gen_p50": 141.5, "gen_p25": 0.0, "gen_p75": 0.0,
            "prompt_p50": 900.0, "draft_acc_p50": None, "draft_len_p50": None, "last_ts": 1700000000.5,
            "argv": {"--model": "/models/s/m.gguf", "--ctx-size": "8192"}, "is_current": True,
            "diff": {"--ctx-size": "8192"}, "rel_pct": 100},
           {"instance": "8080@1690000000", "n": 3, "gen_p50": 99.0, "gen_p25": 0.0, "gen_p75": 0.0,
            "prompt_p50": 0.0, "draft_acc_p50": None, "draft_len_p50": None, "last_ts": 1690000000.0,
            "argv": {"--model": "/models/s/m.gguf"}, "is_current": False, "diff": {"--ctx-size": "unset"},
            "rel_pct": 70}]

MOE = {"synth.expert_count": ["u32", 64], "synth.expert_used_count": ["u32", 8]}
# The crate keys its sliding-window rules on this architecture identifier.
SWA_ARCH = "gemma3"  # an existing architecture identifier of the crate and services
SWA = {f"{SWA_ARCH}.attention.sliding_window": ["u32", 1024], f"{SWA_ARCH}.attention.key_length": ["u32", 256],
       f"{SWA_ARCH}.attention.value_length": ["u32", 256]}
QUERIES = ["", "?sessions=4", "?preset=balanced", "?preset=long-ctx", "?preset=bogus", "?vision=false",
           "?verified_ctx=16384", "?prompt_budget_s=30", "?prompt_budget_s=0", "?prompt_budget_s=nan",
           "?prompt_budget_s=-inf", "?budget_gib=12", "?budget_gib=inf", "?budget_gib=nan", "?mode=+Code+",
           "?spec=coding", "?spec=off&mode=agent", "?sessions=0", "?sessions=99", "?sessions=-3",
           "?sessions=2&sessions=3", "?verified_ctx=-5&prompt_budget_s=1e9", "?mode=WRITING&vision=YES"]

CASES: list[dict] = [
    {"id": "flat-dense", "files": [g("m.gguf", 4 * GIB, title="Synthetic dense")], "backends": [gpu()],
     "requests": [("m", q) for q in QUERIES]},
    {"id": "flat-dense-unmeasured", "files": [g("m.gguf", 4 * GIB, ctx=131072)], "backends": [gpu()],
     "measured": {"n": 0}, "history": [], "requests": [("m", ""), ("m", "?verified_ctx=65536"), ("m", "?sessions=8")]},
    {"id": "moe-offload", "files": [g("moe/q-Q4.gguf", 20 * GIB, extra=MOE, ctx=262144)],
     "backends": [gpu(vram=16.0, ram=96.0)],
     "requests": [("q-Q4", ""), ("q-Q4", "?preset=fast"), ("q-Q4", "?preset=long-ctx"), ("q-Q4", "?budget_gib=10"),
                  ("q-Q4", "?sessions=3&preset=balanced")]},
    {"id": "dense-offload", "files": [g("big.gguf", 30 * GIB, layers=64, embed=5120, heads=40, kv=8)],
     "backends": [gpu(vram=24.0, ram=64.0)],
     "requests": [("big", ""), ("big", "?preset=fast"), ("big", "?preset=long-ctx")]},
    {"id": "subdir-projector-head",
     "files": [g("vl/v-Q4.gguf", 5 * GIB), f("vl/mmproj-v-F16.gguf", 600 * 1024 ** 2),
               f("vl/mmproj-v-Q8_0.gguf", 320 * 1024 ** 2), f("vl/MTP/v-mtp-Q8_0.gguf", 100 * 1024 ** 2),
               f("vl/MTP/v-mtp-BF16.gguf", 180 * 1024 ** 2), f("other.txt")],
     "backends": [gpu(vram=12.0)],
     "requests": [("v-Q4", ""), ("v-Q4", "?vision=0"), ("v-Q4", "?spec=writing"), ("v-Q4", "?mode=code"),
                  ("v-Q4", "?spec=ngram")]},
    {"id": "flat-projector-head",
     "files": [g("fp.gguf", 3 * GIB), f("mmproj-fp-f16.gguf", 300 * 1024 ** 2), f("fp-draft-Q8.gguf", 50 * 1024 ** 2),
               f("fp-mtp-big.gguf", 3 * GIB)],
     "backends": [gpu(vram=8.0)], "requests": [("fp", ""), ("fp", "?vision=off&spec=balanced")]},
    {"id": "saved-section",
     "files": [g("x/x.gguf", 6 * GIB), f("x/proj-mmproj.gguf", 200 * 1024 ** 2), f("x/head-mtp.gguf", 80 * 1024 ** 2),
               f("x/mmproj-unused.gguf", 10)],
     "ini": "[x]\nmodel = /models/x/x.gguf\nmmproj = /models/x/proj-mmproj.gguf\n"
            "spec-draft-model = /models/x/head-mtp.gguf\nspec-type = draft-mtp\nspec-draft-n-max = 8\n"
            "ctx-size = 8192\nngl = 999\nflash-attn = on\ncache-type-k = q8_0\ncache-type-v = q8_0\n"
            "parallel = 1\njinja = true\nbatch-size = 2048\n\n"
            "[y]\nmodel = /models/x/x.gguf\nmmproj = /models/x/missing-mmproj.gguf\nspec-draft-model = /models/nope.gguf\n\n"
            "[z]\nmodel = x/x.gguf\nmmproj =   \nn-cpu-moe = 4\nsome-extra = 1\n",
     "backends": [gpu(vram=16.0)],
     "requests": [("x", ""), ("x", "?spec=coding"), ("x", "?preset=long-ctx"), ("y", ""), ("z", "")]},
    # A relative saved projector path: Python resolves it against its working directory (where it
    # does not exist); the native route refuses it (see the lib test's deliberate refusals).
    {"id": "saved-relative-projector",
     "files": [g("sub/m.gguf", 4 * GIB), f("sub/p.gguf", 64 * 1024 ** 2)],
     "ini": "[r]\nmodel = /models/sub/m.gguf\nmmproj = sub/p.gguf\n", "backends": [gpu()],
     "requests": [("r", "")]},
    {"id": "sharded",
     "files": [g("sh/s-00001-of-00002.gguf", 3 * GIB), f("sh/s-00002-of-00002.gguf", 3 * GIB),
               f("sh/mmproj-s.gguf", 100 * 1024 ** 2), f("sh/s-imatrix.gguf", 5 * 1024 ** 2), f("sh/readme.md", 7)],
     "backends": [gpu(vram=8.0)], "requests": [("s", "")]},
    {"id": "explicit-model",
     "files": [g("sub/a.gguf", 2 * GIB), g("sub/b.gguf", 3 * GIB, layers=40), f("sub/mmproj-b.gguf", 64 * 1024 ** 2)],
     "ini": "[short]\nmodel = /models/sub/b.gguf\n\n[flat]\nmodel = sub/b.gguf\n\n[gone]\nmodel = /models/sub/missing.gguf\n",
     "backends": [gpu()], "requests": [("short", ""), ("flat", ""), ("gone", ""), ("a", ""), ("b", "")]},
    {"id": "sliding-window-arch",
     "files": [g("gm.gguf", 8 * GIB, arch=SWA_ARCH, layers=48, embed=3840, heads=16, kv=8, ctx=131072,
                 extra={**SWA, f"{SWA_ARCH}.attention.sliding_window_pattern": ["u32", 6]})],
     "backends": [gpu(vram=16.0)], "requests": [("gm", ""), ("gm", "?sessions=2")]},
    {"id": "hybrid-kv-layers",
     "files": [g("hy.gguf", 4 * GIB, layers=12, kv=["arr:u32", [0, 0, 0, 4] * 3]),
               g("hp.gguf", 4 * GIB, layers=24, kv=["arr:u32", [0, 0, 0, 4] * 6])],
     "backends": [gpu()], "requests": [("hy", ""), ("hp", "")]},
    {"id": "refuse-no-backends", "files": [g("m.gguf", GIB)], "backends": [], "requests": [("m", "")]},
    {"id": "refuse-unsized", "files": [g("m.gguf", GIB)],
     "backends": [gpu(name="synthetic-unsized", vram=0.0), gpu(name="b2", vram=0.0)], "requests": [("m", "")]},
    {"id": "refuse-ram", "files": [g("m.gguf", 200 * GIB)], "backends": [gpu(vram=8.0, ram=8.5)],
     "requests": [("m", "")]},
    {"id": "refuse-block-count", "files": [g("m.gguf", GIB, layers=5000)], "backends": [gpu()],
     "requests": [("m", "")]},
    {"id": "refuse-kv", "files": [g("m.gguf", GIB, embed=0, kv=None), g("n.gguf", GIB, layers=0)],
     "backends": [gpu()], "requests": [("m", ""), ("n", "")]},
]

CASES += [
    {"id": "system-routing-role",
     "files": [g("r1.gguf", GIB), g(f"{ROLE}-x.gguf", GIB), g(f"{ROLE}_v1/z.gguf", GIB), g(f"{ROLE}-chat.gguf", GIB),
               g("plain.gguf", GIB)],
     "ini": f"[r1]\nctx-size = 4096\n\n[{ROLE}-x]\nctx-size = 1\n\n[z]\nmodel = /models/{ROLE}_v1/z.gguf\n\n"
            f"[{ROLE}-chat]\nctx-size = 1\n\n[plain]\nm = /models/{ROLE.upper()}.v2/w.gguf\n\n[*]\nctx-size = 1\n",
     # A repeated key: the last value wins.
     "roles": '{"r1": " ' + ROLE.title() + ' ", "' + ROLE + '-chat": "chat", "plain": "", "r1": "' + ROLE.upper() + '"}',
     "backends": [gpu()],
     "requests": [("r1", ""), (f"{ROLE}-x", ""), ("z", ""), (f"{ROLE}-chat", ""), ("plain", ""), (ROLE.upper() + "-X", ""),
                  (f"{ROLE}-x", "?sessions=x")]},
    {"id": "roles-file-unusable", "files": [g(f"{ROLE}-y.gguf", GIB)], "ini": f"[{ROLE}-y]\nctx-size = 1\n",
     "roles": "[1, 2]", "backends": [gpu()], "requests": [(f"{ROLE}-y", "")]},
    {"id": "roles-file-not-json", "files": [g(f"{ROLE}-y.gguf", GIB)], "ini": f"[{ROLE}-y]\nctx-size = 1\n",
     "roles": "{not json", "backends": [gpu()], "requests": [(f"{ROLE}-y", "")]},
    {"id": "no-file", "files": [f("other.txt"), g("dir/inner.gguf", GIB)],
     "ini": "[ghost]\nmodel = /models/nowhere.gguf\n\n[folder]\nmodel = /models/dir\n",
     "backends": [gpu()], "requests": [("missing", ""), ("ghost", ""), ("folder", ""), ("a%20b", ""), ("%C3%BC", ""), ("a%3Fb", ""), ("a%23b", ""),
                                        ("missing", "?vision=maybe")]},
    {"id": "no-models-folder", "files": None, "backends": [gpu()], "requests": [("m", "")]},
    {"id": "bad-header", "files": [f("broken.gguf", 64), f("tiny.gguf", 3), f("sub/broken2.gguf", 30)],
     "backends": [gpu()], "requests": [("broken", ""), ("tiny", ""), ("broken2", "")]},
    {"id": "companion-named-file", "files": [g("x-mmproj.gguf", GIB)], "backends": [gpu()],
     "requests": [("x-mmproj", "")]},
    {"id": "query-422", "files": [g("m.gguf", GIB)], "backends": [gpu()],
     "requests": [("m", "?sessions=x"), ("m", "?vision=maybe"), ("m", "?verified_ctx=1.5"),
                  ("m", "?prompt_budget_s=abc"), ("m", "?budget_gib="), ("m", "?sessions=&vision="),
                  ("m", "?budget_gib=x&sessions=y&vision=z&verified_ctx=2.&prompt_budget_s=e"),
                  ("missing", "?sessions=x")]},
    {"id": "telemetry-fails", "files": [g("m.gguf", 4 * GIB)], "backends": [gpu()], "measured": "raise",
     "requests": [("m", "")]},
    {"id": "backend-list-raises", "files": [g("m.gguf", 4 * GIB)], "backends": "raise", "requests": [("m", "")]},
    {"id": "budget-and-cards",
     "files": [g("m.gguf", 14 * GIB, layers=48, embed=5120)],
     "backends": [gpu(name="pair", vram=24.0, cards=[12.0, 12.0], count=2), gpu(name="odd", vram=20.0, cards=[16.0, 4.0], count=2),
                  gpu(name="pair", vram=8.0, cards=[], count=1), gpu(name="zero-cards", vram=10.0, cards=[0.0, 0.0], count=2)],
     "cache_cap": 512,
     "requests": [("m", ""), ("m", "?budget_gib=16"), ("m", "?budget_gib=0.25"), ("m", "?budget_gib=-1")]},
    {"id": "baseline-args", "files": [g("m.gguf", 4 * GIB)],
     "backends": [gpu(args=["--ctx-size", "8192", "-fa", "on", "--jinja", "-ngl", "999", "--parallel", "1",
                           "--cache-type-k", "q8_0", "--no-mmap", "-t", "8"])],
     "requests": [("m", ""), ("m", "?sessions=2")]},
    {"id": "measured-time-cap", "files": [g("m.gguf", 2 * GIB, ctx=262144)], "backends": [gpu(vram=48.0)],
     "measured": {**MEASURED, "prompt_p50": 123.456}, "requests": [("m", ""), ("m", "?prompt_budget_s=3600")]},
    {"id": "no-template-mtp-builtin",
     "files": [g("nt.gguf", 3 * GIB, template=False, extra={"synth.nextn_predict_layers": ["u32", 1]})],
     "backends": [gpu()], "requests": [("nt", ""), ("nt", "?spec=coding"), ("nt", "?mode=chat")]},
    {"id": "default-section-and-preamble", "files": [g("d.gguf", 2 * GIB)],
     "ini": "version = 1\n[DEFAULT]\ncache-ram = 256\n\n[d]\nctx-size = 4096\n", "backends": [gpu()],
     "requests": [("d", ""), ("DEFAULT", "")]},
    {"id": "odd-names", "files": [g("a b.gguf", GIB), g("q.gguf", GIB)], "backends": [gpu()],
     "requests": [("a%20b", ""), ("q", "")]},
]


def build(root: Path, case: dict) -> Path:
    models = root / "models"
    if case["files"] is None:
        return models
    models.mkdir()
    for spec in case["files"]:
        path = models / spec["name"]
        path.parent.mkdir(parents=True, exist_ok=True)
        data = build_gguf(spec["gguf"]) if "gguf" in spec else b""
        if data:
            spec["header"] = data.hex()  # the native test writes these exact bytes
        with open(path, "wb") as handle:
            handle.write(data)
            size = spec.get("size")
            if size is not None and size > len(data):
                handle.truncate(size)
    if case.get("ini") is not None:
        (models / "models.ini").write_text(case["ini"], encoding="utf-8")
    if case.get("roles") is not None:
        (models / "models.roles.json").write_text(case["roles"], encoding="utf-8")
    return models


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith(("NOEVIA_", "GGUF_", "MODEL_", "LLAMACPP_")) or key.upper() in {
                "DATA_DIR", "MODELS_DIR", "MODELS_INI_PATH", "TZ"} or "PROXY" in key.upper():
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import autoconfig, config, helpers, services, telemetry  # noqa: E402
    from app.main import app  # noqa: E402

    settings = config.settings
    settings.model_loader_token = TOKEN
    client = TestClient(app, raise_server_exceptions=False)  # no lifespan: no sampler, no startup
    headers = {"x-model-loader-token": TOKEN}
    names = ("api.py", "helpers.py", "ini.py", "autoconfig.py", "autoconfig_core.py", "gguf_meta.py",
             "telemetry.py", "utils.py", "config.py", "main.py", "discover.py")
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest() for name in names}
    default_cap = settings.llamacpp_autoconfig_cache_ram_max_mib
    state: dict = {}

    def ingest(_names, **_):
        return 0

    def stats_for(*, model_path="", alias="", **_):
        state["asked"] = {"section": alias, "model": model_path}
        if state["case"].get("measured") == "raise":
            raise RuntimeError("synthetic telemetry failure")
        return telemetry.Stats(**state["case"].get("measured", MEASURED))

    def config_history(**_):
        return [telemetry.ConfigRun(**h) for h in state["case"].get("history", HISTORY)]

    def backend_list():
        backends = state["case"]["backends"]
        if backends == "raise":
            raise RuntimeError("synthetic discovery failure")
        out = []
        for b in backends:
            b = json.loads(json.dumps(b))
            b["baseline"] = autoconfig.parse_baseline(b["baseline_args"]) if b["baseline_args"] else {}
            out.append(b)
        return out

    services._effective_container_names = lambda: ["synthetic-gpu"]
    telemetry.ingest = ingest
    telemetry.stats_for = stats_for
    telemetry.config_history = config_history
    helpers._backend_list = backend_list

    out = []
    for case in CASES:
        state["case"] = case
        with tempfile.TemporaryDirectory() as root:
            settings.data_dir = Path(root)
            models = build(Path(root), case)
            settings.models_dir = models
            settings.models_ini_path = models / "models.ini"
            settings.llamacpp_autoconfig_cache_ram_max_mib = case.get("cache_cap", default_cap)
            results = []
            for name, query in case["requests"]:
                os.environ.pop(ENV, None)
                state.pop("asked", None)
                path = f"/api/v1/sections/{name}/autoconfig{query}"
                response = client.get(path, headers=headers)
                runtime = None
                asked = state.pop("asked", None)
                if asked is not None:
                    os.environ[ENV] = "1"
                    child = client.post("/api/v1/internal/section-autoconfig-runtime", content=json.dumps(asked),
                                        headers={**headers, "content-type": "application/json"})
                    os.environ.pop(ENV, None)
                    runtime = {"request": asked, "status": child.status_code, "body": child.content.decode("utf-8")}
                results.append({"path": path, "status": response.status_code,
                                "content_type": response.headers.get("content-type"),
                                "body": response.content.decode("utf-8"), "runtime": runtime})
            out.append({**case, "cache_cap": case.get("cache_cap", default_cap), "results": results})
    settings.llamacpp_autoconfig_cache_ram_max_mib = default_cap
    json.dump({"oracle": digest, "cases": out}, sys.stdout, ensure_ascii=True, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
