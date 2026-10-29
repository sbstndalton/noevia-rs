# Native backend candidate

This isolated candidate composes the original Rust HTTP front with native execution. It has no
Node/Python business-execution fallback. It is **not a complete or qualified backend replacement**:
unsupported original contracts still refuse, and production packaging, pins, flags and original
checkouts have not been changed.

## Entry points

- `--init-original-state NEW_ABSOLUTE_DIRECTORY` creates a new, private directory containing the
  original cowork.db auth-v5 schema and original module tables. It generates a setup code in
  `first-run-setup-code` (0600), without logging it. `NOEVIA_SETUP_CODE` optionally supplies the
  code, primarily for synthetic tests. Existing directories refuse; this does not migrate an
  older installation. Existing Store/Writer schema and ownership guards validate the result.
- `--existing-front ABSOLUTE_JOURNAL_PATH` is the Linux integration candidate. Set `UI_DATA_DIR`,
  `NOEVIA_START_ID` (32 lowercase hexadecimal characters), `PUBLIC_ORIGIN`, `NOEVIA_WEB_DIST`,
  `UI_PORT`, `NATIVE_CANDIDATE_MODEL_ENDPOINT` and `INFERENCE_MODEL`. `INFERENCE_API_KEY` is optional.
  The original front config still requires `NOEVIA_LEGACY_UPSTREAM`; an inert loopback address such
  as `http://127.0.0.1:9` suffices because the candidate replaces that transport with an in-process
  native callback. It never contacts that address for business execution. UI binds loopback only.
- `--existing-projects ABSOLUTE_JOURNAL_PATH` serves the narrower native API with original project
  history, without assembling the complete original Rust front. Also Linux-only.
- `--init-experimental-state ABSOLUTE_PATH` and `--experimental-state ABSOLUTE_PATH` retain the
  explicitly separate early execution-store prototype. That schema is not an original-state
  migration and should not be chosen to evaluate frontend compatibility.

Fresh original setup, auth/CSRF, session revocation, native chat, original transcript persistence,
project source context, browser user-save ordering, process shutdown and the hardened storage
child reexec have synthetic actual-HTTP/process tests. They do not perform inference. Linux tests
are authoritative for original filesystem integration; macOS does not supply its Linux guarantees.
All builds/tests must use the workspace's existing `devrun` wrapper with explicit working directory.

## Integrated behavior

The original Rust account/auth/static/project front remains in place. The candidate adds native
model SSE/tool execution, scoped approval, bounded transport, live account tool-policy reads,
manual toolbox selection, original sampling presets and per-account sampling preference. Provider
wire fields are separated from UI transcript metadata. Native project context uses original
instructions, memories, framed text sources and `read_project_file`; the core time tool is native.

Original transcript writes retain the account Writer and canonical holder/generation/revision/
tombstone fences. Browser pre-saved current user messages are not duplicated; a sole queued user
save can be accepted without losing metadata, while unrelated edits conflict. Completed tool
rounds persist as original assistant `toolCalls`, with applied/declined/not-run status. Completed
write replay uses framed already-done notes; external engines receive no private tool arguments,
targets or results in those notes. Interrupted/edited/paused history normalization remains limited.

Partial text/reasoning/tool snapshots use the original partial-file format and start marker.
Periodic saves occur at event boundaries after four seconds. Future cancellation publishes an
unmarked interrupted partial; successful completion clears it. This is not full parity with the
original interrupted-transcript append path. Marker retirement or post-commit cleanup failures
remain errors, not proof of successful acknowledgment. Native SIGTERM/CTRL-C stops acceptance,
drains requests for five seconds and joins the actual Diary replay pass; a blocking SQLite pass
may take longer to finish.

Native workspace reads, backed-up administrator-only legacy root claim, transcript search and
retention use original stores. Native file-free project creation/configuration preserves original
records; unsupported upload/connector/routing combinations refuse.

## Diary, DAV and auxiliary authorities

`NATIVE_CANDIDATE_DIARY_REGISTRY` names an explicit trusted JSON file mapping user UUIDs to absolute
`corpus` and `journal` database paths. Native Diary files/editor/history/today/source/recovery and
workspace operations hold account/session/add-on/local-storage fences. The existing native corpus
and workspace kernels retain CAS, outboxes, Trash capsules and operation receipts. Workspace ZIP
export/import uses the original format and receipt-preserving apply engine.

`NATIVE_CANDIDATE_DIARY_LAYOUT` supplies `root`, `monthlyPrefix`, `entriesPrefix`, `monthFileTemplate`,
`indexFile` and `daily`. It enables original journal replay (`exchange`, `exchange_edit`,
`index_month`, `index_update`) in bounded passes every 30 seconds, preserving order, duplicate
markers, scaffold creation, quarantine and outage cooldown. It does not start inference/indexing.
`NATIVE_CANDIDATE_WORKSPACE_PROTECTION` supplies the existing workspace protection configuration.

Optional separate DAV configuration uses `COWORK_DAV_PORT`, `COWORK_DAV_SCOPE` and
`COWORK_DAV_ORIGIN`. Original transport/scope/app-password/account/code-network checks remain.
Native PUT/MKCOL/DELETE/MOVE/COPY call managed workspace authorities. Unconditional replacement
preserves the original Trash capsule before CAS. Pending journal/protected-file refusals and
credential revocation are tested. Complete mutation audit, cross-database audit atomicity and
assembled listener qualification remain incomplete.

The crate also contains native MCP session/tool transport with bounded cleanup, explicit job
leases/crash uncertainty, complete local SQLite snapshots, and a model/profile-separated index.
The index is an explicitly new schema, not legacy sqlite-vec migration. Local snapshots preserve
receipts but are not the original remote backup/outbox/restore lifecycle.

## Remaining authority

Full public API parity is unfinished. Provider/OAuth/model routing, thinking/compaction, skills,
attachments, Code/Cowork, source jobs, deep research, connector administration, complete feature
startup/public/private/device integration, Diary capture/preparation/orchestration, legacy vector
retrieval/reranking/grounding, remote receipt-preserving backup/restore, internal MCP/catalogue/
interposers, worker adoption and release lifecycle qualification remain incomplete. Existing
native Docling/Laya/model-manager/sandbox/code-egress components are reused where the front already
owns them; that does not prove their complete assembled release authority.

No narrow test, new schema, route count or component reuse establishes full completion. Original
source retirement, independent exact-head review, fresh CI, synthetic full Diary/live-model
qualification and production activation gates remain separate. The Mac application remains last.
