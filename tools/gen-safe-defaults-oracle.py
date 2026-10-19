#!/usr/bin/env python3
"""Actual private plan, public response and final files on synthetic sparse models only."""
import importlib.util
from pathlib import Path
spec = importlib.util.spec_from_file_location("writer_oracle", Path(__file__).with_name("gen-section-writes-oracle.py"))
g = importlib.util.module_from_spec(spec)
spec.loader.exec_module(g)

def file(name, ctx=32768, extra=None, size=None):
    data = {"gguf": g.gguf(arch="synth", ctx=ctx, extra=extra)}
    if size is not None: data["size"] = size
    return [name, data, 1600000000]

def post(name): return g.post(f"/api/v1/sections/{name}/safe-defaults")

g.CASES = [
    g.case("flat-and-repeat", [post("syn-flat"), post("syn-flat")], files=[file("syn-flat.gguf",4096)]),
    g.case("missing-and-draft", [post("missing"), post("missing-draft"),post("syn-draft")], files=[file("syn-draft.gguf")]),
    g.case("bad-header-registers", [post("syn-bad")], files=[["syn-bad.gguf","not a valid header",1600000000]]),
    g.case("claimed-by-alias", [post("syn-claimed")], ini="[alias]\nmodel = /models/syn-claimed.gguf\n", files=[file("syn-claimed.gguf")]),
    g.case("inherited-claim", [post("syn-claimed")], ini="[DEFAULT]\nmodel = /models/syn-claimed.gguf\ncache-ram = 128\n[alias]\n", files=[file("syn-claimed.gguf")]),
    g.case("existing-before-discovery", [post("missing-draft")], ini="[missing-draft]\nctx-size = 2048\n"),
    g.case("large-mtp-build", [post("syn-mtp-build")], files=[file("syn-mtp-build.gguf",size=3*1024**3)]),
    g.case("small-mtp-build", [post("syn-mtp-build")], files=[file("syn-mtp-build.gguf",size=1024)]),
    g.case("builtin-head", [post("syn-builtin")], files=[file("syn-builtin.gguf",extra=[("synth.nextn_predict_layers",4,1)])]),
    g.case("folder-head-projector-priority", [post("syn-folder"),post("syn-folder-Q4")], files=[
        file("syn-folder/syn-folder-Q4.gguf",extra=[("synth.nextn_predict_layers",4,1)]),
        file("syn-folder/z-mmproj.gguf"), file("syn-folder/a-mmproj.gguf"),
        file("syn-folder/z-syn-folder-draft-Q8.gguf")]),
    g.case("default-cache-and-small-context", [post("syn-default")], ini="[DEFAULT]\nthreads = 4\ncache-ram = 128\n", files=[file("syn-default.gguf",1024)]),
    g.case("projector-name-and-invalid-name",[post("syn-mmproj"),post("bad%20name")],files=[file("syn-mmproj.gguf")]),
]
if __name__ == "__main__": raise SystemExit(g.main())
