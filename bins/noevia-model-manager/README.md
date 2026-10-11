# Native model-manager: health and backend logs

Refs noevia#1319 and #1318. This binary owns only `GET /api/v1/health` and
`GET /api/v1/backends/{name}/logs`. The other 41 pinned Python endpoint/method pairs
remain Python-owned and are explicitly listed in `python_owned`. Full S1 remains open.

`MODEL_MANAGER_IMPL=python` is the default and execs the existing uvicorn app unchanged.
`MODEL_MANAGER_IMPL=rust` requires the `native-logs` compiled feature and valid configuration
before any child starts. Rust supervises one existing Python app on fixed private loopback
port 8091; Python retains its existing DB/seed/reap/sampler/INI startup for unported work.
The native process does not duplicate that business startup. The corresponding services
patch refuses health/logs in the Python child via `NOEVIA_NATIVE_MODEL_MANAGER_CONFIRMED=1`.
Rust-owned errors never proxy to Python. The public administrator/session/tenant gate
remains in the current public server; the service independently owns its shared-token gate.
Activation and release pins belong to the owner and primary; this source change activates none.

The service reads Docker only through a Unix socket (default `/var/run/docker.sock`).
It never offers Docker writes, exec, model loads, inference, downloads or deletion.
Bollard's model/query types are reused. Its aggregate JSON reader and log codec buffer
untrusted responses/declared frames before yielding, so bounded native Hyper ingress and
explicit frame decoding replace those codecs. No extra HTTP proxy is inserted.
Only exact GET list/inspect/image-inspect/log operations are admitted. First image RepoTag
is used for discovery, including stopped containers; explicit configured names remain allowed.

Limits: four concurrent HTTP requests, five seconds per native logs request, 600 seconds
for Python-owned traffic (the existing public proxy budget), 8 KiB URI and Docker
header parser buffer, 32 Docker headers, 1 MiB Docker response before parsing, 1,024
containers (a bounded serde visitor refuses a sentinel before growing its inventory), 256-byte container names,
1 MiB log input, 2 MiB redacted output, 4,000 returned lines, and 4 KiB search text.
Python-owned request/response proxy bodies are capped at 1/16 MiB. Docker errors are
sanitized; redirects, ambiguous framing, invalid/truncated frames and cap breaches refuse.
These caps and Unix-only transport are deliberately stricter admission than Python.

Redaction precedes searching and level filtering. Regex matching is linear and uses no
lookarounds. Redaction is deliberately broader for numeric credentials, embedded token
shapes and Unicode word boundaries; established `n_*`, `tokens`, `token_count` and numeric
session counters retain their old text. Python's additional ignore-case I variants are
explicitly admitted. Non-ASCII search filters are refused because contextual Unicode
lowercasing differs; UTF-8 replacement decoding and all Python splitlines delimiters remain.
Tail accepts bounded ASCII integer syntax (including arbitrarily large magnitudes that
clamp), with zero mapped to 400; invalid syntax returns a sanitized 422 detail rather than
FastAPI's input-echoing validation object.

Qualification uses the actual pinned Python `api.py` SHA
`cbd633a8f9e150ff8f93fa3191bd7b4dd9278c43d093b19b8d1686ce1f979a19` from services
`a5161c1d404aefce0928a8c00c64093fd19bc48d`. `generate-oracle` evaluates only the actual
AST redaction/log functions with synthetic in-memory collaborators, without importing the
app or invoking startup. CI regenerates its exact fixture. Properties cover secret probes
and hostile frames; tests use only owned Unix fake peers and a test-only uvicorn executable.
Both compiled-feature variants prove selector/child behavior. No live Docker or model activity.

The services Docker build/pin change follows a reviewed, merged Rust SHA; the existing
services default Rust pin does not contain this binary. This Rust PR only references #1319;
the linked services integration must qualify before closing it.

Unicode hardening (#1325): regex word classes explicitly use Python letter/number/underscore semantics and whitespace includes U+001C–U+001F. Counter exemptions require single-word ASCII letter/digit/underscore keys. AWS access-key prefixes are scrubbed even before word-character suffixes. The bounded payload is scrubbed before Python-compatible line splitting, then each line again; this intentionally redacts more than Python when control whitespace separates keys and values across lines. Adversarial fixtures retain actual Python output for comparison and assert the stricter native no-secret output and empty secret-query result.
