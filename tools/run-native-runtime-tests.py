#!/usr/bin/env python3
"""Run combined native runtime test harnesses from a private cwd, never production binaries."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--capability-omission", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    if not args.capability_omission:
        # These probes otherwise deliberately allow a local skip or use a historical /tmp gate.
        # The aggregate qualification must exercise the actual, independently pinned sources.
        for key in ["PYTHON", "NOEVIA_PROMPTS_APP", "STARTUP_TEST_PYTHON",
                    "STARTUP_TEST_APP", "NOEVIA_DIAGNOSE_SDK_ORACLE"]:
            if not os.environ.get(key):
                raise SystemExit(f"aggregate qualification requires {key}")
        for key in ["NOEVIA_PROMPTS_APP", "STARTUP_TEST_APP", "NOEVIA_DIAGNOSE_SDK_ORACLE"]:
            path = Path(os.environ[key])
            if not path.is_absolute() or not path.exists():
                raise SystemExit(f"aggregate qualification requires an existing absolute {key}")
    command = ["cargo", "test", "--locked", "-p", "noevia-model-manager"]
    if args.capability_omission:
        command += ["--no-default-features", "--features", "native-logs", "--test", "lifecycle"]
    else:
        command += ["--all-features", "--all-targets"]
    command += ["--no-run", "--message-format=json"]
    output = subprocess.run(command, cwd=root, check=True, stdout=subprocess.PIPE, text=True).stdout
    artifacts = {}
    for line in output.splitlines():
        item = json.loads(line)
        if (item.get("reason") == "compiler-artifact"
                and item.get("profile", {}).get("test") is True
                and item.get("executable")):
            artifacts[item["executable"]] = item["target"]["name"]
    if not artifacts or (args.capability_omission and len(artifacts) != 1):
        raise SystemExit("required test harness artifacts missing")
    with tempfile.TemporaryDirectory(prefix="qa-native-runtime-tests-", dir="/tmp") as folder:
        os.chmod(folder, 0o700)
        old = os.umask(0o077)
        try:
            for executable, name in sorted(artifacts.items()):
                print(f"Running test harness: {name}", flush=True)
                filters = (["--exact", "advertised_feature_matches_build_and_never_spawns_child"]
                           if args.capability_omission else [])
                subprocess.run([executable, *filters, "--test-threads=1"],
                               cwd=folder, check=True, timeout=300)
        finally:
            os.umask(old)

if __name__ == "__main__":
    main()
