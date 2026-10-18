#!/usr/bin/env python3
"""Generate bins/noevia-model-manager/tests/fixtures/search-python.v1.json (noevia#1318).

Runs the ACTUAL noevia-services model-manager application (its FastAPI app, middleware and router:
`api.search`, `api.search_repo`, `hf.search_models`, `hf.repo_detail`, `hf.gguf_header`,
`hf.owner_avatars`, `discover`, `helpers._downloaded_and_still_present`, `services.vram_fit_chips`,
`helpers._preset_estimates_with_reason`, `db`, and `model_files` through the real model-files
binary) against synthetic repositories, a synthetic models folder and store, with the Hub replaced
by a local fake on 127.0.0.1, and records for each request:

  * the exact status, content type and body of `GET /api/v1/search` or `GET /api/v1/search/repo`,
  * every request the fake Hub (and the fake CDN a redirect leads to) received: path and query,
    Authorization and Range headers,
  * the avatar cache after the request,
  * what the native side asks of the Python child (its two private calls) and the child's answers,
    obtained by calling the real private endpoints of the same app in the same state.

It also records Python's own answers for the grammars the native side must follow: the query
values pydantic accepts for an int, a float and a bool, `str.strip` and `int(str)`,
`datetime.fromisoformat` through `discover._age_days`, and `round`, `.1f`, `human_bytes` and int
true division.

    MODEL_FILES_BIN=<model-files> python3 -I tools/gen-search-oracle.py <services>/model-manager/app > fixture.json

Every repository, owner, file and token is synthetic. The trusted publisher sets are replaced by
neutral names, the clock, the backends and the hardware by fixed values. Nothing reaches the real Hub.
The fake Hub's own address is written as @HUB@ and the CDN's as @CDN@ in the headers they send.
"""
from __future__ import annotations

import hashlib
import http.server
import json
import os
import sqlite3
import struct
import sys
import tempfile
import threading
import time
from pathlib import Path
from urllib.parse import quote_plus, urlencode

TOKEN = "hf_synthetic_search_token_0123456789"
GIB = 1024 ** 3
GB = 10 ** 9
NOW0 = 1_800_000_000.5
TIMEOUT = 1.0
QUANTISERS = {"quant-one", "quant-two"}
LABS = {"lab-one", "lab-two", "lab-three"}


# ---- builders ---------------------------------------------------------------------------------

def build_gguf(kv: dict) -> bytes:
    out = bytearray(b"GGUF")
    out += struct.pack("<IQQ", 3, 0, len(kv))
    for key, (kind, value) in kv.items():
        k = key.encode()
        out += struct.pack("<Q", len(k)) + k
        if kind == "str":
            v = value.encode()
            out += struct.pack("<IQ", 8, len(v)) + v
        elif kind == "u32":
            out += struct.pack("<II", 4, value)
        elif kind == "f32":
            out += struct.pack("<If", 6, value)
        elif kind == "bool":
            out += struct.pack("<IB", 7, 1 if value else 0)
        elif kind == "u64":
            out += struct.pack("<IQ", 10, value)
        else:
            raise ValueError(kind)
    return bytes(out)


def arch_kv(arch="testarch", blocks=8, ctx=8192, embed=512, heads=8, kv_heads=4, extra=None) -> dict:
    kv = {"general.architecture": ["str", arch],
          f"{arch}.context_length": ["u32", ctx],
          f"{arch}.embedding_length": ["u32", embed],
          f"{arch}.block_count": ["u32", blocks],
          f"{arch}.attention.head_count": ["u32", heads],
          f"{arch}.attention.head_count_kv": ["u32", kv_heads],
          f"{arch}.feed_forward_length": ["u32", embed * 4]}
    kv.update(extra or {})
    return kv


def entry(path, size=None, kind="file", lfs=None):
    e = {"type": kind, "oid": "0" * 40, "path": path}
    if size is not None:
        e["size"] = size
    if lfs is not None:
        e["lfs"] = lfs
    return e


def big(path, size):
    """A weights file as the Hub lists it: the size is the LFS size."""
    return entry(path, 135, lfs={"size": size, "oid": "0" * 64})


def answer(body=None, status=200, headers=None, raw: bytes | None = None, content_type="application/json",
           **extra):
    data = raw if raw is not None else json.dumps(body).encode()
    h = [["content-type", content_type]] if content_type else []
    h += headers or []
    return {"status": status, "headers": h, "body": data.hex(), **extra}


def page(entries, link=None, **kw):
    return answer(entries, headers=[["link", link]] if link else None, **kw)


def item(id, downloads=0, likes=0, last_modified="2026-01-10T00:00:00.000Z", tags=("gguf",), **extra):
    d = {"_id": "0" * 8, "id": id, "modelId": id, "downloads": downloads, "likes": likes,
         "lastModified": last_modified, "tags": list(tags) if isinstance(tags, (list, tuple)) else tags, "pipeline_tag": "text-generation"}
    d.update(extra)
    return d


def models_target(limit=30, sort="trendingScore", q=None, tag=True):
    params = [("limit", str(limit)), ("sort", sort), ("direction", "-1")]
    if q:
        params.append(("search", q))
    if tag:
        params.append(("filter", "gguf"))
    return "/api/models?" + urlencode(params, quote_via=quote_plus)


def tree(repo):
    return f"/api/models/{repo}/tree/main?recursive=true&expand=true"


def resolve(repo, path):
    return f"/{repo}/resolve/main/{path}"


def done(filename, repo, status="done"):
    return (f"INSERT INTO download_history(repo_id, filename, dest_path, total_bytes, status, started_at)"
            f" VALUES('{repo}', '{filename}', '/models/{filename}', 1, '{status}', 1.0)")


def token(value: str) -> str:
    return f"INSERT INTO kv(key, value) VALUES('hf_token', {value})"


def setting(key, value):
    return f"INSERT INTO kv(key, value) VALUES('{key}', '{value}')"


def avatar(owner, url, at):
    return f"INSERT INTO avatar_cache(owner, url, fetched_at) VALUES('{owner}', '{url}', {at})"


def req(get, now=NOW0, **kw):
    return {"get": get, "now": now, **kw}


# ---- the synthetic Hub and CDN ----------------------------------------------------------------

def handler(name):
    class H(http.server.BaseHTTPRequestHandler):
        responses: dict = {}
        seen: list = []
        base = ""
        protocol_version = "HTTP/1.1"
        lock = threading.Lock()

        def log_message(self, *args):
            pass

        def do_GET(self):  # noqa: N802
            with H.lock:
                H.seen.append({"path": self.path, "authorization": self.headers.get("authorization"),
                               "range": self.headers.get("range")})
            a = H.responses.get(self.path)
            if a is None:
                body = b'{"error":"Repository not found"}'
                self.send_response(404)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if a.get("delay"):
                time.sleep(a["delay"])
            mode = a.get("mode")
            if mode == "drop":
                self.close_connection = True
                self.connection.close()
                return
            body = bytes.fromhex(a["body"]) + bytes(a.get("zeros", 0))
            try:
                self.send_response(a["status"])
                for k, v in a["headers"]:
                    self.send_header(k, v.replace("@HUB@", Hubs.base).replace("@CDN@", Cdns.base))
                self.send_header("content-length", str(len(body) + (100 if mode == "partial" else 0)))
                self.end_headers()
                self.wfile.write(body)
                if mode == "partial":
                    self.close_connection = True
                    self.connection.close()
            except (BrokenPipeError, ConnectionResetError):
                self.close_connection = True

    H.__name__ = name
    return H


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 256


Hubs = handler("Hub")
Cdns = handler("Cdn")


# ---- cases: searches --------------------------------------------------------------------------

def q4(name, gb):
    return big(name, int(gb * GB))


ALPHA = "lab-one/alpha-7B-GGUF"
BETA = "quant-one/beta-35B-A3B-GGUF"
GAMMA = "unknown-org/gamma-GGUF"
DELTA = "lab-two/delta-1B"
EPS = "quant-two/eps-70B-GGUF"
ZETA = "lab-three/zeta-GGUF"
ETA = "eta-solo"

BROWSE_ITEMS = [
    item(ALPHA, 120000, 300, "2026-01-05T10:00:00.000Z", ("gguf", "text-generation", "license:example-license")),
    item(BETA, 50000, 100, "2025-12-20T00:00:00Z", ("gguf", "moe", "license:other-license") + tuple(f"t{i}" for i in range(15))),
    item(GAMMA, 700, 3, "2024-01-01T00:00:00.000Z", ("gguf",)),
    item(DELTA, 9000, 40, "2025-06-01T00:00:00.000Z", ("transformers",)),
    item(EPS, 20000, 10, "2025-11-01T00:00:00.000Z", ("gguf", "license:example-license")),
    item(ZETA, 4000, 1, "2025-10-01T00:00:00.000Z", ("gguf",)),
    item(ETA, 10, 0, "", ("gguf",)),
]

ALPHA_TREE = [
    entry("README.md", 100),
    q4("alpha-7B-Q4_K_M.gguf", 4.4),
    q4("alpha-7B-Q8_0.gguf", 7.7),
    q4("alpha-7B-Q2_K.gguf", 2.7),
    q4("alpha-7B-IQ1_S.gguf", 1.6),
    q4("mmproj-alpha-F16.gguf", 0.9),
    q4("alpha-7B-imatrix.gguf", 0.5),
    q4("alpha-7B-tiny-Q4_0.gguf", 0.1),
    q4("alpha-7B-draft-Q4_0.gguf", 0.4),
    q4("notes.gguf", 2.0),
]
BETA_TREE = [
    q4("beta-35B-A3B-Q8_0-00001-of-00003.gguf", 9.0),
    q4("beta-35B-A3B-Q8_0-00002-of-00003.gguf", 9.0),
    q4("beta-35B-A3B-Q8_0-00003-of-00003.gguf", 9.0),
    q4("beta-35B-A3B-Q4_K_M.gguf", 20.0),
    q4("beta-35B-A3B-Q5_K_M.gguf", 24.5),
]
GAMMA_TREE = [q4("gamma-3B-Q4_K_M.gguf", 1.9), q4("gamma-3B-Q6_K.gguf", 2.5)]
EPS_TREE = [q4("eps-70B-IQ2_XS.gguf", 21.0), q4("eps-70B-Q3_K_M.gguf", 22.0), q4("eps-70B-Q4_K_M.gguf", 40.0)]
ETA_TREE = [q4("eta-Q4_K_M.gguf", 0.7)]

TREES = {
    tree(ALPHA): page(ALPHA_TREE),
    tree(BETA): page(BETA_TREE),
    tree(GAMMA): page(GAMMA_TREE),
    tree(DELTA): page([entry("model.safetensors", 100)]),
    tree(EPS): page(EPS_TREE),
    tree(ETA): page(ETA_TREE),
    # ZETA is not listed: the Hub answers 404.
}
AVATARS = {
    "/api/organizations/lab-one/overview": answer({"avatarUrl": "/avatars/lab-one.png"}),
    "/api/organizations/quant-one/overview": answer({"avatarUrl": "//cdn.example.test/quant-one.png"}),
    "/api/organizations/unknown-org/overview": answer({"name": "x"}, status=404),
    "/api/users/unknown-org/overview": answer({"avatarUrl": "https://cdn.example.test/unknown-org.png"}),
    "/api/organizations/quant-two/overview": answer({"avatarUrl": ""}),
    "/api/users/quant-two/overview": answer({"avatarUrl": ""}),
}
BROWSE_HUB = {models_target(): answer(BROWSE_ITEMS), **TREES, **AVATARS}


def with_hub(*extra, base=None):
    hub = dict(base or BROWSE_HUB)
    for d in extra:
        hub.update(d)
    return hub


def listing(items, **kw):
    return {models_target(**kw): answer(items)}


BACKENDS = [{"name": "synthetic-gpu", "vram_gb": 24.0}, {"name": "synthetic-cpu", "vram_gb": 0}]
ALL = "trustedOnly=false&showUnsuitable=true"


def hostile(field_items):
    """One hostile hub item beside a good one: the listing, its tree and its owner's avatars."""
    good = item(ALPHA, 100, 1)
    return {models_target(): answer([good] + field_items), tree(ALPHA): page([q4("alpha-7B-Q4_K_M.gguf", 4.4)])}


def hostile_case(id, extra_item, get="/api/v1/search?" + ALL, hub_extra=None, **kw):
    return {"id": id, "hub": {**hostile([extra_item]), **(hub_extra or {})}, "requests": [req(get)], **kw}


NBSP, NBH, EAC, NL = chr(0xA0), chr(0x2011), chr(0xE9), chr(10)
CJK = chr(0x6A21) + chr(0x578B)
DOTTED, DOTLESS, LONGS = chr(0x130), chr(0x131), chr(0x17F)
AR1, AR2, AR7 = chr(0x661), chr(0x662), chr(0x667)
AR5 = chr(0x665) * 5
FW4, FW7 = chr(0xFF14), chr(0xFF17)
BASE_ID = "lab-two/odd-1B-GGUF"
ODD_TREE = {tree(BASE_ID): page([q4("odd-1B-Q4_K_M.gguf", 0.9)])}

SEARCH_CASES: list[dict] = [
    {"id": "browse-defaults", "hub": BROWSE_HUB, "requests": [
        req("/api/v1/search"),
        req("/api/v1/search?trustedOnly=false"),
        req("/api/v1/search?" + ALL),
        req("/api/v1/search?sort=trending&" + ALL),
        req("/api/v1/search?sort=trending&trustedOnly=false"),
    ]},
    {"id": "sorts-and-limits",
     "hub": with_hub(*[listing(BROWSE_ITEMS, **kw) for kw in (
         {"sort": "downloads"}, {"sort": "likes"}, {"sort": "lastModified"}, {"limit": 1}, {"limit": 60},
         {"limit": 5}, {"limit": 30, "sort": "downloads"})]),
     "requests": [
         req("/api/v1/search?sort=downloads&" + ALL),
         req("/api/v1/search?sort=likes&" + ALL),
         req("/api/v1/search?sort=lastModified&" + ALL),
         req("/api/v1/search?sort=bogus&" + ALL),
         req("/api/v1/search?sort=&" + ALL),
         req("/api/v1/search?limit=1&" + ALL),
         req("/api/v1/search?limit=0&sort=downloads&" + ALL),
         req("/api/v1/search?limit=61&" + ALL),
         req("/api/v1/search?limit=-3&" + ALL),
         req("/api/v1/search?limit=+5&" + ALL),
         req("/api/v1/search?limit=%205%20&" + ALL),
         req("/api/v1/search?limit=99999999999999999999999&" + ALL),
         req("/api/v1/search?limit=1&limit=60&" + ALL),
     ]},
    {"id": "filters", "hub": BROWSE_HUB, "requests": [
        req("/api/v1/search?" + ALL + "&owner=ONE"),
        req("/api/v1/search?" + ALL + "&owner=%C3%BC"),
        req("/api/v1/search?" + ALL + "&license=EXAMPLE"),
        req("/api/v1/search?" + ALL + "&license=nomatch"),
        req("/api/v1/search?" + ALL + "&minGb=5&maxGb=22"),
        req("/api/v1/search?" + ALL + "&minGb=5"),
        req("/api/v1/search?" + ALL + "&maxGb=3"),
        req("/api/v1/search?" + ALL + "&minGb=nan"),
        req("/api/v1/search?" + ALL + "&maxGb=inf&minGb=-inf"),
        req("/api/v1/search?" + ALL + "&quants=q4_k_m,Q8_0"),
        req("/api/v1/search?" + ALL + "&quants=,,"),
        req("/api/v1/search?" + ALL + "&quants=none"),
        req("/api/v1/search?" + ALL + "&minParams=7&maxParams=35"),
        req("/api/v1/search?" + ALL + "&minParams=36"),
        req("/api/v1/search?" + ALL + "&moe=moe"),
        req("/api/v1/search?" + ALL + "&moe=dense"),
        req("/api/v1/search?" + ALL + "&moe=whatever"),
        req("/api/v1/search?" + ALL + "&vision=true"),
        req("/api/v1/search?" + ALL + "&vision=ON"),
        req("/api/v1/search?trustedOnly=0&showUnsuitable=yes&vision=f"),
        req("/api/v1/search?trustedOnly=n&showUnsuitable=T"),
    ]},
    {"id": "invalid-queries", "hub": {}, "requests": [
        req("/api/v1/search?limit=abc"),
        req("/api/v1/search?limit=1.5"),
        req("/api/v1/search?limit="),
        req("/api/v1/search?limit=" + "9" * 4301),
        req("/api/v1/search?trustedOnly=maybe&showUnsuitable=2&vision=%20true"),
        req("/api/v1/search?minGb=x&maxGb=1e&minParams=0x1&maxParams=%EF%BC%91"),
        req("/api/v1/search?limit=a&trustedOnly=b&showUnsuitable=c&minGb=d&maxGb=e&minParams=f&maxParams=g&vision=h"),
        req("/api/v1/search?limit=%C3%BC&vision=%FF"),
        req("/api/v1/search/repo"),
        req("/api/v1/search/repo?repo2=x"),
    ]},
    {"id": "query-text", "hub": with_hub(
        listing(BROWSE_ITEMS, q="alpha"), listing(BROWSE_ITEMS, q="a b*c~d/e"),
        listing(BROWSE_ITEMS, q="\u00fc\u00e9 \u4e2d"), listing(BROWSE_ITEMS, q="x"),
        listing(BROWSE_ITEMS, q="a+b%c&d=e")), "requests": [
        req("/api/v1/search?q=alpha&" + ALL),
        req("/api/v1/search?q=%20%20alpha%20%E3%80%80&" + ALL),
        req("/api/v1/search?q=a+b*c~d%2Fe&" + ALL + "&owner=o%2Ap"),
        req("/api/v1/search?q=%C3%BC%C3%A9+%E4%B8%AD&" + ALL),
        req("/api/v1/search?q=%1fx%1f&" + ALL),
        req("/api/v1/search?q=a%2Bb%25c%26d%3De&" + ALL),
        req("/api/v1/search?q=x&q=alpha&" + ALL),
        req("/api/v1/search?q=%FF&" + ALL),
    ]},
    {"id": "empty-and-loose-passes", "hub": {
        models_target(q="loose"): answer([]),
        models_target(q="loose", tag=False): answer([
            item("lab-one/looseA", 5, 1, tags=("other",)), item("lab-one/looseB-gguf", 3, 0, tags=()),
            item("lab-one/looseC", 4, 0, tags=("GGUF-v2", "x"))]),
        models_target(q="plain"): answer([]),
        models_target(q="plain", tag=False): answer([
            item("lab-one/plainA", 5, 1, tags=("other",)), item("lab-one/plainB", 3, 0, tags=())]),
        models_target(q="none"): answer([]),
        models_target(q="none", tag=False): answer([]),
        models_target(q="notlist"): answer({"items": []}),
        models_target(q="notlist", tag=False): answer("text"),
        models_target(q="gone"): answer([]),
        models_target(q="gone", tag=False): answer({"error": "boom"}, status=500),
        models_target(): answer([]),
        tree("lab-one/looseB-gguf"): page([q4("looseB-Q4_K_M.gguf", 1.0)]),
        tree("lab-one/looseC"): page([q4("looseC-Q4_K_M.gguf", 1.0)]),
        tree("lab-one/looseA"): page([q4("looseA-Q4_K_M.gguf", 1.0)]),
        tree("lab-one/plainA"): page([q4("plainA-Q4_K_M.gguf", 1.0)]),
        tree("lab-one/plainB"): page([q4("plainB-Q4_K_M.gguf", 1.0)]),
        "/api/organizations/lab-one/overview": answer({"avatarUrl": "https://cdn.example.test/lab-one.png"}),
    }, "requests": [
        req("/api/v1/search?q=loose&" + ALL),
        req("/api/v1/search?q=plain&" + ALL),
        req("/api/v1/search?q=none&" + ALL),
        req("/api/v1/search?q=notlist&" + ALL),
        req("/api/v1/search?q=gone&" + ALL),
        req("/api/v1/search?" + ALL),
        req("/api/v1/search?q=%20&" + ALL),
    ]},
    {"id": "no-backend-and-small-gpu", "hub": BROWSE_HUB, "backends": [], "requests": [
        req("/api/v1/search?" + ALL)], "then": [
        {"backends": [{"name": "small", "vram_gb": 8.0}], "requests": [req("/api/v1/search?" + ALL)]},
        {"backends": [{"name": "a", "vram_gb": 1.0}, {"name": "b", "vram_gb": 0.5}],
         "requests": [req("/api/v1/search?" + ALL)]},
        {"backends": [{"name": "huge", "vram_gb": 1e16}], "requests": [req("/api/v1/search?" + ALL)]},
        {"backends": [{"name": "frac", "vram_gb": 11.5}], "requests": [req("/api/v1/search?" + ALL)]},
    ]},
    {"id": "trusted-setting-and-downloads", "hub": BROWSE_HUB,
     "seed": [setting("trusted_quantisers", " Unknown-Org , ,LAB-ONE\u00c9, second "),
              done("m1/m1-Q4_K_M.gguf", ALPHA), done("m1/mmproj-m1.gguf", ALPHA), done("gone/gone.gguf", ALPHA),
              done("bare.gguf", GAMMA), done("m2/m2.gguf", GAMMA, status="error"), done("x.gguf", "")],
     "files": [{"name": "m1/m1-Q4_K_M.gguf"}, {"name": "m1/mmproj-m1.gguf"}, {"name": "bare.gguf"}],
     "requests": [req("/api/v1/search?trustedOnly=true&showUnsuitable=false"), req("/api/v1/search?" + ALL)]},
    {"id": "trusted-setting-wrong-type", "hub": BROWSE_HUB, "seed": ["INSERT INTO kv(key, value) VALUES('trusted_quantisers', X'41')"],
     "requests": [req("/api/v1/search?" + ALL)]},
    {"id": "token-sent", "hub": {**BROWSE_HUB}, "seed": [token(f"'{TOKEN}'")], "requests": [
        req("/api/v1/search?" + ALL)]},
    {"id": "token-number-text", "hub": {**BROWSE_HUB}, "seed": [token("7")], "requests": [
        req("/api/v1/search?" + ALL)]},
    {"id": "token-blob", "hub": {**BROWSE_HUB}, "seed": ["INSERT INTO kv(key, value) VALUES('hf_token', X'4142')"],
     "requests": [req("/api/v1/search?" + ALL)]},
    {"id": "token-not-ascii", "hub": {**BROWSE_HUB}, "seed": [token("'tok\u00e9n'")], "requests": [
        req("/api/v1/search?" + ALL)]},
    {"id": "token-refused-by-h11", "hub": {**BROWSE_HUB}, "seed": [token("'tok '")], "requests": [
        req("/api/v1/search?" + ALL)]},
    {"id": "token-with-inner-space", "hub": {**BROWSE_HUB}, "seed": [token("'tok en'")], "requests": [
        req("/api/v1/search?" + ALL)]},
    {"id": "hub-statuses", "hub": {
        models_target(limit=1): answer({"error": "bad token"}, status=401),
        models_target(limit=2): answer(raw=b"", status=403, content_type=None),
        models_target(limit=3): answer(raw=b"Too Many Requests", status=429, content_type="text/plain"),
        models_target(limit=4): answer(raw=(b"  \n\t " + b"x" * 300 + b"  "), status=500, content_type="text/plain"),
        models_target(limit=5): answer(raw=b"bad \xff\xfe gateway \xc3", status=502, content_type="text/plain"),
        models_target(limit=6): answer(raw=b"caf\xe9", status=503, content_type="text/plain; charset=ISO-8859-1"),
        models_target(limit=7): answer(raw=b"\xe2\x80\x83\xc2\xa0 unicode\xe3\x80\x80", status=404, content_type="text/plain"),
        models_target(limit=8): answer(raw=b"nope", status=400, content_type="text/plain; charset=UTF-8"),
        models_target(limit=9): answer(raw=b"x" * 199 + "\u00e9y".encode(), status=500, content_type="text/plain"),
        models_target(limit=10): answer(raw=b"", status=302, content_type=None, headers=[["location", "/elsewhere"]]),
        models_target(limit=11): answer(raw=b"[]", status=301, headers=[["location", "/elsewhere"]]),
        models_target(limit=12): answer(raw=b"{", status=200),
        models_target(limit=13): answer(raw=b"\xff\xfe", status=200),
        models_target(limit=14): answer(raw=b"", status=204, content_type=None),
        models_target(limit=15): answer([1, "a", None, [], {"id": ALPHA}]),
        tree(ALPHA): page([q4("alpha-7B-Q4_K_M.gguf", 4.4)]),
    }, "requests": [req(f"/api/v1/search?limit={n}&" + ALL) for n in range(1, 16)]},
    {"id": "hub-down", "hub": {}, "hub_down": True, "requests": [req("/api/v1/search?" + ALL)]},
    {"id": "hub-search-timeout", "hub": {models_target(): answer(BROWSE_ITEMS, delay=1.6)},
     "requests": [req("/api/v1/search?" + ALL)]},
    {"id": "hub-search-dropped", "hub": {models_target(): answer([], mode="drop"),
                                        models_target(q="part"): answer(BROWSE_ITEMS, mode="partial")},
     "requests": [req("/api/v1/search?" + ALL), req("/api/v1/search?q=part&" + ALL)]},
    {"id": "tree-failures", "hub": {
        models_target(): answer([item(f"lab-one/t{i}-GGUF", 100 - i, 1) for i in range(8)]),
        tree("lab-one/t0-GGUF"): page([q4("t0-Q4_K_M.gguf", 1.0)]),
        tree("lab-one/t1-GGUF"): page([], status=500),
        tree("lab-one/t2-GGUF"): answer(raw=b"not json"),
        tree("lab-one/t3-GGUF"): answer({"error": "a dict"}),
        tree("lab-one/t4-GGUF"): page([{"type": "file", "path": ["not", "a", "str"]}]),
        tree("lab-one/t5-GGUF"): page([q4("t5-Q4_K_M.gguf", 1.0)], delay=1.6),
        tree("lab-one/t6-GGUF"): page([q4("t6-Q4_K_M.gguf", 1.0)], mode="drop"),
        tree("lab-one/t7-GGUF"): page([q4("t7-Q4_K_M.gguf", 1.0)], link='<@HUB@/api/models/lab-one/t7-GGUF/tree/main?recursive=true&expand=true&cursor=p2>; rel="next"'),
        tree("lab-one/t7-GGUF") + "&cursor=p2": page([q4("t7-Q8_0.gguf", 2.0)]),
    }, "requests": [req("/api/v1/search?" + ALL)]},
    {"id": "listing-cache", "hub": BROWSE_HUB, "requests": [
        req("/api/v1/search?" + ALL, now=NOW0),
        req("/api/v1/search?" + ALL, now=NOW0 + 3599.0),
        req("/api/v1/search?" + ALL, now=NOW0 + 3600.5),
        req("/api/v1/search?" + ALL, now=NOW0 + 3601.0),
        req("/api/v1/search?" + ALL, now=NOW0 + 3601.0 + 3599.0),
    ]},
    {"id": "avatars", "hub": with_hub({
        models_target(): answer([item(f"own{i}/m-GGUF", 100 - i, 1) for i in range(1, 14)]),
        **{tree(f"own{i}/m-GGUF"): page([q4("m-Q4_K_M.gguf", 1.0)]) for i in range(1, 14)},
        "/api/organizations/own1/overview": answer({"avatarUrl": "https://cdn.example.test/1.png"}),
        "/api/organizations/own2/overview": answer({"avatarUrl": ""}),
        "/api/users/own2/overview": answer({"avatarUrl": "//cdn.example.test/2.png"}),
        "/api/organizations/own3/overview": answer(raw=b"not json"),
        "/api/users/own3/overview": answer({"avatarUrl": "/u/3.png"}),
        "/api/organizations/own4/overview": answer({}, status=500),
        "/api/users/own4/overview": answer({"avatarUrl": None}),
        "/api/organizations/own5/overview": answer({"avatarUrl": "x"}, delay=1.6),
        "/api/users/own5/overview": answer({"avatarUrl": "https://cdn.example.test/5.png"}),
        "/api/organizations/own6/overview": answer({"avatarUrl": "https://cdn.example.test/6.png"}, mode="drop"),
        "/api/users/own6/overview": answer({"avatarUrl": "https://cdn.example.test/6u.png"}),
        "/api/organizations/own7/overview": answer({"avatarUrl": "relative.png"}),
        "/api/organizations/own8/overview": answer({"avatarUrl": 0}),
        "/api/users/own8/overview": answer({"avatarUrl": False, "x": 1}),
        "/api/organizations/own9/overview": answer({"avatarUrl": " "}),
        "/api/organizations/own10/overview": answer({"avatarUrl": "/"}),
        "/api/organizations/own11/overview": answer({"avatarUrl": "//"}),
        "/api/organizations/own12/overview": answer({"avatarUrl": "https://cdn.example.test/12.png"}),
        "/api/organizations/own13/overview": answer({"avatarUrl": "https://cdn.example.test/13.png"}),
    }, base={}), "seed": [avatar("own12", "https://cdn.example.test/cached12.png", NOW0 - 100),
                          avatar("own13", "https://cdn.example.test/stale13.png", NOW0 - 7 * 86400 - 1),
                          avatar("own1", "", NOW0 - 7 * 86400 + 10), avatar("own7", "", NOW0 - 7 * 86400)],
     "requests": [req("/api/v1/search?" + ALL, now=NOW0), req("/api/v1/search?" + ALL, now=NOW0 + 7 * 86400 + 5)]},
    {"id": "avatar-non-dict-answer", "hub": with_hub({
        models_target(): answer([item(ALPHA, 100, 1)]),
        "/api/organizations/lab-one/overview": answer([1, 2]),
    }), "requests": [req("/api/v1/search?" + ALL)]},
    {"id": "avatar-non-string-url", "hub": with_hub({
        models_target(): answer([item(ALPHA, 100, 1)]),
        "/api/organizations/lab-one/overview": answer({"avatarUrl": 5}),
    }), "requests": [req("/api/v1/search?" + ALL)]},
    {"id": "avatar-token", "hub": with_hub(), "seed": [token(f"'{TOKEN}'")], "requests": [
        req("/api/v1/search?" + ALL)]},
    {"id": "avatar-cache-column-types", "hub": with_hub(), "seed": [
        "INSERT INTO avatar_cache(owner, url, fetched_at) VALUES('lab-one', 'u1', 'text-time')"],
     "requests": [req("/api/v1/search?" + ALL)]},
    {"id": "avatar-cache-int-time", "hub": with_hub(), "seed": [
        "INSERT INTO avatar_cache(owner, url, fetched_at) VALUES('lab-one', 'https://cdn.example.test/i.png', 1799999999)"],
     "requests": [req("/api/v1/search?" + ALL)]},
    hostile_case("item-downloads-string", item(BASE_ID, "12", 3), hub_extra=ODD_TREE),
    hostile_case("item-downloads-bad-string", item(BASE_ID, "twelve", 3), hub_extra=ODD_TREE),
    hostile_case("item-downloads-float", item(BASE_ID, 3.9, -2.5), hub_extra=ODD_TREE),
    hostile_case("item-downloads-bool-null", item(BASE_ID, True, None), hub_extra=ODD_TREE),
    hostile_case("item-downloads-list", item(BASE_ID, [1], 0), hub_extra=ODD_TREE),
    hostile_case("item-downloads-underscore", item(BASE_ID, " 1_000 ", "-7"), hub_extra=ODD_TREE),
    hostile_case("item-downloads-nan", item(BASE_ID, float("nan"), 0), hub_extra=ODD_TREE),
    hostile_case("item-downloads-negative", item(BASE_ID, -1000000, 5), hub_extra=ODD_TREE),
    hostile_case("item-lastmodified-number", item(BASE_ID, 5, 1, last_modified=20260101), hub_extra=ODD_TREE),
    hostile_case("item-lastmodified-true", item(BASE_ID, 5, 1, last_modified=True), hub_extra=ODD_TREE),
    hostile_case("item-lastmodified-forms", {"id": BASE_ID, "downloads": 1, "lastModified": "2026-01-10"}, hub_extra=ODD_TREE),
    hostile_case("item-tags-string", item(BASE_ID, 5, 1, tags="gguf"), hub_extra=ODD_TREE),
    hostile_case("item-tags-dict", item(BASE_ID, 5, 1, tags={"gguf": 1, "license:x": 2, "gguf": 3}), hub_extra=ODD_TREE),
    hostile_case("item-tags-scalars", item(BASE_ID, 5, 1, tags=[1, 2.5, None, True, False, "license:", "License:y", 1e20]), hub_extra=ODD_TREE),
    hostile_case("item-tags-number", item(BASE_ID, 5, 1, tags=5), hub_extra=ODD_TREE),
    hostile_case("item-tags-empty-values", item(BASE_ID, 5, 1, tags=[]), hub_extra=ODD_TREE),
    hostile_case("item-siblings-ok", item(BASE_ID, 5, 1, siblings=[{"rfilename": "a.gguf"}, {"rfilename": None}, {}]), hub_extra=ODD_TREE),
    hostile_case("item-siblings-bad-entry", item(BASE_ID, 5, 1, siblings=[{"rfilename": "a.gguf"}, "b.gguf"]), hub_extra=ODD_TREE),
    hostile_case("item-siblings-string", item(BASE_ID, 5, 1, siblings="abc"), hub_extra=ODD_TREE),
    hostile_case("item-siblings-number", item(BASE_ID, 5, 1, siblings=3), hub_extra=ODD_TREE),
    hostile_case("item-id-from-modelid", {"modelId": BASE_ID, "downloads": 5}, hub_extra=ODD_TREE),
    hostile_case("item-id-missing", {"downloads": 5}, hub_extra={tree(""): page([])}, native_requests="unchecked"),
    hostile_case("item-id-number", {"id": 12, "downloads": 5}, native_requests="unchecked"),
    hostile_case("item-id-falsy-modelid", {"id": "", "modelId": BASE_ID}, hub_extra=ODD_TREE),
    hostile_case("item-id-no-slash", {"id": "solo-gguf"}, hub_extra={tree("solo-gguf"): page([q4("solo-Q4_K_M.gguf", 1.0)])}),
    hostile_case("item-id-hostile-shapes", {"id": "a/../b?x#y"}, native_requests="unchecked"),
    {"id": "items-beyond-forty", "hub": {
        models_target(limit=60): answer([item(f"lab-one/m{i:02d}-GGUF", 1000 - i, 1) for i in range(45)] + [
            item("lab-one/late", "bad")]),
        **{tree(f"lab-one/m{i:02d}-GGUF"): page([q4(f"m{i:02d}-Q4_K_M.gguf", 1.0)]) for i in range(45)}},
     "requests": [req("/api/v1/search?limit=60&" + ALL)]},
    {"id": "repeated-ids", "hub": {
        models_target(): answer([item(ALPHA, 5, 1), item(ALPHA, 6, 1), item(ALPHA, 7, 1)]),
        tree(ALPHA): page(ALPHA_TREE)}, "requests": [req("/api/v1/search?" + ALL), req("/api/v1/search?" + ALL)]},
    {"id": "files-odd-shapes", "hub": {
        models_target(): answer([item("lab-one/odd-GGUF", 5, 1)]),
        tree("lab-one/odd-GGUF"): page([
            q4("odd-Q4_K_M.GGUF", 1.0), q4("sub/dir/odd-Q5_K_M.gguf", 1.2),
            q4("odd-Q6_K-00001-of-00002.gguf", 0.7), q4("odd-Q6_K-00002-of-00002.gguf", 0.7),
            q4("x-00001-of-00003.gguf", 0.7), q4("plain.gguf", 1.0), q4("weights-MXFP4.gguf", 1.0),
            q4("m-UD-IQ2_XXS.gguf", 1.0), q4("m-BF16.gguf", 2.0), q4("m-F32.gguf", 4.0),
            q4("m-Q4_0.eagle3.gguf", 1.0), q4("m-medusa-Q4_0.gguf", 1.0), q4("m-mtp-Q4_0.gguf", 1.0),
            q4("m-mtpx-Q4_0.gguf", 1.0), q4("m-Q4_K_M-imatrix.gguf", 1.0),
            entry("dir", kind="directory"),
            entry("lfsless-Q4_K_M.gguf", 3 * GB),
            entry("zero-Q4_K_M.gguf", 0), entry("nosize-Q4_K_M.gguf"),
            entry("strsize-Q4_K_M.gguf", "2000000000"),
        ])}, "requests": [req("/api/v1/search?" + ALL)]},
    {"id": "unicode-paths", "hub": {
        models_target(): answer([item("lab-one/uni-GGUF", 50, 1), item("lab-one/uni2-GGUF", 40, 1)]),
        tree("lab-one/uni-GGUF"): page([
            q4(CJK + "-Q4_K_M.gguf", 1.0), q4("caf" + EAC + "-Q5_K_M.gguf", 1.2),
            q4("nb" + NBH + "x-Q4_K_M.gguf", 1.0), q4("model" + NBH + "Q4_K_M.gguf", 1.0),
            q4("z-" + DOTLESS + "Q2_XS.gguf", 1.0), q4("z-" + DOTTED + "Q3_M.gguf", 1.0),
            q4("model-Q4_K_M" + EAC + chr(0xFC) + ".gguf", 1.0), q4("model-Q" + FW4 + "_K.gguf", 1.0),
            q4("n-Q4_K_M-" + AR5 + "-of-" + AR5 + ".gguf", 1.0), q4("n-Q4_K_M-" + AR1 * 5 + "-of-" + AR2 * 5 + ".gguf", 1.0),
            q4("medu" + LONGS + "a-Q4_K_M.gguf", 1.0), q4(CJK + "mtp" + CJK + "-Q4_K_M.gguf", 1.0),
            q4("x" + NBH + "mtp" + NBH + "y-Q4_K_M.gguf", 1.0), q4("x-mtp-y-Q4_K_M.gguf", 1.0),
            q4(DOTTED + "matrix-Q4_K_M.gguf", 1.0), q4("q-" + AR7 + "B-Q4_K_M.gguf", 3.0),
            q4("r-" + FW7 + "B-Q6_K.gguf", 6.0), q4("s-7" + NBSP + "B-Q8_0.gguf", 8.0),
            q4("t-" + DOTTED + "Q2_K.gguf", 1.0), q4("dir" + EAC + "/u-Q4_K_M.gguf", 1.0),
            q4(chr(0x3A3) + "-Q4_K_M.gguf", 1.0), q4("w-Q4_K_M" + NL + ".gguf", 1.0),
        ]),
        tree("lab-one/uni2-GGUF"): page([q4("only-Q4_K_M.gguf", 1.0)]),
    }, "requests": [req("/api/v1/search?" + ALL)]},
    {"id": "unicode-ids", "hub": {
        models_target(): answer([item("lab-one/" + CJK + "-7B-GGUF", 50, 1), item("lab-one/caf" + EAC + "-3B-GGUF", 49, 1),
                                 item("lab-one/m-7" + NBSP + "B-GGUF", 48, 1), item("lab-one/n" + NBH + "7B-GGUF", 47, 1),
                                 item("lab-one/o-" + AR7 + "B-GGUF", 46, 1), item("lab-one/p-" + FW7 + "B-GGUF", 45, 1),
                                 item("lab-one/q-" + FW7 + "." + FW7 + "B-A" + FW7 + "B-GGUF", 44, 1),
                                 item("lab-one/r-7B-A3B" + NBH + "x", 43, 1),
                                 item("lab-one/s-" + EAC + "7B-GGUF", 42, 1),
                                 item("lab-one/t-" + CJK + "7B" + NL, 41, 1),
                                 item(EAC + "-org/u-1B-GGUF", 40, 1),
                                 item("lab-one/v-MoE" + DOTTED + "-GGUF", 39, 1)]),
    }, "native_requests": "unchecked", "requests": [req("/api/v1/search?" + ALL)]},
    {"id": "param-names", "hub": {
        models_target(): answer([item(i, 5, 1) for i in (
            "lab-one/a-7B-70B-GGUF", "lab-one/b-70B_7B-GGUF", "lab-one/c-0B-GGUF", "lab-one/d-1.5b-it-GGUF",
            "lab-one/e-8x7B-GGUF", "lab-one/f-3B-A1B-GGUF", "lab-one/g-30B-A3B-Q4-GGUF", "lab-one/h-100B-GGUF",
            "lab-one/i-big-120b-a22b-GGUF", "lab-one/j-mixture-of-experts-GGUF", "lab-one/k-22B-a4b-GGUF",
            "lab-one/l-1 B-GGUF")]),
        **{tree(f"lab-one/{n}"): page([q4(f"{n}-Q3_K_M.gguf", 1.0), q4(f"{n}-IQ2_M.gguf", 0.6), q4(f"{n}-Q4_K_M.gguf", 2.0)])
           for n in ("a-7B-70B-GGUF", "b-70B_7B-GGUF", "c-0B-GGUF", "d-1.5b-it-GGUF", "e-8x7B-GGUF", "f-3B-A1B-GGUF",
                     "g-30B-A3B-Q4-GGUF", "h-100B-GGUF", "i-big-120b-a22b-GGUF", "j-mixture-of-experts-GGUF",
                     "k-22B-a4b-GGUF")}},
     "native_requests": "unchecked", "requests": [req("/api/v1/search?" + ALL)]},
    hostile_case("item-id-huge-params", item("lab-one/h-" + "9" * 400 + "B-GGUF", 5, 1)),
    {"id": "items-beyond-forty-good", "hub": {
        models_target(limit=60): answer([item(f"lab-one/g{i:02d}-GGUF", 1000 - i, 1) for i in range(45)]),
        **{tree(f"lab-one/g{i:02d}-GGUF"): page([q4(f"g{i:02d}-Q4_K_M.gguf", 1.0)]) for i in range(45)}},
     "requests": [req("/api/v1/search?limit=60&" + ALL)]},
]


# ---- cases: one repository --------------------------------------------------------------------

RB = "lab-one/rb-7B-GGUF"
MAIN = "a-7B-Q4_K_M.gguf"


def header_answer(kv=None, pad=0, status=206, **kw):
    raw = build_gguf(kv or arch_kv()) + b"\0" * pad
    return answer(raw=raw, status=status, content_type="application/octet-stream",
                  headers=[["content-range", f"bytes 0-{len(raw) - 1}/{len(raw)}"]] if status == 206 else None, **kw)


RB_TREE = [
    entry("README.md", 100), entry("config.json", 200), entry("tokenizer.model", 300),
    q4(MAIN, 4.4), q4("rb-7B-Q8_0.gguf", 7.7),
    q4("rb-7B-Q6_K-00001-of-00003.gguf", 2.0), q4("rb-7B-Q6_K-00002-of-00003.gguf", 2.0),
    q4("rb-7B-Q6_K-00003-of-00003.gguf", 1.5),
    q4("mmproj-rb-F16.gguf", 0.9), q4("mmproj-rb-BF16.gguf", 0.95),
    q4("rb-7B-imatrix.gguf", 0.5), q4("rb-7B-tiny.gguf", 0.1), q4("rb-7B-draft-Q4_0.gguf", 0.4),
    q4("zz-70B-IQ1_S.gguf", 30.0), q4("sub-model.GGUF", 1.0),
    entry("sub", kind="directory"), entry("sub/inner-Q4_K_M.gguf", 3 * GB),
]
RB_HUB = {tree(RB): page(RB_TREE), resolve(RB, MAIN): header_answer(pad=64)}


def repo_get(repo, **kw):
    return req("/api/v1/search/repo?repo=" + quote_plus(repo), **kw)


def rcase(id, hub, repos=(RB,), **kw):
    return {"id": id, "hub": hub, "requests": [repo_get(r) for r in repos], "kind": "repo", **kw}


REPO_CASES: list[dict] = [
    rcase("repo-basic", RB_HUB),
    rcase("repo-twice-uses-header-cache", RB_HUB, repos=(RB, RB)),
    rcase("repo-token", RB_HUB, seed=[token(f"'{TOKEN}'")]),
    rcase("repo-no-gpu", RB_HUB, gpu=False),
    rcase("repo-cpu-only-gpu-none", RB_HUB, gpu=False, cpu=False),
    rcase("repo-impossible-size", {tree(RB): page([q4("rb-huge-Q4_K_M.gguf", 200.0), q4("rb-small-Q4_K_M.gguf", 1.0)]),
                                   resolve(RB, "rb-huge-Q4_K_M.gguf"): header_answer(pad=8)}),
    rcase("repo-cpu-slow", {tree(RB): page([q4("rb-big-Q4_K_M.gguf", 33.0), q4("rb-mid-Q4_K_M.gguf", 20.0)]),
                            resolve(RB, "rb-big-Q4_K_M.gguf"): header_answer(pad=8)}, gpu=False),
    rcase("repo-header-401", {tree(RB): page(RB_TREE), resolve(RB, MAIN): answer({}, status=401)},
          repos=(RB,)),
    rcase("repo-header-403", {tree(RB): page(RB_TREE), resolve(RB, MAIN): answer({}, status=403)}),
    rcase("repo-header-404", {tree(RB): page(RB_TREE)}),
    rcase("repo-header-500", {tree(RB): page(RB_TREE), resolve(RB, MAIN): answer({}, status=500)}),
    rcase("repo-header-empty", {tree(RB): page(RB_TREE), resolve(RB, MAIN): answer(raw=b"", status=206, content_type=None)}),
    rcase("repo-header-garbage", {tree(RB): page(RB_TREE), resolve(RB, MAIN): answer(raw=b"not a gguf at all" * 10, status=206, content_type=None)}),
    rcase("repo-header-truncated", {tree(RB): page(RB_TREE), resolve(RB, MAIN): answer(raw=build_gguf(arch_kv())[:40], status=206, content_type=None)}),
    rcase("repo-header-ignores-range", {tree(RB): page(RB_TREE), resolve(RB, MAIN): answer(
        raw=build_gguf(arch_kv()), zeros=1_500_000, status=200, content_type="application/octet-stream")}),
    rcase("repo-header-timeout", {tree(RB): page(RB_TREE), resolve(RB, MAIN): header_answer(delay=1.6)}),
    rcase("repo-header-dropped", {tree(RB): page(RB_TREE), resolve(RB, MAIN): header_answer(mode="drop")}),
    rcase("repo-header-partial", {tree(RB): page(RB_TREE), resolve(RB, MAIN): header_answer(mode="partial")}),
    rcase("repo-header-missing-keys", {tree(RB): page(RB_TREE), resolve(RB, MAIN): header_answer(
        kv={"general.architecture": ["str", "testarch"]}, pad=8)}),
    rcase("repo-header-no-context", {tree(RB): page(RB_TREE), resolve(RB, MAIN): header_answer(
        kv={k: v for k, v in arch_kv().items() if "context_length" not in k}, pad=8)}),
    rcase("repo-header-big-context", {tree(RB): page(RB_TREE), resolve(RB, MAIN): header_answer(
        kv=arch_kv(ctx=1048576, blocks=40, embed=4096, heads=32, kv_heads=8), pad=8)}),
    rcase("repo-header-redirect-to-cdn", {tree(RB): page(RB_TREE), resolve(RB, MAIN): answer(
        raw=b"", status=302, content_type=None, headers=[["location", "@CDN@/blob/rb.gguf"]])},
          cdn={"/blob/rb.gguf": header_answer(pad=16)}, seed=[token(f"'{TOKEN}'")]),
    rcase("repo-header-redirect-same-origin", {tree(RB): page(RB_TREE), resolve(RB, MAIN): answer(
        raw=b"", status=307, content_type=None, headers=[["location", "@HUB@/blob/rb.gguf"]]),
        "/blob/rb.gguf": header_answer(pad=16)}, seed=[token(f"'{TOKEN}'")]),
    rcase("repo-header-gated-then-ok", {tree(RB): page(RB_TREE), resolve(RB, MAIN): answer({}, status=401)},
          repos=(RB, RB), then_hub={resolve(RB, MAIN): header_answer(pad=8)}),
    rcase("repo-tree-404", {}),
    rcase("repo-tree-500", {tree(RB): page([], status=500)}),
    rcase("repo-tree-503-text", {tree(RB): answer(raw=b"down", status=503, content_type="text/plain")}),
    rcase("repo-tree-redirect-no-location", {tree(RB): answer(raw=b"", status=302, content_type=None)}),
    rcase("repo-tree-bad-json", {tree(RB): answer(raw=b"{not json")}),
    rcase("repo-tree-bad-encoding", {tree(RB): answer(raw=b"\xff\xfe\x00")}),
    rcase("repo-tree-dict-page", {tree(RB): answer({"error": "x"})}),
    rcase("repo-tree-null-page", {tree(RB): answer(None)}),
    rcase("repo-tree-empty", {tree(RB): page([])}),
    rcase("repo-tree-files-refused", {tree(RB): page([{"type": "file", "path": ["x"]}])}),
    rcase("repo-tree-entry-not-object", {tree(RB): page([1])}),
    rcase("repo-tree-bad-size", {tree(RB): page([{"type": "file", "path": "x-Q4_K_M.gguf", "size": "big"}])}),
    rcase("repo-tree-timeout", {tree(RB): page(RB_TREE, delay=1.6)}),
    rcase("repo-tree-dropped", {tree(RB): page(RB_TREE, mode="drop")}),
    rcase("repo-tree-partial", {tree(RB): page(RB_TREE, mode="partial")}),
    rcase("repo-hub-down", {}, hub_down=True),
    rcase("repo-token-refused", {tree(RB): page(RB_TREE)}, seed=[token("'tok '")], native="refused"),
    rcase("repo-token-not-ascii", {tree(RB): page(RB_TREE)}, seed=[token("'tok\u00e9'")]),
    rcase("repo-two-pages", {
        tree(RB): page(RB_TREE[:8], link='<@HUB@/api/models/lab-one/rb-7B-GGUF/tree/main?recursive=true&expand=true&cursor=p2>; rel="next"'),
        tree(RB) + "&cursor=p2": page(RB_TREE[8:]), resolve(RB, MAIN): header_answer(pad=8)}),
    rcase("repo-link-header-forms", {
        tree(RB): page(RB_TREE[:4], link='<@HUB@/x>; rel="prev", <@HUB@/api/models/lab-one/rb-7B-GGUF/tree/main?recursive=true&expand=true&cursor=p2> ; rel="next"'),
        tree(RB) + "&cursor=p2": page(RB_TREE[4:], link='<@HUB@/x>; rel="last"'), resolve(RB, MAIN): header_answer(pad=8)}),
    rcase("repo-ids", {tree("solo"): page([q4("solo-Q4_K_M.gguf", 1.0)]),
                       tree("a/b/c"): page([q4("c-Q4_K_M.gguf", 1.0)]),
                       tree("Org.Name_1/Repo-2.x"): page([q4("r-Q4_K_M.gguf", 1.0)])},
          repos=("solo", "a/b/c", "Org.Name_1/Repo-2.x")),
    rcase("repo-ids-not-hub-shaped", {}, repos=("", "a b/c", "../x", "a//b", "a/b?x=1", "a/b#c", "a/%41", "a/\u00fc"),
          native_requests="unchecked"),
    rcase("repo-name-sorting", {tree(RB): page([
        q4("B-Q4_K_M.gguf", 1.0), q4("a-Q4_K_M.gguf", 1.0), q4("C-Q4_K_M.GGUF", 1.0), q4("mmproj-z.gguf", 1.0),
        q4("mmproj-A.gguf", 1.0), q4("\u00fc-Q4_K_M.gguf", 1.0), q4("\u00dc-Q8_0.gguf", 1.0), q4("z/y-Q4_K_M.gguf", 1.0),
        entry("tokenizer.json", 5), entry("Config.json", 5), entry("a-draft.gguf", 5)]),
        resolve(RB, "a-Q4_K_M.gguf"): header_answer(pad=8)}),
    rcase("repo-odd-names", {tree(RB): page([q4("plain.gguf", 1.0), q4("B-upper-Q4_K_M.GGUF", 1.0)]),
                             resolve(RB, "B-upper-Q4_K_M.GGUF"): header_answer(pad=8)}),
    rcase("repo-unicode-names", {tree(RB): page([
        q4(CJK + "-Q4_K_M.gguf", 1.0), q4("caf" + EAC + "-Q4_K_M.gguf", 1.0), q4(chr(0xC9) + "cole-Q4_K_M.gguf", 1.0),
        q4(DOTTED + "-Q4_K_M.gguf", 1.0), q4(chr(0xDF) + "-Q4_K_M.gguf", 1.0), q4(chr(0x3A3) + "-Q4_K_M.gguf", 1.0),
        q4(chr(0x391) + chr(0x3A3) + "-Q4_K_M.gguf", 1.0), q4("x" + NBSP + "-Q4_K_M.gguf", 1.0),
        q4("x" + NBH + "-Q4_K_M.gguf", 1.0), q4(DOTTED + "matrix.gguf", 1.0), q4(DOTLESS + "matrix.gguf", 1.0),
        q4("MMPROJ-" + CJK + ".gguf", 1.0), entry(CJK + ".json", 3), entry("caf" + EAC + ".txt", 3)]),
        resolve(RB, "%C3%89cole-Q4_K_M.gguf"): header_answer(pad=8),
        resolve(RB, "caf%C3%A9-Q4_K_M.gguf"): header_answer(pad=8)}),
    rcase("repo-only-support-files", {tree(RB): page([entry("README.md", 1), entry("config.json", 2), entry("tokenizer.model", 3)])}),
    rcase("repo-only-projector", {tree(RB): page([q4("mmproj-a-F16.gguf", 0.9)])}),
    rcase("repo-sizes", {tree(RB): page([
        entry("a-Q4_K_M.gguf", 3 * GB), big("b-Q4_K_M.gguf", 5 * GB), entry("c-Q4_K_M.gguf", 0),
        entry("d-Q4_K_M.gguf", "2000000000"), entry("e-Q4_K_M.gguf", 2000000000.9),
        entry("f-Q4_K_M.gguf", True), entry("g-Q4_K_M.gguf", None, lfs={"size": 400 * GB}),
        entry("h-Q4_K_M.gguf", None, lfs={"size": 0}), entry("i-Q4_K_M.gguf", 6 * GB, lfs={})]),
        resolve(RB, "a-Q4_K_M.gguf"): header_answer(pad=8)}),
    rcase("repo-path-quoting", {tree(RB): page([q4("a b (1)-Q4_K_M.gguf", 1.0), q4("[x]-Q4_K_M.gguf", 1.0)]),
                                resolve(RB, "[x]-Q4_K_M.gguf"): header_answer(pad=8)}),
    rcase("repo-path-quoting-space", {tree(RB): page([q4("a b (1)-Q4_K_M.gguf", 1.0)]),
                                      resolve(RB, "a%20b%20(1)-Q4_K_M.gguf"): header_answer(pad=8)}),
    rcase("repo-path-quoting-unicode", {tree(RB): page([q4("\u4e2d-Q4_K_M.gguf", 1.0)]),
                                        resolve(RB, "%E4%B8%AD-Q4_K_M.gguf"): header_answer(pad=8)}),
    rcase("repo-path-dot-segments", {tree(RB): page([q4("../x/../a-Q4_K_M.gguf", 1.0)]),
                                     resolve(RB, "../x/../a-Q4_K_M.gguf"): header_answer(pad=8),
                                     f"/{RB}/resolve/a-Q4_K_M.gguf": header_answer(pad=8)}),
    rcase("repo-path-question-mark", {tree(RB): page([q4("a?b#c-Q4_K_M.gguf", 1.0)]),
                                      f"/{RB}/resolve/main/a?b": header_answer(pad=8)}),
]


# ---- the grammars -----------------------------------------------------------------------------

class Lcg:
    def __init__(self, seed):
        self.state = seed

    def next(self, bound):
        self.state = (self.state * 6364136223846793005 + 1442695040888963407) % (1 << 64)
        return (self.state >> 33) % bound


def grammar_corpus():
    alphabet = list("019+-_. eExinfatysoN:") + [chr(0xA0), chr(0x663), chr(0x2003)]
    out = [""]
    layer = [""]
    for _ in range(3):
        layer = [s + c for s in layer for c in alphabet]
        out += layer
    rng = Lcg(20261017)
    small = list("0123456789+-_.eE ")
    for _ in range(2500):
        n = 4 + rng.next(10)
        out.append("".join(small[rng.next(len(small))] for _ in range(n)))
    out += ["1" * 25, "9" * 4300, "9" * 4301, "-" + "9" * 40, "+" + "0" * 30 + "1", "1_000_000", "1__0", "_1", "1_",
            "30.0", "30.00", "30.", ".0", "-0", "-0.0", "+0.0", "3_0.0", "30.0_0", "30._0", "1e2", "1E2", "1e+2",
            "1.5e-3", "0.1e1_0", "infinity", "Infinity", "-INF", "+nan", "-nan", "NaN", "nAn", "inf", "iNf", "1e400",
            "-1e400", "1e-400", "0x1", "1,5", "1 5", " 1", "1 ", "\t1\n", "\x0b1\x0c", "\u3000" + "1", "1" + "\u3000",
            "\u0661", "\uff11", "\u00b2", "true", "True", "TRUE", "tRuE", "false", "yes", "no", "on", "off", "y", "n", "t",
            "f", "1", "0", "01", "Y", "N", "YES", "NO", "ON", "OFF", "Yes", "ON ", " on", "", "2", "-1", "enabled"]
    seen = set()
    unique = []
    for s in out:
        if s not in seen:
            seen.add(s)
            unique.append(s)
    return unique


def grammar():
    from pydantic import TypeAdapter, ValidationError
    adapters = {"int": TypeAdapter(int), "float": TypeAdapter(float), "bool": TypeAdapter(bool)}
    corpus = grammar_corpus()
    accepted = {"int": {}, "float": {}, "bool": {}, "int_py": {}}
    for s in corpus:
        for kind, ta in adapters.items():
            try:
                v = ta.validate_python(s)
            except ValidationError as e:
                if kind == "int" and e.errors()[0]["type"] == "int_parsing_size":
                    accepted["int"][s] = "size"
                continue
            accepted[kind][s] = ("true" if v else "false") if kind == "bool" else repr(v)
        try:
            accepted["int_py"][s] = str(int(s))
        except ValueError:
            pass
        except Exception as e:  # noqa: BLE001
            accepted["int_py"][s] = type(e).__name__
    spaces = {"int": [], "float": [], "bool": [], "strip": []}
    for cp in range(0x110000):
        if 0xD800 <= cp <= 0xDFFF:
            continue
        c = chr(cp)
        if c in '0123456789.':
            continue
        for kind, ta, probe in (("int", adapters["int"], "1" + c), ("float", adapters["float"], "1" + c),
                                ("bool", adapters["bool"], "true" + c)):
            try:
                ta.validate_python(probe)
                spaces[kind].append(cp)
            except ValidationError:
                pass
        if c.isspace():
            spaces["strip"].append(cp)
    return {"corpus": corpus, "accepted": accepted, "spaces": spaces}


def iso_corpus():
    templates = ["2026-01-05T10:00:00.000Z", "2026-01-05T10:00:00+00:00", "2026-01-05", "20260105T100000Z",
                 "2026-W02-1T10:00:00Z", "2026W021T100000+01:00", "2026-01-05 10:00:00.5-05:30", "2026-01-05T10",
                 "2026-01-05T10:00", "2026-01-05T10:00:00,25Z", "2026-01-05T10:00:00.123456789+00:00:30.5",
                 "2026-W02", "2026W02", "2026-W02-1", "0001-01-01T00:00:00Z", "9999-12-31T23:59:59.999999Z",
                 "2024-02-29T12:00:00Z", "2023-02-29T12:00:00Z", "2026-13-01T00:00:00Z", "2026-00-10T00:00:00Z",
                 "2026-01-05T24:00:00Z", "2026-01-05T10:60:00Z", "2026-01-05T10:00:60Z", "2026-01-05T10:00:00+24:00",
                 "2026-01-05T10:00:00+23:59:59.999999", "2026-01-05T10:00:00-00:00", "2026-01-05T10:00:00+0000",
                 "2026-01-05T10:00:00+00", "2026-01-05T10:00:00+0", "2026-01-05T10:00:00+000", "2026-01-05T10:00:00.Z",
                 "2026-01-05T10:00:00.1Z", "2026-01-05T10:00:00.123456Z", "2026-01-05T10:00:00.1234567Z",
                 "2026-01-05T10:00:00.12345a", "2026-01-05T1000", "2026-01-05T100000", "2026-01-05T10:0000",
                 "2026-01-05T1000:00", "2026-01-05XYZ", "2026-01-05T", "2026-01-05T10:00:00ZZ", "2026-01-05T10:00:00Z ",
                 " 2026-01-05T10:00:00Z", "2026-01-05T10:00:00 Z", "2026-01-05t10:00:00Z", "2026-1-5T10:00:00Z",
                 "26-01-05T10:00:00Z", "2026/01/05T10:00:00Z", "2026-01-0510:00:00Z", "20260105", "2026-0105",
                 "202601-05", "2026-W1-1", "2026-W53-1", "2020-W53-7", "2026-W00-1", "2026-W02-0", "2026-W02-8",
                 "2026-W02-1-", "2026-W02-1X10:00", "2026-W02-10:00", "2026-W021T10", "2026W02", "2026W0210",
                 "2026W021", "2026W0211", "2026-W02-11", "2026-W02-1T", "0000-01-01T00:00:00Z", "10000-01-01",
                 "2026-01-05T10:00:00+00:00:00", "2026-01-05T10:00:00+01:00:00.5", "2026-01-05T10:00:00+1:00",
                 "2026-01-05T10:00:00-23:59", "1999-12-31T23:59:59.999999-23:59", "9999-12-31T23:59:59+00:01",
                 "0001-01-01T00:00:00+00:01", "0001-01-01T00:00:00-00:01", "2026-01-05T10:00:00+05:30Z",
                 "2026-01-05T10:00:00Z+05:30", "2026-01-05T10:00:00.Z+00:00", "2026-01-05T10:00:00,Z", "2026-01-05T10:00:",
                 "2026-01-05T10::00", "2026-01-05T:10:00", "2026-01-05T1:00:00", "2026-01-05T10:0:00",
                 "2026-01-05T10:00:0", "2026-01-05T10-00-00Z", "2026-01-05T10.00.00Z", "20260105T10:00:00Z",
                 "2026-01-05T100000Z", "2026-01-05T10:00:00.", "2026-01-05T10:00:00,", "2026-01-05T10:00:00.5.5",
                 "2026-01-05T10:00:00+05:30:15.5", "2026-01-05T10:00:00+05:3", "2026-01-05T10:00:00+05:300",
                 "2026-01-05T10:00:00+0530:15", "2026-01-05T10:00:00+053015", "2026-01-05T10:00:00+053015.5",
                 "2026-01-05T10:00:00+05:30:1", "2026-12-31T23:59:59Z", "2026-01-01T00:00:00Z", "1970-01-01T00:00:00Z",
                 "1969-12-31T23:59:59.999999Z", "2026-W01-1", "2025-W01-1", "2024-W01-1T00", "2021-W52-7", "2020-W53-4"]
    rng = Lcg(424242)
    chars = "0123456789-:+TZ.,W /xe\x00\t\u00e9\u0663_"
    out = list(templates)
    for _ in range(4000):
        s = templates[rng.next(len(templates))]
        for _ in range(1 + rng.next(3)):
            op = rng.next(6)
            if not s:
                break
            i = rng.next(len(s))
            if op == 0:
                s = s[:i] + chars[rng.next(len(chars))] + s[i + 1:]
            elif op == 1:
                s = s[:i] + s[i + 1:]
            elif op == 2:
                s = s[:i] + chars[rng.next(len(chars))] + s[i:]
            elif op == 3:
                s = s[:i]
            elif op == 4:
                s = s + templates[rng.next(len(templates))][i % 9:]
            else:
                j = rng.next(len(s))
                s = s[:min(i, j)] + s[max(i, j):]
        out.append(s)
    seen = set()
    unique = []
    for s in out:
        if s not in seen:
            seen.add(s)
            unique.append(s)
    return unique


def unicode_tables():
    """What Python's `re` takes for `\w`, `\d`, `\s` and an IGNORECASE letter, for the characters
    Python's Unicode database assigns (the native side is compared over exactly those)."""
    import re
    import unicodedata

    def ranges(points):
        out = []
        for c in points:
            if out and out[-1][1] == c - 1:
                out[-1][1] = c
            else:
                out.append([c, c])
        return out

    assigned = [c for c in range(0x110000)
                if not 0xD800 <= c <= 0xDFFF and unicodedata.category(chr(c)) != "Cn"]
    word, digit, space = re.compile(r"\w"), re.compile(r"\d"), re.compile(r"\s")
    letters = {}
    for letter in "abcdefghijklmnopqrstuvwxyz":
        rx = re.compile(letter, re.I)
        letters[letter] = [c for c in assigned if c > 127 and rx.fullmatch(chr(c))]
    nd = [c for c in assigned if digit.fullmatch(chr(c))]
    return {"assigned": ranges(assigned),
            "word": ranges(c for c in assigned if word.fullmatch(chr(c))),
            "digit": ranges(nd), "digit_values": [[c, unicodedata.decimal(chr(c))] for c in nd],
            "space": ranges(c for c in assigned if space.fullmatch(chr(c))),
            "letters": letters, "unicode_version": unicodedata.unidata_version}


def numbers():
    from app.utils import human_bytes
    rng = Lcg(99)
    floats = []
    for _ in range(1500):
        bits = (rng.next(1 << 32) << 32) | rng.next(1 << 32)
        x = struct.unpack("<d", struct.pack("<Q", bits))[0]
        if x != x or abs(x) == float("inf") or abs(x) > 1e30:
            continue
        floats.append(x)
    for k in range(0, 700):
        floats.append(k / 8)
        floats.append(k / 1000)
        floats.append(k / 100 + 0.005)
        floats.append(k * 0.7)
    floats += [0.125, 0.375, 2.5, 0.5, 1.5, 2.675, 1.005, 1e22, 1e21, 123456789.125, -0.001, -0.0, 0.0, 5e-324]
    rows = []
    for x in floats:
        rows.append([struct.pack("<d", x).hex(), repr(round(x, 2)), repr(round(x, 1)), format(x, ".1f"),
                     format(x, ".2f")])
    sizes = []
    for _ in range(800):
        sizes.append(rng.next(1 << (1 + rng.next(63))))
    sizes += [0, 1, 1023, 1024, 1025, 1048575, 1048576, 1 << 30, (1 << 30) - 1, 1 << 40, 1 << 50, 1 << 60, (1 << 62) - 1,
              1536, 1023 * 1024 + 512, -1, -1024, -(1 << 20)]
    human = [[n, human_bytes(n)] for n in sizes]
    ratios = []
    for _ in range(1500):
        n = rng.next(1 << (1 + rng.next(63)))
        ratios.append([n, repr(n / 10 ** 6)])
    for n in (0, 1, 2 ** 53, 2 ** 53 + 1, 2 ** 53 + 3, 2 ** 59, 3 * 10 ** 17, 123456789012345678):
        ratios.append([n, repr(n / 10 ** 6)])
    return {"floats": rows, "human_bytes": human, "ratios": ratios}


# ---- the run ----------------------------------------------------------------------------------

def main() -> int:
    app_dir = Path(sys.argv[1]).resolve()
    for key in list(os.environ):
        if key.upper().startswith(("NOEVIA_", "GGUF_", "MODEL_AUTOCONFIG")) or key.upper() in {
                "MODEL_LOADER_TOKEN", "DATA_DIR", "MODELS_DIR", "MODELS_INI_PATH"} or "PROXY" in key.upper():
            del os.environ[key]
    sys.path.insert(0, str(app_dir.parent))
    import datetime as dtmod

    import httpx
    from fastapi.testclient import TestClient  # noqa: E402

    from app import api, config, db, discover, helpers, hf, hw, services  # noqa: E402
    from app.main import app  # noqa: E402

    servers = {}
    for name, h in (("hub", Hubs), ("cdn", Cdns)):
        s = Server(("127.0.0.1", 0), h)
        threading.Thread(target=s.serve_forever, daemon=True).start()
        h.base = f"http://127.0.0.1:{s.server_address[1]}"
        servers[name] = s
    real_base = Hubs.base
    hf.SEARCH_TIMEOUT_S = TIMEOUT

    # Every client the routes build waits TIMEOUT at most (the native test uses the same figure).
    real_init = httpx.AsyncClient.__init__

    def init(self, *args, **kwargs):
        kwargs["timeout"] = TIMEOUT
        real_init(self, *args, **kwargs)

    httpx.AsyncClient.__init__ = init

    # The clock: time.time and datetime.now.
    clock = [NOW0]
    time.time = lambda: clock[0]
    real_datetime = dtmod.datetime

    class FixedDatetime(real_datetime):
        @classmethod
        def now(cls, tz=None):
            return real_datetime.fromtimestamp(clock[0], tz)

    dtmod.datetime = FixedDatetime

    # Neutral trusted publishers, fixed hardware.
    discover.TRUSTED_QUANTISERS = set(QUANTISERS)
    discover.TRUSTED_LABS = set(LABS)
    hw.usable_ram_gb = lambda: 64.0
    hw.host_ram_gb = lambda: 64.0
    hw.gpu_count_for = lambda name: 1
    hw.card_vram_gb_for = lambda name: 24.0
    state = {"backends": BACKENDS}
    helpers._backend_list = lambda: list(state["backends"])

    client = TestClient(app, raise_server_exceptions=False)  # no lifespan: no sampler, no startup
    names = ("api.py", "helpers.py", "hf.py", "discover.py", "db.py", "services.py", "model_files.py",
             "utils.py", "config.py", "main.py", "autoconfig.py", "autoconfig_core.py", "gguf_meta.py")
    digest = {name: hashlib.sha256((app_dir / name).read_bytes()).hexdigest() for name in names}
    out = []
    schema = None
    for case in SEARCH_CASES + REPO_CASES:
        Hubs.responses = case.get("hub", {})
        Cdns.responses = case.get("cdn", {})
        Hubs.base = real_base
        down = "http://127.0.0.1:1" if case.get("hub_down") else real_base
        hf.HF_API = down + "/api"
        hf.HF_RESOLVE = down
        gpu = case.get("gpu", True)
        config.settings.gpu_vram = "synthetic-gpu:24" if gpu else ""
        names_ = (["synthetic-gpu"] if gpu else []) + (["synthetic-cpu"] if case.get("cpu", True) else [])
        services._effective_container_names = lambda n=names_: list(n)
        services._cpu_backends = lambda c=case.get("cpu", True): ["synthetic-cpu"] if c else []
        state["backends"] = case.get("backends", BACKENDS)
        hf._HEADER_CACHE.clear()
        hf._HEADER_GATED.clear()
        api._REPO_FILES.clear()
        with tempfile.TemporaryDirectory() as root:
            config.settings.data_dir = Path(root)
            models = Path(root) / "models"
            models.mkdir()
            for spec in case.get("files", []):
                path = models / spec["name"]
                path.parent.mkdir(parents=True, exist_ok=True)
                with open(path, "wb") as handle:
                    handle.truncate(spec.get("size", 16))
            config.settings.models_dir = models
            config.settings.models_ini_path = models / "models.ini"
            db._DB_PATH = None
            db.init()
            c = sqlite3.connect(Path(root) / "model_loader.db", isolation_level=None)
            if schema is None:
                schema = [r[0] for r in c.execute(
                    "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid")]
            c.execute("DELETE FROM kv")
            for statement in case.get("seed", []):
                c.execute(statement)
            results = []
            steps = [{"requests": case["requests"], **{k: v for k, v in case.items() if k in ("backends",)}}]
            steps += case.get("then", [])
            for part in steps:
                if "backends" in part:
                    state["backends"] = part["backends"]
                if "then_hub" in part:
                    Hubs.responses = {**Hubs.responses, **part["then_hub"]}
                for step in part["requests"]:
                    clock[0] = step["now"]
                    Hubs.seen, Cdns.seen = [], []
                    if case.get("then_hub") and results:
                        Hubs.responses = {**Hubs.responses, **case["then_hub"]}
                    response = client.get(step["get"])
                    key = lambda r: (r["path"], r["authorization"] or "", r["range"] or "")  # noqa: E731
                    result = {"get": step["get"], "now": step["now"], "status": response.status_code,
                              "content_type": response.headers.get("content-type"),
                              "body": response.content.decode("utf-8"),
                              "requests": [] if case.get("native_requests") == "unchecked" else sorted(Hubs.seen, key=key),
                              "backends": state["backends"],
                              "avatars": [list(r) for r in c.execute(
                                  "SELECT owner, url, fetched_at FROM avatar_cache ORDER BY owner")]}
                    if case.get("cdn"):
                        result["cdn_requests"] = sorted(Cdns.seen, key=key)
                    os.environ["NOEVIA_NATIVE_MODEL_MANAGER_SEARCH_CONFIRMED"] = "1"
                    try:
                        if step["get"].startswith("/api/v1/search/repo"):
                            result["runtime"] = repo_runtime(client, response, hf)
                        elif response.status_code != 422 and not (
                                response.status_code == 200 and "error" in response.json()):
                            r = client.get("/api/v1/internal/search-runtime")
                            result["runtime"] = {"status": r.status_code, "body": r.content.decode("utf-8")}
                        else:
                            result["runtime"] = None
                    finally:
                        del os.environ["NOEVIA_NATIVE_MODEL_MANAGER_SEARCH_CONFIRMED"]
                    results.append(result)
            c.close()
            record = {k: v for k, v in case.items() if k not in ("requests", "then")}
            out.append({**record, "results": results})
    for s in servers.values():
        s.shutdown()
    json.dump({"oracle": digest, "schema": schema, "cases": out, "grammar": grammar(),
               "iso": [[s, repr(discover._age_days(s))] for s in iso_corpus()], "numbers": numbers(),
               "unicode": unicode_tables()},
              sys.stdout, ensure_ascii=True, indent=1, sort_keys=True)
    sys.stdout.write("\n")
    return 0


def repo_runtime(client, response, hf):
    """What the native side asks of the child for this repository answer, and the child's reply."""
    if response.status_code != 200:
        return None
    body = response.json()
    groups = body.get("groups") or []
    if not groups:
        return None
    summary = None
    probe = next((g for g in groups if g["files"][0]["path"].lower().endswith(".gguf") and not g["projector"]), None)
    if probe:
        cached = hf._HEADER_CACHE.get(f"{body['repo']}/{probe['files'][0]['path']}")
        summary = cached or None
    mm = [g["bytes"] for g in groups if g["projector"]]
    request = {"groups": [{"bytes": g["bytes"],
                           "estimate": bool(summary) and g["files"][0]["path"].lower().endswith(".gguf")
                           and not g["projector"]} for g in groups],
               "mmprojBytes": min(mm) if mm else None, "summary": summary}
    r = client.post("/api/v1/internal/search-repo-runtime", json=request)
    return {"request": request, "status": r.status_code, "body": r.content.decode("utf-8")}


if __name__ == "__main__":
    raise SystemExit(main())
