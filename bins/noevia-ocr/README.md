# Native OCR sidecar (S3)

Build `cargo build --release -p noevia-ocr --features native-ocr`. Without the explicit build
feature, startup and capability advertisement fail closed. `--features` prints `native-ocr`;
`--healthcheck` checks loopback port 8030 within three seconds, accepting Python HTTP/1.0 and
Rust HTTP/1.1. The listener defaults to `0.0.0.0:8030` (`NOEVIA_OCR_LISTEN` overrides it for tests).

This binary owns `/health`, `/extract`, `/extract-docx` and `/reduce-pdf` directly. It uses
`docx-text` as a Rust library and only the external poppler, tesseract (eng+deu), and ghostscript
engines. No Python, Node, wasm, pdfium or pdf.js participates in native requests. Deployment
selection is owned by noevia-services/integration and remains Python by default.

The reference is noevia-services `792ecf5c30653b61a3357b2f45d8cc41b3b292c5`.
A single processing slot, 25 MiB normal / 60 MiB reduction input, 50 unique integer pages
between 1 and 300, 30-second body read, 600-second OCR / 160-second reduction budgets,
step timeouts, error objects, Unicode trimming and scalar-character text truncation follow it.
Children run in private process groups; timeout/cancellation/success kill remaining descendants,
and the direct child is reaped. Temporary files are removed on all exits. Logs never contain
document content or subprocess stderr.

Additional hardening: 32 connections, 32 headers / 32 KiB header buffer, 30-second headers,
one request per connection, 665-second whole-connection lifetime. Private stdout/stderr and
artifacts are checked every 20 ms and fail closed on excess (4 MiB OCR stdout, 1 MiB diagnostics,
64 MiB raster, 190000 bytes native text, 25 MiB reduced PDF). File reads always take cap+1 bytes
before checking; no output is read into an unbounded buffer. Disk output can exceed its threshold
between checks; these are refusal thresholds, not an OS disk quota. Existing `docx-text` strict
ZIP/XML refusals continue (documented in that crate). Unsupported methods receive JSON 501
rather than Python's HTML 501. Hyper rejects ambiguous HTTP framing before handler dispatch.

CI runs all existing front corpus jobs unchanged and separately tests this sidecar against the
actual pinned Python server. Synthetic DOCX and HTTP refusals compare status/JSON; real scanned,
mixed, encrypted and malformed PDFs compare OCR JSON. The scan's empty native text layer and
a deliberately dropped OCR result are negative controls. Real oversized reduction compares kind,
note, output limit and retained native text; compressed PDF bytes vary with engine metadata and
are not compared byte-for-byte. Proptest fuzzes page-header validation. Local engine qualification
requires `NOEVIA_OCR_REFERENCE=/path/to/noevia-services/ocr NOEVIA_OCR_REAL_ENGINES=1 cargo test
-p noevia-ocr --features native-ocr`; missing engines cause real-engine assertions to fail.
Without these variables the reference/engine test emits an explicit skip (CI supplies both).

Outstanding qualification: CPU/RSS of external engine children is bounded by image limits rather
than per-child rlimits, which this safe Rust launcher does not set. Real 600-second exhaustion and
network header saturation are not waited out in unit tests. Services CI owns hardened-image boot
and switching both paths; the existing front replay does not prove OCR switch execution.
