#!/usr/bin/env python3
"""Compile with Cargo, then execute Store tests from a private cwd (Cargo otherwise resets it)."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--features", default="native-db-init")
    parser.add_argument("--omitted", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    command = ["cargo", "test", "--locked", "-p", "noevia-model-manager", "--features", args.features,
               "--test", "db_init", "--bin", "noevia-model-manager", "--no-run", "--message-format=json"]
    if not args.omitted:
        command.append("--lib")
    output = subprocess.run(command, cwd=root, check=True, stdout=subprocess.PIPE, text=True).stdout
    artifacts = {}
    for line in output.splitlines():
        item = json.loads(line)
        if item.get("reason") == "compiler-artifact" and item.get("profile", {}).get("test") and item.get("executable"):
            artifacts[item["executable"]] = item["target"]["kind"]
    if len(artifacts) != (2 if args.omitted else 3):
        raise SystemExit(f"expected test binaries missing: {len(artifacts)}")
    with tempfile.TemporaryDirectory(prefix="qa-db-init-tests-", dir="/tmp") as folder:
        os.chmod(folder, 0o700)
        old = os.umask(0o077)
        try:
            for executable, kinds in sorted(artifacts.items()):
                filters = ["prompts::"] if "lib" in kinds else []
                subprocess.run([executable, *filters, "--test-threads=1"], cwd=folder, check=True, timeout=180)
        finally:
            os.umask(old)

if __name__ == "__main__":
    main()
