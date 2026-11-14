# Load advisor recordings (sbstndalton/noevia#2550 item 2)

The oracle for `model_maint::load_advisor` (the front's native auto-tune load advisor): core's own
`server/load-advisor.cjs` `createLoadAdvisor({ env }).judge`, with its real decision client
(`decision-endpoint.cjs` and what it requires) and the real load verdict (`dav-parse.wasm`
`loadVerdict`), against a scripted stub decision service on 127.0.0.1. `record-node.cjs` holds the
scenarios and writes `scenarios.json` and `SOURCES.sha256` (the core files it reads, at the recorded
commit). `tests/load_advisor_replay.rs` replays every scenario through the native advisor against
the same stub and compares byte for byte.

Re-record on the N5 builder (owner decision 19) from this checkout. `<core>` is a noevia-core
checkout (no `npm ci` needed: only core's own files and node builtins are loaded); `<rs-ref>` is a
noevia-rs checkout at `NOEVIA_RS_REF` from `<core>/server/dav-parse.lock`, from which the pinned
module is built (core refuses a module whose sha256 is not `DAV_PARSE_WASM_SHA256`):

    timeout 2500 ~/bin/n5run --with <core> --with <rs-ref> --pull --timeout 2400 -- 'set -e
      mkdir -p /tmp/r6-advisor; W=/tmp/r6-advisor/dav-parse.wasm
      (cd <rs-ref> && CARGO_TARGET_DIR=<rs-ref>/target tools/build-dav-parse-wasm.sh $W)
      F=bins/noevia-server/tests/fixtures/load-advisor
      DAV_PARSE_WASM=$W node $F/record-node.cjs <core> $F/scenarios.json
      DAV_PARSE_WASM=$W node $F/record-node.cjs <core> /tmp/r6-advisor/second.json
      cmp $F/scenarios.json /tmp/r6-advisor/second.json'

Recording twice gives byte-identical files. Each run takes about 15 s (three scenarios wait out the
2 s advice budget or the 5 s lane hold).

## Format (v1)

`{ "version": 1, "sourceSha256": { "<core path>": "<sha256>" }, "scenarios": [ ... ] }`, one entry per
scenario:

- `name`; `env`: what the advisor was given as its environment (`COWORK_DECISION_URL`,
  `COWORK_DECISION_MODEL`, `COWORK_DECISION_API_KEY`), the stub's port written `<port>`. The only key
  is the synthetic `qa-decision-key-1`.
- `stub`: `replies` by arrival order, each `{ status, headers?, body | bodyHex | bodyRepeat:{text,count},
  delayMs? }` (sent as `HTTP/1.1 <status> QA`, the headers, `Content-Length` unless 204,
  `Connection: close`), `{ reset: true }` (the connection is closed without a response) or
  `{ hang: true }` (never answered); `singleWorker: true` answers a request that arrives while another
  is unanswered with `503 {"error":"busy"}`; `closed: true`: nothing listens on the port.
- `calls`: `single` (one judge), `sequential` (the second judge after the first returned) or
  `concurrent` (the second while the first is open); `inputs`: the judge inputs.
- `requests`: what the stub received, in order: `method`, `path`, `contentType`, `authorization`
  (the header or null), `body` (UTF-8 text), `reply` (the scripted reply's index, or `"busy"`).
- `results`: `JSON.stringify` of each judge result, in input order.
