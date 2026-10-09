#!/usr/bin/env python3
"""Generate crates/model-autoconfig's differential fixtures (MODEL_AUTOCONFIG, autoconfig slices 1-6).

The reference is noevia-services' model-manager/app/autoconfig_core.py (`size_plan`, the size
core of autoconfig: fit sweep, pick, context cap, presets, prompt cache). It imports only the
stdlib, so it runs here without the service's dependencies:

    uv run --python 3.12 tools/gen-model-autoconfig.py --model-manager <noevia-services>/model-manager

Writes crates/model-autoconfig/tests/fixtures/model-autoconfig.v1.json (the size core) and
model-autoconfig-check.v1.json (slice 2: `check` with one part, input prep or values assembly;
the reference is `check_reference`) and model-autoconfig-present.v1.json (slices 3-6: `check` with
one of the parts spec, files, present or baseline). All three files are copied verbatim into noevia-services
(model-manager/tests/fixtures/), where CI checks they are byte-identical. Every input is synthetic: invented model shapes and backends, never a real
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
OUT_CHECK = OUT.with_name("model-autoconfig-check.v1.json")
OUT_PRESENT = OUT.with_name("model-autoconfig-present.v1.json")
CHECK_SEED = 20261009
CHECK_RANDOM_CASES = 400
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
        # #1159: a hybrid model's KV shape covers only its attention layers (11 of 30 here).
        ("hybrid: KV over the attention layers only", v(shape=core.kv_shape("lfm2", 11, 8, 64), layers=30,
                                                       model_gb_raw=1.5, native_ctx=32768)),
        ("hybrid: recurrent state charged", v(shape=dict(core.kv_shape("lfm2", 11, 8, 64), recurrent_bytes=19 * 4 * 2**20 * 2),
                                              layers=30, model_gb_raw=1.5, native_ctx=32768, n_sessions=2)),
        ("hybrid: long-ctx preset", v(shape=core.kv_shape("lfm2", 11, 8, 64), layers=30, model_gb_raw=1.5,
                                      native_ctx=32768, preset="long-ctx")),
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


# ---- slice 2: input prep and values assembly (`check` with one part) ----
#
# Case names starting "[stricter]" are inputs on which the port refuses (non-ASCII digits, a
# string where a backend number belongs, a NaN in a compared name, a file size past 2^53) while
# Python answers or raises: the expectation is still Python's, and the Rust test requires a
# refusal that is not a Python exception there. Random cases stay inside the reproduced domain.

ODD = [None, True, False, 2.9, -1.0, 0.0, 1e29, "32", " 8 ", "x", "", "1_0", "+4", "\x1c7\x1f", [], [8, 1],
       [None], {}, {"_array": True, "count": 8, "sample": [8, 8, 1]}, {"_array": True, "sample": "88"},
       {"_array": False, "sample": [4]}, {"_array": True, "count": "8", "sample": [1, 1.0, True]}, 0, 1, -7, 2**60]


def _pick(r: random.Random, normal: list, odd: float = 0.12):
    return r.choice(ODD) if r.random() < odd else r.choice(normal)


def prep_model(r: random.Random) -> dict:
    layers = r.choice([1, 2, 6, 12, 24, 32, 42, 48, 64, 80, r.randint(1, 160)])
    m: dict = {"block_count": _pick(r, [layers, layers, 0, 4096, 4097, -1]),
               "attention_head_count": _pick(r, [8, 16, 32, 64, 0, -4]),
               "embedding_length": _pick(r, [2048, 4096, 5120, 0]),
               "context_length": _pick(r, [0, 4096, 32768, 131072, 262144, -5, 2**40]),
               "attention_head_count_kv": _pick(r, [8, 4, 1, True, [2, 8], [None, 1], {"_array": True, "count": layers,
                                                    "sample": [r.choice([8, 1, 2, None, "4", 2.5]) for _ in range(8)]}], 0.2)}
    if r.random() < 0.4:
        m["expert_count"] = _pick(r, [2, 4, 8, 16, 32, 64, 128, True, 1])
    if r.random() < 0.3:
        m.update(key_length=_pick(r, [128, 256, True, 0]), value_length=_pick(r, [128, 512, 0]))
    if r.random() < 0.2:
        m.update(full_attention_interval=_pick(r, [0, 1, 2, 4, True]), ssm_state_size=_pick(r, [0, 128]))
    if r.random() < 0.4:
        per = r.choice([1, 2, 3, 6])
        n = r.choice([0, 3, 8])
        pat = [(i % per) != per - 1 for i in range(n)]
        m.update(sliding_window=_pick(r, [512, 1024, 1, True, 0, -3]),
                 sliding_window_pattern=_pick(r, [{"_array": True, "count": layers, "sample": pat}, pat, None,
                                                  [1, 1.0, True, 1, 1.0, True], ["a", "a", "b"] * 2, [[1], [1.0]] * 3,
                                                  [{"a": 1}, {"a": True}] * 2, {"_array": True, "count": layers, "sample": "ab" * 3}],
                                              0.2),
                 key_length_swa=_pick(r, [None, 128, 256]), value_length_swa=_pick(r, [None, 256]),
                 shared_kv_layers=_pick(r, [None, 0, 4, 18, 1000, -2]))
        if r.random() < 0.5:
            m["attention_head_count_kv"] = r.choice([
                {"_array": True, "count": layers, "sample": [r.choice([8, 1, "2", " 3 ", "x", None, 0, -3, 2.9, 1e20, [], {}])
                                                            for _ in range(8)]},
                [r.choice([8, 1, 2.0]) for _ in range(r.randint(1, 8))]])
    if r.random() < 0.1:
        m["extra"] = r.choice(ODD)   # ignored keys stay ignored
    return m


def prep_backends(r: random.Random) -> list:
    out = []
    for i in range(r.choice([1, 1, 1, 2, 3])):
        gc = r.choice([1, 1, 2, 3])
        b = {"name": r.choice(["a", "b", f"x{i}", 1, 1.0, True, None]),
             "vram_gb": r.choice([0, 0.0, 4, 8.0, 12, 23.9, 24, 48.0, None, True]),
             "gpu_count": gc, "host_ram_gb": r.choice([0, 16, 32.0, 64, 125.5, None]), "baseline": {}}
        if gc > 1 and r.random() < 0.7:
            b["card_vram_gb"] = [r.choice([8, 12, 11.6, 16.0, True]) for _ in range(r.choice([gc, gc - 1, gc + 1]))]
        if r.random() < 0.06:
            b[r.choice(["gpu_count", "card_vram_gb", "host_ram_gb"])] = r.choice([None, "x", [], {"k": 1}, "", 0, 2.5, [1, "2"]])
        if r.random() < 0.03:
            del b["name"]
        out.append(b)
    if r.random() < 0.03:
        out = r.choice([None, [], ["x"], [1], {"a": 1}, "ab", 3])
    return out


def prep_case(r: random.Random) -> dict:
    return {"n_sessions": r.choice([1, 1, 2, 4, 8, 9, 0, None, True, 2.5]),
            "arch": _pick(r, ["llama", "gemma3", "Gemma2", "GEMMA4", "qwen3", ""], 0.05),
            "model": r.choice([prep_model(r)] * 30 + [None, {}, [], "x"]),
            "file_size": r.choice([int(r.choice([0.5, 2, 4.7, 9, 20, 70, 300]) * 2**30) + r.randint(0, 2**20),
                                   0, 2**53, 1.5e10, True, None, "10", [], -2**30]),
            "backends": prep_backends(r),
            "projector": {"has_mmproj": r.random() < 0.3, "mmproj_gb": r.choice([0.0, 0.78, 1.55, 2]),
                          "mtp_gb": r.choice([0.0, 0.0, 0.12])}}


FEATURE_KEYS = ("accepts_enable_thinking", "accepts_reasoning_effort", "uses_think_tags", "uses_channel_thought",
                "accepts_preserve_thinking")
SPEC = [[], [["spec-type", ""], ["spec-draft-n-max", ""], ["spec-draft-n-min", ""], ["spec-draft-p-min", ""],
             ["spec-draft-model", ""], ["spec-draft-ngl", ""]],
        [["spec-type", "draft-mtp"], ["spec-draft-n-max", "4"], ["spec-draft-n-min", ""], ["spec-draft-p-min", "0.25"],
         ["spec-draft-model", "/models/m/mtp.gguf"], ["spec-draft-ngl", "999"]],
        [["spec-type", "ngram-simple"], ["ngl", "12"], ["batch-size", "64"]]]


def values_case(r: random.Random) -> dict:
    sized = r.random() < 0.8
    initial = r.choice([0, 4096, 32768, 131072, 262144])
    current = {k: r.choice(v) for k, v in {
        "mmproj": ["", " /models/m/mmproj-F16.gguf ", "/models/m/mmproj.gguf", "\u3000"],
        "ubatch-size": ["", "512", "2048", " 4096 ", "1_024", "x", "-5", "+3000", "1.5", "\x1c8000"],
        "batch-size": ["", "256", "1024", "8192", "x", "0"]}.items() if r.random() < 0.4}
    features = r.choice([None, {}, {k: r.choice([True, False, 0, 1, "", "x", None]) for k in FEATURE_KEYS if r.random() < 0.6}]
                        + ([[1], "x", 3] if r.random() < 0.05 else []))
    return {"model_rel": r.choice(["", "/models/m/m-Q4_K_M.gguf"]), "n_sessions": r.choice([1, 1, 2, 4, 8]),
            "chat_template": r.random() < 0.8, "features": features,
            "section": r.choice(["", "m"]), "vision": r.random() < 0.85, "current": current,
            "mmproj_rel": r.choice(["", "", "/models/m/mmproj-F16.gguf", "(remote projector)"]),
            "has_mmproj": r.random() < 0.4, "spec": r.choice(SPEC),
            "plan": {"initial_ctx": initial, "sized": sized, "ctx": r.choice([initial, 4096, 65536, 2**40]) if sized else initial,
                     "ngl": r.choice([None, None, 12, 0, 999]), "fit": r.random() < 0.3,
                     "cache_ram": r.choice([None, 1024, 8192])},
            "rope": r.choice([{}, {"arch": r.choice(["gemma3", "GEMMA3n", "gemma2", "llama", 3, True, "", None, " gemma3"]),
                                   "rope_scaling_type": r.choice([None, "", "none", " NONE ", "\u3000none\x1f", "linear", "yarn",
                                                                  0, 1, True, [], ["none"], "n\u00f3ne"]),
                                   "rope_scaling_factor": r.choice([None, 0, 0.0, 4.0, -1.0, 2, True, False, "2"])}]),
            "native_ctx": r.choice([0, 4096, 8192, 32768, 131072, -5, 3, 7]),
            "rec_gpu_count": r.choice([None, 1, 1, 2, 4])}


def check_handmade() -> list[tuple[str, dict]]:
    good_model = {"block_count": 32, "attention_head_count": 32, "embedding_length": 4096,
                  "attention_head_count_kv": 8, "context_length": 131072}
    one = [{"name": "llama-cuda", "vram_gb": 24.0, "gpu_count": 1, "host_ram_gb": 64.0, "baseline": {}}]
    base = {"n_sessions": 1, "arch": "llama", "model": good_model, "file_size": int(4.7 * 2**30),
            "backends": one, "projector": {"has_mmproj": False, "mmproj_gb": 0.0, "mtp_gb": 0.0}}

    def p(**over) -> dict:
        d = json.loads(json.dumps(base))
        d.update(over)
        return {"prep": d}

    def mm(**over) -> dict:
        d = dict(good_model)
        d.update(over)
        return d
    gemma4 = mm(block_count=42, attention_head_count=16, embedding_length=4096, sliding_window=512,
                sliding_window_pattern={"_array": True, "count": 42, "sample": [True] * 5 + [False] + [True, True]},
                key_length=512, value_length=512, key_length_swa=256, value_length_swa=256, shared_kv_layers=18,
                attention_head_count_kv={"_array": True, "count": 42, "sample": [8] * 5 + [1] + [8, 8]})
    v_base = {"model_rel": "/models/m/m.gguf", "n_sessions": 1, "chat_template": True, "features": {},
              "section": "m", "vision": True, "current": {}, "mmproj_rel": "", "has_mmproj": False, "spec": [],
              "plan": {"initial_ctx": 32768, "sized": True, "ctx": 32768, "ngl": None, "fit": False, "cache_ram": 8192},
              "rope": {}, "native_ctx": 131072, "rec_gpu_count": 1}

    def v(**over) -> dict:
        d = json.loads(json.dumps(v_base))
        d.update(over)
        return {"values": d}
    return [
        ("prep: dense model", p()),
        ("prep: implausible block count refuses", p(model=mm(block_count=5000))),
        ("prep: negative block count refuses", p(model=mm(block_count=-1))),
        ("prep: no backends refuses", p(backends=[])),
        ("prep: null backends refuses", p(backends=None)),
        ("prep: larger than VRAM plus RAM refuses", p(file_size=200 * 2**30)),
        ("prep: no backend reports VRAM", p(backends=[dict(one[0], vram_gb=0), {"vram_gb": None}, {"name": 7, "vram_gb": 0.0}])),
        ("prep: unsized KV refuses with the missing fields", p(model={"block_count": 32})),
        ("prep: zero kv heads", p(model=mm(attention_head_count_kv=0))),
        ("prep: bool kv heads stays a bool", p(model=mm(attention_head_count_kv=True))),
        ("prep: kv heads sample ties keep the first", p(model=mm(attention_head_count_kv={"_array": True, "sample": [1, 8, 8, 1]}))),
        ("prep: kv heads sample of a string", p(model=mm(attention_head_count_kv={"_array": True, "sample": "8"}))),
        ("prep: kv heads sample of a dict iterates keys", p(model=mm(attention_head_count_kv={"_array": True, "sample": {"4": 0, "2": 0}}))),
        ("prep: kv heads sample of a number raises", p(model=mm(attention_head_count_kv={"_array": True, "sample": 3}))),
        ("prep: kv heads sample element unparsable raises", p(model=mm(attention_head_count_kv={"_array": True, "sample": ["x"]}))),
        ("prep: kv heads list", p(model=mm(attention_head_count_kv=[4, 8]))),
        # #1159: llama.cpp reads head_count_kv per layer; absent means head_count (MHA), and a 0
        # entry is a layer with no KV cache (hybrid LFM2 / Jamba). Only attention layers are sized.
        ("prep: absent kv heads is MHA", p(model={k: v for k, v in good_model.items() if k != "attention_head_count_kv"})),
        ("prep: absent kv and head count", p(model={"block_count": 32, "embedding_length": 4096, "context_length": 8192})),
        ("prep: zero head count, absent kv", p(model={"block_count": 32, "attention_head_count": 0, "embedding_length": 4096})),
        ("prep: GQA scalar kv heads", p(model=mm(attention_head_count_kv=4))),
        ("prep: MHA per-layer kv heads", p(model=mm(block_count=4, attention_head_count_kv=[32, 32, 32, 32]))),
        ("prep: hybrid per-layer kv heads, fully known", p(arch="lfm2", model=mm(block_count=6, attention_head_count_kv=[0, 0, 8, 0, 0, 8]))),
        ("prep: hybrid per-layer kv heads, mixed counts", p(arch="lfm2", model=mm(block_count=6, attention_head_count_kv=[0, 4, 0, 8, 0, 2]))),
        ("prep: hybrid per-layer kv heads, full sample dict", p(arch="lfm2", model=mm(block_count=4, attention_head_count_kv=
                                                             {"_array": True, "count": 4, "sample": [0, 8, 0, 8]}))),
        ("prep: hybrid per-layer kv heads, prefix only, refuses", p(arch="lfm2", model=mm(block_count=30, attention_head_count_kv=
                                                                 {"_array": True, "count": 30, "sample": [0, 0, 8, 0, 0, 8, 0, 0]}))),
        ("prep: hybrid per-layer kv heads, wrong length, refuses", p(arch="lfm2", model=mm(block_count=6, attention_head_count_kv=[0, 8, 0]))),
        ("prep: hybrid per-layer kv heads, huge count, refuses", p(arch="lfm2", model=mm(block_count=6, attention_head_count_kv=
                                                                {"_array": True, "count": 2**130, "sample": [0, 8]}))),
        ("prep: per-layer kv heads all zero refuses", p(arch="lfm2", model=mm(block_count=4, attention_head_count_kv=[0, 0, 0, 0]))),
        ("prep: per-layer kv heads negative", p(arch="lfm2", model=mm(block_count=3, attention_head_count_kv=[0, -2, -1]))),
        ("prep: hybrid per-layer kv heads with a bool stays as before", p(model=mm(block_count=3, attention_head_count_kv=[0, True, 8]))),
        ("prep: hybrid per-layer kv heads with a float stays as before", p(model=mm(block_count=3, attention_head_count_kv=[8.0, 0, 8]))),
        ("prep: hybrid per-layer dict, _array not true stays as before", p(model=mm(attention_head_count_kv=
                                                                       {"_array": 1, "count": 32, "sample": [8, 0]}))),
        ("prep: hybrid per-layer kv heads under interleaved attention stays as before", p(model=mm(
            block_count=4, full_attention_interval=2, attention_head_count_kv=[0, 8, 0, 8]))),
        ("prep: hybrid per-layer kv heads under a sliding window stays as before", p(model=mm(
            block_count=4, sliding_window=512, attention_head_count_kv=[0, 8, 0, 8]))),
        ("prep: hybrid per-layer kv heads with a zero sliding window", p(model=mm(
            block_count=4, sliding_window=0, attention_head_count_kv=[8, 0, 8, 0]))),
        # Review of #1159: the non-attention layers' recurrent state is charged per session, and a
        # shared-KV declaration on top of a per-layer hybrid array refuses.
        ("prep: granite-4.0-h shaped hybrid charges SSM state", p(arch="granitehybrid", model=mm(
            block_count=40, ssm_state_size=128, attention_head_count_kv=[0] * 5 + [8] + [0] * 9 + [8] + [0] * 9 + [8]
            + [0] * 9 + [8] + [0] * 5))),
        ("prep: hybrid recurrent state scales with sessions", p(arch="lfm2", n_sessions=4, model=mm(
            block_count=6, attention_head_count_kv=[0, 0, 8, 0, 0, 8]))),
        ("prep: hybrid per-layer kv heads with shared KV refuses", p(arch="lfm2", model=mm(
            block_count=6, shared_kv_layers=2, attention_head_count_kv=[0, 0, 8, 0, 0, 8]))),
        ("prep: hybrid per-layer kv heads with zero shared KV", p(arch="lfm2", model=mm(
            block_count=6, shared_kv_layers=0, attention_head_count_kv=[0, 0, 8, 0, 0, 8]))),
        ("prep: hybrid refusal comes after the backend refusals", p(backends=[], model=mm(attention_head_count_kv=
                                                                  {"_array": True, "count": 32, "sample": [0, 8]}))),
        ("prep: string ints are read as Python reads them", p(model=mm(block_count=" 32 ", attention_head_count="+3_2",
                                                                       context_length="\x1c8192\x1f"))),
        ("prep: an unparsable block count raises", p(model=mm(block_count="thirty"))),
        ("prep: a float block count truncates", p(model=mm(block_count=31.9))),
        ("prep: a list block count raises", p(model=mm(block_count=[32]))),
        ("prep: non-string arch raises", p(arch=5)),
        ("prep: non-dict model raises", p(model=["x"])),
        ("prep: gemma by prefix, any case", p(arch="GeMMa2", model=mm(block_count=12))),
        ("prep: hybrid attention", p(model=mm(full_attention_interval=4, ssm_state_size=128))),
        ("prep: declared sliding window, gemma-4 like", p(arch="gemma4", model=gemma4, file_size=14 * 2**30)),
        ("prep: sliding-window period with 1 == 1.0 == True", p(arch="gemma4", model=dict(gemma4, sliding_window_pattern=
                                                                [1, 1.0, True, 0, 1, 1.0, True, 0], block_count=8))),
        ("prep: sliding-window pattern of lists and dicts", p(arch="gemma4", model=dict(gemma4, block_count=6, sliding_window_pattern=
                                                              [[1], {"a": 1}, [1.0], {"a": True}, [True], {"a": 1.0}]))),
        ("prep: per-layer heads odd values fall back", p(arch="gemma4", model=dict(gemma4, attention_head_count_kv=
                                                         {"_array": True, "count": 42, "sample": [None, " 3 ", 2.9, -4, 1e20, 8, 1, 8]}))),
        ("prep: per-layer heads odd values raise in the head count", p(arch="gemma4", model=dict(gemma4, attention_head_count_kv=
                                                         {"_array": True, "count": 42, "sample": ["x", None, [], {}]}))),
        ("prep: per-layer heads infinite raises", p(arch="gemma4", model=dict(gemma4, attention_head_count_kv=
                                                    {"_array": True, "count": 42, "sample": [float("inf")] * 8}))),
        ("prep: per-layer heads NaN falls back", p(arch="gemma4", model=dict(gemma4, attention_head_count_kv=
                                                   {"_array": True, "count": 42, "sample": [float("nan")] * 8}))),
        ("prep: duplicate backend names share same_as", p(backends=[dict(one[0], name=1), dict(one[0], name=1.0, vram_gb=12.0),
                                                                    dict(one[0], name=True, gpu_count=2, card_vram_gb=[12, 12.0])])),
        ("prep: a backend without a name raises KeyError", p(backends=[{"vram_gb": 24.0}])),
        ("prep: a non-dict backend raises", p(backends=[one[0], "x"])),
        ("prep: gpu_count None raises", p(backends=[dict(one[0], gpu_count=None)])),
        ("prep: a card that is not a number raises", p(backends=[dict(one[0], gpu_count=2, card_vram_gb=[12, "12"])])),
        ("prep: projector and draft head reserve", p(projector={"has_mmproj": True, "mmproj_gb": 1.55, "mtp_gb": 0.12})),
        ("prep: integer projector size", p(projector={"has_mmproj": True, "mmproj_gb": 2, "mtp_gb": 0.0})),
        ("prep: n_sessions clamped", p(n_sessions=12)),
        ("prep: file size a float", p(file_size=5.5e9)),
        ("prep: file size a string raises", p(file_size="5")),
        ("[stricter] prep: non-ASCII digits", p(model=mm(block_count="\u0663\u0662"))),
        ("[stricter] prep: a string VRAM", p(backends=[dict(one[0], vram_gb="24")])),
        ("[stricter] prep: a NaN backend name", p(backends=[dict(one[0], name=float("nan"))])),
        ("[stricter] prep: a file size past 2^53", p(file_size=2**60, backends=[dict(one[0], host_ram_gb=0)])),
        ("[stricter] prep: a block count past 2^100", p(model=mm(block_count=2**110))),
        ("[stricter] prep: a per-layer head count past 2^100", p(arch="gemma4", model=dict(gemma4, attention_head_count_kv=
                                                                 {"_array": True, "count": 42, "sample": [1e300] * 8}))),
        ("values: dense single session", v()),
        ("values: four sessions", v(n_sessions=4)),
        ("values: expert offload pops placement", v(spec=[["ngl", "4"], ["tensor-split", "1,1"]],
                                                    plan=dict(v_base["plan"], fit=True))),
        ("values: dense ngl from the preset", v(plan=dict(v_base["plan"], ngl=40))),
        ("values: unsized keeps the initial context", v(plan={"initial_ctx": 0, "sized": False, "ctx": 0, "ngl": None,
                                                              "fit": False, "cache_ram": None})),
        ("values: ctx-size appended after spec when initial is 0", v(spec=SPEC[2], plan=dict(v_base["plan"], initial_ctx=0))),
        ("values: reasoning flags", v(features={k: True for k in FEATURE_KEYS})),
        ("values: features not a dict raises", v(features=["accepts_enable_thinking"])),
        ("values: projector raises ubatch to the image bound", v(has_mmproj=True, mmproj_rel="/models/m/mmproj.gguf")),
        ("values: projector keeps a larger ubatch", v(has_mmproj=True, current={"ubatch-size": " 4_096 "})),
        ("values: projector ignores an unparsable ubatch", v(has_mmproj=True, current={"ubatch-size": "big"})),
        ("values: projector raises a small batch", v(has_mmproj=True, current={"batch-size": "512"})),
        ("values: projector leaves a multi-session batch", v(has_mmproj=True, n_sessions=2)),
        ("values: the saved projector wins", v(has_mmproj=True, current={"mmproj": " /models/m/mine.gguf "},
                                               mmproj_rel="/models/m/other.gguf")),
        ("values: vision off drops the projector key", v(vision=False, mmproj_rel="/models/m/mmproj.gguf")),
        ("values: rope scaling past the native context", v(native_ctx=8192, plan=dict(v_base["plan"], ctx=20480))),
        ("values: rope owned by gemma3", v(native_ctx=8192, rope={"arch": "Gemma3n"})),
        ("values: rope owned by a declared type", v(native_ctx=8192, rope={"rope_scaling_type": " YaRN "})),
        ("values: rope type none is not owned", v(native_ctx=8192, rope={"rope_scaling_type": "\u3000NONE\x1c"})),
        ("values: rope owned by a factor", v(native_ctx=8192, rope={"rope_scaling_factor": 2})),
        ("values: rope not dict raises", v(native_ctx=8192, rope=["x"])),
        ("values: split mode on several GPUs", v(rec_gpu_count=2)),
        ("[stricter] values: non-ASCII digits in a saved ubatch", v(has_mmproj=True, current={"ubatch-size": "\u0664\u0660"})),
    ]


def check_cases() -> list[tuple[str, dict]]:
    cases = check_handmade()
    r = random.Random(CHECK_SEED)
    for i in range(CHECK_RANDOM_CASES):
        if i % 2 == 0:
            cases.append((f"random prep {i}", {"prep": prep_case(r)}))
        else:
            cases.append((f"random values {i}", {"values": values_case(r)}))
    return cases


def check_reference(core, parts: dict) -> dict:
    try:
        return core.check_reference(json.loads(json.dumps(parts)))
    except Exception as e:  # noqa: BLE001 - any exception is the reference refusing the input
        return {"error": type(e).__name__}


# ---- slices 3-6: speculative profiles, companion files, presentation, baseline (`check`) ----
#
# Written to model-autoconfig-present.v1.json, one part per case. Case names starting
# "[stricter]" are inputs the port refuses on purpose (a non-ASCII section or model file name,
# repr() past U+024F, a case-insensitive comparison of two non-ASCII strings, an unsorted
# known-key list, a non-string saved value) while Python answers.

PRESENT_SEED = 20261010
PRESENT_RANDOM_CASES = 1600

SPEC_STRS = ["", " ", "none", "None", "draft-mtp", "draft-mtp,ngram-simple", "ngram-simple", " draft-mtp ",
             "\u3000ngram-simple", "4", "8", "2", "1", "0.25", "0.05", "0.60", " 4 ", "999", "12",
             "/models/m/mtp-Q8_0.gguf", " /models/m/mtp.gguf "]
SPEC_PROFILE_KNOBS = [("draft-mtp", {"spec-draft-n-max": "4", "spec-draft-p-min": "0.25"}),
                      ("draft-mtp,ngram-simple", {"spec-draft-n-max": "8", "spec-draft-n-min": "1",
                                                  "spec-draft-p-min": "0.05"}),
                      ("draft-mtp", {"spec-draft-n-max": "2", "spec-draft-p-min": "0.60"}),
                      ("ngram-simple", {}), ("none", {}), ("", {})]
SPEC_SECTION = ("spec-type", "spec-draft-model", "spec-draft-ngl", "spec-draft-n-max", "spec-draft-n-min",
                "spec-draft-p-min")


def spec_case(r: random.Random) -> dict:
    current = None
    if r.random() < 0.75:
        current = {}
        if r.random() < 0.6:
            stype, knobs = r.choice(SPEC_PROFILE_KNOBS)
            current["spec-type"] = stype
            current.update(knobs)
        for k in r.sample(SPEC_SECTION, r.randint(0, 3)):
            current[k] = r.choice(SPEC_STRS)
        if r.random() < 0.03:
            current[r.choice(SPEC_SECTION)] = r.choice([None, 1, True, []])
    return {"section": r.choice(["", "m", "Qwen-27B-Q4"]), "current": current,
            "spec_profile": r.choice(["", "", None, "off", "balanced", "coding", "writing", "ngram", "custom",
                                      "bogus", " coding ", "\u3000ngram", "Custom"]),
            "mode": r.choice(["", "chat", "code", "agent", "writing", "x", None, 3] + ([[1]] if r.random() < 0.03 else [])),
            "files": r.random() < 0.6, "found_mtp": r.choice(["", "", "/models/m/MTP/mtp-Q4_0.gguf"]),
            "nextn": r.choice([None, None, 0, 1, 2, "1", " 2 ", True, False, 2.5, -1, "0"]
                              + (["x", []] if r.random() < 0.05 else []))}


FILE_NAMES = ["m-Q4_K_M.gguf", "m-Q4_K_M-mmproj-F16.gguf", "mmproj-BF16.gguf", "mmproj-F32.gguf", "MMPROJ.GGUF",
              "mmproj", "x.mmproj.gguf.txt", "mQ4KM-MTP-Q8_0.gguf", "m-Q4_K_M-draft-Q4_0.gguf", "m.draft.gguf",
              "draft-m.gguf", "m-noMTP-x.gguf", "nodraft-draft-.gguf", "x.Gguf", ".gguf", "x.", "a.GGUF", "mtp",
              "mtp.gguf", "draft.gguf", "x-mtp.gguf", "MTP-x.gguf", "a_mtp_b.gguf", "a.mtp.b.gguf", "-draft-",
              "\u0130-draft-.gguf", "K\u212a-mtp.gguf", "\u00fc-mtp-.gguf", "m\u00f6del-mmproj.gguf", "mm\u0130proj.gguf",
              "m-Q4_K_M", "M-Q4-K-M-mtp.gguf", "big-mtp-q4.gguf", "s_mtp_.gguf", "s.mtp.gguf", "mmproj-s.gguf",
              "m.q4.k.m-mmproj.gguf", "no_mtp-x.gguf", "x-draft", "draft"]
SIZES = [0, 1, 7, 60 * 2**20, 100 * 2**20, 2**31, 2**31 + 1, 3 * 2**30, 60 * 2**20]
SECTIONS = ["m-Q4_K_M", "m", "mQ4KM", "M-Q4_K_M", "x", "s", "big-mtp-q4", "a.b", "-", "", "mtp", "draft-m"]


def listing(r: random.Random) -> list | None:
    if r.random() < 0.06:
        return None
    out = []
    for name in sorted(r.sample(FILE_NAMES, r.randint(0, 9))):
        kind = r.choices(["file", "other", "error"], [0.85, 0.12, 0.03])[0]
        size = (r.choice(SIZES) if r.random() > 0.03 else None) if kind == "file" else None
        out.append([name, kind, size])
    return out


def files_call(r: random.Random) -> dict:
    rule = r.choice(["mmproj_subdir", "mmproj_flat", "mtp_folder", "mtp_flat", "projector"])
    if rule == "projector":
        return {"rule": rule, "files": r.random() < 0.7,
                "current": r.choice(["", "", " ", "/models/m/mine.gguf", " /models/m/x.gguf ", "\u3000"]),
                "found": r.choice(["", "/models/m/mmproj-F16.gguf"]), "stat_gb": r.choice([0.0, 0.78, 1.5500001, 0.0001]),
                "vision": r.random() < 0.8,
                "override": r.choice([None, None, 0, 0.0, 0.8, 2, True, False, -1.0, float("nan"), float("inf")]
                                     + (["x"] if r.random() < 0.05 else []))}
    req = {"rule": rule, "listing": listing(r)}
    if rule == "mmproj_subdir":
        req["subdir"] = r.choice(["m", "a/b", "Big-MTP-Q4"])
    else:
        req["section"] = r.choice(SECTIONS)
    if rule == "mtp_folder":
        req["prefix"] = r.choice(["/models/m/", "/models/m/MTP/"])
    return req


BASE_VALUES = [("model", ["/models/m/m-Q4_K_M.gguf"]), ("ctx-size", ["32768", "65536", "8192"]),
               ("parallel", ["1", "2", "4"]), ("cont-batching", ["on"]), ("keep", ["256"]), ("batch-size", ["4096", "1024"]),
               ("ngl", ["999", "40"]), ("flash-attn", ["on", "ON"]), ("cache-type-k", ["q8_0"]), ("cache-type-v", ["q8_0"]),
               ("cache-reuse", ["1"]), ("jinja", ["true"]), ("mmproj", ["/models/m/mmproj-F16.gguf"]),
               ("spec-type", ["", "draft-mtp"]), ("spec-draft-n-max", ["", "4"]), ("spec-draft-model", ["", "/models/m/h.gguf"]),
               ("fit", ["on"]), ("cache-ram", ["8192", "1024"]), ("reasoning", ["on"]), ("chat-template-kwargs", ['{"reasoning_effort": "medium"}']),
               ("mmproj-offload", ["on"]), ("image-max-tokens", ["1024"]), ("ubatch-size", ["1024", "4096"]),
               ("rope-scaling", ["linear"]), ("rope-scale", ["2.5", "4.0"]), ("split-mode", ["layer"]),
               ("lora", ["/x.gguf"]), ("threads", ["8"])]
CUR_STRS = ["", " ", "0", "1", "8", "999", " 999 ", "30", "4", "on", "true", "TRUE", "On", "1_024", " 32768 ", "32768",
            "65536", "x", "-5", "q8_0", "Q8_0", "it's", 'say "hi"', "both'\"", "back\\slash", "tab\there", "nl\nx",
            "\x7f", "\x01", "\u00fc", "\u0130", "/models/m/mmproj-F16.gguf", "none"]
CUR_KEYS = [k for k, _ in BASE_VALUES] + ["n-cpu-moe", "cpu-moe", "tensor-split", "reasoning-format", "spec-draft-ngl",
                                          "spec-draft-p-min", "rope-scale", "chat-template-file", "context-shift"]
QUANT_NAMES = ["m-Q4_K_M.gguf", "m-Q2_K.gguf", "x-UD-IQ2_XS.gguf", "q-iq3_m-c", "Q3", "a.Q1_0.gguf", "IQ4_NL",
               "b_q2_k_s", "c-UD-Q3_K_XL-00001-of-00002.gguf", "Q2__K", "IQ3xx", "aQ2_K", "m-Q3\n", "F16", ""]


def present_case(r: random.Random) -> dict:
    values = []
    for k, opts in r.sample(BASE_VALUES, r.randint(1, 14)):
        values.append([k, r.choice(opts)])
    if r.random() < 0.05:
        values.append([r.choice(["weird-key", "zz-top", "a"]), "1"])
    current = None
    if r.random() < 0.7:
        current = {k: r.choice(CUR_STRS) for k in r.sample(CUR_KEYS, r.randint(0, 10))}
        if r.random() < 0.3:
            current["ctx-size"] = str(r.choice([8192, 32768, 65536, 131072]) * r.choice([1, 2]))
    recommended = r.random() < 0.85
    rec_backend = None
    if recommended:
        rec_backend = {}
        if r.random() < 0.8:
            rec_backend["baseline"] = r.choice([{}, None, {k: r.choice(CUR_STRS + [None]) for k in r.sample(CUR_KEYS, r.randint(0, 5))}])
        if r.random() < 0.8:
            rec_backend["gpu_count"] = r.choice([1, 1, 2, 3, True] + (["2", 2.7] if r.random() < 0.1 else []))
    presets = []
    if recommended:
        for key in r.sample(["fast", "balanced", "long-ctx"], r.randint(0, 3)):
            presets.append({"key": key, "ctx": r.choice([8192, 32768, 65536, 16384]),
                            "offload_kind": r.choice(["none", "ngl", "n-cpu-moe", "cpu-moe"]),
                            "ngl": r.choice([999, 30, 0]), "n_cpu_moe": r.choice([0, 4, 6])})
    feats = r.choice([None, {}, {k: r.choice([True, False, 0, 1, "", "x", None]) for k in FEATURE_KEYS if r.random() < 0.6}])
    return {"values": values, "current": current, "recommended": recommended,
            "rec_name": r.choice(["llama-cuda", "a", 1, 1.0, True, None]) if recommended else None,
            "rec_backend": rec_backend,
            "arch": r.choice(["llama", "gemma3", "Gemma2", "GEMMA4", "", None, "qwen3", "g\u0130mma"]),
            "chat_template": r.random() < 0.8, "features": feats,
            "n_sessions": r.choice([1, 1, 2, 4, 8]), "rec_ctx": r.choice([0, 8192, 16384, 32768, 24576, 1000, 65536]),
            "has_mmproj": r.random() < 0.3, "mmproj_vram_gb": r.choice([0.5, 1.9100000001, 2.125, 0.0]),
            "mmproj_gb": r.choice([0.0, 0.78, 1.555, 2, True, 0.005]),
            "rope": r.choice([{}, {"arch": r.choice(["gemma3", "llama", None]),
                                   "rope_scaling_type": r.choice([None, "", "none", " NONE ", "NONE", "linear", "\u3000none"]),
                                   "rope_scaling_factor": r.choice([None, 0, 4.0])}]),
            "native_ctx": r.choice([0, 4096, 8192, 131072, -5, 1000]), "layers": r.choice([32, 48, 1]),
            "experts": r.choice([None, None, 1, 2, 8, 128, True, 0, -3, 2**70]),
            "offload": [r.choice(["none", "ngl", "n-cpu-moe", "cpu-moe"]), r.choice([0, 4, 12])],
            "presets": presets, "model_rel": r.choice(["", "/models/m/" + r.choice(QUANT_NAMES), r.choice(QUANT_NAMES)]),
            "general": r.choice([None, {}, {"params_raw": r.choice([None, 0, 4e9, 35e9, 1e11, 99_999_999_999, 100_000_000_000, 1.2e11])}])}


BASELINE_ARGS = ["-ngl", "999", "-c", "8192", "--ctx-size", "4096", "-fa", "--flash-attn", "on", "-np", "2", "--parallel",
                 "-ctv", "q8_0", "-ctk", "--jinja", "--models-dir", "/models", "-t", "8", "--threads", "--port", "8080",
                 "--cache-type-v", "Q8_0", "--unknown", "-x", "--no-mmap", "--mmproj", "--cont-batching", "--cpu-moe",
                 "-ncmoe", "4", "--n-cpu-moe", "-ub", "2048", "--batch-size", "--split-mode", "layer", "--", "-", "",
                 "-1", "--keep", "--spec-type", "--reasoning", "ON", "--fit", "-fit", "-kvo", "-cmoe", "-fitt", "-mg"]
# A synthetic stand-in for ini.ALL_KNOWN_KEYS (the rule takes the set as data).
KNOWN = sorted({"ctx-size", "flash-attn", "parallel", "jinja", "threads", "cache-type-v", "cache-type-k", "mmproj",
                "cont-batching", "cpu-moe", "n-cpu-moe", "batch-size", "split-mode", "keep", "spec-type", "reasoning",
                "fit", "ngl", "ubatch-size", "", "models-dir"} - {""})


def baseline_case(r: random.Random) -> dict:
    args = [r.choice(BASELINE_ARGS) for _ in range(r.randint(0, 16))]
    if r.random() < 0.04 and args:
        args[r.randrange(len(args))] = r.choice([None, 1, True, 2.5, [], {}])
    return {"args": args, "known": KNOWN}


def present_handmade() -> list[tuple[str, dict]]:
    r = random.Random(1)
    base = present_case(r)
    base.update(values=[["parallel", "1"], ["ctx-size", "32768"]], current={"ctx-size": "32768"}, model_rel="m.gguf",
                arch="llama", rec_backend={"baseline": {}, "gpu_count": 1}, recommended=True, rec_name="a")

    def pc(**over) -> dict:
        d = json.loads(json.dumps(base))
        d.update(over)
        return {"present": d}
    return [
        ("present: the #1152 order, changed then superseded sorted",
         pc(values=[["parallel", "2"], ["ctx-size", "8192"]],
            current={"tensor-split": "1,1", "ngl": "30", "keep": "64", "cpu-moe": "on", "fit": "off", "parallel": "1"})),
        ("present: a baseline conflict and a redundancy", pc(rec_backend={"baseline": {"parallel": "1", "ctx-size": "32768"}},
                                                             values=[["parallel", "2"], ["ctx-size", "32768"]])),
        ("present: multi-GPU disclosure", pc(rec_backend={"gpu_count": 4})),
        ("present: a gpu count that is not a number raises", pc(rec_backend={"gpu_count": "four"})),
        ("present: projector reservation formatted", pc(has_mmproj=True, mmproj_vram_gb=2.125, mmproj_gb=0.625)),
        ("present: MoE with expert offload", pc(experts=64, offload=["n-cpu-moe", 12])),
        ("present: MoE that does not fit", pc(experts=64, recommended=False, rec_backend=None, presets=[])),
        ("present: rope extension quirk", pc(values=[["rope-scaling", "linear"], ["rope-scale", "2.5"]], native_ctx=8192, rec_ctx=20480)),
        ("present: a sub-Q4 warning and a small context", pc(model_rel="/models/x/m-UD-IQ2_XS.gguf", rec_ctx=8192)),
        ("present: params as a string raises... in Python only", pc(model_rel="m-Q2_K.gguf", general={"params_raw": []})),
        ("present: the saved preset by ngl", pc(current={"ctx-size": "65536", "ngl": " 30 "},
                                                presets=[{"key": "fast", "ctx": 32768, "offload_kind": "ngl", "ngl": 30, "n_cpu_moe": 0}],
                                                n_sessions=2)),
        ("present: the saved preset by cpu-moe", pc(current={"ctx-size": "32768", "cpu-moe": "TRUE"},
                                                    presets=[{"key": "long-ctx", "ctx": 32768, "offload_kind": "cpu-moe", "ngl": 999, "n_cpu_moe": 0}])),
        ("present: an undeclared key is a quirk", pc(values=[["parallel", "1"], ["zz", "1"], ["aa", "2"]])),
        ("present: Kelvin sign lower-cases to k", pc(rec_backend={"baseline": {"cache-type-k": "Q8_0", "threads": "K\u212a"}},
                                                     values=[["cache-type-k", "q8_0"], ["threads", "kk"]], current=None)),
        ("[stricter] present: repr past U+024F", pc(current={"ctx-size": "\u2192"})),
        ("present: repr of a no-break space and C1 controls", pc(current={"keep": "\u00a0\u0085\u00ad\u00ff"})),
        ("[stricter] present: a saved context of Unicode spaces", pc(current={"ctx-size": "\u00a0"},
                                                                     presets=[{"key": "fast", "ctx": 32768, "offload_kind": "none", "ngl": 999, "n_cpu_moe": 0}])),
        ("[stricter] present: a non-ASCII model file name", pc(model_rel="/models/m/m\u00f6del-Q2_K.gguf")),
        ("[stricter] present: two non-ASCII baseline values", pc(rec_backend={"baseline": {"parallel": "\u00c9"}},
                                                                 values=[["parallel", "\u00e9"]])),
        ("[stricter] present: a non-string saved value", pc(current={"ctx-size": ["32768"]})),
        ("[stricter] present: a 40-digit saved context", pc(current={"ctx-size": "9" * 40},
                                                            presets=[{"key": "fast", "ctx": 32768, "offload_kind": "none", "ngl": 999, "n_cpu_moe": 0}])),
        ("spec: a hand-tuned section echoes every key", {"spec": {"section": "m", "current": {"spec-type": "draft-mtp", "spec-draft-n-max": " 6 "},
                                                                  "spec_profile": "", "mode": "", "files": True, "found_mtp": "",
                                                                  "nextn": None}}),
        ("spec: a head-needing profile without a head falls back to off",
         {"spec": {"section": "m", "current": None, "spec_profile": "coding", "mode": "", "files": False, "found_mtp": "",
                   "nextn": 0}}),
        ("spec: built-in MTP layers count as a head", {"spec": {"section": "m", "current": None, "spec_profile": "", "mode": "agent",
                                                                "files": False, "found_mtp": "", "nextn": "2"}}),
        ("spec: a mode that is a list raises", {"spec": {"section": "m", "current": None, "spec_profile": "", "mode": [1],
                                                         "files": True, "found_mtp": "/models/m/h.gguf", "nextn": None}}),
        ("files: the smallest projector in the folder", {"files": [{"rule": "mmproj_subdir", "subdir": "m", "listing": [
            ["mmproj-BF16.gguf", "file", 800], ["mmproj-F16.gguf", "file", 800], ["mmproj-F32.gguf", "file", 1600]]}]}),
        ("files: an unsized projector stops the scan", {"files": [{"rule": "mmproj_subdir", "subdir": "m", "listing": [
            ["mmproj-F32.gguf", "file", 1600], ["mmproj-a.gguf", "file", None], ["mmproj-b.gguf", "file", 1]]}]}),
        ("files: an is_file error empties a head folder", {"files": [{"rule": "mtp_folder", "prefix": "/models/m/", "section": "m",
                                                                      "listing": [["a-mtp-.gguf", "file", 5], ["b", "error", None]]}]}),
        ("files: the model is never its own head", {"files": [{"rule": "mtp_folder", "prefix": "/models/m/", "section": "m-MTP-Q4",
                                                               "listing": [["m-MTP-Q4.gguf", "file", 5], ["x-mtp-.gguf", "file", 9]]}]}),
        ("files: an unsized flat head raises OSError", {"files": [{"rule": "mtp_flat", "section": "m",
                                                                   "listing": [["m-mtp-.gguf", "file", None]]}]}),
        ("files: Kelvin and dotted I in names", {"files": [{"rule": "mtp_flat", "section": "kk", "listing": [
            ["K\u212a-mtp.gguf", "file", 3], ["\u0130-draft-.gguf", "file", 1]]}]}),
        ("[stricter] files: a non-ASCII section name", {"files": [{"rule": "mmproj_flat", "section": "m\u00f6del",
                                                                   "listing": [["m\u00f6del-mmproj.gguf", "file", 1]]}]}),
        ("baseline: short and long flags", {"baseline": [{"args": ["-ngl", "999", "--ctx-size", "8192", "--jinja", "-fa", "on"],
                                                          "known": KNOWN}]}),
        ("baseline: a non-string argument raises", {"baseline": [{"args": ["-ngl", 5], "known": KNOWN}]}),
        ("baseline: a null value is true", {"baseline": [{"args": ["-c", None], "known": KNOWN}]}),
        ("[stricter] baseline: known keys out of order", {"baseline": [{"args": ["--jinja"], "known": ["jinja", "ctx-size"]}]}),
    ]


def present_cases() -> list[tuple[str, dict]]:
    cases = present_handmade()
    r = random.Random(PRESENT_SEED)
    makers = [("spec", spec_case), ("files", lambda r: [files_call(r) for _ in range(r.randint(1, 4))]),
              ("present", present_case), ("baseline", lambda r: [baseline_case(r) for _ in range(r.randint(1, 3))])]
    for i in range(PRESENT_RANDOM_CASES):
        part, make = makers[i % len(makers)]
        cases.append((f"random {part} {i}", {part: make(r)}))
    return cases


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

    out = {"version": 1, "reference": "noevia-services model-manager/app/autoconfig_core.py check_reference",
           "cases": []}
    for name, parts in check_cases():
        text = json.dumps(parts, sort_keys=True)
        out["cases"].append({"name": name, "input": text, "expect": check_reference(core, json.loads(text))})
    errors = sum(1 for c in out["cases"] if "error" in c["expect"])
    OUT_CHECK.write_text(json.dumps(out, sort_keys=True, separators=(",", ":")) + "\n")
    print(f"{len(out['cases'])} cases ({errors} where the reference raises) -> {OUT_CHECK.relative_to(ROOT)}")

    out = {"version": 1, "reference": "noevia-services model-manager/app/autoconfig_core.py check_reference (spec, files, present, baseline)",
           "cases": []}
    for name, parts in present_cases():
        text = json.dumps(parts, sort_keys=True)
        out["cases"].append({"name": name, "input": text, "expect": check_reference(core, json.loads(text))})
    errors = sum(1 for c in out["cases"] if "error" in c["expect"])
    OUT_PRESENT.write_text(json.dumps(out, sort_keys=True, separators=(",", ":")) + "\n")
    print(f"{len(out['cases'])} cases ({errors} where the reference raises) -> {OUT_PRESENT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
