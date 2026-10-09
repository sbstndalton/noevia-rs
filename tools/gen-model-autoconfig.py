#!/usr/bin/env python3
"""Generate crates/model-autoconfig's differential fixtures (MODEL_AUTOCONFIG, autoconfig slice 1).

The reference is noevia-services' model-manager/app/autoconfig_core.py (`size_plan`, the size
core of autoconfig: fit sweep, pick, context cap, presets, prompt cache). It imports only the
stdlib, so it runs here without the service's dependencies:

    uv run --python 3.12 tools/gen-model-autoconfig.py --model-manager <noevia-services>/model-manager

Writes crates/model-autoconfig/tests/fixtures/model-autoconfig.v1.json. The file is copied
verbatim into noevia-services (model-manager/tests/fixtures/), where CI checks the two are
byte-identical. Every input is synthetic: invented model shapes and backends, never a real
model file, never a model run.

Each case holds the request as JSON text and the reference's answer: the plan, or
{"error": "<Python exception type>"} where the reference raises. Floats are written by
json.dumps (repr: the shortest string that reads back as the same float), so the file does not
depend on the Python version or locale; the expectations depend only on IEEE double arithmetic
and round()'s documented semantics.
"""
from __future__ import annotations

import argparse
import json
import random
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "crates" / "model-autoconfig" / "tests" / "fixtures" / "model-autoconfig.v1.json"
SERVICE_PYTHON = (3, 12)
SEED = 20261008
RANDOM_CASES = 160

PRESETS = ["", "fast", "balanced", "long-ctx"]


def shape_of(core, r: random.Random, layers: int) -> dict | None:
    """A kv_shape() from invented GGUF-like metadata."""
    kind = r.choice(["dense", "dense", "gemma-old", "swa", "swa", "hybrid", "kv-len", "shared"])
    heads = r.choice([8, 16, 32, 40, 64])
    embed = r.choice([2048, 3072, 4096, 5120, 8192])
    kv_heads = r.choice([1, 2, 4, 8, heads, True])
    kw: dict = {}
    arch = r.choice(["llama", "qwen3", "mistral", ""])
    if kind == "gemma-old":
        arch = r.choice(["gemma2", "gemma3", "Gemma"])
    if kind == "kv-len":
        kw.update(key_length=r.choice([128, 256, 512]), value_length=r.choice([128, 256, 512]))
    if kind == "hybrid":
        kw.update(full_attention_interval=r.choice([2, 3, 4, 8]), ssm_state_size=r.choice([0, 128]))
    if kind == "shared":
        kw.update(shared_kv_layers=r.choice([0, 1, 3, layers, layers + 10]))
    if kind == "swa":
        arch = r.choice(["gemma4", "gemma3", "llama"])
        per = r.choice([2, 3, 6, 7])
        n = min(layers, r.choice([6, 12, 42]))
        pat = [(i % per) != per - 1 for i in range(n)]
        heads_pat = [(1 if p is False else 8) for p in pat]
        kw.update(
            sliding_window=r.choice([512, 1024, 4096, 1, True]),
            sliding_window_pattern=r.choice([
                {"_array": True, "count": layers, "sample": pat}, pat,
                {"_array": True, "count": layers + 1, "sample": pat}, None,
                [1, 1.0, True, 0, "x", None, [], {}][: r.randint(0, 8)]]),
            key_length_swa=r.choice([None, 128, 256]),
            value_length_swa=r.choice([None, 256]),
            shared_kv_layers=r.choice([None, 0, 4, 18, 1000]),
            kv_heads_pattern=r.choice([
                {"_array": True, "count": layers, "sample": heads_pat},
                heads_pat, None, ["8", " 2 ", "x", None, 0, -3, 2.9, 1e30][: r.randint(0, 8)]]),
        )
    return core.kv_shape(arch, layers, kv_heads, embed // heads, **kw)


def backend(r: random.Random, multi: bool) -> dict:
    gpu_count = r.choice([2, 2, 3, 4]) if multi else 1
    vram = r.choice([4.0, 8.0, 12.0, 16.0, 23.9, 24.0, 32.0, 48.0, 80.0, round(r.uniform(1, 100), 3)])
    cards: list[float] = []
    if multi and r.random() < 0.7:
        n = r.choice([gpu_count, gpu_count, gpu_count, gpu_count - 1, gpu_count + 1])
        cards = [r.choice([8.0, 11.6, 12.0, 16.0, 24.0, vram / gpu_count]) for _ in range(n)]
    if r.random() < 0.03:
        cards = [r.choice([float("inf"), float("nan"), -1.0, 0.0])] * gpu_count
    return {"vram_gb": vram if r.random() > 0.02 else float("inf"), "gpu_count": gpu_count,
            "cards": cards,
            "host_ram_gb": r.choice([0.0, 16.0, 32.0, 64.0, 125.5, 8.5, float("nan"), float("inf")]
                                    if r.random() < 0.05 else [0.0, 16.0, 32.0, 64.0, 125.5]),
            "same_as": 0}


def request(core, r: random.Random) -> dict | None:
    layers = r.choice([1, 2, 6, 12, 24, 32, 40, 48, 64, 80, r.randint(1, 160)])
    shape = shape_of(core, r, layers)
    if shape is None:
        return None
    is_moe = r.random() < 0.4
    experts = r.choice([2, 4, 8, 16, 32, 64, 128]) if is_moe else None
    backends = [backend(r, r.random() < 0.35) for _ in range(r.choice([1, 1, 1, 2, 3]))]
    # same_as: which earlier backend shares this one's name (a duplicated container name).
    names = [r.choice(["a", "b", "c"]) for _ in backends] if r.random() < 0.3 else [str(i) for i in range(len(backends))]
    for i, b in enumerate(backends):
        b["same_as"] = names.index(names[i])
    has_mmproj = r.random() < 0.25
    return {
        "shape": shape,
        "layers": layers,
        "native_ctx": r.choice([0, 4096, 8192, 32768, 40960, 131072, 262144, 1048576, 100000,
                                -5, 2**40, r.randint(1, 300000)]),
        "model_gb_raw": (int(r.choice([0.5, 2, 4.7, 9, 14, 20, 27, 40, 70, 90, 200]) * 2**30)
                         + r.randint(0, 2**20)) / (1024 ** 3),
        "moe_ratio": core._moe_ratio(experts),
        "is_moe": isinstance(experts, int) and experts > 1,
        "mmproj_vram_gb": (r.choice([0.8, 1.55, 0.3]) * 1.0 + 0.5 + r.choice([0.0, 0.2])) if has_mmproj else 0.0,
        "n_sessions": r.choice([1, 1, 1, 2, 4, 8]),
        "backends": backends,
        "preset": r.choice(PRESETS),
        "prompt_tps": r.choice([0.0, 0.0, 0.0, 500.0, 1737.4, 80.0, 0.001, 1e-300, float("nan"),
                                float("inf"), -5.0]),
        "prompt_budget_s": r.choice([120.0, 10.0, 0.5, 3600.0, 1.0] if r.random() > 0.03
                                    else [float("nan"), float("inf")]),
        "verified_ctx": r.choice([0, 0, 0, 16384, 50000, 200000, -7]),
        "cache_ram_cap_mib": r.choice([1024, 1024, 2048, 0, 16384, 10**6]),
    }


def handmade(core) -> list[tuple[str, dict]]:
    """Named edge cases, each pinning one rule."""
    dense = core.kv_shape("llama", 32, 8, 128)
    swa = core.kv_shape("gemma4", 42, 8, 256, sliding_window=512,
                        sliding_window_pattern={"_array": True, "count": 42,
                                                "sample": [True, True, True, True, True, False] * 2},
                        key_length_swa=256, value_length_swa=256, shared_kv_layers=18,
                        kv_heads_pattern={"_array": True, "count": 42, "sample": [8, 8, 8, 8, 8, 1] * 2})
    base = {"shape": dense, "layers": 32, "native_ctx": 131072, "model_gb_raw": 4.7, "moe_ratio": 0.0,
            "is_moe": False, "mmproj_vram_gb": 0.0, "n_sessions": 1,
            "backends": [{"vram_gb": 24.0, "gpu_count": 1, "cards": [], "host_ram_gb": 64.0, "same_as": 0}],
            "preset": "", "prompt_tps": 0.0, "prompt_budget_s": 120.0, "verified_ctx": 0,
            "cache_ram_cap_mib": 1024}

    def v(**over) -> dict:
        d = json.loads(json.dumps(base))
        d.update(over)
        return d
    two = [{"vram_gb": 23.9, "gpu_count": 2, "cards": [12.0, 11.9], "host_ram_gb": 125.5, "same_as": 0}]
    return [
        ("dense fits, unmeasured cap", v()),
        ("dense measured prompt rate caps by time", v(prompt_tps=500.0)),
        ("dense verified context caps", v(verified_ctx=50000)),
        ("dense long-ctx preset offloads layers", v(preset="long-ctx", model_gb_raw=20.0)),
        ("dense too big for any context", v(model_gb_raw=90.0, backends=[dict(base["backends"][0], vram_gb=4.0)])),
        ("moe needs expert offload -> fit on", v(is_moe=True, moe_ratio=0.92, model_gb_raw=60.0)),
        ("moe balanced preset", v(is_moe=True, moe_ratio=0.85, model_gb_raw=30.0, preset="balanced")),
        ("two cards, per-card split", v(backends=two, model_gb_raw=20.0)),
        ("cards shorter than gpu_count raise IndexError",
         v(backends=[dict(two[0], cards=[12.0], gpu_count=3)], model_gb_raw=30.0)),
        ("budget NaN with a rate raises ValueError", v(prompt_tps=100.0, prompt_budget_s=float("nan"))),
        ("infinite rate raises OverflowError", v(prompt_tps=float("inf"))),
        ("sliding-window gemma", v(shape=swa, layers=42, native_ctx=262144, model_gb_raw=14.0)),
        ("no headroom: no cache-ram", v(backends=[dict(base["backends"][0], host_ram_gb=8.0)])),
        ("cache-ram cap zero", v(cache_ram_cap_mib=0)),
        ("eight sessions", v(n_sessions=8)),
        ("negative native context", v(native_ctx=-5)),
        ("duplicate backend name", v(backends=[dict(base["backends"][0], vram_gb=12.0),
                                               dict(base["backends"][0], vram_gb=12.0, host_ram_gb=16.0, same_as=0)])),
        ("projector pinned to main GPU", v(mmproj_vram_gb=2.05, backends=two)),
        ("huge period head count overflows", v(shape=dict(swa, period=[[False, 1e300]] * 6), layers=42)),
    ]


def reference(core, req: dict) -> dict:
    try:
        return core.size_plan(json.loads(json.dumps(req)))
    except Exception as e:  # noqa: BLE001 - any exception is the reference refusing the input
        return {"error": type(e).__name__}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--model-manager", required=True, type=Path)
    args = ap.parse_args()
    if sys.version_info[:2] != SERVICE_PYTHON:
        sys.exit(f"run with Python {SERVICE_PYTHON[0]}.{SERVICE_PYTHON[1]} (the service's), not {sys.version.split()[0]}")
    sys.path.insert(0, str(args.model_manager.resolve()))
    from app import autoconfig_core as core

    cases = []
    for name, req in handmade(core):
        cases.append((name, req))
    r = random.Random(SEED)
    while len(cases) < RANDOM_CASES + len(handmade(core)):
        req = request(core, r)
        if req is not None:
            cases.append((f"random {len(cases)}", req))
    out = {"version": 1, "reference": "noevia-services model-manager/app/autoconfig_core.py size_plan",
           "cases": []}
    for name, req in cases:
        text = json.dumps(req, sort_keys=True)
        out["cases"].append({"name": name, "input": text, "expect": reference(core, json.loads(text))})
    errors = sum(1 for c in out["cases"] if "error" in c["expect"])
    OUT.write_text(json.dumps(out, sort_keys=True, separators=(",", ":")) + "\n")
    print(f"{len(out['cases'])} cases ({errors} where the reference raises) -> {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
