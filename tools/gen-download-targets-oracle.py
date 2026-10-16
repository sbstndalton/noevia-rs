#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/download-targets-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, middleware, router,
`api.download_targets` and `services._mount_points` / `_is_mount_point`) against synthetic models
folders and synthetic mount tables, and records the exact status, content type and body bytes of
`GET /api/v1/download-targets`:

    python3 -I tools/gen-download-targets-oracle.py <services>/model-manager/app > fixture.json

Every folder, name and mount line is synthetic. Each case is declarative (entries to create, the
MODEL_DOWNLOAD_TARGETS value and the mount table text with `{ROOT}`/`{REAL}` placeholders) so the
Rust test can build the identical tree. Bodies record the models folder as `{ROOT}`.
CI re-runs this against the pinned services ref and requires a byte-identical fixture.
"""
from __future__ import annotations

import hashlib
import json
import os
import sys
import tempfile
from pathlib import Path

# id, root ("plain" | "symlink" | "missing"), entries, declared, mountinfo (None: no table).
# entries: ["d", name] folder, ["f", name] file, ["l", name, target] symlink (target relative).
M = "36 25 0:32 / {path} rw,relatime shared:1 - ext4 /dev/synthetic rw"


def line(path: str) -> str:
    return M.format(path=path)


CASES = [
    ("empty-no-table", "plain", [], "", None),
    ("empty-table", "plain", [["d", "a"]], "", ""),
    ("declared-basic", "plain", [["d", "a"], ["d", "b"]], "a, b ,,c", None),
    ("declared-dedupe-sorted", "plain", [["d", "zeta"], ["d", "Alpha"], ["d", "beta"]],
     "zeta,beta,Alpha,beta", ""),
    ("declared-filtered", "plain", [["d", ".hidden"], ["d", "x"], ["f", "file"], ["d", "real"],
                                    ["l", "link", "real"]],
     ".hidden,x/y,file,link,missing,real", None),
    ("declared-whitespace", "plain", [["d", "d"], ["d", "e"], ["d", "f g"]],
     " d ,\x1fe\x1f,\tf g\n, ,\x1c", None),
    ("declared-only-commas", "plain", [["d", "a"]], ",,, ,", None),
    ("mount-listed", "plain", [["d", "m1"], ["f", "m2"], ["d", ".m3"], ["d", "plain"]], "",
     "\n".join([line("{ROOT}/m1"), line("{ROOT}/m2"), line("{ROOT}/.m3"), line("/"), line("{ROOT}")])),
    ("mount-escaped-space", "plain", [["d", "with space"], ["d", "tab\there"]], "",
     "\n".join([line("{ROOT}/with\\040space"), line("{ROOT}/tab\\011here")])),
    ("mount-normpath", "plain", [["d", "m1"], ["d", "m2"], ["d", "m3"], ["d", "m4"]], "",
     "\n".join([line("{ROOT}/m1/"), line("{ROOT}/./m2"), line("{ROOT}/x/../m3"), line("{ROOT}//m4")])),
    ("mount-line-separators", "plain", [["d", "a"], ["d", "b"], ["d", "c"], ["d", "d"], ["d", "e"]], "",
     line("{ROOT}/a") + "\r\n" + line("{ROOT}/b") + "\x85" + line("{ROOT}/c") + " "
     + line("{ROOT}/d") + "\x0b" + line("{ROOT}/e")),
    ("mount-short-lines", "plain", [["d", "a"], ["d", "b"]], "",
     "36 25 0:32 /\n" + "x x x x {ROOT}/a\n" + "only four fields here\n" + "a  b c {ROOT}/b"),
    ("mount-odd-escapes", "plain", [["d", "ǿ"], ["d", "q8"], ["d", "back\\slash"]], "",
     "\n".join([line("{ROOT}/\\777"), line("{ROOT}/q\\8"), line("{ROOT}/back\\134slash")])),
    ("mount-bytes-are-code-points", "plain", [["d", "ä"], ["d", "Ã¤"]], "",
     line("{ROOT}/\\303\\244")),
    ("mount-invalid-utf8-table", "plain", [["d", "a"], ["d", "b"]], "",
     {"hex": (line("{ROOT}/a") + "\n").encode().hex() + "ff fe".replace(" ", "")
      + ("\n" + line("{ROOT}/b")).encode().hex()}),
    ("mount-symlinked-folder", "plain", [["d", "real"], ["l", "link", "real"]], "",
     "\n".join([line("{ROOT}/link"), line("{ROOT}/real")])),
    ("mount-and-declared", "plain", [["d", "both"], ["d", "only-mount"], ["d", "only-decl"]],
     "both,only-decl", "\n".join([line("{ROOT}/both"), line("{ROOT}/only-mount")])),
    ("realpath-root", "symlink", [["d", "m1"], ["d", "m2"]], "",
     "\n".join([line("{REAL}/m1")])),
    ("missing-root", "missing", [], "a,b", line("{ROOT}/a")),
    ("unicode-sort-and-escapes", "plain",
     [["d", "été"], ["d", "Zed"], ["d", "ßx"], ["d", "q\"b\\"], ["d", "ctl\x01x"],
      ["d", "\U0001F600"], ["d", "del\x7f"]],
     "été,Zed,ßx,q\"b\\,ctl\x01x,\U0001F600,del\x7f", None),
]


def build(base: Path, root_mode: str, entries) -> Path:
    real = base / "real-models"
    if root_mode == "missing":
        return base / "no-such-models"
    real.mkdir()
    for entry in entries:
        kind, name = entry[0], entry[1]
        if kind == "d":
            (real / name).mkdir()
        elif kind == "f":
            (real / name).write_bytes(b"synthetic")
        else:
            os.symlink(entry[2], real / name)
    if root_mode == "symlink":
        link = base / "models-link"
        os.symlink(real, link)
        return link
    return real


def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {
                "MODEL_LOADER_TOKEN", "MODELS_DIR", "MODEL_DOWNLOAD_TARGETS"}:
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import config, services  # noqa: E402
    from app.main import app  # noqa: E402

    client = TestClient(app, raise_server_exceptions=False)  # no lifespan
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest()
              for name in ("api.py", "services.py", "main.py", "config.py")}
    out = []
    for case_id, root_mode, entries, declared, mountinfo in CASES:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(os.path.realpath(tmp))
            root = build(base, root_mode, entries)
            real = base / "real-models"
            table = base / "mountinfo"
            if mountinfo is not None:
                if isinstance(mountinfo, dict):
                    raw = bytes.fromhex(mountinfo["hex"])
                else:
                    raw = mountinfo.encode("utf-8")
                raw = raw.replace(b"{ROOT}", str(root).encode()).replace(b"{REAL}", str(real).encode())
                table.write_bytes(raw)
            services.MOUNTINFO_PATH = str(table)
            config.settings.models_dir = root
            config.settings.model_download_targets = declared
            response = client.get("/api/v1/download-targets")
            body = response.content.decode("utf-8")
            # The folder appears JSON-escaped; the synthetic temp paths need no escaping.
            body = body.replace(str(root), "{ROOT}")
            out.append({
                "id": case_id,
                "root": root_mode,
                "entries": entries,
                "declared": declared,
                "mountinfo": mountinfo,
                "status": response.status_code,
                "content_type": response.headers.get("content-type"),
                "body": body,
            })
    json.dump({"oracle": digest, "cases": out}, sys.stdout, ensure_ascii=False, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
