//! The Diary's model client over HTTP (the transport half of `diary/agent/llm.py`'s `LLMClient`).
//!
//! The protocol (payloads, the stream state machine, retry classes, failure kinds) is
//! `diary_core::llm`; this module sends and reads. A streamed answer is read chunk by chunk:
//! every provider line reports progress to the exchange deadline, and leaving the read (the cap
//! was passed, or the deadline dropped the call) closes the connection, so the model server stops
//! generating (#127).
//!
//! Known differences from the Python:
//! - The served-model resolution (`resolve_served_model`) is not ported: the Diary builds its
//!   clients with a fixed model (`resolve_served_model=False`), so the model id is sent as
//!   configured, refused when empty (#2099).
//! - Transport failures are named after the closest httpx class: a refused or failed connection
//!   is `ConnectError`, a connect timeout `ConnectTimeout`, a read timeout `ReadTimeout`, anything
//!   else while reading `ReadError`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use diary_core::llm::{self, ChatReply, Completion, Event, Flow, LlmError, StreamState};
use serde_json::{json, Value};

use crate::deadline::Progress;

pub type Sleep = Arc<dyn Fn(f64) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub api_key: String,
    pub chat_model: String,
    pub embed_model: String,
    /// A dedicated embeddings server; the engine credential is never sent there.
    pub embed_base_url: String,
    pub timeout_s: f64,
    pub max_retries: i64,
    pub retry_backoff_s: f64,
    pub reasoning_tokens: i64,
    pub setting_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            chat_model: String::new(),
            embed_model: String::new(),
            embed_base_url: String::new(),
            timeout_s: 300.0,
            max_retries: 3,
            retry_backoff_s: 2.0,
            reasoning_tokens: llm::DEFAULT_MAX_REASONING_TOKENS,
            setting_key: "LLM_CHAT_MODEL".into(),
        }
    }
}

pub struct Client {
    cfg: Config,
    chat: reqwest::Client,
    embed: reqwest::Client,
    embed_base: String,
    embed_auth: bool,
    sleep: Sleep,
}

fn transport_class(e: &reqwest::Error) -> &'static str {
    if e.is_connect() {
        if e.is_timeout() {
            "ConnectTimeout"
        } else {
            "ConnectError"
        }
    } else if e.is_timeout() {
        "ReadTimeout"
    } else {
        "ReadError"
    }
}

fn tokio_sleep() -> Sleep {
    Arc::new(|s: f64| {
        Box::pin(tokio::time::sleep(
            // A wait too long to represent waits "forever" (Python's sleep raises), never zero.
            Duration::try_from_secs_f64(s.max(0.0)).unwrap_or(Duration::MAX),
        )) as Pin<Box<dyn Future<Output = ()> + Send>>
    })
}

/// The read of one streamed answer (or a plain JSON body) from a response.
async fn read_stream(
    mut resp: reqwest::Response,
    model: &str,
    max_tokens: Option<i64>,
    emit: &mut Option<&mut (dyn FnMut(Event) + Send)>,
    progress: Option<&Progress>,
) -> Result<Completion, LlmError> {
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(LlmError::Status(status));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let mut state = StreamState::new(model, max_tokens, emit.is_some());
    let mut events = Vec::new();
    let flush = |events: &mut Vec<Event>, emit: &mut Option<&mut (dyn FnMut(Event) + Send)>| {
        if let Some(f) = emit.as_mut() {
            for e in events.drain(..) {
                f(e);
            }
        } else {
            events.clear();
        }
    };
    if !content_type.contains("text/event-stream") {
        let body = resp
            .bytes()
            .await
            .map_err(|e| LlmError::Transport(transport_class(&e)))?;
        let mut alive = || {
            if let Some(p) = progress {
                p.alive();
            }
        };
        let read = state.body(&body, &mut events, &mut alive);
        flush(&mut events, emit);
        read?;
        return state.finish();
    }
    let mut utf8 = llm::Utf8Decoder::default();
    let mut lines = llm::LineDecoder::default();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| LlmError::Transport(transport_class(&e)))?;
        let batch = match &chunk {
            Some(bytes) => lines.decode(&utf8.decode(bytes)),
            None => {
                let mut tail = lines.decode(&utf8.flush());
                tail.extend(lines.flush());
                tail
            }
        };
        for line in batch {
            if let Some(p) = progress {
                p.alive();
            }
            let step = state.line(&line, &mut events);
            flush(&mut events, emit);
            if step? == Flow::Stop {
                // Leaving here drops the response: the connection closes and the server stops.
                return state.finish();
            }
        }
        if chunk.is_none() {
            return state.finish();
        }
    }
}

impl Client {
    pub fn new(cfg: Config) -> Result<Self, LlmError> {
        let timeout =
            Duration::try_from_secs_f64(cfg.timeout_s.max(0.0)).unwrap_or(Duration::from_secs(300));
        let build = |redirects: bool| {
            reqwest::Client::builder()
                .connect_timeout(timeout)
                .read_timeout(timeout)
                .redirect(if redirects {
                    reqwest::redirect::Policy::limited(20)
                } else {
                    reqwest::redirect::Policy::none()
                })
                .no_proxy()
                .build()
                .map_err(|_| LlmError::Transport("ConnectError"))
        };
        let base = cfg.base_url.trim_end_matches('/').to_owned();
        let embed_base = cfg.embed_base_url.trim_end_matches('/').to_owned();
        let separate = !embed_base.is_empty() && embed_base != base;
        Ok(Self {
            chat: build(true)?,
            // Embedding calls never follow a redirect: it would carry the Diary's text elsewhere.
            embed: build(false)?,
            embed_base: if separate { embed_base } else { base.clone() },
            embed_auth: !separate,
            cfg: Config {
                base_url: base,
                ..cfg
            },
            sleep: tokio_sleep(),
        })
    }

    /// Replaces the retry wait (tests record it).
    pub fn with_sleep(mut self, sleep: Sleep) -> Self {
        self.sleep = sleep;
        self
    }

    fn model(&self) -> Result<&str, LlmError> {
        if self.cfg.chat_model.trim().is_empty() {
            return Err(LlmError::ModelNotServed {
                setting: self.cfg.setting_key.clone(),
                message: format!(
                    "no model id is set in {}; set it to an id the model engine serves (the diary never lets the engine choose a model)",
                    self.cfg.setting_key
                ),
            });
        }
        Ok(&self.cfg.chat_model)
    }

    fn post(
        &self,
        client: &reqwest::Client,
        base: &str,
        auth: bool,
        path: &str,
        body: &Value,
    ) -> reqwest::RequestBuilder {
        let mut req = client
            .post(format!("{base}{path}"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .body(body.to_string());
        if auth && !self.cfg.api_key.is_empty() {
            req = req.bearer_auth(&self.cfg.api_key);
        }
        req
    }

    async fn send(&self, body: &Value) -> Result<reqwest::Response, LlmError> {
        self.post(
            &self.chat,
            &self.cfg.base_url,
            true,
            "/chat/completions",
            body,
        )
        .send()
        .await
        .map_err(|e| LlmError::Transport(transport_class(&e)))
    }

    async fn wait(&self, attempt: i64) {
        let s = self.cfg.retry_backoff_s * 2f64.powi(i32::try_from(attempt).unwrap_or(i32::MAX));
        (self.sleep)(s).await;
    }

    /// Python `chat_stream`: streamed once, events emitted, the answer checked (#135/#146).
    pub async fn chat_stream(
        &self,
        messages: &Value,
        emit: &mut (dyn FnMut(Event) + Send),
        temperature: f64,
        max_tokens: Option<i64>,
        progress: Option<&Progress>,
    ) -> Result<ChatReply, LlmError> {
        let model = self.model()?.to_owned();
        let payload = llm::chat_payload(
            &model,
            messages,
            temperature,
            true,
            max_tokens,
            self.cfg.reasoning_tokens,
            None,
        );
        let resp = self.send(&payload).await?;
        let mut sink: Option<&mut (dyn FnMut(Event) + Send)> = Some(&mut *emit);
        let c = read_stream(resp, &model, max_tokens, &mut sink, progress).await?;
        let mut events = Vec::new();
        let reply = llm::finish_chat_stream(c, &mut events);
        for e in events {
            emit(e);
        }
        reply
    }

    /// Python `chat`: with `progress` (a call bounded by the exchange deadline) the request is
    /// streamed internally and retried only before the answer began; without it, one plain
    /// completion with `_post_with_retries`.
    pub async fn chat(
        &self,
        messages: &Value,
        temperature: f64,
        max_tokens: Option<i64>,
        stop: Option<&[String]>,
        progress: Option<&Progress>,
    ) -> Result<ChatReply, LlmError> {
        let model = self.model()?.to_owned();
        let tries = self.cfg.max_retries;
        if progress.is_some() {
            let payload = llm::chat_payload(
                &model,
                messages,
                temperature,
                true,
                max_tokens,
                self.cfg.reasoning_tokens,
                stop,
            );
            let mut last = String::from("None");
            for attempt in 0..tries.max(0) {
                let outcome = match self.send(&payload).await {
                    Ok(resp) => read_stream(resp, &model, max_tokens, &mut None, progress).await,
                    Err(e) => Err(e),
                };
                match outcome {
                    Ok(c) => {
                        return Ok(ChatReply {
                            content: llm::strip_reasoning(&c.content, true),
                            reasoning: c.reasoning,
                            truncated: c.finish == json!("length"),
                        })
                    }
                    Err(LlmError::Status(status)) if status < 500 => {
                        return Err(LlmError::Llm(format!("chat failed: HTTP {status}")))
                    }
                    Err(LlmError::Status(status)) => last = format!("HTTPStatusError {status}"),
                    Err(LlmError::Transport(class @ ("ConnectError" | "ConnectTimeout"))) => {
                        last = class.to_owned()
                    }
                    Err(e @ LlmError::Llm(_)) => return Err(e),
                    Err(e) => {
                        return Err(LlmError::Llm(format!("chat failed: {}", e.python_class())))
                    }
                }
                if attempt == tries - 1 {
                    break;
                }
                self.wait(attempt).await;
            }
            return Err(LlmError::Llm(format!(
                "chat failed after {tries} attempts: {last}"
            )));
        }
        let payload = llm::chat_payload(
            &model,
            messages,
            temperature,
            false,
            max_tokens,
            self.cfg.reasoning_tokens,
            stop,
        );
        let mut last = String::from("None");
        for attempt in 0..tries.max(0) {
            let outcome = async {
                let resp = self.send(&payload).await?;
                let status = resp.status().as_u16();
                if !(200..300).contains(&status) {
                    return Err(LlmError::Status(status));
                }
                let body = resp
                    .bytes()
                    .await
                    .map_err(|e| LlmError::Transport(transport_class(&e)))?;
                llm::parse_chat(&llm::parse_json_bytes(&body)?, max_tokens)
            }
            .await;
            match outcome {
                Ok(reply) => return Ok(reply),
                Err(e) => last = e.python_class().to_owned(),
            }
            if attempt == tries - 1 {
                break;
            }
            self.wait(attempt).await;
        }
        Err(LlmError::Llm(format!(
            "chat failed after {tries} attempts: {last}"
        )))
    }

    /// Python `embed`: one vector per text, from the embeddings server (no redirects followed,
    /// a too-long input reported, never retried).
    pub async fn embed(
        &self,
        texts: &[String],
        model: Option<&str>,
    ) -> Result<Vec<Vec<f64>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Python `model or self.embed_model`: an empty id means the configured one.
        let model = model
            .filter(|m| !m.is_empty())
            .unwrap_or(&self.cfg.embed_model);
        let body = json!({"model": model, "input": texts});
        let tries = self.cfg.max_retries;
        let mut last = String::from("None");
        for attempt in 0..tries.max(0) {
            let outcome = async {
                let resp = self
                    .post(
                        &self.embed,
                        &self.embed_base,
                        self.embed_auth,
                        "/embeddings",
                        &body,
                    )
                    .send()
                    .await
                    .map_err(|e| EmbedError::Retry(transport_class(&e).to_owned()))?;
                let status = resp.status().as_u16();
                if (300..400).contains(&status) {
                    return Err(EmbedError::Redirected(status));
                }
                let bytes = resp
                    .bytes()
                    .await
                    .map_err(|e| EmbedError::Retry(transport_class(&e).to_owned()))?;
                if !(200..300).contains(&status) {
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some(t) = llm::too_large(status, &text) {
                        return Err(EmbedError::TooLarge(t));
                    }
                    return Err(EmbedError::Retry(format!("HTTPStatusError {status}")));
                }
                let data = llm::parse_json_bytes(&bytes)
                    .map_err(|e| EmbedError::Retry(e.python_class().to_owned()))?;
                llm::parse_embed(&data, texts.len())
                    .map_err(|e| EmbedError::Retry(e.python_class().to_owned()))
            }
            .await;
            match outcome {
                Ok(v) => return Ok(v),
                Err(e @ (EmbedError::Redirected(_) | EmbedError::TooLarge(_))) => return Err(e),
                Err(EmbedError::Retry(class)) | Err(EmbedError::Failed(class)) => last = class,
            }
            if attempt == tries - 1 {
                break;
            }
            self.wait(attempt).await;
        }
        Err(EmbedError::Failed(format!(
            "embeddings failed after {tries} attempts: {last}"
        )))
    }
}

/// A failed embedding call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedError {
    /// The server answered with a redirect; refused, never retried.
    Redirected(u16),
    /// #1759: the server refused an input as too long; never retried.
    TooLarge(llm::TooLarge),
    /// A failure worth another try (internal).
    Retry(String),
    /// Every try failed.
    Failed(String),
}
