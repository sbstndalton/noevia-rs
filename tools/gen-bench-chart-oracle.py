#!/usr/bin/env python3
"""Record actual chart prediction helpers on synthetic files; no startup or live services."""
import importlib.util
import hashlib
import json
import os
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("section_oracle", Path(__file__).with_name("gen-section-autoconfig-oracle.py"))
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)

def main():
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith(("NOEVIA_", "MODEL_", "GGUF_", "LLAMACPP_")):
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from app import helpers, config, autoconfig
    from app.main import app
    from fastapi.testclient import TestClient
    settings = config.settings
    settings.model_loader_token = "synthetic-chart-token"
    client = TestClient(app, raise_server_exceptions=False)
    GIB = builder.GIB
    cases = []
    configurations = [
        ("flat", [builder.g("syn.gguf", 2*GIB)], [builder.gpu()], "", ["syn", "missing"]),
        ("folder-single-file", [builder.g("folder/syn-00001-of-00002.gguf", GIB), builder.g("folder/syn-00002-of-00002.gguf", 30*GIB)], [builder.gpu()], "[syn]\nmodel = /models/folder/syn-00001-of-00002.gguf\nctx-size = 12288\n", ["syn"]),
        ("no-backends", [builder.g("syn.gguf", GIB)], [], "", ["syn"]),
        ("invalid-header", [builder.f("syn.gguf")], [builder.gpu()], "", ["syn"]),
        ("context-absent", [builder.g("syn.gguf", GIB)], [builder.gpu(vram=18.125)], "[syn]\n", ["syn"]),
        ("context-invalid", [builder.g("syn.gguf", GIB)], [builder.gpu()], "[syn]\nctx-size = invalid\n", ["syn"]),
        ("context-negative", [builder.g("syn.gguf", GIB)], [builder.gpu()], "[syn]\nctx-size = -4096\n", ["syn"]),
        ("context-underscores", [builder.g("syn.gguf", GIB)], [builder.gpu()], "[syn]\nctx-size = 8_192\n", ["syn"]),
    ]
    for ident, files, backends, ini, aliases in configurations:
        case = {"id": ident, "files": files, "backends": backends, "ini": ini, "aliases": aliases, "cache_cap": 1024}
        with tempfile.TemporaryDirectory() as tmp:
            models = builder.build(Path(tmp), case)
            settings.models_dir = models
            settings.models_ini_path = models / "models.ini"
            settings.llamacpp_autoconfig_cache_ram_max_mib = 1024
            helpers._backend_list = lambda: backends
            os.environ["NOEVIA_NATIVE_MODEL_MANAGER_BENCH_RUNS_CONFIRMED"] = "1"
            response = client.get("/api/v1/internal/bench-run-runtime", params=[("alias", a) for a in aliases], headers={"x-model-loader-token": "synthetic-chart-token"})
            assert response.status_code == 200, response.text
            case["runtime"] = response.json()
            cases.append(case)
    # Exercise the actual helper's frontier selection with a deterministic analyze result.
    selections = []
    frontier = [{"ctx":4096,"gpu_gb":1.125,"kv_gb":0.0},{"ctx":12288,"gpu_gb":2.0,"kv_gb":0.125}]
    original = autoconfig.analyze
    for ctx in ["8192", "0", "invalid", "", "8_192", "-1"]:
        with tempfile.TemporaryDirectory() as tmp:
            models = builder.build(Path(tmp), {"files":[builder.g("syn.gguf", GIB)], "ini":"[syn]\nctx-size = " + ctx + "\n"})
            settings.models_dir = models
            settings.models_ini_path = models / "models.ini"
            helpers._backend_list = lambda: [builder.gpu()]
            plan = {"frontier":frontier,"recommended_total_ctx":12288,"recommended_ctx":4096}
            autoconfig.analyze = lambda **_: SimpleNamespace(**{**plan, "frontier": [SimpleNamespace(**row) for row in plan["frontier"]]})
            selections.append({"ctx":ctx,"plan":plan,"expected":helpers._predicted_vram_gb("syn")})
    autoconfig.analyze = original
    json.dump({"oracle":{name:hashlib.sha256((app_dir/name).read_bytes()).hexdigest() for name in ["helpers.py","autoconfig.py","ini.py"]},"cases":cases,"selections":selections}, sys.stdout, indent=1, sort_keys=True)
    print()

if __name__ == "__main__": main()
