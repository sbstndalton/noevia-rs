"""Tests for routes_check.py: python3 -m unittest tools/contracts/test_routes_check.py"""
import pathlib
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import routes_check  # noqa: E402

SCRIPT = pathlib.Path(__file__).parent / "routes_check.py"

CLIENT = """
// '/api/commented/out' in a comment is not a call
export const a = () => getJson('/api/profile');
export const b = (id: string) => apiFetch(`/api/projects/${encodeURIComponent(id)}/files?x=1`);
export const c = (p: string) => getJson(`/api/integrations/storage/files${p ? `/${p}` : ''}`);
const msg = 'PUT /api/integrations/storage failed';
"""


class RoutesCheck(unittest.TestCase):
    def tree(self, routes):
        d = pathlib.Path(tempfile.mkdtemp())
        (d / "src").mkdir()
        (d / "src" / "api.ts").write_text(CLIENT)
        (d / "src" / "api.test.ts").write_text("getJson('/api/test/only')")
        (d / "routes.toml").write_text("".join(f'[[route]]\npath = "{p}"\nowner = "node"\n\n' for p in routes))
        return d

    def run_check(self, d):
        return subprocess.run([sys.executable, str(SCRIPT), "--web", str(d), "--routes", str(d / "routes.toml")], capture_output=True, text=True)

    def test_extracts_client_paths(self):
        d = self.tree([])
        self.assertEqual(sorted(routes_check.client_paths(d)), ["/api/integrations/storage/files{*?}", "/api/profile", "/api/projects/{id}/files"])

    def test_passes_when_every_path_is_listed_under_any_param_name(self):
        d = self.tree(["/api/profile", "/api/projects/{projectId}/files", "/api/integrations/storage/files{*?}"])
        r = self.run_check(d)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)

    def test_fails_when_a_path_is_missing(self):
        d = self.tree(["/api/profile"])
        r = self.run_check(d)
        self.assertEqual(r.returncode, 1)
        self.assertIn("/api/projects/{id}/files", r.stdout)

    def test_refuses_an_entry_without_an_owner(self):
        d = self.tree([])
        (d / "routes.toml").write_text('[[route]]\npath = "/api/profile"\n')
        self.assertEqual(self.run_check(d).returncode, 1)


if __name__ == "__main__":
    unittest.main()
