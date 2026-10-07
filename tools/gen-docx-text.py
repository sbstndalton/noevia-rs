#!/usr/bin/env python3
"""Generate crates/docx-text/tests/fixtures/docx-text.v1.json (noevia#981).

The reference is noevia-services' ocr/docx_text.py (`extract_docx_py`, or `extract_docx` before
the Rust switch existed). Every document is synthetic and built here: invented text, invented
members, deterministic timestamps. Run with the Python the OCR image runs (3.12) and a built
docx-text binary:

    cargo build --release -p docx-text-cli
    uv run --python 3.12 tools/gen-docx-text.py --ocr <noevia-services>/ocr \
        --bin target/release/docx-text

Each case records Python's result and the binary's. The generator refuses to write the file if
the binary is ever more lenient than Python (accepts what Python refuses, or returns different
text), or if a hand-written case's refusal class differs from Python's without a declared
reason (`stricter`). Seeded mutants of the valid documents only need to keep that safety rule:
when a mutant breaks several things at once the two sides may name different first problems.

The fixture file is copied verbatim into noevia-services (ocr/tests/fixtures/), where CI `cmp`s
it against this repo's copy at the pinned ref and replays it against the live Python function.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import importlib
import io
import json
import random
import struct
import subprocess
import sys
import xml.etree.ElementTree as ET
import zipfile
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "crates" / "docx-text" / "tests" / "fixtures" / "docx-text.v1.json"
W = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
DATE = (2020, 1, 1, 0, 0, 0)

MESSAGES = {
    "Invalid DOCX container": "container",
    "DOCX container exceeds its processing limits": "limits",
    "Duplicate or excessive DOCX members": "members",
    "Encrypted or oversized DOCX container": "encrypted_or_oversized",
    "DOCX main document is missing": "missing",
    "DOCX text exceeds its decompression limit": "decompression",
    "DOCX text exceeds its processing limit": "xml_limit",
    "Unsupported DOCX XML declarations or encoding": "xml_declarations",
    "Unsupported DOCX document namespace": "namespace",
    "DOCX body is missing": "body",
}


def classify(e: BaseException) -> str:
    """The refusal class of a Python exception (the same table lives in the services test)."""
    if isinstance(e, RecursionError):
        return "nesting"
    if isinstance(e, ET.ParseError):
        return "xml_malformed"
    if type(e) is ValueError and str(e) in MESSAGES:
        return MESSAGES[str(e)]
    return "container"  # BadZipFile, EOFError, zlib.error, NotImplementedError, UnicodeDecodeError...


LONG_TEXT = 2000


def compact(text: str, truncated: bool) -> dict:
    """A result as recorded: long texts by UTF-8 sha256 and character count (byte-identical
    comparison without storing 200k characters twice per case)."""
    if len(text) > LONG_TEXT:
        return {"text_sha256": hashlib.sha256(text.encode()).hexdigest(), "text_chars": len(text), "truncated": truncated}
    return {"text": text, "truncated": truncated}


def run_python(fn, data: bytes) -> dict:
    try:
        out = fn(data)
    except Exception as e:  # noqa: BLE001 - every exception is a refusal (the server answers 422)
        return {"error": classify(e)}
    return compact(out["text"], out["truncated"])


def run_rust(binary: str, data: bytes) -> dict:
    proc = subprocess.run([binary, "extract"], input=data, capture_output=True, timeout=60, check=False)
    if proc.returncode == 1:
        assert proc.stdout == b""
        err = proc.stderr.decode()
        assert err.startswith("docx-text: refused: "), err
        return {"error": err.removeprefix("docx-text: refused: ").strip()}
    assert proc.returncode == 0, proc.stderr
    out = json.loads(proc.stdout)
    return compact(out["text"], out["truncated"])


# --- building documents --------------------------------------------------------------------

def document(body: str, *, decl: str = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n',
             root: str = f'<w:document xmlns:w="{W}">', end: str = "</w:document>") -> str:
    return f"{decl}{root}<w:body>{body}</w:body>{end}"


PARTS = {
    "[Content_Types].xml": '<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>',
    "_rels/.rels": '<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>',
    "word/_rels/document.xml.rels": '<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>',
    "word/header1.xml": f'<w:hdr xmlns:w="{W}"><w:p><w:r><w:t>SYNTHETIC HEADER NOT EXTRACTED</w:t></w:r></w:p></w:hdr>',
    "word/footer1.xml": f'<w:ftr xmlns:w="{W}"><w:p><w:r><w:t>SYNTHETIC FOOTER NOT EXTRACTED</w:t></w:r></w:p></w:ftr>',
}


class Unseekable(io.RawIOBase):
    """A write-only stream: zipfile then writes data descriptors (flag bit 3)."""
    def __init__(self):
        self.buf = bytearray()

    def writable(self):
        return True

    def write(self, b):
        self.buf += b
        return len(b)


def build(xml: str | bytes | None, *, parts=PARTS, method=zipfile.ZIP_DEFLATED, stream=False,
          extra_members=(), comment=b"", force_zip64=False) -> bytes:
    sink = Unseekable() if stream else io.BytesIO()
    with zipfile.ZipFile(sink, "w") as z:
        members = [("[Content_Types].xml", parts["[Content_Types].xml"])] if "[Content_Types].xml" in (parts or {}) else []
        if xml is not None:
            members.append(("word/document.xml", xml))
        members += [(k, v) for k, v in (parts or {}).items() if k != "[Content_Types].xml"]
        members += list(extra_members)
        for name, value in members:
            info = zipfile.ZipInfo(name, DATE)
            info.compress_type = method
            data = value.encode() if isinstance(value, str) else value
            if force_zip64:
                with z.open(info, "w", force_zip64=True) as f:
                    f.write(data)
            else:
                z.writestr(info, data)
        z.comment = comment
    return bytes(sink.buf) if stream else sink.getvalue()


def eocd(data: bytes) -> int:
    return data.rfind(b"PK\x05\x06")


def central_entries(data: bytes):
    """(offset of each central directory entry, its name), in order."""
    e = eocd(data)
    size, off = struct.unpack_from("<LL", data, e + 12)
    out, pos = [], off
    while pos < off + size:
        n, x, c = struct.unpack_from("<HHH", data, pos + 28)
        out.append((pos, data[pos + 46:pos + 46 + n]))
        pos += 46 + n + x + c
    return out


def local_offset(data: bytes, name: bytes) -> int:
    for pos, n in central_entries(data):
        if n == name:
            return struct.unpack_from("<L", data, pos + 42)[0]
    raise KeyError(name)


def patch(data: bytes, at: int, fmt: str, *values) -> bytes:
    b = bytearray(data)
    struct.pack_into(fmt, b, at, *values)
    return bytes(b)


def set_flag_everywhere(data: bytes, name: bytes, bit: int) -> bytes:
    for pos, n in central_entries(data):
        if n == name:
            data = patch(data, pos + 8, "<H", struct.unpack_from("<H", data, pos + 8)[0] | bit)
    lo = local_offset(data, name)
    return patch(data, lo + 6, "<H", struct.unpack_from("<H", data, lo + 6)[0] | bit)


def run(words: int, seed: int) -> str:
    rng = random.Random(seed)
    return " ".join(str(rng.randrange(10 ** 6)) for _ in range(words))


WORDS = "alpha bravo delta echo golf hotel india kilo lima mike oscar papa romeo sierra tango zulu".split()


def counting(n: int, seed: int = 981) -> str:
    """n characters of synthetic words: compressible (small fixtures) but well under 200x."""
    rng = random.Random(seed)
    out, size = [], 0
    while size < n:
        w = rng.choice(WORDS) + " "
        out.append(w)
        size += len(w)
    return "".join(out)[:n]


def para(text: str) -> str:
    return f"<w:p><w:r><w:t>{text}</w:t></w:r></w:p>"


def cases():
    """(name, bytes, stricter-reason or None). A reason means the binary may refuse (or refuse
    with another class) where Python does not; the generator still requires the binary never be
    more lenient."""
    c = []

    def add(name, data, stricter=None):
        c.append((name, data, stricter))

    # --- documents Python extracts --------------------------------------------------------
    add("paragraphs", build(document(para("Synthetic first paragraph.") + para("Second, invented.") + "<w:p/>" + para("Third"))))
    add("table", build(document(
        "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Item</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Qty</w:t></w:r></w:p></w:tc></w:tr>"
        "<w:tr><w:tc><w:p><w:r><w:t>Widget</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>3</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"
        + para("after table"))))
    add("nested-table", build(document(
        "<w:tbl><w:tr><w:tc><w:tbl><w:tr><w:tc><w:p><w:r><w:t>inner</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:tc></w:tr></w:tbl>")))
    add("skipped-revisions-fields-drawings", build(document(
        "<w:p><w:r><w:t>Kept</w:t></w:r><w:hyperlink><w:r><w:t> link label</w:t></w:r></w:hyperlink>"
        "<w:del><w:r><w:delText>DELETED</w:delText></w:r></w:del><w:ins><w:r><w:t> inserted</w:t></w:r></w:ins>"
        "<w:moveFrom><w:r><w:t>MOVED AWAY</w:t></w:r></w:moveFrom><w:moveTo><w:r><w:t> moved here</w:t></w:r></w:moveTo>"
        "<w:r><w:instrText>HYPERLINK http://never-fetch.invalid</w:instrText></w:r>"
        "<w:r><w:drawing><w:t>IN DRAWING</w:t></w:drawing><w:pict><w:t>IN PICT</w:t></w:pict></w:r></w:p>")))
    add("tabs-breaks", build(document(
        "<w:p><w:r><w:t>a</w:t><w:tab/><w:t>b</w:t><w:br/><w:t>c</w:t><w:cr/><w:t>d</w:t>"
        "<w:tab><w:t>ignored inside tab</w:t></w:tab></w:r></w:p>")))
    add("headers-footers-not-extracted", build(document(para("Body only"))))
    add("unicode", build(document(
        para("Grüße, café, naïve, Œuvre, ß ẞ") + para("中文文本，日本語のテキスト，한국어") +
        para("emoji 🙂🧪 and combining é ä") + para("math 𝔘𝔫𝔦𝔠𝔬𝔡𝔢 ∑∫√"))))
    add("rtl", build(document(
        para("مرحبا بالعالم الاصطناعي") + para("שלום עולם סינתטי") +
        para("mixed ‫RTL אב‬ and ‏marks‎"))))
    add("python-whitespace-strip", build(document(
        para(" 　  \u0085 padded     ") + "<w:p/>")))
    add("zero-width-not-stripped", build(document(para("​zero width​"))))
    add("xml-space-preserve", build(document(
        '<w:p><w:r><w:t xml:space="preserve">  spaced  </w:t></w:r><w:r><w:t xml:space="preserve"> x </w:t></w:r></w:p>')))
    add("references-cdata-comments-pis", build(document(
        "<w:p><w:r><w:t>a&lt;b&gt;c&amp;d&apos;e&quot;f &#65;&#x42;&#x1F600; "
        "x<!-- comment -->y<?pi data?>z<![CDATA[<raw & cdata>]]></w:t></w:r></w:p>")))
    add("line-ends", build(document("<w:p><w:r><w:t>one\r\ntwo\rthree\nfour&#13;five&#10;six<![CDATA[\r\n]]></w:t></w:r></w:p>")))
    add("t-with-child", build(document("<w:p><w:r><w:t>before<w:x>child</w:x>after</w:t></w:r></w:p>")))
    add("empty-t", build(document("<w:p><w:r><w:t/><w:t></w:t><w:t>x</w:t></w:r></w:p>")))
    add("default-namespace", build(document(
        "<p><r><t>default ns</t></r></p>", root=f'<document xmlns="{W}">', end="</document>").replace("<w:body>", "<body>").replace("</w:body>", "</body>")))
    add("other-prefix", build(document(
        para("x").replace("w:", "q:"), root=f'<w:document xmlns:w="{W}" xmlns:q="{W}">')))
    add("foreign-namespace-ignored", build(document(
        f'<w:p><o:x xmlns:o="urn:synthetic"><w:r><w:t>under foreign</w:t></w:r></o:x><w:r><o:t xmlns:o="urn:other">not w:t</o:t></w:r></w:p>')))
    add("unqualified-t-ignored", build(document("<w:p><t>no namespace</t><w:r><w:t>yes</w:t></w:r></w:p>")))
    add("rebound-default-namespace", build(document(
        f'<w:p xmlns="urn:x"><t>not w</t><w:r xmlns=""><w:t>w text</w:t></w:r></w:p>')))
    add("second-body-ignored", build(document(
        para("first body"), end=f"<w:body>{para('second body')}</w:body></w:document>")))
    add("elements-before-body", build(
        f'<w:document xmlns:w="{W}"><w:background/><w:p><w:r><w:t>outside body</w:t></w:r></w:p><w:body>{para("inside")}</w:body></w:document>'))
    add("no-declaration", build(document(para("no decl"), decl="")))
    add("bom-and-version-1.1", build(b"\xef\xbb\xbf" + document(para("bom"), decl='<?xml version="1.1"?>').encode()))
    add("single-quoted-declaration", build(document(para("sq"), decl="<?xml version='1.0' encoding='utf-8' standalone='no' ?>")))
    add("prolog-epilog-misc", build(document(para("misc"), decl='<?xml version="1.0"?><!-- c --> <?xml-stylesheet href="x"?>\n') + "<!-- after -->\n"))
    add("attributes", build(document(
        "<w:p w:rsidR=\"00A1\" w:rsidRDefault='00B2'><w:pPr><w:jc w:val=\"center\"/></w:pPr><w:r a=\"&lt;&#x9;&amp;\" b='\"'><w:t>attrs</w:t></w:r></w:p>")))
    add("stored", build(document(para("stored, not deflated")), method=zipfile.ZIP_STORED))
    add("data-descriptors", build(document(para("streamed with data descriptors")), stream=True))
    add("non-ascii-member-names", build(document(para("names")), extra_members=[("media/bild-ä.txt", "x"), ("media/日本.txt", "y")]))
    add("cp437-member-name", build(document(para("cp437")), extra_members=[("media/AA.bin", "z")])
        .replace(b"media/AA.bin", b"media/\x82\x94.bin"))
    add("empty-body", build(document("")))
    add("only-whitespace-text", build(document(para("   ") + para("\t"))))
    add("nesting-200", build(document("<w:p>" + "<w:r>" * 200 + "<w:t>deep</w:t>" + "</w:r>" * 200 + "</w:p>")))
    add("many-paragraphs", build(document("".join(para(f"line {i}") for i in range(3000)))))
    add("exactly-limit", build(document(para(counting(200000)))))
    add("limit-then-element", build(document(para(counting(200000)) + "<w:p/>")))
    add("limit-minus-one-then-newline", build(document(para(counting(199999)) + para("x"))))
    add("over-limit", build(document(para(counting(220000)))))
    mb = random.Random(7)
    add("over-limit-multibyte", build(document(para("".join(mb.choice(["äé€ ", "中文 ", "🙂 ", "ß "]) for _ in range(60000))[:205000]))))
    add("exactly-limit-with-newline", build(document(para(counting(199999)))))
    add("limit-split-across-runs", build(document("".join(para(counting(50000)) for _ in range(5)))))

    # --- documents Python refuses ----------------------------------------------------------
    add("not-a-zip", b"this is not a zip file, just synthetic bytes")
    add("empty-input", b"")
    add("empty-zip", build(None, parts={}))
    add("non-docx-zip", build(None, parts={"readme.txt": "synthetic", "data/x.csv": "a,b\n1,2\n"}))
    good = build(document(para("truncate me")))
    add("truncated-tail", good[:-12])
    add("truncated-middle", good[: len(good) // 2] + good[len(good) // 2 + 40:])
    add("truncated-to-eocd", good[eocd(good):])
    add("zip-bomb-ratio", build(document(para("A" * 3_000_000))))
    # A real >64 MiB total needs ~65 KB even at DEFLATE's best ratio, so the total-size bomb
    # declares its sizes (both sides read them from the central directory before inflating).
    bomb = build(document(para("small")), extra_members=[("word/media/a.bin", bytes(1024)), ("word/media/b.bin", bytes(1024))])
    for pos, n in central_entries(bomb):
        if n in (b"word/media/a.bin", b"word/media/b.bin"):
            bomb = patch(bomb, pos + 24, "<L", 33 * 1024 * 1024)
    add("zip-bomb-declared-total", bomb)
    add("xml-over-8mib", build(document(para("A" * (8 * 1024 * 1024 + 10)))))
    dup = io.BytesIO()
    with zipfile.ZipFile(dup, "w") as z:
        import warnings
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            z.writestr(zipfile.ZipInfo("word/document.xml", DATE), document(para("one")))
            z.writestr(zipfile.ZipInfo("word/document.xml", DATE), document(para("two")))
    add("duplicate-document", dup.getvalue())
    # zipfile's writer cuts names at NUL itself, so the NUL is patched in afterwards.
    nul = build(document(para("nul")), extra_members=[("word/document.xmlXhidden", document(para("shadow")))]
                ).replace(b"word/document.xmlXhidden", b"word/document.xml\0hidden")
    add("duplicate-after-nul-truncation", nul)
    add("utf8-and-cp437-duplicate", build(document(para("a")), extra_members=[("media/\u00e9.txt", "1"), ("media/Z.txt", "2")])
        .replace(b"media/Z.txt", b"media/\x82.txt"))
    many = build(document(para("many")), parts={}, extra_members=[(f"m/{i}", "") for i in range(1001)])
    add("1001-members", many)
    e = eocd(many)
    add("1001-members-eocd-lies", patch(patch(many, e + 8, "<HH", 1, 1), e, "<4s", b"PK\x05\x06"))
    add("encrypted", set_flag_everywhere(good, b"word/document.xml", 0x1))
    add("encrypted-other-member", set_flag_everywhere(good, b"word/header1.xml", 0x1))
    add("strong-encryption-flag", set_flag_everywhere(good, b"word/document.xml", 0x40))
    add("xxe-doctype-entity", build(
        f'<?xml version="1.0"?><!DOCTYPE w:document [<!ENTITY x SYSTEM "file:///etc/passwd">]><w:document xmlns:w="{W}"><w:body><w:p><w:r><w:t>&x;</w:t></w:r></w:p></w:body></w:document>'))
    add("billion-laughs", build(
        '<?xml version="1.0"?><!DOCTYPE l [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">]>'
        + document("<w:p><w:r><w:t>&b;</w:t></w:r></w:p>", decl="")))
    add("doctype-lowercase-spaced", build(document(para("x"), decl="<!  doctype w:document>")))
    add("entity-text-in-cdata", build(document("<w:p><w:r><w:t><![CDATA[<!ENTITY inside cdata>]]></w:t></w:r></w:p>")))
    add("undefined-entity", build(document("<w:p><w:r><w:t>&nbsp;</w:t></w:r></w:p>")))
    add("utf16-le-bom", build(document(para("utf16")).encode("utf-16")))
    add("utf16-be-no-doctype", build(document(para("utf16"), decl='<?xml version="1.0" encoding="UTF-16"?>').encode("utf-16-be")))
    add("nul-byte", build(document(para("a\0b")).encode()))
    add("wrong-namespace", build(document(para("x").replace("w:", "v:"), root='<v:document xmlns:v="urn:not-word">', end="</v:document>").replace("<w:body>", "<v:body>").replace("</w:body>", "</v:body>")))
    add("no-namespace-root", build("<document><body/></document>"))
    add("missing-body", build(f'<w:document xmlns:w="{W}"><w:p/></w:document>'))
    add("body-not-direct-child", build(f'<w:document xmlns:w="{W}"><w:x><w:body>{para("nested")}</w:body></w:x></w:document>'))
    for name, xml in [
        ("malformed-unclosed", document(para("x")).replace("</w:document>", "")),
        ("malformed-mismatched", document("<w:p><w:r></w:p></w:r>")),
        ("malformed-unbound-prefix", document("<x:p/>")),
        ("malformed-cdata-end-in-text", document(para("a ]]> b"))),
        ("malformed-junk-after-root", document(para("x")) + "junk"),
        ("malformed-two-roots", document(para("x")) + document("", decl="")),
        ("malformed-duplicate-attribute", document('<w:p a="1" a="2"/>')),
        ("malformed-duplicate-expanded-attribute", document(f'<w:p xmlns:a="urn:s" xmlns:b="urn:s" a:x="1" b:x="2"/>')),
        ("malformed-lt-in-attribute", document('<w:p a="<"/>')),
        ("malformed-control-char", document(para("bell\x01"))),
        ("malformed-char-ref-zero", document(para("&#0;"))),
        ("malformed-char-ref-fffe", document(para("&#xFFFE;"))),
        ("malformed-undeclare-prefix", document('<w:p xmlns:a=""/>')),
        ("malformed-double-dash-comment", document("<w:p><!-- a -- b --></w:p>")),
        ("malformed-reserved-pi", document("<w:p><?xml x?></w:p>")),
        ("malformed-late-declaration", " " + document(para("x"))),
        ("malformed-standalone-value", document(para("x"), decl='<?xml version="1.0" standalone="maybe"?>')),
        ("malformed-no-space-between-attributes", document('<w:p a="1"b="2"/>')),
        ("malformed-text-before-root", "text" + document(para("x"), decl="")),
        ("malformed-empty-xml", ""),
    ]:
        add(name, build(xml))
    add("malformed-invalid-utf8", build(document(para("x")).encode().replace(b"x</w:t>", b"\xff</w:t>")))
    add("malformed-surrogate-utf8", build(document(para("x")).encode().replace(b"x</w:t>", b"\xed\xa0\x80</w:t>")))
    add("nesting-2000", build(document("<w:p>" + "<w:r>" * 2000 + "<w:t>deep</w:t>" + "</w:r>" * 2000 + "</w:p>")))
    e = eocd(good)
    add("eocd-multi-disk", patch(good, e + 4, "<H", 1))
    add("eocd-count-mismatch", patch(good, e + 8, "<H", 1))
    add("eocd-offset-past-end", patch(good, e + 16, "<L", e))
    add("trailing-garbage", good + b"TRAILING")
    add("comment-length-lies", patch(good, e + 20, "<H", 5))
    add("crc-mismatch", patch(good, [p for p, n in central_entries(good) if n == b"word/document.xml"][0] + 16, "<L", 0))
    lo = local_offset(good, b"word/document.xml")
    add("local-name-differs", good[:lo + 30] + b"word/documenX.xml" + good[lo + 30 + 17:])
    add("local-signature-broken", patch(good, lo, "<4s", b"PK\x09\x09"))
    add("future-extract-version", patch(good, [p for p, n in central_entries(good) if n == b"word/document.xml"][0] + 6, "<B", 64))
    add("bad-utf8-flagged-name", build(document(para("x")), extra_members=[("media/\u00e9", "1")])
        .replace(b"media/\xc3\xa9", b"media/\xff\xfe"))

    # --- Python accepts, the binary refuses (stricter) --------------------------------------
    add("archive-comment", build(document(para("commented")), comment=b"synthetic archive comment"),
        "a non-empty archive comment is refused: it can carry trailing data or a second archive")
    add("prepended-data", b"SYNTHETIC-PREFIX" * 4 + good,
        "data before the first member (zipfile's concat shift) is refused")
    add("forced-zip64-extra", build(document(para("zip64")), force_zip64=True),
        "zip64 extra fields are refused")
    hl = local_offset(good, b"word/header1.xml")
    add("other-member-local-header-differs", good[:hl + 30] + b"word/headerX.xml" + good[hl + 30 + 16:],
        "every member's local header must agree with the central directory (Python checks only the opened member)")
    dd = build(document(para("descriptor")), stream=True)
    hlo = local_offset(dd, b"word/document.xml")
    csize = struct.unpack_from("<L", dd, [p for p, n in central_entries(dd) if n == b"word/document.xml"][0] + 20)[0]
    desc_at = hlo + 30 + len(b"word/document.xml") + csize
    add("data-descriptor-size-mismatch", patch(dd, desc_at + 8, "<L", csize + 1),
        "a data descriptor that disagrees with the central directory is refused (Python ignores descriptors)")
    add("bzip2-document", build(document(para("bzip2")), method=zipfile.ZIP_BZIP2),
        "only stored and DEFLATE members are accepted (Python also inflates bzip2/LZMA)")
    add("latin1-declared", build(document(para("plain ascii"), decl='<?xml version="1.0" encoding="ISO-8859-1"?>')),
        "a declared encoding other than UTF-8 is refused (expat would decode latin-1)")
    add("non-ascii-element-name", build(document("<w:p><w:r><w:t>x</w:t></w:r><w:é/></w:p>")),
        "non-ASCII element and attribute names are refused")
    add("nesting-300", build(document("<w:p>" + "<w:r>" * 300 + "<w:t>deep</w:t>" + "</w:r>" * 300 + "</w:p>")),
        "element nesting deeper than 256 is refused (Python's recursive walk fails near 1000)")
    add("rebound-xml-prefix", build(document(para("x"), root=f'<w:document xmlns:w="{W}" xmlns:xml="http://www.w3.org/XML/1998/namespace">')),
        "re-declaring the xml prefix is refused")
    nul_only = build(document(para("x")), extra_members=[("media/aXb.txt", "1")]).replace(b"media/aXb.txt", b"media/a\0b.txt")
    add("nul-in-member-name", nul_only, "member names containing NUL are refused (Python truncates them)")
    return c


def mutants(seeds, rng: random.Random, count: int):
    out = []
    for i in range(count):
        name, base = seeds[rng.randrange(len(seeds))]
        b = bytearray(base)
        op = rng.randrange(5)
        if op == 0:  # flip bits
            for _ in range(rng.randint(1, 4)):
                at = rng.randrange(len(b))
                b[at] ^= 1 << rng.randrange(8)
        elif op == 1:  # overwrite a header-ish field near a signature
            sigs = [j for j in range(len(b) - 4) if b[j:j + 2] == b"PK" and b[j + 2] in (1, 3, 5, 7)]
            at = rng.choice(sigs) + rng.randrange(4, 46) if sigs else rng.randrange(len(b))
            for k in range(rng.randint(1, 4)):
                if at + k < len(b):
                    b[at + k] = rng.choice([0, 0xFF, rng.randrange(256)])
        elif op == 2:  # truncate
            b = b[: rng.randrange(len(b))]
        elif op == 3:  # cut a slice out
            at = rng.randrange(len(b))
            del b[at: at + rng.randint(1, 64)]
        else:  # insert bytes
            at = rng.randrange(len(b) + 1)
            b[at:at] = bytes(rng.randrange(256) for _ in range(rng.randint(1, 16)))
        out.append((f"mutant-{i:03d}-of-{name}", bytes(b)))
    return out


def lenient(py: dict, rs: dict) -> bool:
    """The binary is more lenient than Python: it extracts what Python refuses, or extracts
    different text."""
    return "error" not in rs and py != rs


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--ocr", required=True, help="noevia-services ocr/ directory")
    ap.add_argument("--bin", required=True, help="built docx-text binary")
    ap.add_argument("--out", default=str(OUT))
    a = ap.parse_args()
    if sys.version_info[:2] != (3, 12):
        sys.exit("run with Python 3.12, the OCR image's interpreter")
    sys.path.insert(0, a.ocr)
    mod = importlib.import_module("docx_text")
    fn = getattr(mod, "extract_docx_py", None) or mod.extract_docx

    records, problems = [], []
    hand = cases()
    for name, data, stricter in hand:
        py, rs = run_python(fn, data), run_rust(a.bin, data)
        rec = {"name": name, "kind": "case", "docx": base64.b64encode(data).decode(), "python": py, "rust": rs}
        if stricter:
            rec["stricter"] = stricter
            if "error" not in rs:
                problems.append(f"{name}: declared stricter but the binary accepted it")
        elif py != rs:
            problems.append(f"{name}: python {py if 'error' in py else 'ok'} != rust {rs if 'error' in rs else 'ok'}")
        if lenient(py, rs):
            problems.append(f"{name}: binary more lenient than Python")
        records.append(rec)
    seeds = [(n, d) for n, d, s in hand if s is None and "error" not in run_python(fn, d) and len(d) < 8 * 1024]
    stats = {"mutants": 0, "both_ok": 0, "both_refused": 0, "class_differs": 0, "stricter": 0}
    for name, data in mutants(seeds, random.Random(981), 600):
        py, rs = run_python(fn, data), run_rust(a.bin, data)
        if lenient(py, rs):
            problems.append(f"{name}: binary more lenient than Python ({py} vs {rs})")
        stats["mutants"] += 1
        if "error" in py and "error" in rs:
            stats["both_refused"] += 1
            stats["class_differs"] += py != rs
        elif "error" in rs:
            stats["stricter"] += 1
        else:
            stats["both_ok"] += 1
        records.append({"name": name, "kind": "mutant", "docx": base64.b64encode(data).decode(), "python": py, "rust": rs})
    if problems:
        sys.exit("refusing to write fixtures:\n  " + "\n  ".join(problems))
    doc = {
        "version": 1,
        "about": "noevia#981 differential fixtures: synthetic DOCX documents, Python ocr/docx_text.py "
                 "extract_docx vs the docx-text binary. Generated by noevia-rs tools/gen-docx-text.py.",
        "python": "%d.%d" % sys.version_info[:2],
        "zlib": zlib.ZLIB_VERSION,
        "mutant_stats": stats,
        "cases": records,
    }
    Path(a.out).write_text(json.dumps(doc, ensure_ascii=True, indent=0, sort_keys=False) + "\n")
    print(f"wrote {a.out}: {len(hand)} cases, {stats}")


if __name__ == "__main__":
    main()
