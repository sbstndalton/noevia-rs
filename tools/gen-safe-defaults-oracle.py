#!/usr/bin/env python3
"""Actual private plan, public response and final files on synthetic sparse models only."""
import argparse
import importlib.util
import json
import os
import sys
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
def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", type=Path)
    parser.add_argument("--projector-order", choices=("a-first", "z-first"))
    parser.add_argument("--assemble-order-variants", nargs=2, type=Path, metavar=("A_FIRST", "Z_FIRST"))
    args = parser.parse_args()
    if args.assemble_order_variants:
        assert args.projector_order is None
        documents = [json.loads(p.read_text()) for p in args.assemble_order_variants]
        a, z = documents
        for key in ("oracle", "epoch", "stamp", "migrate_flag"):
            assert a[key] == z[key], key
        cases = [d["cases"] for d in documents]
        assert all(len(c) == 1 and c[0]["id"] == "folder-head-projector-priority" for c in cases)
        for key in ("id", "steps", "files", "ini", "roles", "cap", "hard", "impl"):
            assert cases[0][0][key] == cases[1][0][key], key
        for rows, selected in zip(cases, ("a-mmproj.gguf", "z-mmproj.gguf")):
            assert json.loads(rows[0]["answers"][1]["plan"]["body"])["values"]["mmproj"] == "/models/syn-folder/" + selected
        json.dump({"oracle":a["oracle"], "epoch":a["epoch"], "stamp":a["stamp"],
                   "variants":{"a-first":cases[0][0], "z-first":cases[1][0]}},
                  sys.stdout, ensure_ascii=False, indent=1, sort_keys=True)
        sys.stdout.write("\n")
        return 0
    # The historical oracle recorded a-first. Make that explicit without changing its
    # fixture bytes. Each order-specific invocation uses a fresh original app module.
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {"MODEL_LOADER_TOKEN", "DATA_DIR", "MODELS_DIR", "MODELS_INI_PATH"}:
            del os.environ[key]
    sys.path.insert(0, str(args.app.resolve().parent))
    from app import config, helpers
    original_path = helpers.Path
    first = "z-mmproj.gguf" if args.projector_order == "z-first" else "a-mmproj.gguf"
    second = "a-mmproj.gguf" if first == "z-mmproj.gguf" else "z-mmproj.gguf"
    class OrderedFixturePath(type(Path())):
        def iterdir(self):
            entries = list(super().iterdir())
            if self == Path(config.settings.models_dir) / "syn-folder":
                names = [p.name for p in entries]
                assert names.count(first) == names.count(second) == 1
                # Change only the two known synthetic projector slots. Other entries
                # retain their real order; is_file/suffix and all application code stay real.
                positions = [i for i, name in enumerate(names) if name in (first, second)]
                by_name = {p.name:p for p in entries if p.name in (first, second)}
                entries[positions[0]], entries[positions[1]] = by_name[first], by_name[second]
            return iter(entries)
    if args.projector_order:
        g.CASES = [c for c in g.CASES if c["id"] == "folder-head-projector-priority"]
        assert len(g.CASES) == 1
    helpers.Path = OrderedFixturePath
    original_argv = sys.argv
    sys.argv = [original_argv[0], str(args.app)]
    try:
        return g.main()
    finally:
        sys.argv = original_argv
        helpers.Path = original_path

if __name__ == "__main__": raise SystemExit(main())
