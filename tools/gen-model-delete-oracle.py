#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/model-delete-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, middleware, router,
`api.delete_models`, `api._find_entry`, `services.snapshot_models_dir`, `services.delete_gguf`,
`services._running_containers`, `services.model_holders`, the HF-cache, protected-folder and
mount-point helpers, `ini`) against synthetic model folders, a fake Docker client and a fake
`/proc/self/mountinfo`, and records for every request the status, content type and body of
`POST /api/v1/models/delete`, the whole tree left behind (inside and outside the models folder)
and how many container listings were made:

    python3 -I tools/gen-model-delete-oracle.py <services>/model-manager/app > fixture.json

Every file name, section, container and value is synthetic; file contents are zeros of a given
size. No Docker daemon, container or real model is touched. The case's temporary folder is
written as @ROOT@ in bodies (the native test substitutes its own the same way). CI re-runs this
against the pinned services ref and requires a byte-identical fixture.
"""
from __future__ import annotations

import hashlib
import json
import os
import sys
import tempfile
from pathlib import Path


def f(name: str, size: int = 10) -> dict:
    return {"name": name, "size": size}


def req(models=None, raw: str | None = None, content_type: str | None = "application/json") -> dict:
    body = raw if raw is not None else json.dumps({"models": models})
    return {"body": body, "content_type": content_type}


def container(name, cid="c0ffee", cmd=None, entrypoint=None, args=None, env=None, mounts=None):
    return {"name": name, "id": cid, "cmd": cmd, "entrypoint": entrypoint, "args": args, "env": env or [],
            "mounts": mounts or []}


CASES: list[dict] = [
    {"id": "flat-one", "files": [f("x.gguf", 100), f("y.gguf", 50)],
     "requests": [req(["x.gguf"]), req(["x.gguf"])]},
    {"id": "subdir-whole", "files": [f("sub/m-00001-of-00002.gguf", 2048), f("sub/m-00002-of-00002.gguf", 1024),
                                     f("sub/mmproj-m.gguf", 300), f("sub/README.md", 7), f("sub/chat_template.jinja", 5),
                                     f("sub/deeper/notes.txt", 3), f("keep.gguf", 1)],
     "requests": [req(["sub/m.gguf"])]},
    {"id": "subdir-shared", "files": [f("sub/a.gguf", 10), f("sub/b.gguf", 20), f("sub/mmproj-b.gguf", 30)],
     "requests": [req(["sub/a.gguf"]), req(["sub/b.gguf"])]},
    {"id": "nested-other", "files": [f("sub/a.gguf"), f("sub/old/z.gguf"), f("sub/mmproj-a.gguf")],
     "requests": [req(["sub/a.gguf"])]},
    {"id": "nested-companion-only", "files": [f("sub/a.gguf"), f("sub/proj/mmproj-x.gguf")],
     "requests": [req(["sub/a.gguf"])]},
    {"id": "per-file-empties-folder", "files": [f("sub/a.gguf"), f("sub/old/z.gguf")],
     "requests": [req(["sub/old/z.gguf"]), req(["sub/a.gguf"])]},
    {"id": "download-target", "env": {"MODEL_DOWNLOAD_TARGETS": " dl/ , other "},
     "files": [f("dl/a.gguf"), f("dl/notes.txt")], "requests": [req(["dl/a.gguf"])]},
    {"id": "download-target-empties", "env": {"MODEL_DOWNLOAD_TARGETS": "dl"},
     "files": [f("dl/a.gguf")], "requests": [req(["dl/a.gguf"])]},
    {"id": "mount-point", "mountinfo": ["36 35 98:0 / @MODELS@/mnt rw,noatime - ext4 /dev/x rw",
                                        "37 35 98:0 /x @MODELS@/sub/in\\040ner rw - ext4 /dev/y rw", "short line"],
     "files": [f("mnt/a.gguf"), f("mnt/b.txt"), f("sub/a.gguf"), f("sub/in ner/data.bin")],
     "requests": [req(["mnt/a.gguf"]), req(["sub/a.gguf"])]},
    {"id": "hf-cache", "files": [f("models--o--r/blobs/b1", 100), f("models--o--r/blobs/b2", 40),
                                 f("models--o--r/refs/main", 4)],
     "links": [["models--o--r/snapshots/rev1/x-Q4.gguf", "../../blobs/b1"],
               ["models--o--r/snapshots/rev1/mmproj-x.gguf", "../../blobs/b2"]],
     "requests": [req(["models--o--r/snapshots/rev1/x-Q4.gguf"])]},
    {"id": "hf-cache-shared-blob", "files": [f("models--o--r/blobs/b1", 100), f("models--o--r/blobs/b3", 9)],
     "links": [["models--o--r/snapshots/rev1/x-Q4.gguf", "../../blobs/b1"],
               ["models--o--r/snapshots/rev2/x-Q4.gguf", "../../blobs/b1"],
               ["models--o--r/snapshots/rev1/config.json", "../../blobs/b3"]],
     "requests": [req(["models--o--r/snapshots/rev1/x-Q4.gguf"])]},
    {"id": "hf-two-quants", "files": [f("models--o--r/blobs/q4", 64), f("models--o--r/blobs/q8", 128)],
     "links": [["models--o--r/snapshots/rev/Q4/x-Q4.gguf", "../../../blobs/q4"],
               ["models--o--r/snapshots/rev/Q8/x-Q8.gguf", "../../../blobs/q8"]],
     "requests": [req(["models--o--r/snapshots/rev/Q4/x-Q4.gguf"]),
                  req(["models--o--r/snapshots/rev/Q8/x-Q8.gguf"])]},
    {"id": "hf-in-download-target", "env": {"MODEL_DOWNLOAD_TARGETS": "hf/models--o--r/snapshots/rev"},
     "files": [f("hf/models--o--r/blobs/q4", 64)],
     "links": [["hf/models--o--r/snapshots/rev/x-Q4.gguf", "../../blobs/q4"]],
     "requests": [req(["hf/models--o--r/snapshots/rev/x-Q4.gguf"])]},
    {"id": "hf-dangling-and-outside", "files": [f("models--o--r/blobs/b1", 21), f("elsewhere.bin", 5)],
     "links": [["models--o--r/snapshots/rev/x.gguf", "../../blobs/b1"],
               ["models--o--r/snapshots/rev/gone.bin", "../../blobs/missing"],
               ["models--o--r/snapshots/rev/out.bin", "../../../elsewhere.bin"]],
     "requests": [req(["models--o--r/snapshots/rev/x.gguf"])]},
    {"id": "hf-beside", "files": [f("models--o--r/blobs/a", 11), f("models--o--r/blobs/b", 12),
                                  f("models--o--r/blobs/p", 13)],
     "links": [["models--o--r/snapshots/rev/a.gguf", "../../blobs/a"],
               ["models--o--r/snapshots/rev/b.gguf", "../../blobs/b"],
               ["models--o--r/snapshots/rev/mmproj-b.gguf", "../../blobs/p"]],
     "requests": [req(["models--o--r/snapshots/rev/a.gguf"])]},
    {"id": "keys", "files": [f("x.gguf", 5), f("sub/y.gguf", 6)],
     "requests": [req(["../x"]), req(["/abs.gguf"]), req(["a/b/c/d/e/f.gguf"]), req(["nope.gguf"]),
                  req([5, True, None, 1.5, -0.0, 1e100]), req({"x.gguf": 1}), req(3), req(None),
                  req([]), req(raw="{}"), req(raw="[]"), req(raw='"x"'), req(raw="{bad"), req(raw=""),
                  req(raw='{"models": ["nope.gguf"]}', content_type=None),
                  req(raw='{"models": ["nope.gguf"]}', content_type="text/plain"),
                  req(raw='{"models": ["nope.gguf"], "models": ["x.gguf"]}'),
                  req(["sub/y.gguf", "sub/y.gguf", "x.gguf"])]},
    {"id": "string-models", "files": [f("x"), f("q.gguf")], "requests": [req("q.gguf")]},
    {"id": "holders-command", "files": [f("x.gguf"), f("sub/s.gguf")],
     "docker": [container("embed", cid="e1", cmd=["--model", "/models/x.gguf", "--port", "8080"])],
     "requests": [req(["sub/s.gguf", "x.gguf"]), req(["sub/s.gguf"])]},
    {"id": "holders-command-eq-and-norm", "files": [f("sub/s.gguf"), f("t.gguf")],
     "docker": [container("embed", entrypoint="/bin/llama --model=/models//sub/./s.gguf", args=["--model=t.gguf"])],
     "requests": [req(["sub/s.gguf"]), req(["t.gguf"])]},
    {"id": "holders-mount-source", "env": {"MODELS_HOST_PATH": "/srv/models/"},
     "files": [f("x.gguf"), f("y.gguf")],
     "docker": [container("other", cmd=["--model", "/data/x.gguf"], mounts=[{"Destination": "/data", "Source": "/elsewhere"}]),
                container("embed", cid="e2", cmd=["--model", "/m/y.gguf"], mounts=[{"Destination": "/m/", "Source": "/srv/models"}])],
     "requests": [req(["x.gguf"]), req(["y.gguf"])]},
    {"id": "holders-env", "files": [f("x.gguf"), f("y.gguf"), f("z.gguf"), f("w.gguf")],
     "ini": "[short]\nmodel = /models/w.gguf\n\n[z]\nctx-size = 4\n",
     "docker": [container("web", cid="w1", env=["EMBEDDING_MODEL=x", "RERANK_MODEL=y", "LLM_EMBED_MODEL=default",
                                                 "EMBED_MODEL= short ", "NOISE"]),
                container("model-loader", cid="ml", env=["EMBEDDING_MODEL=z"]),
                container("self", cid="abc123", env=["EMBEDDING_MODEL=z"])],
     "env": {"HOSTNAME": "abc"},
     "requests": [req(["y.gguf"]), req(["x.gguf"]), req(["z.gguf"]), req(["w.gguf"])]},
    {"id": "holders-rerank-on", "files": [f("y.gguf")],
     "docker": [container("diary", env=["RERANK_MODEL=y.gguf", "NOEVIA_FEATURE_RAG_RERANK=On"])],
     "requests": [req(["y.gguf"])]},
    {"id": "embed-guard", "files": [f("x.gguf"), f("y.gguf"), f("z.gguf")],
     "env": {"NOEVIA_EMBED_SERVICE_GUARDED": "1", "NOEVIA_EMBED_SERVICE_CONTAINER": "embed",
             "NOEVIA_EMBED_SERVICE_URL": "http://Embed:8080/v1"},
     "docker": [container("embed", cid="e1", env=["EMBEDDING_MODEL=x"]),
                container("web", cid="w1", env=["EMBEDDING_MODEL=y", "EMBEDDING_BASE_URL=http://embed:8080"]),
                container("diary", cid="d1", env=["LLM_EMBED_MODEL=z", "LLM_EMBED_BASE_URL=http://gpu-box:8080"])],
     "requests": [req(["x.gguf"]), req(["y.gguf"]), req(["z.gguf"])]},
    {"id": "docker-fails", "files": [f("x.gguf")], "docker": "fail",
     "requests": [req(["nope.gguf"]), req(["x.gguf"])]},
    {"id": "no-models-dir", "models": "missing", "requests": [req(["x.gguf"])]},
    {"id": "linked-file", "files": [f("real/b.gguf", 77)], "links": [["a.gguf", "real/b.gguf"]],
     "requests": [req(["a.gguf"]), req(["real/b.gguf"])]},
    {"id": "linked-folder-inside", "files": [f("sub/a.gguf"), f("other/z.gguf", 33), f("other/n.txt", 4)],
     "links": [["sub/link", "../other"]], "requests": [req(["sub/a.gguf"])]},
    {"id": "companion-alone", "files": [f("sub/mmproj-x.gguf"), f("mmproj-flat.gguf"), f("solo/x-draft-Q4.gguf")],
     "requests": [req(["sub/mmproj-x.gguf"]), req(["mmproj-flat.gguf"]), req(["solo/x-draft-Q4.gguf"])]},
    {"id": "flat-companion-stays", "files": [f("a.gguf"), f("b.gguf"), f("mmproj-a.gguf")],
     "requests": [req(["a.gguf"])]},
    {"id": "readonly-folder", "files": [f("ro/a.gguf"), f("ro/b.gguf")], "readonly": ["ro"],
     "requests": [req(["ro/a.gguf"])]},
    {"id": "case-and-sort", "files": [f("Up/B.GGUF"), f("sub/a.gguf"), f("A.gguf")],
     "requests": [req(["Up/B.GGUF", "sub/a.gguf", "A.gguf"])]},
    {"id": "models-is-a-file", "models": "file", "requests": [req(["x.gguf"])]},
    # noevia#1964: the second key is "not found"; `sub/dup.gguf` (nobody named it) stays.
    {"id": "flat-fallback", "files": [f("dup.gguf", 3), f("sub/dup.gguf", 4), f("sub/other.gguf", 5)],
     "requests": [req(["dup.gguf", "dup.gguf"])]},
    # noevia#1966: a partial download beside or below the model is another model on its way, so
    # the whole-folder delete degrades to per-file and the partial file stays.
    {"id": "partial-download-beside", "files": [f("sub/a.gguf", 6), f("sub/b.gguf.download", 7),
                                                f("sub/notes.txt", 2)],
     "requests": [req(["sub/a.gguf"])]},
    {"id": "partial-download-below", "files": [f("sub/a.gguf", 6), f("sub/mmproj-a.gguf", 3),
                                               f("sub/old/c.GGUF.download", 7)],
     "requests": [req(["sub/a.gguf"])]},
]


def build(root: Path, case: dict) -> Path:
    models = root / "models"
    if case.get("models") == "file":
        models.write_bytes(b"")
    elif case.get("models") != "missing":
        models.mkdir()
    for spec in case.get("files", []):
        path = models / spec["name"]
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, "wb") as handle:
            handle.truncate(spec["size"])
    for name, target in case.get("links", []):
        path = models / name
        path.parent.mkdir(parents=True, exist_ok=True)
        os.symlink(target, path)
    return models


def tree(root: Path) -> list:
    out = []
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        here = Path(dirpath)
        for name in sorted(dirnames + filenames):
            p = here / name
            rel = p.relative_to(root).as_posix()
            if p.is_symlink():
                out.append([rel, "link", os.readlink(p)])
            elif p.is_dir():
                out.append([rel, "dir", None])
            else:
                out.append([rel, "file", p.stat().st_size])
    return sorted(out)


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith(("NOEVIA_", "MODELS_", "MODEL_", "GGUF_", "LLAMA_", "EMBED")) \
                or key.upper() in {"DATA_DIR", "DOCKER_HOST", "HOSTNAME", "RERANK_MODEL"}:
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from docker.errors import APIError  # noqa: E402
    from fastapi.testclient import TestClient  # noqa: E402

    from app import config, services  # noqa: E402
    from app.main import app  # noqa: E402

    client = TestClient(app, raise_server_exceptions=False)  # no lifespan: no sampler, no db
    names = ("api.py", "main.py", "services.py", "ini.py", "utils.py", "config.py")
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest() for name in names}
    lists = [0]

    class Fake:
        def __init__(self, spec):
            self.name = spec["name"]
            self.id = spec["id"]
            cfg = {"Env": spec["env"]}
            if spec["cmd"] is not None:
                cfg["Cmd"] = spec["cmd"]
            if spec["entrypoint"] is not None:
                cfg["Entrypoint"] = spec["entrypoint"]
            self.attrs = {"Id": spec["id"], "Name": "/" + spec["name"], "Config": cfg, "Mounts": spec["mounts"]}
            if spec["args"] is not None:
                self.attrs["Args"] = spec["args"]

    class Containers:
        def __init__(self, specs):
            self.specs = specs

        def list(self):
            lists[0] += 1
            if self.specs == "fail":
                raise APIError("synthetic: socket refused")
            return [Fake(s) for s in self.specs]

    class Client:
        def __init__(self, specs):
            self.containers = Containers(specs)

    out = []
    tmp_info = tempfile.mkdtemp()
    for case in CASES:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            models = build(root, case)
            env = case.get("env", {})
            for key in ("HOSTNAME", "NOEVIA_EMBED_SERVICE_GUARDED", "NOEVIA_EMBED_SERVICE_CONTAINER",
                        "NOEVIA_EMBED_SERVICE_URL"):
                os.environ.pop(key, None)
                if key in env:
                    os.environ[key] = env[key]
            config.settings.models_dir = models
            config.settings.models_ini_path = models / "models.ini"
            config.settings.models_host_path = env.get("MODELS_HOST_PATH", "")
            config.settings.model_download_targets = env.get("MODEL_DOWNLOAD_TARGETS", "")
            if case.get("ini") is not None:
                (models / "models.ini").write_text(case["ini"], encoding="utf-8")
            # Outside the recorded tree: its size depends on the temporary folder's name.
            mountinfo = Path(tmp_info) / "mountinfo"
            mountinfo.unlink(missing_ok=True)
            if case.get("mountinfo") is not None:
                text = "\n".join(case["mountinfo"]).replace("@MODELS@", str(models)) + "\n"
                mountinfo.write_text(text, encoding="utf-8")
            services.MOUNTINFO_PATH = str(mountinfo)
            docker_spec = case.get("docker")
            services._docker_client = (lambda: None) if docker_spec is None else (lambda s=docker_spec: Client(s))
            for name in case.get("readonly", []):
                os.chmod(models / name, 0o555)
            results = []
            for request in case["requests"]:
                lists[0] = 0
                headers = {} if request["content_type"] is None else {"content-type": request["content_type"]}
                response = client.post("/api/v1/models/delete", content=request["body"].encode(), headers=headers)
                results.append({"status": response.status_code, "content_type": response.headers.get("content-type"),
                                "body": response.content.decode("utf-8").replace(str(root), "@ROOT@"),
                                "docker_lists": lists[0], "tree": tree(root)})
            for name in case.get("readonly", []):
                os.chmod(models / name, 0o755)
            out.append({**case, "results": results})
    import shutil
    shutil.rmtree(tmp_info)
    json.dump({"oracle": digest, "cases": out}, sys.stdout, ensure_ascii=True, indent=0, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
