//! R9 answering, step 1 (D9): `POST /internal/answer?tenant=<uuid>`, the read-only answer half of
//! one Diary exchange for Python to call.
//!
//! Python keeps the `/v1/chat/completions` route, the sessions, the log pipeline's save and the
//! tenant writer lock. With `DIARY_RUST_ANSWER` (not wired yet) its `_run_exchange` asks this
//! endpoint for the context and the companion's answer instead of building them itself; for the
//! shadow comparison it asks for the context only (`mode: "context"`) and compares the messages.
//! This endpoint writes nothing: no corpus, no journal, no index (the chunk index is opened with
//! a guard that refuses every write), and it refuses a request carrying the writer header.
//!
//! Authenticated like `/internal/fence` (the control token Python gives this process); the tenant
//! is named by Python, which has resolved and recovered it. Absent `answer` start-up config: 404.
//! Anything this side does not reproduce exactly is declined (421 + the decline header) so Python
//! answers it: a tenant that is not a ready managed one, an INDEX.md tenant, a memory listing whose
//! order is undetermined, an index that would need a legacy adoption, or a missing schema.
//!
//! Request JSON (all from Python's own state for this exchange):
//! `{"mode": "context" | "answer", "message", "turns": [...replayable turns], "day": "YYYY-MM-DD",
//!   "extra_context", "trace": bool, "max_answer_tokens", "cfg": {"context": {...}, "retrieval":
//!   {...}}, "system_prompt", "stream": bool}`.
//! Replies: `context` → `{"messages", "trace"}`; `answer` → `{"visible", "marker", "truncated",
//! "trace"}`, or with `stream` an event stream of the model's events and a final
//! `{"type": "answered", ...}` or `{"type": "failed", "failure": ...}` event.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Response};
use chrono::NaiveDate;
use diary_answer::deadline::Budget;
use diary_answer::embedder::HttpEmbedder;
use diary_answer::exchange::{Exchange, ExchangeError};
use diary_answer::model::{Client, Config as ClientConfig};
use diary_answer::search::{IndexSearch, Shared};
use diary_core::assembler::{Assembler, Store, StoreError};
use diary_core::pipeline::Pipeline;
use diary_core::retrieval_trace::RetrievalTrace;
use diary_core::streaming::{self, Failure};
use diary_retrieval::embedding::config::EmbeddingConfig;
use diary_retrieval::index::{Embedder, Options, Retriever};
use diary_retrieval::{slots, Publication};
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use crate::answer_store;
use crate::corpus::{self, Layout};
use crate::managed::{self, Managed};
use crate::server::{App, CONTROL_HEADER, WRITER_HEADER};

/// The `answer` start-up config (Python sends it only when the endpoint may be used).
#[derive(Debug, Clone, Deserialize)]
pub struct AnswerConfig {
    /// The configuration tree `seed_from_cfg` reads (`llm.embed_*`), for the embedding targets.
    pub embedding_seed: Value,
    #[serde(default = "default_timeout")]
    pub timeout_s: f64,
    #[serde(default = "default_retries")]
    pub max_retries: i64,
    /// `llm.max_reasoning_tokens` (the model client's reasoning allowance).
    #[serde(default)]
    pub reasoning_tokens: Option<i64>,
}

fn default_timeout() -> f64 {
    300.0
}

fn default_retries() -> i64 {
    3
}

#[derive(Debug, Deserialize)]
struct AnswerRequest {
    mode: String,
    message: String,
    #[serde(default)]
    turns: Vec<Value>,
    day: String,
    #[serde(default)]
    extra_context: String,
    #[serde(default)]
    trace: bool,
    max_answer_tokens: i64,
    cfg: Value,
    system_prompt: String,
    #[serde(default)]
    stream: bool,
}

/// The tenant's corpus as the assembler reads it: no lock, nothing written. The memory block is
/// read before the assembly (an undetermined listing order declines the whole request).
struct ContextStore<'a> {
    store: &'a Managed,
    layout: &'a Layout,
    memory: String,
}

impl Store for ContextStore<'_> {
    fn day_text(&self, day: NaiveDate, max_chars: i64) -> Result<String, StoreError> {
        Ok(answer_store::context_day_text(
            self.store,
            self.layout,
            day,
            max_chars,
        ))
    }
    fn standing_sections_text(&self, _max_chars: i64) -> Result<String, StoreError> {
        // A Layout describes no INDEX.md tenant (corpus::layout refuses those): nothing to show.
        Ok(String::new())
    }
    fn memory_text(&self) -> String {
        self.memory.clone()
    }
}

/// The chunk index opened for reading only: any write it would make is refused.
struct ReadOnly;

impl Publication for ReadOnly {
    fn run(
        &self,
        _write: &mut dyn FnMut() -> diary_retrieval::Result<()>,
    ) -> diary_retrieval::Result<()> {
        Err(diary_retrieval::Error::Busy(
            "the answer endpoint writes nothing".into(),
        ))
    }
}

fn status(code: u16) -> Response<Body> {
    Response::builder()
        .status(code)
        .body(Body::empty())
        .unwrap_or_default()
}

fn json_reply(code: u16, v: &Value) -> Response<Body> {
    Response::builder()
        .status(code)
        .header("content-type", "application/json")
        .body(Body::from(v.to_string()))
        .unwrap_or_default()
}

fn failure_json(e: &ExchangeError) -> Value {
    match e {
        ExchangeError::Timeout { nothing_saved, why } => {
            json!({"kind": "timeout", "nothing_saved": nothing_saved, "message": why.message()})
        }
        ExchangeError::Model(m) => {
            json!({"kind": "model", "class": m.python_class(), "message": m.to_string()})
        }
        ExchangeError::Context(c) => json!({"kind": "context", "message": format!("{c:?}")}),
        ExchangeError::NativeWriteUnavailable => json!({"kind": "native_write_unavailable"}),
    }
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

/// `POST /internal/answer`.
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
        return status(403);
    }
    let Some(cfg) = app.answer.clone() else {
        return status(404);
    };
    if headers.contains_key(WRITER_HEADER) {
        // The answer half never writes; a writer's request is a caller error.
        return json_reply(
            409,
            &json!({"detail": "the answer endpoint does not take the writer role"}),
        );
    }
    let Some(uid) = tenant_of(query) else {
        return json_reply(400, &json!({"detail": "missing or invalid tenant"}));
    };
    let Ok(bytes) = axum::body::to_bytes(body, crate::server::MAX_BODY).await else {
        return status(413);
    };
    let Ok(req) = serde_json::from_slice::<AnswerRequest>(&bytes) else {
        return json_reply(400, &json!({"detail": "invalid answer request"}));
    };
    let Ok(day) = NaiveDate::parse_from_str(&req.day, "%Y-%m-%d") else {
        return json_reply(400, &json!({"detail": "invalid day"}));
    };
    if !matches!(req.mode.as_str(), "context" | "answer") {
        return json_reply(400, &json!({"detail": "unknown mode"}));
    }
    let handle = tokio::runtime::Handle::current();
    let task = tokio::task::spawn_blocking(move || serve(&app, &cfg, &uid, req, day, &handle));
    task.await.unwrap_or_else(|_| status(500))
}

/// The tenant-side work, on a blocking thread: the store and the index are SQLite connections
/// (not shared between threads); the model and embeddings calls are driven through `handle`.
fn serve(
    app: &App,
    cfg: &AnswerConfig,
    uid: &str,
    req: AnswerRequest,
    day: NaiveDate,
    handle: &tokio::runtime::Handle,
) -> Response<Body> {
    let root = app.data_root.join("users").join(uid);
    let Ok(Some((store, settings))) = managed::ready(&root, uid) else {
        return crate::server::decline();
    };
    let Some(layout) = corpus::layout(&app.corpus, &settings) else {
        return crate::server::decline();
    };
    let Ok(memory) = answer_store::memory_text(&store) else {
        return crate::server::decline();
    };
    let state = app.data_root.join("embedding-config.json");
    let Ok(embedding) = EmbeddingConfig::new(&cfg.embedding_seed, Some(&state)) else {
        return crate::server::decline();
    };
    let engine = ClientConfig {
        base_url: app.llm_main.base_url.clone(),
        api_key: app.llm_main.api_key.clone(),
        chat_model: app.llm_main.chat_model.clone(),
        setting_key: app.llm_main.setting_key.clone(),
        timeout_s: cfg.timeout_s,
        max_retries: cfg.max_retries,
        reasoning_tokens: cfg
            .reasoning_tokens
            .unwrap_or(diary_core::llm::DEFAULT_MAX_REASONING_TOKENS),
        ..ClientConfig::default()
    };
    let embedder = Arc::new(HttpEmbedder::new(
        engine.clone(),
        Arc::new(Mutex::new(embedding)),
        handle.clone(),
    ));
    let Ok(retriever) = Retriever::open(
        &root.join("managed-index.db"),
        Shared(embedder.clone()),
        ReadOnly,
        Options {
            adopt_on_read: false,
            ..Options::default()
        },
    ) else {
        return crate::server::decline();
    };
    // Python's search would adopt a legacy table here (a write): leave it to Python.
    if slots::needs_legacy_adoption(retriever.connection(), Some(&embedder.active()))
        .unwrap_or(true)
    {
        return crate::server::decline();
    }
    let Ok(main) = handle.block_on(async { Client::new(engine) }) else {
        return status(500);
    };
    let search = IndexSearch {
        retriever: Mutex::new(retriever),
    };
    let context_store = ContextStore {
        store: &store,
        layout: &layout,
        memory,
    };
    let assembler = Assembler {
        store: &context_store,
        search: Some(&search),
        cfg: &req.cfg,
        system_prompt: req.system_prompt.clone(),
        local_reference: String::new(),
    };
    let pipeline = Pipeline {
        templates: serde_json::Map::new(),
    };
    let exchange = Exchange {
        assembler: &assembler,
        main: &main,
        aux: &main,
        pipeline: &pipeline,
        max_answer_tokens: req.max_answer_tokens,
    };
    let mut trace = req.trace.then(RetrievalTrace::new);
    if req.mode == "context" {
        return match handle.block_on(async {
            exchange.context(
                &req.message,
                &req.turns,
                day,
                &req.extra_context,
                trace.as_mut(),
            )
        }) {
            Ok(messages) => {
                let messages: Vec<Value> = messages
                    .iter()
                    .map(|m| json!({"role": m.role, "content": m.content}))
                    .collect();
                json_reply(
                    200,
                    &json!({"messages": messages, "trace": trace.as_ref().map(RetrievalTrace::to_json)}),
                )
            }
            Err(_) => crate::server::decline(),
        };
    }
    let budget = Budget::new(None, None);
    if !req.stream {
        let out = handle.block_on(exchange.answer(
            &req.message,
            &req.turns,
            day,
            &req.extra_context,
            &budget,
            None,
            trace.as_mut(),
            false,
        ));
        return match out {
            Ok(a) => json_reply(
                200,
                &json!({"visible": a.visible, "marker": a.marker, "truncated": a.truncated,
                        "trace": trace.as_ref().map(RetrievalTrace::to_json)}),
            ),
            Err(e) => json_reply(200, &json!({"failed": failure_json(&e)})),
        };
    }
    // The streamed answer: every model event, then the outcome. Collected into one body here
    // (step 1); the incremental relay comes with the Python caller.
    let mut frames: Vec<String> = Vec::new();
    let mut sink = |e: Value| {
        if streaming::shown(&e) {
            frames.push(streaming::frame(&e));
        }
    };
    let out = handle.block_on(exchange.answer(
        &req.message,
        &req.turns,
        day,
        &req.extra_context,
        &budget,
        Some(&mut sink),
        trace.as_mut(),
        false,
    ));
    let last = match out {
        Ok(a) => {
            json!({"type": "answered", "visible": a.visible, "marker": a.marker, "truncated": a.truncated,
                        "trace": trace.as_ref().map(RetrievalTrace::to_json)})
        }
        Err(e) => {
            let failure = match &e {
                ExchangeError::Timeout { nothing_saved, .. } => Failure::Timeout {
                    nothing_saved: *nothing_saved,
                },
                ExchangeError::Model(m) => Failure::from(m),
                _ => Failure::Other,
            };
            json!({"type": "failed", "failure": failure_json(&e), "event": streaming::error_event(failure)})
        }
    };
    frames.push(streaming::frame(&last));
    let mut resp = Response::builder().status(200);
    for (k, v) in streaming::HEADERS {
        resp = resp.header(k, v);
    }
    resp.body(Body::from(frames.concat())).unwrap_or_default()
}
