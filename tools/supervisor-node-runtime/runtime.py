#!/usr/bin/env python3
"""Finite Node22 upstream-backport recipe; no compilation on import or verify."""
import argparse
import ast
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import signal
import stat
import subprocess
import tarfile
import time
import urllib.request

if not __debug__:
    raise RuntimeError('Recipe verification cannot run with Python assertions disabled')

RECIPE = Path(__file__).resolve().parent
SOURCE_LIMIT = 100 * 1024 * 1024
EXPANDED_LIMIT = 1024 * 1024 * 1024
PROBE = """const crypto=require('node:crypto');console.log(JSON.stringify({
versions:process.versions,features:process.features,release:process.release,
cryptoCapabilities:{hashes:crypto.getHashes().sort(),ciphers:crypto.getCiphers().sort(),
curves:crypto.getCurves().sort(),fips:crypto.getFips()},
config:process.config,arch:process.arch,platform:process.platform}))"""


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def locked():
    data = json.loads((RECIPE / 'source-lock.json').read_text())
    assert data['schema'] == 1 and data['nodeVersion'] == '22.23.4'
    assert data['platform'] == 'linux/amd64' and data['compileJobs'] == 2
    assert data['configureArgs'] == ['--with-intl=full-icu',
                                    '--release-urlbase=https://nodejs.org/download/release/']
    assert data['compileTimeoutSeconds'] == 7800
    assert data['sourceUrl'] == 'https://nodejs.org/dist/v22.23.4/node-v22.23.4.tar.xz'
    assert data['patchFile'] == 'libuv-partial-hup.patch'
    assert sha(RECIPE / data['patchFile']) == data['patchSha256']
    manifest_path = RECIPE / 'builder-manifest.json'
    assert 'sha256:' + sha(manifest_path) == data['builder']['linuxAmd64Manifest']
    manifest = json.loads(manifest_path.read_text())
    assert manifest['schemaVersion'] == 2
    assert manifest['config']['digest'] == data['builder']['configDigest']
    assert {f['path'] for f in data['files']} == {
        'deps/uv/src/unix/stream.c', 'deps/uv/src/uv-common.h'}
    assert len(data['files']) == 2
    for value in [data['sourceSha256'], data['patchSha256']] + [
            f[k] for f in data['files'] for k in ('beforeSha256', 'afterSha256')]:
        assert re.fullmatch('[0-9a-f]{64}', value)
    read_evidence(data)
    return data


def recipe_sha():
    digest = hashlib.sha256()
    # Match the CI exact-key cache inputs. Test/copy edits do not force another
    # C++ compilation when all actual recipe inputs remain byte-identical.
    for name in ('builder-manifest.json', 'ci.sh', 'gas-metadata-evidence.json', 'libuv-partial-hup.patch',
                 'runtime.py', 'source-lock.json'):
        path = RECIPE / name
        assert path.is_file() and not path.is_symlink()
        digest.update(name.encode() + b'\0' + bytes.fromhex(sha(path)))
    return digest.hexdigest()


def read_evidence(data):
    policy = data['configurationComparison']
    assert policy['evidenceFile'] == 'gas-metadata-evidence.json'
    path = RECIPE / policy['evidenceFile']
    assert sha(path) == policy['evidenceSha256']
    evidence = json.loads(path.read_text())
    assert evidence['schema'] == 1 and evidence['nodeSourceSha256'] == data['sourceSha256']
    assert evidence['builderManifest'] == data['builder']['linuxAmd64Manifest']
    assert policy['referenceGasVersion'] == evidence['referenceGasVersion'] == '2.35'
    assert policy['builderGasVersion'] == evidence['builderGasVersion'] == '2.40'
    assert {item['label'] for item in evidence['cryptoTests']} == {
        'hash', 'hmac', 'cipher', 'authenticated', 'sign'}
    assert len(evidence['cryptoTests']) == 5
    return evidence


def source_evidence(source, evidence):
    expected = {item['path']: item['sha256'] for item in evidence['gasFieldReferences']}
    assert len(expected) == len(evidence['gasFieldReferences'])
    actual = {}
    for path in source.rglob('*'):
        if not path.is_file():
            continue
        found, tail = False, b''
        with path.open('rb') as stream:
            while chunk := stream.read(1024 * 1024):
                block = tail + chunk
                found |= b'gas_version' in block
                tail = block[-32:]
        if found:
            actual[path.relative_to(source).as_posix()] = sha(path)
    assert actual == expected, 'Assembler field source references changed'
    for item in evidence['supportingSources'] + evidence['cryptoTests']:
        assert sha(source / item['path']) == item['sha256']
    for item in evidence['predicates']:
        lines = (source / item['path']).read_text().splitlines()
        assert lines[item['line'] - 1].strip() == item['literal']
        if 'constantLine' in item:
            assert lines[item['constantLine'] - 1].strip() == item['constantLiteral']
    return branch_outcomes(evidence)


def branch_outcomes(evidence):
    outcomes = []
    for item in evidence['predicates']:
        threshold = tuple(map(int, item['threshold'].split('.')))
        left = tuple(map(int, evidence['referenceGasVersion'].split('.')))
        right = tuple(map(int, evidence['builderGasVersion'].split('.')))
        assert item['operator'] in ('>=', '<')
        a = left >= threshold if item['operator'] == '>=' else left < threshold
        b = right >= threshold if item['operator'] == '>=' else right < threshold
        assert a == b
        outcomes.append({'path': item['path'], 'line': item['line'],
                         'referenceOutcome': a, 'builderOutcome': b})
    return outcomes


def configuration_comparison(reference, candidate, evidence):
    differences = []

    def compare(a, b, path):
        if isinstance(a, dict) and isinstance(b, dict):
            assert a.keys() == b.keys(), 'Configuration keys changed'
            for key in sorted(a):
                compare(a[key], b[key], path + '/' + key)
        elif json.dumps(a, sort_keys=True) != json.dumps(b, sort_keys=True):
            differences.append({'path': path, 'reference': a, 'candidate': b})

    compare(reference, candidate, '')
    expected = {'path': '/variables/gas_version',
                'reference': evidence['referenceGasVersion'],
                'candidate': evidence['builderGasVersion']}
    assert differences == [expected], 'Unexpected configuration drift or assembler pair'
    return {'rawConfigurationEqual': False, 'acceptedDiscoveryDifference': expected,
            'sourceEvidence': 'Pinned source predicates agree; not machine-code identity'}


def runtime_comparison(reference, candidate, configured, evidence):
    parity({key: value for key, value in reference.items() if key != 'config'},
           {key: value for key, value in candidate.items() if key != 'config'})
    # The compiled binary must expose exactly what configure actually generated,
    # including the truthful assembler version. Never rewrite either snapshot.
    parity(configured, candidate['config'])
    return configuration_comparison(reference['config'], candidate['config'], evidence)


def selected_tools():
    tools = {}
    for name in ('as', 'ld'):
        result = subprocess.run(['/usr/bin/gcc', '-print-prog-name=' + name],
                                capture_output=True, text=True, check=True, timeout=15)
        value = result.stdout.strip()
        assert len(value) < 4096
        path = Path(value) if Path(value).is_absolute() else Path(shutil.which(value))
        path = path.resolve(strict=True)
        assert path.is_relative_to('/usr')
        version = subprocess.run([str(path), '--version'], capture_output=True,
                                 text=True, check=True, timeout=15).stdout
        assert len(version) <= 64 * 1024
        tools[name] = {'path': str(path), 'sha256': sha(path), 'version': version.strip()}
    assert tools['as']['version'].splitlines()[0].startswith('GNU assembler')
    assert tools['as']['version'].splitlines()[0].endswith(' 2.40')
    return tools


def crypto_checks(source, output, reference, candidate, evidence):
    results = []
    for role, binary in (('reference', reference), ('candidate', candidate)):
        binary_sha = sha(binary)
        for item in evidence['cryptoTests']:
            assert sha(source / item['path']) == item['sha256']
            name = 'crypto-' + role + '-' + item['label'] + '.log'
            log = output / name
            run([str(binary), str(source / item['path'])], source, log, 120)
            assert not re.search(r'\b(?:skipped|skipping)\b', log.read_text(), re.I)
            results.append({'role': role, 'label': item['label'], 'binarySha256': binary_sha,
                            'sourceSha256': item['sha256'],
                            'log': name, 'logSha256': sha(log), 'status': 'passed'})
    return results


def download(destination):
    data = locked()
    # Fixed official URL, exclusive output, finite byte/time bounds; no installer.
    deadline = time.monotonic() + 120
    with urllib.request.urlopen(data['sourceUrl'], timeout=15) as response:
        assert response.url == data['sourceUrl']
        with destination.open('xb') as output:
            total = 0
            while chunk := response.read(1024 * 1024):
                total += len(chunk)
                assert total <= SOURCE_LIMIT and time.monotonic() < deadline
                output.write(chunk)
    assert sha(destination) == data['sourceSha256']


def extract(archive, destination, data):
    assert archive.is_file() and not archive.is_symlink()
    assert archive.stat().st_size <= SOURCE_LIMIT
    assert sha(archive) == data['sourceSha256']
    destination.mkdir(mode=0o700)  # Never reuse an earlier partial source tree.
    total = 0
    prefix = 'node-v' + data['nodeVersion']
    with tarfile.open(archive, 'r:xz') as source:
        for member in source:
            parts = PurePosixPath(member.name).parts
            assert parts and parts[0] == prefix and '..' not in parts
            assert not member.name.startswith('/') and len(member.name) <= 4096
            assert member.isdir() or member.isfile()  # Actual locked archive has no links/devices.
            target = destination.joinpath(*parts[1:])
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            total += member.size
            assert 0 <= member.size <= SOURCE_LIMIT and total <= EXPANDED_LIMIT
            target.parent.mkdir(parents=True, exist_ok=True)
            with source.extractfile(member) as src, target.open('xb') as dst:
                shutil.copyfileobj(src, dst, length=1024 * 1024)
            target.chmod(0o755 if member.mode & 0o111 else 0o644)


def patch_source(source, data):
    """Apply exact line positions and context. No offset/fuzz search exists."""
    lines = (RECIPE / data['patchFile']).read_text().splitlines(keepends=True)
    expected = {f['path']: f for f in data['files']}
    changed = {}
    index = 0
    while index < len(lines):
        assert lines[index].startswith('--- a/')
        name = lines[index][6:].rstrip('\n')
        assert name in expected and name not in changed
        assert lines[index + 1] == '+++ b/' + name + '\n'
        path = source / name
        assert path.is_file() and not path.is_symlink()
        assert sha(path) == expected[name]['beforeSha256']
        before = path.read_text().splitlines(keepends=True)
        output, cursor = [], 0
        index += 2
        hunks = 0
        while index < len(lines) and lines[index].startswith('@@'):
            match = re.fullmatch(r'@@ -(\d+),(\d+) \+(\d+),(\d+) @@\n', lines[index])
            assert match
            old_start, old_count, new_start, new_count = map(int, match.groups())
            assert cursor <= old_start - 1 <= len(before)
            output.extend(before[cursor:old_start - 1])
            cursor = old_start - 1
            assert len(output) == new_start - 1
            index += 1
            old_seen = new_seen = 0
            while index < len(lines) and not lines[index].startswith(('@@', '--- a/')):
                line = lines[index]
                assert line[:1] in (' ', '+', '-') and line.endswith('\n')
                if line[0] in ' -':
                    assert cursor < len(before) and before[cursor] == line[1:]
                    cursor += 1
                    old_seen += 1
                if line[0] in ' +':
                    output.append(line[1:])
                    new_seen += 1
                index += 1
            assert (old_seen, new_seen) == (old_count, new_count)
            hunks += 1
        assert hunks
        output.extend(before[cursor:])
        content = ''.join(output).encode()
        assert hashlib.sha256(content).hexdigest() == expected[name]['afterSha256']
        changed[name] = content
    assert changed.keys() == expected.keys()
    # Validate every file before writing either, so a bad second hunk cannot
    # leave a deceptively half-applied patch.
    for name, content in changed.items():
        (source / name).write_bytes(content)
    for path in (source / 'deps/uv').rglob('*'):
        if path.suffix in ('.c', '.h'):
            assert b'UV_HANDLE_READ_PARTIAL' not in path.read_bytes()


def run(command, cwd, log, timeout):
    # Only a child created by this recipe is terminated on its own deadline.
    with log.open('wb') as stream:
        child = subprocess.Popen(command, cwd=cwd, stdout=stream,
                                 stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=10)
            raise
    assert code == 0, f'{command[0]} failed; inspect {log.name}'


def probe(binary):
    environment = os.environ.copy()
    for key in ('NODE_OPTIONS', 'NODE_PATH', 'LD_PRELOAD', 'LD_LIBRARY_PATH'):
        environment.pop(key, None)
    result = subprocess.run([str(binary), '-e', PROBE], check=True,
                            capture_output=True, text=True, timeout=15, env=environment)
    assert len(result.stdout) <= 256 * 1024
    return json.loads(result.stdout)


def parity(reference, candidate):
    # No runtime-variable/feature allowlist: mismatches require investigation,
    # never golden regeneration or silent configuration normalization.
    assert json.dumps(candidate, sort_keys=True) == json.dumps(reference, sort_keys=True), \
        'Node reference/candidate configuration or version mismatch'


def process_config(value):
    # Node22 tools/js2c.cc turns exactly these pseudo-Boolean strings into
    # Booleans when embedding config.gypi; mirror that serialization, not a
    # behavioral configuration allowlist.
    if isinstance(value, dict):
        return {key: process_config(item) for key, item in value.items()}
    if isinstance(value, list):
        return [process_config(item) for item in value]
    if value == 'true':
        return True
    if value == 'false':
        return False
    return value


def builder_identity(path, data):
    rows = json.loads(path.read_text())
    assert isinstance(rows, list) and len(rows) == 1
    row = rows[0]
    assert row['Architecture'] == 'amd64' and row['Os'] == 'linux'
    # Classic Docker IDs are config digests. The containerd image store uses
    # the pulled target descriptor digest (moby daemon/containerd/image_inspect.go).
    # Only these two exact identities are allowed; locked() independently binds
    # the platform manifest's actual bytes to its config digest.
    assert row['Id'] in (data['builder']['configDigest'], data['builder']['linuxAmd64Manifest'])
    allowed = {name + '@' + data['builder']['linuxAmd64Manifest'] for name in (
        'node', 'library/node', 'docker.io/library/node')}
    assert allowed.intersection(row['RepoDigests'])
    return row['Id']


def build(archive, work, output):
    data = locked()
    assert platform.system() == 'Linux' and platform.machine() == 'x86_64'
    assert os.environ.get('NOEVIA_NODE_BUILDER') == data['builder']['linuxAmd64Manifest']
    assert os.environ.get('LC_ALL') == 'C.UTF-8'
    assert os.environ.get('CC') == '/usr/bin/gcc' and os.environ.get('CXX') == '/usr/bin/g++'
    assert not any(os.environ.get(k) for k in (
        'NODE_OPTIONS', 'NODE_PATH', 'LD_PRELOAD', 'LD_LIBRARY_PATH',
        'CFLAGS', 'CXXFLAGS', 'CPPFLAGS', 'LDFLAGS', 'PYTHONPATH'))
    identity = archive.parent / 'builder-inspect.json'
    output.mkdir(mode=0o700)
    # Preserve the actual inspection even if identity verification refuses it.
    # An output directory without a success receipt is never a verified cache.
    shutil.copy2(identity, output / 'builder-inspect.json')
    builder_id = builder_identity(identity, data)
    work.mkdir(mode=0o700)
    reference = probe('/usr/local/bin/node')
    assert reference['versions']['node'] == data['nodeVersion']
    assert reference['arch'] == 'x64' and reference['platform'] == 'linux'
    tools = {}
    for name in ('gcc', 'g++', 'make', 'python3', 'ldd', 'npm'):
        result = subprocess.run([name, '--version'], capture_output=True, text=True,
                                check=True, timeout=15)
        assert len(result.stdout) <= 64 * 1024
        tools[name] = result.stdout.strip()
    compiler_tools = selected_tools()
    source = work / 'node'
    extract(archive, source, data)
    patch_source(source, data)
    evidence = read_evidence(data)
    branch_outcomes = source_evidence(source, evidence)
    # Reference prefix is fixed by the immutable builder, not caller input.
    prefix = reference['config']['variables']['node_prefix']
    assert isinstance(prefix, str) and prefix.startswith('/') and len(prefix) < 256
    configure = ['python3', 'configure.py', *data['configureArgs'], '--prefix=' + prefix]
    (output / 'reference.json').write_text(json.dumps(reference, indent=2) + '\n')
    run(configure, source, output / 'configure.log', 300)
    generated = process_config(ast.literal_eval((source / 'config.gypi').read_text()))
    (output / 'configured.json').write_text(json.dumps(generated, indent=2) + '\n')
    comparison = configuration_comparison(reference['config'], generated, evidence)
    (output / 'configuration-comparison.json').write_text(json.dumps(comparison, indent=2) + '\n')
    run(['make', '-j2', 'node'], source, output / 'compile.log', data['compileTimeoutSeconds'])
    binary = source / 'out/Release/node'
    assert binary.is_file() and not binary.is_symlink()
    assert binary.open('rb').read(4) == b'\x7fELF'
    candidate = probe(binary)
    parity(comparison, runtime_comparison(reference, candidate, generated, evidence))
    crypto_results = crypto_checks(source, output, Path('/usr/local/bin/node'), binary, evidence)
    shutil.copy2(binary, output / 'node')
    (output / 'node').chmod(0o555)
    shutil.copy2(source / 'LICENSE', output / 'LICENSE.node')
    shutil.copy2(source / 'deps/uv/LICENSE', output / 'LICENSE.libuv')
    shutil.copy2(identity, output / 'builder-inspect.json')
    linkage = subprocess.run(['ldd', str(binary)], capture_output=True, text=True,
                             check=True, timeout=15).stdout
    assert 'not found' not in linkage and len(linkage) <= 64 * 1024
    receipt = {'schema': 2, 'recipeSha256': recipe_sha(), 'source': data,
               'builderImageId': builder_id, 'builderInspectSha256': sha(identity),
               'referenceBinarySha256': sha('/usr/local/bin/node'),
               'binarySha256': sha(output / 'node'), 'reference': reference,
               'candidate': candidate, 'configuredSha256': sha(output / 'configured.json'),
               'configurationComparison': comparison, 'sourceBranchOutcomes': branch_outcomes,
               'compilerSelectedTools': compiler_tools, 'cryptoChecks': crypto_results,
               'tools': tools, 'linkage': linkage,
               'configureCommand': configure, 'compileCommand': ['make', '-j2', 'node'],
               'qualification': 'Unqualified: mandatory TCP and both shipped image gates remain',
               'licenses': {n: sha(output / n) for n in ('LICENSE.node', 'LICENSE.libuv')}}
    (output / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')


def verify(output):
    data = locked()
    assert output.is_dir() and not output.is_symlink()
    for name in ('node', 'receipt.json', 'builder-inspect.json', 'LICENSE.node', 'LICENSE.libuv',
                 'configured.json', 'configuration-comparison.json'):
        path = output / name
        assert path.is_file() and not path.is_symlink()
    assert (output / 'receipt.json').stat().st_size <= 1024 * 1024
    receipt = json.loads((output / 'receipt.json').read_text())
    assert receipt['schema'] == 2 and receipt['source'] == data
    assert receipt['recipeSha256'] == recipe_sha()
    assert receipt['reference']['versions']['node'] == data['nodeVersion']
    assert receipt['reference']['platform'] == 'linux' and receipt['reference']['arch'] == 'x64'
    identity = output / 'builder-inspect.json'
    assert sha(identity) == receipt['builderInspectSha256']
    assert builder_identity(identity, data) == receipt['builderImageId']
    binary = output / 'node'
    assert binary.is_file() and not binary.is_symlink()
    assert stat.S_ISREG(binary.stat().st_mode) and binary.stat().st_mode & 0o111
    assert sha(binary) == receipt['binarySha256']
    for name in ('LICENSE.node', 'LICENSE.libuv'):
        assert sha(output / name) == receipt['licenses'][name]
    evidence = read_evidence(data)
    assert sha(output / 'configured.json') == receipt['configuredSha256']
    configured = json.loads((output / 'configured.json').read_text())
    comparison = runtime_comparison(receipt['reference'], receipt['candidate'], configured, evidence)
    parity(receipt['sourceBranchOutcomes'], branch_outcomes(evidence))
    parity(comparison, receipt['configurationComparison'])
    parity(comparison, json.loads((output / 'configuration-comparison.json').read_text()))
    for name in ('as', 'ld'):
        item = receipt['compilerSelectedTools'][name]
        assert Path(item['path']).is_absolute() and Path(item['path']).is_relative_to('/usr')
        assert re.fullmatch('[0-9a-f]{64}', item['sha256'])
        assert isinstance(item['version'], str) and 0 < len(item['version']) <= 64 * 1024
    assert receipt['compilerSelectedTools']['as']['version'].splitlines()[0].endswith(' 2.40')
    expected = {(role, item['label']): item for role in ('reference', 'candidate')
                for item in evidence['cryptoTests']}
    assert len(receipt['cryptoChecks']) == len(expected)
    seen = set()
    for item in receipt['cryptoChecks']:
        key = (item['role'], item['label'])
        assert key in expected and key not in seen and item['status'] == 'passed'
        seen.add(key)
        assert item['sourceSha256'] == expected[key]['sha256']
        binary_sha = (receipt['referenceBinarySha256'] if item['role'] == 'reference'
                      else receipt['binarySha256'])
        assert item['binarySha256'] == binary_sha
        assert item['log'] == 'crypto-' + item['role'] + '-' + item['label'] + '.log'
        log = output / item['log']
        assert log.is_file() and not log.is_symlink() and sha(log) == item['logSha256']
        assert not re.search(r'\b(?:skipped|skipping)\b', log.read_text(), re.I)
    parity(receipt['candidate'], probe(binary))
    print(json.dumps({'recipeSha256': recipe_sha(), 'binarySha256': sha(binary),
                      'node': receipt['candidate']['versions']['node'],
                      'qualification': 'TCP/image acceptance still required'}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=('download', 'build', 'verify'))
    parser.add_argument('paths', type=Path, nargs='+')
    args = parser.parse_args()
    assert all(path.is_absolute() for path in args.paths)
    if args.operation == 'download':
        assert len(args.paths) == 1
        download(args.paths[0])
    elif args.operation == 'build':
        assert len(args.paths) == 3
        build(*args.paths)
    else:
        assert len(args.paths) == 1
        verify(args.paths[0])


if __name__ == '__main__':
    main()
