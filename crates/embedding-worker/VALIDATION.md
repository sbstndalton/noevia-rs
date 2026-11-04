# Source packet verification

Joined Rust main 73441129af1d36f4b5f8e1da7673eb2aacc9d615 with a normal merge
onto the existing embedding-input-framing branch (merge 2e33266). Core read-only
reference is b91b3f0324bfd8daea0e7c7b5f3a40ad4cde91fc.

No full Cargo workspace suite, inference, model loading, feature flags, production
sign-in, deployment, push or pull request was performed. No server startup wiring
was added. These are synthetic source tests; independent review and CI remain.

All build/format/test commands below ran through devrun. Linux verification used
the assigned scratch checkout and the supplied ms02-rs-verify.sh, bounded by
`timeout -k 15 4200`, with its serialized target lock and two Cargo jobs.

| Command / operation | Result |
|---|---|
| `git merge --no-edit 73441129af1d36f4b5f8e1da7673eb2aacc9d615` | PASS, normal merge 2e33266 |
| Initial Mac `devrun cargo fmt -p embedding-worker -p prompt-framing` (two queued calls) | CANCELED before execution: shared Mac devrun lock; only owned waiting processes stopped |
| rsync source to assigned scratch, excluding target/.git; scp supplied verify script | PASS |
| Linux `timeout -k 15 600 ~/bin/devrun cargo fmt -p embedding-worker -p prompt-framing` | PASS, formatted source copied back |
| First supplied verify: packages embedding-worker,prompt-framing; lib:prompt-framing pkg:embedding-worker | PASS: fmt ok; clippy rc 0; prompt-framing 12 unit tests; worker 12 integration tests |
| Mac Core-oracle generator through bounded devrun | CANCELED before execution due to shared lock |
| First Linux Core-oracle generator | FAIL: redundant CommonJS `module` declaration; no runtime/product code implicated |
| Intermediate second verify queued while the oracle fixture was empty | CANCELED while waiting for target lock; no test result credited |
| Corrected Linux Core-oracle generator via `timeout -k 15 120 ~/bin/devrun node` | PASS: 198 cases from unmodified Core embedding-profile.cjs; JS whitespace, title slicing and lone surrogates |
| Final supplied verify: packages embedding-worker,prompt-framing; pkg:prompt-framing pkg:embedding-worker | PASS: fmt ok; clippy rc 0; all tests below green |
| `git diff --check` | PASS |

Final effective Rust commands inside the supplied script:

```text
cargo fmt -p embedding-worker -p prompt-framing -- --check
cargo clippy -q -p embedding-worker -p prompt-framing --all-targets -- -D warnings
cargo test -q -p prompt-framing
cargo test -q -p embedding-worker
```

Final results:

```text
fmt: ok
clippy rc 0
prompt-framing unit: 12 passed; 0 failed
prompt-framing embedding oracle: 1 passed; 0 failed (198 Core cases)
prompt-framing fixtures: 5 passed; 0 failed
prompt-framing properties: 5 passed; 0 failed
embedding-worker fill: 3 passed; 0 failed
embedding-worker worker/service: 12 passed; 0 failed
all doc tests: 0 failures
```

Coverage includes request framing, batch refusal fallback, invalid vector rejection,
config change during embedding refusing publication, tenant labels, active/next
ordering, refusal exclusion, pacing, unavailable/error continuation, due-only
retry, transient hold/recovery, drift after transient failure, previous identity
protection, between-batch served recheck, version abort, stop, coalesced kicks,
option bounds, served-id parsing and sticky/stale service status/protection.

Concrete HTTP/SQLite adapters, on-disk outbox/restart behavior and assembled runtime
acceptance are not tested by these trait fixtures. See README for retirement gates
and explicit behavior differences. Test stubs assert the publication fence contract;
they do not prove a future adapter implements it.
