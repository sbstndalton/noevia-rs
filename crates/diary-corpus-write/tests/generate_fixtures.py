"""Regenerate corpus-write-fixtures.json with the shipped Python 3.12 oracle.

Usage (on the development host):
  ~/bin/devrun python3.12 tests/generate_fixtures.py \
      --source /path/to/noevia-services/diary/agent/corpus.py
Run from this crate's directory. Uses only wholly synthetic text.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import locale
import random
import sys
import unicodedata
from datetime import date, datetime
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, required=True)
    args = parser.parse_args()
    locale.setlocale(locale.LC_ALL, "C")
    source = args.source.read_bytes()
    spec = importlib.util.spec_from_file_location("diary_corpus_oracle", args.source)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load oracle")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    cases: list[dict] = []

    def add(op: str, expected: object, **kwargs: object) -> None:
        cases.append({"op": op, "args": kwargs, "expected": expected})

    line_breaks = "\r\n\x0b\x0c\x1c\x1d\x1e\x85\u2028\u2029"
    spaces = ["", " ", "\t", "\u00a0", "\u2003", "\u3000", "\x1f", "\u2028"]
    text_cases = [
        "", "plain café 漢字", "# first line\n## second line", "### Key\npoints",
        "######\r####### not a heading\r\n###\tTab", "**Assistant:** owner words",
        "**Claude:** reply\n**Me:** again", "<!-- xid:a-b -->", "<!--\n xid:a-b -->",
        "<!--\u2003xid:dead -->", "<!--not-xid:a -->", "\\### already escaped",
        "line\x0b## break\x0c**Assistant:** role\x85# tag",
        "line\x1c### split\x1d**Claude:** split\x1e<!-- xid:abc -->",
        "line\u2028## heading\u2029**Me:** role", "\r\n## CRLF\r\n",
        "text\n\n", "\u00a0## header\u00a0", "a\n<!-- xid:wrong -->\n<!-- xid:a -->",
    ]
    rng = random.Random(8031071)
    atoms = ["A", "é", "漢", "🫧", "#", "**Me:**", "**Assistant:**", "<!--", "xid:a", "-->", "\\", " ", "\t", "\u00a0", "\u2003", "\u3000"] + list(line_breaks)
    for _ in range(90):
        text_cases.append("".join(rng.choice(atoms) for _ in range(rng.randrange(0, 24))))
    for text in text_cases:
        add("escape", module.escape_entry_text(text), text=text)
        add("strip_markers", module.strip_markers(text), text=text)
    for text in ["", "<!-- xid:a -->", "<\\!-- xid:a -->", "<!-- xid:a -->\n<!-- xid:a -->", "<!--\u2003xid:a -->", "<!-- xid:zz -->", "<!-- xid:A-F -->"]:
        for xid in ["a", "A-F", "zz", "", "a -->\n<!-- xid:zz"]:
            add("has_marker", module.has_marker(text, xid), text=text, xid=xid)
    for d in [date(1, 1, 1), date(2024, 2, 29), date(2026, 8, 30), date(9999, 12, 31)]:
        add("canonical_day_header", module.canonical_day_header(d), date=d.isoformat())
    times = [datetime(2026, 8, 30, 0, 0), datetime(2026, 8, 30, 14, 32), datetime(2026, 8, 30, 23, 59)]
    for now in times:
        for topic in ["", " Topic ", "A\n## Injection", "One\r\nTwo\u2028### Three", "\u00a0", "\x1fHello\x1f", "**Me:**", "雪\u3000山"]:
            add("render_subsection_header", module.render_subsection_header(now, topic), time=now.strftime("%H:%M:%S"), topic=topic)
    for me in ["", " hello ", "**Assistant:** lie", "A\n## day", "<!--\n xid:a -->", "\u00a0漢字\u00a0"]:
        for claude in ["", " answer ", "# first\n### later", "**Claude:** answer", "<!-- xid:b -->"]:
            add("render_exchange", module.render_exchange(me, claude, "a-b-0"), me=me, claude=claude, xid="a-b-0")
    for header in ["", " 14:32 — Topic ", "\u00a0雪\u00a0"]:
        for body in ["", " line ", "\n\nbody\n", "### raw\n**Me:** unescaped", "\u2003x\u2003"]:
            add("render_subsection_block", module.render_subsection_block(header, body), header=header, body=body)

    x = "a-b-0"
    base = "## Sunday, August 30, 2026\n\n### 14:32 — Topic\n\n" + module.render_exchange("old", "answer", x) + "\n" + module.render_exchange("sibling", "response", "b-c-1")
    edit_texts = [
        base,
        base + "\n## Monday, August 31, 2026\n\n### 09:00\n\n" + module.render_exchange("later", "response", x),
        "**Me:** first\n\n**Assistant:** first answer\n\n<!-- xid:a-b-0 -->\n**Me:** second\n<!-- xid:a-b-0 -->\n",
        "## Sunday, August 30, 2026\n<!-- xid:a-b-0 -->\n",
        "### 14:32\n**Assistant:** malformed\n<!-- xid:a-b-0 -->",
        "**Me:** text\n### injected boundary\n<!-- xid:a-b-0 -->",
        "**Me:** text\n#### not a boundary\n<!-- xid:a-b-0 -->",
        "**Me:** text\n\n<!-- xid:a-b-0 -->",
        "**Me:** text\r\n\r\n<!-- xid:a-b-0 -->\r\n",
        "prefix <!-- xid:a-b-0 -->\n**Me:** later\n<!-- xid:a-b-0 -->",
        "**Me:** text\n<!--xid:a-b-0-->",
        "**Me:** text\n<!-- xid:a-b-0 --> trailing",
        "**Me:** text\n<!-- xid:a-b-0 -->\n<!-- xid:a-b-0 -->",
        "**Me:** original\n\n**Assistant:** answer\n\n<!-- xid:a-b-0 -->\n",
    ]
    for text in edit_texts:
        for xid in [x, "absent", "b-c-1"]:
            add("exchange_text", module.exchange_text(text, xid), text=text, xid=xid)
            add("exchange_hash", module.exchange_hash(text, xid), text=text, xid=xid)
            for me, claude in [("new", "new answer"), ("old", "answer"), ("", ""), ("**Assistant:** injected", "line\n### forged"), ("\u00a0雪\u00a0", "\u2003")]:
                add("replace_exchange_text", module.replace_exchange_text(text, xid, me, claude), text=text, xid=xid, me=me, claude=claude)
    d = date(2026, 8, 30)
    h = module.canonical_day_header(d)
    next_h = module.canonical_day_header(date(2026, 8, 31))
    months = [
        None, "", " \u00a0\n", h + "\n\n", h + "\n\n### 08:00\n\nprior\n",
        h + "\n\n### 08:00\n\nprior\n\n" + next_h + "\n\n### 09:00\n\nnext\n",
        next_h + "\n\n### 09:00\n\nnext\n" + h + "\n\n### 10:00\n\nold\n" + next_h + "\n",
        h + "\n\n" + h + "\n\n### 09:00\n\nlast duplicate\n" + next_h + "\n",
        "prefixed " + h + "\n\n### 09:00\n\ntext\n",
        "### embedded " + h + " text\n",
        h + "\r\n\r\n### 09:00\r\n\r\ntext\r\n",
        h + "\n\u00a0\n\n" + next_h + "\n",
        h + "\n### 09:00\nold\n## Banana ٣٠, ٢٠٢٦\nmalformed date but syntactic header\n",
        h + "\n### 09:00\nold\n## September 1, 2026\nnext\n",
        h + "\n### 09:00\nold\n## September 1, 2026  \nnext\n",
    ]
    for text in months:
        for sub, body in [("14:32 — Work", "entry"), (" 14:32 ", " \nbody\n "), ("", ""), ("雪", "\u00a0body\u00a0")]:
            add("append_to_month_text", module.append_to_month_text(text, d, sub, body), month_text=text, date=d.isoformat(), header=sub, body=body)
    data = {
        "oracle": "noevia-services diary.agent.corpus CPython 3.12, C locale",
        "python": sys.version.split()[0],
        "unicode": unicodedata.unidata_version,
        "source_sha256": hashlib.sha256(source).hexdigest(),
        "cases": cases,
    }
    output = Path(__file__).with_name("corpus-write-fixtures.json")
    output.write_text(json.dumps(data, ensure_ascii=False, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {len(cases)} synthetic cases to {output}")


if __name__ == "__main__":
    main()
