# Native benchmark observations (noevia#1318 step 3f)

Native observation is always installed on the native benchmark runner when the
`native-bench-observe` feature is compiled (noevia#1071); the former
`MODEL_MANAGER_BENCHMARK_OBSERVE_IMPL` selector is retired and ignored, and there is no
separate observer selection. It requires a native benchmark runner and the actual
native backend sampler, which `native-routing` provides. `--features` advertises
`native-bench-observe`. Observation does not start a benchmark. Rollback restores the previous
image and configuration, without changing the job/store schema.

Native observation reads cached sampled usage, not a true peak. It never refreshes hardware
per request. It then makes a separate timestamped stdout+stderr tail-400 read for contention,
and, only for a successful cold request, a tail-800 read for load duration. Both use read-only
Docker inspect/log GETs. A configured/effectively discovered backend name is required. Separate
tails and their order matter: a larger tail changes the attribution context at its boundary.
A missing cache/GPU gives `[]`; absent/unreachable log data gives false contention or null load
as in Python. Every field is computed locally once selected; no native fault calls the child.

Bounds are deliberate extra refusals: 8 MiB per raw/decoded log tail; 1,600 logical lines;
64 KiB per line; 256 GPU cards; 64 KiB observation query; 1,024 aliases; 10 seconds for the
whole observation (including admission and identity discovery). A bound, unsupported timestamp
or parser shape, or deadline returns an explicit benchmark job error, rather than silently
recording an uncontended result. One tail is parsed and released before the next is read.
Cancellation drops pending async log reads; an already admitted bounded CPU parse can finish
on the blocking pool. Runner cancellation is observed at its existing unit-of-work boundary,
so a pending observation lasts at most the observation deadline. No per-request hardware exec
or service mutation is added. Existing shared blocking-pool admission bounds parser workers.

The load parser keeps the earliest start of the first completed episode, including eviction
and repeated waiting markers, and rounds milliseconds as Python does. It deliberately does
not infer time from first-token latency or choose a different episode. The shared native
telemetry parser handles contention without a database attribution callback.

`tools/gen-bench-observe-oracle.py` imports the actual services `bench.py` and `telemetry.py`
with synthetic cache/Docker adapters, records source SHA256s, and never opens a real store,
container, hardware device or network connection. It reuses synthetic router logs from the
existing telemetry oracle. CI regenerates/compares the fixture against services
`e5f0c045a17a0331f4d660d4d0147fca11dc67dc`, tests native and omitted capability builds, and runs
the runner/sweep differential suites unchanged. The production Docker transport's allowlist
accepts only the two exact finite log-tail queries (no follow/stream wildcard).

Remaining: Python database initialization/migrations and other private collaborators are not
retired here. The Python compatibility runner still needs its observation helpers. Removing
that runner/helper and its private route requires a separate retirement/release decision.
