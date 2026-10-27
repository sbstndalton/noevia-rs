"""Run explicitly with --source pointing at the frozen services source tree."""
import argparse
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import generate_contract_fixtures as generator
import oracle_inputs as inputs

SOURCE = None
HERE = Path(__file__).resolve().parent
NAMES = ('read-contract.json', 'mutation-contract.json', 'unicode15-list.json')


class SourceFixtureGeneration(unittest.TestCase):
    def test_all_expected_bytes_come_from_source(self):
        outputs = generator.generate(SOURCE)
        self.assertEqual(set(outputs), set(NAMES))
        for name, data in outputs.items():
            self.assertEqual(data, (HERE / 'fixtures' / name).read_bytes(), name)

    def test_wrong_source_refuses_without_touching_outputs(self):
        from oracle_source import PINS
        with tempfile.TemporaryDirectory(prefix='workspace-oracle-control-') as tmp:
            root = Path(tmp)
            source = root / 'source'
            agent = source / 'diary/agent'
            agent.mkdir(parents=True)
            for name in PINS:
                shutil.copyfile(SOURCE / 'diary/agent' / name, agent / name)
            with (agent / 'app.py').open('ab') as handle:
                handle.write(b'\n# deliberate checksum negative\n')
            import oracle_source
            with mock.patch.object(oracle_source, 'compile_nodes') as compile_source:
                with self.assertRaisesRegex(ValueError, 'frozen source hash mismatch: app.py'):
                    oracle_source.load(source, inputs.NOW)
                compile_source.assert_not_called()
            out = root / 'outputs'
            out.mkdir()
            before = {name: b'unchanged output sentinel\n' for name in NAMES}
            for name, data in before.items():
                (out / name).write_bytes(data)
            run = subprocess.run([sys.executable, str(HERE / 'generate_contract_fixtures.py'),
                                  '--source', str(source), '--output-dir', str(out)],
                                 capture_output=True, text=True)
            self.assertNotEqual(run.returncode, 0)
            self.assertIn('frozen source hash mismatch: app.py', run.stderr)
            self.assertEqual({name: (out / name).read_bytes() for name in NAMES}, before)
            self.assertFalse(list(root.rglob('*.db')))

    def test_check_mode_does_not_repair_corrupted_golden_files(self):
        with tempfile.TemporaryDirectory(prefix='workspace-oracle-check-') as tmp:
            out = Path(tmp)
            for name in NAMES:
                (out / name).write_bytes(b'deliberately wrong golden\n')
            run = subprocess.run([sys.executable, str(HERE / 'generate_contract_fixtures.py'),
                                  '--source', str(SOURCE), '--output-dir', str(out), '--check'],
                                 capture_output=True, text=True)
            self.assertNotEqual(run.returncode, 0)
            self.assertIn('fixture mismatch:', run.stderr)
            for name in NAMES:
                self.assertEqual((out / name).read_bytes(), b'deliberately wrong golden\n')

    def test_interpreter_and_unicode_profiles_fail_closed(self):
        with mock.patch.object(generator.sys, 'version_info', (3, 12, 14)):
            with self.assertRaisesRegex(ValueError, 'requires CPython'):
                generator.validate_runtime()
        with mock.patch.object(generator.unicodedata, 'unidata_version', '16.0.0'):
            with self.assertRaisesRegex(ValueError, 'requires CPython'):
                generator.validate_runtime()

        with mock.patch.object(generator.sys, 'get_int_max_str_digits', return_value=0):
            with self.assertRaisesRegex(ValueError, 'requires CPython'):
                generator.validate_runtime()

    def test_unexpected_actual_operation_failure_is_not_an_expected_409(self):
        from oracle_source import load
        base, ops, trash, http_error = load(SOURCE, inputs.NOW)
        with tempfile.TemporaryDirectory(prefix='workspace-oracle-abort-') as tmp:
            directory = Path(tmp)
            case = {'name': 'intentional-overflow-control', 'op': 'copy',
                    'path': 'Notes/one.md', 'destination': 'Dest/copied.md',
                    'generation': '9' * 4300}
            with self.assertRaisesRegex(RuntimeError, 'unexpected non-Trash oracle ValueError') as error:
                generator.mutation_case(base, ops, trash, http_error, directory, 'Diary', case)
            self.assertIsInstance(error.exception.__cause__, ValueError)
            with sqlite3.connect(directory / 'synthetic/managed-diary.db') as db:
                self.assertEqual(db.execute("SELECT value FROM meta WHERE key='generation'").fetchone()[0],
                                 '9' * 4300)
                self.assertEqual(db.execute('SELECT count(*) FROM files').fetchone()[0], len(inputs.FILES))
                self.assertIsNone(db.execute("SELECT path FROM files WHERE path='Diary/Dest/copied.md'").fetchone())
                self.assertEqual(db.execute('SELECT count(*) FROM workspace_index_outbox').fetchone()[0], 0)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    SOURCE = parser.parse_args().source
    generator.validate_runtime()
    unittest.main(argv=[sys.argv[0]], verbosity=2)
