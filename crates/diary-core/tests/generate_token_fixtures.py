"""Regenerate token-estimate-fixtures.json from the frozen Python estimate_tokens (diary/agent/util.py).

Usage (stdlib only, isolated mode):

    python3 -I crates/diary-core/tests/generate_token_fixtures.py <path-to>/diary/agent/util.py

The script checks the file's sha256 against PINNED_SHA256 (noevia-services origin/main at the R9
head start), then executes only the `estimate_tokens` function from it (parsed with ast, so the
module's imports never run). All inputs are synthetic.
"""

import ast
import hashlib
import json
import pathlib
import sys

PINNED_SHA256 = "f1cfedf68937724331989b9083e2fff661b0c8b09db86659f32e41803ea4dfe9"
OUT = pathlib.Path(__file__).with_name("token-estimate-fixtures.json")


def load_function(path):
    data = pathlib.Path(path).read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    if digest != PINNED_SHA256:
        sys.exit(f"refusing {path}: sha256 {digest} is not the pinned {PINNED_SHA256}")
    tree = ast.parse(data.decode("utf-8"), filename=path)
    wanted = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "estimate_tokens"]
    if len(wanted) != 1:
        sys.exit("expected estimate_tokens in the pinned file")
    namespace = {}
    exec(compile(ast.Module(body=wanted, type_ignores=[]), path, "exec"), namespace)
    return namespace["estimate_tokens"]


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: generate_token_fixtures.py <diary/agent/util.py>")
    estimate = load_function(sys.argv[1])
    texts = [""] + ["x" * n for n in list(range(1, 80)) + [359, 360, 361, 3599, 3600, 3601]]
    texts += ["é" * 36, "日本語文", "😀" * 36, "😀" * 37, "á" * 18, "\u0000" * 4, " " * 7, "\n" * 8, "x😀" * 25]
    cases = [{"text": t, "tokens": estimate(t)} for t in texts]
    OUT.write_text(json.dumps({"source_sha256": PINNED_SHA256, "cases": cases}, ensure_ascii=False, indent=1) + "\n",
                   encoding="utf-8")
    print(f"wrote {OUT} ({len(cases)} cases)")


if __name__ == "__main__":
    main()
