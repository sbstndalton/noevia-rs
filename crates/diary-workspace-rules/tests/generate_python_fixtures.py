"""Execute selected shipped Python 3.12 source functions on synthetic data only.

Run from the noevia-rs worktree: `~/bin/devrun python3.12 .../generate_python_fixtures.py`.
No managed storage, DB, runtime, or real Diary data is opened.
"""
import ast
import base64
import calendar
import hashlib
import json
import random
import re
import sys
import time
import struct
import uuid
from pathlib import Path
from types import SimpleNamespace

SERVICES = Path('/home/sebastian/noevia-work/noevia-services/diary/agent')
OUT = Path(__file__).parent / 'fixtures' / 'python.json'

def load_function(file, name, env):
    source = (SERVICES / file).read_text()
    nodes = [n for n in ast.parse(source).body if isinstance(n, ast.FunctionDef) and n.name == name]
    assert len(nodes) == 1, (file, name)
    exec(compile(ast.Module(body=nodes, type_ignores=[]), str(SERVICES / file), 'exec'), env)
    return env[name]

def digest(data): return hashlib.sha256(data).hexdigest()

def fake_join(root, *parts):
    return '/'.join(p.strip('/') for p in [root, *parts] if p)

env = {'re': re, 'calendar': calendar, 'hashlib': hashlib, 'json': json,
       'base64': base64, 'uuid': uuid, 'MAX_FILE': 512 * 1024, 'digest': digest,
       'HTTPException': lambda *a: ValueError(*a),
       'MEMORY_FOLDER': 'AI Memory', 'OpError': lambda *a: ValueError(*a), 'time': time}
for file, name in [('corpus_store.py', 'month_name_pattern'),
                   ('workspace_files.py', 'safe_path'),
                   ('workspace_ops.py', 'rel_path'),
                   ('workspace_ops.py', 'protected'),
                   ('workspace_ops.py', 'folder_version'),
                   ('workspace_ops.py', '_replaced_name'),
                   ('workspace_trash.py', 'identity'),
                   ('workspace_trash.py', 'decode')]:
    load_function(file, name, env)

rng = random.Random(430911)
months = [calendar.month_name[n] for n in range(1, 13)]
protection = []
configs = [
    ('', 'Entries', '', '{year}-{month02}.md', 'INDEX.md', months),
    ('Corpus/é', 'Daily', 'months/', 'Diary - {month_name} {year}.md', 'index.md', months),
    ('/root//', 'Capture', '/archive/', '{year}/{month}.md', 'Index.md',
     ['Januar', 'Februar', 'März', 'Avril', 'mai', 'juin', 'juillet', 'août', 'septembre', 'octobre', 'novembre', 'décembre']),
]
for root, entries, monthly, template, index, names in configs:
    env['month_name_pattern'] = lambda names=names: '|'.join(re.escape(m) for m in names)
    store = SimpleNamespace(remote_root=root, entries_prefix=entries, monthly_prefix=monthly,
                            month_file_template=template, index_file=index,
                            _join=lambda *parts, root=root: fake_join(root, *parts),
                            index_path=lambda root=root, monthly=monthly, index=index: fake_join(root, monthly, index))
    base = fake_join(root, monthly)
    paths = [store.index_path(), fake_join(root, entries), fake_join(root, entries, 'x.md'),
             fake_join(root, 'AI Memory'), fake_join(root, 'AI Memory', 'x.md'),
             fake_join(root, 'Ordinary.md'), fake_join(root, monthly, '2026-09.md'),
             fake_join(root, monthly, '2026-9.md'), fake_join(root, monthly, 'Diary - März 2026.md'),
             fake_join(root, monthly, 'Diary - août 2026.md'),
             fake_join(root, monthly, '2026/9.md'), fake_join(root, monthly, '2026/09.md'),
             fake_join(root, monthly, '9999-99.md'),
             fake_join(root, monthly, '2026-09.md\n'), fake_join(root, monthly, '１２３４-09.md')]
    for i in range(120):
        paths.append(fake_join(root, rng.choice(['', monthly, entries, 'AI Memory', 'Random']),
                                   rng.choice(['x.md', '2026-01.md', '2010-1.md', 'Diary - septembre 2010.md', 'é.md', f'{i}-x.md'])))
    for path in paths:
        protection.append({'config': {'remote_root': root, 'entries_prefix': entries,
                          'monthly_prefix': monthly, 'month_file_template': template,
                          'index_file': index, 'month_names': names},
                          'path': path, 'expected': env['protected'](store, path)})

folder = []
for i in range(180):
    full = rng.choice(['notes', 'Corpus/é', 'emoji/😀', ''])
    paths = [full + rng.choice(['', '/a.md', '/é.md', '/😀.md', '/nested/f.md']) for _ in range(rng.randrange(0, 15))]
    files = {p: {'version': digest((p + str(i)).encode())} for p in paths[:rng.randrange(len(paths)+1)]}
    dirs = list(dict.fromkeys(paths[rng.randrange(len(paths)+1):])) if paths else []
    folder.append({'full': full, 'files': [{'path': p, 'version': r['version']} for p, r in files.items()],
                   'dirs': dirs, 'expected': env['folder_version'](files, dirs, full)})

paths = ['notes/a.md', 'é/😀.MD', '/notes/a.md/', '', '/', '../a.md', '.trash/a.md',
         'notes//a.md', 'notes/a.txt', 'x' * 500 + '.md', 'x' * 496 + '.md',
         'a\\b.md', 'a\0b.md']
path_cases = []
for path in paths:
    for mode in ['safe', 'rel', 'rel_root']:
        try:
            result = env['safe_path'](path) if mode == 'safe' else env['rel_path'](path, mode == 'rel_root')
        except Exception:
            result = None
        path_cases.append({'path': path, 'mode': mode, 'expected': result})

ids = ['12345678-1234-1234-1234-123456789abc', '{12345678-1234-1234-1234-123456789ABC}',
       'urn:uuid:12345678123412341234123456789abc', '12345678123412341234123456789abc',
       '1234567_123412341234123456789abc', 'URN:UUID:12345678123412341234123456789abc',
       'x', '', '12345678-1234-1234-1234-123456789abz',
       '+12345678123412341234123456789ab', '0x12345678123412341234123456789a',
       ' 12345678123412341234123456789a', '12345678123412341234123456789a ',
       '12345678123412341234123456789a_', '12345678_23412341234123456789ab',
       '12345678__3412341234123456789ab',
       '١' * 32, '0x_' + '1' * 29, '\u001c' + '1' * 31]
for _ in range(300):
    h = f'{rng.getrandbits(128):032x}'
    mode = rng.randrange(9)
    if mode == 0: value = h
    elif mode == 1: value = h[:8] + '-' + h[8:12] + '-' + h[12:16] + '-' + h[16:20] + '-' + h[20:]
    elif mode == 2: value = '{' + h + '}'
    elif mode == 3: value = 'urn:uuid:' + h
    elif mode == 4: value = h[:rng.randrange(1,31)] + '_' + h[rng.randrange(1,31):]
    elif mode == 5: value = '+' + h[:-1]
    elif mode == 6: value = '0x' + h[:-2]
    elif mode == 7: value = h[:15] + '-' + h[15:]
    else: value = h[:15] + 'x' + h[16:]
    ids.append(value)
identity = []
for value in ids:
    try: expected = env['identity'](value)
    except Exception: expected = None
    identity.append({'input': value, 'expected': expected})

capsules = []
for i in range(100):
    raw = rng.randbytes(rng.randrange(0, 1000))
    path = rng.choice(['notes/a.md', 'é/😀.MD', 'x.md', 'a b/c.md', 'x/line\nfeed.md', 'x/\u2028é.md'])
    operation = str(uuid.UUID(int=rng.getrandbits(128)))
    at = rng.choice([0.0, -0.0, 1.0, 1e-7, 1e16, 1792793504.9675405,
                     rng.random()*2e10])
    state = rng.choice(['trashed', 'restored'])
    record = {'format': 'noevia-trash-v1', 'id': operation, 'path': path, 'version': digest(raw),
              'data': base64.b64encode(raw).decode(), 'state': state, 'trashedAt': at}
    if state == 'restored': record['restoredAt'] = at+1.0
    encoded = json.dumps(record, sort_keys=True, ensure_ascii=False).encode()
    assert env['decode'](encoded, operation)[1] == raw
    capsules.append({'id': operation, 'path': path, 'raw': base64.b64encode(raw).decode(),
                     'state': state, 'trashedAtBits': struct.pack('>d', at).hex(),
                     'restoredAtBits': struct.pack('>d', record['restoredAt']).hex() if state == 'restored' else None,
                     'expected': encoded.decode(), 'sha256': digest(encoded)})

replaced = []
for path in ['notes/a.md', 'é/😀.MD', 'plain.md', 'x' * 497 + '.md',
             'é/' + '😀' * 490 + '.md', '.' * 470 + '/name.md']:
    for at in [0.0, -1.2, 1792793504.9675405]:
        replaced.append({'path': path, 'atBits': struct.pack('>d', at).hex(),
                         'expected': env['_replaced_name'](path, at)})

marker_re = next(n.value for n in ast.parse((SERVICES / 'corpus.py').read_text()).body
                 if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'MARKER_RE' for t in n.targets))
marker = eval(compile(ast.Expression(marker_re), str(SERVICES / 'corpus.py'), 'eval'), {'re': re})
whitespace = [chr(i) for i in range(0x110000) if chr(i).isspace()]
marker_cases = [b'', b'raw <!-- xid:abcd-1234 -->', b'raw <!-- xid:z -->', b'\xff\xfe']
marker_cases += [f'<!--{c}xid:abcd{c}-->'.encode() for c in whitespace]
marker_cases += [f'<!--{c}xid:abcd{c}-->'.encode() for c in ['\u200b', '\u180e']]
markers = [{'raw': base64.b64encode(raw).decode(),
            'trashable': len(raw) <= 512*1024 and not marker.search(raw.decode('utf-8', errors='replace'))}
           for raw in marker_cases]

legacy = []
base = json.loads(capsules[0]['expected'])
variants = [base, {**base, 'extra': {'x': 1}}, {k:v for k,v in base.items() if k != 'trashedAt'},
            {**base, 'data': 'eA=='}, {**base, 'path': '../escape.md'},
            {**base, 'state': 'wrong'}, {**base, 'data': '!'},
            {**base, 'version': 'changed'}, {**base, 'path': 'a.txt'},
            {**base, 'data': base64.b64encode(b'').decode(), 'version': digest(b'')}]
# Python json.loads accepts duplicate keys with the last value winning.
variants.append(json.dumps(base, sort_keys=True, ensure_ascii=False).replace('"path": ', '"path": "../escape.md", "path": ', 1))
for variant in variants:
    encoded = (variant if isinstance(variant, str) else json.dumps(variant, sort_keys=True, ensure_ascii=False)).encode()
    try: env['decode'](encoded, base['id']); valid = True
    except Exception: valid = False
    legacy.append({'encoded': encoded.decode(), 'operation': base['id'], 'valid': valid})

out = {'source': {'sourceSha256': {name: digest((SERVICES / name).read_bytes()) for name in
                  ['workspace_ops.py', 'workspace_trash.py', 'workspace_files.py', 'corpus_store.py', 'corpus.py']},
                  'pythonVersion': list(sys.version_info[:3]),
                  'files': ['workspace_ops.py', 'workspace_trash.py', 'workspace_files.py', 'corpus_store.py']},
       'protection': protection, 'folder': folder, 'paths': path_cases,
       'identity': identity, 'capsules': capsules, 'legacy': legacy,
       'replaced': replaced, 'markers': markers}
OUT.write_text(json.dumps(out, ensure_ascii=False, separators=(',', ':')) + '\n')
print(f'{len(protection)} protected, {len(folder)} folder, {len(capsules)} capsule, {len(legacy)} legacy, {len(replaced)} replace, {len(markers)} marker fixtures; {OUT}')
