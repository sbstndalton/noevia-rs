"""Synthetic CPython byte oracle for managed_storage.py at services 6c1eb589.

Run through devrun. This mirrors the production manifest expression and its
json.dumps(sort_keys=True, ensure_ascii=False).encode(), never touches a corpus.
"""
import hashlib
import json
from pathlib import Path
import random
import struct
import sys
import uuid

TENANT = '12345678-1234-1234-1234-123456789abc'

def float_bits(bits):
    return struct.unpack('>d', bits.to_bytes(8, 'big'))[0]


def main():
    rng = random.Random(1071)
    bits = {0x42ec8b01f8b6d744, 0, 1, 2, (1 << 63), (1 << 63) + 1, 0x0010000000000000,
            0x7fefffffffffffff, 0x7ff0000000000000, 0xfff0000000000000,
            0x7ff8000000000000, 0xfff8000000000001}
    for value in [1.0, -1.0, 1e-4, 1e-5, 1e15, 1e16, 1e23, 1e-7,
                  2.2250738585072014e-308, 1792792432.1050122]:
        number = int.from_bytes(struct.pack('>d', value), 'big')
        bits.update([number - 1, number, number + 1])
    bits.update(rng.getrandbits(64) for _ in range(2048))
    float_cases = [{'bits': f'{value:016x}', 'python': json.dumps(float_bits(value))}
                   for value in sorted(bits)]
    cases = []
    selected = sorted(bits)[:12] + [0x8000000000000000, 0x3ff0000000000000,
                                   0x41dab6b8dc06b8d3, 0x42ec8b01f8b6d744,
                                   0x7fefffffffffffff, 0x7ff0000000000000,
                                   0xfff0000000000000, 0x7ff8000000000000]
    selected += [int.from_bytes(struct.pack('>d', value), 'big')
                 for value in [1e-4, 1e-5, 1e15, 1e16, 1e23, 1e-7, -1.0]]
    for index, saved_bits in enumerate(dict.fromkeys(selected)):
        files = [{'path': 'Notes/é😀.md', 'sha256': hashlib.sha256(b'synthetic\r\n').hexdigest()},
                 {'path': 'assets/bin.dat', 'sha256': hashlib.sha256(b'\x00\xff').hexdigest()}]
        base = '/'.join(p.strip('/') for p in ('Corpus/é', 'noevia-backups', TENANT) if p)
        rows = [{**row, 'object': base + '/objects/' + row['sha256'] + '/' + row['path'].rsplit('/', 1)[-1]}
                for row in files]
        settings = {'é': '\u2028\u2029\x00\b\f\n\r\t"\\/😀', 'a': [None, True, False, -0.0, 1e-7],
                    'nested': {'😀': 10**100, '\ue000': -(10**80), 'z': float_bits(saved_bits)}}
        generation = [0, -1, 2**64, 10**100][index % 4]
        manifest = {'format': 'noevia-diary-backup-v1', 'tenant': TENANT, 'generation': generation,
                    'savedAt': float_bits(saved_bits), 'settings': settings,
                    'directories': sorted(['empty/é', 'empty']), 'files': rows}
        body = json.dumps(manifest, sort_keys=True, ensure_ascii=False).encode()
        cases.append({'bits': f'{saved_bits:016x}', 'generation': str(generation),
                      'settings': json.dumps(settings, ensure_ascii=False),
                      'expected': body.decode(), 'sha256': hashlib.sha256(body).hexdigest(),
                      'base': base, 'manifestKey': base + '/manifests/' + hashlib.sha256(body).hexdigest() + '.json'})
    reader_only = []
    for raw in ['"\\ud800"', '"\\udfff"']:
        parsed = json.loads(raw)
        try:
            json.dumps(parsed, ensure_ascii=False).encode()
            writer_can_emit = True
        except UnicodeEncodeError:
            writer_can_emit = False
        reader_only.append({'raw': raw, 'writerCanEmit': writer_can_emit})
    uuid_reader_only = '1234567_123412341234123456789abc'
    namespace_cases = []
    for key in [f'x/noevia-backups/{TENANT}/objects/anything/nested/old-name',
                f'noevia-backups/{TENANT}/objects/a',
                f'xnoevia-backups/{TENANT}/objects/a',
                f'noevia-backups/{TENANT}/objects',
                f'../noevia-backups/{TENANT}/objects/a']:
        safe = not (key.startswith('/') or '\\' in key or '\x00' in key
                    or any(part in ('', '.', '..') for part in key.split('/')))
        accepted = safe and f'/noevia-backups/{TENANT}/objects/' in '/' + key
        namespace_cases.append({'object': key, 'pythonAccepts': accepted})
    destination = Path(__file__).parent / 'fixtures' / 'python.json'
    destination.write_text(json.dumps({'source': 'services6c1eb589 managed_storage.py:283,291-319',
                                      'pythonVersion': list(sys.version_info[:3]),
                                      'readerOnlySurrogates': reader_only,
                                      'readerOnlyUuid': {'input': uuid_reader_only, 'python': str(uuid.UUID(uuid_reader_only))},
                                      'namespace': namespace_cases,
                                      'floats': float_cases, 'manifests': cases},
                                     ensure_ascii=True, separators=(',', ':')) + '\n')

if __name__ == '__main__':
    main()
