# Node oracle traces for the auto-tune runner (R6)

`traces/NNN-<test>-<instance>.json`: one per `createFullAutotuner` instance that noevia-core's
`server/llamacpp-full-autotune.test.cjs` builds. The format is defined in
`FORMAT.md` beside this file.

Re-record (from a noevia-core checkout with `server/wasm/dav-parse.wasm` in place):

    timeout 1200 ~/bin/devrun node crates/autotune-runner/tests/fixtures/record-node.cjs \
      <core checkout> crates/autotune-runner/tests/fixtures/traces

This replaces `traces/NNN-*.json`, then prints the node:test counts and the trace count and size.
`record-node.cjs <core> --baseline` runs the same test file without the recorder; the counts must match.
To check determinism, record a second time into another directory and `diff -r` the two.

- `trace-preload.cjs` is the `--require` preload that wraps the runner's dependencies. It does not
  change the core checkout.
- Every trace carries the sha256 of the runner and the test file it was recorded from. Re-record when
  either changes.
