#!/usr/bin/env python3
"""Generate crates/model-files/tests/fixtures/model-backups.v1.json (noevia#1021).

`model-files backups` decides which recovery copies of models.ini a write makes and which old
ones it removes (crates/model-files/src/backups.rs). There is no Python implementation in
noevia-services to compare with (its fallback keeps the old behaviour), so `ref_plan` below is an
independent reference written from the specification, used only to produce expectations:

    python3 tools/gen-model-backups.py > crates/model-files/tests/fixtures/model-backups.v1.json

The file is copied verbatim into noevia-services (model-manager/tests/fixtures/). Every name and
time is synthetic.
"""
from __future__ import annotations

import json
import random
import re

KEEP_ROTATING = 10
KEEP_REVISIONS = 10
MAX_KEEP = 1000
MAX_EXISTING = 10_000
MAX_NAME_BYTES = 255
MAX_INPUT_BYTES = 4 * 1024 * 1024
HEX64 = re.compile(r"[0-9a-f]{64}")


def valid_name(s) -> bool:
    return (isinstance(s, str) and s not in ("", ".", "..") and len(s.encode()) <= MAX_NAME_BYTES
            and not any(c in s for c in "/\\\0"))


def is_rev(file: str, name: str) -> bool:
    p = file + ".noevia-backup-"
    return name.startswith(p) and HEX64.fullmatch(name[len(p):]) is not None


def is_rot(file: str, name: str) -> bool:
    p = file + ".bak-"
    return name.startswith(p) and len(name) > len(p)


def uint(v):
    return v if isinstance(v, int) and not isinstance(v, bool) and 0 <= v < 2 ** 128 else None


def ref_plan(text: str) -> dict:
    """The specification, as a reference. Returns the plan or {"error": code}."""
    if len(text.encode()) > MAX_INPUT_BYTES:
        return {"error": "too_large"}
    try:
        v = json.loads(text)
    except ValueError:
        return {"error": "input"}
    bad = {"error": "input"}
    if not isinstance(v, dict):
        return bad
    file = v.get("file")
    if not valid_name(file):
        return bad
    base = v.get("baseRevision")
    if base is not None and not isinstance(base, str):
        return bad
    if base is not None and not HEX64.fullmatch(base):
        return {"error": "base_revision"}
    backup = v.get("backup", True)
    if backup is None:
        backup = True
    if not isinstance(backup, bool):
        return bad
    rot_name = v.get("rotatingName")
    if rot_name is not None and not (isinstance(rot_name, str) and valid_name(rot_name) and is_rot(file, rot_name)):
        return bad
    keep = v.get("keepRevisions")
    if keep is None:
        keep = KEEP_REVISIONS
    elif uint(keep) is None or not 1 <= keep <= MAX_KEEP:
        return bad
    existing = v.get("existing")
    if not isinstance(existing, list) or len(existing) > MAX_EXISTING:
        return bad
    listed = []
    for e in existing:
        if not isinstance(e, dict):
            return bad
        name, mt = e.get("name"), uint(e.get("mtimeNs"))
        if not valid_name(name) or mt is None:
            return bad
        listed.append((name, mt))
    names = [n for n, _ in listed]
    if len(set(names)) != len(names):
        return bad

    rotating = rot_name if backup else None
    revision = f"{file}.noevia-backup-{base}" if (backup and base is not None) else None
    prune = set()
    rot = sorted({n for n in names if is_rot(file, n)} | ({rotating} if rotating else set()), reverse=True)
    prune |= {n for n in rot[KEEP_ROTATING:] if n != rotating}
    big = 2 ** 128
    revs = {n: mt for n, mt in listed if is_rev(file, n)}
    if revision:
        revs[revision] = big
    order = sorted(revs, key=lambda n: (-revs[n], n))
    prune |= {n for n in order[keep:] if n != revision}
    prune &= set(names)
    return {"rotating": rotating, "revision": revision, "prune": sorted(prune)}


def main() -> None:
    rng = random.Random(1021)
    hexs = lambda: "".join(rng.choice("0123456789abcdef") for _ in range(64))  # noqa: E731
    cases = []

    def case(name, req):
        text = req if isinstance(req, str) else json.dumps(req, separators=(",", ":"))
        cases.append({"name": name, "input": text, "expect": ref_plan(text)})

    base = hexs()
    # Hand-written situations.
    case("first write, empty dir", {"file": "models.ini", "baseRevision": base, "rotatingName": "models.ini.bak-20261007-100000", "existing": []})
    case("no base revision: rotating only", {"file": "models.ini", "rotatingName": "models.ini.bak-20261007-100000", "existing": [{"name": "models.ini", "mtimeNs": 1}]})
    tune = [{"name": f"models.ini.noevia-backup-{hexs()}", "mtimeNs": 1_759_800_000_000_000_000 + i * 1_000_000_000} for i in range(30)]
    case("auto-tune spam: 30 revision copies, a normal write", {"file": "models.ini", "baseRevision": base, "rotatingName": "models.ini.bak-20261007-100000", "existing": tune})
    case("auto-tune spam: a hinted write still prunes", {"file": "models.ini", "baseRevision": base, "backup": False, "rotatingName": "models.ini.bak-20261007-100000", "existing": tune})
    case("hinted write keeps the newest pre-tune copy", {"file": "models.ini", "baseRevision": base, "backup": False, "keepRevisions": 1, "existing": tune})
    case("this write's copy already exists and is old", {"file": "models.ini", "baseRevision": tune[0]["name"][-64:], "keepRevisions": 2, "existing": tune[:5]})
    case("equal mtimes: name ascending stays", {"file": "models.ini", "keepRevisions": 2, "existing": [{"name": f"models.ini.noevia-backup-{c * 64}", "mtimeNs": 7} for c in "fedcba"]})
    case("operator names untouched", {"file": "models.ini", "baseRevision": base, "rotatingName": "models.ini.bak-20261007-100000", "keepRevisions": 1, "existing": [
        {"name": n, "mtimeNs": 1} for n in ["models.ini", "models.ini.noevia-backup-notes", "models.ini.noevia-backup-" + "A" * 64, "other.ini.noevia-backup-" + "a" * 64,
                                               "models.ini.bak-before-d3", "models.ini.bak-", "models.ini.tmp-1-2", "models.ini.noevia-backup-" + "a" * 63]]})
    case("rotating: 12 old + new, keep 10 by name", {"file": "models.ini", "baseRevision": base, "rotatingName": "models.ini.bak-20261007-100000-1", "existing": [
        {"name": f"models.ini.bak-202609{d:02}-000000", "mtimeNs": d} for d in range(1, 13)] + [{"name": "models.ini.bak-20261007-100000", "mtimeNs": 99}]})
    case("another preset file name", {"file": "presets.ini", "baseRevision": base, "rotatingName": "presets.ini.bak-1", "keepRevisions": 1, "existing": [
        {"name": "presets.ini.noevia-backup-" + "b" * 64, "mtimeNs": 3}, {"name": "models.ini.noevia-backup-" + "c" * 64, "mtimeNs": 1}]})
    case("huge mtimes", {"file": "models.ini", "keepRevisions": 1, "existing": [{"name": "models.ini.noevia-backup-" + "d" * 64, "mtimeNs": 2 ** 127}, {"name": "models.ini.noevia-backup-" + "e" * 64, "mtimeNs": 2 ** 64}]})
    case("unicode names are escaped", {"file": "modèls.ini", "baseRevision": base, "rotatingName": "modèls.ini.bak-1", "existing": [{"name": "modèls.ini.bak-0", "mtimeNs": 1}]})

    # Seeded random directories.
    for i in range(160):
        file = rng.choice(["models.ini", "models.ini", "presets.ini"])
        n = rng.choice([0, 1, 3, 9, 10, 11, 14, 25])
        existing, seen = [], set()
        for _ in range(n):
            kind = rng.random()
            if kind < 0.5:
                name = f"{file}.noevia-backup-{hexs()}"
            elif kind < 0.85:
                name = f"{file}.bak-2026{rng.randint(1, 12):02}{rng.randint(1, 28):02}-{rng.randint(0, 235959):06d}" + rng.choice(["", "", "-1", "-2"])
            else:
                name = rng.choice([file, f"{file}.tmp-{rng.randint(1, 9)}", "other.ini.bak-1", f"{file}.noevia-backup-x", f"{file}.bak-manual"])
            if name in seen:
                continue
            seen.add(name)
            existing.append({"name": name, "mtimeNs": rng.choice([rng.randint(0, 10), rng.randint(0, 2 ** 63)])})
        req = {"file": file, "existing": existing}
        if rng.random() < 0.8:
            req["baseRevision"] = rng.choice([hexs()] + [e["name"][-64:] for e in existing if ".noevia-backup-" in e["name"] and HEX64.fullmatch(e["name"][-64:])])
        if rng.random() < 0.5:
            req["backup"] = rng.random() < 0.5
        if rng.random() < 0.8:
            req["rotatingName"] = f"{file}.bak-20261007-{rng.randint(0, 235959):06d}"
        if rng.random() < 0.6:
            req["keepRevisions"] = rng.choice([1, 2, 5, 10, 20])
        case(f"random {i}", req)

    # Refusals.
    ok = {"file": "models.ini", "existing": []}
    for name, req in [
        ("not JSON", "{"),
        ("not an object", "[]"),
        ("no existing", {"file": "models.ini"}),
        ("file with a slash", {**ok, "file": "a/models.ini"}),
        ("file dot-dot", {**ok, "file": ".."}),
        ("file too long", {**ok, "file": "m" * 256}),
        ("base revision uppercase", {**ok, "baseRevision": "A" * 64}),
        ("base revision short", {**ok, "baseRevision": "a" * 63}),
        ("base revision not text", {**ok, "baseRevision": 5}),
        ("backup not boolean", {**ok, "backup": "false"}),
        ("backup zero", {**ok, "backup": 0}),
        ("keep zero", {**ok, "keepRevisions": 0}),
        ("keep too many", {**ok, "keepRevisions": 1001}),
        ("keep fractional", {**ok, "keepRevisions": 2.5}),
        ("rotating name for another file", {**ok, "rotatingName": "other.ini.bak-1"}),
        ("rotating name with a slash", {**ok, "rotatingName": "models.ini.bak-1/x"}),
        ("listed name with a NUL", {**ok, "existing": [{"name": "a\u0000b", "mtimeNs": 1}]}),
        ("negative mtime", {**ok, "existing": [{"name": "x", "mtimeNs": -1}]}),
        ("mtime as text", {**ok, "existing": [{"name": "x", "mtimeNs": "1"}]}),
        ("mtime boolean", {**ok, "existing": [{"name": "x", "mtimeNs": True}]}),
        ("repeated name", {**ok, "existing": [{"name": "x", "mtimeNs": 1}, {"name": "x", "mtimeNs": 2}]}),
        ("listed entry not an object", {**ok, "existing": ["x"]}),
    ]:
        case("refused: " + name, req)
    cases.append({"name": "refused: too large", "input": json.dumps(ok) + " " * MAX_INPUT_BYTES, "expect": {"error": "too_large"}})
    big = cases.pop()
    big["input"] = None
    big["pad"] = MAX_INPUT_BYTES
    big["inputBase"] = json.dumps(ok)
    cases.append(big)

    out = {"version": 1, "limits": {"keepRotating": KEEP_ROTATING, "keepRevisions": KEEP_REVISIONS, "maxKeep": MAX_KEEP,
                                    "maxExisting": MAX_EXISTING, "maxNameBytes": MAX_NAME_BYTES, "maxInputBytes": MAX_INPUT_BYTES},
           "cases": cases}
    print(json.dumps(out, indent=1, ensure_ascii=True))


if __name__ == "__main__":
    main()
