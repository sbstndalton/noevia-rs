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
supervisor's own HOME when the start message omitted it or sent an empty value. Configured connection and wall limits must be positive decimal integers;
noninteger and scientific-notation strings fail startup explicitly rather than
silently falling back to a looser default. The listener defaults
to 0.0.0.0, as the existing isolated image does. A partial start line times
out after 15 seconds, tighter than the old unbounded wait. If
SANDBOX_BRIDGE_IMPL=rust, the child receives that flag so pi-acp-bridge keeps its
pinned WASM framing path. A later image integration must verify the pinned module
at startup; this source-only binary does not perform the Node loader preflight.

The listener has no authentication. It must only be considered for a later
image/Compose integration with the existing internal code network, unpublished
port, read-only root, uid 1000, capability drop, memory/pid limits and workspace
volume. The source-only candidate does not change any of those controls.
Realpath followed by process cwd selection has the same namespace-swap race as
the Node supervisor; deployment must retain a single-writer workspace namespace.

After a child exits, queued stdout may still be in the pipe or a slow client's
TCP path. The supervisor keeps forwarding while writes make progress. It closes
a quiet inherited stdout pipe after 100 ms. A blocked client write times out
after 2 seconds and the connection is reset so incomplete ACP output cannot
look like clean EOF. Group SIGKILL still runs after the configured grace while
the leader remains unreaped, and final child.wait follows that signal.
