//! Shift 4, step 6 (track R9): `POST /v1/chat/completions` in this process, in main mode, behind
//! the parent switch DIARY_RUST_EXCHANGE=on (unset: the child answers every exchange, unchanged).
//!
//! Served here only for a writer tenant of this process (DIARY_RUST_WRITES) that this process
//! has recovered (DIARY_RUST_RECOVERY=on, `recovery::recover_for_reads`), with the embedding owner
//! here and this process's own reading of the configuration (`Setup`, from config.yaml at start).
//! The exchange is `diary_answer::exchange::Exchange::run` (Python `_run_exchange`, proven by its
//! own recorded differential); this module is the route around it (app.py `v1_chat_completions`
//! and `exchange_stream`): the request body, the replayable history, the session registry
//! (`_session`), the entry time (`entry_target`), the dirty re-index before the exchange, the save
//! under the writer slot, the background re-index after it, and the reply (JSON, or the event
//! stream with `diary_events`).
//!
//! Anything this side does not reproduce is declined (`None`) BEFORE any effect, and the child
//! answers it: a body that is not a JSON object or whose fields Python would read through `str()`
//! (a non-string message content or session id), no user message, a retrieval trace request, an
//! entry time or day in a form this side does not parse as Python's `fromisoformat` does, a
//! tenant that is not a ready managed one (or has INDEX.md enabled, or a memory listing whose
//! order is undetermined), an index without vector support or still to adopt, and
//! DIARY_CORPUS_WRITE_IMPL other than `rust`.
//!
//! Named differences:
//! - Sessions are this process's (the child keeps its own): an exchange the child serves later
//!   in the same session sees only the turns it saw, as after a restart; the client's thread
//!   seeds an empty session either way (D30).
//! - The served chat model is resolved once per exchange, before the context build (Python
//!   resolves at the model call, through the same 60 s cache).
//! - Stream frames are JSON-equal to Python's, not byte-equal (D5).
//! - A streamed exchange of a tenant not yet recovered here waits for that recovery before the
//!   stream opens (Python recovers inside the stream, under keep-alives); the web's reads before
//!   an exchange normally recover the tenant first (D31). "Reading diary entries and memory…" is
//!   sent after the dirty re-index, not before it.
//! - A journal row this side cannot replay that appears between the recovery and the save fails
//!   the save after the answer (Python would replay it).

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::Response;
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate};
use diary_answer::embedder::HttpEmbedder;
use diary_answer::exchange::{Exchange, ExchangeError};
use diary_answer::model::{Client, Config as ClientConfig};
use diary_answer::search::{IndexSearch, Shared};
use diary_core::assembler::{Assembler, Store, StoreError};
use diary_core::llm::LlmError;
use diary_core::pipeline::{Failed, Journal as ExchangeJournal, Pipeline};
use diary_core::streaming::{self, Failure};
use diary_retrieval::index::{Embedder, Options, Retriever};
use diary_retrieval::slots;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::answer_store;
use crate::corpus::{self, Layout};
use crate::managed::{self, Managed};
use crate::server::App;
use crate::write;
use crate::writes::{self, Ctx, EntryTime};

/// The switch.
pub const SWITCH: &str = "DIARY_RUST_EXCHANGE";
/// `_SESSION_CAP`, `_SESSION_TTL_S`.
pub const SESSION_CAP: usize = 256;
pub const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// The retrieval trace's operator flag and header (retrieval_trace.py): a traced exchange is the
/// child's.
const TRACE_FLAG: &str = "DIARY_RETRIEVAL_TRACE";
const TRACE_HEADER: &str = "x-diary-retrieval-trace";
const INVALID_ENTRY: &str = "Invalid diary date/time; include a timezone offset";

/// What the exchange reads of the configuration (this process's own reading, at start).
pub struct Setup {
    pub settings: diary_operator::server_config::ExchangeSettings,
    pub system_prompt: Prompt,
    pub templates: Map<String, Value>,
}

/// The system prompt: ContextAssembler._system_prompt reads it at every build (an operator's edit
/// applies to the next exchange).
pub enum Prompt {
    /// A fixed text (tests).
    Text(String),
    /// `prompts.system_path` when it exists, else the default prompt file.
    Files {
        custom: Option<std::path::PathBuf>,
        default: std::path::PathBuf,
    },
}

impl Prompt {
    /// The prompt text now (Path.read_text: UTF-8, universal newlines), or None when it cannot be
    /// read (Python's exchange fails then).
    pub fn text(&self) -> Option<String> {
        match self {
            Prompt::Text(t) => Some(t.clone()),
            Prompt::Files { custom, default } => {
                let path = custom.as_ref().filter(|p| p.exists()).unwrap_or(default);
                let bytes = std::fs::read(path).ok()?;
                let text = String::from_utf8(bytes).ok()?;
                Some(text.replace("\r\n", "\n").replace('\r', "\n"))
            }
        }
    }
}

/// `config.yaml` (with the environment) and the prompt files, as the child reads them.
pub fn setup(
    data: &diary_operator::value::Value,
    config_dir: &std::path::Path,
) -> Result<Setup, String> {
    Ok(Setup {
        settings: diary_operator::server_config::exchange_settings(data)?,
        system_prompt: {
            // Read once here so a prompt that cannot be read refuses the switch at start.
            diary_operator::config::system_prompt(data, config_dir)?;
            let custom = diary_operator::config::get(data, "prompts.system_path")
                .filter(diary_operator::value::Value::truthy)
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from);
            Prompt::Files {
                custom,
                default: config_dir.join("prompts").join("system.md"),
            }
        },
        templates: diary_operator::server_config::logging_templates(config_dir)?,
    })
}

// ------------------------------------------------------------------------------- sessions

struct Entry {
    turns: Arc<Mutex<Vec<Value>>>,
    last_used: Instant,
    order: u64,
}

#[derive(Default)]
struct SessionMap {
    entries: HashMap<String, Entry>,
    tick: u64,
}

/// Python `SESSIONS`: conversation turns per tenant and client session id, bounded (LRU, cap,
/// TTL swept when full).
#[derive(Default)]
pub struct Sessions {
    map: Mutex<SessionMap>,
}

fn valid_session_id(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl Sessions {
    /// Python `_session(session_id, tenant_id)` for a UUID tenant: its turns, created when missing.
    pub fn get(&self, tenant: &str, session_id: &str) -> Arc<Mutex<Vec<Value>>> {
        let sid = if valid_session_id(session_id) {
            session_id
        } else {
            "default"
        };
        let key = format!("{}:{sid}", tenant.to_ascii_lowercase());
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        m.tick += 1;
        let tick = m.tick;
        let now = Instant::now();
        if let Some(e) = m.entries.get_mut(&key) {
            e.last_used = now;
            e.order = tick;
            return e.turns.clone();
        }
        if m.entries.len() >= SESSION_CAP {
            m.entries
                .retain(|_, e| now.duration_since(e.last_used) <= SESSION_TTL);
        }
        while m.entries.len() >= SESSION_CAP {
            let oldest = m
                .entries
                .iter()
                .min_by_key(|(_, e)| e.order)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    m.entries.remove(&k);
                }
                None => break,
            }
        }
        let turns = Arc::new(Mutex::new(Vec::new()));
        m.entries.insert(
            key,
            Entry {
                turns: turns.clone(),
                last_used: now,
                order: tick,
            },
        );
        turns
    }

    /// Tenant deletion drops the tenant's sessions (Python pops them with its tombstone).
    pub fn forget_tenant(&self, uid: &str) {
        let prefix = format!("{}:", uid.to_ascii_lowercase());
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .retain(|k, _| !k.starts_with(&prefix));
    }

    /// How many sessions are held (tests).
    pub fn len(&self) -> usize {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ------------------------------------------------------------------------------- request

/// The request as `v1_chat_completions` reads it.
pub struct Parsed {
    /// The last user message, stripped.
    pub last: String,
    /// `_replayable_history(messages[:last_at])`.
    pub prior: Vec<Value>,
    pub session_id: String,
    /// `stream is True and diary_events is True`.
    pub stream: bool,
    /// `optional_reference(body)`.
    pub extra: String,
    pub entry: EntryTarget,
}

impl Parsed {
    /// The stream path runs `entry_target` before `_tenant_state`: its 400 comes first.
    pub fn before_tenant_state(&self) -> Option<Response<Body>> {
        (self.stream && matches!(self.entry, EntryTarget::Invalid))
            .then(|| detail(400, INVALID_ENTRY))
    }
}

/// `entry_target(body)`.
pub enum EntryTarget {
    At(DateTime<FixedOffset>, NaiveDate),
    /// HTTPException(400, INVALID_ENTRY).
    Invalid,
}

/// `request.json()` and the fields the route reads, or None (the child's to answer).
pub fn parse(bytes: &[u8], trace_requested: bool) -> Option<Parsed> {
    if trace_requested {
        return None;
    }
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(bytes) else {
        return None;
    };
    // `body.get("messages") or []`: a truthy value other than a list is iterated by Python.
    let messages: &[Value] = match body.get("messages") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => &[],
        Some(Value::Array(a)) => a,
        Some(Value::String(s)) if s.is_empty() => &[],
        Some(Value::Object(o)) if o.is_empty() => &[],
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => &[],
        Some(_) => return None,
    };
    let last_at = messages
        .iter()
        .rposition(|m| m.get("role").and_then(Value::as_str) == Some("user"))?;
    let last = match messages.get(last_at).and_then(|m| m.get("content")) {
        None => String::new(),
        Some(Value::String(s)) => crate::py::strip(s).to_owned(),
        // str() of anything else is its repr: the child's.
        Some(_) => return None,
    };
    if last.is_empty() {
        // 400 "no user message": the child answers it.
        return None;
    }
    let prior = replayable_history(messages.get(..last_at).unwrap_or_default());
    let session_id = match body.get("session_id") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        None | Some(Value::Null) | Some(Value::Bool(false)) => "openai-client".to_owned(),
        Some(Value::String(_)) => "openai-client".to_owned(),
        Some(_) => return None,
    };
    let stream = body.get("stream") == Some(&Value::Bool(true))
        && body.get("diary_events") == Some(&Value::Bool(true));
    let extra = match (body.get("extrasEnabled"), body.get("extraContext")) {
        (Some(Value::Bool(true)), Some(Value::String(s))) => {
            diary_core::pyconv::slice(s, None, Some(12000))
        }
        _ => String::new(),
    };
    let entry = entry_target(&body)?;
    Some(Parsed {
        last,
        prior,
        session_id,
        stream,
        extra,
        entry,
    })
}

/// `_replayable_history`: the answered exchanges of the client's earlier turns, the last 16
/// messages, starting on a question.
fn replayable_history(messages: &[Value]) -> Vec<Value> {
    let raw: Vec<Value> = messages
        .iter()
        .filter(|m| {
            matches!(
                m.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            ) && m.get("content").is_some_and(Value::is_string)
        })
        .map(|m| serde_json::json!({"role": m.get("role"), "content": m.get("content")}))
        .collect();
    let pairs = diary_core::context::completed_exchanges(&raw);
    let mut tail: Vec<Value> = pairs
        .get(pairs.len().saturating_sub(16)..)
        .unwrap_or_default()
        .to_vec();
    if tail
        .first()
        .is_some_and(|p| p.get("role").and_then(Value::as_str) != Some("user"))
    {
        tail.remove(0);
    }
    tail
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::Bool(true)) => true,
    }
}

/// `entry_target(body)`; None where `fromisoformat` might read the value differently.
fn entry_target(body: &Map<String, Value>) -> Option<EntryTarget> {
    let raw = body.get("entryTime");
    let now = if truthy(raw) {
        match raw {
            Some(Value::String(s)) => match writes::parse_entry_time(s) {
                EntryTime::At(at) => at,
                EntryTime::Invalid => return Some(EntryTarget::Invalid),
                EntryTime::Unsure => return None,
            },
            // fromisoformat(non-str): TypeError, caught as a 400.
            _ => return Some(EntryTarget::Invalid),
        }
    } else {
        chrono::Local::now().fixed_offset()
    };
    let selected = body.get("entryDay");
    let day = if truthy(selected) {
        match selected {
            Some(Value::String(s))
                if s.len() == 10
                    && s.bytes().enumerate().all(|(i, b)| match i {
                        4 | 7 => b == b'-',
                        _ => b.is_ascii_digit(),
                    }) =>
            {
                match NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                    Ok(d) => d,
                    Err(_) => return Some(EntryTarget::Invalid),
                }
            }
            // Other ISO forms (week dates, basic format) are the child's.
            Some(Value::String(_)) => return None,
            _ => return Some(EntryTarget::Invalid),
        }
    } else {
        now.date_naive()
    };
    if !(1900..=2100).contains(&day.year()) || day > now.date_naive() {
        return Some(EntryTarget::Invalid);
    }
    Some(EntryTarget::At(now, day))
}

// ------------------------------------------------------------------------------- replies

#[derive(Serialize)]
struct ErrorDetail<'a> {
    message: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a Value,
    reasoning_content: &'a Value,
}

#[derive(Serialize)]
struct Choice<'a> {
    index: u8,
    message: Message<'a>,
    finish_reason: &'a str,
}

#[derive(Serialize)]
struct Usage {
    prompt_tokens: u8,
    completion_tokens: u8,
    total_tokens: u8,
}

#[derive(Serialize)]
struct Diary<'a> {
    decision: &'a Value,
    xid: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,
}

#[derive(Serialize)]
struct Completion<'a> {
    id: String,
    object: &'a str,
    created: i64,
    model: &'a str,
    choices: [Choice<'a>; 1],
    usage: Usage,
    diary: Diary<'a>,
}

fn json_response(status: u16, body: Vec<u8>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap_or_default()
}

fn error_response(status: u16, message: &str, kind: &str, code: Option<&str>) -> Response<Body> {
    let body = ErrorBody {
        error: ErrorDetail {
            message,
            kind,
            code,
        },
    };
    json_response(status, serde_json::to_vec(&body).unwrap_or_default())
}

/// HTTPException(400, detail): `{"detail": ...}`.
fn detail(status: u16, text: &str) -> Response<Body> {
    json_response(
        status,
        serde_json::to_vec(&serde_json::json!({"detail": text})).unwrap_or_default(),
    )
}

/// The JSON reply of a successful exchange (app.py builds the same dict).
fn completion(result: &Map<String, Value>) -> Response<Body> {
    let null = Value::Null;
    let empty = Value::String(String::new());
    let xid = result.get("xid").unwrap_or(&null);
    let truncated = result.get("truncated") == Some(&Value::Bool(true));
    let id = match xid {
        Value::String(s) if !s.is_empty() => format!("chatcmpl-{s}"),
        _ => "chatcmpl-local".to_owned(),
    };
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let body = Completion {
        id,
        object: "chat.completion",
        created,
        model: "diary-companion",
        choices: [Choice {
            index: 0,
            message: Message {
                role: "assistant",
                content: result.get("reply").unwrap_or(&null),
                reasoning_content: result.get("reasoning").unwrap_or(&empty),
            },
            finish_reason: if truncated { "length" } else { "stop" },
        }],
        usage: Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        },
        diary: Diary {
            decision: result.get("decision").unwrap_or(&null),
            xid,
            truncated: truncated.then_some(true),
        },
    };
    json_response(200, serde_json::to_vec(&body).unwrap_or_default())
}

/// The JSON reply of a failed exchange, as v1_chat_completions maps each exception.
fn failure_response(e: &ExchangeError) -> Response<Body> {
    match e {
        ExchangeError::Timeout { nothing_saved, .. } => {
            let message = if *nothing_saved {
                "the model did not answer before the diary deadline; nothing was saved"
            } else {
                "the model did not answer before the diary deadline"
            };
            error_response(502, message, "api_error", Some("chat_timeout"))
        }
        ExchangeError::Model(LlmError::LengthCap) => error_response(
            502,
            diary_core::llm::LENGTH_CAP_TEXT,
            "api_error",
            Some("length_cap"),
        ),
        ExchangeError::Model(LlmError::EmptyAnswer) => error_response(
            502,
            diary_core::llm::EMPTY_ANSWER_TEXT,
            "api_error",
            Some("empty_answer"),
        ),
        ExchangeError::NativeWriteUnavailable => {
            error_response(503, "diary write transform unavailable", "api_error", None)
        }
        ExchangeError::Model(_) | ExchangeError::Context(_) => {
            error_response(502, "model error", "api_error", None)
        }
    }
}

/// The stream's error event (exchange_stream's except clauses).
fn failure_event(e: &ExchangeError) -> Value {
    let failure = match e {
        ExchangeError::Timeout { nothing_saved, .. } => Failure::Timeout {
            nothing_saved: *nothing_saved,
        },
        ExchangeError::Model(m) => Failure::from(m),
        ExchangeError::Context(_) | ExchangeError::NativeWriteUnavailable => Failure::Other,
    };
    streaming::error_event(failure)
}

// ------------------------------------------------------------------------------- the tenant

/// The tenant's corpus as the assembler reads it (no lock, nothing written).
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
        // A Layout describes no INDEX.md tenant: nothing to show.
        Ok(String::new())
    }
    fn memory_text(&self) -> String {
        self.memory.clone()
    }
}

/// The save of one exchange, under the tenant's writer slot (Python's `store.log_exchange` under
/// the store lock, a writer tenant's lease): the native write path, recovery first (as every
/// native write).
struct SlotJournal<'a> {
    app: &'a App,
    uid: &'a str,
    /// The entry's local time (the saved subsection's header).
    now: chrono::NaiveTime,
}

impl ExchangeJournal for SlotJournal<'_> {
    fn log_exchange(
        &mut self,
        day: &str,
        sub_header: &str,
        me_text: &str,
        claude_text: &str,
    ) -> Result<String, (bool, String)> {
        let Ok(day) = NaiveDate::parse_from_str(day, "%Y-%m-%d") else {
            return Err((false, format!("invalid day {day:?}")));
        };
        let app = self.app;
        let uid = self.uid;
        let root = app.data_root.join("users").join(uid);
        let saved = app
            .writers
            .with_lock_as(uid, None, app.write_wait, |held| {
                if crate::backup::pending(&root) {
                    return Err((false, "backup settlement uncertain".to_string()));
                }
                let Ok(Some((reader, settings))) = managed::ready_for_write(&root, uid, false)
                else {
                    return Err((false, "the tenant is not a ready managed Diary".to_string()));
                };
                let Some(layout) = corpus::layout(&app.corpus, &settings) else {
                    return Err((false, "the tenant's layout is not served here".to_string()));
                };
                let journal = write::Journal::open(&root.join("managed-index.db"), held)
                    .map_err(|e| (false, e.into_message()))?;
                let recovery = app
                    .recovered
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(uid.to_owned())
                    .or_default()
                    .clone();
                let ctx = Ctx {
                    recovery: &recovery,
                    reader: &reader,
                    store: write::Store::new(reader.db_path()),
                    journal,
                    layout: &layout,
                };
                if !crate::recovery::supported(&ctx) {
                    return Err((false, "a journal row this side does not replay".to_string()));
                }
                crate::recovery::recover(&ctx).map_err(|e| (false, e.into_message()))?;
                answer_store::log_exchange(&ctx, day, self.now, sub_header, me_text, claude_text)
            })
            .unwrap_or_else(|_| Err((false, "the tenant's writer slot is busy".to_string())));
        // A row was journaled (applied or pending): the child hears of it as of a native write.
        if matches!(saved, Ok(_) | Err((true, _))) {
            app.writers.note_native_write(uid);
        }
        saved
    }

    fn index_enabled(&self) -> bool {
        false
    }

    fn standing_sections_text(&mut self, _max_chars: i64) -> Result<String, Failed> {
        Ok(String::new())
    }

    fn update_standing_sections(
        &mut self,
        _open_questions: &Value,
        _timeline: &Value,
        _today: &str,
    ) -> Result<Option<String>, Failed> {
        Ok(None)
    }
}

/// Everything the exchange needs of one ready tenant, checked before any effect.
struct Ready {
    store: Managed,
    layout: Layout,
    memory: String,
}

fn ready(app: &App, uid: &str) -> Option<Ready> {
    if app.corpus_write_impl != "rust" {
        return None;
    }
    let root = app.data_root.join("users").join(uid);
    let Ok(Some((store, settings))) = managed::ready_for_write(&root, uid, false) else {
        return None;
    };
    let layout = corpus::layout(&app.corpus, &settings)?;
    let memory = answer_store::memory_text(&store).ok()?;
    let embedder = app.save_embedder.get()?;
    let index = root.join("managed-index.db");
    let retriever = Retriever::open_existing(
        &index,
        Shared(embedder.clone()),
        crate::answer::ReadOnly,
        Options {
            adopt_on_read: false,
            ..Options::default()
        },
    )
    .ok()?;
    if !retriever.vec_available()
        || slots::needs_legacy_adoption(retriever.connection(), Some(&embedder.active()))
            .unwrap_or(true)
    {
        return None;
    }
    Some(Ready {
        store,
        layout,
        memory,
    })
}

/// The served chat model of one client, or "" (the client then refuses the call: Python's
/// ModelNotServed / unreachable engine fails the call the same way).
async fn served(app: &App, aux: bool) -> String {
    let Some(models) = app.models.as_ref() else {
        return String::new();
    };
    let mut memo = crate::health::Memo::new();
    let resolved = if aux {
        let partner = models.aux_prefers_main.then_some(&models.main);
        crate::health::resolve(&app.http, &models.aux, partner, &mut memo).await
    } else {
        crate::health::resolve(&app.http, &models.main, None, &mut memo).await
    };
    resolved.map(|(m, _)| m).unwrap_or_default()
}

fn client(app: &App, setup: &Setup, aux: bool, model: String) -> Option<Client> {
    let models = app.models.as_ref()?;
    let c = if aux {
        &models.aux.cfg
    } else {
        &models.main.cfg
    };
    Client::new(ClientConfig {
        base_url: c.base_url.clone(),
        api_key: c.api_key.clone(),
        chat_model: model,
        setting_key: c.setting_key.clone(),
        timeout_s: setup.settings.timeout_s,
        max_retries: setup.settings.max_retries,
        reasoning_tokens: setup.settings.reasoning_tokens,
        ..ClientConfig::default()
    })
    .ok()
}

/// `_reindex_dirty(st)` before the exchange (under the writer slot; failures logged).
fn reindex_dirty(app: &App, uid: &str, embedder: &Arc<HttpEmbedder>) {
    let root = app.data_root.join("users").join(uid);
    let _ = app.writers.with_lock_as(uid, None, app.write_wait, |held| {
        crate::reindex::after_save(&root, uid, held, embedder.clone());
    });
}

/// One exchange for `uid`, the request already parsed. None: declined before any effect.
pub async fn serve(
    app: Arc<App>,
    setup: Arc<Setup>,
    sessions: Arc<Sessions>,
    uid: String,
    parsed: Parsed,
) -> Option<Response<Body>> {
    let embedder = app.save_embedder.get()?.clone();
    let check = {
        let (app, uid) = (app.clone(), uid.clone());
        tokio::task::spawn_blocking(move || ready(&app, &uid))
            .await
            .ok()?
    };
    let tenant = check?;
    let (now, day) = match &parsed.entry {
        EntryTarget::At(now, day) => (*now, *day),
        EntryTarget::Invalid => {
            if !parsed.stream {
                // The JSON path builds and seeds the session first, then entry_target raises.
                let session = sessions.get(&uid, &parsed.session_id);
                seed(&session, &parsed.prior);
            }
            // (The stream path runs entry_target before anything.)
            return Some(detail(400, INVALID_ENTRY));
        }
    };
    let main_model = served(&app, false).await;
    let aux_model = served(&app, true).await;
    let main = client(&app, &setup, false, main_model)?;
    let aux = client(&app, &setup, true, aux_model)?;
    let run = Run {
        app,
        setup,
        sessions,
        uid,
        parsed,
        tenant,
        embedder,
        main,
        aux,
        now,
        day,
    };
    if run.parsed.stream {
        Some(run.stream())
    } else {
        Some(run.json().await)
    }
}

/// `if prior and not sess["turns"]: sess["turns"].extend(prior)`.
fn seed(session: &Arc<Mutex<Vec<Value>>>, prior: &[Value]) {
    let mut turns = session.lock().unwrap_or_else(|e| e.into_inner());
    if !prior.is_empty() && turns.is_empty() {
        turns.extend(prior.iter().cloned());
    }
}

struct Run {
    app: Arc<App>,
    setup: Arc<Setup>,
    sessions: Arc<Sessions>,
    uid: String,
    parsed: Parsed,
    tenant: Ready,
    embedder: Arc<HttpEmbedder>,
    main: Client,
    aux: Client,
    now: DateTime<FixedOffset>,
    day: NaiveDate,
}

impl Run {
    /// `_run_exchange` around `Exchange::run`, on a blocking thread (the store and the index are
    /// SQLite connections; the model calls are driven through the runtime handle).
    fn exchange(
        &self,
        handle: &tokio::runtime::Handle,
        emit: Option<&mut (dyn FnMut(Value) + Send)>,
    ) -> Result<Map<String, Value>, ExchangeError> {
        let root = self.app.data_root.join("users").join(&self.uid);
        if self.app.writers.is_fenced(&self.uid) {
            // Deleted meanwhile (Python's `_session` answers 410 under its tombstone).
            return Err(ExchangeError::Model(LlmError::Llm("tenant deleted".into())));
        }
        reindex_dirty(&self.app, &self.uid, &self.embedder);
        let session = self.sessions.get(&self.uid, &self.parsed.session_id);
        let mut turns = session.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let retriever = Retriever::open_existing(
            &root.join("managed-index.db"),
            Shared(self.embedder.clone()),
            crate::answer::ReadOnly,
            Options {
                adopt_on_read: false,
                ..Options::default()
            },
        )
        .map_err(|_| ExchangeError::Model(LlmError::Llm("index unavailable".into())))?;
        let search = IndexSearch {
            retriever: Mutex::new(retriever),
        };
        let store = ContextStore {
            store: &self.tenant.store,
            layout: &self.tenant.layout,
            memory: self.tenant.memory.clone(),
        };
        let system_prompt = self.setup.system_prompt.text().ok_or_else(|| {
            ExchangeError::Model(LlmError::Llm("system prompt unreadable".into()))
        })?;
        let assembler = Assembler {
            store: &store,
            search: Some(&search),
            cfg: &self.setup.settings.cfg,
            system_prompt,
            local_reference: String::new(),
        };
        let pipeline = Pipeline {
            templates: self.setup.templates.clone(),
        };
        let exchange = Exchange {
            assembler: &assembler,
            main: &self.main,
            aux: &self.aux,
            pipeline: &pipeline,
            max_answer_tokens: self.setup.settings.max_answer_tokens,
        };
        let mut journal = SlotJournal {
            app: &self.app,
            uid: &self.uid,
            now: self.now.time(),
        };
        let result = handle.block_on(exchange.run(
            &mut journal,
            &self.parsed.last,
            &mut turns,
            self.day,
            &self.parsed.extra,
            emit,
            None,
        ));
        if let Ok(r) = &result {
            // The turns this exchange added go to the shared session (Python appends to it).
            // Exchange::run pushes the question and the answer last (it trims from the front).
            if matches!(
                r.get("decision").and_then(Value::as_str),
                Some("logged" | "skipped")
            ) && turns.len() >= 2
            {
                let added: Vec<Value> = turns.get(turns.len() - 2..).unwrap_or_default().to_vec();
                let mut shared = session.lock().unwrap_or_else(|e| e.into_inner());
                shared.extend(added);
                let excess = shared
                    .len()
                    .saturating_sub(diary_answer::exchange::SESSION_TURNS);
                shared.drain(..excess);
            }
            // `_reindex_today` in the background, once the exchange returned.
            crate::reindex::reindex_today(
                self.app.clone(),
                self.uid.clone(),
                corpus::document_path(&self.tenant.layout, self.day),
                self.embedder.clone(),
            );
        }
        result
    }

    async fn json(self) -> Response<Body> {
        // _session + the seed happen before _run_exchange.
        let session = self.sessions.get(&self.uid, &self.parsed.session_id);
        seed(&session, &self.parsed.prior);
        let handle = tokio::runtime::Handle::current();
        let out = tokio::task::spawn_blocking(move || {
            let result = self.exchange(&handle, None);
            match &result {
                Ok(r) => completion(r),
                Err(e) => failure_response(e),
            }
        })
        .await;
        out.unwrap_or_else(|_| error_response(502, "model error", "api_error", None))
    }

    /// `exchange_stream(work)`: the fixed status frame, the exchange's events (never reasoning),
    /// then `diary` and `done` or one error event; a keep-alive comment after 5 s of silence. A
    /// reader that goes away does not stop the exchange (its save is journaled); later events
    /// are dropped.
    fn stream(self) -> Response<Body> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(streaming::QUEUE_LIMIT);
        // Unbounded: the exchange emits from inside its runtime calls, where it must not block
        // (Python's queue holds 256 and waits; named difference: no wait here).
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<Option<Value>>();
        let handle = tokio::runtime::Handle::current();
        // The writer: frames from the events, keep-alives while idle.
        let frames = tx.clone();
        handle.spawn(async move {
            let _ = frames.send(Bytes::from(streaming::STATUS_FRAME)).await;
            loop {
                let next = tokio::time::timeout(
                    Duration::from_secs(streaming::KEEP_ALIVE_AFTER_S),
                    events_rx.recv(),
                )
                .await;
                let frame = match next {
                    Err(_) => streaming::KEEP_ALIVE_FRAME.to_owned(),
                    Ok(None | Some(None)) => break,
                    Ok(Some(Some(event))) => streaming::frame(&event),
                };
                if frames.send(Bytes::from(frame)).await.is_err() {
                    // The reader went away: keep draining so the exchange never waits.
                    while let Some(Some(_)) = events_rx.recv().await {}
                    break;
                }
            }
        });
        drop(tx);
        let worker = handle.clone();
        handle.spawn_blocking(move || {
            let sink = events_tx.clone();
            let mut emit = |e: Value| {
                if streaming::shown(&e) {
                    let _ = sink.send(Some(e));
                }
            };
            // work(emit): _recover_state (done: the tenant is recovered here), the session seed.
            let session = self.sessions.get(&self.uid, &self.parsed.session_id);
            seed(&session, &self.parsed.prior);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.exchange(&worker, Some(&mut emit))
            }));
            let last = match &result {
                Ok(Ok(r)) => vec![streaming::result_event(r), streaming::done_event()],
                Ok(Err(e)) => vec![failure_event(e)],
                // Python's `except Exception`: the generic error event.
                Err(_) => vec![streaming::error_event(Failure::Other)],
            };
            for e in last {
                let _ = events_tx.send(Some(e));
            }
            let _ = events_tx.send(None);
        });
        let mut resp = Response::builder().status(200);
        for (k, v) in streaming::HEADERS {
            resp = resp.header(k, v);
        }
        resp.body(Body::new(Frames(rx))).unwrap_or_default()
    }
}

/// The stream's body: the frames the writer sends, until it drops its sender. Dropping the body
/// (the reader went away) closes the channel; the writer then only drains the exchange's events.
struct Frames(tokio::sync::mpsc::Receiver<Bytes>);

impl http_body::Body for Frames {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|frame| frame.map(|b| Ok(http_body::Frame::data(b))))
    }
}

/// The operator flag and the request header (`trace_requested`).
pub fn trace_requested(headers: &axum::http::HeaderMap) -> bool {
    std::env::var(TRACE_FLAG).ok().as_deref() == Some("1")
        && headers
            .get(TRACE_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| crate::py::strip(v) == "1")
}
