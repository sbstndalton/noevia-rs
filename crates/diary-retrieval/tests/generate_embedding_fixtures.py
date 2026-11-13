"""Record embedding-fixtures.json from the frozen Python module diary/agent/embedding.py.

Usage (CPython 3.12 only, the Diary image's interpreter; the venv needs the Diary requirements):

    <venv>/bin/python -I crates/diary-retrieval/tests/generate_embedding_fixtures.py <noevia-services>/diary

The script refuses any other source: the sha256 of agent/embedding.py and agent/llm.py (which
supplies `InputTooLarge`) must match PINNED. Every text, title, model id, host name and path
here is synthetic. Floats are recorded as the 16 hex digits of their IEEE-754 big-endian bytes,
so no decimal round trip can hide a last-bit difference. Each section is a list of cases with
the Python answer next to the inputs; the Rust differential test (`embedding_differential.rs`)
replays every one.
"""

import hashlib
import ipaddress
import json
import logging
import os
import pathlib
import random
import re
import struct
import sys
import tempfile
import unicodedata

sys.dont_write_bytecode = True

PINNED = {
    "agent/embedding.py": "54a99e73ddc4d9987729fd245c3981c23a90227407c5e31bcf74519429093d1c",
    "agent/llm.py": "cd18536747b405221b6a3a2dba67412ee1dec172d304f9cb5bde846b36d1b3a2",
}
OUT = pathlib.Path(__file__).with_name("embedding-fixtures.json")


def load(diary_dir):
    root = pathlib.Path(diary_dir).resolve()
    for rel, want in PINNED.items():
        got = hashlib.sha256((root / rel).read_bytes()).hexdigest()
        if got != want:
            sys.exit(f"refusing {rel}: sha256 {got} is not the pinned {want}")
    sys.path.insert(0, str(root))
    logging.getLogger("agent.embedding").addHandler(logging.NullHandler())  # no stderr noise
    from agent import embedding  # noqa: E402
    from agent.config import Config  # noqa: E402
    from agent.llm import InputTooLarge  # noqa: E402
    return embedding, Config, InputTooLarge


emb, Config, InputTooLarge = None, None, None


def fl(x):
    """A float as the hex of its big-endian IEEE-754 bytes."""
    return struct.pack(">d", x).hex()


def err(exc):
    return {"message": str(exc), "code": exc.code, "status": exc.status}


def ok_or_err(fn):
    try:
        return {"ok": fn()}
    except emb.EmbeddingConfigError as exc:
        return {"error": err(exc)}
    except IndexError:
        return {"index_error": True}


# ---- string classes -----------------------------------------------------------------------

def ranges(codepoints):
    out, start, prev = [], None, None
    for c in codepoints:
        if start is None:
            start = prev = c
        elif c == prev + 1:
            prev = c
        else:
            out.append([start, prev])
            start = prev = c
    if start is not None:
        out.append([start, prev])
    return out


def whitespace_section():
    isspace = [c for c in range(0x110000) if chr(c).isspace()]
    re_s = [c for c in range(0x110000) if re.match(r"\s", chr(c))]
    assert isspace == re_s, "str.isspace and re \\s differ"
    nfkc = [c for c in range(0x80, 0x110000)
            if not 0xD800 <= c <= 0xDFFF
            and any(x in unicodedata.normalize("NFKC", chr(c)) for x in "/?#@:")]
    return {"space_ranges": ranges(isspace), "nfkc_netloc_chars": nfkc,
            "unicode_version": unicodedata.unidata_version}


# ---- synthetic text -----------------------------------------------------------------------

WORDS = ("orchard blossom canal survey lantern harbour ferry meadow granite ledger quiet "
         "amber willow copper tide market bridge garden station lamp river clock pebble "
         "violet hearth ridge meridian atlas ember thistle marble harvest signal").split()
UNI_WORDS = ["café", "naïve", "über", "日本語", "🌿plant", "éclair", "𝔘nicode", "ångström",
             "тест", "مرحبا", "a​b", "x y"]
TITLE_POOL = [
    None, "", "  ", "Plain title", "A | B", "line1\nline2", "tabs\t\tand   spaces", " nbsp em",
    "é accents", "🌿 astral", "x" * 200, "pipe||||run", "\x1c\x1d sep", "\r\n\r\n", "|", "title|\n|",
    " x y", "\u0085", "none", "a b " * 100, "🌿" * 170, "end 　", "  lead and trail  ",
    "mixed \r\n | \t | \n x", "Ünïcödé Títle", "\x0b\x0cform", "none | none",
]


def rand_text(rng, kind, n_chars):
    """Seeded synthetic text of roughly n_chars code points."""
    def word():
        return rng.choice(UNI_WORDS) if rng.random() < 0.08 else rng.choice(WORDS)
    out, size = [], 0
    while size < n_chars:
        if kind == "paragraphs":
            piece = " ".join(word() for _ in range(rng.randint(4, 40))) + ".\n\n"
        elif kind == "lines":
            piece = " ".join(word() for _ in range(rng.randint(2, 14))) + "\n"
        elif kind == "sentences":
            piece = " ".join(word() for _ in range(rng.randint(3, 12))) + ". "
        elif kind == "words":
            piece = word() + " "
        elif kind == "mixed":
            piece = word() + rng.choice([" ", " ", " ", ". ", "\n", "\n\n", "  ", "\t", ".\n"])
        elif kind == "long_token":
            piece = "".join(rng.choice("abcdefghij0123456789") for _ in range(rng.randint(50, 400)))
        elif kind == "whitespace":
            piece = word() + rng.choice(["   ", "\n\n\n\n", " \n ", "　", "  "])
        else:  # nosep
            piece = "".join(rng.choice("abcdefé\U0001F33F") for _ in range(rng.randint(20, 200)))
        out.append(piece)
        size += len(piece)
    return "".join(out)[:n_chars]


# ---- frame / clean_title ------------------------------------------------------------------

def frame_section():
    rng = random.Random(1636)
    titles = []
    for t in TITLE_POOL:
        titles.append({"title": t, "expected": emb.clean_title(t)})
    alphabet = ["a", "b", " ", "\n", "\r", "|", "\t", " ", " ", "　", "\x1c", "é", "🌿", "́", "x"]
    for _ in range(45):
        t = "".join(rng.choice(alphabet) for _ in range(rng.randint(0, 220)))
        titles.append({"title": t, "expected": emb.clean_title(t)})
    cases = []
    text_pool = ["", "a query about orchards", "Line one\nLine two", "🌿 astral text", "café", "x" * 300,
                 "pipe | in | text", "  padded  ", " nbsp"]
    profiles = list(emb.PROFILES) * 4 + ["", "unknown-1", "Nomic-V1", "nomic-v1 ", "embeddinggemma-3", 'q"uote']
    kinds = list(emb.KINDS) * 3 + ["Query", "", "doc", "documents", "similarity "]
    for i in range(130):
        profile = rng.choice(profiles)
        texts = [rng.choice(text_pool) for _ in range(rng.randint(0, 4))]
        mode = rng.random()
        if mode < 0.45:
            kind = rng.choice(kinds)
        else:
            kind = [rng.choice(kinds if rng.random() < 0.15 else list(emb.KINDS)) for _ in range(len(texts))]
            if kind and rng.random() < 0.1:
                kind = kind[:-1]  # shorter than the texts: IndexError in Python
        tmode = rng.random()
        if tmode < 0.4:
            tl = None
        elif tmode < 0.5:
            tl = []
        else:
            tl = [rng.choice(TITLE_POOL) for _ in range(len(texts) + (rng.choice([0, 0, 0, -1]) if texts else 0))]
        case = {"profile": profile, "texts": texts, "kind": kind, "titles": tl}
        case["expected"] = ok_or_err(lambda: emb.frame(profile, texts, kind, tl))
        cases.append(case)
    # fixed cases: the model card's prompts, exact
    for profile in emb.PROFILES:
        for kind in emb.KINDS:
            texts = ["alpha | beta", "gamma"]
            tl = ["T | one\n", None]
            cases.append({"profile": profile, "texts": texts, "kind": kind, "titles": tl,
                          "expected": ok_or_err(lambda: emb.frame(profile, texts, kind, tl))})
    return {"clean_title": titles, "frame": cases}


# ---- split_text / mean / embed_fitting ----------------------------------------------------

def split_section():
    rng = random.Random(1759)
    kinds = ["paragraphs", "lines", "sentences", "words", "mixed", "long_token", "whitespace", "nosep"]
    cases = []
    for i in range(140):
        kind = kinds[i % len(kinds)]
        n = rng.choice([0, 1, 2, 5, 63, 64, 65, 120, 300, 800, 1500, 2500, 4000])
        n = max(0, n + rng.randint(-3, 3)) if n else 0
        text = rand_text(rng, kind, n) if n else ""
        parts = rng.choice([2, 2, 3, 4, 5, 7, 8, 12, 16, 24, 32, 40, rng.randint(2, 40), 1, 0, -3])
        pieces = emb.split_text(text, parts)
        cases.append({"text": text, "parts": parts, "pieces": pieces})
    return cases


def mean_section():
    rng = random.Random(42)
    cases = []

    def add(vectors, weights):
        try:
            result = {"ok": [fl(x) for x in emb._mean(vectors, weights)]}
        except IndexError:
            result = {"index_error": True}
        cases.append({"vectors": [[fl(x) for x in v] for v in vectors], "weights": weights, "expected": result})

    for _ in range(36):
        dim = rng.randint(1, 12)
        n = rng.randint(1, 8)
        scale = rng.choice([1e-3, 1.0, 1e3, 1e8])
        vectors = [[rng.uniform(-1, 1) * scale for _ in range(dim)] for _ in range(n)]
        weights = [rng.randint(1, 4000) for _ in range(n)]
        add(vectors, weights)
    # sums where Neumaier compensation differs from a plain loop
    add([[1e16, 3.0], [1.0, 1e-16], [-1e16, 2.0]], [1, 1, 1])
    add([[0.1] * 5, [0.2] * 5, [0.3] * 5, [-0.6] * 5], [1, 1, 1, 1])
    add([[1e100, 1.0], [1.0, 1e100], [-1e100, -1e100]], [3, 5, 7])
    add([[(-1) ** i * 10.0 ** (i % 7) for i in range(9)] for _ in range(6)], [64, 65, 66, 67, 68, 69])
    add([[0.0, 0.0], [0.0, 0.0]], [5, 7])                 # zero norm: returned unnormalised
    add([[1.0, 2.0], [3.0, 4.0]], [0, 0])                 # weights sum to zero: total is 1.0
    add([[1.0, 2.0, 3.0], [3.0, 4.0]], [1, 1])            # second vector shorter: IndexError
    add([[1.0, 2.0], [3.0, 4.0, 5.0]], [1, 1])            # first vector shorter: truncated
    add([[1.0, 2.0]], [10])
    add([], [])                                           # no vectors: IndexError
    add([[float("inf"), 1.0], [1.0, 1.0]], [1, 1])
    add([[float("nan"), 1.0], [1.0, 1.0]], [1, 1])
    add([[-0.0, -0.0], [-0.0, -0.0]], [1, 1])
    # square roots where pow(x, 0.5), which Python uses, and sqrt(x) differ in the last bit; and sums
    # where the compensated sum differs from a plain left-to-right loop
    import math

    def averaged(vectors, weights):
        total = float(sum(weights)) or 1.0
        return [sum(v[j] * w for v, w in zip(vectors, weights)) / total for j in range(len(vectors[0]))]

    def plain_averaged(vectors, weights):
        total = float(sum(weights)) or 1.0
        out = []
        for j in range(len(vectors[0])):
            acc = 0.0
            for v, w in zip(vectors, weights):
                acc += v[j] * w
            out.append(acc / total)
        return out
    found_pow = found_sum = 0
    while found_pow < 8 or found_sum < 8:
        vectors = [[rng.random() * rng.choice([1e-3, 1.0, 1e3]) for _ in range(rng.randint(2, 8))]
                   for _ in range(rng.randint(2, 6))]
        width = min(len(v) for v in vectors)
        vectors = [v[:width] for v in vectors]
        weights = [rng.randint(1, 3000) for _ in vectors]
        out = averaged(vectors, weights)
        sq = sum(x * x for x in out)
        if found_pow < 8 and sq ** 0.5 != math.sqrt(sq):
            add(vectors, weights)
            found_pow += 1
        elif found_sum < 8 and out != plain_averaged(vectors, weights):
            add(vectors, weights)
            found_sum += 1
    return cases


def fake_vec(text):
    h = hashlib.sha256(text.encode("utf-8")).digest()
    return [b / 255.0 for b in h[:6]]


def sha12(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:12]


class FakeSend:
    """The embeddings server: refuses an over-long framed text (or batch) as too large, optionally
    saying how many tokens, and fails with another error for a marked text."""

    def __init__(self, spec):
        self.spec = spec
        self.calls = []

    def __call__(self, texts, kinds, titles):
        self.calls.append({"texts": [[len(t), sha12(t)] for t in texts], "kinds": list(kinds),
                           "titles": list(titles)})
        framed = emb.frame(self.spec["profile"], texts, kinds, titles)
        marker = self.spec.get("other_marker")
        if marker and any(marker in t for t in texts):
            raise RuntimeError("synthetic server failure")
        size = cap = None
        longest = max((len(f) for f in framed), default=0)
        if self.spec.get("max_chars") is not None and longest > self.spec["max_chars"]:
            size, cap = longest, self.spec["max_chars"]
        elif self.spec.get("batch_chars") is not None and sum(len(f) for f in framed) > self.spec["batch_chars"]:
            size, cap = sum(len(f) for f in framed), self.spec["batch_chars"]
        if size is not None:
            report = self.spec.get("report", "none")
            if report == "chars3":
                raise InputTooLarge("synthetic too large", size // 3, cap // 3)
            if report == "fixed":
                raise InputTooLarge("synthetic too large", self.spec.get("tokens"), self.spec.get("limit"))
            raise InputTooLarge("synthetic too large")
        return [fake_vec(f) for f in framed]


def fitting_section():
    rng = random.Random(1766)
    cases = []

    def add(spec, texts, kind, titles):
        send = FakeSend(spec)
        try:
            result = {"ok": [[fl(x) for x in v] for v in emb.embed_fitting(send, texts, kind, titles)]}
        except InputTooLarge as exc:
            result = {"error": {"kind": "too_large", "tokens": exc.tokens, "limit": exc.limit}}
        except IndexError:
            result = {"index_error": True}
        except emb.EmbeddingConfigError as exc:
            result = {"error": {"kind": "other", "message": str(exc), "code": exc.code}}
        except RuntimeError as exc:
            result = {"error": {"kind": "other", "message": str(exc)}}
        cases.append({"spec": spec, "texts": texts, "kind": kind, "titles": titles,
                      "expected": result, "calls": send.calls})

    def doc(kind, n):
        return rand_text(rng, kind, n)

    # fits at once: sent once, exactly as given
    add({"profile": "nomic-v1"}, ["alpha", "beta"], "document", None)
    add({"profile": "embeddinggemma-2"}, ["alpha", "beta"], ["query", "document"], [None, "T | x"])
    add({"profile": "nomic-v1"}, [], "document", None)
    # one over-long document, with and without token counts
    for report, mx in [("chars3", 400), ("none", 400), ("chars3", 150), ("none", 90)]:
        for prof in ("nomic-v1", "embeddinggemma-2"):
            for k in ("paragraphs", "mixed", "long_token"):
                add({"profile": prof, "max_chars": mx, "report": report}, [doc(k, rng.randint(600, 3000))],
                    "document", ["Title | one"] if prof != "nomic-v1" else None)
    # a batch refused as a whole: bisected, accepted halves go out once
    for prof in ("nomic-v1", "embeddinggemma-2"):
        for n in (2, 3, 5, 8):
            texts = [doc("mixed", 80 if i % 3 else 1500) for i in range(n)]
            add({"profile": prof, "max_chars": 500, "report": "chars3"}, texts, "document",
                [f"t{i}" for i in range(n)] if prof != "nomic-v1" else None)
    add({"profile": "nomic-v1", "batch_chars": 600, "report": "none"}, [doc("words", 200) for _ in range(6)],
        "document", None)
    # kinds: a refused query is raised unchanged; mixed kinds in one batch
    add({"profile": "embeddinggemma-2", "max_chars": 200, "report": "chars3"}, [doc("words", 900)], "query", None)
    add({"profile": "embeddinggemma-2", "max_chars": 300, "report": "chars3"},
        [doc("words", 100), doc("paragraphs", 900), doc("words", 100)], ["query", "document", "similarity"],
        [None, "T", None])
    # a text shorter than SPLIT_MIN_CHARS is a real refusal; depth exhaustion
    add({"profile": "nomic-v1", "max_chars": 30, "report": "chars3"}, [doc("words", 60)], "document", None)
    add({"profile": "nomic-v1", "max_chars": 40, "report": "none"}, [doc("words", 5000)], "document", None)
    add({"profile": "nomic-v1", "max_chars": 40, "report": "chars3"}, [doc("sentences", 4000)], "document", None)
    # reported token counts of every odd shape
    for tokens, limit in [(0, 100), (500, 0), (None, 100), (500, None), (-500, 100), (500, -100), (10 ** 6, 100),
                          (1000, 1), (7, 10), (10 ** 12, 10 ** 12), (100, 10 ** 15), (33, 3), (97, 100), (2, 2)]:
        add({"profile": "nomic-v1", "max_chars": 300, "report": "fixed", "tokens": tokens, "limit": limit},
            [doc("mixed", 1200)], "document", None)
    # another error is raised unchanged, in the first call and in a later piece
    add({"profile": "nomic-v1", "other_marker": "qqq"}, ["a qqq b"], "document", None)
    marked = doc("lines", 1800) + " qqq " + doc("lines", 300)
    add({"profile": "nomic-v1", "max_chars": 400, "report": "chars3", "other_marker": "qqq"}, [marked], "document", None)
    add({"profile": "nomic-v1", "max_chars": 400, "report": "chars3", "other_marker": "qqq"},
        [doc("lines", 100), marked, doc("lines", 100)], "document", None)
    # fail closed on kind inside the send (frame raises)
    add({"profile": "nomic-v1"}, ["alpha"], "bogus", None)
    # argument lengths
    add({"profile": "nomic-v1"}, ["a", "b"], ["document"], None)
    add({"profile": "embeddinggemma-2"}, ["a", "b"], "document", ["only-one"])
    # unicode and astral text
    add({"profile": "embeddinggemma-2", "max_chars": 200, "report": "chars3"},
        [("🌿 café é́ " * 120)], "document", ["Ünï"])
    # random mix
    for _ in range(14):
        n = rng.randint(1, 5)
        spec = {"profile": rng.choice(emb.PROFILES), "max_chars": rng.choice([120, 250, 600, None]),
                "batch_chars": rng.choice([None, 900]), "report": rng.choice(["none", "chars3"])}
        texts = [doc(rng.choice(["paragraphs", "mixed", "sentences", "words"]), rng.choice([40, 200, 900, 2200]))
                 for _ in range(n)]
        add(spec, texts, "document", None if spec["profile"] == "nomic-v1" else [f"t{i}" for i in range(n)])
    return cases


# ---- targets ------------------------------------------------------------------------------

def target_cases():
    long_ok = "m" * 200
    raws = [
        {"model": "qa-embed-a"},
        {"model": "qa-embed-a", "profile": "embeddinggemma-2"},
        {"model": "qa-embed-a", "profile": "nomic-v1", "base_url": "http://qa-embed.invalid:8080"},
        {"model": "qa-embed-a", "baseUrl": "https://qa-embed.invalid/v1/"},
        {"model": "qa-embed-a", "base_url": None, "baseUrl": "http://x.invalid"},
        {"model": "qa-embed-a", "base_url": "", "baseUrl": "http://x.invalid"},
        {"model": "qa-embed-a", "base_url": "http://a.invalid", "baseUrl": "ftp://bad"},
        {"model": "  qa-embed-a  "},
        {"model": "qa-embed-a\n"},
        {"model": "\tqa-embed-a\r\n"},
        {"model": "qa-embed-a "},
        {"model": "qa-èmbed-c"},
        {"model": "qa-\U0001F33F-embed"},
        {"model": long_ok},
        {"model": long_ok + "x"},
        {"model": "\U0001F33F" * 200},
        {"model": "\U0001F33F" * 201},
        {"model": "qa embed"},
        {"model": "qa\x1fembed"},
        {"model": "qa\x00embed"},
        {"model": "qa\x7fembed"},
        {"model": "qa​embed"},
        {"model": "qa embed"},
        {"model": "\x1c\x1dqa-embed-a\x1e"},
        {"model": ""},
        {"model": None},
        {"model": "   "},
        {},
        {"profile": "nomic-v1"},
        {"model": 5},
        {"model": 0},
        {"model": True},
        {"model": False},
        {"model": 1.5},
        {"model": 1e22},
        {"model": 1e-7},
        {"model": 100000.0},
        {"model": 1e16},
        {"model": 123456789.125},
        {"model": -2.5},
        {"model": []},
        {"model": ["a"]},
        {"model": [1]},
        {"model": [1, 2]},
        {"model": [[]]},
        {"model": [None]},
        {"model": [True]},
        {"model": [1.5]},
        {"model": ["a\nb"]},
        {"model": ["it's"]},
        {"model": ["say \"hi\""]},
        {"model": [{}]},
        {"model": {}},
        {"model": {"a": 1}},
        {"model": "qa-embed-a", "profile": ""},
        {"model": "qa-embed-a", "profile": None},
        {"model": "qa-embed-a", "profile": "  embeddinggemma-2\n"},
        {"model": "qa-embed-a", "profile": "unknown"},
        {"model": "qa-embed-a", "profile": "Nomic-V1"},
        {"model": "qa-embed-a", "profile": 5},
        {"model": "qa-embed-a", "profile": True},
        {"model": "qa-embed-a", "profile": False},
        {"model": "qa-embed-a", "profile": 0},
        {"model": "qa-embed-a", "profile": []},
        {"model": "qa-embed-a", "profile": ["x"]},
        {"model": "qa-embed-a", "profile": {"a": 1}},
        {"model": "qa-embed-a", "profile": 1.25},
        {"model": "qa-embed-a", "profile": "   "},
        {"model": "qa-embed-a", "profile": "é"},
        {"model": "qa-embed-a", "base_url": " http://h.invalid "},
        {"model": "qa-embed-a", "base_url": "http://h.invalid///"},
        {"model": "qa-embed-a", "base_url": "http://h.invalid:80/a/b/"},
        {"model": "qa-embed-a", "base_url": "ftp://h.invalid"},
        {"model": "qa-embed-a", "base_url": "http://u:p@h.invalid"},
        {"model": "qa-embed-a", "base_url": "http://u@h.invalid"},
        {"model": "qa-embed-a", "base_url": "http://h.invalid?x=1"},
        {"model": "qa-embed-a", "base_url": "http://h.invalid/a?x"},
        {"model": "qa-embed-a", "base_url": "http://h.invalid#f"},
        {"model": "qa-embed-a", "base_url": "http://h.invalid/a b"},
        {"model": "qa-embed-a", "base_url": "http://h .invalid"},
        {"model": "qa-embed-a", "base_url": "HTTP://h.invalid"},
        {"model": "qa-embed-a", "base_url": "http:///x"},
        {"model": "qa-embed-a", "base_url": "http://"},
        {"model": "qa-embed-a", "base_url": "https://"},
        {"model": "qa-embed-a", "base_url": "http://[::1]:8080/v1"},
        {"model": "qa-embed-a", "base_url": "http://h.invalid "},
        {"model": "qa-embed-a", "base_url": "http://h.é.invalid/é"},
        {"model": "qa-embed-a", "base_url": "https://h.invalid//"},
        {"model": "qa-embed-a", "base_url": "/"},
        {"model": "qa-embed-a", "base_url": "http://h.invalid/\n"},
        {"model": "qa-embed-a", "base_url": 5},
        {"model": "qa-embed-a", "base_url": True},
        {"model": "qa-embed-a", "base_url": 0},
        {"model": "qa-embed-a", "base_url": []},
        {"model": "qa-embed-a", "base_url": ["http://h.invalid"]},
        {"model": "qa-embed-a", "base_url": {}},
        {"model": "qa-embed-a", "extra": "ignored", "baseUrl": " "},
        None, [], "qa-embed-a", 5, True, 1.5, [{"model": "qa-embed-a"}],
    ]
    return raws


def normalize_section():
    cases = []
    for raw in target_cases():
        try:
            t = emb.normalize_target(raw)
            result = {"ok": t.to_json(), "identity": t.identity, "slot_id": t.slot_id}
        except emb.EmbeddingConfigError as exc:
            result = {"error": err(exc)}
        cases.append({"raw": raw, "expected": result})
    return cases


def model_re_section():
    samples = ["qa-embed-a", "qa-embed-a\n", "qa-embed-a\n\n", "\n", "", "a", "a b", "a\tb", "a\x1fb", "a\x7fb",
               "\x00", "a\x00", "x" * 200, "x" * 201, "x" * 200 + "\n", "x" * 201 + "\n", "\U0001F33F" * 200,
               "\U0001F33F" * 201, "a ", "a​", "a ", "a\x85", "a\x1c", "a\r", "a\r\n", "\na",
               "a\nb", "é", "qa/embed:a_b.c-d", "﻿x", "a　b", "a᠎b"]
    return [{"model": s, "matches": bool(emb._MODEL_RE.match(s))} for s in samples]


def base_url_section():
    samples = [None, "", " ", "http://h.invalid", "http://h.invalid/", "http://h.invalid//", "https://h.invalid:8/v1///",
               "ftp://h.invalid", "http://u@h", "http://h?x", "http://h#x", "http://h/ x", "http://h/\n",
               "\nhttp://h.invalid\n", "http:// h", "HTTPS://h", "http://h:80:90", "http://[::1]/", "http://h/%20",
               "https:/h", "http//h", 5, 0, True, False, 1.5, [], ["x"], {}, {"a": 1}]
    cases = []
    for v in samples:
        try:
            r = {"ok": emb._normalize_base_url(v)}
        except emb.EmbeddingConfigError as exc:
            r = {"error": err(exc)}
        cases.append({"value": v, "expected": r})
    return cases


STR_VALUES = ["5", "0", "-0.0", "0.0", "1.5", "-2.5", "1e22", "1e16", "1e15", "1e-7", "1e-5", "0.0001", "0.00001",
              "123456789.125", "100000.0", "12345678901234567", "3.141592653589793", "2.5e-10", "1.7976931348623157e308",
              "5e-324", "true", "false", "null", "[]", "{}", "[1]", "[1, 2]", "[1.5]", "[null]", "[true]", '["a"]',
              '["it\'s"]', '["say \\"hi\\""]', '["a\\nb"]', '["tab\\t"]', '["back\\\\slash"]', '["\\u00e9"]', '["\\u00a0"]',
              '["\\u2003"]', '["\\u0001"]', '["\\u007f"]', '["\\u0085"]', '["\\ud83c\\udf3f"]', '[[]]', '[[1]]', '[{}]',
              '{"a": 1}', '{"a": [1, "b"]}', '[["x"]]', '"plain"', '""', '[-1]', '[1e22]', "9007199254740993",
              "-9007199254740993", "18446744073709551615", "-5"]


def str_section():
    out = []
    for text in STR_VALUES:
        v = json.loads(text)
        out.append({"json": text, "str_or_empty": str(v or "")})
    return out


def known_differences_section():
    """Cases where the Rust port answers differently; Python's answer and the pinned Rust answer."""
    out = []

    def raw_case(name, raw_text, rust):
        raw = json.loads(raw_text)
        try:
            python = {"ok": emb.normalize_target(raw).to_json()}
        except emb.EmbeddingConfigError as exc:
            python = {"error": err(exc)}
        out.append({"name": name, "kind": "normalize_target", "json": raw_text, "python": python, "rust": rust})

    # 3. str() of a non-string
    raw_case("big_int_model", '{"model": 12345678901234567890123}',
             {"ok": {"base_url": None, "model": "1.2345678901234568e+22", "profile": "nomic-v1"}})
    raw_case("private_use_in_list", '{"model": ["\\ue000"]}',
             {"ok": {"base_url": None, "model": "['']", "profile": "nomic-v1"}})
    raw_case("format_char_in_list", '{"model": ["\\u200b"]}',
             {"ok": {"base_url": None, "model": "['​']", "profile": "nomic-v1"}})
    raw_case("negative_zero_in_list", '{"model": [-0]}',
             {"ok": {"base_url": None, "model": "[-0.0]", "profile": "nomic-v1"}})
    # a dict profile is echoed in key order by Python; here in sorted key order (or file order if the
    # build enables serde_json's preserve_order): the test accepts either, never anything else
    raw_case("dict_profile_key_order", '{"model": "qa-embed-a", "profile": {"b": 1, "a": 2}}',
             {"error": {"message": "Unknown embedding profile \"{'a': 2, 'b': 1}\" (known: nomic-v1, embeddinggemma-2).",
                        "code": "invalidTarget", "status": 400}, "also_accepted_message":
              "Unknown embedding profile \"{'b': 1, 'a': 2}\" (known: nomic-v1, embeddinggemma-2)."})
    # 1. JSON literals Python's json.loads accepts and serde_json refuses
    for name, body in [("nan_literal", '{"version": 1, "active": {"model": NaN}}'),
                       ("infinity_literal", '{"version": 1, "active": {"model": Infinity}}'),
                       ("huge_exponent", '{"version": 1, "active": {"model": 1e400}}'),
                       ("lone_surrogate", '{"version": 1, "active": {"model": "a\\ud800b"}}')]:
        with tempfile.TemporaryDirectory() as d:
            f = pathlib.Path(d) / "embedding-config.json"
            f.write_text(body, encoding="utf-8")
            c = emb.EmbeddingConfig(Config({}), f)
            python = c.get()
            # a lone surrogate cannot be written into the fixture as JSON text: keep its UTF-16 bytes
            python = {"source": python["source"], "model_utf16le_hex": python["active"]["model"].encode(
                "utf-16-le", "surrogatepass").hex(), "damaged": python.get("damaged")}
        out.append({"name": name, "kind": "state_file", "text": body, "python": python,
                    "rust": "damaged"})
    return out


# ---- seed_from_cfg / state_path_for -------------------------------------------------------

class Capture(logging.Handler):
    def __init__(self):
        super().__init__(logging.DEBUG)
        self.lines = []

    def emit(self, record):
        self.lines.append([record.levelname, record.getMessage()])


def captured(fn):
    logger = logging.getLogger("agent.embedding")
    handler = Capture()
    old_level, old_prop = logger.level, logger.propagate
    logger.setLevel(logging.DEBUG)
    logger.propagate = False
    logger.addHandler(handler)
    try:
        result = fn()
    finally:
        logger.removeHandler(handler)
        logger.setLevel(old_level)
        logger.propagate = old_prop
    return result, handler.lines


def seed_section():
    llm = lambda **kw: {"llm": kw}  # noqa: E731
    cfgs = [
        {},
        llm(),
        llm(embed_model="qa-embed-a"),
        llm(embed_model=" qa-embed-a ", embed_base_url=" http://h.invalid:8080/// ", embed_profile=" embeddinggemma-2 "),
        llm(embed_base_url="/", embed_model=""),
        llm(embed_base_url="http://h.invalid/v1/", embed_model="qa-embed-a", embed_profile="weird-profile"),
        llm(embed_next_model="qa-embed-b"),
        llm(embed_next_model="qa-embed-b", embed_next_profile="embeddinggemma-2"),
        llm(embed_next_model="qa-embed-b", embed_next_profile="embeddinggemma-2", embed_next_base_url="http://n.invalid//"),
        llm(embed_model="qa-embed-a", embed_next_model="qa-embed-a"),
        llm(embed_model="qa-embed-a", embed_next_model="qa-embed-a", embed_next_profile="embeddinggemma-2"),
        llm(embed_model="qa-embed-a", embed_base_url="http://h.invalid", embed_next_model="qa-embed-a",
            embed_next_base_url="http://h.invalid/"),
        llm(embed_model="qa-embed-a", embed_base_url="http://h.invalid", embed_next_model="qa-embed-a",
            embed_next_base_url="http://other.invalid"),
        llm(embed_model=5, embed_base_url=0, embed_profile=False),
        llm(embed_model=1.5, embed_profile=["x"]),
        llm(embed_model=None, embed_base_url=None, embed_next_model=None),
        llm(embed_model=True),
        llm(embed_next_model=7, embed_next_profile=2),
        llm(embed_model="  qa-embed-a 　"),
        {"llm": "not a dict"},
        {"llm": None},
        {"llm": {"embed_model": "m"}, "other": 1},
        llm(embed_next_model="  ", embed_next_profile="embeddinggemma-2"),
        llm(embed_next_model="qa-embed-b", embed_next_profile="   "),
        llm(embed_next_model="qa-embed-b", embed_base_url="http://h.invalid"),
        {"llm.embed_model": "dotted-key-is-not-a-path"},
        llm(embed_model="qa-embed-a", embed_base_url="http://[::1]:8080"),
        llm(embed_model={"a": 1}),
    ]
    out = []
    for cfg in cfgs:
        state, lines = captured(lambda: emb.seed_from_cfg(Config(cfg)))
        out.append({"cfg": cfg,
                    "expected": {k: (v.to_json() if v else None) for k, v in state.items()},
                    "warnings": lines})
    return out


def state_path_section():
    cfgs = [
        {}, {"retrieval": {}}, {"retrieval": {"db_path": ""}}, {"retrieval": {"db_path": ":memory:"}},
        {"retrieval": {"db_path": "index.db"}}, {"retrieval": {"db_path": "./index.db"}},
        {"retrieval": {"db_path": "data/t/index.db"}}, {"retrieval": {"db_path": "/data/t/index.db"}},
        {"retrieval": {"db_path": "/index.db"}}, {"retrieval": {"db_path": "/"}}, {"retrieval": {"db_path": "."}},
        {"retrieval": {"db_path": ".."}}, {"retrieval": {"db_path": "../x/index.db"}},
        {"retrieval": {"db_path": "a//b///index.db"}}, {"retrieval": {"db_path": "a/./b/./index.db"}},
        {"retrieval": {"db_path": "//srv/index.db"}}, {"retrieval": {"db_path": "///srv/index.db"}},
        {"retrieval": {"db_path": "////srv//x/index.db"}}, {"retrieval": {"db_path": "dir/"}},
        {"retrieval": {"db_path": "dir//"}}, {"retrieval": {"db_path": "/dir/sub/"}},
        {"retrieval": {"db_path": "café/î.db"}}, {"retrieval": {"db_path": 5}}, {"retrieval": {"db_path": 0}},
        {"retrieval": {"db_path": None}}, {"retrieval": {"db_path": True}},
        {"retrieval": {"embedding_state_path": "/state/emb.json", "db_path": "/data/index.db"}},
        {"retrieval": {"embedding_state_path": "state//emb.json"}},
        {"retrieval": {"embedding_state_path": "./emb.json"}},
        {"retrieval": {"embedding_state_path": "//x/emb.json"}},
        {"retrieval": {"embedding_state_path": "/"}},
        {"retrieval": {"embedding_state_path": ""}, "retrieval_x": 1},
        {"retrieval": {"embedding_state_path": "", "db_path": "/d/i.db"}},
        {"retrieval": {"embedding_state_path": None, "db_path": "/d/i.db"}},
        {"retrieval": {"embedding_state_path": 7}},
        {"retrieval": {"embedding_state_path": ["a"]}},
        {"retrieval": {"embedding_state_path": True, "db_path": "/d/i.db"}},
        {"retrieval": {"embedding_state_path": 0, "db_path": "/d/i.db"}},
        {"retrieval": "x"},
    ]
    out = []
    for cfg in cfgs:
        try:
            r = {"ok": str(emb.state_path_for(Config(cfg)))}
        except TypeError:
            r = {"type_error": True}
        out.append({"cfg": cfg, "expected": r})
    return out


# ---- addresses ----------------------------------------------------------------------------

V4_NETS = ["0.0.0.0/8", "10.0.0.0/8", "127.0.0.0/8", "169.254.0.0/16", "172.16.0.0/12", "192.0.0.0/24",
           "192.0.0.170/31", "192.0.2.0/24", "192.168.0.0/16", "198.18.0.0/15", "198.51.100.0/24",
           "203.0.113.0/24", "240.0.0.0/4", "255.255.255.255/32", "192.0.0.9/32", "192.0.0.10/32",
           "100.64.0.0/10", "224.0.0.0/4", "192.0.0.8/32", "192.88.99.0/24", "192.31.196.0/24", "192.52.193.0/24",
           "192.175.48.0/24", "198.18.0.0/15", "169.254.169.254/32", "1.0.0.0/8", "8.8.8.0/24", "11.0.0.0/8",
           "126.0.0.0/8", "128.0.0.0/8", "172.15.0.0/16", "172.32.0.0/16", "192.169.0.0/16", "193.0.0.0/8"]
V6_NETS = ["::1/128", "::/128", "::ffff:0:0/96", "64:ff9b:1::/48", "100::/64", "2001::/23", "2001:db8::/32",
           "2002::/16", "3fff::/20", "fc00::/7", "fe80::/10", "2001:1::1/128", "2001:1::2/128", "2001:3::/32",
           "2001:4:112::/48", "2001:20::/28", "2001:30::/28", "ff00::/8", "64:ff9b::/96", "2001::/32", "fec0::/10",
           "::/96", "::/8", "100::/8", "200::/7", "400::/6", "800::/5", "1000::/4", "4000::/3", "6000::/3",
           "8000::/3", "a000::/3", "c000::/3", "e000::/4", "f000::/5", "f800::/6", "fe00::/9", "2000::/3",
           "2001:2::/48", "2001:10::/28", "2620:4f:8000::/48", "2a00::/12", "2606:4700::/32", "fd00::/8",
           "::ffff:0:0:0/96"]


def addr_str_forms(n):
    """The ways one address can be written."""
    out = [n.compressed]
    if n.version == 6:
        out.append(n.exploded)
        out.append(n.compressed.upper())
    return out


def address_samples():
    rng = random.Random(4242)
    s = []
    for text in V4_NETS:
        net = ipaddress.ip_network(text)
        first, last = int(net.network_address), int(net.broadcast_address)
        for v in {first - 1, first, (first + last) // 2, last, last + 1}:
            if 0 <= v <= 0xFFFFFFFF:
                s.append(str(ipaddress.IPv4Address(v)))
    for text in V6_NETS:
        net = ipaddress.ip_network(text)
        first, last = int(net.network_address), int(net.broadcast_address)
        for v in {first - 1, first, (first + last) // 2, last, last + 1, first + 1}:
            if 0 <= v < 1 << 128:
                s.extend(addr_str_forms(ipaddress.IPv6Address(v)))
    # IPv4 carried in IPv6, in every spelling
    v4s = ["10.1.2.3", "127.0.0.1", "8.8.8.8", "100.64.0.1", "169.254.1.1", "0.0.0.0", "224.0.0.1", "240.0.0.1",
           "192.0.2.1", "203.0.113.7", "198.51.100.9", "192.0.0.9", "192.0.0.10", "192.0.0.170", "255.255.255.255",
           "172.16.5.5", "172.32.0.1", "1.1.1.1", "198.18.0.1", "192.168.0.1"]
    for v in v4s:
        a = ipaddress.IPv4Address(v)
        hi, lo = (int(a) >> 16) & 0xFFFF, int(a) & 0xFFFF
        s += [f"::ffff:{v}", f"::FFFF:{v}", f"::ffff:{hi:x}:{lo:x}", f"0:0:0:0:0:ffff:{v}", f"::fffe:{v}",
              f"::ffff:0:{v}", f"64:ff9b::{v}", f"::{v}", f"::{hi:x}:{lo:x}", f"2002:{hi:x}:{lo:x}::1",
              f"{v}%eth0", f"::ffff:{v}%eth0"]
    s += ["::1", "::2", "::", "::0.0.0.1", "::0.0.0.2", "::1%lo", "::1%", "::1%%", "fe80::1%eth0", "fe80::1%", "fe80::1%%x",
          "fe80::1%25eth0", "fe80::1%a%b", "%eth0", "%", "1.2.3.4%", "ff02::1%eth0", "2001:db8::1%zone",
          "8.8.8.8%eth0", "2606:4700::1111%eth0", "fd00::1%\u00e9", "fe80::1%\u0001"]
    junk = ["", " ", "  ", "\n", "1.2.3", "1.2.3.4.5", "256.1.1.1", "1.1.1.256", "01.1.1.1", "1.1.1.01", "1.1.1.1 ",
            " 1.1.1.1", "1.1.1.1\n", "\n1.1.1.1", "1.1.1.1\x00", "\u0663.1.1.1", "0x7f.0.0.1", "127.1", "2130706433",
            "1.1.1.-1", "+1.1.1.1", "1..1.1", ".1.1.1", "1.1.1.", "1.1.1.1.", "::g", ":::", "::::", "1::2::3",
            "12345::", "1:2:3:4:5:6:7:8:9", "1:2:3:4:5:6:7:8", "1:2:3:4:5:6:7::", "::2:3:4:5:6:7:8", "1::3:4:5:6:7:8",
            "1:2:3:4:5:6:7:8::", "::1:2:3:4:5:6:7:8", ":1", "1:", "1:2", ":", "::ffff:1.2.3", "::ffff:1.2.3.4.5",
            "::ffff:256.1.1.1", "::ffff:01.1.1.1", "1:2:3:4:5:6:1.2.3.4", "1:2:3:4:5:6:7:1.2.3.4",
            "1:2:3:4:5::1.2.3.4", "::1.2.3.4:5", "1.2.3.4::", "localhost", "example", "qa-embed.invalid",
            "http://1.2.3.4", "1.2.3.4/32", "1.2.3.4/", "fe80::1/64", "::1/128", "[::1]", "[::1", "::1]",
            "0:0:0:0:0:0:0:1", "0000:0000:0000:0000:0000:0000:0000:0001", "00000::1", "::00001", "0::1", "::0001",
            "::0x1", "::1 ", " ::1", "::1\n", ":: 1", "1:2:3:4:5:6:7", "1:2:3:4:5:6:7:8:", ":1:2:3:4:5:6:7:8",
            "1:2:3:4:5:6:7:", ":2:3:4:5:6:7:8", "1::2:3:4:5:6:7:8", "1:2:3:4::5:6:7:8", "\u00e9", "\U0001F33F",
            "\uff11\uff12\uff17.0.0.1", "\u0661::", "::\u0661", "a" * 46, "0:" * 22 + "0", "1:" * 4 + ":" * 3 + "1",
            "::ffff:127.0.0.1.", "::ffff:0a00:0001", "::FFFF:0A00:0001", "::ffff:", ":ffff:1.2.3.4",
            "10.0.0.1\u00a0", "1e1.0.0.1", "0b1.0.0.1", "٣.٣.٣.٣", "1_0.0.0.1", "10.0.0.0x1"]
    s += junk
    for _ in range(140):
        a = ipaddress.IPv4Address(rng.getrandbits(32))
        s.append(str(a))
        if rng.random() < 0.3:
            s.append(f"::ffff:{a}")
    for _ in range(160):
        base = ipaddress.ip_network(rng.choice(V6_NETS))
        span = int(base.broadcast_address) - int(base.network_address)
        v = int(base.network_address) + (rng.randint(0, span) if span < (1 << 60) else rng.getrandbits(60))
        a = ipaddress.IPv6Address(v)
        s.append(rng.choice([a.compressed, a.exploded, a.compressed.upper()]))
    for _ in range(40):  # fully random v6
        s.append(str(ipaddress.IPv6Address(rng.getrandbits(128))))
    seen, out = set(), []
    for x in s:
        if x not in seen:
            seen.add(x)
            out.append(x)
    return out


def address_section():
    samples = address_samples()
    non_local = [{"address": a, "reason": emb.non_local_reason(a)} for a in samples]

    def parsed(text):
        try:
            ip = ipaddress.ip_address(text)
        except ValueError:
            return None
        return [ip.version, str(int(ip))]
    parse = [{"text": a, "ip": parsed(a)} for a in samples]
    return {"non_local_reason": non_local, "ip_address": parse}


RESOLVER_TABLE = {
    "qa-embed.invalid": ["10.1.2.3"],
    "qa-int.invalid": ["10.0.0.1", "10.0.0.2"],
    "qa-pub.invalid": ["8.8.8.8"],
    "qa-mixed.invalid": ["10.0.0.5", "93.184.216.34"],
    "qa-mixed-first.invalid": ["93.184.216.34", "10.0.0.5"],
    "qa-mixed6.invalid": ["fd00::5", "2606:4700::1111"],
    "qa-empty.invalid": [],
    "qa-fail.invalid": {"raise": "gaierror"},
    "qa-timeout.invalid": {"raise": "TimeoutError"},
    "qa-oserror.invalid": {"raise": "OSError"},
    "qa-v6.invalid": ["fd12:3456::1"],
    "qa-lo.invalid": ["127.0.0.1", "::1"],
    "qa-mapped.invalid": ["::ffff:10.0.0.9"],
    "qa-mapped-pub.invalid": ["::ffff:8.8.8.8"],
    "qa-linklocal.invalid": ["fe80::1%eth0"],
    "qa-linklocal4.invalid": ["169.254.0.7"],
    "qa-junk.invalid": ["not-an-ip"],
    "qa-cgnat.invalid": ["100.64.1.1"],
    "qa-unspec.invalid": ["0.0.0.0"],
    "qa-multicast.invalid": ["224.0.0.251"],
    "qa-reserved.invalid": ["240.0.0.1"],
    "qa-doc.invalid": ["203.0.113.9"],
    "qa-v6doc.invalid": ["2001:db8::1"],
    "qa-compat.invalid": ["::10.0.0.1"],
    "qa-uni-\u00fcml.invalid": ["10.9.9.9"],
    "qa-sub.qa-embed.invalid": ["10.2.2.2"],
    "a b": ["10.3.3.3"],
    "v1.fe": ["10.4.4.4"],
    "qa-embed.invalid:80": ["10.5.5.5"],
    "\uff11\uff10.0.0.1": ["10.6.6.6"],
    "\uff31\uff21-embed.invalid": ["10.7.7.7"],
}


def url_cases():
    hosts = ["qa-embed.invalid", "QA-Embed.INVALID", "10.0.0.5", "8.8.8.8", "[::1]", "[fd00::1]", "[2606:4700::1111]",
             "qa-pub.invalid", "qa-mixed.invalid", "qa-fail.invalid", "qa-empty.invalid", "127.0.0.1", "0.0.0.0",
             "100.64.0.9", "[::ffff:10.0.0.1]", "[fe80::1%25eth0]", "localhost", "qa-unknown.invalid"]
    fixed = [
        "http://qa-embed.invalid", "https://qa-embed.invalid:8443/v1", "HTTP://QA-EMBED.INVALID/", "Https://10.1.1.1:80",
        "http://qa-int.invalid/v1/embeddings", "http://qa-pub.invalid", "http://qa-mixed.invalid", "http://qa-mixed-first.invalid",
        "http://qa-mixed6.invalid", "http://qa-empty.invalid", "http://qa-fail.invalid", "http://qa-timeout.invalid",
        "http://qa-oserror.invalid", "http://qa-v6.invalid", "http://qa-lo.invalid", "http://qa-mapped.invalid",
        "http://qa-mapped-pub.invalid", "http://qa-linklocal.invalid", "http://qa-linklocal4.invalid",
        "http://qa-junk.invalid", "http://qa-cgnat.invalid", "http://qa-unspec.invalid", "http://qa-multicast.invalid",
        "http://qa-reserved.invalid", "http://qa-doc.invalid", "http://qa-v6doc.invalid", "http://qa-compat.invalid",
        "http://QA-\u00dcML.invalid", "http://qa-sub.qa-embed.invalid:9/x", "http://qa-unlisted.invalid",
        "http://10.0.0.5", "http://10.0.0.5:8080/v1", "http://127.0.0.1:11435", "http://[::1]:8080/", "http://[::1]",
        "http://[fd00::1]:80", "http://8.8.8.8", "http://[2606:4700::1111]/", "http://0.0.0.0:1", "http://100.64.0.9",
        "http://[::ffff:10.0.0.1]", "http://[::ffff:8.8.8.8]", "http://[fe80::1%25eth0]/", "http://[fe80::1%eth0]/",
        "http://[fe80::1%]/", "http://[fe80::1]/", "http://[10.0.0.1]/", "http://[v1.fe]/", "http://[vZ.a]/",
        "http://[v1.]/", "http://[::1/", "http://::1]/", "http://[::1]x/", "http://[::1]:80x/", "http://a[b]/",
        "http://u[x@h]/", "http://[::1]:80:90/", "http://[]/", "http://[/", "http://]/", "ftp://10.0.0.5", "//10.0.0.5",
        "10.0.0.5", "", " ", "http:", "http:/10.0.0.5", "http:10.0.0.5", "http://", "http:///x", "https://:80",
        "  http://10.0.0.5", "\thttp://10.0.0.5", "\x00http://10.0.0.5", "http://10.0.\n0.5", "http://10.0.0.5\r\n/x",
        "ht\ttp://10.0.0.5", "javascript:alert(1)", "1http://10.0.0.5", "ht+tp://10.0.0.5", "x-y.z://10.0.0.5",
        "http://u@10.0.0.5", "http://u:p@10.0.0.5", "http://@10.0.0.5", "http://:@10.0.0.5", "http://:pw@10.0.0.5",
        "http://u:@10.0.0.5", "http://a@b@10.0.0.5", "http://a:b@c:d@10.0.0.5", "http://10.0.0.5:", "http://10.0.0.5:abc",
        "http://10.0.0.5:65535", "http://10.0.0.5:65536", "http://10.0.0.5:00080", "http://10.0.0.5:0", "http://10.0.0.5:-1",
        "http://10.0.0.5:+80", "http://10.0.0.5: 80", "http://10.0.0.5:\u0663", "http://10.0.0.5:99999999999999999999",
        "http://10.0.0.5:8:9", "http://10.0.0.5/?x", "http://10.0.0.5?x", "http://10.0.0.5#x", "http://10.0.0.5/#x",
        "http://10.0.0.5/%00", "http://10.0.0.5/a b", "http://10.0.0.5\\x", "http://10.0.0.5;x", "http://10.0.0.5%20",
        "http://10.0.0.5.", "http://10.0.0.5..", "http://010.0.0.5", "http://10.0.0", "http://10.0.0.256", "http://0x7f.1",
        "http://2130706433", "http://127.1", "http://qa-embed.invalid.", "http://-qa.invalid", "http://qa_embed.invalid",
        "http://a b/", "http://v1.fe/", "http://qa-embed.invalid:80/", "http://qa-embed.invalid%25x", "http://%65x.invalid",
        "http://\u2100/", "http://\u2105x/", "http://a\uff0fb/", "http://qa\uff20host/", "http://qa\uff1ahost/",
        "http://qa\uff1fhost/", "http://qa\uff03host/", "http://qa\ufe13host/", "http://qa\u2047host/", "http://\u2a74x/",
        "http://\uff11\uff10.0.0.1/", "http://\uff31\uff21-embed.invalid", "http://\u00e9.invalid", "http://\U0001F33F.invalid",
        "http://qa\u00a0embed.invalid", "http://qa\u3000embed.invalid", "http://\u0130.invalid", "http://\u03a3\u03a3.invalid",
        "http://[::1]\u00e9/", "http://\u00e9@10.0.0.5", "http://10.0.0.5/\u00e9", "http:\\\\10.0.0.5",
        "HTTP://10.0.0.5", "hTtPs://10.0.0.5", "https://10.0.0.5", "httpss://10.0.0.5", "http+x://10.0.0.5",
        "http://10.0.0.5:8080/v1///", "http://[::1]:8080///", "http://qa-embed.invalid///",
        "///10.0.0.5", "http:////10.0.0.5", "http://10.0.0.5@", "http://[::1]@10.0.0.5", "http://[::1]:80@10.0.0.5",
    ]
    rng = random.Random(7)
    schemes = ["http", "https", "HTTP", "ftp", "", "ws"]
    users = ["", "", "", "u@", "u:p@", ":p@", "@"]
    ports = ["", "", ":80", ":8080", ":", ":x", ":70000"]
    paths = ["", "/", "/v1", "/v1/", "?q", "#f", "/a b"]
    for _ in range(70):
        sch = rng.choice(schemes)
        fixed.append(f"{sch}{'://' if sch else '//'}{rng.choice(users)}{rng.choice(hosts)}{rng.choice(ports)}{rng.choice(paths)}")
    seen, out = set(), []
    for u in fixed:
        if u not in seen:
            seen.add(u)
            out.append(u)
    return out


def url_section():
    calls = []

    def resolver(host):
        calls.append(host)
        answer = RESOLVER_TABLE.get(host, [])
        if isinstance(answer, dict):
            exc = {"gaierror": __import__("socket").gaierror, "TimeoutError": TimeoutError, "OSError": OSError}[answer["raise"]]
            raise exc("synthetic")
        return answer

    cases = []
    for url in url_cases():
        calls.clear()
        r = emb.check_embedding_address(url, resolver)
        cases.append({"url": url, "ok": r.ok, "reason": r.reason, "transient": r.transient, "resolver_calls": list(calls)})
    return {"resolver_table": RESOLVER_TABLE, "cases": cases}


# ---- EmbeddingConfig scenarios ------------------------------------------------------------

ENV_CFG = {"llm": {"embed_base_url": "http://qa-embed.invalid:8080/", "embed_model": "qa-embed-a",
                   "embed_profile": "nomic-v1", "embed_next_model": "qa-embed-b",
                   "embed_next_profile": "embeddinggemma-2", "embed_next_base_url": "http://qa-next.invalid"}}
PLAIN_CFG = {"llm": {"embed_model": "qa-embed-a"}}
VALID_FILE = json.dumps({"version": 1, "active": {"base_url": None, "model": "qa-embed-b", "profile": "embeddinggemma-2"},
                         "next": {"base_url": "http://qa-next.invalid", "model": "qa-embed-c", "profile": "nomic-v1"},
                         "previous": {"base_url": None, "model": "qa-embed-a", "profile": "nomic-v1"}}, indent=2)
A = {"model": "qa-embed-a"}
B = {"model": "qa-embed-b", "profile": "embeddinggemma-2"}
C = {"model": "qa-embed-c", "base_url": "http://qa-c.invalid:9/v1/"}
GET = {"op": "get"}

SCENARIOS = {
    "environment_full_flow": (ENV_CFG, "state/embedding-config.json", None, [
        GET, {"op": "set_next", "raw": B}, {"op": "set_next", "raw": {"model": "qa-embed-a", "base_url": "http://qa-embed.invalid:8080"}},
        {"op": "set_next", "raw": {"model": "x y"}}, {"op": "set_next", "raw": C}, {"op": "set_next", "raw": None},
        {"op": "clear_next"}, {"op": "switch"}, {"op": "set_next", "raw": B},
        {"op": "switch", "expected": C}, {"op": "switch", "expected": B}, GET, {"op": "finish"}, {"op": "rollback"},
        {"op": "rollback"}, {"op": "set_next", "raw": A}, {"op": "clear_next"}, {"op": "finish"}, {"op": "finish"},
        {"op": "rollback"}, {"op": "reload"}, GET, {"op": "set_next", "raw": C}, {"op": "reload"}, {"op": "switch"},
        {"op": "reload"}]),
    "plain_environment": (PLAIN_CFG, "embedding-config.json", None, [
        GET, {"op": "switch"}, {"op": "rollback"}, {"op": "finish"}, {"op": "clear_next"}, GET, {"op": "set_next", "raw": B},
        {"op": "reload"}, {"op": "remove_file"}, {"op": "reload"}]),
    "no_state_file": (PLAIN_CFG, None, None, [
        GET, {"op": "set_next", "raw": B}, {"op": "switch"}, {"op": "rollback"}, {"op": "finish"}, {"op": "reload"}, GET]),
    "saved_file_roundtrip": (PLAIN_CFG, "embedding-config.json", VALID_FILE, [
        GET, {"op": "refuse", "reasons": {"active": "x"}, "transient": False}, {"op": "rollback"}, GET, {"op": "switch"},
        {"op": "switch"}, {"op": "finish"}, {"op": "clear_next"}, {"op": "reload"}]),
    "unicode_and_astral_persist": (PLAIN_CFG, "embedding-config.json", None, [
        {"op": "set_next", "raw": {"model": "qa-\u00e8mbed-\U0001F33F", "base_url": "http://qa-\u00e9.invalid/\u00e9"}},
        {"op": "switch"}, {"op": "reload"}, GET]),
    "numeric_model_is_accepted": (PLAIN_CFG, "embedding-config.json", None, [
        {"op": "set_next", "raw": {"model": 5}}, {"op": "set_next", "raw": {"model": 1.5, "profile": "embeddinggemma-2"}},
        {"op": "set_next", "raw": {"model": True}}]),
    "refuse_and_release_seeded": (ENV_CFG, "embedding-config.json", None, [
        GET, {"op": "release", "which": "next"}, {"op": "refuse", "reasons": {"next": "n1"}, "transient": True}, GET,
        {"op": "refuse", "reasons": {"next": "n2"}, "transient": True}, GET, {"op": "release", "which": "next"}, GET,
        {"op": "release", "which": "next"}, {"op": "refuse", "reasons": {"active": "a1"}, "transient": True}, GET,
        {"op": "set_next", "raw": C}, {"op": "clear_next"}, {"op": "rollback"},
        {"op": "refuse", "reasons": {"active": "a2", "next": "n3"}, "transient": False}, GET,
        {"op": "release", "which": "active"}, GET, {"op": "refuse", "reasons": {}, "transient": False},
        {"op": "refuse", "reasons": {"active": "", "next": ""}, "transient": True}, GET]),
    "release_active_restores": (ENV_CFG, "embedding-config.json", None, [
        {"op": "refuse", "reasons": {"active": "a1", "next": "n1"}, "transient": True}, GET,
        {"op": "release", "which": "active"}, GET, {"op": "release", "which": "next"}, GET, {"op": "set_next", "raw": C}, GET]),
    "release_next_over_admin_choice": (ENV_CFG, "embedding-config.json", None, [
        {"op": "refuse", "reasons": {"next": "n1"}, "transient": True}, {"op": "set_next", "raw": C},
        {"op": "release", "which": "next"}, GET]),
    "refuse_unset_next": (PLAIN_CFG, "embedding-config.json", None, [
        {"op": "refuse", "reasons": {"next": "n1"}, "transient": True}, GET, {"op": "release", "which": "next"}, GET]),
    "refused_active_locks_changes": (ENV_CFG, "embedding-config.json", None, [
        {"op": "refuse", "reasons": {"active": "address refused"}, "transient": False}, GET, {"op": "set_next", "raw": C},
        {"op": "clear_next"}, {"op": "switch"}, {"op": "rollback"}, {"op": "finish"}, {"op": "set_next", "raw": A}]),
    "refuse_ignored_once_saved": (ENV_CFG, "embedding-config.json", None, [
        {"op": "clear_next"}, {"op": "refuse", "reasons": {"active": "late"}, "transient": False}, GET]),
    "persist_failure_is_an_os_error": (PLAIN_CFG, "blocker/embedding-config.json", None, [
        {"op": "make_blocker"}, GET, {"op": "set_next", "raw": B}, GET, {"op": "clear_next"}, GET]),
    "state_file_is_a_directory": (PLAIN_CFG, "embedding-config.json", None, [
        {"op": "make_dir"}, {"op": "reload"}, GET, {"op": "set_next", "raw": B}]),
}

DAMAGED_FILES = {
    "empty_file": ("", False), "whitespace_only": ("  \n", False), "truncated_json": ('{"version": 1,', False),
    "not_json": ("not json", False), "list_top": ("[]", False), "null_top": ("null", False), "string_top": ('"x"', False),
    "int_top": ("5", False), "float_top": ("1.5", False), "true_top": ("true", False),
    "version_two": ('{"version": 2, "active": {"model": "a"}}', False), "version_missing": ('{"active": {"model": "a"}}', False),
    "version_string": ('{"version": "1", "active": {"model": "a"}}', False), "version_null": ('{"version": null}', False),
    "active_missing": ('{"version": 1}', False), "active_null": ('{"version": 1, "active": null}', False),
    "active_list": ('{"version": 1, "active": []}', False),
    "active_no_model": ('{"version": 1, "active": {"profile": "nomic-v1"}}', False),
    "active_bad_model": ('{"version": 1, "active": {"model": "a b"}}', False),
    "active_bad_profile": ('{"version": 1, "active": {"model": "a", "profile": "zzz"}}', False),
    "active_bad_url": ('{"version": 1, "active": {"model": "a", "base_url": "ftp://x"}}', False),
    "next_truthy_non_dict": ('{"version": 1, "active": {"model": "a"}, "next": "x"}', False),
    "next_number": ('{"version": 1, "active": {"model": "a"}, "next": 3}', False),
    "previous_list": ('{"version": 1, "active": {"model": "a"}, "previous": [1]}', False),
    "previous_bad_profile": ('{"version": 1, "active": {"model": "a"}, "previous": {"model": "p", "profile": "q"}}', False),
    "bom": ("\ufeff" + '{"version": 1, "active": {"model": "a"}}', False),
    "trailing_garbage": ('{"version": 1, "active": {"model": "a"}} x', False),
    "single_quotes": ("{'version': 1}", False),
    "invalid_utf8": (b'{"version": 1, "active": {"model": "\xff"}}', True),
    "nul_bytes": (b"\x00\x00", True),
}
LOADABLE_FILES = {
    "version_true": '{"version": true, "active": {"model": "a"}}',
    "version_float_one": '{"version": 1.0, "active": {"model": "a"}}',
    "next_falsy_variants": '{"version": 1, "active": {"model": "a"}, "next": 0, "previous": ""}',
    "next_empty_dict": '{"version": 1, "active": {"model": "a"}, "next": {}, "previous": false}',
    "next_null": '{"version": 1, "active": {"model": "a"}, "next": null, "previous": null}',
    "extra_keys": '{"version": 1, "extra": [1], "active": {"model": "a", "x": 1}}',
    "base_url_camel": '{"version": 1, "active": {"model": "a", "baseUrl": "http://h.invalid/"}}',
    "numeric_model": '{"version": 1, "active": {"model": 5}}',
    "duplicate_keys": '{"version": 1, "version": 2, "active": {"model": "a"}, "active": {"model": "b"}}',
    "padded": ' \n\t{"version": 1, "active": {"model": " a "}}\n\n',
    "profile_gemma": '{"version": 1, "active": {"model": "a", "profile": "embeddinggemma-2"}, "previous": {"model": "p"}}',
}
for name, (content, masked) in DAMAGED_FILES.items():
    SCENARIOS[f"damaged_{name}"] = (PLAIN_CFG, "embedding-config.json", content,
                                    [GET, {"op": "set_next", "raw": B}, {"op": "clear_next"},
                                     {"op": "refuse", "reasons": {"active": "r"}, "transient": False}, {"op": "reload"}])
for name, content in LOADABLE_FILES.items():
    SCENARIOS[f"loadable_{name}"] = (PLAIN_CFG, "embedding-config.json", content,
                                     [GET, {"op": "set_next", "raw": B}, {"op": "reload"}])
MASKED_SCENARIOS = {f"damaged_{n}" for n, (_, m) in DAMAGED_FILES.items() if m} | {
    f"damaged_{n}" for n in ("empty_file", "whitespace_only", "truncated_json", "not_json", "bom", "trailing_garbage",
                             "single_quotes")}


def run_config_scenario(name, spec):
    cfg_data, rel, initial, ops = spec
    steps = []
    with tempfile.TemporaryDirectory() as d:
        base = pathlib.Path(d)
        state_file = base / rel if rel else None
        if initial is not None:
            state_file.parent.mkdir(parents=True, exist_ok=True)
            if isinstance(initial, bytes):
                state_file.write_bytes(initial)
            else:
                state_file.write_text(initial, encoding="utf-8")
        cfg = Config(cfg_data)
        c = emb.EmbeddingConfig(cfg, state_file)

        def clean(x):
            return json.loads(json.dumps(x).replace(json.dumps(d)[1:-1], "<DIR>"))

        def snapshot():
            if state_file is not None and state_file.is_file():
                return state_file.read_bytes().hex()
            return None

        steps.append({"op": "init", "get": clean(c.get()), "file": snapshot()})
        for op in ops:
            kind = op["op"]
            result = None
            try:
                if kind == "get":
                    result = {"ok": c.get()}
                elif kind == "set_next":
                    result = {"ok": c.set_next(op["raw"])}
                elif kind == "clear_next":
                    result = {"ok": c.clear_next()}
                elif kind == "switch":
                    exp = op.get("expected")
                    result = {"ok": c.switch_to_next(emb.normalize_target(exp) if exp else None)}
                elif kind == "rollback":
                    result = {"ok": c.rollback()}
                elif kind == "finish":
                    result = {"ok": c.finish()}
                elif kind == "refuse":
                    c.refuse_seeded(op["reasons"], op["transient"])
                    result = {"ok": None}
                elif kind == "release":
                    c.release_seeded(op["which"])
                    result = {"ok": None}
                elif kind == "reload":
                    c = emb.EmbeddingConfig(cfg, state_file)
                    result = {"ok": None}
                elif kind == "remove_file":
                    state_file.unlink()
                    result = {"ok": None}
                elif kind == "make_blocker":
                    state_file.parent.parent.mkdir(parents=True, exist_ok=True)
                    state_file.parent.write_text("a file where the directory should be")
                    result = {"ok": None}
                elif kind == "make_dir":
                    if state_file.exists():
                        state_file.unlink()
                    state_file.mkdir()
                    result = {"ok": None}
            except emb.EmbeddingConfigError as exc:
                result = {"error": err(exc)}
            except OSError as exc:
                result = {"os_error": type(exc).__name__}
            steps.append({"op": op, "result": clean(result), "get": clean(c.get()),
                          "file": snapshot(), "tmp_left": sorted(p.name for p in base.rglob("*.tmp"))})
    return steps


def config_section():
    out = {}
    for name, spec in SCENARIOS.items():
        cfg_data, rel, initial, ops = spec
        out[name] = {"cfg": cfg_data, "file": rel,
                     "initial": (initial.hex() if isinstance(initial, bytes) else initial),
                     "initial_is_hex": isinstance(initial, bytes),
                     "masked_exception_text": name in MASKED_SCENARIOS,
                     "steps": run_config_scenario(name, spec)}
    return out


# ---- guard scenarios ----------------------------------------------------------------------

def ans(ok=False, reason="", transient=False):
    return {"ok": ok, "reason": reason, "transient": transient}


OK = ans(True)
PUBLIC = ans(False, "qa-pub.invalid is not on this network (8.8.8.8 is a public address)", False)
NODNS = ans(False, "the name does not resolve (gaierror)", True)
EMPTY = ans(False, "the name does not resolve", True)
BOOM = {"raise": "RuntimeError"}
N_URL, X_URL = "http://qa-embed.invalid:8080", "http://qa-next.invalid"
GUARD_CFG = {"llm": {"embed_base_url": N_URL, "embed_model": "qa-embed-a", "embed_next_model": "qa-embed-b",
                     "embed_next_profile": "embeddinggemma-2", "embed_next_base_url": X_URL}}
ONLY_ACTIVE = {"llm": {"embed_base_url": N_URL, "embed_model": "qa-embed-a"}}
NO_URL = {"llm": {"embed_model": "qa-embed-a", "embed_next_model": "qa-embed-b", "embed_next_base_url": X_URL}}
RUN = {"run": 1}

GUARD_SCENARIOS = {
    "both_local": (GUARD_CFG, None, {N_URL: [OK], X_URL: [OK]}, [], [RUN]),
    "no_urls_at_all": ({"llm": {"embed_model": "m"}}, None, {}, [], []),
    "next_url_only": (NO_URL, None, {X_URL: [PUBLIC]}, [], [RUN]),
    "active_public_next_local": (GUARD_CFG, None, {N_URL: [PUBLIC], X_URL: [OK]}, [], [RUN]),
    "both_public": (GUARD_CFG, None, {N_URL: [PUBLIC], X_URL: [PUBLIC]}, [], [RUN]),
    "malformed_is_permanent": (ONLY_ACTIVE, None, {N_URL: [ans(False, "not a URL")]}, [], [RUN]),
    "transient_then_ok_in_first_round": (GUARD_CFG, None, {N_URL: [NODNS, NODNS, OK], X_URL: [OK]}, [], [RUN]),
    "transient_then_public_in_first_round": (ONLY_ACTIVE, None, {N_URL: [NODNS, EMPTY, PUBLIC]}, [], [RUN]),
    "never_resolves_then_resolves_later": (GUARD_CFG, None, {N_URL: [NODNS] * 8 + [OK], X_URL: [OK]}, [],
                                           [RUN, RUN, RUN, RUN]),
    "never_resolves_then_turns_public": (ONLY_ACTIVE, None, {N_URL: [NODNS] * 7 + [PUBLIC]}, [], [RUN, RUN, RUN]),
    "both_transient_released_apart": (GUARD_CFG, None, {N_URL: [NODNS] * 7 + [OK], X_URL: [EMPTY] * 9 + [OK]}, [],
                                      [RUN, RUN, RUN, RUN, RUN, RUN]),
    "recheck_keeps_trying_after_an_exception": (ONLY_ACTIVE, None, {N_URL: [NODNS] * 6 + [BOOM, BOOM, OK]}, [],
                                                [RUN, RUN, RUN, RUN]),
    "exception_in_first_round_decides_nothing": (GUARD_CFG, None, {N_URL: [OK], X_URL: [BOOM]}, [], [RUN]),
    "exception_on_a_retry": (GUARD_CFG, None, {N_URL: [NODNS, BOOM], X_URL: [OK]}, [], [RUN]),
    "custom_delays": (ONLY_ACTIVE, None, {N_URL: [NODNS, NODNS, NODNS]}, [0.5, 0.25], [RUN, RUN]),
    "no_retries": (ONLY_ACTIVE, None, {N_URL: [NODNS]}, [], [RUN]),
    "admin_choice_stops_rechecks": (GUARD_CFG, None, {N_URL: [NODNS] * 20, X_URL: [NODNS] * 20}, [],
                                    [RUN, {"admin": {"op": "set_next", "raw": {"model": "qa-embed-z"}}}, RUN, RUN, RUN]),
    "saved_state_is_not_guarded": (GUARD_CFG, VALID_FILE.replace("qa-next.invalid", "qa-next.invalid"),
                                   {N_URL: [PUBLIC], X_URL: [PUBLIC]}, [], [RUN]),
}
DEFAULT_DELAYS = [1.0, 2.0, 4.0, 8.0, 15.0]  # checked against emb.RETRY_DELAYS_S in main()


def run_guard_scenario(name, spec):
    cfg_data, initial, answers, delays_override, steps_spec = spec
    delays = delays_override or (DEFAULT_DELAYS if name not in ("no_retries",) else [])
    if name == "no_retries":
        delays = []
    out = {"cfg": cfg_data, "initial": initial, "answers": answers, "retry_delays": delays, "steps": []}
    with tempfile.TemporaryDirectory() as d:
        state_file = pathlib.Path(d) / "embedding-config.json"
        if initial is not None:
            state_file.write_text(initial, encoding="utf-8")
        c = emb.EmbeddingConfig(Config(cfg_data), state_file)
        counters, calls, sleeps, queue = {}, [], [], []

        def check(url):
            calls.append(url)
            seq = answers[url]
            i = counters.get(url, 0)
            counters[url] = i + 1
            item = seq[min(i, len(seq) - 1)]
            if "raise" in item:
                raise RuntimeError("synthetic")
            return emb.AddressCheck(item["ok"], item["reason"], item["transient"])

        def clean(x):
            return json.loads(json.dumps(x).replace(json.dumps(d)[1:-1], "<DIR>"))

        def run_guard():
            emb.guard_seeded_addresses(c, check=check, sleep=sleeps.append,
                                       schedule=lambda fn, s: queue.append((fn, s)), retry_delays=delays)
        _, logs = captured(run_guard)
        out["steps"].append({"step": "guard", "get": clean(c.get()), "calls": list(calls), "sleeps": list(sleeps),
                             "scheduled": [s for _, s in queue], "logs": logs})
        calls.clear()
        for step in steps_spec:
            before = len(queue)
            if "admin" in step:
                try:
                    c.set_next(step["admin"]["raw"])
                except emb.EmbeddingConfigError:
                    pass
                out["steps"].append({"step": step, "get": clean(c.get())})
                continue
            if not queue:
                out["steps"].append({"step": step, "queue_empty": True})
                continue
            fn, _ = queue.pop(0)
            before = len(queue)
            _, logs = captured(fn)
            out["steps"].append({"step": step, "get": clean(c.get()), "calls": list(calls),
                                 "scheduled": [s for _, s in queue[before:]], "queue": len(queue), "logs": logs})
            calls.clear()
    return out


def guard_section():
    return {name: run_guard_scenario(name, spec) for name, spec in GUARD_SCENARIOS.items()}


def main():
    global emb, Config, InputTooLarge
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    if sys.version_info[:2] != (3, 12):
        sys.exit("record with CPython 3.12, the Diary image's interpreter")
    emb, Config, InputTooLarge = load(sys.argv[1])
    assert DEFAULT_DELAYS == list(emb.RETRY_DELAYS_S)
    data = {
        "source": "noevia-services diary/agent/embedding.py",
        "pinned_sha256": PINNED,
        "python": sys.version.split()[0],
        "constants": {"PROFILES": list(emb.PROFILES), "KINDS": list(emb.KINDS), "DEFAULT_PROFILE": emb.DEFAULT_PROFILE,
                      "RETRY_DELAYS_S": list(emb.RETRY_DELAYS_S), "RECHECK_S": emb.RECHECK_S,
                      "SPLIT_MARGIN": emb.SPLIT_MARGIN, "SPLIT_MAX_DEPTH": emb.SPLIT_MAX_DEPTH,
                      "SPLIT_MIN_CHARS": emb.SPLIT_MIN_CHARS, "SPLIT_MAX_PARTS": emb.SPLIT_MAX_PARTS},
        "classes": whitespace_section(),
        "framing": frame_section(),
        "split_text": split_section(),
        "mean": mean_section(),
        "embed_fitting": fitting_section(),
        "normalize_target": normalize_section(),
        "model_re": model_re_section(),
        "base_url": base_url_section(),
        "str_or_empty": str_section(),
        "seed_from_cfg": seed_section(),
        "state_path_for": state_path_section(),
        "addresses": address_section(),
        "urls": url_section(),
        "config": config_section(),
        "guard": guard_section(),
        "known_differences": known_differences_section(),
    }
    OUT.write_text(json.dumps(data, ensure_ascii=True, indent=None, sort_keys=True, separators=(",", ":")) + "\n",
                   encoding="utf-8")
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
