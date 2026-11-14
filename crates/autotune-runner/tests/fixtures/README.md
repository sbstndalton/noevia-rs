# Node oracle traces for the auto-tune runner (R6)

`traces/NNN-<test>-<instance>.json`: one per `createFullAutotuner` instance that noevia-core's
`server/llamacpp-full-autotune.test.cjs` builds, and `traces/SNNN-<test>-<instance>.json` one per instance
`scenarios.test.cjs` beside this file builds. The format is defined in `FORMAT.md` beside this file.

Re-record (from a noevia-core checkout at the commit in `SOURCES.sha256`, `server/wasm/dav-parse.wasm` in
place or named by `DAV_PARSE_WASM`; on the N5 builder, owner decision 19):

    timeout 1700 ~/bin/n5run --with <core> --timeout 1600 -- \
      'node crates/autotune-runner/tests/fixtures/record-node.cjs <core> crates/autotune-runner/tests/fixtures/traces'

This replaces `traces/NNN-*.json` (core's tests) and `traces/SNNN-*.json` (`scenarios.test.cjs`), then
prints the node:test counts and the trace count and size per file.
`record-node.cjs <core> --baseline` runs the same test files without the recorder; the counts must match.
To check determinism, record a second time into another directory and `diff -r` the two.

- `trace-preload.cjs` is the `--require` preload that wraps the runner's dependencies. It does not
  change the core checkout.
- `scenarios.test.cjs`: the scenario tests (cancellation tokens that are no longer current, and the
  child aborts), recorded with `TRACE_PREFIX=S`.
- Every trace carries the sha256 of the runner and the test file it was recorded from. Re-record when
  either changes.
