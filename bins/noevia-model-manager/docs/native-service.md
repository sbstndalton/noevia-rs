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
Missing models.ini remains missing. service::Ready requires completed schema/seed and all
installed native owners. There is no private runtime fallback or invented empty preset.

## Shutdown

One eight-second total grace fits the existing ten-second container stop window. Stop
accepting requests, stop sampler/watch loops, request cancellation of warm/download/benchmark
jobs, then drain admissions/their registered work, diagnosis, and independent native workers.
Worker lifetimes register BEFORE spawning and release from the actual worker. Cancellation
alone is not completion. Both clean and deadline exits retain StateData/writer leases until
OS process termination. No destructor releases a lock while another worker is alive. On
deadline the process logs and exits; the kernel stops same-process workers before releasing
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
