#!/usr/bin/env python3
"""Generate crates/workspace-restore-verify/tests/fixtures/workspace-restore.v1.json.

The reference is noevia-services' diary/agent/workspace_restore.py `verify` (python mode). Every
archive here is synthetic and built in this file; the output does not depend on the clock, the
locale or the Python version's zipfile timestamps (fixed date_time, explicit attributes,
seeded mutations), and is written with sorted keys and ASCII escapes.

    cargo build --release -p workspace-restore-verify-cli
    python3 -I tools/gen-workspace-restore.py --diary <noevia-services>/diary \
        --bin target/release/workspace-restore-verify

(The Diary's module imports its storage helpers, so run it with the Diary's requirements
installed, for example a virtualenv from diary/requirements-dev.txt.)

Each case records Python's decision (`accept <listing sha256>` or `reject: <code>`, the code being
the key of workspace_restore.MESSAGES for Python's message) and the binary's. The generator
refuses to write the file if the binary ever accepts what Python rejects, accepts with a
different listing, or disagrees without a declared `stricter` reason; seeded mutants may only
disagree by the binary answering `reject: invalid`.

The file is copied verbatim into noevia-services (diary/tests/fixtures/), where CI `cmp`s it
against this repo's copy at the pinned ref and replays it against the live Python module and,
through WORKSPACE_RESTORE_VERIFY_IMPL=rust, the binary.
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
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "crates" / "workspace-restore-verify" / "tests" / "fixtures" / "workspace-restore.v1.json"
DATE = (2026, 1, 2, 3, 4, 6)
FILE_ATTR = 0o600 << 16
DIR_ATTR = (0o40775 << 16) | 0x10
SEED = 982
MUTANTS = 160


def manifest_for(files, directories, settings=None):
    return {"format": "noevia-workspace-v1", "settings": settings or {}, "directories": sorted(directories),
            "fileCount": len(files), "bytes": sum(map(len, files.values())),
            "files": [{"path": p, "bytes": len(d), "sha256": hashlib.sha256(d).hexdigest()} for p, d in sorted(files.items())]}


def zip_members(members, compression=zipfile.ZIP_STORED):
    """members: (name, data, external_attr or None for the default) in order."""
    out = io.BytesIO()
    with zipfile.ZipFile(out, "w", compression=compression) as bundle:
        for name, data, attr in members:
            info = zipfile.ZipInfo(name, date_time=DATE)
            info.compress_type = compression
            info.create_system = 3
            info.external_attr = (DIR_ATTR if name.endswith("/") else FILE_ATTR) if attr is None else attr
            bundle.writestr(info, data)
    return out.getvalue()


def export(files, directories=(), manifest=None, compression=zipfile.ZIP_STORED, extra=(), attrs=None, drop=()):
    """An archive laid out like workspace_export.archive, with optional changes."""
    directories = sorted(set(directories))
    m = manifest_for(files, directories) if manifest is None else manifest
    body = m if isinstance(m, bytes) else json.dumps(m, ensure_ascii=False, sort_keys=True, indent=2).encode()
    attrs = attrs or {}
    members = [("manifest.json", body, attrs.get("manifest.json")), ("workspace/", b"", attrs.get("workspace/"))]
    members += [("workspace/" + d + "/", b"", attrs.get("workspace/" + d + "/")) for d in directories]
    members += [("workspace/" + p, d, attrs.get("workspace/" + p)) for p, d in sorted(files.items())]
    members += list(extra)
    members = [m for m in members if m[0] not in drop]
    return zip_members(members, compression)


def offsets(body):
    """(local header offsets, central entry offsets) by member order."""
    eocd = body.rfind(b"PK\x05\x06")
    cd_size, cd_off = struct.unpack("<II", body[eocd + 12:eocd + 20])
    central, local, pos = [], [], cd_off
    while pos < cd_off + cd_size:
        central.append(pos)
        n, e, c = struct.unpack("<HHH", body[pos + 28:pos + 34])
        local.append(struct.unpack("<I", body[pos + 42:pos + 46])[0])
        pos += 46 + n + e + c
    return local, central


def patch(body, changes):
    b = bytearray(body)
    for at, fmt, value in changes:
        struct.pack_into(fmt, b, at, value)
    return bytes(b)


def member_index(body, name):
    with zipfile.ZipFile(io.BytesIO(body)) as z:
        return [i.filename for i in z.infolist()].index(name)


def set_sizes(body, name, usize, csize=None, crc=None):
    """Patch the declared sizes (and optionally CRC) of `name` in both headers."""
    i = member_index(body, name)
    local, central = offsets(body)
    ch = []
    if crc is not None:
        ch += [(local[i] + 14, "<I", crc), (central[i] + 16, "<I", crc)]
    if csize is not None:
        ch += [(local[i] + 18, "<I", csize), (central[i] + 20, "<I", csize)]
    ch += [(local[i] + 22, "<I", usize), (central[i] + 24, "<I", usize)]
    return patch(body, ch)


def set_flag(body, name, bit):
    i = member_index(body, name)
    local, central = offsets(body)
    lf = struct.unpack("<H", body[local[i] + 6:local[i] + 8])[0] | bit
    return patch(body, [(local[i] + 6, "<H", lf), (central[i] + 8, "<H", lf)])


def set_method(body, name, method):
    i = member_index(body, name)
    local, central = offsets(body)
    return patch(body, [(local[i] + 8, "<H", method), (central[i] + 10, "<H", method)])


def nested(depth):
    return b'{"format":"noevia-workspace-v1","directories":[],"files":[],"fileCount":0,"bytes":0,"settings":' + b"[" * depth + b"]" * depth + b"}"


def cases():
    note = b"---\nsynthetic: yes\n---\nSynthetic note\r\n"
    files = {"raw/note.md": note, "assets/image.bin": b"\x00\xff\x10", "Entries/2026/01.md": b"# January\n"}
    dirs = ["raw", "assets", "Entries", "Entries/2026", "empty"]
    base = export(files, dirs)
    big = {"big.md": b"lorem ipsum " * 4000}
    m = manifest_for(files, dirs)

    def mm(**changes):
        x = json.loads(json.dumps(m))
        for k, v in changes.items():
            if v is KeyError:
                x.pop(k, None)
            else:
                x[k] = v
        return x

    def rows(fn):
        x = json.loads(json.dumps(m))
        fn(x["files"])
        return x

    c = [
        ("accept/export", base, None),
        ("accept/empty-workspace", export({}, []), None),
        ("accept/deflated", export({**files, **big}, dirs, compression=zipfile.ZIP_DEFLATED), None),
        ("accept/unicode-names", export({"Tagebuch/Grüße – ü.md": "ü".encode(), "日記/一.md": b"x"}, ["Tagebuch", "日記"]), None),
        ("accept/empty-file", export({"empty.md": b""}, []), None),
        ("accept/mode-zero", export(files, dirs, attrs={"workspace/raw/note.md": 0, "workspace/empty/": 0}), None),
        ("accept/tab-and-space-names", export({"a b/c\td.md": b"t"}, ["a b"]), None),
        ("accept/settings-kept", export(files, dirs, manifest={**m, "settings": {"timezone": "UTC", "n": [1, 2, {"k": None}]}}), None),
        ("accept/extra-manifest-keys", export(files, dirs, manifest={**m, "exportedBy": "synthetic"}), None),
        # Container.
        ("container/not-zip", b"not a zip", None),
        ("container/empty", b"", None),
        ("container/truncated", base[: len(base) // 2], None),
        ("container/eocd-only", b"PK\x05\x06" + b"\x00" * 18, None),
        ("container/crc-mismatch", patch(base, [(offsets(base)[0][3] + 14, "<I", 0), (offsets(base)[1][3] + 16, "<I", 0)]), None),
        ("container/bad-local-signature", patch(base, [(offsets(base)[0][-1], "<I", 0x04034B51)]), None),
        ("container/extract-version", patch(base, [(offsets(base)[1][-1] + 6, "<B", 99)]), None),
        ("container/corrupt-deflate", None, "corrupt-deflate"),
        ("container/central-name-differs-from-local", None, "name-differs"),
        # Entry checks.
        ("entries/duplicate-name", zip_members([("manifest.json", b"{}", None), ("manifest.json", b"{}", None)]), None),
        ("path/dotdot", export(files, dirs, extra=[("../escape", b"bad", None)]), None),
        ("path/absolute", export(files, dirs, extra=[("/absolute", b"bad", None)]), None),
        ("path/nested-dotdot", export(files, dirs, extra=[("workspace/../../escape", b"bad", None)]), None),
        ("path/backslash", export(files, dirs, extra=[("workspace/a\\b", b"bad", None)]), None),
        ("path/dot-segment", export(files, dirs, extra=[("workspace/./x", b"bad", None)]), None),
        ("path/empty-segment", export(files, dirs, extra=[("workspace//x", b"bad", None)]), None),
        ("path/only-slash", export(files, dirs, extra=[("/", b"", None)]), None),
        ("entry_type/symlink", export(files, dirs, attrs={"workspace/raw/note.md": 0o120777 << 16}), None),
        ("entry_type/symlink-dir-name", export(files, dirs, attrs={"workspace/empty/": 0o120777 << 16}), None),
        ("entry_type/fifo", export(files, dirs, attrs={"workspace/raw/note.md": 0o010644 << 16}), None),
        ("entry_type/char-device", export(files, dirs, attrs={"workspace/raw/note.md": 0o020644 << 16}), None),
        ("entry_type/block-device", export(files, dirs, attrs={"workspace/raw/note.md": 0o060644 << 16}), None),
        ("entry_type/socket", export(files, dirs, attrs={"workspace/raw/note.md": 0o140644 << 16}), None),
        ("entry_type/encrypted", set_flag(base, "workspace/raw/note.md", 1), None),
        ("entry_limits/file-over-64mib", set_sizes(base, "workspace/raw/note.md", 64 * 1024 * 1024 + 1), None),
        ("entry_limits/bzip2", set_method(base, "workspace/raw/note.md", 12), None),
        ("entry_limits/lzma", set_method(base, "workspace/raw/note.md", 14), None),
        ("entry_limits/unknown-method", set_method(base, "workspace/raw/note.md", 99), None),
        ("directory_not_empty/workspace", None, "dir-payload-root"),
        ("directory_not_empty/listed", None, "dir-payload-listed"),
        ("type_mismatch/dir-mode-on-file", export(files, dirs, attrs={"workspace/raw/note.md": (0o40700 << 16)}), None),
        ("type_mismatch/file-mode-on-dir", export(files, dirs, attrs={"workspace/empty/": (0o100600 << 16)}), None),
        ("expanded/declared-sizes-over-264mib", None, "expanded"),
        # Manifest.
        ("invalid/missing-manifest", export(files, dirs, drop=("manifest.json",)), None),
        ("manifest_size/declared-over-8mib", set_sizes(export(files, dirs), "manifest.json", 8 * 1024 * 1024 + 1), None),
        ("invalid/manifest-not-json", export(files, dirs, manifest=b"{not json"), None),
        ("invalid/manifest-not-utf8", export(files, dirs, manifest=b'{"format":"\xff"}'), None),
        ("manifest_object/utf8-bom-then-array", export(files, dirs, manifest=b"\xef\xbb\xbf[]"), None),
        ("invalid/manifest-two-boms", export(files, dirs, manifest=b"\xef\xbb\xbf\xef\xbb\xbf{}"), None),
        ("accept/manifest-utf8-bom", export(files, dirs, manifest=b"\xef\xbb\xbf" + json.dumps(m).encode()), None),
        ("accept/manifest-utf16-bom", export(files, dirs, manifest=json.dumps(m).encode("utf-16")), None),
        ("accept/manifest-utf16-be", export(files, dirs, manifest=json.dumps(m).encode("utf-16-be")), None),
        ("accept/manifest-utf32-le", export(files, dirs, manifest=json.dumps(m).encode("utf-32-le")), None),
        ("accept/manifest-utf32-bom", export(files, dirs, manifest=json.dumps(m).encode("utf-32")), None),
        ("invalid/manifest-deeply-nested", export({}, [], manifest=nested(100000)), None),
        ("manifest_object/array", export(files, dirs, manifest=[]), None),
        ("manifest_object/null", export(files, dirs, manifest=b"null"), None),
        ("manifest_object/string", export(files, dirs, manifest=b'"noevia-workspace-v1"'), None),
        ("format/missing", export(files, dirs, manifest=mm(format=KeyError)), None),
        ("format/other", export(files, dirs, manifest=mm(format="noevia-workspace-v2")), None),
        ("format/not-string", export(files, dirs, manifest=mm(format=1)), None),
        ("counts/files-not-list", export(files, dirs, manifest=mm(files={})), None),
        ("counts/directories-missing", export(files, dirs, manifest=mm(directories=KeyError)), None),
        ("counts/too-many-directories", export(files, dirs, manifest=mm(directories=["d%d" % i for i in range(5001)])), None),
        ("counts/too-many-files", export(files, dirs, manifest=mm(files=[{}] * 5001)), None),
        ("path/directory-not-string", export(files, dirs, manifest=mm(directories=[1])), None),
        ("path/directory-traversal", export(files, dirs, manifest=mm(directories=["a/../b"])), None),
        ("path/directory-absolute", export(files, dirs, manifest=mm(directories=["/etc"])), None),
        ("duplicate_directory", export(files, dirs, manifest=mm(directories=["raw", "raw"])), None),
        ("file_record/not-object", export(files, dirs, manifest=mm(files=[None])), None),
        ("file_record/bytes-bool", export(files, dirs, manifest=rows(lambda r: r[0].update(bytes=True))), None),
        ("file_record/bytes-float", export(files, dirs, manifest=rows(lambda r: r[0].update(bytes=3.0))), None),
        ("file_record/bytes-negative", export(files, dirs, manifest=rows(lambda r: r[0].update(bytes=-1))), None),
        ("file_record/bytes-string", export(files, dirs, manifest=rows(lambda r: r[0].update(bytes="3"))), None),
        ("file_record/sha-missing", export(files, dirs, manifest=rows(lambda r: r[0].pop("sha256"))), None),
        ("file_record/sha-not-string", export(files, dirs, manifest=rows(lambda r: r[0].update(sha256=0))), None),
        ("invalid/path-missing", export(files, dirs, manifest=rows(lambda r: r[0].pop("path"))), None),
        ("path/file-not-string", export(files, dirs, manifest=rows(lambda r: r[0].update(path=["x"]))), None),
        ("path/file-traversal", export(files, dirs, manifest=rows(lambda r: r[0].update(path="../escape"))), None),
        ("path/file-nul", export(files, dirs, manifest=rows(lambda r: r[0].update(path="a\x00b"))), None),
        ("overlap/duplicate-file", export(files, dirs, manifest=rows(lambda r: r.append(dict(r[0])))), None),
        ("overlap/file-is-directory", export(files, dirs, manifest=mm(directories=dirs + ["raw/note.md"])), None),
        ("invalid/listed-file-missing", export(files, dirs, drop=("workspace/raw/note.md",)), None),
        ("checksum/content-changed", export({**files, "raw/note.md": b"changed"}, dirs, manifest=m), None),
        ("checksum/size-wrong", export(files, dirs, manifest=rows(lambda r: r[0].update(bytes=r[0]["bytes"] + 1))), None),
        ("checksum/sha-uppercase", export(files, dirs, manifest=rows(lambda r: r[0].update(sha256=r[0]["sha256"].upper()))), None),
        ("parent_overlap/file-under-file", export({"a": b"1", "a/b": b"2"}, []), None),
        ("parent_overlap/dir-under-file", export({"a": b"1"}, ["a/b"]), None),
        ("entries_mismatch/unlisted-file", export(files, dirs, extra=[("workspace/extra", b"bad", None)]), None),
        ("entries_mismatch/unlisted-top-level", export(files, dirs, extra=[("README.txt", b"bad", None)]), None),
        ("entries_mismatch/missing-directory-entry", export(files, dirs, drop=("workspace/empty/",)), None),
        ("entries_mismatch/missing-workspace-root", export(files, dirs, drop=("workspace/",)), None),
        ("entries_mismatch/directory-as-file-entry", None, "dir-as-file"),
        ("totals_invalid/file-count-missing", export(files, dirs, manifest=mm(fileCount=KeyError)), None),
        ("totals_invalid/file-count-bool", export(files, dirs, manifest=mm(fileCount=False, bytes=0)), None),
        ("totals_invalid/bytes-float", export(files, dirs, manifest=mm(bytes=float(m["bytes"]))), None),
        ("totals_mismatch/file-count", export(files, dirs, manifest=mm(fileCount=2)), None),
        ("totals_mismatch/bytes", export(files, dirs, manifest=mm(bytes=m["bytes"] + 1)), None),
        ("totals_mismatch/negative-bytes", export(files, dirs, manifest=mm(bytes=-1)), None),
    ]
    specials = {}
    # A corrupt DEFLATE stream (Python: zlib.error, an uncaught server error before noevia#982).
    d = export(big, [], compression=zipfile.ZIP_DEFLATED)
    loc = offsets(d)[0][member_index(d, "workspace/big.md")]
    n, e = struct.unpack("<HH", d[loc + 26:loc + 30])
    b = bytearray(d)
    b[loc + 30 + n + e] = 0xFF
    b[loc + 30 + n + e + 1] = 0xFF
    specials["corrupt-deflate"] = bytes(b)
    # A DEFLATE member declaring 12 bytes that inflates to 1 MiB of zeros.
    zeros = b"\x00" * (1024 * 1024)
    bomb = export({"bomb.md": zeros[:12]}, [], compression=zipfile.ZIP_DEFLATED)
    comp = zlib_raw(zeros)
    specials["bomb"] = rebuild_member(bomb, "workspace/bomb.md", comp, 12, zipfile.crc32(zeros[:12]))
    # The central directory names a member differently from its local header.
    nd = bytearray(base)
    ci = offsets(base)[1][member_index(base, "workspace/raw/note.md")]
    nlen = struct.unpack("<H", base[ci + 28:ci + 30])[0]
    nd[ci + 46 + nlen - 1:ci + 46 + nlen] = b"x"
    specials["name-differs"] = bytes(nd)
    specials["dir-payload-root"] = export(files, dirs, drop=("workspace/",), extra=[("workspace/", b"payload", None)])
    specials["dir-payload-listed"] = export(files, [d for d in dirs if d != "empty"], manifest=m, extra=[("workspace/empty/", b"payload", None)])
    # Five members each declaring 60 MiB: 300 MiB declared, nothing inflated.
    ex = export({"a%d" % i: b"x" for i in range(5)}, [])
    for i in range(5):
        ex = set_sizes(ex, "workspace/a%d" % i, 60 * 1024 * 1024)
    specials["expanded"] = ex
    # A listed directory stored as a file-named entry (no trailing slash) with file mode.
    specials["dir-as-file"] = export(files, [d for d in dirs if d != "empty"], manifest=m, extra=[("workspace/empty", b"", None)])
    out = []
    for name, body, special in c:
        out.append((name, specials[special] if special else body, None))
    # Inputs Python accepts and the Rust leaf refuses on purpose.
    stricter = [
        ("stricter/archive-comment", comment(base, b"synthetic comment"), "archive comment"),
        ("stricter/prepended-bytes", b"MZ synthetic stub" + base, "data before the first member"),
        ("stricter/nul-in-entry-name", None, "member name with NUL"),
        ("stricter/manifest-nan-setting", export(files, dirs, manifest=json.dumps(m)[:-1].encode() + b', "settings": NaN}'), "NaN in JSON"),
        ("stricter/manifest-nesting-200", export(files, dirs, manifest=json.dumps({**m, "settings": "@"}).encode().replace(b'"@"', b"[" * 200 + b"]" * 200)), "JSON nesting beyond serde_json's limit"),
        ("stricter/manifest-utf16-lone-surrogate", export(files, dirs, manifest=json.dumps(m)[:-1].encode("utf-16-le") + ', "x": "'.encode("utf-16-le") + b"\x00\xd8" + '"}'.encode("utf-16-le")), "lone surrogate (Python decodes with surrogatepass)"),
        ("stricter/manifest-utf8-encoded-surrogate", export(files, dirs, manifest=json.dumps(m)[:-1].encode() + b', "x": "\xed\xa0\x80"}'), "UTF-8-encoded surrogate (Python decodes with surrogatepass)"),
        ("stricter/huge-int-bytes", export(files, dirs, manifest=rows(lambda r: r[0].update(bytes=2 ** 70))), "integer beyond 64 bits (both refuse; class differs)"),
        ("stricter/trailing-deflate-input", None, "DEFLATE input after the end of stream"),
        # Python inflates only up to the declared 12 bytes (bounded) and their CRC matches.
        ("stricter/deflate-bomb-lies-about-size", None, "DEFLATE output beyond the declared size"),
    ]
    # A raw name "workspace/raw/note.md\0junk", which Python truncates back to the listed one.
    specials["nul"] = nul_name(files, dirs)
    specials["trailing"] = trailing_deflate(files, dirs)
    for name, body, reason in stricter:
        if body is None:
            body = specials["nul" if "nul" in name else "bomb" if "bomb" in name else "trailing"]
        out.append((name, body, reason))
    # Seeded single-byte mutants of a small deflated export.
    small = export({"a.md": b"alpha " * 20, "b/c.md": b"gamma"}, ["b", "e"], compression=zipfile.ZIP_DEFLATED)
    rng = random.Random(SEED)
    for k in range(MUTANTS):
        b = bytearray(small)
        at = rng.randrange(len(b))
        b[at] ^= 1 << rng.randrange(8)
        out.append((f"mutant/{k:03d}-byte-{at}", bytes(b), "mutant"))
    return out


def zlib_raw(data):
    import zlib
    c = zlib.compressobj(9, zlib.DEFLATED, -15)
    return c.compress(data) + c.flush()


def rebuild_member(body, name, payload, usize, crc):
    """Replace one member's compressed bytes and sizes, rewriting offsets (single pass rebuild)."""
    with zipfile.ZipFile(io.BytesIO(body)) as z:
        infos = z.infolist()
    out = io.BytesIO()
    central = b""
    for info in infos:
        off = out.tell()
        raw = payload if info.filename == name else raw_member(body, info)
        crc_ = crc if info.filename == name else info.CRC
        us = usize if info.filename == name else info.file_size
        nm = info.filename.encode("utf-8")
        flags = info.flag_bits
        hdr = struct.pack("<4sHHHHHIIIHH", b"PK\x03\x04", 20, flags, info.compress_type, 0, 0x21, crc_, len(raw), us, len(nm), 0)
        out.write(hdr + nm + raw)
        central += struct.pack("<4sBBHHHHHIIIHHHHHII", b"PK\x01\x02", 20, 3, 20, flags, info.compress_type, 0, 0x21,
                               crc_, len(raw), us, len(nm), 0, 0, 0, 0, info.external_attr, off) + nm
    cd = out.tell()
    out.write(central)
    out.write(struct.pack("<4sHHHHIIH", b"PK\x05\x06", 0, 0, len(infos), len(infos), len(central), cd, 0))
    return out.getvalue()


def raw_member(body, info):
    loc = info.header_offset
    n, e = struct.unpack("<HH", body[loc + 26:loc + 30])
    return body[loc + 30 + n + e:loc + 30 + n + e + info.compress_size]


def comment(body, text):
    return body[:-2] + struct.pack("<H", len(text)) + text


def nul_name(files, dirs):
    body = export(files, dirs)
    with zipfile.ZipFile(io.BytesIO(body)) as z:
        infos = z.infolist()
    out = io.BytesIO()
    central = b""
    for info in infos:
        off = out.tell()
        raw = raw_member(body, info)
        nm = info.filename.encode() + (b"\x00junk" if info.filename == "workspace/raw/note.md" else b"")
        hdr = struct.pack("<4sHHHHHIIIHH", b"PK\x03\x04", 20, info.flag_bits, info.compress_type, 0, 0x21, info.CRC, len(raw), info.file_size, len(nm), 0)
        out.write(hdr + nm + raw)
        central += struct.pack("<4sBBHHHHHIIIHHHHHII", b"PK\x01\x02", 20, 3, 20, info.flag_bits, info.compress_type, 0, 0x21,
                               info.CRC, len(raw), info.file_size, len(nm), 0, 0, 0, 0, info.external_attr, off) + nm
    cd = out.tell()
    out.write(central)
    out.write(struct.pack("<4sHHHHIIH", b"PK\x05\x06", 0, 0, len(infos), len(infos), len(central), cd, 0))
    return out.getvalue()


def trailing_deflate(files, dirs):
    body = export(files, dirs, compression=zipfile.ZIP_DEFLATED)
    name = "workspace/raw/note.md"
    with zipfile.ZipFile(io.BytesIO(body)) as z:
        info = z.getinfo(name)
    return rebuild_member(body, name, raw_member(body, info) + b"\x00\x00", info.file_size, info.CRC)


def python_decision(module, body):
    try:
        files, dirs, _ = module.verify(body)
    except ValueError as exc:
        codes = {v: k for k, v in module.MESSAGES.items()}
        message = str(exc)
        if message not in codes:
            raise SystemExit(f"Python raised an unmapped message: {message!r}")
        return "reject: " + codes[message]
    return "accept " + module.listing_sha256(files, dirs)


def rust_decision(binary, body):
    proc = subprocess.run([binary, "verify"], input=body, capture_output=True, timeout=120)
    out = proc.stdout.decode()
    if proc.returncode not in (0, 1) or not out.endswith("\n"):
        raise SystemExit(f"binary failed: exit {proc.returncode}")
    return out[:-1]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--diary", required=True, type=Path)
    parser.add_argument("--bin", required=True)
    args = parser.parse_args()
    sys.path.insert(0, str(args.diary.resolve()))
    import os
    os.environ.pop("WORKSPACE_RESTORE_VERIFY_IMPL", None)
    module = importlib.import_module("agent.workspace_restore")
    out = []
    seen = set()
    for name, body, reason in cases():
        assert name not in seen, name
        seen.add(name)
        py, rs = python_decision(module, body), rust_decision(args.bin, body)
        if rs.startswith("accept") and rs != py:
            raise SystemExit(f"{name}: the binary accepts what Python decides as {py!r}: {rs!r}")
        if py != rs:
            if reason is None:
                raise SystemExit(f"{name}: Python {py!r} != binary {rs!r} with no declared reason")
            if reason == "mutant" and rs != "reject: invalid":
                raise SystemExit(f"{name}: mutant disagreement other than reject: invalid: {py!r} vs {rs!r}")
        elif reason not in (None, "mutant"):
            raise SystemExit(f"{name}: declared stricter ({reason}) but both decide {py!r}")
        case = {"name": name, "archive_b64": base64.b64encode(body).decode("ascii"), "python": py, "rust": rs}
        if reason not in (None, "mutant"):
            case["stricter"] = reason
        out.append(case)
    doc = {"version": 1, "reference": "noevia-services diary/agent/workspace_restore.py verify (python mode)", "cases": out}
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(doc, indent=1, sort_keys=True, ensure_ascii=True) + "\n")
    agree = sum(c["python"] == c["rust"] for c in out)
    print(f"{len(out)} cases, {agree} identical, {len(out) - agree} stricter; wrote {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
