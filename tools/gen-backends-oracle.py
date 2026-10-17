#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/backends-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application: the GPU and backend sampler
(`hw._sample_loop` for one tick at a time: `services._effective_container_names`, `hw._collect`,
`_detect_vendor`, `_read_drm`, `_read_vulkan`, `_read_container_runtime`, the history and its
pruning) and `GET /api/v1/backends` (`api.backends`: `services.snapshot_llama_backends` with its
Docker reads, `_parse_started_at`, `_resolve_internal_port` and the llama-server probes). It talks
to a fake Docker daemon on a Unix socket through docker-py (so docker-py turns the daemon's JSON
into its objects, errors and messages exactly as in production), to fake llama-servers on
127.0.0.1, and reads a fake `/sys/class/drm` in a temporary folder:

    python3 -I tools/gen-backends-oracle.py <services>/model-manager/app > fixture.json

Recorded per step: after a sampler tick, the backend cache as JSON (what the native sampler pushes
to the child) and the `docker exec` GPU reads the native sampler asks the child for
(`hw.exec_probe`); after a GET, the status and body. `docker exec` itself is answered at the
docker-py boundary (`Container.exec_run`) from the case, since the native side never runs it.
`time.time` and `datetime.now` are pinned per step; TZ is UTC. Every name is synthetic; no Docker
daemon, container, GPU or model is touched.
"""
from __future__ import annotations

import asyncio
import hashlib
import http.server
import json
import os
import shutil
import socketserver
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

API = "1.47"
LLAMA_IMAGE = "ghcr.io/ggml-org/llama.cpp:server-vulkan"


# ---------------------------------------------------------------- the synthetic world

def cid(n: int) -> str:
    return f"{n:02x}" * 32


def iid(n: int) -> str:
    return "sha256:" + f"{n + 0x80:02x}" * 32


def container(n, name, image_n, *, status="running", started="2026-10-17T10:00:00.123456789Z", ports=None,
              cmd=None, env=None, health=None, devices=None, entrypoint=None):
    state = {"Status": status, "StartedAt": started}
    if health is not None:
        state["Health"] = {"Status": health}
    return {
        "Id": cid(n), "Name": "/" + name, "Image": iid(image_n), "State": state,
        "Config": {"Cmd": cmd if cmd is not None else ["--host", "0.0.0.0"], "Entrypoint": entrypoint,
                   "Env": env or []},
        "NetworkSettings": {"Ports": ports if ports is not None else {}},
        "HostConfig": {"Devices": devices or []},
    }


def image(n, tags):
    return {"Id": iid(n), "RepoTags": tags}


def cpu_stats(total, system, online=4, percpu=None, pre_total=None, pre_system=None):
    cpu = {"cpu_usage": {"total_usage": total}, "system_cpu_usage": system}
    if online is not None:
        cpu["online_cpus"] = online
    if percpu is not None:
        cpu["cpu_usage"]["percpu_usage"] = percpu
    pre = {}
    if pre_total is not None:
        pre = {"cpu_usage": {"total_usage": pre_total}, "system_cpu_usage": pre_system}
    return cpu, pre


def stats(total=4_000_000, system=100_000_000, pre_total=2_000_000, pre_system=90_000_000, online=4,
          percpu=None, usage=3 * 1024 ** 3, limit=0, sub=None):
    cpu, pre = cpu_stats(total, system, online, percpu, pre_total, pre_system)
    mem = {"usage": usage, "limit": limit}
    if sub is not None:
        mem["stats"] = sub
    return {"cpu_stats": cpu, "precpu_stats": pre, "memory_stats": mem}


def amd_card(index, *, vendor="0x1002", device="0x150e", vram_total=512 * 1024 ** 2, vram_used=100 * 1024 ** 2,
             gtt_total=48 * 1024 ** 3, gtt_used=9 * 1024 ** 3, busy=37, hwmon=True, temp=51000, power=23_500_000,
             freq=2_900_000_000, power_input=None):
    files = {
        f"card{index}/device/vendor": vendor + "\n",
        f"card{index}/device/device": device + "\n",
        f"card{index}/device/mem_info_vram_total": f"{vram_total}\n",
        f"card{index}/device/mem_info_vram_used": f"{vram_used}\n",
        f"card{index}/device/mem_info_gtt_total": f"{gtt_total}\n",
        f"card{index}/device/mem_info_gtt_used": f"{gtt_used}\n",
        f"card{index}/device/gpu_busy_percent": f"{busy}\n",
    }
    if hwmon:
        base = f"card{index}/device/hwmon/hwmon{index + 3}"
        files[f"{base}/temp1_input"] = f"{temp}\n"
        if power is not None:
            files[f"{base}/power1_average"] = f"{power}\n"
        if power_input is not None:
            files[f"{base}/power1_input"] = f"{power_input}\n"
        files[f"{base}/freq1_input"] = f"{freq}\n"
    return files


NVIDIA_TWO = "50, 2048, 24576, 61, 120.5, Synthetic RTX\n70, 4096, 24576, 66, 130.25, Synthetic RTX\n"
NVIDIA_MIXED = "10, 1024, 8192, 40, 50, Card A\n20, 512, 4096, 45, 30.5, Card B\n"
ROCM = json.dumps({"card0": {"GPU use (%)": "7", "GPU Memory Allocated (VRAM%)": "25",
                             "Temperature (Sensor edge) (C)": "45.0", "Average Graphics Package Power (W)": "30.0"}})
MODELS_LOADED = json.dumps({"data": [{"id": "synthetic-a", "status": {"value": "loaded"}},
                                     {"id": "synthetic-b", "status": {"value": "unloaded"}}]})
MODELS_NONE = json.dumps({"data": [{"id": "synthetic-a", "status": {"value": "unloaded"}}]})
MODELS_EMPTY = json.dumps({"data": []})
MODELS_FIXED = json.dumps({"data": [{"id": "embed-fixed"}]})

T0 = 1_792_240_000.0  # 2026-10-17T... UTC, after every StartedAt below


def tick(at, docker=None, sysfs=None, exec_=None):
    return {"op": "tick", "at": at, "docker": docker, "sysfs": sysfs, "exec": exec_}


def get(at, llama=None, docker=None):
    return {"op": "get", "at": at, "llama": llama, "docker": docker}


def world(containers, images, stats_by=None, stats_status=None, version=True, missing_images=(), image_errors=None,
          inspect_errors=None, list_status=200):
    return {"containers": containers, "images": images, "stats": stats_by or {}, "stats_status": stats_status or {},
            "version": version, "missing_images": list(missing_images), "image_errors": image_errors or {},
            "inspect_errors": inspect_errors or {}, "list_status": list_status}


def cases():
    out = []
    # 1. A Vulkan backend on an AMD APU read from sysfs, a stopped CUDA one, a non-llama container,
    # the model loader itself (never a backend), and a llama image whose first tag is <none>.
    vulkan = container(1, "127.0.0.1", 1, ports={"18180/tcp": [{"HostIp": "0.0.0.0", "HostPort": "18080"}]},
                       devices=[{"PathOnHost": "/dev/dri/card1", "PathInContainer": "/dev/dri/card1"},
                                {"PathOnHost": "/dev/dri/renderD128", "PathInContainer": "/dev/dri/renderD128"}],
                       health="healthy")
    stopped = container(2, "llama-stopped", 2, status="exited", started="2026-10-16T08:00:00Z")
    other = container(3, "web-other", 3)
    loader = container(4, "model-loader", 1)
    nonetag = container(5, "llama-untagged-first", 4, status="created", started="0001-01-01T00:00:00Z")
    images = [image(1, [LLAMA_IMAGE]), image(2, ["ghcr.io/ggml-org/llama.cpp:server-cuda"]),
              image(3, ["nginx:latest"]), image(4, ["<none>:<none>", "local/llama.cpp:full"])]
    w = world([vulkan, stopped, other, loader, nonetag], images,
              {cid(1): stats(sub={"inactive_file": 512 * 1024 ** 2, "anon": 2 * 1024 ** 3}, limit=64 * 1024 ** 3)})
    sysfs = amd_card(1)
    sysfs.update(amd_card(0, vendor="0x8086", device="0x46a6"))  # an Intel card not mapped into it
    w2 = json.loads(json.dumps(w))
    w2["stats"][cid(1)] = stats(total=9_000_000, pre_total=4_000_000, system=110_000_000, pre_system=100_000_000,
                                sub={"inactive_file": 0, "anon": 3 * 1024 ** 3}, limit=64 * 1024 ** 3)
    sysfs2 = amd_card(1, busy=88, vram_used=200 * 1024 ** 2, gtt_used=12 * 1024 ** 3, temp=63500, power=41_250_000)
    sysfs2.update(amd_card(0, vendor="0x8086", device="0x46a6"))
    out.append({"id": "vulkan-sysfs-apu", "env": {}, "steps": [
        get(T0 - 1, {"127.0.0.1": [200, MODELS_LOADED]}, w),
        tick(T0, w, sysfs), tick(T0 + 2, w2, sysfs2),
        get(T0 + 3, {"127.0.0.1": [200, MODELS_LOADED]}, w2),
    ]})
    # 2. Configured names (one missing, with a restart error), a ROCm backend whose rocm-smi works
    # (VRAM from GPU_VRAM), then fails (sysfs fallback), cgroup v1 memory keys and per-CPU counts.
    rocm = container(6, "127.0.0.1", 6, cmd=["--port", "19090"], env=["LLAMA_ARG_PORT=7000"],
                     devices=[{"PathOnHost": "/dev/dri/card2"}])
    w = world([rocm], [image(6, ["ghcr.io/ggml-org/llama.cpp:server-rocm"])],
              {cid(6): stats(online=None, percpu=[1, 2, 3], sub={"cache": 1024 ** 3, "rss": 1536 * 1024 ** 2},
                             limit=8 * 1024 ** 3)})
    out.append({"id": "configured-rocm", "env": {"LLAMA_CONTAINERS": " llama-gone ,127.0.0.1,, llama-gone",
                                                  "GPU_VRAM": "127.0.0.1:16, bad, x:y, other : 8"},
                "restart_errors": {"llama-gone": "APIError: synthetic restart failure", "127.0.0.1": "APIError: x"},
                "steps": [
                    tick(T0, w, amd_card(2, hwmon=False), {"127.0.0.1": {"rocm-smi": [0, ROCM]}}),
                    tick(T0 + 2, w, amd_card(2, hwmon=False), {"127.0.0.1": {"rocm-smi": [1, ""]}}),
                    get(T0 + 2.5, {"127.0.0.1": [200, MODELS_FIXED]}, w),
                ]})
    # 3. NVIDIA through nvidia-smi: two equal cards, then two different ones, then a failed read.
    cuda = container(7, "localhost", 7, ports={"18100/tcp": None})
    w = world([cuda], [image(7, ["registry.example/llama.cpp:server-cuda12"])], {cid(7): stats(limit=16 * 1024 ** 3)})
    out.append({"id": "nvidia-smi", "env": {}, "steps": [
        tick(T0, w, {}, {"localhost": {"nvidia-smi": [0, NVIDIA_TWO]}}),
        tick(T0 + 2, w, {}, {"localhost": {"nvidia-smi": [0, NVIDIA_MIXED]}}),
        tick(T0 + 4, w, {}, {"localhost": {"nvidia-smi": [9, ""]}}),
        tick(T0 + 6, w, {}, {"localhost": {"nvidia-smi": [0, "garbage, x\n"]}}),
        get(T0 + 7, {"localhost": [500, "boom"]}, w),
    ]})
    # 4. Vulkan without an AMD card: rocm-smi/nvidia-smi through the fallback, then the GPU_VRAM
    # declaration, then nothing (no declaration for the second backend).
    vk_a = container(8, "127.0.0.1", 8, devices=[{"PathOnHost": "/dev/dri/card0"}])
    vk_b = container(9, "localhost", 8, cmd=["--port", "18191"])
    w = world([vk_a, vk_b], [image(8, ["ghcr.io/ggml-org/llama.cpp:server-vulkan-b4000"])],
              {cid(8): stats(), cid(9): stats(pre_system=100_000_000)})
    intel = amd_card(0, vendor="0x8086")
    out.append({"id": "vulkan-fallbacks", "env": {"GPU_VRAM": "127.0.0.1:12", "LLAMA_DEFAULT_PORT": "18190"}, "steps": [
        tick(T0, w, intel, {"127.0.0.1": {"nvidia-smi": [0, NVIDIA_MIXED]}, "localhost": {}}),
        tick(T0 + 2, w, intel, {"127.0.0.1": {}, "localhost": {"rocm-smi": [0, ROCM]}}),
        get(T0 + 3, {"127.0.0.1": [200, MODELS_NONE], "localhost": [200, MODELS_EMPTY]}, w),
    ]})
    # 5. Docker errors: an image that is gone (404) and one the daemon fails to read (500), a
    # container the daemon fails to inspect, and container stats it refuses.
    gone = container(10, "127.0.0.1", 10, health="unhealthy")
    broken = container(11, "llama-broken-image", 11)
    flaky = container(12, "localhost", 12, cmd=["--port", "18201"])
    w = world([gone, broken, flaky],
              [image(10, ["ghcr.io/ggml-org/llama.cpp:server"]), image(11, ["ghcr.io/ggml-org/llama.cpp:server"]),
               image(12, ["ghcr.io/ggml-org/llama.cpp:server-cuda"])],
              {cid(12): stats()}, stats_status={cid(12): 500})
    w_list = json.loads(json.dumps(w))
    w_after = json.loads(json.dumps(w))
    w_after["missing_images"] = [iid(10)]
    w_after["image_errors"] = {iid(11): [500, {"message": "synthetic image read failure"}]}
    w_after["inspect_errors"] = {"localhost": [500, {"message": "synthetic inspect failure"}]}
    out.append({"id": "docker-errors", "env": {"LLAMA_CONTAINERS": "127.0.0.1,llama-broken-image,localhost",
                                                "LLAMA_DEFAULT_PORT": "18200"},
                "steps": [
                    tick(T0, w_list, {}, {"localhost": {"nvidia-smi": [0, NVIDIA_TWO]}}),
                    tick(T0 + 2, w_after, {}, {}),
                    get(T0 + 3, {}, w_after),
                ]})
    # 6. No Docker at all: every configured name is "docker unreachable".
    out.append({"id": "docker-down", "env": {"LLAMA_CONTAINERS": "llama-a,llama-b"},
                "restart_errors": {"llama-a": "APIError: kept"},
                "steps": [tick(T0, None), get(T0 + 1, {}, None)]})
    # 7. Probe outcomes and uptimes: healthy-but-unreachable, a published port from Cmd, a
    # container started seconds, minutes, hours and days ago, and a StartedAt without fractions.
    probes = []
    for i, (name, started, extra) in enumerate([
        ("127.0.0.1", "2026-10-17T11:59:30.5Z", {"health": "healthy"}),
        ("localhost", "2026-10-17T11:15:00Z", {"cmd": ["llama-server", "--port=18211"], "health": "healthy"}),
    ]):
        probes.append(container(20 + i, name, 20, started=started, **extra))
    w = world(probes, [image(20, ["ghcr.io/ggml-org/llama.cpp:server"])], {cid(20): stats(), cid(21): stats()})
    noon = datetime(2026, 10, 17, 12, 0, 0, tzinfo=timezone.utc).timestamp()
    out.append({"id": "probes-and-uptime", "env": {"LLAMA_DEFAULT_PORT": "18210"}, "steps": [
        get(noon, {"127.0.0.1": None, "localhost": None}, w),
        get(noon + 3600 * 5 + 61, {"127.0.0.1": [200, "not json"], "localhost": None}, w),
        get(noon + 86400 * 3 + 7200, {"127.0.0.1": [200, MODELS_EMPTY], "localhost": [404, ""]}, w),
    ]})
    # 8. History: 125 ticks keep the last 120 points in the answer; the vendor is kept per name
    # even when the image changes; a backend that disappears from a non-empty discovery is pruned.
    keep = container(30, "127.0.0.1", 30, devices=[{"PathOnHost": "/dev/dri/card3"}])
    leave = container(31, "localhost", 31, cmd=["--port", "18221"])
    w = world([keep, leave], [image(30, [LLAMA_IMAGE]), image(31, ["ghcr.io/ggml-org/llama.cpp:server"])],
              {cid(30): stats(), cid(31): stats()})
    w_cuda = json.loads(json.dumps(w))
    w_cuda["images"][0]["RepoTags"] = ["ghcr.io/ggml-org/llama.cpp:server-cuda"]
    w_gone = json.loads(json.dumps(w_cuda))
    w_gone["containers"] = [keep]
    steps = [tick(T0 + 2 * i, w if i < 3 else w_cuda, amd_card(3, busy=i % 100, temp=40000 + i * 10)) for i in range(125)]
    steps.append(get(T0 + 250, {"127.0.0.1": [200, MODELS_LOADED], "localhost": [200, MODELS_LOADED]}, w_cuda))
    steps.append(tick(T0 + 252, w_gone, amd_card(3)))
    steps.append(get(T0 + 253, {"127.0.0.1": [200, MODELS_LOADED]}, w_gone))
    out.append({"id": "history-vendor-prune", "env": {"LLAMA_DEFAULT_PORT": "18220"}, "steps": steps})
    # 9. Container stats edges: no previous sample, a zero system delta, a memory limit, no
    # memory sub-stats, an empty stats answer, and a power read from power1_input.
    edge_a = container(40, "127.0.0.1", 40, devices=[{"PathOnHost": "/dev/dri/card4"}])
    edge_b = container(41, "localhost", 40, cmd=["--port", "18231"],
                       devices=[{"PathOnHost": "/dev/dri/card4"}, {"PathOnHost": "/dev/dri/card5"}])
    w = world([edge_a, edge_b], [image(40, [LLAMA_IMAGE])],
              {cid(40): {"cpu_stats": {"cpu_usage": {"total_usage": 5}, "system_cpu_usage": 10},
                         "memory_stats": {"usage": 5 * 1024 ** 3, "limit": 2 * 1024 ** 3}},
               cid(41): stats(system=90_000_000, sub={})})
    two = amd_card(4, power=None, power_input=12_000_000, vram_total=8 * 1024 ** 3, gtt_total=4 * 1024 ** 3)
    two.update(amd_card(5, device="0x1900", hwmon=False, vram_total=16 * 1024 ** 3))
    w_bad = json.loads(json.dumps(w))
    w_bad["stats"][cid(40)] = {}  # nothing at all: no CPU or memory figures
    out.append({"id": "stats-edges", "env": {"LLAMA_DEFAULT_PORT": "18230"}, "steps": [
        tick(T0, w, two), tick(T0 + 2, w_bad, two),
        get(T0 + 3, {"127.0.0.1": [200, MODELS_LOADED], "localhost": [200, MODELS_LOADED]}, w_bad),
    ]})
    return out


# ---------------------------------------------------------------- the fakes

STATE = {"world": None, "exec": {}, "llama": {}}


class Daemon(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def address_string(self):
        return "docker"

    def send(self, status, body):
        raw = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        w = STATE["world"]
        path, _, query = self.path.partition("?")
        if path.startswith("/v1."):
            path = "/" + path.split("/", 2)[2]
        parts = path.split("/")
        by_ref = {}
        for c in w["containers"]:
            by_ref[c["Id"]] = c
            by_ref[c["Name"][1:]] = c
        if path == "/version":
            return self.send(200, {"ApiVersion": API, "Version": "27.0.0"})
        if path == "/containers/json":
            if w["list_status"] != 200:
                return self.send(w["list_status"], {"message": "synthetic list failure"})
            return self.send(200, [{"Id": c["Id"], "Names": [c["Name"]], "Image": c["Image"]} for c in w["containers"]])
        if len(parts) == 4 and parts[1] == "containers" and parts[3] == "json":
            if parts[2] in w["inspect_errors"]:
                status, body = w["inspect_errors"][parts[2]]
                return self.send(status, body)
            c = by_ref.get(parts[2])
            if c is None:
                return self.send(404, {"message": f"No such container: {parts[2]}"})
            return self.send(200, c)
        if len(parts) == 4 and parts[1] == "containers" and parts[3] == "stats":
            c = by_ref.get(parts[2])
            if c is None:
                return self.send(404, {"message": f"No such container: {parts[2]}"})
            status = w["stats_status"].get(c["Id"], 200)
            if status != 200:
                return self.send(status, {"message": "synthetic stats failure"})
            answer = w["stats"].get(c["Id"], {})
            return self.send(200, answer.encode() if isinstance(answer, str) else answer)
        if len(parts) == 4 and parts[1] == "images" and parts[3] == "json":
            ref = parts[2] if parts[2].startswith("sha256:") else "sha256:" + parts[2]
            if ref in w["image_errors"]:
                status, body = w["image_errors"][ref]
                return self.send(status, body)
            if ref in w["missing_images"]:
                return self.send(404, {"message": f"No such image: {ref}"})
            for i in w["images"]:
                if i["Id"] == ref:
                    return self.send(200, i)
            return self.send(404, {"message": f"No such image: {ref}"})
        return self.send(404, {"message": "page not found"})


class UnixServer(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True

    def get_request(self):
        request, _ = super().get_request()
        return request, ("docker", 0)


class Llama(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        host = self.server.backend_name
        answer = STATE["llama"].get(host)
        status, body = answer
        raw = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


def main() -> None:
    app_dir = Path(sys.argv[1]).resolve()
    root = Path(tempfile.mkdtemp(prefix="gen-backends-"))
    sock_path = root / "docker.sock"
    drm = root / "drm"
    (root / "models").mkdir()
    (root / "data").mkdir()
    os.environ.update(TZ="UTC", MODELS_DIR=str(root / "models"), MODELS_INI_PATH=str(root / "models" / "models.ini"),
                      DATA_DIR=str(root / "data"), MODEL_LOADER_TOKEN="", DOCKER_HOST=f"unix://{sock_path}")
    time.tzset()
    sys.path.insert(0, str(app_dir.parent))
    import docker.models.containers as dmc
    from docker.models.containers import ExecResult
    from fastapi.encoders import jsonable_encoder
    from fastapi.responses import JSONResponse

    from app import api, hw, services
    from app.config import settings

    server = UnixServer(str(sock_path), Daemon)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    # Probe targets: the backend named 127.0.0.1 and the one named localhost each reach their own
    # fake llama-server; the port comes from the container's inspect data, as in production.
    clock = {"now": T0}
    real_time = time.time
    time.time = lambda: clock["now"]

    class Pinned(datetime):
        @classmethod
        def now(cls, tz=None):
            return datetime.fromtimestamp(clock["now"], tz)

    services.datetime = Pinned
    hw._DRM_ROOT = str(drm)

    def exec_run(self, cmd, *args, **kwargs):
        program = cmd.split()[0]
        answer = STATE["exec"].get(self.name, {}).get(program)
        if answer is None:
            return ExecResult(126, f"OCI runtime exec failed: {program}: not found".encode())
        return ExecResult(answer[0], answer[1].encode())

    dmc.Container.exec_run = exec_run

    class Stop(Exception):
        pass

    def stop(_seconds):
        raise Stop

    hw.time.sleep = stop

    def write_sysfs(files):
        shutil.rmtree(drm, ignore_errors=True)
        drm.mkdir()
        for rel, text in (files or {}).items():
            p = drm / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)

    out_cases = []
    for case in cases():
        hw._CACHE.clear()
        hw._HISTORY.clear()
        hw._VENDOR_CACHE.clear()
        services._restart_errors.clear()
        services._restart_errors.update(case.get("restart_errors") or {})
        env = case["env"]
        settings.llama_containers = env.get("LLAMA_CONTAINERS", "")
        settings.gpu_vram = env.get("GPU_VRAM", "")
        settings.llama_default_port = int(env.get("LLAMA_DEFAULT_PORT", "8080"))
        steps_out = []
        for step in case["steps"]:
            clock["now"] = step["at"]
            w = step.get("docker")
            STATE["world"] = w
            os.environ["DOCKER_HOST"] = f"unix://{sock_path}" if w is not None else f"unix://{root}/absent.sock"
            if step["op"] == "tick":
                write_sysfs(step.get("sysfs"))
                STATE["exec"] = step.get("exec") or {}
                try:
                    hw._sample_loop()
                except Stop:
                    pass
                probes = {}
                for name in services._effective_container_names() if w is not None else []:
                    for kind in ("nvidia", "amd", "amd-nvidia"):
                        probes[f"{name} {kind}"] = api._plain(hw.exec_probe(name, kind))
                steps_out.append({"op": "tick", "at": step["at"], "docker": w, "sysfs": step.get("sysfs") or {},
                                  "exec": step.get("exec") or {},
                                  "cache": api._plain(dict(hw._CACHE)), "probes": probes})
            else:
                llama = step.get("llama") or {}
                STATE["llama"] = dict(llama)
                # Both probe hosts reach 127.0.0.1, each on its own port (from its inspect data). A
                # backend with no answer has no listener at all: its probe is refused.
                listeners = []
                for c in ((w or {}).get("containers") or []):
                    name = c["Name"][1:]
                    if llama.get(name) is None:
                        continue
                    _hp, port = services._resolve_internal_port(c)
                    srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), Llama)
                    srv.backend_name = name
                    threading.Thread(target=srv.serve_forever, daemon=True).start()
                    listeners.append(srv)
                try:
                    body = JSONResponse(content=jsonable_encoder(asyncio.run(api.backends()))).body
                    status, text = 200, body.decode()
                except Exception:  # noqa: BLE001 - FastAPI's plain 500
                    status, text = 500, "Internal Server Error"
                for srv in listeners:
                    srv.shutdown()
                    srv.server_close()
                steps_out.append({"op": "get", "at": step["at"], "docker": w, "llama": llama,
                                  "status": status, "body": text})
        out_cases.append({"id": case["id"], "env": env, "restart_errors": case.get("restart_errors") or {},
                          "steps": steps_out})
    server.shutdown()
    time.time = real_time
    shutil.rmtree(root, ignore_errors=True)
    sources = {}
    for name in ("api.py", "config.py", "hw.py", "services.py"):
        sources[name] = hashlib.sha256((app_dir / name).read_bytes()).hexdigest()
    json.dump({"api_version": API, "sources": sources, "cases": out_cases}, sys.stdout, indent=1, sort_keys=True,
              ensure_ascii=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
