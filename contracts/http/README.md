# HTTP contracts (full-Rust migration M0)

- `routes.toml`: every `/api/` path the web client (noevia-web `src/`, tests excluded, at the
  commit in `web.ref`) names, with the server that owns it. All `owner = "node"` until a Rust
  slice takes a route over. CI job `routes-check` fails when the client names a path missing here
  (`tools/contracts/routes_check.py`); paths are matched by shape, so `{id}` and `{chatId}` are the
  same parameter. Methods are not listed yet.
- `corpus/`: a synthetic recorded corpus. `exchanges/NNNNNN-METHOD-path.json` are request/response
  pairs written by noevia-core's opt-in recorder (`NOEVIA_CONTRACT_RECORD`,
  `server/contract-record.cjs`) while `tools/contract-corpus/generate.cjs` drove a server on a
  throwaway data dir with a mock model and Diary. Secrets are `<secret:N>`, ids `<id:N>`,
  timestamps `<ts>`; the same value has the same placeholder throughout. `manifest.json` names the
  inputs a replay must supply (the setup code file, the synthetic passwords). Refresh it with
  `tools/contracts/refresh-corpus.sh <noevia-core run id>`.

Replay against a server started on a fresh copy of an empty data dir:

```sh
mkdir -p /tmp/empty-seed
cargo run -p replay -- run --corpus contracts/http/corpus --base http://127.0.0.1:18021 \
  --seed /tmp/empty-seed --server-cmd 'node /path/to/noevia-core/server/index.cjs' \
  --server-env INFERENCE_BASE_URL=http://127.0.0.1:<mock> --server-env MODEL_MANAGER_KIND=none \
  --state-out node-state.json
cargo run -p replay -- coverage --corpus contracts/http/corpus --routes contracts/http/routes.toml
```

The server under test needs the same mock model and Diary the corpus was recorded against
(`generate.cjs` starts them; a replay harness that starts them too is follow-up work).
