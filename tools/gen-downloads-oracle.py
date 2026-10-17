#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/downloads-python.v1.json (noevia#1318 slice 10).

Runs the ACTUAL noevia-services model-manager application: its FastAPI app, the planning
request the native parent sends (`x-noevia-download-plan: 1` under
NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOADS_CONFIRMED=1), `downloader.DownloadManager` and `api._job`.
Each case runs against a scripted fake file server (a "hub" origin, which may receive the token,
and a "cdn" origin), with synthetic files only. It records:

  * for every start: the child's planning answer (status, body, the files it would queue), which
    the native test serves from a fake child, and which is the public route's own answer;
  * the job table after each listed step, the files left in the models folder (with sha256 and
    validator contents), the download_history rows, and every request the fake servers received.

Speeds, ETAs, histories and timestamps depend on timing and are compared only as "set or not".
The concurrent cases compare request lists sorted. The exact rendering of those fields is
recorded separately (`render`) from crafted jobs with a pinned clock, together with `_validator`,
`_http_date` and `safe_dest` cases.

    python3 -I tools/gen-downloads-oracle.py <services>/model-manager/app > fixture.json

Every name, repository, token and byte is synthetic; nothing reaches a real host.
"""
from __future__ import annotations

import hashlib
import json
import os
import socket
import socketserver
import sqlite3
import sys
import tempfile
import threading
import time
from pathlib import Path
from urllib.parse import urlsplit

TOKEN = "hf_synthetic_download_token_0123"
REPO = "synthetic-owner/synthetic-repo"
BIG = "synthetic-owner/synthetic-big"
CHUNK = 32 * 1024
PARALLEL_MIN = 64 * 1024
# httpx's `aiter_bytes(1024 * 1024)` hands Python its progress a full MiB at a time; a stalled
# body stops exactly there, so both sides report the same count while it waits.
STALL = 1024 * 1024


def content(seed: int, n: int) -> bytes:
    return bytes((seed + i * 7) % 251 for i in range(n))


# ---------------------------------------------------------------------------------------------
# The fake file server. Tests/downloads.rs implements the same rules; keep them in step.
# A file spec: {"server", "path", "size", "seed", optional "etag", "last_modified", "date",
# "ranges" (Accept-Ranges and ranged GETs), "redirect": {"server", "path"}, "redirect_loop",
# "status" and "status_body" (HEAD and GET answer that status), "head_size", "get_size",
# "range_mode" (honor | ignore | 416 | wrong_offset | other_total), "fail_range_start",
# "bad_range_start", "stall_after", "truncate_after"}.
# ---------------------------------------------------------------------------------------------

class Fake:
    def __init__(self):
        self.files: dict = {}
        self.log: list = []
        self.logging = True
        self.release = threading.Event()
        self.ports: dict = {}
        self.lock = threading.Lock()

    def host(self, server: str) -> str:
        return f"127.0.0.1:{self.ports[server]}"


FAKE = Fake()


def answer(sock, status: int, headers: list, body: bytes = b"", send_body: bool = True):
    head = f"HTTP/1.1 {status} X\r\n" + "".join(f"{k}: {v}\r\n" for k, v in headers)
    head += "Connection: close\r\n\r\n"
    sock.sendall(head.encode("latin-1") + (body if send_body else b""))


def handle(server: str, sock):
    data = b""
    while b"\r\n\r\n" not in data:
        piece = sock.recv(65536)
        if not piece:
            return
        data += piece
    head = data.split(b"\r\n\r\n", 1)[0].decode("latin-1").split("\r\n")
    method, target, _ = head[0].split(" ", 2)
    headers = {}
    for line in head[1:]:
        k, _, v = line.partition(":")
        headers[k.strip().lower()] = v.strip()
    path, _, query = target.partition("?")
    with FAKE.lock:
        if FAKE.logging:
            FAKE.log.append([server, method, target, headers.get("range"), headers.get("if-range"),
                             headers.get("authorization")])
    spec = FAKE.files.get((server, path))
    if spec is None:
        answer(sock, 404, [("Content-Length", "9")], b"not found", method != "HEAD")
        return
    get = method == "GET"
    if spec.get("redirect_loop"):
        n = int(query.removeprefix("r=") or 0) + 1
        answer(sock, 302, [("Location", f"http://{FAKE.host(server)}{path}?r={n}"), ("Content-Length", "0")])
        return
    if spec.get("redirect"):
        to = spec["redirect"]
        answer(sock, 302, [("Location", f"http://{FAKE.host(to['server'])}{to['path']}"), ("Content-Length", "0")])
        return
    size = spec["size"]
    body = content(spec["seed"], size)
    meta = []
    if spec.get("ranges"):
        meta.append(("Accept-Ranges", "bytes"))
    for key, name in (("etag", "ETag"), ("last_modified", "Last-Modified"), ("date", "Date")):
        if spec.get(key) is not None:
            meta.append((name, spec[key]))
    if spec.get("status"):
        text = spec.get("status_body", "").encode()
        answer(sock, spec["status"], [("Content-Length", str(len(text)))], text, get)
        return
    if not get:
        answer(sock, 200, [("Content-Length", str(spec.get("head_size", size)))] + meta, b"", False)
        return
    wanted = headers.get("range")
    if wanted and (spec.get("ranges") or spec.get("range_mode")):
        first, _, last = wanted.removeprefix("bytes=").partition("-")
        first = int(first)
        if last:
            last = int(last)
            if first == spec.get("fail_range_start"):
                answer(sock, 200, [("Content-Length", str(size))] + meta, body)
                return
            total = size + 1 if first == spec.get("bad_range_start") else size
            part = body[first:last + 1]
            answer(sock, 206, [("Content-Length", str(len(part))),
                               ("Content-Range", f"bytes {first}-{last}/{total}")] + meta, part)
            return
        mode = spec.get("range_mode", "honor")
        current = spec.get("etag") or spec.get("last_modified")
        if_range = headers.get("if-range")
        if mode == "416":
            answer(sock, 416, [("Content-Length", "0"), ("Content-Range", f"bytes */{size}")])
            return
        if mode == "wrong_offset":
            answer(sock, 206, [("Content-Length", str(size)), ("Content-Range", f"bytes 0-{size - 1}/{size}")] + meta,
                   body)
            return
        if mode == "other_total":
            part = body[first:]
            answer(sock, 206, [("Content-Length", str(len(part))),
                               ("Content-Range", f"bytes {first}-{size - 1}/{size + 7}")] + meta, part)
            return
        if mode == "honor" and (if_range is None or if_range == current):
            part = body[first:]
            answer(sock, 206, [("Content-Length", str(len(part))),
                               ("Content-Range", f"bytes {first}-{size - 1}/{size}")] + meta, part)
            return
    full = content(spec["seed"], spec.get("get_size", size))
    if spec.get("truncate_after") is not None:
        answer(sock, 200, [("Content-Length", str(len(full)))] + meta, full[:spec["truncate_after"]])
        return
    if spec.get("stall_after") is not None:
        n = spec["stall_after"]
        answer(sock, 200, [("Content-Length", str(len(full)))] + meta, full[:n])
        FAKE.release.wait(30)
        try:
            sock.sendall(full[n:])
        except OSError:
            pass
        return
    answer(sock, 200, [("Content-Length", str(len(full)))] + meta, full)


def serve(server: str) -> socketserver.ThreadingTCPServer:
    class Handler(socketserver.BaseRequestHandler):
        def handle(self):  # noqa: D401
            try:
                handle(server, self.request)
            except OSError:
                pass

    class Server(socketserver.ThreadingTCPServer):
        request_queue_size = 128  # eight parallel ranges connect at once

    s = Server(("127.0.0.1", 0), Handler)
    s.daemon_threads = True
    FAKE.ports[server] = s.server_address[1]
    threading.Thread(target=s.serve_forever, daemon=True).start()
    return s


def dead_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


# ---------------------------------------------------------------------------------------------
# Cases.
# ---------------------------------------------------------------------------------------------

def cdn(path: str, size: int, seed: int, **kw) -> dict:
    return {"server": "cdn", "path": path, "size": size, "seed": seed, **kw}


def hub(path: str, size: int, seed: int, **kw) -> dict:
    return {"server": "hub", "path": path, "size": size, "seed": seed, **kw}


def url(server: str, path: str, **extra) -> dict:
    return {"op": "start", "path": "/api/v1/downloads", "body": {"url": f"http://{{{server.upper()}}}{path}", **extra}}


def repo(name: str, path: str, **extra) -> dict:
    return {"op": "start", "path": "/api/v1/downloads", "body": {"repo": name, "path": path, **extra}}


IDLE = {"op": "wait", "until": "idle"}
LIST = {"op": "list"}
RESOLVE = "/" + REPO + "/resolve/main/"
REPO_FILES = [("Synthetic-Q4_K_M.gguf", 3000), ("mmproj-synthetic-f16.gguf", 500),
              ("synthetic-draft-0.5B-Q8_0.gguf", 200)]
REPOS = {REPO: REPO_FILES, BIG: [("Synthetic-Big-Q8_0.gguf", 8 * CHUNK)]}


def partial(name: str, seed: int, total: int, keep: int, validator: str | None) -> dict:
    return {"op": "partial", "file": name, "seed": seed, "size": keep, "validator": validator, "total": total}


RESUME = "/files/synthetic-resume.gguf"
RESUME_TEMP = "synthetic-resume/synthetic-resume.gguf.download"
LM = "Mon, 01 Jan 2024 00:00:00 GMT"

CASES = [
    {"id": "url-small", "files": [cdn("/files/synthetic-small.gguf", 4096, 1, etag='"v1"')],
     "steps": [url("cdn", "/files/synthetic-small.gguf"), IDLE, LIST]},
    {"id": "url-target-and-name", "targets": ["archive"],
     "files": [cdn("/files/x.gguf", 1500, 2)],
     "steps": [url("cdn", "/files/x.gguf", filename="custom-name.gguf", target="archive"), IDLE, LIST]},
    {"id": "hub-companions", "token": TOKEN,
     "files": [hub(RESOLVE + n, s, 10 + i, redirect={"server": "cdn", "path": "/blobs/" + n})
               for i, (n, s) in enumerate(REPO_FILES)]
     + [cdn("/blobs/" + n, s, 10 + i, etag=f'"b{i}"') for i, (n, s) in enumerate(REPO_FILES)],
     "steps": [repo(REPO, "Synthetic-Q4_K_M.gguf"), IDLE, LIST]},
    {"id": "parallel", "token": TOKEN,
     "files": [hub("/" + BIG + "/resolve/main/Synthetic-Big-Q8_0.gguf", 8 * CHUNK, 5, ranges=True, etag='"big"')],
     "steps": [repo(BIG, "Synthetic-Big-Q8_0.gguf"), IDLE, LIST]},
    {"id": "parallel-chunk-200", "requests": False, "progress": False,
     "files": [hub("/" + BIG + "/resolve/main/Synthetic-Big-Q8_0.gguf", 8 * CHUNK, 5, ranges=True,
                   fail_range_start=3 * CHUNK)],
     "steps": [repo(BIG, "Synthetic-Big-Q8_0.gguf"), IDLE, LIST]},
    {"id": "parallel-bad-range", "requests": False, "progress": False,
     "files": [hub("/" + BIG + "/resolve/main/Synthetic-Big-Q8_0.gguf", 8 * CHUNK, 5, ranges=True,
                   bad_range_start=0)],
     "steps": [repo(BIG, "Synthetic-Big-Q8_0.gguf"), IDLE, LIST]},
    {"id": "resume-206", "files": [cdn(RESUME, 3000, 7, etag='"r1"', range_mode="honor")],
     "steps": [partial(RESUME_TEMP, 7, 3000, 1000, '"r1"\t3000'), url("cdn", RESUME), IDLE, LIST]},
    {"id": "resume-no-validator", "files": [cdn(RESUME, 3000, 7, etag='"r1"', range_mode="honor")],
     "steps": [partial(RESUME_TEMP, 7, 3000, 1000, None), url("cdn", RESUME), IDLE, LIST]},
    {"id": "resume-changed-upload", "files": [cdn(RESUME, 3000, 8, etag='"new"', range_mode="honor")],
     "steps": [partial(RESUME_TEMP, 7, 3000, 1000, '"old"\t3000'), url("cdn", RESUME), IDLE, LIST]},
    {"id": "resume-416", "files": [cdn(RESUME, 3000, 7, etag='"r1"', range_mode="416")],
     "steps": [partial(RESUME_TEMP, 7, 3000, 1000, '"r1"\t3000'), url("cdn", RESUME), IDLE, LIST]},
    {"id": "resume-wrong-offset", "files": [cdn(RESUME, 3000, 7, etag='"r1"', range_mode="wrong_offset")],
     "steps": [partial(RESUME_TEMP, 7, 3000, 1000, '"r1"\t3000'), url("cdn", RESUME), IDLE, LIST]},
    {"id": "resume-other-total", "files": [cdn(RESUME, 3000, 7, etag='"r1"', range_mode="other_total")],
     "steps": [partial(RESUME_TEMP, 7, 3000, 1000, '"r1"\t3000'), url("cdn", RESUME), IDLE, LIST]},
    {"id": "resume-last-modified",
     "files": [cdn(RESUME, 3000, 7, last_modified=LM, date="Tue, 02 Jan 2024 00:00:00 GMT", range_mode="honor")],
     "steps": [partial(RESUME_TEMP, 7, 3000, 1000, LM + "\t3000"), url("cdn", RESUME), IDLE, LIST]},
    {"id": "recent-last-modified-weak-etag",
     "files": [cdn(RESUME, 3000, 7, etag='W/"w"', last_modified=LM, date="Mon, 01 Jan 2024 00:00:10 GMT",
                   range_mode="honor")],
     "steps": [url("cdn", RESUME), IDLE, LIST]},
    {"id": "http-404", "files": [cdn("/files/missing.gguf", 10, 1, status=404, status_body="synthetic not found")],
     "steps": [url("cdn", "/files/missing.gguf"), IDLE, LIST]},
    {"id": "oversized", "files": [cdn("/files/over.gguf", 100, 3, head_size=100, get_size=150)],
     "steps": [url("cdn", "/files/over.gguf"), IDLE, LIST]},
    {"id": "short", "files": [cdn("/files/short.gguf", 100, 3, get_size=60, etag='"s"')],
     "steps": [url("cdn", "/files/short.gguf"), IDLE, LIST]},
    {"id": "truncated", "files": [cdn("/files/cut.gguf", 31, 4, truncate_after=10, etag='"c"')],
     "steps": [url("cdn", "/files/cut.gguf"), IDLE, LIST]},
    {"id": "cancel-running", "files": [cdn("/files/slow.gguf", STALL + 1000, 6, etag='"slow"', stall_after=STALL)],
     "steps": [url("cdn", "/files/slow.gguf"), {"op": "wait", "until": {"file": "slow/slow.gguf", "downloaded": STALL}},
               LIST, {"op": "cancel", "file": "slow/slow.gguf"}, IDLE, LIST, {"op": "release"}]},
    {"id": "cancel-queued-and-clear", "concurrent": 1,
     "files": [cdn("/files/slow.gguf", STALL + 1000, 6, stall_after=STALL), cdn("/files/next.gguf", 500, 9)],
     "steps": [url("cdn", "/files/slow.gguf"), {"op": "wait", "until": {"file": "slow/slow.gguf", "downloaded": STALL}},
               partial("next/next.gguf.download", 9, 500, 100, None),
               url("cdn", "/files/next.gguf"), {"op": "wait", "until": {"file": "next/next.gguf", "status": "queued"}},
               LIST, {"op": "cancel", "file": "next/next.gguf"}, {"op": "cancel_unknown"},
               {"op": "wait", "until": {"file": "next/next.gguf", "status": "canceled"}}, {"op": "release"}, IDLE,
               LIST, {"op": "clear"}, LIST]},
    {"id": "dedupe", "files": [cdn("/files/slow.gguf", STALL + 1000, 6, stall_after=STALL)],
     "steps": [url("cdn", "/files/slow.gguf"), {"op": "wait", "until": {"file": "slow/slow.gguf", "downloaded": STALL}},
               url("cdn", "/files/slow.gguf"), LIST, {"op": "release"}, IDLE, LIST]},
    {"id": "redirect-loop", "token": TOKEN, "files": [hub("/files/loop.gguf", 10, 1, redirect_loop=True)],
     "steps": [url("hub", "/files/loop.gguf"), IDLE, LIST]},
    {"id": "refused", "steps": [url("dead", "/files/x.gguf"), IDLE, LIST]},
    {"id": "token-origin-only", "token": TOKEN,
     "files": [cdn("/files/c.gguf", 300, 1), hub("/files/h.gguf", 300, 2)],
     "steps": [url("cdn", "/files/c.gguf"), IDLE, url("hub", "/files/h.gguf"), IDLE, LIST]},
    {"id": "draft-head", "token": TOKEN, "section": True,
     "files": [hub(RESOLVE + "synthetic-draft-0.5B-Q8_0.gguf", 200, 12)],
     "steps": [{"op": "start", "path": "/api/v1/sections/synthetic-model/draft-heads/download",
                "body": {"path": "synthetic-draft-0.5B-Q8_0.gguf"}}, IDLE, LIST]},
    {"id": "plan-refusals",
     "steps": [{"op": "start", "path": "/api/v1/downloads", "body": {"url": "ftp://example.test/x.gguf"}},
               {"op": "start", "path": "/api/v1/downloads", "body": {"target": "nowhere"}},
               repo(REPO, "absent.gguf"),
               {"op": "start", "path": "/api/v1/downloads", "raw": "[1]"},
               {"op": "start", "path": "/api/v1/downloads", "raw": "{not json"},
               LIST]},
]


def placeholders(text: str, dead: int) -> str:
    for server in ("hub", "cdn"):
        text = text.replace(FAKE.host(server), "{" + server.upper() + "}")
    return text.replace(f"127.0.0.1:{dead}", "{DEAD}")


def normal_job(j: dict, dead: int, progress: bool = True) -> dict:
    if not progress:
        # Which sibling ranges finish before a failing one cancels them depends on timing.
        j = {**j, "downloaded": None, "pct": None, "chunks": []}
    return {"repo": placeholders(j["repo"], dead), "filename": j["filename"], "status": j["status"],
            "error": None if j["error"] is None else placeholders(j["error"], dead), "bytes": j["bytes"],
            "downloaded": j["downloaded"], "pct": j["pct"], "started": j["startedAt"] is not None,
            "completed": j["completedAt"] is not None, "parallel": j["parallel"],
            "chunks": [{"index": c["index"], "pct": c["pct"], "status": c["status"]} for c in j["chunks"]],
            "keys": list(j.keys())}


def files_under(root: Path) -> list:
    out = []
    for p in sorted(root.rglob("*")):
        if p.is_file():
            data = p.read_bytes()
            entry = {"path": str(p.relative_to(root)), "size": len(data),
                     "sha256": hashlib.sha256(data).hexdigest()}
            if p.name.endswith(".validator"):
                entry["text"] = data.decode()
            out.append(entry)
    return out


# ---------------------------------------------------------------------------------------------
# Pure pieces: rendering with a pinned clock, the validator and HTTP dates, safe_dest.
# ---------------------------------------------------------------------------------------------

def render_cases(api, downloader):
    from fastapi.responses import JSONResponse
    from pathlib import Path as P
    now = 1700000100.0
    out = []
    specs = [
        {"total": 0, "downloaded": 0, "samples": [], "status": "queued"},
        {"total": 3000, "downloaded": 1000, "samples": [[1700000096.0, 0], [1700000096.5, 400],
                                                        [1700000097.25, 1000]], "status": "downloading"},
        {"total": 10 ** 10, "downloaded": 123456789, "samples": [[1700000094.0, 0], [1700000095.0, 50],
                                                                  [1700000099.0, 123456789]],
         "status": "downloading", "started": 1700000094.0},
        {"total": 5, "downloaded": 5, "samples": [[1700000000.0, 0], [1700000000.0, 5]], "status": "done",
         "started": 1700000000.0, "completed": 1700000001.5},
        {"total": 7, "downloaded": 3, "samples": [[1700000099.0, 0], [1700000100.0, 3]], "status": "error",
         "error": "RuntimeError: synthetic “quoted” failure\n", "started": 1.0, "completed": 2.0},
        {"total": 1024 ** 3, "downloaded": 1, "samples": [[1700000099.9, 0], [1700000100.0, 1]],
         "status": "downloading"},
        {"total": 8 * 3, "downloaded": 9, "parallel": True, "status": "downloading",
         "samples": [[t, b] for t, b in zip([1700000090.0 + 0.5 * i for i in range(130)], range(0, 130 * 7, 7))],
         "chunks": [{"idx": 0, "start": 0, "end": 2, "downloaded": 3, "samples": [[1700000099.0, 0], [1700000099.5, 3]]},
                    {"idx": 1, "start": 3, "end": 5, "downloaded": 0, "samples": []},
                    {"idx": 2, "start": 6, "end": 8, "downloaded": 1, "samples": [[1700000099.0, 1]]},
                    {"idx": 3, "start": 9, "end": 8, "downloaded": 3, "samples": []}]},
        {"total": 3, "downloaded": 9, "status": "downloading", "samples": [[1700000098.0, 9], [1700000099.0, 0]]},
    ]
    for i, s in enumerate(specs):
        job = downloader.DownloadJob(f"render{i:06d}", "synthetic-owner/r", f"f{i}.gguf", "http://x/f",
                                     P("/m/f"), P("/m/f.download"), s["total"])
        job.downloaded_bytes = s["downloaded"]
        job.status = s["status"]
        job.error = s.get("error")
        job.started_at = s.get("started")
        job.completed_at = s.get("completed")
        job.parallel = s.get("parallel", False)
        job._speed_samples = [tuple(x) for x in s["samples"]]
        for c in s.get("chunks", []):
            cs = downloader.ChunkState(idx=c["idx"], start=c["start"], end=c["end"], downloaded=c["downloaded"])
            cs._samples = [tuple(x) for x in c["samples"]]
            job.chunks.append(cs)
        real = downloader.time.time
        downloader.time.time = lambda: now
        try:
            body = JSONResponse(api._job(job)).body.decode()
        finally:
            downloader.time.time = real
        out.append({"spec": s, "json": body})
    return {"now": now, "cases": out}


def validator_cases(downloader):
    import httpx
    now = 1700000000.0
    headers = [
        {"etag": '"strong"'}, {"etag": 'W/"weak"'}, {"etag": "  "}, {"etag": ' "padded" '},
        {"etag": 'W/"weak"', "last-modified": LM, "date": "Tue, 02 Jan 2024 00:00:00 GMT"},
        {"last-modified": LM, "date": "Mon, 01 Jan 2024 00:00:59 GMT"},
        {"last-modified": LM, "date": "Mon, 01 Jan 2024 00:01:00 GMT"},
        {"last-modified": LM},
        {"last-modified": "Sat, 01 Jan 2050 00:00:00 GMT"},
        {"last-modified": "garbage"},
        {"last-modified": "Mon, 01 Jan 2024 00:00:00 -0000", "date": "Tue, 02 Jan 2024 00:00:00 GMT"},
        {"last-modified": "Mon, 01 Jan 2024 00:00:00 +0100", "date": "Tue, 02 Jan 2024 00:00:00 GMT"},
        {"last-modified": "Mon, 01 Jan 2024 00:00:00 EST", "date": "garbage"},
        {"last-modified": "Monday, 01-Jan-24 00:00:00 GMT", "date": "Tue, 02 Jan 2024 00:00:00 GMT"},
        {"last-modified": "Mon Jan  1 00:00:00 2024", "date": "Tue, 02 Jan 2024 00:00:00 GMT"},
    ]
    dates = ["Mon, 01 Jan 2024 00:00:00 GMT", "01 Jan 2024 00:00:00 GMT", "Mon,01 Jan 2024 00:00 UT",
             "1 jan 69 23:59:59 +0000", "1 jan 68 23:59:59 +0000", "Jan 1 2024 10:00:00 PDT",
             "Mon, 31 Feb 2024 00:00:00 GMT", "Mon, 01 Jan 2024 24:00:00 GMT", "Mon, 01 Jan 2024 00:00:00",
             "Mon, 01 Jan 2024 00.00.00 GMT", "Mon, 01 Jan 2024 00:00:00 +2400", "Mon, 01 Jan 2024 00:00:00 +2359",
             "Mon, 01 Jan 0000 00:00:00 GMT", "Mon, 01 Jan 1_9_9_9 00:00:00 GMT", "", "   ", "x",
             "Mon, 01 January 2024 00:00:00 z", "2024 Jan 01 00:00:00 GMT", "Mon, 01 Jan 2024 00:00:00 XYZ",
             "Tue, 02 Jan 2024 03:04:05 -0130", "Mon 01 Jan 2024 00:00:00 GMT", "01-Jan-2024 00:00:00 GMT",
             "Mon, 1 Jan 2024 0:0:0 GMT", "Mon, 01 Jan 2024 00:00:00 +01", "Mon, 01 Jan 2024, 00:00:00 GMT"]
    real = downloader.time.time
    downloader.time.time = lambda: now
    try:
        validators = [{"headers": h, "validator": downloader._validator(httpx.Headers(h))} for h in headers]
    finally:
        downloader.time.time = real
    return {"now": now, "validators": validators,
            "dates": [{"value": d, "time": downloader._http_date(d)} for d in dates]}


SAFE_NAMES = ["a.gguf", "sub/a.gguf", "./a.gguf", "sub/../a.gguf", "../escape.gguf", "/abs/elsewhere.gguf",
              "inside/x.gguf", "outside/x.gguf", "loop/x.gguf", "", ".", "sub/", "inside/../../x.gguf",
              "sub/./b//c.gguf", "dangling/x.gguf"]


def safe_dest_cases(downloader, config):
    with tempfile.TemporaryDirectory() as root:
        base = Path(root).resolve()
        models = base / "models"
        (models / "sub").mkdir(parents=True)
        (base / "elsewhere").mkdir()
        (models / "inside").symlink_to(models / "sub")
        (models / "outside").symlink_to(base / "elsewhere")
        (models / "loop").symlink_to(models / "loop2")
        (models / "loop2").symlink_to(models / "loop")
        (models / "dangling").symlink_to(models / "nowhere")
        config.settings.models_dir = models
        out = []
        for name in SAFE_NAMES:
            try:
                dest = downloader.safe_dest(name)
                result = "inside:" + os.path.relpath(str(dest), str(models))
            except ValueError:
                result = "outside"
            except RuntimeError:
                result = "raised"
            out.append({"name": name, "result": result})
        return out


# ---------------------------------------------------------------------------------------------

def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith("NOEVIA_NATIVE_") or key.upper() in {
                "MODEL_LOADER_TOKEN", "DATA_DIR", "MODELS_DIR", "MODELS_INI_PATH", "MODEL_DOWNLOAD_TARGETS",
                "MAX_CONCURRENT_DOWNLOADS"} or "PROXY" in key.upper():
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    from fastapi.testclient import TestClient  # noqa: E402

    from app import api, config, db, downloader, hf  # noqa: E402
    from app.main import app  # noqa: E402

    app.router.on_startup.clear()
    servers = [serve("hub"), serve("cdn")]
    dead = dead_port()
    hf.HF_RESOLVE = f"http://{FAKE.host('hub')}"
    hub_port = FAKE.ports["hub"]

    def token_origin(u: str) -> bool:
        try:
            p = urlsplit(u)
            return (p.scheme == "http" and p.hostname == "127.0.0.1" and p.port == hub_port
                    and p.username is None and p.password is None and not any(c.isspace() for c in u))
        except ValueError:
            return False
    hf.is_token_origin = token_origin
    downloader.PARALLEL_MIN_SIZE = PARALLEL_MIN

    async def repo_detail(name, revision="main"):
        if name not in REPOS:
            raise RuntimeError("synthetic: no such repository")
        return hf.HfRepoDetail(id=name, files=[hf.HfFile(path=p, size=s, quant=None, shard_base=p, shard_index=None,
                                                         shard_total=None) for p, s in REPOS[name]],
                               readme_snippet=None)
    hf.repo_detail = repo_detail

    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest()
              for name in ("api.py", "downloader.py", "db.py", "hf.py", "helpers.py", "main.py", "utils.py")}
    render = render_cases(api, downloader)
    validators = validator_cases(downloader)
    safe = safe_dest_cases(downloader, config)
    out = []
    schema = None
    with TestClient(app, raise_server_exceptions=False) as client:
        for case in CASES:
            FAKE.files = {(f["server"], f["path"]): f for f in case.get("files", [])}
            FAKE.log = []
            FAKE.release = threading.Event()
            CREATED[0] = 0
            PROGRESS[0] = case.get("progress", True)
            with tempfile.TemporaryDirectory() as root:
                root = Path(root).resolve()
                models = root / "models"
                models.mkdir()
                for t in case.get("targets", []):
                    (models / t).mkdir()
                config.settings.models_dir = models
                config.settings.data_dir = root
                config.settings.model_download_targets = ",".join(case.get("targets", []))
                config.settings.max_concurrent_downloads = case.get("concurrent", 2)
                downloader.manager.jobs = {}
                downloader.manager._sema = None
                db._DB_PATH = None
                db.init()
                c = sqlite3.connect(root / "model_loader.db", isolation_level=None)
                if schema is None:
                    schema = [r[0] for r in c.execute(
                        "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' "
                        "AND name IN ('kv', 'download_history') ORDER BY rowid")]
                if case.get("token"):
                    c.execute("INSERT INTO kv(key, value) VALUES('hf_token', ?)", (case["token"],))
                c.close()
                if case.get("section"):
                    folder = models / "Synthetic"
                    folder.mkdir()
                    api._resolve_section_gguf = lambda name, f=folder: (f / "Synthetic-Q4_K_M.gguf",
                                                                       "Synthetic/Synthetic-Q4_K_M.gguf",
                                                                       "Synthetic/Synthetic-Q4_K_M.gguf")
                    api.db.repo_for_file = lambda name: REPO
                steps = []
                for step in case["steps"]:
                    steps.append(run_step(client, step, models, root, dead, downloader))
                jobs = wait(client, lambda jobs: not any(j["status"] in ("queued", "downloading") for j in jobs))
                FAKE.release.set()
                c = sqlite3.connect(root / "model_loader.db", isolation_level=None)
                # A job's row is written in its task's `finally`, just after its final state.
                end = time.monotonic() + 20
                while c.execute("SELECT count(*) FROM download_history").fetchone()[0] < CREATED[0]:
                    if time.monotonic() > end:
                        raise SystemExit("timed out waiting for download_history")
                    time.sleep(0.01)
                rows = sorted([[placeholders(r[0], dead), r[1], os.path.relpath(r[2], str(models)), r[3], r[4],
                                None if r[5] is None else placeholders(r[5], dead), r[6] is not None, r[7] is not None]
                               for r in c.execute("SELECT repo_id, filename, dest_path, total_bytes, status, error, "
                                                  "started_at, completed_at FROM download_history ORDER BY id")],
                              key=json.dumps)
                c.close()
                requests = None
                if case.get("requests", True):
                    requests = sorted(([s, m, placeholders(t, dead), r, i, a] for s, m, t, r, i, a in FAKE.log),
                                      key=json.dumps)
                out.append({"id": case["id"], "files": case.get("files", []), "token": case.get("token"),
                            "targets": case.get("targets", []), "concurrent": case.get("concurrent", 2),
                            "steps": steps, "models": files_under(models), "history": rows,
                            "requests": requests})
    for s in servers:
        s.shutdown()
    json.dump({"oracle": digest, "schema": schema, "parallel_min": PARALLEL_MIN, "render": render,
               "validator": validators, "safe_dest": safe, "cases": out}, sys.stdout, ensure_ascii=False,
              indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


CREATED = [0]  # jobs the current case created; each writes one download_history row
PROGRESS = [True]  # whether the current case's progress counts are deterministic


def jobs_of(client) -> list:
    return client.get("/api/v1/downloads").json()["jobs"]


def wait(client, done, deadline: float = 20.0) -> list:
    end = time.monotonic() + deadline
    while True:
        jobs = jobs_of(client)
        if done(jobs):
            return jobs
        if time.monotonic() > end:
            raise SystemExit(f"timed out waiting: {jobs}")
        time.sleep(0.01)


def run_step(client, step, models: Path, root: Path, dead: int, downloader) -> dict:
    op = step["op"]
    if op == "partial":
        p = models / step["file"]
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(content(step["seed"], step["total"])[:step["size"]])
        if step["validator"] is not None:
            p.with_name(p.name + ".validator").write_text(step["validator"])
        return dict(step)
    if op == "start":
        body = step.get("body")
        if body is not None:
            text = json.dumps(body).replace("{CDN}", FAKE.host("cdn")).replace("{HUB}", FAKE.host("hub"))
            text = text.replace("{DEAD}", f"127.0.0.1:{dead}")
        else:
            text = step["raw"]
        os.environ["NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOADS_CONFIRMED"] = "1"
        with FAKE.lock:
            FAKE.logging = False
        try:
            r = client.post(step["path"], content=text.encode(),
                            headers={"content-type": "application/json", "x-noevia-download-plan": "1"})
        finally:
            with FAKE.lock:
                FAKE.logging = True
            del os.environ["NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOADS_CONFIRMED"]
        assert r.status_code == 200, r.content
        plan = r.json()
        # The public route queues exactly these, in this order (`_make_job`), and answers with
        # the plan's own status and body.
        before = len(downloader.manager.jobs)
        client.portal.call(lambda: [downloader.manager._make_job(**j) for j in plan["jobs"]])
        CREATED[0] += len(downloader.manager.jobs) - before
        return {"op": "start", "path": step["path"], "request": text and placeholders(text, dead),
                "plan": json.loads(placeholders(json.dumps(plan), dead))}
    if op == "wait":
        until = step["until"]
        if until == "idle":
            wait(client, lambda jobs: not any(j["status"] in ("queued", "downloading") for j in jobs))
        else:
            def ready(jobs):
                for j in jobs:
                    if j["filename"] == until["file"]:
                        if "downloaded" in until:
                            return j["downloaded"] >= until["downloaded"]
                        return j["status"] == until["status"]
                return False
            wait(client, ready)
        return dict(step)
    if op == "list":
        return {"op": "list", "jobs": sorted((normal_job(j, dead, PROGRESS[0]) for j in jobs_of(client)),
                                             key=lambda j: j["filename"])}
    if op == "cancel":
        job = next(j for j in jobs_of(client) if j["filename"] == step["file"])
        r = client.post(f"/api/v1/downloads/{job['id']}/cancel")
        return {"op": "cancel", "file": step["file"], "status": r.status_code, "body": r.content.decode()}
    if op == "cancel_unknown":
        r = client.post("/api/v1/downloads/synthetic-unknown/cancel")
        return {"op": "cancel_unknown", "status": r.status_code, "body": r.content.decode()}
    if op == "clear":
        r = client.post("/api/v1/downloads/clear")
        return {"op": "clear", "status": r.status_code, "body": r.content.decode()}
    if op == "release":
        FAKE.release.set()
        return dict(step)
    raise SystemExit(f"unknown step {op}")


if __name__ == "__main__":
    raise SystemExit(main())
