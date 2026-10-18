# Native public framework routing (#1318, #2053)

`native-routing` compiles the complete public API ownership set. The new selector
`MODEL_MANAGER_ROUTING_IMPL` defaults to python; rust requires every selector in
routing_cmd::REQUIRED to be rust. Startup first performs bounded read-only trusted-file validation
and config/root qualification. Missing/malformed trusted data, empty publisher sets, or a config
that would leave a public owner on Python refuses. The constructed native stores, sampler/runtime
sources, telemetry, observer and planners are checked again before orphan reap, embedding
seed/warm/watch, listener bind or child startup. Native database bootstrap and store lock acquisition
are preparation after pure prerequisites; this slice does not add transactional schema rollback.

The routing table follows actual API registration order. Full method matches win, otherwise the
first partial route supplies 405 and Allow (a settings/section405 advertises GET, not a union).
Only after no full/partial match does a slash-toggle match produce 307; every trailing slash is
removed. Framework decisions precede body and parameter validation; authentication precedes
framework decisions. The existing 8192-byte raw URI cap remains after authentication. Encoded
health spelling receives the same health authentication exception. HEAD responses have empty
wire bodies, including framework failures.

Paths follow one ASGI UTF8-replacement percent decode. Structural slashes become separators;
%252F becomes a literal %2F parameter. A canonical transport URI re-encodes every literal percent
and reserved/non-ASCII character so existing endpoint decoders do not decode twice. Original URI
and decoded route identity remain in the request extension. Queries are never decoded/rebuilt by
routing. Redirects reproduce pinned Starlette Location quoting using the decoded slash-adjusted
path, original query and Host; the transport URL parser is not used. Pinned Python route regex$
accepts one terminal newline on a static suffix, while a final dynamic parameter captures it.

Backend restart/test receives the decoded name. Effective-name membership, body-before-inventory
validation on test, exact action allowlist and Docker controls remain unchanged. Unknown names,
encoded separators and invalid requests cannot broaden control. Benchmark run integer grammar
remains in its separately qualified parser. The API manifest covers every existing public route;
health/logs and optional embedding/idle-unload are retained parent extensions. Missing native
owners refuse 503; public docs/OpenAPI stay outside the current parent contract and return 404.
No child lifecycle deletion is included. Python routing keeps the existing compatibility path.

The actual-ASGI generator uses services 60764e67f390ea9dad5c91c3f41aad865d757823,
FastAPI 0.115.6, Starlette 0.41.3, Pydantic 2.14.0/Core 2.50.0. It replaces endpoint execution only,
retaining actual registered routes/dependency validation/middleware and explicit raw_path plus
once-decoded scope.path; no TestClient normalization, startup, network or Docker occurs.
The 545-case fixture records framework versions, source hashes and ordered route registration.
Tests compare all framework answers, route matches, Location/Allow/query/auth/body ordering,
encoded backend actions and zero unapproved Docker calls with an unavailable child. Process
qualification verifies bad prerequisites never contact a synthetic Docker socket, initialize a
store or seed an embedding preset. Missing compiled capability refuses before startup too.

Release integration must compile native-routing and pass MODEL_MANAGER_ROUTING_IMPL through
both compose copies and the retained server override. Qualify all required selectors/roots and
trusted publisher data before selecting rust. Roll back by selecting python and restarting while
the compatibility child remains installed.

The reviewed benchmark run path/integer parser from rs#209 is normal-merged without reimplementation.
This branch starts at aggregate 01df44f and also normal-merges aggregate PR #210 at d3a8aa7 and reviewed
rs#211 at 9a74063 diagnosis mutation qualification. The only merge conflict retained two independent
bench-run regression functions at EOF; runtime route code merged automatically.
The remaining child dependencies are explicit follow-up work: supervised uvicorn lifecycle and
8091 readiness, per-store private startup-ready gates, backend cache/restart-error mirrors, and
Python startup INI/default seed behavior. These are not replaced by framework 404/503 responses.
