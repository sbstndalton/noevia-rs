"""Tests for changed_areas.py: python3 -m unittest -v tools/ci/test_changed_areas.py

Replays five merged pull requests (changed_areas_samples.json: the file list GitHub reports, the
merge commit) through the decision at their own merge commits, and prints the jobs each would
start before and after the scoping. Needs the full history (checkout fetch-depth: 0) and cargo.
"""
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).parent
sys.path.insert(0, str(HERE))
import changed_areas  # noqa: E402

REPO = HERE.parent.parent
SAMPLES = json.loads((HERE / "changed_areas_samples.json").read_text())


def git(*args, cwd=REPO):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout


class Samples(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix="changed-areas-")
        cls.decisions = {}
        for s in SAMPLES:
            wt = pathlib.Path(cls.tmp.name) / str(s["pr"])
            git("worktree", "add", "--detach", "--quiet", str(wt), s["merge"])
            try:
                cls.decisions[s["pr"]] = changed_areas.decide(wt, s["merge"] + "^1", s["merge"])
            finally:
                git("worktree", "remove", "--force", str(wt))
        jobs = changed_areas.count_jobs(REPO)
        rows = ["| PR | kind | decision | jobs before | jobs after |", "|---|---|---|---|---|"]
        for s in SAMPLES:
            full = cls.decisions[s["pr"]]["full"]
            rows.append(f"| noevia-rs#{s['pr']} | {s['kind']} | {'full' if full else 'leaf-only'} "
                        f"| {jobs['before']} | {jobs['full'] if full else jobs['leaf']} |")
        print("\n" + "\n".join(rows), file=sys.stderr)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_file_lists_are_the_merged_ones(self):
        for s in SAMPLES:
            with self.subTest(pr=s["pr"]):
                self.assertEqual(self.decisions[s["pr"]]["files"], s["files"])

    def test_decisions(self):
        for s in SAMPLES:
            with self.subTest(pr=s["pr"]):
                d = self.decisions[s["pr"]]
                self.assertEqual(d["full"], s["full"], d["reasons"])

    def test_leaf_samples_name_their_crate(self):
        for s in SAMPLES:
            if s["kind"] == "leaf":
                crates = {f.split("/")[1] for f in s["files"] if f.startswith("crates/")}
                self.assertEqual(self.decisions[s["pr"]]["leaf_crates"], sorted(crates))


class Synthetic(unittest.TestCase):
    """Edits on top of the current tree, decided against HEAD (one temporary commit each)."""

    def run_edit(self, edit, base_edit=None):
        """base_edit (if any) is committed first and is the base; edit is what the PR changes."""
        with tempfile.TemporaryDirectory(prefix="changed-areas-") as tmp:
            wt = pathlib.Path(tmp) / "wt"
            git("worktree", "add", "--detach", "--quiet", str(wt), "HEAD")
            try:
                for step in (base_edit, edit):
                    if step:
                        step(wt)
                        git("add", "-A", cwd=wt)
                        git("-c", "user.name=qa", "-c", "user.email=qa@example.invalid",
                            "commit", "--quiet", "--no-verify", "-m", "qa", cwd=wt)
                return changed_areas.decide(wt, "HEAD^1", "HEAD")
            finally:
                git("worktree", "remove", "--force", str(wt))

    def new_crate(self, wt, name="qa-leaf-crate", body="pub fn qa() {}\n"):
        d = wt / "crates" / name
        (d / "src").mkdir(parents=True)
        (d / "Cargo.toml").write_text(
            f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition.workspace = true\n'
            'license.workspace = true\npublish.workspace = true\n')
        (d / "src" / "lib.rs").write_text(body)
        toml = wt / "Cargo.toml"
        toml.write_text(toml.read_text().replace('members = [', f'members = ["crates/{name}", ', 1))
        lock = wt / "Cargo.lock"
        lock.write_text(lock.read_text() + f'\n[[package]]\nname = "{name}"\nversion = "0.1.0"\n')

    def test_new_unused_crate_is_leaf_only(self):
        d = self.run_edit(self.new_crate)
        self.assertFalse(d["full"], d["reasons"])
        self.assertEqual(d["leaf_crates"], ["qa-leaf-crate"])

    def test_docs_alone_are_leaf_only(self):
        d = self.run_edit(lambda wt: (wt / "docs" / "qa-note.md").write_text("qa\n"))
        self.assertFalse(d["full"], d["reasons"])

    def test_crate_named_by_a_workflow_runs_everything(self):
        def edit(wt):
            self.new_crate(wt)
            f = wt / ".github" / "workflows" / "ci.yml"
            f.write_text(f.read_text() + "# cargo test -p qa-leaf-crate\n")
        self.assertTrue(self.run_edit(edit)["full"])

    def test_crate_named_outside_it_runs_everything(self):
        # The base already has a tool naming the crate (as a path or by package name); the pull
        # request then only changes the crate. A job may run that tool, so everything runs.
        for ref in ("crates/qa-leaf-crate", '"crates" / "qa-leaf-crate"', "cargo run -p qa_leaf_crate"):
            with self.subTest(ref=ref):
                def base(wt, ref=ref):
                    (wt / "tools" / "qa-ref.sh").write_text(ref + "\n")
                d = self.run_edit(self.new_crate, base_edit=base)
                self.assertTrue(d["full"])
                self.assertTrue(any("named outside its crate in tools/qa-ref.sh" in r for r in d["reasons"]),
                                d["reasons"])

    def test_existing_crate_named_by_a_tool(self):
        hits = changed_areas.mentions(REPO, "crates/docx-text", {"docx-text", "docx_text"})
        self.assertIn("tools/gen-docx-text.py", hits)

    def test_crate_with_a_dependent_runs_everything(self):
        def edit(wt):
            f = wt / "crates" / "js-json" / "src" / "lib.rs"
            f.write_text(f.read_text() + "\n// qa\n")
        d = self.run_edit(edit)
        self.assertTrue(d["full"])
        self.assertTrue(any("depended on by" in r for r in d["reasons"]), d["reasons"])

    def test_changed_third_party_lock_entry_runs_everything(self):
        def edit(wt):
            self.new_crate(wt)
            lock = wt / "Cargo.lock"
            text = lock.read_text()
            i = text.index('source = "registry+')
            j = text.index("checksum = ", i)
            lock.write_text(text[:j] + 'checksum = "00' + text[j + len('checksum = "') + 2:])
        d = self.run_edit(edit)
        self.assertTrue(d["full"])
        self.assertTrue(any(r.startswith("Cargo.lock:") for r in d["reasons"]), d["reasons"])

    def test_replaced_lock_version_runs_everything(self):
        # A dependent written as plain `name` reads the same text either side of the bump.
        def edit(wt):
            self.new_crate(wt)
            lock = wt / "Cargo.lock"
            text = lock.read_text()
            i = text.index('name = "zeroize"\nversion = "')
            j = text.index("\n", i + len('name = "zeroize"\n'))
            lock.write_text(text[:i] + 'name = "zeroize"\nversion = "999.0.0"' + text[j:])
        d = self.run_edit(edit)
        self.assertTrue(d["full"])
        self.assertTrue(any("zeroize" in r for r in d["reasons"]), d["reasons"])

    def test_profile_change_runs_everything(self):
        def edit(wt):
            self.new_crate(wt)
            toml = wt / "Cargo.toml"
            toml.write_text(toml.read_text() + "\n[profile.dev.package.qa-leaf-crate]\nopt-level = 1\n")
        self.assertTrue(self.run_edit(edit)["full"])

    def test_toolchain_and_paths_outside_crates_run_everything(self):
        for path in ("rust-toolchain.toml", ".gitignore", "bins/noevia-server/qa.txt",
                     "contracts/qa.txt", "tools/qa.txt", ".github/qa.txt"):
            with self.subTest(path=path):
                def edit(wt, path=path):
                    f = wt / path
                    f.write_text((f.read_text() if f.exists() else "") + "# qa\n")
                self.assertTrue(self.run_edit(edit)["full"])

    def test_errors_run_everything(self):
        r = subprocess.run([sys.executable, str(HERE / "changed_areas.py"), "--base", "no-such-rev"],
                           cwd=REPO, capture_output=True, text=True, check=True)
        self.assertTrue(json.loads(r.stdout)["full"])


if __name__ == "__main__":
    unittest.main()
