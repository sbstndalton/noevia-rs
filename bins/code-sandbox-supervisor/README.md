# Native Code sandbox supervisor (source-only candidate)

This binary is a source-only port of `noevia-core/code-sandbox/supervisor.cjs`.
It is not selected or packaged by any current image. No runtime flag changes here.

It accepts the existing one-line `{"noevia":"start","cwd":"...","env":{...}}`
handshake on the configured TCP listener, then forwards the ACP stream byte for
byte to one fixed harness command. It uses the shared Rust sandbox-bridge parser
for JavaScript-compatible start-message and environment filtering, resolves both
workspace root and requested cwd, and refuses a path outside the root. It spawns
the harness with a cleared environment and a fresh process group. A connection
close, child exit, wall timer, or supervisor shutdown terminates that group.

Configuration matches the current Node supervisor: `CODE_HARNESS_COMMAND` and
`WORKSPACE_ROOT` are required; `CODE_HARNESS_ARGS` is split on spaces;
`PORT` defaults to 8030; `CODE_SANDBOX_MAX_CONNECTIONS` defaults to 4;
`CODE_SANDBOX_MAX_WALL_MS` defaults to 2 hours. `HOME` falls back to the
supervisor's own HOME when the start message omitted it. The listener defaults
to 0.0.0.0, as the existing isolated image does. A partial start line times
out after 15 seconds, tighter than the old unbounded wait.

The listener has no authentication. It must only be considered for a later
image/Compose integration with the existing internal code network, unpublished
port, read-only root, uid 1000, capability drop, memory/pid limits and workspace
volume. The source-only candidate does not change any of those controls.
Realpath followed by process cwd selection has the same namespace-swap race as
the Node supervisor; deployment must retain a single-writer workspace namespace.
