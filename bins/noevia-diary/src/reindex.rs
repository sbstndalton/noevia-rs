//! R9 indexing, step 1 (D9): `POST /internal/reindex?tenant=<uuid>`, the re-index of a tenant's
//! dirty documents in Rust, for Python to call where `_reindex_dirty` runs today.
//!
//! Python keeps the tenant writer lock: it calls this only while it holds it, and says so with the
//! writer header (a call without it is refused). This process also takes its own writer slot for
//! the tenant, as a forwarded write does. The chunk index is written only inside the tenant's
//! primary reservation (Python's `index_publication`), refused while a backup effect is unsettled
//! or the index outbox holds paths (draining it stays with Python, which declines here first).
//!
//! For each dirty document (Python's order): its current text is read, `reindex_file` embeds the
//! new and changed chunks and writes them, and the dirty mark is cleared unless an embedding is
//! still pending, exactly as `_reindex_dirty`. A document whose re-index fails keeps its mark
//! (Python logs "edit reindex pending" and goes on). Authenticated by the control token; 404
//! without the `answer` start-up config (the R9 internal endpoints share it).
//!
//! Reply: `{"documents": [{"document", "embedded", "pending", "error"}]}`.
//!
//! With `"plan_only": true` and `"documents": [...]` (the index shadow, `DIARY_RUST_INDEX=shadow`)
//! nothing is written and no lock is taken: each named document's plan (Python `prepare_reindex`:
//! its chunk rows and the vectors of new and changed chunks) is returned for Python to compare
//! with its own: `{"documents": [{"document", "rows": [[day, header, hash]], "embeddings":
//! {hash: [..]}, "pending", "target", "error"}]}`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Response};
use diary_answer::embedder::HttpEmbedder;
use diary_answer::model::Config as ClientConfig;
use diary_answer::search::Shared;
use diary_retrieval::index::{Embedder, Options, Retriever};
use diary_retrieval::Publication;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use crate::managed;
use crate::server::{App, CONTROL_HEADER, WRITER_HEADER};
use crate::write::Journal;

/// The tenant's primary reservation around each index write.
struct Reserved<'j, 'h>(&'j Journal<'h>);

impl Publication for Reserved<'_, '_> {
    fn run(
        &self,
        write: &mut dyn FnMut() -> diary_retrieval::Result<()>,
    ) -> diary_retrieval::Result<()> {
        let mut inner: Option<diary_retrieval::Error> = None;
        let outcome = self.0.index_publication(|| match write() {
            Ok(()) => Ok(()),
            Err(e) => {
                inner = Some(e);
                Err(crate::write::Error::other("index write failed"))
            }
        });
        match (outcome, inner) {
            (Ok(()), _) => Ok(()),
            (Err(_), Some(e)) => Err(e),
            (Err(e), None) if e.is_settlement_refusal() => Err(diary_retrieval::Error::Busy(
                "backup settlement uncertain".into(),
            )),
            (Err(_), None) => Err(diary_retrieval::Error::Busy(
                "index publication refused".into(),
            )),
        }
    }
}

fn status(code: u16, body: Value) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_default()
}

fn tenant_of(query: &[u8]) -> Option<String> {
    let q = std::str::from_utf8(query).ok()?;
    let uid = q
        .split('&')
        .find_map(|kv| kv.strip_prefix("tenant="))?
        .to_ascii_lowercase();
    let ok = uid.len() == 36
        && uid.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        });
    ok.then_some(uid)
}

/// `POST /internal/reindex`.
pub async fn internal(
    app: Arc<App>,
    query: &[u8],
    headers: &HeaderMap,
    body: Body,
) -> Response<Body> {
    let supplied = headers
        .get(CONTROL_HEADER)
        .map(|v| v.as_bytes().to_vec())
        .unwrap_or_default();
    if app.control_token.is_empty()
        || !bool::from(supplied.as_slice().ct_eq(app.control_token.as_bytes()))
    {
        return status(403, json!({}));
    }
    let Some(cfg) = app.answer.clone() else {
        return status(404, json!({}));
    };
    let Some(uid) = tenant_of(query) else {
        return status(400, json!({"detail": "missing or invalid tenant"}));
    };
    // Python's live embedding configuration (its in-memory refusals decide the active target).
    let Ok(bytes) = axum::body::to_bytes(body, 1 << 20).await else {
        return status(413, json!({}));
    };
    let Ok(request) = serde_json::from_slice::<Value>(&bytes) else {
        return status(400, json!({"detail": "invalid reindex request"}));
    };
    let Some(active) = request
        .get("embedding")
        .and_then(crate::answer::live_target)
    else {
        return crate::server::decline();
    };
    let handle = tokio::runtime::Handle::current();
    if request.get("plan_only") == Some(&json!(true)) {
        let documents: Option<Vec<String>> = request
            .get("documents")
            .and_then(Value::as_array)
            .and_then(|d| d.iter().map(|v| v.as_str().map(str::to_owned)).collect());
        let Some(documents) = documents else {
            return status(
                400,
                json!({"detail": "plan_only needs the documents, as strings"}),
            );
        };
        // Held until the reply is ready: a caller that gives up drops it, which stops the plans.
        let (_caller, gone) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::task::spawn_blocking(move || {
            plans(&app, &cfg, &uid, active, &documents, handle, gone)
        });
        return task.await.unwrap_or_else(|_| status(500, json!({})));
    }
    // A plan writes nothing; a re-index needs the writer lock Python holds.
    if headers.get(WRITER_HEADER).map(|v| v.as_bytes()) != Some(b"1") {
        return status(
            409,
            json!({"detail": "a re-index needs the tenant's writer lock"}),
        );
    }
    let lease = crate::lease::lease_of(headers);
    let app2 = app.clone();
    let task = tokio::task::spawn_blocking(move || {
        let root = app2.data_root.join("users").join(&uid);
        if crate::backup::pending(&root) {
            return status(
                503,
                json!({"detail": "Diary backup settlement is uncertain."}),
            );
        }
        let engine = ClientConfig {
            base_url: app2.llm_main.base_url.clone(),
            api_key: app2.llm_main.api_key.clone(),
            timeout_s: cfg.timeout_s,
            max_retries: cfg.max_retries,
            ..ClientConfig::default()
        };
        let embedder = Arc::new(HttpEmbedder::fixed(engine, active, handle));
        if !root.join("managed-index.db").is_file() {
            return crate::server::decline();
        }
        app2.writers
            // Main mode: Python asks under its lease (joined); otherwise its own lock is separate.
            .with_lock_as(&uid, lease, app2.write_wait, |held| {
                match reindex_held(&root, &uid, held, &embedder) {
                    Some(out) => status(200, json!({"documents": out})),
                    None => crate::server::decline(),
                }
            })
            .unwrap_or_else(|refused| match refused {
                crate::write::Refused::Fenced => status(410, json!({"detail": "tenant deleted"})),
                crate::write::Refused::Busy => crate::server::reply(crate::workspace::Reply::Busy),
            })
    });
    task.await.unwrap_or_else(|_| status(500, json!({})))
}

/// The re-index of a tenant's dirty documents while its writer slot is `held` (Python's
/// `_reindex_dirty` body after the outbox drain): read each, `reindex_file`, clear the mark unless
/// an embedding is still pending. None: nothing can be re-indexed here (no index file, not ready
/// for a write, no vector extension), and every mark stays.
pub(crate) fn reindex_held(
    root: &std::path::Path,
    uid: &str,
    held: &crate::write::WriteGuard<'_>,
    embedder: &Arc<HttpEmbedder>,
) -> Option<Vec<Value>> {
    let index_path = root.join("managed-index.db");
    if !index_path.is_file() {
        return None;
    }
    let Ok(Some((store, _))) = managed::ready_for_write(root, uid, true) else {
        return None;
    };
    let journal = Journal::open(&index_path, held).ok()?;
    let documents = journal.dirty_documents().ok()?;
    // Adoption allowed: the writer lock is held (Python D5).
    let mut retriever = Retriever::open_existing(
        &index_path,
        Shared(embedder.clone()),
        Reserved(&journal),
        Options::default(),
    )
    .ok()?;
    if !retriever.vec_available() {
        return None;
    }
    let mut out = Vec::new();
    for document in documents {
        let text = match store.get_text(&document) {
            Ok(t) => t,
            Err(_) => {
                out.push(
                    json!({"document": document, "embedded": 0, "pending": true, "error": "read"}),
                );
                continue;
            }
        };
        match retriever.reindex_file(&document, text.as_deref()) {
            Ok(embedded) => {
                let pending = retriever.pending_embeddings();
                let cleared = pending || journal.clear_dirty(&document).is_ok();
                out.push(json!({"document": document, "embedded": embedded, "pending": pending,
                                "error": if cleared { Value::Null } else { json!("clear") }}));
            }
            Err(e) => out.push(json!({"document": document, "embedded": 0, "pending": true, "error": e.to_string()})),
        }
    }
    Some(out)
}

/// S6b (main mode, the embedding owner here): a native PUT /api/file or edit re-indexes the
/// tenant's dirty documents under the slot it saved under, as Python's handlers run
/// `_reindex_dirty` before they reply. A failure is logged as Python's "edit reindex pending"
/// and never changes the save's reply; the marks stay for the next pass.
pub(crate) fn after_save(
    root: &std::path::Path,
    uid: &str,
    held: &crate::write::WriteGuard<'_>,
    embedder: Arc<HttpEmbedder>,
) {
    match reindex_held(root, uid, held, &embedder) {
        None => {
            eprintln!("noevia-diary: edit reindex pending: the index cannot be re-indexed here")
        }
        Some(out) => {
            for d in out.iter().filter(|d| !d["error"].is_null()) {
                eprintln!(
                    "noevia-diary: edit reindex pending: {}",
                    d["error"].as_str().unwrap_or("error")
                );
            }
        }
    }
}

/// The index shadow (`plan_only`): for each named document, what this side's re-index would write
/// (Python `prepare_reindex`: the parsed chunks and the vectors of the new and changed ones), read
/// and embedded but never written. Python calls it under its own locks, before its own re-index
/// of the same documents, and compares the plans. Declined where a re-index here would first have
/// to adopt a legacy table (a write) or the tenant is not a ready managed one.
fn plans(
    app: &App,
    cfg: &crate::answer::AnswerConfig,
    uid: &str,
    active: diary_retrieval::Target,
    documents: &[String],
    handle: tokio::runtime::Handle,
    mut gone: tokio::sync::oneshot::Receiver<()>,
) -> Response<Body> {
    let root = app.data_root.join("users").join(uid);
    // Dirty documents are the point here. Python holds its store lock around this call (after
    // replaying the journal and draining the outbox), so a pending journal row declines.
    let Ok(Some((store, _))) = managed::ready_for_write(&root, uid, true) else {
        return crate::server::decline();
    };
    let index_path = root.join("managed-index.db");
    if !index_path.is_file() {
        return crate::server::decline();
    }
    let engine = ClientConfig {
        base_url: app.llm_main.base_url.clone(),
        api_key: app.llm_main.api_key.clone(),
        timeout_s: cfg.timeout_s,
        max_retries: cfg.max_retries,
        ..ClientConfig::default()
    };
    let embedder = Arc::new(HttpEmbedder::fixed(engine, active, handle));
    let Ok(mut retriever) = Retriever::open_existing(
        &index_path,
        Shared(embedder.clone()),
        crate::answer::ReadOnly,
        Options {
            adopt_on_read: false,
            ..Options::default()
        },
    ) else {
        return crate::server::decline();
    };
    if !retriever.vec_available()
        || diary_retrieval::slots::needs_legacy_adoption(
            retriever.connection(),
            Some(&embedder.active()),
        )
        .unwrap_or(true)
    {
        return crate::server::decline();
    }
    let mut out = Vec::new();
    for document in documents {
        if matches!(
            gone.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ) {
            // The caller gave up (its deadline): no more embedding calls for a reply nobody reads.
            return status(503, json!({}));
        }
        let text = match store.get_text(document) {
            Ok(t) => t,
            Err(_) => {
                out.push(json!({"document": document, "error": "read"}));
                continue;
            }
        };
        match retriever.prepare_reindex(document, text.as_deref()) {
            Ok(plan) => out.push(plan_json(document, &plan)),
            Err(e) => out.push(json!({"document": document, "error": e.to_string()})),
        }
    }
    status(200, json!({"documents": out}))
}

fn plan_json(document: &str, plan: &diary_retrieval::index::ReindexPlan) -> Value {
    let rows: Vec<Value> = plan
        .rows
        .iter()
        .map(|r| json!([r.day, r.header, r.content_hash]))
        .collect();
    let embeddings: serde_json::Map<String, Value> = plan
        .embeddings
        .iter()
        .map(|(h, v)| (h.clone(), json!(v)))
        .collect();
    json!({
        "document": document,
        "rows": rows,
        "embeddings": embeddings,
        "pending": plan.pending,
        "target": plan.target.as_ref().map(|t| json!({"base_url": t.base_url, "model": t.model, "profile": t.profile})),
        "error": Value::Null,
    })
}

/// S8: `api_health`'s tenant detail for a ready managed tenant (`store`), read without a lock:
/// the journal counts (`Journal.pending_count`, `quarantined_count`) and `Retriever.stats()`
/// with the live target of the configuration this process owns. None: Python's to answer (no
/// index, no vector extension, an index still to adopt, a read error).
pub(crate) fn health_detail(
    store: &managed::Managed,
    embedder: Arc<HttpEmbedder>,
) -> Option<crate::health::TenantDetail> {
    let index_path = store.db_path().parent()?.join("managed-index.db");
    if !index_path.is_file() {
        return None;
    }
    let retriever = Retriever::open_existing(
        &index_path,
        Shared(embedder.clone()),
        crate::answer::ReadOnly,
        Options {
            adopt_on_read: false,
            ..Options::default()
        },
    )
    .ok()?;
    if !retriever.vec_available()
        || diary_retrieval::slots::needs_legacy_adoption(
            retriever.connection(),
            Some(&embedder.active()),
        )
        .unwrap_or(true)
    {
        return None;
    }
    let count =
        |sql: &str| -> Option<i64> { retriever.connection().query_row(sql, [], |r| r.get(0)).ok() };
    let journal_pending = count("SELECT COUNT(*) FROM journal WHERE applied = 0")?;
    let journal_quarantined = count(
        "SELECT COUNT(*) FROM journal WHERE applied = 1 AND last_error LIKE 'quarantined:%'",
    )?;
    let stats = retriever.stats().ok()?;
    Some(crate::health::TenantDetail {
        journal_pending,
        journal_quarantined,
        retrieval: crate::health::RetrievalStats {
            chunks: stats.chunks,
            vec_available: stats.vec_available,
            dim: stats.dim,
            model: stats.model,
            profile: stats.profile,
        },
    })
}
