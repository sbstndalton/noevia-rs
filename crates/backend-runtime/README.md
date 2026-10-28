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
contract has not been qualified. The native OpenAI provider now streams bounded SSE content/reasoning and assembles tool-call fragments. It requires a complete stop/tool_calls finish plus DONE; JSON-only or truncated provider responses are refused rather than committed.

The new native index does not import or mutate legacy sqlite-vec slots. Query embedding,
reranking, temporal/entity orchestration and the complete Diary HTTP service still need wiring.
Snapshot/restore is an explicit local SQLite recovery primitive, not an implementation of the
existing immutable remote backup-manifest/outbox contract. DAV DELETE/MOVE/COPY now call guarded native workspace kernels; full PUT replacement preservation semantics, audit and release lifecycle qualification remain unfinished. The general tool loop's context recheck does not by itself
provide an atomic final authorization fence inside arbitrary remote MCP tools.

No route count, narrow test result or reusable kernel proves these missing authorities complete.
Production migration, source retirement, exact independent review, Linux qualification,
synthetic full Diary/live-app validation and release gates remain required.

Additional candidate integration: `--existing-projects ABSOLUTE_JOURNAL_PATH` on Linux uses the existing project transcript authority (`UI_DATA_DIR`, `NOEVIA_START_ID`) rather than creating a second history store. Final commits hold the original transcript/list/move locks and check holder, generation, revision, tombstone and operation liveness. This Linux path and its original-authority regression test passed on the authorized isolated MS-02 test checkout; the baseline Linux-only filesystem dependency remains intact. Original public project routes are retained by the assembled front; non-basic chat execution remains incomplete.

`indexing::Indexer` performs native corpus parsing, bounded embedding requests, exact-version publication and vector search. Embeddings are permitted model-engine calls only. Publication holds the managed corpus writer, commits the index, then consumes the existing workspace index outbox; search excludes pending outbox entries while holding that writer. A synthetic concurrent-edit test covers stale publication and stale retrieval refusal. This is the candidate index schema, not migration of an existing sqlite-vec database; production worker activation, account/lifecycle authorization and complete Diary API contracts remain unqualified.

Original managed Diary HTTP contracts are now available through an explicit trusted tenant registry (`NATIVE_CANDIDATE_DIARY_REGISTRY`, absolute JSON file mapping canonical user IDs to absolute `corpus` and existing `journal` paths). `/api/diary/files` and `/api/diary/file` use actual account Writer/session/Diary-feature fences through the corpus operation; no remote-storage fallback is synthesized. Original editor size/version/missing-file/unchanged-save/listing behavior is covered with synthetic actual-auth SQLite fixtures. Full recovery/reindex scheduling and remote storage remain incomplete.

On Linux, `--existing-front ABSOLUTE_JOURNAL_PATH` additionally assembles the existing `noevia-server` front around this executor. Its original configuration requirements and native routes/static files/upkeep/code-network and HTTP connection limits remain. Its proxy object is replaced with a native in-process executor, including nested delegation sites; it cannot open the legacy upstream in this mode. The candidate listener stays loopback-only. Readiness is sent to the native executor and remains unavailable until its full contract is implemented; no upstream health success is reused. Production/default constructors and selected deployment configuration are unchanged. The candidate plus original frontend lib/binary/integration suite passed on isolated Linux at 5d3d3d1; this does not imply completion of the remaining API methods.

The native turn runner now owns the original per-turn tool-exchange result cache: canonical keys use the existing Rust tool-exchange authority, writes invalidate read results only at actual attempt, and uncertain failures are cached rather than automatically re-executed. Tool wire indices remain unique across model rounds. Tenant/scope-bound adapters are filtered before offering tools and checked again at invocation. Existing provider-egress Rust rules refuse Diary and unknown storage-root calls for external providers. Unknown engine provenance is conservatively external; complete original provider registry, routing/consent and sensitive-result handling remain unfinished. Stored private/unknown tool history is refused for external engines rather than silently disclosed.

The Linux assembled candidate can start the original separate DAV endpoint from `COWORK_DAV_PORT`, `COWORK_DAV_SCOPE`, `COWORK_DAV_ORIGIN`, and proxy-token settings. Its actual bind remains loopback-only. `NATIVE_CANDIDATE_WORKSPACE_PROTECTION` is explicit JSON matching the original workspace protection configuration; only its presence enables DELETE/MOVE/COPY. Mutation IDs are allocated once per admitted operation and uncertain requests are never automatically retried. Original account, credential, scope, local-storage and migration fences remain held through mutations; receipt/Trash preservation stays in the existing workspace kernel.

An explicit `NATIVE_CANDIDATE_DIARY_LAYOUT` JSON configuration (`root`, `monthlyPrefix`, `entriesPrefix`, `monthFileTemplate`, `indexFile`, `daily`) enables native replay of existing `exchange`, `exchange_edit`, `index_month`, and `index_update` journal intents. The assembled front schedules bounded passes every 30 seconds under the live account Writer fence. Replay preserves insertion order, duplicate markers, create-only scaffold files, CAS retries, dependent quarantine and outage cooldown. Existing corpora/journals remain explicit operator-selected paths; remote storage and full Diary request/orchestration remain incomplete. This worker does not activate the new retrieval schema or call an inference engine.

Free chat now reads the original sanitized account instructions and persistent memories under its live session fence, and includes computed calendar facts in model context. These transient system parts are not saved as transcript messages. Full project/attachment/skill context, provider-specific formatting and routing remain incomplete. Live HTTP session checks gate every disclosure and hold the account Writer through final original transcript commit.
