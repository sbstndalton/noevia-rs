"""Offline r2 oracle, using the frozen original Python config loader.

record <unused-binary> <evidence-dir> | replay <config-oracle> <evidence-dir>
Fixtures live outside the source tree. Only interpreter traceback frames are
omitted: exception class/message and both output streams compare byte-for-byte.
"""
import json
import io
import logging
import os
from pathlib import Path
import subprocess
import sys
import traceback

CASES = [
    ("value-first", "llm: {chat_model: !!str {=: local-model, note: ignored}}\n"),
    ("value-last", "llm: {chat_model: !!str {note: ignored, =: local-model}}\n"),
    ("value-middle", "x: !!str {before: [a, {b: c}], =: chosen, after: {d: [e]}}\ny: retained\n"),
    ("first-value-wins", "x: !!str {=: first, note: ignored, =: last}\n"),
    ("nested-first", "x: !!str {=: {=: chosen, note: ignored}, note: ignored}\n"),
    ("nested-last", "x: !!str {note: ignored, =: {note: ignored, =: chosen}}\n"),
    ("nested-tag-ignored", "x: !!str {=: !!int {=: 0123, extra: ignored}, extra: ignored}\n"),
    ("ignored-unknown-tag", "x: !!str {note: !unknown ignored, =: chosen}\n"),
    ("ignored-unhashable-key", "x: !!str {? [a, b]: ignored, =: chosen}\n"),
    ("ignored-merge", "x: !!str {<<: not-a-mapping, =: chosen}\n"),
    ("selected-tag-ignored", "x: !!str {=: !unknown chosen, extra: ignored}\n"),
    ("explicit-value-key", "x: !!str {!!value '=': chosen, extra: ignored}\n"),
    ("uri-value-key", "x: !!str {!<tag:yaml.org,2002:value> '=': chosen, extra: ignored}\n"),
    ("sequence-value-key", "x: !!str {? !!value [a]: chosen, extra: ignored}\n"),
    ("mapping-value-key", "x: !!str {? !!value {a: b}: chosen, extra: ignored}\n"),
    ("quoted-equals-refused", "x: !!str {'=': chosen, extra: ignored}\n"),
    ("string-tag-equals-refused", "x: !!str {!!str '=': chosen, extra: ignored}\n"),
    ("no-value-refused", "x: !!str {extra: ignored}\n"),
    ("nested-no-value-refused", "x: !!str {=: {extra: ignored}, extra: ignored}\n"),
    ("selected-sequence-refused", "x: !!str {extra: ignored, =: [chosen]}\n"),
    ("nested-sequence-refused", "x: !!str {=: {=: [chosen]}, extra: ignored}\n"),
    ("anchor-octal-lexeme", "source: &number 0123\nx: !!str {extra: ignored, =: *number}\n"),
    ("anchor-bool-lexeme", "source: &boolean YES\nx: !!str {=: *boolean, extra: ignored}\n"),
    ("anchor-float-lexeme", "source: &float 1.00e+03\nx: !!str {=: *float, extra: ignored}\n"),
    ("anchor-null-lexeme", "source: &nil ~\nx: !!str {=: *nil, extra: ignored}\n"),
    ("anchor-nested-mapping", "source: &mapping {=: {=: 0123, extra: ignored}, extra: ignored}\nx: !!str {=: *mapping, extra: ignored}\n"),
    ("ignored-anchor-before-value", "x: !!str {extra: &number 0123, =: *number}\ny: *number\n"),
    ("ignored-anchor-after-value", "x: !!str {=: chosen, extra: &number 0123}\ny: *number\n"),
    ("ignored-collection-anchor", "x: !!str {=: chosen, extra: &collection {a: [b, c]}}\ny: *collection\n"),
    ("ignored-nested-anchor", "x: !!str {extra: {a: [&word chosen]}, =: *word}\ny: *word\n"),
    ("selected-anchor", "x: !!str {=: &word chosen, extra: ignored}\ny: *word\n"),
    ("wrapper-anchor", "x: &wrapper !!str {=: chosen, extra: ignored}\ny: *wrapper\n"),
    ("ignored-wrapper-anchor", "x: !!str {=: first, extra: &wrapper !!str {=: second, extra: ignored}}\ny: *wrapper\n"),
    ("selected-sequence-alias-refused", "source: &sequence [a]\nx: !!str {=: *sequence, extra: ignored}\n"),
    ("normal-value-key", "x: {=: chosen, extra: ignored}\n"),
    ("normal-explicit-value-key", "x: {!!value '=': chosen, extra: ignored}\n"),
    ("deferred-key-alias", "source: {&equals =: ignored}\nx: !!str {*equals : chosen, extra: ignored}\n"),
    ("wrapper-key-alias", "x: !!str {&equals =: chosen, extra: ignored}\ny: !!str {*equals : second, extra: ignored}\n"),
    ("retagged-root-key-alias", "&equals =: ignored\nx: !!str {*equals : chosen, extra: ignored}\n"),
    ("int-wrapper", "x: !!int {extra: ignored, =: 0123}\n"),
    ("float-wrapper", "x: !!float {=: 1.25e+03, extra: ignored}\n"),
    ("bool-wrapper", "x: !!bool {extra: ignored, =: YES}\n"),
    ("bool-wrapper-invalid", "x: !!bool {extra: ignored, =: invalid}\n"),
    ("null-wrapper", "x: !!null {extra: ignored, =: anything}\n"),
    ("binary-wrapper-padding", "x: !!binary {extra: ignored, =: abc}\n"),
    ("timestamp-wrapper-invalid", "x: !!timestamp {extra: ignored, =: 2026-13-01}\n"),
]

mode, binary, directory = sys.argv[1:]
evidence = Path(directory).resolve()
root = evidence / "wrappers"
root.mkdir(exist_ok=True)
if mode == "record":
    sys.path.insert(0, str(evidence / "python"))
    from agent.config import load_config

rows = []
for name, text in CASES:
    path = root / (name + ".yaml")
    path.write_text(text)
    if mode == "record":
        previous = dict(os.environ)
        os.environ.clear()
        log = io.StringIO()
        handler = logging.StreamHandler(log)
        logging.getLogger().addHandler(handler)
        try:
            out = json.dumps(load_config(str(path)).as_dict(), indent=2) + "\n"
            rc, err = 0, log.getvalue()
        except Exception as exc:
            rc, out = 1, ""
            err = log.getvalue() + "".join(traceback.format_exception_only(exc))
            (root / (name + ".traceback")).write_text("".join(traceback.format_exception(exc)))
        finally:
            logging.getLogger().removeHandler(handler)
            os.environ.clear()
            os.environ.update(previous)
    else:
        result = subprocess.run([binary, "config", str(path)], env={}, capture_output=True, timeout=5)
        rc, out, err = result.returncode, result.stdout.decode("utf-8"), result.stderr.decode("utf-8")
    rows.append(dict(case=name, input_hex=path.read_bytes().hex(), exit=rc, stdout=out, stderr=err))

fixture = evidence / "wrapper-python312.json"
if mode == "record":
    fixture.write_text(json.dumps(rows, indent=2) + "\n")
    print("recorded", len(rows), "original-loader wrapper cases")
else:
    expected = json.loads(fixture.read_text())
    differences = [dict(expected=a, actual=b) for a, b in zip(expected, rows, strict=True) if a != b]
    report = dict(total=len(rows), passed=len(rows) - len(differences), differences=differences)
    (evidence / "wrapper-replay.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    sys.exit(bool(differences))
