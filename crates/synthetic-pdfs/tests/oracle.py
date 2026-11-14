"""Driver for tests/oracle.rs: run the reference synthetic_pdfs.py over a fixed case table.

Usage: python3 -I oracle.py <directory holding synthetic_pdfs.py> <output directory>
Each case writes <out>/<name> (bytes) or <out>/<name>.error (the exception's type name).
"""
import sys

sys.path.insert(0, sys.argv[1])
import synthetic_pdfs as f  # noqa: E402

out = sys.argv[2]


def case(name, build):
    try:
        data = build()
    except Exception as e:  # noqa: BLE001 - the type is the verdict
        with open(f"{out}/{name}.error", "w") as handle:
            handle.write(type(e).__name__)
        return
    if data is not None:
        with open(f"{out}/{name}", "wb") as handle:
            handle.write(data)


def pixels(*args, **kwargs):
    f.render_lines.cache_clear()
    width, height, data = f.render_lines(*args, **kwargs)
    assert (width, height) == (f.IMAGE_W, f.IMAGE_H)
    return data


f.write_all(f"{out}/fixtures")
case("pixels-rows", lambda: pixels(tuple(f.ROWS)))
case("pixels-header", lambda: pixels((f.HEADER,)))
case("pixels-font", lambda: pixels(("ABCDEFGHIJKLM", "NOPQRSTUVWXYZ", "0123456789 -."), left=0, top=6, line_gap=0))
case("pixels-empty", lambda: pixels(()))
case("pixels-bottom", lambda: pixels(("TOTAL",), top=f.IMAGE_H - 7 * f.CELL))
case("pixels-lowercase", lambda: pixels(("a",)))
case("pixels-too-wide", lambda: pixels(("X" * 66,)))
case("pixels-widest", lambda: pixels(("X" * 65,)))
case("pixels-off-top", lambda: pixels(("A",), top=0))
case("pdf-none", lambda: f.pdf([]))
case("pdf-blank-page", lambda: f.pdf([{}]))
case("pdf-empty-lists", lambda: f.pdf([{"scan": [], "text": []}]))
case("pdf-escapes", lambda: f.pdf([{"text": ["(x)\\", "a(b)c"]}]))
case("pdf-latin1", lambda: f.pdf([{"text": ["café"]}]))
case("pdf-not-latin1", lambda: f.pdf([{"text": ["€"]}]))
case("pdf-three-pages", lambda: f.pdf([{"scan": ["A"], "text": ["B"]}, {"text": ["C"]}, {"scan": ["D 1"]}]))
for p in (1, 2, 3, 100000):
    case(f"pdf-padding-{p}", lambda p=p: f.pdf([{"text": ["X"]}], padding=p))
case("encrypted-empty", lambda: f.encrypted_pdf(b"", b""))
case("encrypted-long", lambda: f.encrypted_pdf(b"u" * 40, b"o"))
case("oversized", f.oversized_text_pdf)
case("malformed", f.malformed_pdf)
case("serialise-empty", lambda: f._serialise({}))
for name, size, chunk in (("ballast-300k", 300000, 1024 * 1024), ("ballast-tiny", 10, 1024 * 1024), ("ballast-chunked", 300000, 7777)):
    case(name, lambda n=name, s=size, c=chunk: f.write_ballast_pdf(f"{out}/{n}", s, chunk=c) and None)
