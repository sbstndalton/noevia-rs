"""Synthetic actual Python HTTP oracle; never import or load the Docling engine."""
import ast
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import socket
import sys
import tempfile
import threading

REF = "b25a677182097c5ed72221e01fee5d5587aa7043"
HASHES = {
    "server.py": "97bda681d83b3137fd4261c37d30df7cd261f55555ca8b62982c02f892d9d082",
    "isolation.py": "49bef971629aebaf68a63b358a021d702e3660dbe4876d6d05dc45e57b142983",
    "isolated_worker.py": "543fc179d5a58b1dfe919016a163d3786b93be8a44c232b2977e3004ba38b281",
    "extract.py": "76baed430a897ccd9e7cc5f3d5d927d368eda9b5e01dee8509874014cbb0e317",
}


def prepare(source, target):
    for name, expected in HASHES.items():
        assert hashlib.sha256((source / name).read_bytes()).hexdigest() == expected
    for name in ("server.py", "isolation.py", "isolated_worker.py", "fake_converter.py"):
        shutil.copyfile(source / name, target / name)
    tree = ast.parse((source / "extract.py").read_text())
    supported = next(ast.literal_eval(n.value) for n in tree.body if isinstance(n, ast.Assign)
                     and any(isinstance(t, ast.Name) and t.id == "SUPPORTED" for t in n.targets))
    # Only the engine collaborator changes. HTTP, child protocol and lifecycle are
    # the actual pinned production modules. No model imports or downloads.
    (target / "extract.py").write_text(
        "from fake_converter import extract as fake\nSUPPORTED=" + repr(supported) + "\n"
        "def extract(path,name):\n result=fake(path,name)\n result.pop('pid',None)\n return result\n")


def exchange(port, method, path, length, name, data):
    header = f"{method} {path} HTTP/1.0\r\nHost: localhost\r\n"
    if length is not None:
        header += f"Content-Length: {length}\r\n"
    if name is not None:
        header += f"X-Document-Name: {name}\r\n"
    with socket.create_connection(("127.0.0.1", port), timeout=10) as connection:
        connection.sendall(header.encode("latin-1") + b"\r\n" + data)
        reply = bytearray()
        while True:
            try:
                chunk = connection.recv(65536)
            except ConnectionResetError:
                break
            if not chunk:
                break
            reply.extend(chunk)
    headers, body = bytes(reply).split(b"\r\n\r\n", 1)
    size = next(int(line.split(b":", 1)[1]) for line in headers.split(b"\r\n")
                if line.lower().startswith(b"content-length:"))
    assert len(body) == (0 if method == "HEAD" else size)
    content_type = next(line.split(b":", 1)[1].strip().decode() for line in headers.split(b"\r\n")
                        if line.lower().startswith(b"content-type:"))
    return {"status": int(headers.split()[1]), "type": content_type,
            "body": json.loads(body) if content_type == "application/json" else body.decode()}


def main(source, output):
    with tempfile.TemporaryDirectory(prefix="noevia-docling-oracle-") as root:
        root = Path(root)
        prepare(source, root)
        sys.path.insert(0, str(root))
        spec = importlib.util.spec_from_file_location("server", root / "server.py")
        server = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(server)
        httpd = server.ThreadingHTTPServer(("127.0.0.1", 0), server.Handler)
        thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        thread.start()
        cases = [("GET", "/health", None, None, b""), ("GET", "//health", None, None, b""),
                 ("GET", "/health?x=1", None, None, b""), ("POST", "/unknown", None, None, b""),
                 ("POST", "/extract", None, "x.md", b""),
                 ("POST", "/extract", "26214401", "x.md", b""),
                 ("POST", "/extract", "1", None, b""),
                 ("POST", "/extract", "1", "x/y.md", b""),
                 ("POST", "/extract", "1", "x\\y.md", b""),
                 ("POST", "/extract", "1", "x"*201, b""),
                 ("POST", "/extract", "1", "x"*200, b"x"),
                 ("POST", "/extract", "1", "x.zip", b"x"),
                 ("POST", "/extract", "1", "x.md ", b"x"),
                 ("POST", "/extract", "+1", "x.MD", b"x"),
                 ("POST", "/extract", "1_0", "x.md", b"abcdefghij"),
                 ("POST", "/extract", "4", "x.md", b"BOOM")]
        cases += [("POST", "/extract", "1", "x" + suffix, b"x") for suffix in sorted(server.extractor.SUPPORTED)]
        results = []
        try:
            for method, path, length, name, data in cases:
                results.append({"method": method, "path": path, "length": length, "name": name,
                                "data": base64.b64encode(data).decode(),
                                "expected": exchange(httpd.server_address[1], method, path, length, name, data)})
        finally:
            httpd.shutdown()
            httpd.server_close()
            server.runner.close()
        output.write_text(json.dumps({"services": REF, "sources": HASHES, "cases": results}, indent=2) + "\n")
        print(f"actual Python Docling HTTP goldens: {len(results)}")


if __name__ == "__main__":
    main(Path(sys.argv[1]), Path(sys.argv[2]))
