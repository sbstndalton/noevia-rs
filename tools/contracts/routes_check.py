#!/usr/bin/env python3
"""Check that every /api/ path the web client names is listed in contracts/http/routes.toml.

Usage:
  routes_check.py --web <noevia-web checkout> [--routes contracts/http/routes.toml]   check
  routes_check.py --web <noevia-web checkout> --list                                   print paths

Scans src/**/*.{ts,tsx} (tests excluded) for string and template literals that start with
/api/. A template expression becomes a {name} segment ({id} for encodeURIComponent(id), {*}
otherwise); query strings are dropped. Exit 1 if a client path is missing from routes.toml.
Paths in routes.toml the client no longer names are reported as a warning, not a failure (the
server may still serve them to other callers).
"""
import argparse
import pathlib
import re
import sys
import tomllib

ROUTE_PARAM = re.compile(r"\{[^}/]*\}")


def literals(text):
    """Yield the raw body of every '...', "..." and `...` literal (template ${} kept verbatim)."""
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c == "/" and text.startswith("//", i):
            j = text.find("\n", i)
            i = n if j < 0 else j
            continue
        if c == "/" and text.startswith("/*", i):
            j = text.find("*/", i + 2)
            i = n if j < 0 else j + 2
            continue
        if c in "'\"":
            j = i + 1
            while j < n and text[j] != c and text[j] != "\n":
                j += 2 if text[j] == "\\" else 1
            yield text[i + 1:j]
            i = j + 1
            continue
        if c == "`":
            j, out, depth = i + 1, [], 0
            while j < n:
                ch = text[j]
                if ch == "\\":
                    out.append(text[j:j + 2]); j += 2; continue
                if depth == 0 and ch == "`":
                    break
                if text.startswith("${", j):
                    depth += 1; out.append("${"); j += 2; continue
                if depth and ch == "{":
                    depth += 1
                elif depth and ch == "}":
                    depth -= 1
                out.append(ch); j += 1
            yield "".join(out)
            i = j + 1
            continue
        i += 1


def expr_name(expr):
    m = re.fullmatch(r"\s*encodeURIComponent\(\s*([A-Za-z_$][\w$.]*)\s*\)\s*", expr)
    if m:
        return m.group(1).split(".")[-1]
    return "*"


def normalise(body):
    """'/api/x/${encodeURIComponent(id)}/y?z=1' -> '/api/x/{id}/y'; None if not a path."""
    if not body.startswith("/api/"):
        return None
    out, i = [], 0
    while i < len(body):
        if body.startswith("${", i):
            depth, j = 1, i + 2
            while j < len(body) and depth:
                depth += {"{": 1, "}": -1}.get(body[j], 0)
                j += 1
            expr = body[i + 2:j - 1]
            # A trailing optional suffix (`${path ? `/${...}` : ''}`) or query builder is not a segment.
            if out and out[-1].endswith("/"):
                out.append("{" + expr_name(expr) + "}")
            elif "?" in expr and ":" in expr:
                out.append("{*?}")
            i = j
            continue
        ch = body[i]
        if ch in "?#":
            break
        if ch in " \t":
            return None  # prose such as 'PUT /api/x failed'
        out.append(ch)
        i += 1
    path = "".join(out).rstrip("/")
    return path or None


def client_paths(web):
    src = pathlib.Path(web) / "src"
    if not src.is_dir():
        sys.exit(f"routes_check: {src} is not a directory")
    found = {}
    for f in sorted(list(src.rglob("*.ts")) + list(src.rglob("*.tsx"))):
        if ".test." in f.name or "/__tests__/" in str(f):
            continue
        text = f.read_text(encoding="utf-8")
        for body in literals(text):
            p = normalise(body)
            if p:
                found.setdefault(p, set()).add(str(f.relative_to(web)))
    return found


def shape(path):
    """Compare paths with parameter names erased: /api/x/{id} == /api/x/{chatId}."""
    return ROUTE_PARAM.sub("{}", path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--web", required=True)
    ap.add_argument("--routes", default=str(pathlib.Path(__file__).resolve().parents[2] / "contracts/http/routes.toml"))
    ap.add_argument("--list", action="store_true")
    a = ap.parse_args()
    found = client_paths(a.web)
    if a.list:
        for p in sorted(found):
            print(p, "\t", ",".join(sorted(found[p])))
        return 0
    with open(a.routes, "rb") as fh:
        doc = tomllib.load(fh)
    listed = {}
    for r in doc.get("route", []):
        if not isinstance(r.get("path"), str) or r.get("owner") not in ("node", "rust"):
            print(f"routes_check: bad entry {r!r} (needs path and owner = node|rust)", file=sys.stderr)
            return 1
        listed[shape(r["path"])] = r
    missing = sorted(p for p in found if shape(p) not in listed)
    stale = sorted(r["path"] for k, r in listed.items() if k not in {shape(p) for p in found} and r.get("client", True))
    for p in missing:
        print(f"::error::web client path {p} ({', '.join(sorted(found[p]))}) is missing from routes.toml")
    for p in stale:
        print(f"::warning::routes.toml lists {p} but the web client no longer names it (set client = false if a non-web caller uses it)")
    print(f"routes_check: {len(found)} client paths, {len(listed)} listed, {len(missing)} missing")
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
