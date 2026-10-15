# Native Docling service (#1322)

`NOEVIA_DOCLING_IMPL=python|rust` defaults to `python`. Python mode execs the
existing `server.py`; Rust mode requires `--features native-docling` and refuses
before listening or spawning a worker when the capability is absent. Native
errors never fall back to Python HTTP. `DOCLING_PORT` remains 8031 by default.

Rust owns HTTP validation, admission, generated temporary input, deadlines and
the warm child. The fixed command is `python isolated_worker.py --stdout-replies
extract:extract` in the service directory. Request values cannot choose a command,
target, working directory or input path. The Python extraction engine is retained.

Native startup reads no models. The real worker imports its target and returns
exactly `{"ready":true}`; the engine loads lazily on conversion. A failed start
is 503, conversion/invalid framing is 422, and a conversion timeout is 422 with
the existing time-limit message. Ordinary `{"error":true}` conversion errors
retain the warm child. Bad/truncated/oversized frames, timeout and cancellation
kill the owned process group before reaping. SIGTERM/interrupt also cancels
connections, stops the owned group and reaps its child. The single conversion
slot is released before writing the response.

Both full close and half close cancel conversion, matching the existing Python
owner's documented half-close limitation. A direct socket read observes EOF while
waiting for the engine; no assumption about a Hyper handler's cancellation is used.
Generated `noevia-docling-*` directories are removed on each outcome.

## Bounds and compatibility

- Existing 25 MiB input cap; declared size and name are validated before body allocation.
- Existing name limit: 200 Latin-1 header characters, no slash or backslash; only the
  suffix selects the engine parser. Generated filenames never use the supplied name.
- A stricter eight admitted connections, no unbounded queue, 16 KiB total headers,
  at most 99 header fields, and no transfer-encoding uploads.
- A hard 30-second header phase, 30-second body-and-private-file-write phase, and
  30-second response-write phase. These are stricter absolute limits than Python's
  socket inactivity timeout. No engine starts on an incomplete body.
- Existing long/short conversion defaults: 3600 seconds for PDF/TIFF, 600 seconds
  otherwise; the existing positive finite environment overrides are honored.
  One conversion deadline includes startup/readiness; readiness also has a 30-second cap.
  Unrepresentable/nonfinite override durations use the defaults rather than an infinite wait.
- Framed replies: four-byte big-endian length, checked before fallible reservation;
  readiness 64 bytes, results 32 MiB. The latter admits two million non-BMP characters
  escaped as surrogate pairs (24 MB), plus the 300-page engine envelope. Larger
  results are refused. JSON depth is at most 32 and structural punctuation at most
  20,000 before tree parsing, bounding tiny-value allocation overhead.
- One 25 MiB input and one framed conversion are active. Eight slow replies can each
  retain bounded JSON/string/serialized response copies; front memory is bounded
  independently of the engine's existing container memory limit. There is no
  unbounded stderr/stdout capture; ordinary engine output goes to the null device.
- JSON contracts compare decoded values, status and content type; wire serialization
  whitespace/escaping, Python's Server header and dynamic Date header are not claimed
  byte-identical. Content-Length is required to match the actual response bytes.
  Unsupported-method HTML is compared to the actual production Python 3.12 interpreter
  in mandatory Linux CI, not to a hand-authored oracle or local Python 3.14 template.

These are service contracts, not a real Docling inference qualification. All test
engines are synthetic. No model was imported, downloaded or loaded by these tests.

## Required qualification

Set `NOEVIA_DOCLING_REFERENCE` to `docling/` from actual merged services
`b45a122c04173a14e884fa1aa93a6a101281431d`, archive SHA-256
`3d69592f2b844b474a9ae0f0a6adb20ffa5be03de21e51039ff85305ebc64cff`.
Tests fail when the reference is missing; no silent skip. Source hashes are checked
by the Rust-owned test harness before the actual server/isolation/worker modules are copied to synthetic state.
Only the extraction collaborator is replaced. The Rust example owns source verification, preparation, cases, HTTP exchange and ordered
serialization. Fixed minimal Python glue loads the actual server and synthetic engine;
it defines no oracle cases or HTTP policy. The generator runs real localhost
Python HTTP requests and records 34 JSON cases, including all supported formats.

```
cargo run --locked -p docling-service --example gen-fixtures -- "$NOEVIA_DOCLING_REFERENCE" /tmp/docling-http.json
cmp /tmp/docling-http.json crates/docling-service/tests/fixtures/python-http.v1.json
cargo test -p docling-service
cargo test -p noevia-docling --test capability
cargo test -p noevia-docling --features native-docling --test capability
```

The unconditional `Native Docling HTTP and real worker transport` CI job executes
all three Rust commands, under Python 3.12, with the checksum-verified merged worker.
The positive binary test extracts via the real worker and proves graceful warm-child
cleanup; the omitted-feature binary test requires its specific capability error and
no child-spawn marker. Library tests exercise fragmented/truncated frames, readiness
failure, warm reuse, output limits, slow/incomplete bodies, connection admission,
full/half-close group cleanup and generated-directory cleanup. Properties check name
and input bounds. Native service delivery remains incomplete until the separately
reviewed services Docker wiring and release qualification land; S4 Laya is separate.
