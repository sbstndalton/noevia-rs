# Native backend candidate

This experimental crate contains native execution implementations, not an adopted backend
replacement. No production route ownership, release pin, feature flag or legacy writer has
been changed. It must not be represented as full API parity or installed as a production server.

The candidate includes:

- Existing native account/auth handlers composed with HTTP chat and approval endpoints.
- Model/tool conversation execution, cancellation, scoped approvals, context-change fencing,
  durable turn state and conditional transcript commits.
- Native bounded HTTP and MCP sessions, initialized tool listing/calls and explicit endpoint
  capabilities; no Node/Python execution fallback.
- Durable job claiming, lease-fenced completion and explicit crash uncertainty.
- Diary journal operations over the original journal schema, including insertion-order replay,
  quarantine, dirty-document state and exact-identity enqueue retries.
- Managed corpus reads/writes, direct calls into the existing workspace transaction engine,
  a native model/profile-separated retrieval index and content-hash-bound vector selection.
- DAV backend operations holding the existing account Writer transaction through corpus access
  and commit; the separate workspace operation entry uses the same revocation fence.
- Whole-SQLite snapshot/restore preserving receipts and every other table.

`native-backend-candidate --init-experimental-state ABSOLUTE_PATH` explicitly creates an empty
candidate execution database. `--experimental-state ABSOLUTE_PATH` opens one alongside an
existing auth database selected by `UI_DATA_DIR`. The candidate requires an explicit
`NATIVE_CANDIDATE_MODEL_ENDPOINT` and `INFERENCE_MODEL`, uses optional `INFERENCE_API_KEY`, and
binds loopback only (`NATIVE_CANDIDATE_PORT`, default 18021). The executable does not initialize
or migrate auth/project/Diary data. It starts with no registered tools. Tests use owned temporary
SQLite files, actual loopback HTTP and deterministic fake models; they perform no inference.

## Remaining integration and correctness work

This is **not** a drop-in replacement. The separate execution and retrieval schemas require an
explicit, reviewed migration from current stores; there is no automatic adoption. Basic free
chat is the only newly exposed chat mode. Skills, attachments, Cowork, routing decisions,
provider/OAuth lifecycle, model controls, connector administration, project/source/research
executors and many other existing public routes are not ported. The existing frontend's full
contract has not been qualified. Streaming currently emits completed provider responses rather
than forwarding provider token streams.

The new native index does not import or mutate legacy sqlite-vec slots. Query embedding,
reranking, temporal/entity orchestration and the complete Diary HTTP service still need wiring.
Snapshot/restore is an explicit local SQLite recovery primitive, not an implementation of the
existing immutable remote backup-manifest/outbox contract. DAV DELETE/MOVE/COPY HTTP dispatch,
full replacement preservation semantics, symlink/race hardening and actual Linux lifecycle
qualification remain unfinished. The general tool loop's context recheck does not by itself
provide an atomic final authorization fence inside arbitrary remote MCP tools.

No route count, narrow test result or reusable kernel proves these missing authorities complete.
Production migration, source retirement, exact independent review, Linux qualification,
synthetic full Diary/live-app validation and release gates remain required.

Additional candidate integration: `--existing-projects ABSOLUTE_JOURNAL_PATH` on Linux uses the existing project transcript authority (`UI_DATA_DIR`, `NOEVIA_START_ID`) rather than creating a second history store. Final commits hold the original transcript/list/move locks and check holder, generation, revision, tombstone and operation liveness. This Linux path and its new original-authority regression test are **not compiled or executed on this Mac**; the baseline Linux-only filesystem dependency remains intact. Original public project routes and non-basic chat modes still require front integration.

`indexing::Indexer` performs native corpus parsing, bounded embedding requests, exact-version publication and vector search. Embeddings are permitted model-engine calls only. Publication holds the managed corpus writer, commits the index, then consumes the existing workspace index outbox; search excludes pending outbox entries while holding that writer. A synthetic concurrent-edit test covers stale publication and stale retrieval refusal. This is the candidate index schema, not migration of an existing sqlite-vec database; production worker activation, account/lifecycle authorization and complete Diary API contracts remain unqualified.
