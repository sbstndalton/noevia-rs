#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/fit-python.v1.json (noevia#1318 step 1).

Runs the ACTUAL noevia-services model-manager application's fit primitives:
`services._fit_backends`, `services._cpu_backends`, `services.vram_fit_chips`, `hw.host_ram_gb`,
`hw.usable_ram_gb` and `helpers._backend_list` (the search budget and autoconfig descriptors).
Docker is the fake daemon of `gen-backends-oracle.py` on a Unix socket, reached through docker-py;
the backend cache is loaded through `hw.load_pushed` (what the native sampler pushes to the
child); `/proc/meminfo` is answered from the case. Every name is synthetic; no Docker daemon,
container, GPU or model is touched.

    python3 -I tools/gen-fit-oracle.py <services>/model-manager/app > fixture.json

Recorded per case: the inventory, the chips of every size as the detail runtime renders them
(`JSONResponse`) and as the `-> dict` routes render them (pydantic's JSON mode), the descriptors
without `baseline` (`autoconfig.parse_baseline` of `baseline_args`), and the search budget.
"""
from __future__ import annotations

import builtins
import hashlib
import importlib.util
import io
import json
import os
import shutil
import sys
import tempfile
import threading
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("backends_oracle", HERE / "gen-backends-oracle.py")
bo = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bo)

GIB = 1024 ** 3
container, image, world, cid, iid = bo.container, bo.image, bo.world, bo.cid, bo.iid


def meminfo(total_kib):
    return f"MemTotal:       {total_kib} kB\nMemFree:         1024 kB\n"


def gib_kib(gib):
    return int(gib * 1024 * 1024)


def card(index, total, *, used=1.0, device="0x150e"):
    return {"index": index, "name": f"Synthetic GPU {index}", "util_pct": 3.0, "vram_used_gb": used,
            "vram_total_gb": total, "temp_c": 40.0, "power_w": 10.0, "shared_used_gb": 0.0,
            "shared_total_gb": 0.0, "clock_mhz": 0.0, "device": device}


def gpu(total, *, vendor="vulkan", cards=None, count=1, source="sysfs"):
    return {"vendor": vendor, "name": "Synthetic GPU", "util_pct": 5.0, "vram_used_gb": 1.0,
            "vram_total_gb": total, "temp_c": 41.0, "power_w": 12.0, "gpu_count": count,
            "cards": cards if cards is not None else [], "memory_kind": "unified", "shared_used_gb": 0.0,
            "shared_total_gb": 0.0, "clock_mhz": 0.0, "source": source, "measured": True}


def ok(g=None):
    return {"ok": True, "error": None, "gpu": g, "container": None}


def warming():
    return {"ok": False, "error": "warming up…", "gpu": None, "container": None}


VULKAN = "ghcr.io/ggml-org/llama.cpp:server-vulkan"
CPU = "ghcr.io/ggml-org/llama.cpp:server"
ROCM = "ghcr.io/ggml-org/llama.cpp:server-rocm"
CUDA = "ghcr.io/ggml-org/llama.cpp:server-cuda"
UNKNOWN = "local/llama.cpp:full"


def boundaries(vram):
    """Sizes either side of every GPU threshold for one VRAM figure."""
    out = [0, 1, 2 * GIB, 6 * GIB, int(vram * GIB)]
    for frac in (0.65, 0.9, 1.0):
        edge = int(frac * vram * GIB)
        out += [edge - 1, edge, edge + 1]
    return out


def cases():
    out = []
    # 1. One integrated GPU sampled through sysfs (an int VRAM figure), 64 GiB of RAM less the
    # default reserve. Its image says "server", so without a sample it would run on the CPU.
    w = world([container(1, "llama-igpu", 1)], [image(1, [VULKAN])])
    out.append({"id": "gpu-boundaries", "env": {}, "meminfo": meminfo(gib_kib(64)), "docker": w,
                "cache": {"llama-igpu": ok(gpu(16, cards=[card(0, 16.0)]))},
                "sizes": boundaries(16) + [48 * GIB, 48 * GIB + 1, 10 ** 38, 2 ** 128 - 1]})
    # 2. The same backend while its sample is warming up: a CPU backend. RAM ticks: 64 GiB usable.
    w = world([container(2, "llama-cpu", 2), container(3, "llama-unknown", 3, status="exited")],
              [image(2, [CPU]), image(3, [UNKNOWN])])
    out.append({"id": "cpu-only", "env": {}, "meminfo": meminfo(gib_kib(96)), "docker": w,
                "cache": {"llama-cpu": ok(None), "llama-unknown": warming()},
                "sizes": [0, 1, 30 * GIB - 1, 30 * GIB, 48 * GIB, 64 * GIB - 1, 64 * GIB, 64 * GIB + 1, 200 * GIB]})
    # 3. CPU tight and fits around 75 % of 24 GiB usable (32 GiB less 8), from a meminfo whose
    # MemTotal is not the first line and whose lines end in CRLF.
    w = world([container(4, "llama-cpu", 4)], [image(4, [CPU])])
    out.append({"id": "cpu-tight", "env": {"HOST_RAM_RESERVE_GB": "8"},
                "meminfo": "MemFree: 1 kB\r\nMemTotal:\t" + str(gib_kib(32)) + " kB\r\n", "docker": w,
                "cache": {}, "sizes": [0, 18 * GIB - 1, 18 * GIB, 18 * GIB + 1, 24 * GIB - 1, 24 * GIB, 3 * GIB]})
    # 4. Insufficient RAM: 16 GiB total under a 32 GiB reserve gives 0.0, so the CPU backend has no
    # chip and nothing is ever impossible; the GPU backend keeps its own.
    w = world([container(5, "llama-cpu", 5), container(6, "llama-igpu", 6)], [image(5, [CPU]), image(6, [VULKAN])])
    out.append({"id": "insufficient-ram", "env": {}, "meminfo": meminfo(gib_kib(16)), "docker": w,
                "cache": {"llama-igpu": ok(gpu(8.0))}, "sizes": [0, 4 * GIB, 8 * GIB, 100 * GIB]})
    # 5. Unreadable meminfo shapes: missing, not a number, no value.
    for i, text in enumerate([None, "MemTotal: lots kB\n", "MemTotal:\n", "MemTotal: 1_048_576 kB\n"]):
        w = world([container(7, "llama-cpu", 7)], [image(7, [CPU])])
        out.append({"id": f"meminfo-{i}", "env": {"HOST_RAM_RESERVE_GB": "0"}, "meminfo": text, "docker": w,
                    "cache": {}, "sizes": [0, GIB // 2, 2 * GIB]})
    # 6. Impossible machine: the largest VRAM (a 16 GiB declaration beside a sampled 15.55) plus
    # 31.4 GiB usable RAM; exactly at the ceiling is not impossible.
    w = world([container(8, "llama-igpu", 8), container(9, "llama-big", 9)], [image(8, [VULKAN]), image(9, [VULKAN])])
    out.append({"id": "impossible", "env": {"GPU_VRAM": "llama-big:16", "HOST_RAM_RESERVE_GB": "32.6"},
                "meminfo": meminfo(gib_kib(64)), "docker": w,
                "cache": {"llama-igpu": ok(gpu(15.55)), "llama-big": ok(gpu(4.0))},
                "sizes": [int(47.4 * GIB) - 1, int(47.4 * GIB), int(47.4 * GIB) + 1, 100 * GIB, 10 ** 15]})
    # 7. A sampled VRAM rounded to 0.1 in the machine chip, and a ratio of exactly x.5 %.
    w = world([container(10, "llama-igpu", 10)], [image(10, [VULKAN])])
    out.append({"id": "impossible-rounding", "env": {"HOST_RAM_RESERVE_GB": "0.05"},
                "meminfo": meminfo(gib_kib(8)), "docker": w, "cache": {"llama-igpu": ok(gpu(0.25))},
                "sizes": [GIB // 8, GIB // 4, 9 * GIB, 33 * GIB]})
    # 8. Empty stores: no Docker and nothing configured; Docker with no llama.cpp container.
    out.append({"id": "empty-no-docker", "env": {}, "meminfo": meminfo(gib_kib(64)), "docker": None,
                "cache": {}, "sizes": [0, GIB, 100 * GIB]})
    w = world([container(11, "web-other", 11)], [image(11, ["nginx:latest"])])
    out.append({"id": "empty-docker", "env": {}, "meminfo": meminfo(gib_kib(64)), "docker": w,
                "cache": {}, "sizes": [0, GIB]})
    # 9. A mixed store: a configured backend that is gone (declared), a CPU backend that is also
    # declared (it gets both chips), a sampled integrated GPU whose declaration is 0 (the chips
    # probe it; the descriptors skip it), a ROCm image without a sample (no chip at all), a CUDA
    # one with a two-card sample, an unknown image running on the CPU, a string Cmd.
    w = world([container(12, "llama-cpu", 12, cmd="llama-server"), container(13, "llama-igpu", 13),
               container(14, "llama-rocm", 14), container(15, "llama-cuda", 15, cmd=["--port", "8081", "-ngl", "99"]),
               container(16, "llama-unknown", 16), container(17, "model-loader", 12)],
              [image(12, [CPU]), image(13, ["<none>:<none>", VULKAN]), image(14, [ROCM]), image(15, [CUDA]),
               image(16, [UNKNOWN])])
    out.append({"id": "mixed", "env": {"LLAMA_CONTAINERS": " llama-gone ,llama-cuda,,llama-gone",
                                       "GPU_VRAM": "llama-gone:12, llama-cpu:4, llama-igpu:0, bad, x:y",
                                       "HOST_RAM_RESERVE_GB": " 16 "},
                "meminfo": meminfo(gib_kib(62.5)), "docker": w,
                "cache": {"llama-igpu": ok(gpu(15.6)), "llama-rocm": warming(),
                          "llama-cuda": ok(gpu(48, vendor="nvidia", count=2, cards=[card(0, 24), card(1, 24.0)])),
                          "llama-unknown": ok(gpu(0.0))},
                "sizes": [0, 3 * GIB, 7 * GIB, 11 * GIB, 30 * GIB, 47 * GIB, 60 * GIB, 95 * GIB, 200 * GIB]})
    return out


RESERVE_FORMS = ["0", "8", "32", "8.5", " 16 ", "+8.25\n", "-4", "0.05", "007", "1e3", "inf", "nan",
                 "8.", ".5", "1_0", "0x10", "8 kB", ""]


def main() -> None:
    app_dir = Path(sys.argv[1]).resolve()
    root = Path(tempfile.mkdtemp(prefix="gen-fit-"))
    sock_path = root / "docker.sock"
    (root / "models").mkdir()
    (root / "data").mkdir()
    os.environ.update(TZ="UTC", MODELS_DIR=str(root / "models"), MODELS_INI_PATH=str(root / "models" / "models.ini"),
                      DATA_DIR=str(root / "data"), MODEL_LOADER_TOKEN="", DOCKER_HOST=f"unix://{sock_path}")
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.responses import JSONResponse
    from pydantic import TypeAdapter

    from app import helpers, hw, services
    from app.config import settings

    server = bo.UnixServer(str(sock_path), bo.Daemon)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    mem = {"text": None}
    real_open = builtins.open

    def fake_open(path, *args, **kwargs):
        if str(path) == "/proc/meminfo":
            if mem["text"] is None:
                raise FileNotFoundError(path)
            return io.TextIOWrapper(io.BytesIO(mem["text"].encode()), encoding=kwargs.get("encoding"))
        return real_open(path, *args, **kwargs)

    hw.open = fake_open

    def render(value, mode):
        try:
            if mode == "route":
                value = TypeAdapter(list).dump_python(value, mode="json")
            return JSONResponse(value).body.decode()
        except Exception:  # noqa: BLE001 - the plain 500
            return "500"

    out_cases = []
    for case in cases():
        env = case["env"]
        settings.llama_containers = env.get("LLAMA_CONTAINERS", "")
        settings.gpu_vram = env.get("GPU_VRAM", "")
        settings.host_ram_reserve_gb = float(env.get("HOST_RAM_RESERVE_GB", "32"))
        mem["text"] = case["meminfo"]
        w = case["docker"]
        bo.STATE["world"] = w
        os.environ["DOCKER_HOST"] = f"unix://{sock_path}" if w is not None else f"unix://{root}/absent.sock"
        hw.load_pushed(case["cache"])
        chips = []
        for b in case["sizes"]:
            fit = services.vram_fit_chips(b)
            chips.append({"bytes": str(b), "detail": render(fit, "detail"), "route": render(fit, "route")})
        descriptors = [{k: v for k, v in d.items() if k != "baseline"} for d in helpers._backend_list()]
        budget = max((float(d.get("vram_gb") or 0) for d in helpers._backend_list()), default=0.0)
        out_cases.append({
            "id": case["id"], "env": env, "meminfo": case["meminfo"], "docker": w,
            "cache": json.dumps(case["cache"], ensure_ascii=False),
            "inventory": {"gpu": [[k, repr(v)] for k, v in services._fit_backends().items()],
                          "cpu": services._cpu_backends(), "ram": repr(hw.usable_ram_gb())},
            "chips": chips, "descriptors": render(descriptors, "detail"), "budget": repr(budget),
        })
    # pydantic's own reading of HOST_RAM_RESERVE_GB forms; the native parser accepts a subset.
    from app.config import Settings
    reserve_forms = {}
    for form in RESERVE_FORMS:
        try:
            reserve_forms[form] = repr(Settings(host_ram_reserve_gb=form).host_ram_reserve_gb)
        except Exception:  # noqa: BLE001 - a settings error stops the child
            reserve_forms[form] = "error"
    server.shutdown()
    shutil.rmtree(root, ignore_errors=True)
    sources = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest()
               for name in ("config.py", "helpers.py", "hw.py", "services.py")}
    json.dump({"sources": sources, "cases": out_cases, "reserve_forms": reserve_forms}, sys.stdout, indent=1, sort_keys=True, ensure_ascii=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
