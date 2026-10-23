#!/usr/bin/env python3
"""Freeze entity-focus tables and CPython Unicode 15 predicates from shipped source."""
from __future__ import annotations
import argparse
import hashlib
import importlib.util
import re
import sys
import unicodedata
from pathlib import Path

SOURCE_HASH = "dd4312e26a22cf2132f18bbd0ca4d7fd55a885706b8d0aa8997c14e94eeb7632"

def load(path: Path):
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if digest != SOURCE_HASH: raise SystemExit(f"source hash mismatch: {digest}")
    spec = importlib.util.spec_from_file_location("frozen_entity_focus", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module

def rust_string(s: str) -> str:
    # JSON string syntax is a Rust string literal for these static tokens.
    import json
    return json.dumps(s, ensure_ascii=False)

def ranges(pred):
    out=[]
    start=last=None
    for cp in range(0x110000):
        if pred(chr(cp)):
            if start is None: start=last=cp
            elif cp == last+1: last=cp
            else: out.append((start,last)); start=last=cp
    if start is not None: out.append((start,last))
    return out

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--source',type=Path,required=True)
    parser.add_argument('--output',type=Path,default=Path(__file__).resolve().parents[1]/'src'/'generated_rules.rs')
    args=parser.parse_args()
    ef=load(args.source)
    tables={
      'MONTHS':ef.MONTHS,'WEEKDAYS':ef._WEEKDAYS,'TITLES':ef.TITLES,'NOT_NAMES':ef._NOT_NAMES,
      'AFTER_VERB':ef._AFTER_VERB,'DATE_LEAD':ef._DATE_LEAD,'VERB_FORMS':ef._VERB_FORMS,
      'ROLE_WORDS':ef._ROLE_WORDS,'NOT_PERSON_FIRST':ef._NOT_PERSON_FIRST,
      'NOT_PERSON_LAST':ef._NOT_PERSON_LAST,'SURNAME_LIKE_LAST':ef._SURNAME_LIKE_LAST,
      'PLACE_CUES':ef._PLACE_CUES,'GIVEN_NAMES':ef.GIVEN_NAMES,'RELATIONS':ef.RELATIONS,
      'CUE_WORDS':ef._CUE_WORDS,
    }
    lines=['//! Generated from frozen CPython source; regenerate with tests/generate_rules.py.',
           f'pub const SOURCE_SHA256: &str = "{SOURCE_HASH}";',
           f'pub const PYTHON_VERSION: &str = "{sys.version.split()[0]}";',
           f'pub const UNICODE_VERSION: &str = "{unicodedata.unidata_version}";']
    for name,values in tables.items():
        values=sorted(values)
        lines.append(f'pub const {name}: &[&str] = &[{", ".join(map(rust_string,values))}];')
    lines.append('pub const SHORT_FORMS: &[(&str, &[&str])] = &[')
    for name,fulls in sorted(ef.SHORT_FORMS.items()):
        lines.append(f'    ({rust_string(name)}, &[{", ".join(map(rust_string, fulls))}]),')
    lines.append('];')
    lines.append('pub const PLACE_PHRASES: &[&[&str]] = &[')
    for phrase in sorted(ef.PLACE_PHRASES):
        lines.append(f'    &[{", ".join(map(rust_string, phrase))}],')
    lines.append('];')
    lines.append('pub const FOLD: &[(char, &str)] = &[')
    for src,dest in sorted(ef._FOLD.items()):
        lines.append(f"    ('\\u{{{src:04x}}}', {rust_string(dest)}),")
    lines.append('];')
    for name,pred in [('DECIMAL_RANGES',lambda c: bool(re.fullmatch(r'\d',c))),
                      ('COMBINING_RANGES',lambda c: unicodedata.combining(c)!=0)]:
        rows=ranges(pred)
        lines.append(f'pub const {name}: &[(u32,u32)] = &[')
        lines.extend(f'    (0x{a:x},0x{b:x}),' for a,b in rows)
        lines.append('];')
    args.output.write_text('\n'.join(lines)+'\n')
    print(f'generated {len(tables)} tables, {len(ef.SHORT_FORMS)} short forms, {len(ef.PLACE_PHRASES)} places')

if __name__=='__main__': main()
