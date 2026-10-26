"""Generate synthetic TSV vectors with the actual services Python cut implementation.
Usage: python3 generate_fixtures.py /path/to/diary/agent/score_gap.py
Reference: services 6c1eb5891c75b6fb9f04c4484ba39da80a5b4b9e.
"""
import itertools
import pathlib
import random
import runpy
import sys

cut = runpy.run_path(sys.argv[1])["cut"]

def token(value):
    return "missing" if value is None else repr(value)

cases = [
    ([0.8, 0.725, 0.72, 0.7199, 0.6, None], 0.08),
    ([0.1, 0.9, 0.82, 0.8199999995, 0.819999998], 0.08),
    ([float("nan"), float("inf"), -float("inf"), None], 0.08),
    ([-0.8, -0.7, -0.75, -0.9], 0.08),
    ([-0.0, 0.0], None),
    ([0.0, -0.0], None),
    ([1e308, -1e308], 0.08),
]
for scores in ([], [0.9, 0.1], [None], [0.0, 0.0], [float("nan"), 0.9, None, 0.1]):
    for gap in (None, 0.08, 1.0):
        cases.append((scores, gap))
# Exhaustive small lists plus deterministic longer permutations.
for scores in itertools.product([None, -0.1, 0.0, 0.08, 0.080000002], repeat=3):
    for gap in (None, 0.08):
        cases.append((list(scores), gap))
rng = random.Random(1071)
for _ in range(80):
    scores = [rng.choice([None, float("nan"), float("inf"), rng.uniform(-1, 1)]) for _ in range(rng.randrange(33))]
    cases.append((scores, rng.choice([None, 0.001, 0.08, 1.0])))
lines = ["# gap\tscores\tkept positions\tremoved positions\tbest; actual Python score_gap.cut"]
for scores, gap in cases:
    ranked = [(i, {"score": score}) for i, score in enumerate(scores)]
    kept, removed, best = cut(ranked, gap)
    lines.append("\t".join([token(gap), ",".join(map(token, scores)),
        ",".join(str(i) for i, _ in kept), ",".join(str(i) for i, _ in removed), token(best)]))
pathlib.Path(__file__).with_name("python-cut.tsv").write_text("\n".join(lines) + "\n")
