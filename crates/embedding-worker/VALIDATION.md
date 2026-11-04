# Source packet verification

The first section records the initial **714384c** unwired packet. The continuation
section supersedes its implementation status; see OWNERSHIP.md for the current
conditional wiring and paired Core requirements.

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


## Concrete adapters and lifecycle continuation

No Core embeddings worktree existed, so Core was read-only and no worktree was
created. Rust remained on the assigned branch; no additional merge, push, PR,
deployment, feature-flag edit or real engine/model/embedding run occurred.

The continuation adds bounded reqwest adapters, renewable maintenance-only
embedding admission, real Node-format SQLite/vec0 publication, conditional server
startup/shutdown, Core ownership proof, statically pinned sqlite-vec 0.1.9, and
an unsafe-code guard. Root supplied read-only deployed Python/Node 0.1.9 evidence;
per-connection vec_version checks refuse drift. Diary's >=0.1.6 requirement remains
unpinned, for root's separate issue.

Commands were bounded and used the assigned scratch directory; every Rust
build/format/test ran through devrun. Source rsync excluded .git and target.
The supplied verifier used its shared target flock and two Cargo jobs. All waits
were bounded by timeout; canceled processes below were owned by this worker.

| Continuation command / stage | Result |
|---|---|
| rsync source; scp supplied verifier; bounded Linux devrun cargo fmt for changed packages | PASS, repeated after substantive changes |
| Verify embedding-worker,noevia-server, pkg:embedding-worker (first adapter run) | FAIL: SQLite COUNT decoded as usize; unused import. Both fixed |
| Same verifier after fixes | fmt/clippy PASS; fill3/HTTP4/SQLite5 PASS; unsafe guard FAIL on existing mem-return exception |
| Focused cargo test -p embedding-worker --test unsafe_guard -- --nocapture, under bounded target flock/devrun | FAIL: identified existing mem-return unsafe block; no additional code introduced there |
| Initial focused diagnostic started without explicit shared target | CANCELED; no test result credited; rerun used shared target |
| Verify with engine-admission and native wiring tests | fmt/clippy PASS; HTTP5/SQLite7/engine10/wiring2 PASS; unsafe guard still FAIL pending explicit existing-path policy |
| Intermediate verifier after adding positive startup test | FAIL: test JSON indexing lint plus existing unsafe guard; test changed to get(), guard updated under root's three-path instructions |
| Queued owned verifier runs replaced before target-lock acquisition | CANCELED to synchronize finalized source; no test results credited |
| Full verifier before historical-schema addition | PASS: fmt, clippy rc0; embedding30, prompt-framing23, engine-admission10, native wiring3, GGUF hostile14; zero failures |

The full successful verifier command (also used for final source) is:

```text
timeout -k 15 4200 bash ms02-rs-verify.sh <assigned-scratch> \
  embedding-worker,noevia-server,engine-admission,gguf-memtest,prompt-framing \
  pkg:embedding-worker pkg:prompt-framing lib:engine-admission \
  test:noevia-server:embedding_background test:gguf-memtest:hostile
```

It runs these effective checks through devrun:

```text
cargo fmt -p embedding-worker -p noevia-server -p engine-admission -p gguf-memtest -p prompt-framing -- --check
cargo clippy -q -p embedding-worker -p noevia-server -p engine-admission -p gguf-memtest -p prompt-framing --all-targets -- -D warnings
cargo test -q -p embedding-worker
cargo test -q -p prompt-framing
cargo test -q -p engine-admission --lib
cargo test -q -p noevia-server --test embedding_background
cargo test -q -p gguf-memtest --test hostile
```

The new positive lifecycle test runs for about 30 seconds against synthetic HTTP
stubs: immediate service observation, three/four refreshes, no migration before
30 seconds, a served-model lookup/empty pass thereafter, and joined shutdown.
It sends no /v1/embeddings request. Separate HTTP fixtures exercise only synthetic
vectors. SQLite fixtures reuse server-store's Node schema and the real Rust
outbox initializer, load pinned vec0, check f32 blob/map/legacy formats, refuse
stale hashes/widths/versions/stops/deleted users/projects, and verify publication
blocked by a concurrent outbox transaction rejects the committed tombstone.
No job state/attempt/generation is acknowledged or changed.

The final scoped rerun qualifies Node's historical additive chunk columns and
final native status snapshot fields. Terminal results:

```text
fmt: ok
clippy rc 0
embedding-worker: 31 passed; 0 failed
prompt-framing: 23 passed; 0 failed (198 Core framing oracle cases)
engine-admission --lib: 10 passed; 0 failed
noevia-server embedding_background: 3 passed; 0 failed; 30.22s
GGUF hostile: 14 passed; 0 failed; 3.49s
all doc tests: 0 failures
TOTAL: 81 tests passed; zero failures
```

`git diff --check` passed after copying the formatted checked sources and lockfile
back from the assigned scratch checkout. No full Cargo workspace suite was run.
No owned verification processes remained after terminal completion.


## Independent-review remediation (reviewed base e4e1789b)

P1/P2/P3 regressions were added before the behavior corrections. The reviewed
product source was also exported with `git archive e4e1789b` into the assigned
verification scratch directory, overlaid with the new fixtures/test-only Cargo
features, and tested there. The working branch was never reset/rebased/stashed.
The fixed source was restored from its owned worktree before green qualification.

| Finding | Red evidence | Corrected behavior / green fixture |
|---|---|---|
| P1-1 | Three initial startup regressions fail; exact reviewed source later has 2 passing / 8 failing server lifecycle tests | Damaged saved config, unknown env profile, absent pairing IDs/data root and lock contention stay nonfatal; authenticated session HTTP stays available; saved repair and draining-lock takeover recover |
| P2-1 | Reviewed runtime proof-loss fixture times out | 404 -> 200 suspends/resumes and completes next-slot publication; unconfirmed-since is set then cleared; stop remains shutdown-only |
| P2-2 | Catalogue-free existing index selection fails | All live, non-tombstoned existing indexes covered; no catalogue requirement; shared RAG_DIR outside data root qualified |
| P2-3 | Read fixture misses 300ms deadline behind an account IMMEDIATE transaction | Selection/remaining use deferred read-only RAG and nonlocking liveness checks; only publication takes account write fence |
| P3-1 | Two negative guard fixtures fail under old policy | Contract/token/escape/ABI budgets, explicit existing export paths, include/path validation and isolated loader; workspace forbid restored |
| P3-2 | Both reviewed admission fixtures fail (queue-inflated pause, stop not cancelling entry) | Busy measured after grants; 200ms queue produces minimum-duty pause; stop cancels pending entry promptly |
| P3-3 | Reviewed publication/fallback fixtures and kill-boundary assertion fail | Fresh proof immediately before publication and between fallback singles; stale proof after fence wait rolls back/suspends; actual SIGKILL leaves no vectors and restart completes migration |
| P3-4 | Orphan slot publish, integer-valued floating index ordering, no-loader owner fixtures fail | IF NOT EXISTS; JS integer ordering including 1.0; migration independent of loader; paired-only admission variant; process-wide auto_extension documented |
| P3-5 | Missing qualification supplemented | Actual Node joinSql KNN and embedRowsInto with concurrent chunk/vector replacement; Python vec0 reader; owned native process kill/recovery; full runtime::start migration through synthetic HTTP and real temp vec0 |

Exact reviewed-source red run, all through the supplied verifier/devrun:

```text
fmt ok; clippy rc 0
embedding-worker admission: 0 passed; 2 failed (expected red)
embedding-worker HTTP: 4 passed; 1 failed (1.0 index ordering)
embedding-worker runtime: 1 passed; 5 failed (expected red)
noevia-server embedding_background: 2 passed; 8 failed (expected red)
```

Earlier focused red runs also recorded SQLite 8 passed / 3 failed (coverage,
account lock, orphan tables) and unsafe guard 2 passed / 2 failed. An initial
fixture build lacked tokio rt-multi-thread; that test-only feature was added before
behavioral red evidence was credited. Runtime shutdown panics in failing baseline
fixtures are not counted as separate defects or green evidence.

Green checks use this command, foreground on the assigned Linux scratch checkout:

```text
timeout -k 15 4200 bash ms02-rs-verify.sh <assigned-scratch> \
  embedding-worker,sqlite-vec-loader,noevia-server,engine-admission,prompt-framing,gguf-memtest \
  pkg:embedding-worker lib:sqlite-vec-loader lib:engine-admission pkg:prompt-framing \
  test:noevia-server:embedding_background test:gguf-memtest:hostile
```

The supplied script runs package-scoped fmt/clippy --all-targets -D warnings and
the listed focused tests under its serialized target lock and two Cargo jobs.
No workspace suite, model, production sign-in, feature flag or deployment ran.
Node/Python only read/write synthetic vectors in temporary SQLite fixtures.
The test extension is built from the exact Cargo-resolved sqlite-vec 0.1.9 C source;
production remains statically registered and accepts no extension-library path.

Commit stages: d8609a7 optional startup; 8715bc3 coverage/read fences and config
repair correction; f4ed9a5 reversible proof suspension and publication/fallback
checks; ae0c6b3 isolated loader/guard restoration; final follow-up covers admission,
loader-independent migration, lock-drain recovery and cross-runtime proofs.

No Core/Top/release-kit edits were made by this worker. Required counterpart
front-stop-before-Core-rollback enforcement and Core config-repair consistency
were sent directly to its assigned worker. Exact-head independent review, CI and
coherent-pair release qualification remain separate gates before promotion.


Final remediation verification completed successfully (same foreground verifier
command above). Terminal results:

```text
fmt ok
clippy rc 0
embedding-worker: 49 passed; 0 failed
  admission 2; fill 3; HTTP 5; Node interop 2; runtime/process 7;
  SQLite 13; unsafe guard 5; worker/service 12
sqlite-vec-loader: 1 passed; 0 failed
engine-admission: 10 passed; 0 failed
prompt-framing: 23 passed; 0 failed (198 Core oracle cases)
noevia-server lifecycle: 10 passed; 0 failed (30.27s)
GGUF hostile: 14 passed; 0 failed
TOTAL 107 passed; 0 failed; doc tests 0 failures
```

The final run includes draining-lock acquisition/recovery, missing startup IDs/data
root, proof-age rejection after fence waits, and confirmed-status recovery after
transient suspension. Formatted sources and the resolved lockfile were copied
back from the verified scratch checkout. No owned verification/fixture process
remained. `git diff --check` passed. Exact-head independent review is required
before the user-authorized single normal push; CI/release promotion remain root's.
