#!/usr/bin/env python3
"""Project replay seed wiring and shared utility controls; stdlib, no servers."""
import importlib.util
from pathlib import Path
import shutil
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]


def load_utility(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


# Reuse every security/configuration negative control without running the
# Statistics workflow's shell-based fixture test or any application code.
shared = load_utility("stats_seed_tests", ROOT / "tools/stats-seed.test.py")
HelperContract = shared.HelperContract
seed_helper = load_utility("stats_seed", ROOT / "tools/stats-seed.py")


def project_steps():
    workflow = (ROOT / ".github/workflows/ci.yml").read_text()
    job = workflow.split("\n  corpus-replay-front-rust-projects:\n", 1)[1]
    job = job.split("\n  dav-parse-wasm:\n", 1)[0]
    reference, native = job.split("      - name: Replay through the Rust front\n", 1)
    reference = reference.split(
        "      - name: Replay against Node directly (reference data dir)\n", 1
    )[1]
    return reference, native


class ProjectSeedContract(unittest.TestCase):
    def test_common_seed_matches_both_native_lock_schedules(self):
        reference_step, _ = project_steps()
        setup = reference_step.split("target/debug/replay run", 1)[0]
        initialize = 'python3 tools/stats-seed.py initialize "$RUNNER_TEMP/seed"'
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            seed = root / "seed"
            seed.mkdir()
            # Model only this exact utility invocation, before the real replay's
            # copy boundary. No workflow shell, product imports or subprocesses.
            if initialize in setup:
                seed_helper.initialize(seed)
            reference = root / "reference"
            shutil.copytree(seed, reference)
            for touched in (False, True):
                with self.subTest(native_acquires_lock=touched):
                    work = root / str(touched)
                    shutil.copytree(seed, work)
                    if touched:
                        shared.touch_with_flock(work)
                    self.assertEqual(shared.paths(reference), shared.paths(work),
                                     "native-only extra embedding-config.json.lock")
                    seed_helper.verify(seed, reference)
                    seed_helper.verify(seed, work)
                    self.assertFalse((work / shared.CONFIG).exists())

    def test_workflow_validates_both_copies_and_keeps_full_state_comparison(self):
        reference, native = project_steps()
        initialize = 'python3 tools/stats-seed.py initialize "$RUNNER_TEMP/seed"'
        self.assertEqual(reference.count(initialize), 1)
        self.assertLess(reference.index(initialize), reference.index("target/debug/replay run"))
        for block, work in ((reference, "work-node"), (native, "work-front")):
            verify = f'python3 tools/stats-seed.py verify "$RUNNER_TEMP/seed" "$RUNNER_TEMP/{work}"'
            self.assertEqual(block.count(verify), 1)
            self.assertGreater(block.index(verify), block.index("target/debug/replay run"))
            self.assertIn("set -euo pipefail", block)
        self.assertIn('--native-move-state --server-cmd', native)
        self.assertIn('--expect-state "$RUNNER_TEMP/node-state.json"', native)
        self.assertNotIn("--state-ignore '*embedding-config", native)
        self.assertNotIn("--state-ignore '*.lock'", native)

    def test_unseeded_lock_is_the_exact_constructed_extra_path(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            reference, work = root / "reference", root / "work"
            reference.mkdir()
            work.mkdir()
            shared.touch_with_flock(work)
            self.assertEqual(set(shared.paths(work)) - set(shared.paths(reference)),
                             {shared.LOCK})
            self.assertEqual(set(shared.paths(reference)) - set(shared.paths(work)), set())
            self.assertFalse((work / shared.CONFIG).exists())


if __name__ == "__main__":
    unittest.main()
