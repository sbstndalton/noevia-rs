#!/usr/bin/env python3
"""Pure synthetic fixture checks; no server, native binary, network or model."""
import fcntl
import importlib.util
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parents[1]
LOCK = "embedding-config.json.lock"
CONFIG = "embedding-config.json"


def paths(root):
    return sorted(str(p.relative_to(root)) + ("/" if p.is_dir() else "")
                  for p in root.rglob("*"))


def touch_with_flock(root):
    # OS flock has the same persistent-inode semantics as the native API.
    # The Rust integration test separately exercises that exact existing API.
    fd = os.open(root / LOCK, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    finally:
        os.close(fd)


def workflow_seed(root):
    workflow = (ROOT / ".github/workflows/stats.yml").read_text()
    block = workflow.split("- name: Replay against Node directly (reference data dir)", 1)[1]
    prefix = block.split("run: |", 1)[1].split("target/debug/replay run", 1)[0]
    env = dict(os.environ, RUNNER_TEMP=str(root))
    subprocess.run(["bash", "-euc", prefix], cwd=ROOT, env=env, check=True)
    return root / "seed"


class SeedContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_common_seed_survives_both_worker_schedules(self):
        seed = workflow_seed(self.root)
        reference = self.root / "reference"
        shutil.copytree(seed, reference)
        for touched in (False, True):
            work = self.root / str(touched)
            shutil.copytree(seed, work)
            if touched:
                touch_with_flock(work)
            self.assertEqual(paths(reference), paths(work),
                             "native-only extra embedding-config.json.lock")
            self.assertFalse((work / CONFIG).exists())
        lock = seed / LOCK
        self.assertTrue(stat.S_ISREG(lock.lstat().st_mode))
        self.assertEqual(stat.S_IMODE(lock.stat().st_mode), 0o600)
        self.assertEqual(lock.read_bytes(), b"")

    def test_unseeded_flock_reproduces_exact_extra_path(self):
        reference = self.root / "reference"
        work = self.root / "work"
        reference.mkdir()
        work.mkdir()
        touch_with_flock(work)
        self.assertEqual(set(paths(work)) - set(paths(reference)), {LOCK})
        self.assertEqual(set(paths(reference)) - set(paths(work)), set())


class HelperContract(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location("stats_seed", ROOT / "tools/stats-seed.py")
        self.helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.helper)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.seed = self.root / "seed"
        self.seed.mkdir()
        self.helper.initialize(self.seed)
        self.work = self.root / "work"
        shutil.copytree(self.seed, self.work)

    def test_release_and_reacquire_preserve_inode_and_config_bytes(self):
        config = b'{"version":1,"synthetic":true}\n'
        for directory in (self.seed, self.work):
            (directory / CONFIG).write_bytes(config)
        before = (self.work / LOCK).stat()
        for _ in range(2):
            touch_with_flock(self.work)
            self.helper.verify(self.seed, self.work)
            after = (self.work / LOCK).stat()
            self.assertEqual((before.st_dev, before.st_ino), (after.st_dev, after.st_ino))
            self.assertEqual((self.work / CONFIG).read_bytes(), config)

    def test_lock_corruption_is_rejected(self):
        lock = self.work / LOCK
        for case in ("deleted", "nonempty", "directory", "mode", "symlink", "hardlink"):
            with self.subTest(case=case):
                if lock.is_dir() and not lock.is_symlink():
                    lock.rmdir()
                elif lock.exists() or lock.is_symlink():
                    lock.unlink()
                lock.touch(mode=0o600)
                if case == "deleted":
                    lock.unlink()
                elif case == "nonempty":
                    lock.write_bytes(b"unexpected")
                elif case == "directory":
                    lock.unlink()
                    lock.mkdir()
                elif case == "mode":
                    lock.chmod(0o644)
                elif case == "symlink":
                    lock.unlink()
                    lock.symlink_to(self.seed / LOCK)
                elif case == "hardlink":
                    os.link(lock, self.root / "other-inode-name")
                with self.assertRaises((OSError, ValueError)):
                    self.helper.verify(self.seed, self.work)

    def test_configuration_is_never_normalized(self):
        config = self.work / CONFIG
        config.write_bytes(b"unexpected config")
        with self.assertRaises(ValueError):
            self.helper.verify(self.seed, self.work)
        (self.seed / CONFIG).write_bytes(config.read_bytes())
        self.helper.verify(self.seed, self.work)
        config.write_bytes(b"changed config")
        with self.assertRaises(ValueError):
            self.helper.verify(self.seed, self.work)
        config.unlink()
        with self.assertRaises(ValueError):
            self.helper.verify(self.seed, self.work)

    def test_configuration_wrong_type_and_symlink_are_rejected(self):
        config = self.work / CONFIG
        config.mkdir()
        with self.assertRaises(ValueError):
            self.helper.verify(self.seed, self.work)
        config.rmdir()
        (self.seed / CONFIG).write_bytes(b"synthetic")
        config.symlink_to(self.seed / CONFIG)
        with self.assertRaises(OSError):
            self.helper.verify(self.seed, self.work)

    def test_initialization_never_repairs_existing_lock(self):
        lock = self.seed / LOCK
        lock.write_bytes(b"do not truncate")
        with self.assertRaises(FileExistsError):
            self.helper.initialize(self.seed)
        self.assertEqual(lock.read_bytes(), b"do not truncate")

    def test_unexpected_files_remain_visible_to_state_comparison(self):
        for name in ("sibling.lock", "nested/embedding-config.json.lock", "product.json"):
            with self.subTest(name=name):
                work = self.root / name.replace("/", "-")
                shutil.copytree(self.seed, work)
                unexpected = work / name
                unexpected.parent.mkdir(parents=True, exist_ok=True)
                unexpected.touch()
                # No ignore/projection: the real replay comparator is tested in Rust.
                self.assertNotEqual(paths(self.seed), paths(work))
                self.assertIn(name, paths(work))


if __name__ == "__main__":
    unittest.main()
