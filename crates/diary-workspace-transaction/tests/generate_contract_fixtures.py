#!/usr/bin/env python3
"""Regenerate legacy contracts from pinned Python source, never from Rust output.

Expected results are computed independently using actual AST-selected operation
functions and the actual managed SQLite backend. Legacy module/app startup, HTTP,
auth, journal workers, indexing, model calls and real data are never run.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import locale
import os
import sqlite3
import sys
import tempfile
import types
import unicodedata
import uuid
from pathlib import Path

import oracle_inputs as inputs


def validate_runtime():
    if (sys.implementation.name != 'cpython' or sys.version_info[:3] != (3, 12, 15)
            or unicodedata.unidata_version != '15.0.0'
            or sys.get_int_max_str_digits() != 4300):
        raise ValueError('requires CPython3.12.15 / Unicode15.0.0 / int_max_str_digits=4300')


def store_fixture(base, directory, root, files, directories, generation='0'):
    backend = base['ManagedCorpusBackend'](directory / 'synthetic', inputs.TENANT)
    store = base['CorpusStore']({
        'corpus.root': root, 'corpus.entries_prefix': 'Entries',
        'corpus.monthly_prefix': 'Monthly', 'corpus.index_file': 'index.md',
    }, backend, None)
    with backend.db() as db:
        for key, value in [('active', '1'), ('generation', generation), ('due', '0'),
                           ('snapshot_time', '0'),
                           ('workspace_operation_namespace', inputs.NAMESPACE)]:
            backend.set_meta(db, key, value)
        for path, data, updated in files:
            db.execute('INSERT INTO files VALUES(?,?,?,?)',
                       (store._join(path), data, base['digest'](data), updated))
        for path in directories:
            db.execute('INSERT INTO directories VALUES(?)', (store._join(path),))
    return backend, store


def reads(base, http_error, pins):
    cases = []
    for root in ['Diary', '', 'Tenants/Corpus']:
        with tempfile.TemporaryDirectory(prefix='workspace-read-source-') as tmp:
            _, store = store_fixture(base, Path(tmp), root, inputs.READ_FILES, inputs.READ_DIRS)
            for op, path in inputs.READ_CASES:
                try:
                    body = base['stat'](store, path) if op == 'stat' else base['file_list'](store, path)
                    expected = {'status': 200, 'body': body}
                except (base['OpError'], http_error) as exc:
                    expected = {'status': getattr(exc, 'status', getattr(exc, 'status_code', None))}
                cases.append({'root': root, 'op': op, 'path': path, 'expected': expected})
    return {
        'sourcePins': pins, 'interpreter': sys.version.split()[0],
        'files': [{'path': p, 'data': data.decode(), 'updated': updated}
                  for p, data, updated in inputs.READ_FILES],
        'directories': inputs.READ_DIRS, 'cases': cases,
    }


def snapshot(path):
    with sqlite3.connect(path) as db:
        files = [{'path': p, 'bytes': len(data),
                  'dataSHA256': hashlib.sha256(data).hexdigest(), 'version': version, 'updated': updated}
                 for p, data, version, updated in db.execute('SELECT * FROM files ORDER BY path')]
        return {
            'files': files,
            'directories': [r[0] for r in db.execute('SELECT path FROM directories ORDER BY path')],
            'meta': dict(db.execute('SELECT * FROM meta ORDER BY key')),
            'outbox': [r[0] for r in db.execute('SELECT path FROM workspace_index_outbox ORDER BY path')],
            'imports': db.execute('SELECT * FROM workspace_imports ORDER BY 1').fetchall(),
            'backups': db.execute('SELECT * FROM backups ORDER BY 1').fetchall(),
        }


def derived_fixture_uuid(operation, relative_path):
    # Explicit deterministic UUID input provider for comparison. This matches the
    # separately specified DWT1 identity contract, not Python's random UUID
    # algorithm; it neither implements nor replaces any operation behavior.
    raw = bytearray(hashlib.sha256(operation.encode() + b'\0' + relative_path.encode()).digest()[:16])
    raw[6] = (raw[6] & 15) | 80
    raw[8] = (raw[8] & 63) | 128
    return str(uuid.UUID(bytes=bytes(raw)))


def mutation_case(base, ops, trash, http_error, directory, root, case):
    files = [(p, b'x' * (512 * 1024 + 1) if case.get('profile') == 'large'
              and p == 'Notes/one.md' else data, 1.0) for p, data in inputs.FILES]
    backend, store = store_fixture(base, directory, root, files, inputs.DIRS,
                                   case.get('generation', '0'))
    seed_capsule = None
    with backend.db() as db:
        if case.get('profile') == 'nonmarkdown':
            db.execute('INSERT INTO files VALUES(?,?,?,1.0)',
                       (store._join('Notes/Folder/opaque.bin'), b'opaque', base['digest'](b'opaque')))
        if case.get('profile') == 'wide':
            for i in range(501):
                db.execute('INSERT INTO files VALUES(?,?,?,1.0)',
                           (store._join(f'Wide/{i}.md'), b'x', base['digest'](b'x')))
        if case.get('prepare'):
            state = 'restored' if case['prepare'] == 'restored' else 'trashed'
            data = b'synthetic note'
            record = {'format': 'noevia-trash-v1', 'id': inputs.CAP, 'path': 'Notes/one.md',
                      'version': base['digest'](data), 'data': base64.b64encode(data).decode(),
                      'state': state, 'trashedAt': inputs.NOW - 10}
            if state == 'restored':
                record['restoredAt'] = inputs.NOW - 1
            encoded = json.dumps(record, sort_keys=True, ensure_ascii=False).encode()
            seed_capsule = encoded.decode()
            db.execute('INSERT INTO files VALUES(?,?,?,?)',
                       ('.noevia-trash/' + inputs.CAP + '.json', encoded,
                        base['digest'](encoded), inputs.NOW - 10))
            if case['prepare'] == 'trashed':
                db.execute('DELETE FROM files WHERE path=?', (store._join('Notes/one.md'),))
    body = {k: v for k, v in case.items()
            if k in ('op', 'path', 'destination', 'version', 'destinationVersion', 'overwrite')}
    for field, pathkey in [('version', 'path'), ('destinationVersion', 'destination')]:
        if body.get(field) in ('$source', '$destination'):
            body[field] = ops.stat(store, body[pathkey])['version']
        elif body.get(field) == '$original':
            body[field] = base['digest'](b'synthetic note')
    if case['op'] == 'delete':
        with backend.db() as db:
            files, _ = ops._tree_meta(db, store._join(body['path']))
        ids = [derived_fixture_uuid(inputs.OP, p[len(root) + 1:] if root else p)
               for p in sorted(files)]
    else:
        ids = [inputs.CAP]
    iterator = iter(ids)
    fixture_uuid = types.SimpleNamespace(UUID=uuid.UUID, uuid4=lambda: uuid.UUID(next(iterator)))
    ops.uuid = fixture_uuid
    trash.uuid = fixture_uuid
    try:
        if case['op'] in ('trash', 'restore'):
            result = trash.change(store, {**body, 'action': case['op'], 'id': inputs.CAP})
        elif case['op'] == 'trashList':
            result = trash.list_trash(store)
        else:
            result = ops.operate(store, body)
        expected = {'status': 200, 'body': result}
    except ops.OpError as exc:
        expected = {'status': exc.status, 'message': str(exc)}
    except http_error as exc:
        expected = {'status': exc.status_code, 'message': str(exc.detail)}
    except ValueError as exc:
        if case['op'] not in ('trash', 'restore', 'trashList'):
            raise RuntimeError('unexpected non-Trash oracle ValueError') from exc
        expected = {'status': 409, 'message': str(exc)}
    state = json.dumps(snapshot(backend.db_path), sort_keys=True, ensure_ascii=False,
                       separators=(',', ':'), allow_nan=False).encode()
    return {'name': case['name'], 'root': root, 'input': case, 'sourceBody': body,
            'seedCapsule': seed_capsule, 'expected': expected,
            'expectedStateSHA256': hashlib.sha256(state).hexdigest()}


def mutations(base, ops, trash, http_error, pins):
    cases = []
    for root in ['Diary', '', 'Café/記録']:
        for case in inputs.CASES + inputs.EXTRA_CASES:
            with tempfile.TemporaryDirectory(prefix='workspace-mutation-source-') as tmp:
                cases.append(mutation_case(base, ops, trash, http_error, Path(tmp), root, case))
    return {
        'sourcePins': pins, 'interpreter': sys.version.split()[0],
        'files': [{'path': p, 'data': data.decode()} for p, data in inputs.FILES],
        'directories': inputs.DIRS, 'now': inputs.NOW, 'capsuleId': inputs.CAP,
        'operationId': inputs.OP, 'cases': cases,
    }


def unicode_lists(base, pins):
    cases = []
    for root in ['Diary', '', 'Café/記録']:
        for name, names in inputs.UNICODE_CASES:
            with tempfile.TemporaryDirectory(prefix='workspace-unicode-source-') as tmp:
                _, store = store_fixture(base, Path(tmp), root,
                                         [('Notes/' + n, b'synthetic', 1.0) for n in names], ['Notes'])
                cases.append({'name': name, 'root': root, 'files': names,
                              'expected': base['file_list'](store, 'Notes')})
    return {'python': sys.version.split()[0], 'unicode': unicodedata.unidata_version,
            'pins': pins, 'cases': cases}


def generate(source):
    validate_runtime()
    previous_locale = locale.setlocale(locale.LC_TIME)
    try:
        locale.setlocale(locale.LC_TIME, 'C')  # explicit synthetic locale input
        from oracle_source import load, PINS, READ_PINS
        base, ops, trash, http_error = load(source, inputs.NOW)
        values = {
            'read-contract.json': reads(base, http_error, READ_PINS),
            'mutation-contract.json': mutations(base, ops, trash, http_error, PINS),
            'unicode15-list.json': unicode_lists(base, READ_PINS),
        }
        return {name: (json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + '\n').encode()
                for name, value in values.items()}
    finally:
        locale.setlocale(locale.LC_TIME, previous_locale)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True, help='frozen noevia-services source directory')
    parser.add_argument('--output-dir', type=Path, default=Path(__file__).parent / 'fixtures')
    parser.add_argument('--check', action='store_true', help='compare bytes without writing files')
    args = parser.parse_args()
    # Expected results never use checked-in fixtures, a Rust binary or a native
    # response. Only this final comparison step reads checked-in expected bytes.
    outputs = generate(args.source)
    if args.check:
        different = [name for name, data in outputs.items()
                     if not (args.output_dir / name).is_file()
                     or (args.output_dir / name).read_bytes() != data]
        if different:
            raise SystemExit('fixture mismatch: ' + ', '.join(different))
    else:
        args.output_dir.mkdir(parents=True, exist_ok=True)
        for name, data in outputs.items():
            temporary = None
            try:
                with tempfile.NamedTemporaryFile(dir=args.output_dir, prefix='.oracle-', delete=False) as handle:
                    temporary = Path(handle.name)
                    handle.write(data)
                os.replace(temporary, args.output_dir / name)
            finally:
                if temporary is not None:
                    temporary.unlink(missing_ok=True)
    action = 'checked' if args.check else 'generated'
    print(f'source fixtures {action}: 24 reads, 135 mutations, 18 Unicode list cases')


if __name__ == '__main__':
    main()
