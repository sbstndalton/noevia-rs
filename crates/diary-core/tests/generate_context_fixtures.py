"""Regenerate completed-exchanges-fixtures.json from the frozen Python context module.

Usage (stdlib only, isolated mode):

    python3 -I crates/diary-core/tests/generate_context_fixtures.py <path-to>/diary/agent/context.py

`diary/agent/context.py` uses package-relative imports, so it cannot be imported on its own. This
script checks the file's sha256 against PINNED_SHA256, then executes only the `completed_exchanges`
function and the `REPLY_ROLES` constant taken from its AST. All turns are synthetic.
"""

import ast
import hashlib
import json
import pathlib
import sys
from typing import List, Optional

PINNED_SHA256 = "b972d8d2af8ed51cf46068d5affe002d6992ac36be0791e942aa9fd9473e428f"
OUT = pathlib.Path(__file__).with_name("completed-exchanges-fixtures.json")


def load_function(path):
    data = pathlib.Path(path).read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    if digest != PINNED_SHA256:
        sys.exit(f"refusing {path}: sha256 {digest} is not the pinned {PINNED_SHA256}")
    tree = ast.parse(data.decode("utf-8"), filename=path)
    namespace = {"List": List, "Optional": Optional}
    wanted = []
    for node in tree.body:
        if isinstance(node, ast.FunctionDef) and node.name == "completed_exchanges":
            wanted.append(node)
        elif isinstance(node, ast.Assign) and any(
            isinstance(t, ast.Name) and t.id == "REPLY_ROLES" for t in node.targets
        ):
            wanted.append(node)
    if len(wanted) != 2:
        sys.exit("expected REPLY_ROLES and completed_exchanges in the pinned file")
    module = ast.Module(body=wanted, type_ignores=[])
    exec(compile(module, path, "exec"), namespace)
    return namespace["completed_exchanges"]


TURNS = [
    {"role": "user", "content": "What did I log on Monday?"},
    {"role": "companion", "content": "You logged a walk."},
    {"role": "user", "content": "Unanswered question"},
    {"role": "user", "content": "Replaced question"},
    {"role": "assistant", "content": "Answer to the replaced one"},
    {"role": "user", "content": "   "},
    {"role": "assistant", "content": "reply to a blank question"},
    {"role": "user", "content": "Question with blank reply"},
    {"role": "companion", "content": "   "},
    {"role": "companion", "content": "orphan reply with no question"},
    {"role": "system", "content": "system line"},
    {"role": "user", "content": "padded"},
    {"role": "assistant", "content": "  padded reply  "},
    {"role": "user", "content": "\u001c"},
    {"role": "assistant", "content": "reply after control-only question"},
    {"role": "user", "content": "Question with non-string reply"},
    {"role": "assistant", "content": ["not", "a", "string"]},
    {"role": "user", "content": 42},
    {"role": "assistant", "content": "reply to non-string question"},
    "not a dict",
    None,
    42,
    {"content": "no role key"},
    {"role": "user"},
    {"role": "assistant", "content": "reply to a turn with no content key"},
    {"role": ["user"], "content": "list role never matches"},
    {"role": "user", "content": "Final question"},
    {"role": "assistant", "content": "Final answer"},
    {"role": "user", "content": "Trailing question with no reply"},
]

CASES = {
    "mixed_conversation": TURNS,
    "empty_list": [],
    "none_input": None,
    "only_user_turns": [{"role": "user", "content": "a"}, {"role": "user", "content": "b"}],
    "reply_without_question": [{"role": "assistant", "content": "x"}],
    "question_then_blank_question": [
        {"role": "user", "content": "a"},
        {"role": "user", "content": ""},
        {"role": "companion", "content": "x"},
    ],
    "non_list_input": {"role": "user", "content": "a"},
}


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    completed = load_function(sys.argv[1])
    cases = []
    for name, turns in CASES.items():
        # The Python function accepts Optional[List]; a dict input is iterated by key in Python.
        # Only the list and None cases are meaningful for the API; the dict case is kept to pin
        # what Python does with it (no keys survive the isinstance check).
        turns_arg = turns
        expected = completed(turns_arg)
        cases.append({"name": name, "turns": turns, "expected": expected})
    OUT.write_text(json.dumps({"source_sha256": PINNED_SHA256, "cases": cases}, ensure_ascii=False,
                              indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {OUT} ({len(cases)} cases)")


if __name__ == "__main__":
    main()
