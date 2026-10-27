#!/usr/bin/env python3
"""Generate synthetic exact-output fixtures against a supplied shipped corpus.py."""
from __future__ import annotations
import argparse
import hashlib
import importlib.util
import json
import random
import sys
from pathlib import Path

EXPECTED_SOURCE_SHA256 = "4b8809d3b7d4811723404cbc9a40fe4fe0743bc18bbb449a673ec711c48e0a87"


def load_source(source: Path):
    digest = hashlib.sha256(source.read_bytes()).hexdigest()
    if digest != EXPECTED_SOURCE_SHA256:
        raise SystemExit(f"source hash mismatch: {digest}")
    spec = importlib.util.spec_from_file_location("diary_index_source", source)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def state(idx):
    return {"preamble": idx.preamble.copy(), "month_links": idx.month_links.copy(),
            "sections": [(name, bullets.copy()) for name, bullets in idx.sections.items()]}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path(__file__).with_name("index-fixtures.jsonl"))
    args = parser.parse_args()
    fmt = load_source(args.source)
    rnd = random.Random(3011071)
    names = ["Should I switch jobs?", "Should I switch jobs? (for a friend)",
             "Plan A", "plan a.", "İstanbul project", "ΟΣ", "mañana", "Café",
             "次の予定", "review—one", "a*b", "", "   ", "...", "Καφές"]
    bullets = ["- [ ] Should I switch jobs?", "-[  ] Plan A", "-\t[ ] plan a.",
               " - [x] Café", "* Plan A", "<!-- hidden -->", "- **2026-01-02** — Plan A",
               "- [ ] Should I switch jobs? (for a friend)", "-\u2003[ ]\tΟΣ", " - plain"]
    lines = ["Preamble", "\tPreamble  ", "## Months", "## Open Questions",
             "## Timeline of Key Events", "## Custom", "## months", "##  Odd title  ",
             "- [January 2026](2026-01.md)", " \t- [Feb](2026-02.md)  ",
             "- [Bad](not-a-month.txt)", "_None yet._", "", "random text", *bullets]
    docs = [None, "", "Preamble", "- [ ] Should I switch jobs?\n",
            "## Months\n- [Jan](2026-01.md)\n## Open Questions\n- [ ] Plan A\n",
            "## Open Questions\r\n-[  ] Plan A\r\n## Timeline of Key Events\r- **2026-09-01** — Plan A\r",
            "\u2028## Months\u2028- [Jan](2026-01.md)\u2029## Custom\u2029<!-- hidden -->",
            "##  \n- [ ] hidden\n## Open Questions\n- [ ] Café\n"]
    for _ in range(550):
        delimiter = rnd.choice(["\n", "\r\n", "\r", "\u2028", "\u0085"])
        docs.append(delimiter.join(rnd.choices(lines, k=rnd.randrange(0, 26))))
    for prefix in ["-", " - ", "\u2003-\t", "* "]:
        for label in ["A", "A]B", "", "ΟΣ", "a)b"]:
            for file in ["a.md", "a.md.md", "a)q.md", ".md", "a.txt"]:
                docs.append("## Months\n" + prefix + "[" + label + "](" + file + ")  \n")
    for head in ["##  ", "## \t", "##\u2003A  ", "## A", "## ", "##\u00a0Months", "## months  "]:
        docs.append(head + "\n- [ ] question\n")
    cases = []
    for i, doc in enumerate(docs):
        idx = fmt.parse_index(doc)
        before = state(idx)
        label = rnd.choice(["January 2026", "Feb", "ΟΣ", "İstanbul"])
        filename = rnd.choice(["2026-01.md", "2026-02.md", "x.md", "x.md)tail"])
        registered = fmt.register_month(idx, label, filename)
        after_register = state(idx)
        qops = []
        tops = []
        for target in (qops, tops):
            for _ in range(rnd.randrange(0, 5)):
                action = rnd.choice(["add", "resolve", "edit", "noop"]) if target is qops else rnd.choice(["add", "edit", "noop"])
                op = {"action": action, "text": rnd.choice(names)}
                if rnd.randrange(3) == 0:
                    op["replacement"] = rnd.choice([None, "replacement", "", "ΟΣ", "new plan"])
                if rnd.randrange(2) == 0:
                    op["date"] = rnd.choice(["2026-09-01", "", "now", "\u2003"])
                target.append(op)
        changed = fmt.apply_index_edits(idx, qops, tops, today="2026-09-01")
        cases.append({"id": i, "text": doc, "before": before, "label": label,
                      "filename": filename, "registered": registered,
                      "after_register": after_register, "questions": qops,
                      "timeline": tops, "today": "2026-09-01", "changed": changed,
                      "after": state(idx), "render": idx.render()})
    helpers = []
    helper_inputs = [
        (["- [ ] Should I switch jobs?", "- [ ] Should I switch jobs? (for a friend)"], "Should I switch jobs?"),
        (["-[  ] Plan A", "-\t[ ] Plan A", "- [x] Plan A"], "Plan A"),
        (["-\u2003[ ]\tΟΣ", "* ΟΣ"], "ΟΣ"),
        (["- [ ] Café", "- [ ] Café"], "Café"),
        (["- [ ] ΟΣ", "- [ ] ΟΣ detail"], "ος"),
        (["- [ ] ΟΣ", "- [ ] ΟΣ detail"], "οσ"),
        (["- [ ] ΣΣ", "- [ ] ΣΣ detail"], "σς"),
        (["- [ ] AΣ́", "- [ ] AΣ́ detail"], "aς́"),
    ]
    helper_inputs.extend((rnd.choices(bullets, k=rnd.randrange(0, 9)), rnd.choice(names)) for _ in range(350))
    for i, (bs, needle) in enumerate(helper_inputs):
        helpers.append({"id": i, "bullets": bs, "text": needle,
                        "indices": sorted(fmt._bullet_match_indices(bs, needle)),
                        "stripped": [fmt._bullet_text(b) for b in bs],
                        "checked": [fmt._check_bullet(b, True) for b in bs]})
    rows = [{"kind": "meta", "source_sha256": EXPECTED_SOURCE_SHA256,
             "python": sys.version.split()[0]}]
    rows.extend({"kind": "index", **case} for case in cases)
    rows.extend({"kind": "helper", **case} for case in helpers)
    args.output.write_text("\n".join(json.dumps(row, ensure_ascii=False, separators=(",", ":"))
                                     for row in rows) + "\n")
    print(f"generated {len(cases)} index and {len(helpers)} bullet fixtures")


if __name__ == "__main__":
    main()
