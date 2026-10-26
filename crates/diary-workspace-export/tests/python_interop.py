"""Synthetic interoperability with the current Python authoring implementation.
Usage: python tests/python_interop.py SERVICES_CHECKOUT EXPORT_BINARY
Run through devrun. Does not touch actual Diary state or load models.
"""
import hashlib
import io
import json
from pathlib import Path
import random
import struct
import subprocess
import sys
import tempfile
import zipfile

sys.path.insert(0, str(Path(sys.argv[1]) / "diary"))
from agent.managed_storage import ManagedCorpusBackend
from agent.workspace_export import archive
from agent.workspace_restore import _verify

MAGIC = b"NOEVIA-WORKSPACE-EXPORT-1\n"
OK = b"NOEVIA-WORKSPACE-EXPORT-OK-1\n"
END = b"NOEVIA-WORKSPACE-EXPORT-END-1\n"
assert len(OK) + 8 + 32 + len(END) == 99

def authored(files, directories, settings):
    header = json.dumps({"settings": settings, "directories": directories,
                         "files": [{"path": p, "bytes": len(b)} for p, b in files.items()]},
                        ensure_ascii=False, allow_nan=False).encode()
    request = MAGIC + struct.pack(">I", len(header)) + header + b"".join(files.values())
    result = subprocess.run([sys.argv[2], "export"], input=request, capture_output=True, timeout=120)
    assert result.returncode == 0, (result.returncode, result.stdout[:80], result.stderr)
    assert result.stderr == b""
    assert result.stdout.startswith(OK)
    length = struct.unpack(">Q", result.stdout[len(OK):len(OK)+8])[0]
    digest = result.stdout[len(OK)+8:len(OK)+40]
    body = result.stdout[len(OK)+40:len(OK)+40+length]
    assert result.stdout[len(OK)+40+length:] == END
    assert hashlib.sha256(body).digest() == digest
    _verify(body)  # Actual Python import/restore interop, no external data.
    return body

def semantic(body):
    with zipfile.ZipFile(io.BytesIO(body)) as z:
        assert all(i.compress_type == zipfile.ZIP_STORED for i in z.infolist())
        return json.loads(z.read("manifest.json")), {n:z.read(n) for n in z.namelist() if n != "manifest.json"}

rng = random.Random(1071)
with tempfile.TemporaryDirectory(prefix="synthetic-workspace-export-") as root:
    for i in range(40):
        files = {} if i == 0 else {"日記/note.md": b"---\n# Synthetic\r\n\x00\xff",
                   "manifest.json": b"source manifest", **{
                       f"folder{n}/sub/file{n}.bin": rng.randbytes(rng.randrange(0, 5000))
                       for n in range(i % 17)}}
        directories = ["empty/nested", "空"]
        settings = [None, [], {}, {"unicode":"日記", "int": 10**60,
                    "float": -0.0, "nested": [True, False, None, 1e-12]}][i % 4]
        backend = ManagedCorpusBackend(Path(root)/str(i), "00000000-0000-0000-0000-000000000001")
        backend.activate(files, {}, directories)
        python = archive(backend, "", settings)
        rust = authored(files, directories, settings)
        assert semantic(python) == semantic(rust), i
print("40 synthetic managed Python/Rust export comparisons and Python restore interoperability passed")
