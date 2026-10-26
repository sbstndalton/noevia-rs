"""Synthetic actual-source oracle. Run with Python 3.12 and the services diary root.

No storage, model, edit/render or real Diary data is loaded. corpus.py is run
directly; retrieval._parse_chunks is compiled from its original AST method.
"""
import ast
import hashlib
import json
from pathlib import Path
import random
import runpy
import sys
import types
import unicodedata

if sys.version_info[:2] != (3, 12):
    raise SystemExit("Production oracle requires Python 3.12")

root = Path(sys.argv[1])
output = Path(__file__).parent
corpus_path = root / "agent/corpus.py"
retrieval_path = root / "agent/retrieval.py"
fmt = types.SimpleNamespace(**runpy.run_path(str(corpus_path)))
tree = ast.parse(retrieval_path.read_text())
method = next(n for cls in tree.body if isinstance(cls, ast.ClassDef) and cls.name == "Retriever"
              for n in cls.body if isinstance(n, ast.FunctionDef) and n.name == "_parse_chunks")
scope = {"fmt": fmt}
exec("from __future__ import annotations\n" + ast.unparse(method), scope)
owner = types.SimpleNamespace(_hash=lambda text: hashlib.sha256(text.encode("utf-8")).hexdigest())
parse_chunks = lambda text: scope["_parse_chunks"](owner, text)
sample_tree = ast.parse((root / "tests/test_corpus.py").read_text())
sample = next(ast.literal_eval(n.value) for n in sample_tree.body
              if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "SAMPLE" for t in n.targets))

decimal_zeros = [i for i in range(0x110000) if unicodedata.decimal(chr(i), None) == 0]
unicode_rs = ("//! Generated from Python " + sys.version.split()[0] + "; Unicode " + unicodedata.unidata_version + ".\n"
              "// Python re \\d matches Unicode decimal digits, not all numeric characters.\n"
              "pub(crate) fn decimal(c: char) -> Option<u32> {\n"
              "    let code = c as u32;\n"
              "    [" + ", ".join(hex(i) for i in decimal_zeros) + "].into_iter()\n"
              "        .find(|start| code >= *start && code < *start + 10).map(|start| code - start)\n}\n")
(output.parent / "src/unicode.rs").write_text(unicode_rs)

base = "## Saturday, August 29, 2026\n### 09:15 — Morning\n**Me:** café ☕\n\n**Assistant:** café noted.\n<!-- xid:AbC-123 -->\n"
cases = [("", "empty"), (sample, "existing-SAMPLE"), (base, "unicode-baseline"),
         (base + base, "duplicate-days-and-chunks"), (None, "optional-empty")]
for sep in ["\n", "\r", "\r\n", "\v", "\f", "\x1c", "\x1d", "\x1e", "\x85", "\u2028", "\u2029"]:
    cases.append((base.replace("\n", sep), "splitlines-" + repr(sep)))
for ws in [chr(i) for i in range(0x110000) if chr(i).isspace()] + ["\u200b", "\ufeff"]:
    cases.append((f"## January 1, 2020{ws}\n### {ws}1:02{ws}—{ws} topic{ws}\n{ws}**Me:** hidden\n**Me:**{ws}visible{ws}\n**Claude:**{ws}answer{ws}", "whitespace-" + repr(ws)))
for zero in decimal_zeros:
    digit = lambda n: chr(zero + n)
    cases.append((f"## January {digit(2)}{digit(9)}, {digit(2)}{digit(0)}{digit(2)}{digit(4)}\n### {digit(9)}:{digit(0)}{digit(7)} – Unicode\n**Me:** digits\n**Assistant:** yes", "decimal-" + hex(zero)))
# Unicode 16 digit additions must remain non-digits under the shipped 3.12 oracle.
for zero in [0x10d40, 0x116d0, 0x116da, 0x11bf0, 0x16130, 0x16d70, 0x1ccf0, 0x1e5f1]:
    cases.append((f"## January {chr(zero+1)}, 2020\n### 1:02\n**Me:** newer digits", "excluded-Unicode16-" + hex(zero)))
for heading in ["January 1, 2020", "Bogus 1, 2020", "Friday, fEbRuArY 29, 2000", "Monday, February 29, 1900",
                "Nope, April 31, 2020", "January 0, 2020", "January 1, 0000", "January 1, 0001",
                "January ١, ２０２６", "January ², 2020", "January 001, 2020", "January 1, 20200",
                "January 1, 202", "Jänuary 1, 2020", "Monday,  January 1, 2020", "January 1,2020"]:
    cases.append((f"## {heading}\n### 99:99 - not a clock\n**Me:** x\n**Claude:** y", "day-" + heading))
for body in ["", "stray", "**Me:**", "**Me:**\n**Assistant:**", "**Assistant:** orphan\n**Me:** only",
             "**Me:** hi\nmultiline  \nline two\n**Claude:** old\n**Assistant:** still text",
             "**Me:** first\n**Claude:** a\n**Me:** second\n**Assistant:** b",
             "**Me:** inline **Assistant:** not role", "**Me:** **Assistant:** immediate role",
             "**Me:** m<!-- xid:aaa -->\n**Assistant:** a<!-- xid:BBB -->",
             "**Me:** <!--xid:a--><!-- xid:b -->\n**Claude:** <!-- \nxid:-- \n-->",
             "**Me:** <!-- xid:nope -->\n**Claude:** <!-- xid: -->",
             "**Me:** <!-- xid:a---> tail\n**Claude:** <!--xid:abc-->",
             "**Me:** m\n### new\n**Me:** n\n## Bogus 99, 0000\n### invalid-date\n**Me:** z",
             "**Me:** a\n\n**Me:**\n**Me:** b\n", "\\**Me:** escaped\n**Me:** visible"]:
    cases.append(("preamble\n### ignored\n## January 1, 2020\nstray before sub\n### 1:02\n" + body, "body-" + repr(body)))
rng = random.Random(1071)
tokens = ["**Me:** a", "**Me:**\t", "**Assistant:** b", "**Claude:** c", "stray  ",
          "<!-- xid:aaa -->", "<!--xid:-->", "### 12:34 — topic", "### \t", "## January 1, 2020",
          "## February 30, 2020", "## Nope 1, 2020", "", " café ", "café", "\\**Me:** literal"]
for i in range(300):
    text = "\n".join(rng.choice(tokens) for _ in range(rng.randrange(40)))
    cases.append((text, f"random-{i}"))
fixtures = []
for text, name in cases:
    days = fmt.parse_diary(text or "")
    parsed = [{"header": d.header, "date": d.date.isoformat() if d.date else None,
               "subsections": [{"header": s.header, "time": s.time, "topic": s.topic,
                 "exchanges": [{"me": e.me, "claude": e.claude, "xid": e.xid} for e in s.exchanges]}
                 for s in d.subsections]} for d in days]
    fixtures.append({"name": name, "input": text, "days": parsed, "chunks": parse_chunks(text)})
document = {"source_commit": "6c1eb5891c75b6fb9f04c4484ba39da80a5b4b9e",
            "corpus_sha256": hashlib.sha256(corpus_path.read_bytes()).hexdigest(),
            "retrieval_sha256": hashlib.sha256(retrieval_path.read_bytes()).hexdigest(),
            "python": sys.version.split()[0], "unicode": unicodedata.unidata_version, "fixtures": fixtures}
(output / "python-corpus.json").write_text(json.dumps(document, ensure_ascii=False, indent=2) + "\n")
print(f"Generated {len(fixtures)} fixtures; Unicode {unicodedata.unidata_version}, {len(decimal_zeros)} decimal blocks")
