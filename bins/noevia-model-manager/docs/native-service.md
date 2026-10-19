# Mandatory native service

Build the production executable with native-routing, the complete public API and helper
capability set. Missing capability refuses before any environment/store/listener/Docker
initialization. --features and --healthcheck run independently; the health command probes
/api/v1/health on loopback8090. The executable never starts Python/uvicorn or binds8091.

All MODEL_MANAGER_*_IMPL and MODEL_FILES_IMPL values are retired and ignored, including
invalid/python values. No additional permanent runtime mode exists. Frozen Python services
are test oracles. Startup requires TZ=UTC, qualified canonical MODELS_DIR, MODELS_INI_PATH,
DATA_DIR and bounded SEARCH_TRUSTED_PATH containing nonempty quantisers and labs. Existing
authentication/backend restrictions remain. Pure preflight precedes stores/Docker effects.

The secure SQLite worker initializes the full source-derived schema and prompt seed while
holding its lifetime writer lease. Existing schema migrations are additive/idempotent;
incompatible schemas refuse. Native INI bootstrap preserves stored idle preference
reconciliation followed by optional cache-ram migration, including best-effort error logging.
Store paths retain their existing safety policy: directories must be real, owned by root
or the effective runtime UID, and not writable by group/others. A root-owned sticky
temporary ancestor is allowed only when its immediate child is private (0700) and owned
by the effective UID. Ordinary non-temporary roots do not require mode0700. Existing
database/lease files retain their separate safe-file checks.

Missing models.ini remains missing. service::Ready requires completed schema/seed and all
installed native owners. There is no private runtime fallback or invented empty preset.

## Shutdown

One eight-second total grace fits the existing ten-second container stop window. Stop
accepting requests, stop sampler/watch loops, request cancellation of warm/download/benchmark
jobs, then drain admissions/their registered work, diagnosis, and independent native workers.
Worker lifetimes register BEFORE spawning and release from the actual worker. Cancellation
alone is not completion. Both clean and deadline exits retain StateData/writer leases until
OS process termination. No destructor releases a lock while another worker is alive. On
deadline the process exits124 without potentially blocking log I/O; the kernel stops same-process workers before releasing
leases. SQLite transactions preserve atomic rollback.

## Qualification

tests/native_service.rs runs the actual shipping binary with empty executable PATH, private
0700 temp stores, fake Docker and synthetic files. It checks CLI health/auth/encoded routes,
SQLite-blocked writer SIGTERM drain/second-writer exclusion, restart/idempotence, predecessor
INI byte-order cases, no invented preset/descendants, zero RPC to a separate8091 trap,
deadline exit and SQLite integrity. MODEL_MANAGER_PRODUCTION_BIN optionally chooses a
packaged executable; otherwise Cargo supplies it. Health uses the same helper. No shell or
kill utility is needed. The service unit-process test additionally blocks an independent
worker with an uncommitted SQLite transaction and proves deadline/lease/rollback integrity.

Predecessor lifecycle.rs at routing978660b asserted removed child negotiation, private port
and fatal-child-exit mechanisms. Replacements test mandatory capability, ignored keys,
native readiness and process drain/INI ordering. Advertisement/closed stdout/bounded oracle
pumping remain. routing_capability retains no-side-effect trust/preflight proofs; fullschema
tests retain actual Python fresh/legacy/idempotence/fault oracles. Sampler fixtures compare
the same source cache bytes directly and require zero mirror writes. Historical Rust child
transport fixtures are cfg(test) only. The diagnosis mutation gate replaces obsolete child
confirmation with production mandatory capability; the other four strict cases remain.

## Rollback

Qualify binary and Python-free image first. Keep the previous image digest/configuration/
backup, including old implementation keys: this binary ignores them, the prior image reads
them. Rollback restores that image/configuration, not a child mode in this binary. No
destructive database downgrade or additional permanent mode is introduced. Services image/
Compose/pins remain release-preparation scope. N5 remains sysfs-only; no real jobs in tests.


## Preserved qualification

retirement-test-mapping.json records every original integration source hash and
selected historical test name. The 58 archived integration contracts (including the
retired embedding child-environment mapping) live in tests/historical_oracles; the
additional backend sampler contract lives in
src/backend_stats/tests.rs::historical_backend_stats. All59 compile only into the
library's cfg(test) build. The two sampler replays share a test-only async lock
for their complete fixed-port fixture lifetimes, including awaited listener teardown.
The historical named wrapper calls the preserved original body in an inner helper.
Their recorded API response, file-tree, revision and source-fixture assertions
remain unchanged. The required combined-runtime runner compares the exact named
inventory against this manifest before running all emitted harnesses. No
production feature or environment key enables these historical transports.

The remaining native tests stay in integration targets using the non-test
library. Missing owners now explicitly answer 503 without a child connection;
wrong methods remain 405 and private routes remain 404. Backend restart fixtures
compare the same recorded errors with the native cache and require zero mirror
calls. Component startup targets now cover ignored retired keys, mandatory
capability/TZ preflight and malformed/empty trust without database/INI creation.
The old child supervision startup cases are replaced by the shipping process
contract (including ten INI ordering cases) and actual worker/deadline/lease
contracts. Existing frozen Python schema, response and file oracles remain.

The shipping process contract matches LISTEN socket inodes to its own descriptor
table, requiring exactly port8090; another process's private-port trap is excluded.
The sections mutation control changes the actual production bind8090 to8091 and
must fail this owned-port assertion. This replaces the retired test-only
child-port mutex mutation; the auth, metadata and file-boundary controls remain.

The download transfer integration replays the same frozen plans directly into the real
native queue while retaining all job/file/history/wire assertions. Native Planner has its
separate actual-source parity suite; a production-route partial delete body test installs
both native Planner and native delete owner and proves queue progress and zero private RPC.

The original backend sampler differential also remains required under a cfg(test)-only
historical adapter that sends the former mirror after each real native tick. Its native
variant compares the identical recorded cache/public responses directly and requires zero
private HTTP requests. No production switch or mirror sender is restored.
