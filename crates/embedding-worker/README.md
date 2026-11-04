# Embedding background worker candidate

The initial unwired packet is now extended with concrete bounded HTTP and Node-format
SQLite adapters and conditional noevia-server startup/shutdown wiring. Only service
refresh requires MODEL_LOADER_URL; migration requires authenticated current-start
proof that paired Core has stopped its workers. This branch does not edit Core.
An unmodified Core returns no proof and retains Node ownership. See [OWNERSHIP.md](OWNERSHIP.md) for the
exact paired diff, guard matrix, loader review and rollback risks.

## Core behavior contract

All references below are to Core **b91b3f0324bfd8daea0e7c7b5f3a40ad4cde91fc**.

| Area | Contract and source |
|---|---|
| Startup | `server/index.cjs:235-253`: load/install saved embedding config, initialize RAG with tenant and inference guards, asynchronously guard seeded addresses, install migration and service. Only when MODEL_LOADER_URL exists, refresh immediately after native readiness and every 10s. Register onIndexed to kick migration only while next exists. Saved next triggers a one-shot 30s startup kick. |
| Service input | `server/embedding-service.cjs:30-53`: MODEL_LOADER_URL, optional MODEL_LOADER_TOKEN, EMBEDDING_SERVICE_URL. GET loader `/api/v1/embedding-service`, JSON content type and optional X-Model-Loader-Token, timeout 6000ms. Refresh coalesces one in-flight request and never throws. No embeddings are computed by refresh. |
| Service state/output | `server/embedding-service.cjs:40-62`: remember only successful `{managed:true}` answers with timestamp. Sticky management/enabled state survives HTTP/transport errors. Initial 404 means notManaged; later failures set lastError. status null until managed, then adds stale only after >15000ms and lastError when present. |
| Service delete guards | `server/embedding-service.cjs:86-100`: protect models and previous model names, full GGUF paths, basename without case-insensitive `.gguf`, and running.loaded. fileOf searches models then previous. |
| Service mutations | `server/embedding-service.cjs:64-84,110-118`: serve PUT with 60000ms timeout, waitLoaded polls GET every 2000ms for up to 180000ms, resolveModelFile lists installed models with 8000ms timeout and rejects companion/non-GGUF entries. These API operations are **not ported here**, and must not be confused with the read-only refresh. |
| Configuration | `server/embedding-config.cjs:10-20,26-90,101-120`: active/next/previous; slot identity is profile + NUL + model, not endpoint. Null baseUrl uses inference credentials; explicit endpoint never receives them. Saved version-1 config wins over env, malformed saved state fails closed. Model/URL/profile validation and atomic persistence remain the config owner's responsibility. |
| Migration options | `server/embedding-migration.cjs:59-71`: duty .5 clamped .05..1, batch 4 clamped 1..16, minimum pause 50ms clamped 0..60000, retry 60000ms clamped 1000..3600000, served recheck 300000ms clamped 1000..3600000; numeric values are rounded except duty. JS Number/env coercion precedes numeric clamps. |
| Next-target gate | `server/embedding-migration.cjs:77-111`: verify next before initial fill and after its check expires between batches. Success caches exact target + endpoint/time and resets written-since-check. EndpointUnverified holds without deleting vectors. ModelNotServed after a prior good check or subsequent writes drops partial slots everywhere unless identity is active/previous. Track written separately so transient failure cannot hide a later drift. |
| Served models | `server/embedding-served.cjs:15-51`, `server/rag.cjs:375-400`: GET embeddings `/v1/models` with 10s timeout, redirects refused, response cap 1MiB, explicit endpoints get no engine credentials. Accept data[].id and aliases only if router status is loaded/sleeping (or no status), plus Ollama models[].name/model. Compare trim/lowercase/remove :latest; never turn a listed model file path into an alias. UI's 45s/10s cache at rag:403-424 is not used for migration verification. |
| Pass ownership | `server/embedding-migration.cjs:114-175`: only one pass, concurrent kicks request one more pass. Snapshot config version and abort on change/stop. Active is filled even if next is held; previous is never backfilled. Take targets anew per index. Exceptions affect only that index/target and last error is capped. Refused chunks are skipped for the remainder of this target/index/pass. Zero progress without new refusals stops spinning. |
| Pacing/retry/output | `server/embedding-migration.cjs:121-175`: after a nonterminal batch sleep max(minPause, round(busy*(1-duty)/duty)). lastPass records at, embedded, failed, refused, error, aborted. Only failed passes or held current next schedule one retry timer, and only while next exists. Refusals alone do not schedule retries. stop cancels pending retry and stops future publication. |
| Index input | `server/rag.cjs:1173-1205`: enumerate non-hidden .db files in shared RAG dir plus each active account's rag dir; retain (userId,projectId), never copy across tenants. Existing-only open; unavailable/removed/tombstoned indexes return null. Handles close even on errors. |
| Fill/write | `server/rag.cjs:573-600,1213-1241`: missing chunks selected by target slot + excluded ids. Embed stored body as untitled document, never filename. Batch error retries each row separately; single failures become refused ids. Recheck version/cancellation around network, then in transaction re-read slot and check (chunk id,content_hash), skip vector width mismatch. Coverage supplies remaining; drop only target's slot. `fill::Filler` ports framing/fallback/validation; transactional fences are the Store contract. |
| Request framing | `server/embedding-profile.cjs:20-68`, `server/rag.cjs:350-371,427-459`: nomic-v1 is raw text; embeddinggemma-2 has query/document/similarity prefixes, sanitized/truncated title or none. Preserve UTF-16/lone surrogates. POST `/v1/embeddings`, model/input JSON, redirects refused, output cap 16MiB. Inference admission covers requests. Empty/incomplete multi-input responses fall back to singles; integer indexes sort successful batch responses. Embedding off refuses requests. |
| Configuration API | `server/embedding-migration.cjs:181-294`: status includes coverage, verified/held next and pass state. setNext first proves served model and removes orphan slots. Switch rechecks served, requires coverage ready unless force; same-identity endpoint switches use four synthetic parity samples and cosine >=.995 unless force. Commit only checked next. Keep previous for rollback, remove displaced orphan. clear/rollback/finish clean only orphan slots. These APIs/config persistence are **not ported here**. |
| Outbox interaction | `server/index.cjs:578-614,1320`, `server/rust-index-outbox.cjs:10-25,95-152`: separate per-tenant generation-fenced index/unindex executor, tick every 2s and at request start. Claims pending->running increment attempts; acknowledge/delete only matching generation. Recover interrupted running jobs on process restart; failed jobs retry up to 5 then remain failed. Index rereads current project/file; unindex skips re-uploaded file or deleted project. Sweeps belong to Rust front. RAG index completion triggers migration kick; migration neither claims nor acknowledges outbox jobs. Tombstones prevent index recreation (rag:470-486,612-615). |

## Implementation and remaining acceptance

The pure worker, HTTP adapter, SQLite adapter, pinned sqlite-vec loader and server
lifecycle are implemented. Concrete synthetic tests cover real vec0 temp databases,
Node-schema cowork.db, credential isolation, UTF-16 input, maintenance admission,
outbox/tombstone publication races, startup ownership and shutdown.

[OWNERSHIP.md](OWNERSHIP.md) replaces the initial adapter/wiring checklist with the
current boundary and exact Core/supervisor diff. That paired diff, independent
review, authoritative CI and assembled synthetic qualification remain. The current
Core still owns runtime execution; this packet alone does not retire it.
