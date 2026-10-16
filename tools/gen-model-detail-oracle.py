#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/model-detail-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, middleware, router,
`api.model_detail`, `api._find_entry`, `api._entry`, `services.snapshot_models_dir`,
`services.model_shape`, `ini` and `gguf_meta`) against synthetic model folders, models.ini files
and GGUF headers, and records the exact status, content type and body bytes of
`GET /api/v1/models/detail`:

    python3 -I tools/gen-model-detail-oracle.py <services>/model-manager/app > fixture.json

Two fields are live state, not files: `loadedOn` (Docker discovery plus llama-server probes) and
`fit` (the GPU sampler and host RAM). The native route does not re-implement them; it asks the
Python child for exactly `api._loaded_map()` and `services.vram_fit_chips(bytes)` (the private
`GET /api/v1/internal/model-detail-runtime` call). Here both live sources are replaced by
deterministic synthetic stand-ins, the real `_entry` places them, and the case records the body
the child would answer for that call (`runtime`), rendered by the same JSON response class. The
native test replays that body and checks the request it was asked for.

Every file name, section and value is synthetic. Each case is declarative (GGUF headers as hex,
sparse sizes, nanosecond mtimes, symlinks, ini text) so the Rust test rebuilds the identical tree.
CI re-runs this against the pinned services ref and requires a byte-identical fixture. Run it on
Linux with TZ=UTC (the generator sets it), as CI does.
"""
from __future__ import annotations

import asyncio
import hashlib
import json
import os
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
    if kind == "i32":
        return 5, struct.pack("<i", value)
    if kind == "u64":
        return 10, struct.pack("<Q", value)
    if kind == "i64":
        return 11, struct.pack("<q", value)
    if kind == "f32":
        return 6, struct.pack("<f", value)
    if kind == "f64":
        return 12, struct.pack("<d", value)
    if kind == "bool":
        return 7, struct.pack("<?", value)
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


LLAMA = gguf(("general.architecture", "str", "llama"), ("general.name", "str", "Synthetic Llama"),
             ("general.file_type", "u32", 15), ("llama.context_length", "u32", 8192),
             ("llama.block_count", "u32", 4), ("llama.embedding_length", "u32", 256),
             ("llama.attention.head_count", "u32", 8), ("llama.rope.freq_base", "f32", 10000.0),
             ("tokenizer.ggml.model", "str", "gpt2"),
             ("tokenizer.ggml.tokens", "arr:str", [f"t{i}" for i in range(12)]),
             ("tokenizer.chat_template", "str", "{% if enable_thinking %}<think>{% endif %}{{ messages }}"))
MOE = gguf(("general.architecture", "str", "qwen3moe"), ("general.name", "str", "Synthetic MoE"),
           ("qwen3moe.expert_count", "u32", 128), ("qwen3moe.expert_used_count", "u32", 8),
           ("qwen3moe.block_count", "u32", 2), ("general.file_type", "u32", 7))
MOE_NO_USED = gguf(("general.architecture", "str", "mixtral"), ("mixtral.expert_count", "u64", 16))
NO_ARCH = gguf(("general.name", "str", "no architecture"))
PLAIN = gguf(("general.architecture", "str", "gemma3"), ("gemma3.block_count", "u32", 2))
NONFINITE = gguf(("general.architecture", "str", "llama"), ("llama.rope.freq_base", "f32", float("nan")),
                 ("llama.expert_count", "f64", float("inf")), ("llama.expert_used_count", "f32", float("nan")))


def shape_case(arch_kind: str, arch, count=None, used=None) -> str:
    kvs = [("general.architecture", arch_kind, arch)]
    name = str(arch) if arch_kind != "bool" else ("True" if arch else "False")
    if arch_kind == "f32":
        name = repr(struct.unpack("<f", struct.pack("<f", arch))[0])
    if count is not None:
        kvs.append((f"{name}.expert_count", count[0], count[1]))
    if used is not None:
        kvs.append((f"{name}.expert_used_count", used[0], used[1]))
    return gguf(*kvs)


SHAPES = {
    "shape-str-count.gguf": gguf(("general.architecture", "str", "llama"), ("llama.expert_count", "str", " 12 "),
                                 ("llama.expert_used_count", "str", "+0_4")),
    "shape-str-bad.gguf": gguf(("general.architecture", "str", "llama"), ("llama.expert_count", "str", "1__2"),
                               ("llama.expert_used_count", "str", "x")),
    "shape-float-count.gguf": gguf(("general.architecture", "str", "llama"), ("llama.expert_count", "f64", 7.9),
                                   ("llama.expert_used_count", "f32", -2.5)),
    "shape-bool-count.gguf": gguf(("general.architecture", "str", "llama"), ("llama.expert_count", "bool", True),
                                  ("llama.expert_used_count", "bool", False)),
    "shape-negative.gguf": gguf(("general.architecture", "str", "llama"), ("llama.expert_count", "i32", -3),
                                ("llama.expert_used_count", "i64", 2)),
    "shape-list-count.gguf": gguf(("general.architecture", "str", "llama"),
                                  ("llama.expert_count", "arr:u32", [4, 4]),
                                  ("llama.expert_used_count", "arr:u32", list(range(9)))),
    "shape-big-float.gguf": gguf(("general.architecture", "str", "llama"), ("llama.expert_count", "f64", 3e20),
                                 ("llama.expert_used_count", "u64", 2 ** 63 + 5)),
    "shape-arch-int.gguf": shape_case("u32", 7, ("u32", 4), ("u32", 2)),
    "shape-arch-zero.gguf": shape_case("u32", 0, ("u32", 4)),
    "shape-arch-float.gguf": shape_case("f64", 1.5, ("u32", 3)),
    "shape-arch-bool.gguf": shape_case("bool", True, ("u32", 5), ("u32", 1)),
    "shape-arch-false.gguf": shape_case("bool", False, ("u32", 5)),
    "shape-arch-empty.gguf": gguf(("general.architecture", "str", "")),
    "shape-arch-list.gguf": gguf(("general.architecture", "arr:str", ["a", "b"]),
                                 ("['a', 'b'].expert_count", "u32", 6)),
    "shape-kv-key.gguf": gguf(("general.architecture", "str", "llama"), ("llama.expert_count", "u32", 8),
                              ("kv", "arr:u32", list(range(10)))),
    "shape-kv-small.gguf": gguf(("general.architecture", "str", "llama"), ("llama.expert_count", "u32", 8),
                                ("kv", "arr:u32", [1, 2])),
    "shape-nonfinite.gguf": NONFINITE,
    "shape-unicode-arch.gguf": gguf(("general.architecture", "str", "ラマ"), ("ラマ.expert_count", "u32", 2)),
}

ERRORS = {
    "bad-magic.gguf": "4e4f50450300000000000000",
    "empty.gguf": "",
    "quote-magic.gguf": (b"G'\"F" + b"\0" * 20).hex(),
    "truncated-fixed.gguf": (b"GGUF\x03\x00\x00\x00\x00").hex(),
    "truncated-kv.gguf": gguf(("general.architecture", "str", "llama"))[:-6],
    "summary-raises.gguf": gguf(("general.architecture", "str", "llama"), ("general.file_type", "arr:u32", [1, 2])),
}


def files(*names: str, content: str = PLAIN, mtime: int = T0) -> list:
    return [{"name": n, "hex": content, "mtime": mtime} for n in names]


# --- live-state stand-ins -----------------------------------------------------------------------

def fit_stub(size_bytes: int) -> list[dict]:
    """Deterministic chips with the float shapes the real ones carry (vram_gb from settings or the
    sampler, ratio_pct ints, a CPU chip with `is_cpu`)."""
    gb = size_bytes / GIB
    chips = [{"name": "synthetic-gpu", "vram_gb": 23.5, "verdict": "fits" if gb < 15 else "oom",
              "ratio_pct": round(gb / 23.5 * 100)},
             {"name": "synthetic-cpu", "vram_gb": 0.1 + 0.2, "verdict": "slow", "ratio_pct": 0, "is_cpu": True}]
    if size_bytes < 4096:
        chips.append({"name": "edge", "vram_gb": 1e16, "verdict": "fits", "ratio_pct": 0, "host_ram_gb": 1e-05})
    return chips


def backends_stub(spec: list[list]):
    from app import services

    async def snapshot():
        return [services.LlamaBackend(name=name, found=True, status="running", loaded_model=loaded)
                for name, loaded in spec]
    return snapshot


# --- cases --------------------------------------------------------------------------------------

DEFAULT_BACKENDS = [["llama-a", "llama-flat, short-id"], ["llama-b", "short-id,,other "], ["idle", None],
                    ["llama-c", "  "], ["llama-d", "Synthétique-Q4"]]

CASES: list[dict] = [
    {"id": "flat-models-and-key-validation",
     "files": [{"name": "llama-flat.gguf", "hex": LLAMA, "mtime": T0, "size": 1280},
               {"name": "moe-Q4.gguf", "hex": MOE, "mtime": T0 + 59_999_999_500, "size": 5 * GIB + 7},
               {"name": "moe-no-used.gguf", "hex": MOE_NO_USED, "mtime": T0 - 86_400_000_000_123, "size": 1792},
               {"name": "no-arch.gguf", "hex": NO_ARCH, "mtime": 0, "size": 1023},
               {"name": "UPPER.GGUF", "hex": PLAIN, "mtime": -2_000_000_000, "size": 1024},
               {"name": "tera.gguf", "hex": PLAIN, "mtime": 2_147_000_000_000_000_123,
                "size": 1536 * GIB}],
     "requests": ["key=llama-flat.gguf", "key=moe-Q4.gguf", "key=moe-no-used.gguf", "key=no-arch.gguf",
                  "key=UPPER.GGUF", "key=tera.gguf", "", "other=1", "key", "key=", "key=missing.gguf",
                  "key=..%2Fx.gguf", "key=a..b.gguf", "key=%2Fllama-flat.gguf", "key=/llama-flat.gguf",
                  "key=a/b/c/d/e/f.gguf", "key=a/b/c/d/e.gguf", "key=llama-flat.gguf&key=moe-Q4.gguf",
                  "key=moe-Q4.gguf&key=", "key=llama%2Dflat.gguf", "key=llama-flat.gguf&extra=%ff",
                  "key=upper.gguf", "key=+llama-flat.gguf", "key=llama-flat.gguf;x=1"]},
    {"id": "subfolders-shards-companions",
     "dirs": ["sub", "solo", "deep/a/b/c", "models--org--repo/snapshots/rev1", "models--org--repo/blobs"],
     "files": [{"name": "sub/big-00001-of-00003.gguf", "hex": MOE, "mtime": T0 + 1_000_000_000, "size": 2 * GIB},
               {"name": "sub/big-00003-of-00003.gguf", "hex": "00", "mtime": T0 + 3_000_000_000, "size": GIB},
               {"name": "sub/big-00002-of-00003.gguf", "hex": "00", "mtime": T0 + 2_000_000_000, "size": GIB},
               {"name": "sub/mmproj-big-BF16.gguf", "hex": PLAIN, "mtime": T0, "size": 700},
               {"name": "sub/mmproj-big-F32.gguf", "hex": PLAIN, "mtime": T0, "size": 1300},
               {"name": "sub/big-draft-Q4.gguf", "hex": PLAIN, "mtime": T0, "size": 300},
               {"name": "solo/mmproj-orphan.gguf", "hex": PLAIN, "mtime": T0 + 13},
               {"name": "solo/model.imatrix.gguf", "hex": "", "mtime": T0 + 14},
               {"name": "deep/a/b/c/depth-four.gguf", "hex": LLAMA, "mtime": T0 + 11},
               {"name": "models--org--repo/blobs/0123abcd", "hex": LLAMA, "mtime": T0 + 15}],
     "links": [["models--org--repo/snapshots/rev1/hf-model-Q4.gguf", "../../blobs/0123abcd"],
               ["linked-file.gguf", "sub/mmproj-big-BF16.gguf"]],
     "requests": ["key=sub/big.gguf", "key=big.gguf", "key=sub/big-00001-of-00003.gguf",
                  "key=sub/mmproj-big-BF16.gguf", "key=sub/big-draft-Q4.gguf", "key=solo/mmproj-orphan.gguf",
                  "key=solo/model.imatrix.gguf", "key=deep/a/b/c/depth-four.gguf",
                  "key=models--org--repo/snapshots/rev1/hf-model-Q4.gguf", "key=linked-file.gguf"]},
    {"id": "ini-aliases-and-model-ids",
     "dirs": ["sub"],
     "files": files("alpha-Q8_0.gguf", "beta.gguf", "gamma.gguf", "sub/delta.gguf", "eps.gguf", "zeta.gguf",
                    "theta.gguf", "mmproj-iota.gguf"),
     "ini": {"text": "version = 1\n[DEFAULT]\nctx-size = 1\n[short-id]\nmodel = /models/alpha-Q8_0.gguf\n"
                     "[alpha-Q8_0]\nctx-size = 4096\n[persona]\nmodel = alpha-Q8_0.gguf\n"
                     "[beta-renamed]\nmodel = /beta.gguf\n[gamma]\nmodel = missing.gguf\n"
                     "[delta-id]\nmodel =   sub/delta.gguf  \n[eps]\nctx-size = 2\n"
                     "[zeta-a]\nmodel = /models//zeta.gguf\n[mmproj-iota]\nctx-size = 3\n"},
     "requests": ["key=alpha-Q8_0.gguf", "key=beta.gguf", "key=gamma.gguf", "key=sub/delta.gguf", "key=eps.gguf",
                  "key=zeta.gguf", "key=theta.gguf", "key=mmproj-iota.gguf"]},
    {"id": "ini-default-model-and-duplicates",
     "files": files("one.gguf", "two.gguf"),
     "ini": {"text": "[DEFAULT]\nmodel = two.gguf\n[one]\nctx-size = 1\n[two-a]\nx = 1\n[two-a]\ny = 2\n[one]\nz = 3\n"},
     "requests": ["key=one.gguf", "key=two.gguf"]},
    {"id": "ini-crlf", "files": files("a.gguf"), "ini": {"text": "version = 1\r\n[a-id]\r\nmodel = a.gguf\r\n"},
     "requests": ["key=a.gguf"]},
    {"id": "ini-parse-error-500", "files": files("a.gguf"), "ini": {"text": "[a]\nno delimiter here\n"},
     "requests": ["key=a.gguf", "key=missing.gguf", "key=../x"]},
    {"id": "ini-not-utf8-500", "files": files("a.gguf"), "ini": {"hex": "5b615d0a78203d20ff0a"},
     "requests": ["key=a.gguf"]},
    {"id": "ini-is-directory-500", "files": files("a.gguf"), "ini": "dir", "requests": ["key=a.gguf"]},
    {"id": "models-missing", "models": "missing", "requests": ["key=a.gguf"]},
    {"id": "models-path-is-file", "models": "file", "requests": ["key=a.gguf"]},
    {"id": "models-folder-is-a-symlink", "models": "link", "dirs": ["sub"],
     "files": [{"name": "a.gguf", "hex": LLAMA, "mtime": T0},
               {"name": "sub/b-00001-of-00002.gguf", "hex": MOE, "mtime": T0 + 1},
               {"name": "sub/b-00002-of-00002.gguf", "hex": "", "mtime": T0 + 2}],
     "ini": {"text": "[a]\nmodel = a.gguf\n"},
     "requests": ["key=a.gguf", "key=sub/b.gguf"]},
    {"id": "absolute-links-inside", "dirs": ["sub"],
     "files": [{"name": "sub/x.gguf", "hex": MOE, "mtime": T0 + 7}],
     "abslinks": [["y.gguf", "sub/x.gguf"]],
     "requests": ["key=y.gguf", "key=sub/x.gguf"]},
    {"id": "header-errors",
     "files": [{"name": n, "hex": h, "mtime": T0} for n, h in ERRORS.items()],
     "requests": [f"key={n}" for n in ERRORS]},
    {"id": "shapes",
     "files": [{"name": n, "hex": h, "mtime": T0} for n, h in SHAPES.items()],
     "requests": [f"key={n}" for n in SHAPES]},
    {"id": "loaded-on-and-fit",
     "files": files("llama-flat.gguf", "other.gguf", "Synthétique-Q4.gguf", "zero.gguf", content=LLAMA)
     + [{"name": "empty-size.gguf", "hex": "", "mtime": T0}],
     "ini": {"text": "[short-id]\nmodel = other.gguf\n"},
     "backends": [["llama-a", "llama-flat, short-id"], ["llama-b", "short-id,,other "], ["idle", None],
                  ["llama-c", "  "], ["llama-d", "Synthétique-Q4"], ["llama-a", "llama-flat"]],
     "requests": ["key=llama-flat.gguf", "key=other.gguf", "key=Synth%C3%A9tique-Q4.gguf", "key=zero.gguf",
                  "key=empty-size.gguf"]},
    {"id": "no-backends", "files": files("solo.gguf", content=MOE), "backends": [],
     "requests": ["key=solo.gguf"]},
]


def build(root: Path, case: dict) -> tuple[Path, Path]:
    models = root / "models"
    ini_path = models / "models.ini"
    kind = case.get("models", "dir")
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


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith(("NOEVIA_NATIVE_", "MODELS_", "MODEL_FILES", "GGUF_", "LLAMA_")) \
                or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR", "DOCKER_HOST"}:
            del os.environ[key]
    os.environ["TZ"] = "UTC"
    import time
    time.tzset()
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.responses import JSONResponse  # noqa: E402
    from fastapi.testclient import TestClient  # noqa: E402

    from app import api, config, gguf_meta, services  # noqa: E402
    from app.main import app  # noqa: E402

    client = TestClient(app, raise_server_exceptions=False)  # no lifespan: no sampler, no db
    names = ("api.py", "main.py", "services.py", "ini.py", "utils.py", "gguf_meta.py", "autoconfig.py",
             "autoconfig_core.py", "discover.py", "config.py")
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest() for name in names}
    services.vram_fit_chips = fit_stub
    out = []
    for case in CASES:
        services.snapshot_llama_backends = backends_stub(case.get("backends", DEFAULT_BACKENDS))
        with tempfile.TemporaryDirectory() as tmp:
            models, ini_path = build(Path(tmp), case)
            config.settings.models_dir = models
            config.settings.models_ini_path = ini_path
            gguf_meta._CACHE.clear()
            results = []
            for query in case["requests"]:
                response = client.get("/api/v1/models/detail" + (f"?{query}" if query else ""))
                runtime = None
                if response.status_code == 200:
                    entry = response.json()
                    fit = services.vram_fit_chips(entry["bytes"]) if not entry["companion"] else []
                    body = {"loaded": asyncio.run(api._loaded_map()), "fit": fit}
                    runtime = {"bytes": entry["bytes"], "fit": not entry["companion"],
                               "body": JSONResponse(body).body.decode("utf-8")}
                # `path` is the absolute file path: the case's temporary models folder becomes
                # @MODELS@ (the native test substitutes its own folder the same way).
                body_text = response.content.decode("utf-8").replace(json.dumps(str(models))[1:-1], "@MODELS@")
                results.append({"query": query, "status": response.status_code,
                                "content_type": response.headers.get("content-type"),
                                "body": body_text, "runtime": runtime})
            out.append({**case, "results": results})
    json.dump({"oracle": digest, "cases": out}, sys.stdout, ensure_ascii=True, indent=0, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
