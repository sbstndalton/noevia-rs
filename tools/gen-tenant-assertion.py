#!/usr/bin/env python3
"""Generate crates/tenant-assertion/tests/fixtures/tenant-assertion.v1.json.

The reference is noevia-services' diary/agent/tenant_assertion.py (`verify`, with its nonce cache
reset before every case so replay never decides one, and `storage_secret_ref` + the
`hmac.compare_digest` check app.py runs). Every key, tenant and secret here is synthetic. The
output does not depend on the Python version or the locale: inputs come from a seeded
getrandbits stream, `now` is fixed, and the file is written with sorted keys and ASCII escapes.

    cargo build --release -p tenant-assertion-cli
    python3 -I tools/gen-tenant-assertion.py --diary <noevia-services>/diary \
        --bin target/release/tenant-assertion

Each case records Python's decision and the binary's. The generator refuses to write the file if
the binary ever accepts what Python rejects, or if the two disagree without a declared reason
(`stricter`, for the inputs the Rust side refuses on purpose). Seeded mutants only need to keep
the safety rule.

The fixture file is copied verbatim into noevia-services (diary/tests/fixtures/), where CI `cmp`s
it against this repo's copy at the pinned ref and replays it against the live Python module and
the binary.
"""
from __future__ import annotations

import argparse
import hashlib
import hmac
import importlib.util
import json
import random
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "crates" / "tenant-assertion" / "tests" / "fixtures" / "tenant-assertion.v1.json"
KEY = "synthetic-tenant-key-for-fixtures"
USER = "11111111-1111-4111-8111-111111111111"
OTHER = "22222222-2222-4222-8222-222222222222"
NOW = 1_760_000_000.25
TS = 1_760_000_000
NONCE = "0123456789abcdef0123456789abcdef"
EMPTY = hashlib.sha256(b"").hexdigest()


def load(diary: Path):
    spec = importlib.util.spec_from_file_location("tenant_assertion_ref", diary / "agent" / "tenant_assertion.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def python_verify(ta, i):
    ta._reset_for_tests()
    headers = {"X-Cowork-User-ID": i["user_id"], ta.HEADER: i["assertion"], "X-Cowork-Storage": i["storage"],
               "X-Cowork-Legacy-Owner": i["legacy_owner"], "X-Cowork-Storage-Blocked": i["blocked"]}
    reason = ta.verify(i["key"], headers, i["method"], i["path"], now=float(i["now"]),
                       query=bytes.fromhex(i["query_hex"]), body_hash=i["body_hash"])
    return "accept" if reason is None else "reject: " + reason


def python_secret_ref(ta, i):
    ok = hmac.compare_digest(i["ref"], ta.storage_secret_ref(i["key"], i["user_id"], i["secret"]))
    return "accept" if ok else "reject: bad signature"


def run_bin(binary, i):
    p = subprocess.run([binary, "check"], input=json.dumps(i).encode(), capture_output=True, env={}, timeout=10)
    out = p.stdout.decode().strip()
    if p.returncode == 0 and out == "accept":
        return "accept"
    if p.returncode == 1 and out.startswith("reject: "):
        return out
    return f"fault: exit {p.returncode}"


def base(**kw):
    i = {"op": "verify", "key": KEY, "user_id": USER, "assertion": "", "method": "GET", "path": "/api/diary/entries",
         "query_hex": "", "body_hash": EMPTY, "storage": "", "legacy_owner": "", "blocked": "", "now": repr(NOW)}
    i.update(kw)
    return i


def signed(ta, sign_with=None, ts=TS, nonce=NONCE, **kw):
    """A request whose assertion is Python's signature over `sign_with` (default: the request)."""
    i = base(**kw)
    s = dict(i, **(sign_with or {}))
    headers = {"X-Cowork-Storage": s["storage"], "X-Cowork-Legacy-Owner": s["legacy_owner"],
               "X-Cowork-Storage-Blocked": s["blocked"]}
    i["assertion"] = ta.sign(s["key"], s["user_id"], s["method"], s["path"], headers, ts, nonce,
                             bytes.fromhex(s["query_hex"]), s["body_hash"])
    return i


def cases(ta):
    c = []
    add = lambda name, i, stricter=None: c.append({"name": name, "input": i, **({"stricter": stricter} if stricter else {})})
    add("valid GET", signed(ta))
    add("valid POST json body", signed(ta, method="POST", body_hash=hashlib.sha256(b'{"text":"synthetic"}').hexdigest()))
    add("valid stream body", signed(ta, method="PUT", body_hash="stream"))
    add("valid query", signed(ta, query_hex=b"limit=5&q=caf%C3%A9".hex()))
    add("valid non-utf8 query bytes", signed(ta, query_hex="ff00fe"))
    add("valid storage legacy blocked", signed(ta, storage="eyJraW5kIjoid2ViZGF2In0", legacy_owner="legacy", blocked="1"))
    add("valid upper-case user id", signed(ta, user_id=USER.upper()))
    add("valid lower-case method", signed(ta, method="post"))
    add("valid encoded path", signed(ta, path="/api/files/a%20b/%C3%A9.md"))
    add("valid unicode path and storage", signed(ta, path="/api/files/é", storage="☃"))
    add("valid long key", signed(ta, key="k" * 200))
    add("valid ts 0 far window", signed(ta, ts=0, now="30.0"))
    add("window edge +60", signed(ta, now=repr(TS + 60.0)))
    add("window edge -60", signed(ta, now=repr(TS - 60.0)))
    add("window just past +60", signed(ta, now=repr(TS + 60.000001)))
    add("window just past -60", signed(ta, now=repr(TS - 60.000001)))
    add("window far future", signed(ta, now="1e12"))
    add("missing tenant", signed(ta, user_id=""))
    add("missing assertion", base())
    good = signed(ta)["assertion"]
    head, sig = good.rsplit(".", 1)
    for name, bad in [("leading zero ts", good.replace(f"v2.{TS}.", f"v2.0{TS}.")),
                      ("13 digit ts", good.replace(f"v2.{TS}.", "v2.1234567890123.")),
                      ("upper-case nonce", good.replace(NONCE, NONCE.upper())),
                      ("short signature", head + "." + sig[:-1]),
                      ("padded signature", good + "="),
                      ("trailing dot", good + "."),
                      ("v1 prefix", "v1" + good[2:]),
                      ("leading space", " " + good),
                      ("standard base64 chars", head + "." + sig[:-1] + "+")]:
        add("malformed " + name, base(assertion=bad))
    add("malformed unicode digit ts", base(assertion=good.replace("v2.1", "v2.\u0661", 1)),
        "ASCII digits only (Python's \\d also matches U+0661, then fails the signature)")
    add("bad signature wrong key", signed(ta, sign_with={"key": "another-synthetic-key"}))
    add("bad signature other tenant", signed(ta, sign_with={"user_id": OTHER}))
    for field, value in [("method", "POST"), ("path", "/api/diary/entrieS"), ("query_hex", "00"),
                         ("body_hash", "stream"), ("storage", "x"), ("legacy_owner", "x"), ("blocked", "1")]:
        add(f"bad signature tampered {field}", signed(ta, sign_with={field: value}))
    add("bad signature flipped last char", base(assertion=head + "." + sig[:-1] + ("B" if sig[-1] != "B" else "C")))
    add("bad signature and outside window", signed(ta, sign_with={"key": "another-synthetic-key"}, now="0.0"))
    # Stricter than Python on purpose (Python accepts these when they are correctly signed).
    add("stricter non-ascii user id", signed(ta, user_id="café"), "non-ASCII user id")
    add("stricter non-ascii method", signed(ta, method="GÉT"), "non-ASCII method")
    add("stricter free-form body hash", signed(ta, body_hash="not-a-hash"), "body hash is not sha256 hex or stream")
    add("stricter upper-case body hash", signed(ta, body_hash=EMPTY.upper()), "body hash is not sha256 hex or stream")
    add("stricter empty key", signed(ta, key=""), "empty key")
    # secretRef (app.py _storage_credential).
    ref = ta.storage_secret_ref(KEY, USER, "synthetic-secret")
    sr = lambda **kw: {"op": "secret_ref", "key": KEY, "user_id": USER, "secret": "synthetic-secret", "ref": ref, **kw}
    add("secret ref match", sr())
    add("secret ref match upper-case user", sr(user_id=USER.upper()))
    add("secret ref other secret", sr(secret="other-secret"))
    add("secret ref other tenant", sr(user_id=OTHER))
    add("secret ref empty", sr(ref=""))
    add("secret ref full digest", sr(ref=hmac.new(KEY.encode(), f"{ta.LABEL}:storage-secret\n{USER}\nsynthetic-secret".encode(), hashlib.sha256).hexdigest()))
    add("stricter secret ref non-ascii user", sr(user_id="café", ref=ta.storage_secret_ref(KEY, "café", "synthetic-secret")), "non-ASCII user id")
    # Seeded mutants of valid requests: one character of the assertion or one field changed.
    rng = random.Random(20261009)
    alphabet = "0123456789abcdefABCDEF-_.v=+/ é"
    valid = [x["input"] for x in c if x["name"].startswith("valid")]
    for n in range(400):
        i = dict(valid[rng.getrandbits(16) % len(valid)])
        a = i["assertion"]
        pos = rng.getrandbits(16) % len(a)
        mode = rng.getrandbits(2)
        ch = alphabet[rng.getrandbits(8) % len(alphabet)]
        if mode == 0:
            i["assertion"] = a[:pos] + ch + a[pos + 1:]
        elif mode == 1:
            i["assertion"] = a[:pos] + a[pos + 1:]
        elif mode == 2:
            i["assertion"] = a[:pos] + ch + a[pos:]
        else:
            field = ["user_id", "method", "path", "storage", "legacy_owner", "blocked"][rng.getrandbits(8) % 6]
            i[field] = i[field] + ch
        add(f"mutant {n}", i)
    return c


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--diary", required=True, type=Path)
    ap.add_argument("--bin", required=True)
    ap.add_argument("--check", action="store_true", help="compare with the committed file instead of writing")
    args = ap.parse_args()
    ta = load(args.diary)
    out, problems = [], []
    for case in cases(ta):
        i = case["input"]
        py = python_verify(ta, i) if i["op"] == "verify" else python_secret_ref(ta, i)
        rs = run_bin(args.bin, i)
        if rs == "accept" and py != "accept":
            problems.append(f"{case['name']}: Rust accepts what Python rejects ({py})")
        if not rs.startswith(("accept", "reject: ")):
            problems.append(f"{case['name']}: binary fault {rs}")
        if not case["name"].startswith("mutant"):
            if "stricter" in case:
                if not (py == "accept" and rs.startswith("reject")) and not (
                        py.startswith("reject") and rs.startswith("reject")):
                    problems.append(f"{case['name']}: declared stricter but python={py} rust={rs}")
            elif py != rs:
                problems.append(f"{case['name']}: python={py} rust={rs}")
        out.append({**case, "python": py, "rust": rs})
    if problems:
        sys.exit("refusing to write fixtures:\n  " + "\n  ".join(problems))
    text = json.dumps({"version": 1, "reference": "noevia-services diary/agent/tenant_assertion.py", "cases": out},
                      sort_keys=True, ensure_ascii=True, indent=1) + "\n"
    if args.check:
        if OUT.read_text() != text:
            sys.exit(f"{OUT} is stale; rerun without --check")
        print(f"{OUT.name} up to date ({len(out)} cases)")
        return
    OUT.write_text(text)
    print(f"wrote {OUT} ({len(out)} cases)")


if __name__ == "__main__":
    main()
