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

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Response};
use diary_answer::embedder::HttpEmbedder;
use diary_answer::model::Config as ClientConfig;
use diary_answer::search::Shared;
use diary_retrieval::index::{Options, Retriever};
use diary_retrieval::Publication;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use crate::managed;
use crate::server::{App, CONTROL_HEADER, WRITER_HEADER};
use crate::write::Journal;

/// The tenant's primary reservation around each index write.
struct Reserved<'j, 'h>(&'j Journal<'h>);

impl Publication for Reserved<'_, '_> {
    fn run(&self, write: &mut dyn FnMut() -> diary_retrieval::Result<()>) -> diary_retrieval::Result<()> {
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
            (Err(e), None) if e.is_settlement_refusal() => Err(diary_retrieval::Error::Busy("backup settlement uncertain".into())),
            (Err(_), None) => Err(diary_retrieval::Error::Busy("index publication refused".into())),
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
    let uid = q.split('&').find_map(|kv| kv.strip_prefix("tenant="))?.to_ascii_lowercase();
    let ok = uid.len() == 36
        && uid.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        });
    ok.then_some(uid)
}

/// `POST /internal/reindex`.
pub async fn internal(app: Arc<App>, query: &[u8], headers: &HeaderMap, body: Body) -> Response<Body> {
    let supplied = headers.get(CONTROL_HEADER).map(|v| v.as_bytes().to_vec()).unwrap_or_default();
    if app.control_token.is_empty() || !bool::from(supplied.as_slice().ct_eq(app.control_token.as_bytes())) {
        return status(403, json!({}));
    }
    let Some(cfg) = app.answer.clone() else {
        return status(404, json!({}));
    };
    if headers.get(WRITER_HEADER).map(|v| v.as_bytes()) != Some(b"1") {
        return status(409, json!({"detail": "a re-index needs the tenant's writer lock"}));
    }
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
    let Some(active) = crate::answer::live_target(&request["embedding"]) else {
        return crate::server::decline();
    };
    let handle = tokio::runtime::Handle::current();
    let app2 = app.clone();
    let task = tokio::task::spawn_blocking(move || {
        let root = app2.data_root.join("users").join(&uid);
        if crate::backup::pending(&root) {
            return status(503, json!({"detail": "Diary backup settlement is uncertain."}));
        }
        let engine = ClientConfig {
            base_url: app2.llm_main.base_url.clone(),
            api_key: app2.llm_main.api_key.clone(),
            timeout_s: cfg.timeout_s,
            max_retries: cfg.max_retries,
            ..ClientConfig::default()
        };
        let embedder = Arc::new(HttpEmbedder::fixed(engine, active, handle));
        let index_path = root.join("managed-index.db");
        if !index_path.is_file() {
            return crate::server::decline();
        }
        app2.writers
            .with_lock(&uid, |held| {
                let Ok(Some((store, _))) = managed::ready_for_write(&root, &uid, true) else {
                    return crate::server::decline();
                };
                let Ok(journal) = Journal::open(&root.join("managed-index.db"), held) else {
                    return crate::server::decline();
                };
                let Ok(documents) = journal.dirty_documents() else {
                    return crate::server::decline();
                };
                // Adoption allowed: the writer lock is held (Python D5).
                let Ok(mut retriever) =
                    Retriever::open_existing(&index_path, Shared(embedder.clone()), Reserved(&journal), Options::default())
                else {
                    return crate::server::decline();
                };
                if !retriever.vec_available() {
                    return crate::server::decline();
                }
                let mut out = Vec::new();
                for document in documents {
                    let text = match store.get_text(&document) {
                        Ok(t) => t,
                        Err(_) => {
                            out.push(json!({"document": document, "embedded": 0, "pending": true, "error": "read"}));
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
                status(200, json!({"documents": out}))
            })
            .unwrap_or_else(|| status(410, json!({"detail": "tenant deleted"})))
    });
    task.await.unwrap_or_else(|_| status(500, json!({})))
}
