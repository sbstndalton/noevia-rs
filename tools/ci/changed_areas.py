#!/usr/bin/env python3
"""Decide whether a pull request needs the whole CI or only the workspace-wide Rust job.

    python3 tools/ci/changed_areas.py --base REV --head REV [--repo DIR]
    python3 tools/ci/changed_areas.py --count-jobs [--repo DIR]

Prints a JSON decision and, under GitHub Actions, writes `full=true|false` to $GITHUB_OUTPUT.
`full=false` (the heavy jobs are skipped) only when every changed file is one of:

- a file under a workspace crate in crates/ that no other workspace member depends on (any
  dependency kind, from `cargo metadata` at --head), whose directory and package name no file
  outside it mentions (workflows, tools, bins, contracts, other crates), so no job other than the
  workspace-wide `cargo fmt/clippy/test` can build, run or read it;
- the root Cargo.toml, if only `workspace.members` gained such crates and `workspace.dependencies`
  only gained keys;
- Cargo.lock, if every package other than those crates is unchanged (added packages are fine);
- README.md, AGENTS.md or docs/ (nothing in CI reads them).

Everything else (bins/, contracts/, tools/, .github/, a crate another member depends on, toolchain
or cargo config, any file not listed above) runs everything, and so does any error: when in doubt,
run. --head must be the checked-out tree (--repo), since `cargo metadata` reads it.
"""
import argparse
import json
import os
import pathlib
import re
import subprocess
import sys
import tomllib

DOCS = ("README.md", "AGENTS.md")
DOCS_DIRS = ("docs/",)
# Where a job could reach a crate by name or path. Markdown is prose; the root manifest and lock
# are checked structurally below.
SKIP_SCAN = ("Cargo.toml", "Cargo.lock")


def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True).stdout


def changed_files(repo, base, head):
    out = git(repo, "diff", "--name-only", "--no-renames", "-z", base, head)
    return sorted(p for p in out.split("\0") if p)


def show_toml(repo, rev, path):
    try:
        text = git(repo, "show", f"{rev}:{path}")
    except subprocess.CalledProcessError:
        return None
    return tomllib.loads(text)


def workspace(repo):
    env = dict(os.environ)
    # Only manifests are read (--no-deps, --offline): the hosted image's stable cargo will do, so
    # the detect job installs no toolchain.
    env.setdefault("RUSTUP_TOOLCHAIN", "stable")
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--offline", "--format-version", "1"],
        cwd=repo, check=True, capture_output=True, text=True, env=env,
    ).stdout
    meta = json.loads(out)
    root = pathlib.Path(meta["workspace_root"]).resolve()
    members = set(meta["workspace_members"])
    pkgs = {}
    for p in meta["packages"]:
        if p["id"] not in members:
            continue
        d = pathlib.Path(p["manifest_path"]).resolve().parent
        pkgs[p["name"]] = {"dir": d.relative_to(root).as_posix(), "deps": p["dependencies"]}
    by_dir = {v["dir"]: k for k, v in pkgs.items()}
    dependents = {k: set() for k in pkgs}
    for name, p in pkgs.items():
        for dep in p["deps"]:
            target = None
            if dep.get("path"):
                rel = pathlib.Path(dep["path"]).resolve()
                if rel.is_relative_to(root):
                    target = by_dir.get(rel.relative_to(root).as_posix())
            elif dep.get("source") is None and dep["name"] in pkgs:
                target = dep["name"]
            if target and target != name:
                dependents[target].add(name)
    return pkgs, dependents


def mentions(repo, crate_dir, names):
    """Files outside crate_dir (and outside markdown, root Cargo.toml/lock) naming the crate."""
    pats = sorted({crate_dir, *names})
    alt = "|".join(re.escape(p) for p in pats)
    rx = f"(^|[^A-Za-z0-9_-])({alt})($|[^A-Za-z0-9_-])"
    excl = [f":(exclude){crate_dir}/**", ":(exclude)*.md", *(f":(exclude,top){p}" for p in SKIP_SCAN)]
    r = subprocess.run(["git", "-C", str(repo), "grep", "-I", "-l", "-E", rx, "--", ".", *excl],
                       capture_output=True, text=True)
    if r.returncode not in (0, 1):
        raise RuntimeError(r.stderr.strip())
    return [l for l in r.stdout.splitlines() if l]


def check_manifest(repo, base, head, leaves, pkgs, reasons):
    b, h = show_toml(repo, base, "Cargo.toml"), show_toml(repo, head, "Cargo.toml")
    if b is None or h is None:
        reasons.append("Cargo.toml: missing on one side")
        return
    bw, hw = dict(b.get("workspace", {})), dict(h.get("workspace", {}))
    bm, hm = bw.pop("members", []), hw.pop("members", [])
    bd, hd = bw.pop("dependencies", {}), hw.pop("dependencies", {})
    if {**b, "workspace": bw} != {**h, "workspace": hw}:
        reasons.append("Cargo.toml: more than workspace members/dependencies changed")
    if not set(bm) <= set(hm):
        reasons.append("Cargo.toml: a workspace member was removed")
    leaf_dirs = {pkgs[n]["dir"] for n in leaves}
    for m in sorted(set(hm) - set(bm)):
        if m.rstrip("/") not in leaf_dirs:
            reasons.append(f"Cargo.toml: added member {m} is not a changed leaf crate")
    for k, v in bd.items():
        if hd.get(k) != v:
            reasons.append(f"Cargo.toml: workspace dependency {k} changed or removed")


def lock_packages(lock):
    """{(name, version, source): (checksum, {resolved dependency keys})}. A dependency is written
    as `name`, `name version` or `name version (source)`, qualified only while the name is
    ambiguous, so resolve it: a new version beside an old one must not read as a change, and a
    replaced version must, even where the text stays `name`."""
    pkgs = lock.get("package", [])
    by_name = {}
    for p in pkgs:
        by_name.setdefault(p["name"], []).append(p)

    def resolve(ref):
        m = re.fullmatch(r"(\S+)(?: (\S+))?(?: \((.+)\))?", ref)
        name, version, source = m.groups() if m else (ref, None, None)
        c = [q for q in by_name.get(name, [])
             if (version is None or q["version"] == version) and (source is None or q.get("source") == source)]
        return (c[0]["name"], c[0]["version"], c[0].get("source")) if len(c) == 1 else ("?", ref, None)

    return {(p["name"], p["version"], p.get("source")):
            (p.get("checksum"), frozenset(resolve(d) for d in p.get("dependencies", []))) for p in pkgs}


def check_lock(repo, base, head, leaves, reasons):
    b, h = show_toml(repo, base, "Cargo.lock"), show_toml(repo, head, "Cargo.lock")
    if b is None or h is None:
        reasons.append("Cargo.lock: missing on one side")
        return
    if {k: v for k, v in b.items() if k != "package"} != {k: v for k, v in h.items() if k != "package"}:
        reasons.append("Cargo.lock: header changed")
    bp, hp = lock_packages(b), lock_packages(h)
    for k, (checksum, deps) in sorted(bp.items(), key=str):
        name, version, source = k
        if source is None and name in leaves:
            continue
        if k not in hp:
            reasons.append(f"Cargo.lock: {name} {version} removed or replaced")
            continue
        hsum, hdeps = hp[k]
        # A leaf turning on a feature of a shared registry crate adds that feature's optional
        # dependencies to its entry. Only builds that include the leaf activate the feature: every
        # job but the workspace-wide one selects its packages with -p, so they never see it.
        # Anything else (a changed checksum, a dropped or re-resolved dependency, any change to a
        # workspace member's entry) runs everything.
        if hsum != checksum or not deps <= hdeps or (source is None and deps != hdeps):
            reasons.append(f"Cargo.lock: {name} {version} changed")
    for (name, version, source) in sorted(set(hp) - set(bp), key=str):
        if source is None and name not in leaves:
            reasons.append(f"Cargo.lock: added workspace package {name} is not a changed leaf crate")


def decide(repo, base, head):
    files = changed_files(repo, base, head)
    reasons, leaves = [], set()
    pkgs, dependents = workspace(repo)
    dirs = sorted(((v["dir"], k) for k, v in pkgs.items()), key=lambda t: -len(t[0]))
    manifests = False
    for f in files:
        if f in ("Cargo.toml", "Cargo.lock"):
            manifests = True
            continue
        if f in DOCS or f.startswith(DOCS_DIRS):
            continue
        owner = next((n for d, n in dirs if f.startswith(d + "/")), None)
        if owner is None or not pkgs[owner]["dir"].startswith("crates/"):
            reasons.append(f"{f}: outside a crates/ workspace member")
            continue
        leaves.add(owner)
    for n in sorted(leaves):
        if dependents[n]:
            reasons.append(f"{n}: depended on by {', '.join(sorted(dependents[n]))}")
            continue
        d = pkgs[n]["dir"]
        hits = mentions(repo, d, {n, n.replace("-", "_"), d.rsplit("/", 1)[-1]})
        if hits:
            reasons.append(f"{n}: named outside its crate in {', '.join(hits[:5])}")
    if manifests:
        if "Cargo.toml" in files:
            check_manifest(repo, base, head, leaves, pkgs, reasons)
        if "Cargo.lock" in files:
            check_lock(repo, base, head, leaves, reasons)
    return {"full": bool(reasons), "files": files, "leaf_crates": sorted(leaves), "reasons": reasons}


JOB = re.compile(r"^  ([A-Za-z0-9_-]+):\s*$")
GATED = re.compile(r"^    if:.*needs\.changes\.outputs\.full")
ADDED = ("changes", "ci-scope-selftest")


def count_jobs(repo):
    """Jobs one pull request event starts: before this scoping, after it when everything runs
    (plus a detect job per workflow and the self-test), and after it on a leaf-only change."""
    before = full = leaf = 0
    for f in sorted((pathlib.Path(repo) / ".github" / "workflows").glob("*.yml")):
        text = f.read_text()
        if not re.search(r"^  pull_request:", text, re.M):
            continue  # the reusable detect workflow; its one job is counted at each caller
        in_jobs, jobs = False, []
        for line in text.splitlines():
            if line.startswith("jobs:"):
                in_jobs = True
            elif line and not line.startswith((" ", "#")):
                in_jobs = False
            elif in_jobs and (m := JOB.match(line)):
                jobs.append([m.group(1), False])
            elif in_jobs and jobs and GATED.match(line):
                jobs[-1][1] = True
        for name, gated in jobs:
            before += name not in ADDED
            full += 1
            leaf += not gated
    return {"before": before, "full": full, "leaf": leaf}


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--repo", default=".")
    ap.add_argument("--base")
    ap.add_argument("--head", default="HEAD")
    ap.add_argument("--count-jobs", action="store_true")
    a = ap.parse_args()
    if a.count_jobs:
        print(json.dumps(count_jobs(a.repo)))
        return 0
    if not a.base:
        ap.error("--base is required")
    try:
        d = decide(a.repo, a.base, a.head)
    except Exception as e:  # noqa: BLE001 - any failure means run everything
        d = {"full": True, "files": [], "leaf_crates": [], "reasons": [f"error: {e}"]}
    print(json.dumps(d, indent=2))
    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a") as fh:
            fh.write(f"full={'true' if d['full'] else 'false'}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
