"""Portable refusal/patch tests. Never compiles Node or starts Docker."""
import difflib
import hashlib
import importlib.util
import json
from pathlib import Path
import tarfile
import tempfile
import subprocess
import sys
import os
import copy
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('runtime_recipe', Path(__file__).with_name('runtime.py'))
runtime = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runtime)


class RecipeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='node-recipe-refusal-')
        self.root = Path(self.temporary.name)
        self.recipe = self.root / 'recipe'
        self.recipe.mkdir()
        self.source = self.root / 'source'
        self.source.mkdir()
        self.rows, delta = [], []
        for name in ('deps/uv/src/unix/stream.c', 'deps/uv/src/uv-common.h'):
            before = 'first\nUV_HANDLE_READ_PARTIAL\nlast\n'
            after = 'first\nlast\n'
            target = self.source / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(before)
            self.rows.append({'path': name,
                              'beforeSha256': hashlib.sha256(before.encode()).hexdigest(),
                              'afterSha256': hashlib.sha256(after.encode()).hexdigest()})
            delta.extend(difflib.unified_diff(before.splitlines(True), after.splitlines(True),
                                             fromfile='a/' + name, tofile='b/' + name))
        self.patch = self.recipe / 'fix.patch'
        self.patch.write_text(''.join(delta))
        self.data = {'files': self.rows, 'patchFile': self.patch.name}

    def tearDown(self):
        self.temporary.cleanup()

    def apply(self):
        with patch.object(runtime, 'RECIPE', self.recipe):
            runtime.patch_source(self.source, self.data)

    def assert_original(self):
        for row in self.rows:
            self.assertEqual(runtime.sha(self.source / row['path']), row['beforeSha256'])

    def test_exact_patch(self):
        self.apply()
        for row in self.rows:
            self.assertEqual(runtime.sha(self.source / row['path']), row['afterSha256'])

    def test_changed_preimage_refused(self):
        target = self.source / self.rows[0]['path']
        target.write_text('changed\n')
        with self.assertRaises(AssertionError):
            self.apply()
        self.assertEqual(runtime.sha(self.source / self.rows[1]['path']), self.rows[1]['beforeSha256'])

    def test_bad_second_postimage_never_writes_first(self):
        self.rows[1]['afterSha256'] = '0' * 64
        with self.assertRaises(AssertionError):
            self.apply()
        self.assert_original()

    def test_offset_not_searched(self):
        self.patch.write_text(self.patch.read_text().replace('@@ -1,3 +1,2 @@', '@@ -2,3 +2,2 @@', 1))
        with self.assertRaises(AssertionError):
            self.apply()
        self.assert_original()

    def test_changed_context_refused(self):
        self.patch.write_text(self.patch.read_text().replace(' first\n', ' other\n', 1))
        with self.assertRaises(AssertionError):
            self.apply()
        self.assert_original()

    def test_extra_file_refused(self):
        with self.patch.open('a') as stream:
            stream.write('--- a/extra\n+++ b/extra\n@@ -1,1 +1,1 @@\n-a\n+b\n')
        with self.assertRaises(AssertionError):
            self.apply()
        self.assert_original()

    def test_actual_lock(self):
        data = runtime.locked()
        self.assertEqual(data['nodeVersion'], '22.23.4')
        self.assertEqual(len(data['files']), 2)

    def test_manifest_config_binding_mismatch_refused(self):
        data = runtime.locked()
        for name in ('libuv-partial-hup.patch', 'builder-manifest.json'):
            (self.recipe / name).write_bytes((runtime.RECIPE / name).read_bytes())
        data['builder']['configDigest'] = 'sha256:' + 'f' * 64
        (self.recipe / 'source-lock.json').write_text(json.dumps(data))
        with patch.object(runtime, 'RECIPE', self.recipe):
            with self.assertRaises(AssertionError):
                runtime.locked()

    def test_runtime_parity_has_no_behavior_allowlist(self):
        runtime.parity({'versions': {'icu': '77'}, 'feature': False},
                       {'feature': False, 'versions': {'icu': '77'}})
        for changed in ({'versions': {'icu': '78'}, 'feature': False},
                        {'versions': {'icu': '77'}, 'feature': 0}):
            with self.assertRaises(AssertionError):
                runtime.parity({'versions': {'icu': '77'}, 'feature': False}, changed)

    def test_only_node_gypi_pseudobooleans_converted(self):
        self.assertEqual(runtime.process_config({'x': ['false', 'true', 'other', 7]}),
                         {'x': [False, True, 'other', 7]})

    def test_builder_identity_is_actual_locked_platform(self):
        data = {'builder': {'configDigest': 'sha256:' + 'a' * 64,
                            'linuxAmd64Manifest': 'sha256:' + 'b' * 64}}
        identity = {'Architecture': 'amd64', 'Os': 'linux', 'Id': data['builder']['configDigest'],
                    'RepoDigests': ['node@' + data['builder']['linuxAmd64Manifest']]}
        path = self.root / 'builder.json'
        path.write_text(json.dumps([identity]))
        self.assertEqual(runtime.builder_identity(path, data), identity['Id'])
        containerd = dict(identity, Id=data['builder']['linuxAmd64Manifest'])
        path.write_text(json.dumps([containerd]))
        self.assertEqual(runtime.builder_identity(path, data), containerd['Id'])
        for key, value in [('Architecture', 'arm64'), ('Id', 'sha256:' + 'c' * 64),
                           ('RepoDigests', ['node@sha256:' + 'd' * 64])]:
            wrong = dict(identity)
            wrong[key] = value
            path.write_text(json.dumps([wrong]))
            with self.assertRaises(AssertionError):
                runtime.builder_identity(path, data)

    def test_archive_hash_refused_before_extraction(self):
        archive = self.root / 'archive.tar.xz'
        archive.write_bytes(b'wrong')
        destination = self.root / 'new'
        with self.assertRaises(AssertionError):
            runtime.extract(archive, destination, {'sourceSha256': '0' * 64})
        self.assertFalse(destination.exists())

    def test_archive_link_refused(self):
        archive = self.root / 'archive.tar.xz'
        with tarfile.open(archive, 'w:xz') as stream:
            item = tarfile.TarInfo('node-v22.23.4/escape')
            item.type = tarfile.SYMTYPE
            item.linkname = '/outside'
            stream.addfile(item)
        data = {'sourceSha256': runtime.sha(archive), 'nodeVersion': '22.23.4'}
        with self.assertRaises(AssertionError):
            runtime.extract(archive, self.root / 'new', data)

    def test_mac_compilation_refused_before_work_creation(self):
        with patch.object(runtime.platform, 'system', return_value='Darwin'):
            with self.assertRaises(AssertionError):
                runtime.build(self.root / 'archive', self.root / 'work', self.root / 'output')
        self.assertFalse((self.root / 'work').exists())

    def test_rejected_builder_inspection_is_preserved_before_build(self):
        data = runtime.locked()
        identity = [{'Architecture': 'amd64', 'Os': 'linux', 'Id': 'sha256:' + 'f' * 64,
                     'RepoDigests': ['node@' + data['builder']['linuxAmd64Manifest']]}]
        raw = json.dumps(identity)
        (self.root / 'builder-inspect.json').write_text(raw)
        environment = {'NOEVIA_NODE_BUILDER': data['builder']['linuxAmd64Manifest'],
                       'LC_ALL': 'C.UTF-8', 'CC': '/usr/bin/gcc', 'CXX': '/usr/bin/g++'}
        with patch.object(runtime.platform, 'system', return_value='Linux'), \
                patch.object(runtime.platform, 'machine', return_value='x86_64'), \
                patch.dict(os.environ, environment, clear=True), \
                patch.object(runtime, 'probe') as probe:
            with self.assertRaises(AssertionError):
                runtime.build(self.root / 'archive', self.root / 'work', self.root / 'output')
            probe.assert_not_called()
        self.assertEqual((self.root / 'output/builder-inspect.json').read_text(), raw)
        self.assertFalse((self.root / 'output/receipt.json').exists())
        self.assertFalse((self.root / 'work').exists())

    def test_optimized_python_refuses_all_operations(self):
        result = subprocess.run([sys.executable, '-O', str(Path(runtime.__file__)),
                                 'verify', str(self.root)], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('assertions disabled', result.stderr)

    def test_cache_key_tracks_recipe_inputs_not_test_copy(self):
        for name in ('builder-manifest.json', 'ci.sh', 'gas-metadata-evidence.json', 'libuv-partial-hup.patch',
                     'runtime.py', 'source-lock.json'):
            (self.recipe / name).write_text(name)
        with patch.object(runtime, 'RECIPE', self.recipe):
            first = runtime.recipe_sha()
            (self.recipe / 'README.md').write_text('copy change')
            (self.recipe / 'test_runtime.py').write_text('test change')
            self.assertEqual(first, runtime.recipe_sha())
            (self.recipe / 'ci.sh').write_text('changed builder environment')
            self.assertNotEqual(first, runtime.recipe_sha())

    def test_corrupt_cached_binary_refused_without_execution(self):
        data = runtime.locked()
        output = self.root / 'cache'
        output.mkdir()
        for name in ('node', 'LICENSE.node', 'LICENSE.libuv', 'configured.json',
                     'configuration-comparison.json'):
            (output / name).write_text('owned corrupt candidate')
        (output / 'node').chmod(0o555)
        identity = [{'Architecture': 'amd64', 'Os': 'linux',
                     'Id': data['builder']['configDigest'],
                     'RepoDigests': ['node@' + data['builder']['linuxAmd64Manifest']]}]
        (output / 'builder-inspect.json').write_text(json.dumps(identity))
        receipt = {'schema': 2, 'source': data, 'recipeSha256': runtime.recipe_sha(),
                   'reference': {'versions': {'node': data['nodeVersion']},
                                 'platform': 'linux', 'arch': 'x64'},
                   'builderInspectSha256': runtime.sha(output / 'builder-inspect.json'),
                   'builderImageId': data['builder']['configDigest'], 'binarySha256': '0' * 64}
        (output / 'receipt.json').write_text(json.dumps(receipt))
        with patch.object(runtime, 'probe') as probe:
            with self.assertRaises(AssertionError):
                runtime.verify(output)
            probe.assert_not_called()

    def test_only_exact_directional_discovery_pair_is_accepted(self):
        evidence = runtime.read_evidence(runtime.locked())
        reference = {'variables': {'gas_version': '2.35', 'icu_small': False},
                     'target_defaults': {'libraries': []}}
        candidate = copy.deepcopy(reference)
        candidate['variables']['gas_version'] = '2.40'
        report = runtime.configuration_comparison(reference, candidate, evidence)
        self.assertFalse(report['rawConfigurationEqual'])
        self.assertEqual(reference['variables']['gas_version'], '2.35')
        self.assertEqual(candidate['variables']['gas_version'], '2.40')
        for left, right in [('2.35', '2.35'), ('2.40', '2.35'), ('2.35', '2.41'),
                            ('2.34', '2.40')]:
            a, b = copy.deepcopy(reference), copy.deepcopy(candidate)
            a['variables']['gas_version'], b['variables']['gas_version'] = left, right
            with self.assertRaises(AssertionError):
                runtime.configuration_comparison(a, b, evidence)
        candidate['variables']['icu_small'] = 0
        with self.assertRaises(AssertionError):
            runtime.configuration_comparison(reference, candidate, evidence)

    def test_missing_or_changed_source_evidence_refused(self):
        data = runtime.locked()
        with patch.object(runtime, 'RECIPE', self.recipe):
            with self.assertRaises(OSError):
                runtime.read_evidence(data)
            (self.recipe / 'gas-metadata-evidence.json').write_text('{}')
            with self.assertRaises(AssertionError):
                runtime.read_evidence(data)

    def test_post_compile_must_equal_actual_config_and_all_other_probe_fields(self):
        evidence = runtime.read_evidence(runtime.locked())
        reference = {'config': {'variables': {'gas_version': '2.35'}},
                     'features': {'tls': True}, 'versions': {'icu': '77'},
                     'release': {'sourceUrl': 'official'}, 'cryptoCapabilities': ['sha256']}
        candidate = copy.deepcopy(reference)
        candidate['config']['variables']['gas_version'] = '2.40'
        configured = copy.deepcopy(candidate['config'])
        runtime.runtime_comparison(reference, candidate, configured, evidence)
        for key, value in [('features', {'tls': False}), ('versions', {'icu': '78'}),
                           ('release', {'sourceUrl': 'wrong'}), ('cryptoCapabilities', []),
                           ('config', {'variables': {'gas_version': '2.35'}})]:
            wrong = copy.deepcopy(candidate)
            wrong[key] = value
            with self.assertRaises(AssertionError):
                runtime.runtime_comparison(reference, wrong, configured, evidence)

    def test_changed_source_predicate_or_divergent_decisions_refused(self):
        path = self.source / 'configure.py'
        path.write_text('gas_version >= 2.23\n')
        evidence = {'referenceGasVersion': '2.35', 'builderGasVersion': '2.40',
                    'gasFieldReferences': [{'path': 'configure.py', 'sha256': runtime.sha(path)}],
                    'supportingSources': [], 'cryptoTests': [], 'predicates': [
                        {'path': 'configure.py', 'line': 1, 'literal': path.read_text().strip(),
                         'operator': '>=', 'threshold': '2.23'}]}
        self.assertEqual(len(runtime.source_evidence(self.source, evidence)), 1)
        path.write_text('gas_version >= 2.36\n')
        with self.assertRaises(AssertionError):
            runtime.source_evidence(self.source, evidence)
        evidence['gasFieldReferences'][0]['sha256'] = runtime.sha(path)
        evidence['predicates'][0].update(literal=path.read_text().strip(), threshold='2.36')
        with self.assertRaises(AssertionError):
            runtime.source_evidence(self.source, evidence)

    def test_skipped_crypto_script_never_earns_pass(self):
        script = self.source / 'vector.js'
        script.write_text('// fixed upstream vector\n')
        evidence = {'cryptoTests': [{'label': 'hash', 'path': 'vector.js',
                                    'sha256': runtime.sha(script)}]}
        reference = self.root / 'reference-node'
        candidate = self.root / 'candidate-node'
        reference.write_text('mock reference; never executed')
        candidate.write_text('mock candidate; never executed')

        def skipped(_command, _cwd, log, _timeout):
            log.write_text('1..0 # Skipped: missing crypto\n')

        with patch.object(runtime, 'run', side_effect=skipped):
            with self.assertRaises(AssertionError):
                runtime.crypto_checks(self.source, self.root, reference, candidate, evidence)


if __name__ == '__main__':
    unittest.main()
