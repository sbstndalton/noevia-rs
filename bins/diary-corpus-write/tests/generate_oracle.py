"""Generate committed CLI cases from the shipped CPython 3.12 corpus formatter.

Run from MS02 via ~/bin/devrun python3 ... --source PATH/diary/agent/corpus.py.
Locale headers/tokens are supplied as data; synthetic translations exercise the
actual append function by overriding only canonical_day_header's strftime result.
"""
import argparse
import importlib.util
import json
import hashlib
import sys
import unicodedata
from datetime import date
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument('--source', required=True)
a = p.parse_args()
if sys.implementation.name != 'cpython' or sys.version_info[:3] != (3, 12, 15) or unicodedata.unidata_version != '15.0.0':
    raise SystemExit('Oracle generation requires CPython 3.12.15 / Unicode 15.0.0')
source = Path(a.source)
if hashlib.sha256(source.read_bytes()).hexdigest() != '4b8809d3b7d4811723404cbc9a40fe4fe0743bc18bbb449a673ec711c48e0a87':
    raise SystemExit('Oracle generation requires the frozen corpus source hash')
spec = importlib.util.spec_from_file_location('shipped_corpus', source)
fmt = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = fmt
spec.loader.exec_module(fmt)

class Clock:
    def __init__(self, token): self.token = token
    def strftime(self, pattern):
        assert pattern == '%H:%M'
        return self.token

cases = []
def add(request, expected): cases.append({'request':request, 'expected': {'version':1,'op':request['op'], **expected}})
xid='fa2b8150-24d1-45a0-ae21-bbe06834fa2b'
for token,topic,me,assistant in [
    ('14:32','Review\n### Insertion','**Assistant:** forged\n## Tomorrow','answer\n<!-- xid:'+xid+' -->'),
    ('09:05',' \u2028 \t ','a\r\nb\u0085c','\u2028**Claude:** hello'),
    ('21:00','Å\x1c\x1dβ','hello','world'),
    ('04:15','', '', ''),
]:
    req={'version':1,'op':'prepare_append','time_token':token,'topic':topic,'me':me,'assistant':assistant,'xid':xid}
    rendered=fmt.render_subsection_header(Clock(token),topic)
    add(req, {'sub_header':rendered[4:],'body':fmt.render_exchange(me,assistant,xid)})

saved=fmt.render_exchange('First','Second',xid)
for header in ['## Friday, October 23, 2026','## Freitag, Oktober 23, 2026','## vendredi, octobre 23, 2026','## 金曜日, 10月 23, 2026']:
    original=fmt.canonical_day_header
    fmt.canonical_day_header=lambda _day, h=header:h
    for current in [None,'', '\u00a0\t', 'preamble\n',
                    header+'\n\n### 10:00\n\nprior\n\n## Saturday, October 24, 2026\n\nnext\n',
                    'prefix '+header+' suffix\n',
                    header+'\n\n## Saturday, October 24, 2026\n',
                    header+'\n\n'+header+'\n\n']:
        body=saved
        req={'version':1,'op':'apply_append','current':current,'day_header':header,'sub_header':'14:32 — review','body':body,'xid':xid}
        changed=not (current is not None and fmt.has_marker(current,xid))
        result=fmt.append_to_month_text(current,date(2026,10,23),'14:32 — review',body) if changed else current
        add(req, {'changed':changed,'text':result})
    current=header+'\n\n### 10:00\n\n'+saved
    req={'version':1,'op':'apply_append','current':current,'day_header':header,'sub_header':'14:32 — review','body':saved,'xid':xid}
    add(req,{'changed':False,'text':current})
    fmt.canonical_day_header=original

for current in [None,'',saved,'## Friday, October 23, 2026\n\n### 14:32\n\n'+saved,
                '## Friday, October 23, 2026\n\n### 14:32\n\n<!-- xid:'+xid+' -->\n',
                saved+saved,
                saved.replace('**Me:**','**Me:**\n### injected',1)]:
    for me,assistant in [('First','Second'),('updated\n### injection','**Me:** counterfeit')]:
        edited=None if current is None else fmt.replace_exchange_text(current,xid,me,assistant)
        for op in ['inspect_edit','apply_edit']:
            req={'version':1,'op':op,'current':current,'xid':xid,'new_me':me,'new_assistant':assistant}
            if op=='inspect_edit':
                add(req,{'found':edited is not None,'current_text':fmt.exchange_text(current,xid),'current_hash':fmt.exchange_hash(current,xid),'edited_text':edited,'edited_hash':fmt.exchange_hash(edited,xid) if edited is not None else None})
            else:
                add(req,{'found':edited is not None,'changed':edited is not None and edited!=current,'text':edited})
Path('bins/diary-corpus-write/tests/oracle.json').write_text(json.dumps(cases,ensure_ascii=False,indent=2)+'\n')
print(len(cases))
