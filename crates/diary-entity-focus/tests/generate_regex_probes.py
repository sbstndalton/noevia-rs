#!/usr/bin/env python3
"""Actual frozen-Python regex semantics, synthetic names and text only."""
import argparse
import json
import sys
import unicodedata
from pathlib import Path
from generate_rules import load, SOURCE_HASH
from generate_fixtures import encode_entity, expected, partition


def main():
    if (sys.implementation.name != 'cpython' or sys.version_info[:3] != (3, 12, 15)
            or unicodedata.unidata_version != '15.0.0'):
        raise SystemExit('requires CPython3.12.15 / Unicode15.0.0')
    p = argparse.ArgumentParser()
    p.add_argument('--source', type=Path, required=True)
    p.add_argument('--output', type=Path, default=Path(__file__).with_name('regex-probes.jsonl'))
    args = p.parse_args()
    ef = load(args.source)
    rows = [dict(kind='meta', python=sys.version.split()[0], unicode=unicodedata.unidata_version, source_sha256=SOURCE_HASH)]
    spaces = [chr(cp) for cp in range(0x110000) if chr(cp).isspace()]
    e = ef.Entity('Alder Brannock', ef._keys(['Alder', 'Brannock']))
    refs = ['that\u0301', '\u0301that', 'that\u200c', 'that\u200d', 'that_', 'thaté', 'that\u1c89',
            'ıt written?', 'İt written?', 'thıs written?', 'thiſ written?', 'this records', 'this record',
            'this recorded', 'this records records', 'that week', 'that weeké', 'that week\u0301']
    refs += [ws+'and again' for ws in spaces]
    refs += ['this'+ws+'week' for ws in spaces]
    refs += ['this made'+ws+'up' for ws in spaces]
    for text in refs:
        rows.append(dict(kind='refers_back', text=text, expected=ef.refers_back(text)))
    relations = ["Alder Brannock's "+first+' sister' for first in ['²', 'Ⅳ', '½', 'ͅ', 'Ᲊ']]
    relations += ["Alder Brannock's"+ws+'sister' for ws in spaces]
    relations += ['sister'+ws+'of'+ws+'Alder Brannock' for ws in spaces]
    for text in relations:
        rows.append(dict(kind='relational', text=text, entity=encode_entity(e), expected=ef.relational(text, [e])))
    for name, text in [('Iris Brannock', "ıris Brannock's sister"), ('İris Brannock', "Iris Brannock's sister"),
                       ('Ιris Brannock', "ͅris Brannock's sister"), ('Kerr Brannock', "Kerr Brannock's sister")]:
        entity = ef.Entity(name, ef._keys(name.split()))
        rows.append(dict(kind='relational', text=text, entity=encode_entity(entity), expected=ef.relational(text, [entity])))
    for ws in spaces:
        text = '**'+ws+'Me:'+ws+'** Alder Brannock spoke.'
        rows.append(dict(kind='clean', text=text, expected=ef._clean(text)))
        text = ws+'##'+ws+'Heading\nAlder Brannock spoke.'
        rows.append(dict(kind='body', text=text, expected=ef._body({'text':text})))
    for digit in ['1', '١', '１', '\U0001e5f1']:
        text = '['+digit*4+'-'+digit*2+'-'+digit*2+'] # Header\nAlder Brannock spoke.'
        rows.append(dict(kind='body', text=text, expected=ef._body({'text':text})))
    cues = ['Correction', 'Correction\u0301', 'I\x1fmisremembered', 'I\u0085got it wrong', 'To\x1fcorrect my earlier note']
    for cue in cues:
        candidates = [dict(day='2030-01-10',header='',text='Alder Brannock likes teal tea.',score=.8),
                      dict(day='2030-01-11',header='',text=cue+': Alder Brannock likes blue tea, not teal tea.',score=.9)]
        query = 'What tea does Alder Brannock like?'
        result = ef.focus_candidates(query, candidates)
        rows.append(dict(kind='focus',message=query,rows=candidates,limit=None,margin=.05,earlier=[],
                         partition=partition(candidates,result.anchor),expected=expected(result)))
    args.output.write_text('\n'.join(json.dumps(row,ensure_ascii=False,separators=(',',':')) for row in rows)+'\n')
    print({kind:sum(row['kind']==kind for row in rows) for kind in sorted({row['kind'] for row in rows})})


if __name__ == '__main__':
    main()
