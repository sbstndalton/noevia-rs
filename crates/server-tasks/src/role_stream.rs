//! Native guarded model streaming. Transport owns only engine I/O; retries, prompt isolation,
//! schema validation and bounded correction remain here. Response bodies stop on drop.
use crate::role_prompt;
use js_json::{wtf16, JValue};
use std::{future::Future, pin::Pin};
use tokio::sync::watch;
pub type Wait<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Aborted,
    Transport,
    Invalid,
    ContextRefused,
    TooLarge,
    ContextInvalid,
}
pub trait Body: Send {
    fn next(&mut self) -> Wait<'_, Result<Option<Vec<u8>>, Error>>;
}
pub struct Response {
    pub status: u16,
    pub body: Option<Box<dyn Body>>,
}
pub trait Transport: Send + Sync {
    /// Dropping a send future or its body stops the engine request, including header waits.
    /// Residency/budget/machine identity must be confirmed continuously by the pinned transport.
    fn send(
        &self,
        payload: JValue,
        cancel: watch::Receiver<bool>,
    ) -> Wait<'_, Result<Response, Error>>;
}
pub struct Call<'a> {
    pub role: &'a str,
    pub state: &'a JValue,
    pub roles: &'a [String],
    pub model: &'a str,
    pub thinking: bool,
    pub instructions: &'a str,
    pub schema: &'a JValue,
    pub schema_name: &'a str,
    pub constrain: bool,
    pub schema_supported: bool,
    pub reasoning_model: bool,
    pub max_tokens: u32,
    pub max_bytes: u32,
}
pub struct Answer {
    pub text: String,
    pub corrected: bool,
    pub attempts: u32,
    pub constraint: JValue,
    pub prefix: String,
}
fn to_units(s: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for c in s.chars() {
        if let Some(unit) = wtf16::lone_unit(c) {
            out.push(unit);
        } else {
            out.extend(c.encode_utf16(&mut [0; 2]).iter().copied());
        }
    }
    out
}
fn from_units(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|c| match c {
            Ok(c) => c,
            Err(e) => {
                wtf16::lone_char(e.unpaired_surrogate()).unwrap_or(char::REPLACEMENT_CHARACTER)
            }
        })
        .collect()
}
fn text(s: impl Into<String>) -> JValue {
    JValue::Str(s.into())
}
fn object(fields: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn outcome(applied: bool, reason: Option<&str>, fallback: bool) -> JValue {
    object(vec![
        ("applied", JValue::Bool(applied)),
        (
            "mode",
            if applied {
                text("json_schema")
            } else {
                JValue::Null
            },
        ),
        ("reason", reason.map_or(JValue::Null, text)),
        ("fallback", JValue::Bool(fallback)),
    ])
}
async fn send(
    transport: &dyn Transport,
    payload: JValue,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Response, Error> {
    if *cancel.borrow() {
        return Err(Error::Aborted);
    }
    let forwarded = cancel.clone();
    tokio::select! {biased;_=cancel.changed()=>Err(Error::Aborted),reply=transport.send(payload,forwarded)=>reply}
}
fn correction(v: &stream_guard::Violation) -> JValue {
    object(vec![
        ("type", text("schema_violation_correction")),
        (
            "violation",
            object(vec![
                ("message", text(from_units(&v.message))),
                (
                    "path",
                    if v.path.is_empty() {
                        JValue::Null
                    } else {
                        text(from_units(&v.path))
                    },
                ),
            ]),
        ),
    ])
}
async fn consume(
    mut body: Box<dyn Body>,
    schema: &stream_guard::Schema,
    max_bytes: u32,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Result<String, JValue>, Error> {
    let state = stream_guard::State::new(stream_guard::DEFAULT_MAX_DEPTH, i64::from(max_bytes))
        .map_err(|_| Error::Invalid)?;
    let mut validator = stream_guard::Validator::new(schema, state);
    let mut answer = Vec::new();
    let mut buffer = Vec::new();
    let mut finished = false;
    while !finished {
        if *cancel.borrow() {
            return Err(Error::Aborted);
        }
        let chunk = tokio::select! {biased;_=cancel.changed()=>return Err(Error::Aborted),chunk=body.next()=>chunk?};
        let Some(chunk) = chunk else {
            break;
        };
        buffer.extend(chunk);
        // A model cannot grow an unterminated SSE frame without bound. Never expose frame text.
        if buffer.len() > 8 * 1024 * 1024 {
            return Err(Error::Transport);
        }
        while let Some(nl) = buffer.iter().position(|b| *b == b'\n') {
            let rest = buffer.split_off(nl + 1);
            let line = String::from_utf8_lossy(&buffer).into_owned();
            buffer = rest;
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = server_account::util::js_trim(data);
            if data == "[DONE]" {
                finished = true;
                break;
            }
            let Ok(event) = wtf16::parse(data) else {
                continue;
            };
            if event.get("error").truthy() {
                return Err(Error::Transport);
            }
            let first = match event.get("choices") {
                JValue::Arr(choices) => choices.first().unwrap_or(&JValue::Undefined),
                _ => &JValue::Undefined,
            };
            let delta = first.get("delta").get("content");
            if let Some(delta) = delta.as_str().filter(|s| !s.is_empty()) {
                let units = to_units(delta);
                answer.extend_from_slice(&units);
                if let Some(violation) = validator.feed(&units) {
                    return Ok(Err(correction(violation)));
                }
            }
        }
    }
    if *cancel.borrow() {
        return Err(Error::Aborted);
    }
    if let Some(violation) = validator.end() {
        return Ok(Err(correction(violation)));
    }
    Ok(Ok(from_units(&answer)))
}
pub async fn call(
    transport: &dyn Transport,
    input: Call<'_>,
    mut cancel: watch::Receiver<bool>,
) -> Result<Answer, Error> {
    if !input.roles.iter().any(|role| role == input.role) {
        return Err(Error::ContextInvalid);
    }
    let schema = wtf16::stringify(input.schema).ok_or(Error::Invalid)?;
    let schema = stream_guard::Schema::parse(schema.as_bytes()).map_err(|_| Error::Invalid)?;
    let reason = if !input.constrain {
        Some("flag_off")
    } else if !input.schema_supported {
        Some("provider_unsupported")
    } else if input.reasoning_model {
        Some("reasoning_model")
    } else if input.thinking {
        Some("thinking_enabled")
    } else {
        None
    };
    let mut constrained = reason.is_none();
    let mut constraint = outcome(constrained, reason, false);
    let mut correction = None;
    for attempt in 1..=2 {
        let prompt = role_prompt::prepare(
            input.role,
            input.state,
            input.roles,
            input.model,
            input.thinking,
            input.instructions,
            correction.as_ref(),
        )
        .map_err(|e| match e {
            role_prompt::Refusal::ContextRefused => Error::ContextRefused,
            role_prompt::Refusal::TooLarge => Error::TooLarge,
            _ => Error::ContextInvalid,
        })?;
        let payload = object(vec![
            ("model", text(input.model)),
            ("messages", prompt.messages),
            ("stream", JValue::Bool(true)),
            ("cache_prompt", JValue::Bool(true)),
            (
                "stream_options",
                object(vec![("include_usage", JValue::Bool(true))]),
            ),
            ("temperature", JValue::Num(0.0)),
            ("max_tokens", JValue::Num(f64::from(input.max_tokens))),
            (
                "chat_template_kwargs",
                object(vec![("enable_thinking", JValue::Bool(input.thinking))]),
            ),
        ]);
        let mut effective = payload.clone();
        if constrained {
            if let JValue::Obj(fields) = &mut effective {
                fields.push((
                    "response_format".into(),
                    object(vec![
                        ("type", text("json_schema")),
                        (
                            "json_schema",
                            object(vec![
                                ("name", text(input.schema_name)),
                                ("strict", JValue::Bool(true)),
                                ("schema", input.schema.clone()),
                            ]),
                        ),
                    ]),
                ));
            }
        }
        let mut response = send(transport, effective, &mut cancel).await?;
        if constrained && matches!(response.status, 400 | 422 | 501) {
            let status = response.status;
            drop(response);
            constrained = false;
            constraint = outcome(false, Some(&format!("engine_rejected_{status}")), true);
            response = send(transport, payload, &mut cancel).await?;
        }
        if !(200..300).contains(&response.status) {
            return Err(Error::Transport);
        }
        let body = response.body.take().ok_or(Error::Transport)?;
        match consume(body, &schema, input.max_bytes, &mut cancel).await? {
            Ok(text) => {
                return Ok(Answer {
                    text,
                    corrected: attempt == 2,
                    attempts: attempt,
                    constraint,
                    prefix: prompt.prefix,
                })
            }
            Err(next) => correction = Some(next),
        }
    }
    Err(Error::Invalid)
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
    };
    struct Stream {
        chunks: VecDeque<Vec<u8>>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Stream {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Body for Stream {
        fn next(&mut self) -> Wait<'_, Result<Option<Vec<u8>>, Error>> {
            Box::pin(async move { Ok(self.chunks.pop_front()) })
        }
    }
    struct Fake {
        replies: Mutex<VecDeque<(u16, Vec<Vec<u8>>)>>,
        sent: Mutex<Vec<JValue>>,
        drops: Arc<AtomicUsize>,
    }
    impl Transport for Fake {
        fn send(
            &self,
            payload: JValue,
            _: watch::Receiver<bool>,
        ) -> Wait<'_, Result<Response, Error>> {
            Box::pin(async move {
                self.sent.lock().unwrap().push(payload);
                let (status, chunks) = self.replies.lock().unwrap().pop_front().unwrap();
                Ok(Response {
                    status,
                    body: Some(Box::new(Stream {
                        chunks: chunks.into(),
                        drops: self.drops.clone(),
                    })),
                })
            })
        }
    }
    fn chunks(s: &str) -> Vec<Vec<u8>> {
        vec![format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
            wtf16::stringify(&text(s)).unwrap()
        )
        .into_bytes()]
    }
    fn fixture() -> JValue {
        wtf16::parse(include_str!("../tests/fixtures/role-prompts-node.json")).unwrap()
    }
    fn input<'a>(f: &'a JValue, roles: &'a [String], schema: &'a JValue) -> Call<'a> {
        Call {
            role: "planner",
            state: f.get("state"),
            roles,
            model: "synthetic-model",
            thinking: false,
            instructions: "Instructions for planner",
            schema,
            schema_name: "answer",
            constrain: true,
            schema_supported: true,
            reasoning_model: false,
            max_tokens: 1400,
            max_bytes: 32768,
        }
    }
    #[tokio::test]
    async fn constraint_fallback_and_bounded_correction_drop_each_response() {
        let f = fixture();
        let JValue::Arr(f) = f else { panic!() };
        let f = &f[0];
        let roles = role_prompt::ROLES.map(str::to_owned);
        let schema=wtf16::parse(r#"{"type":"object","required":["answer"],"additionalProperties":false,"properties":{"answer":{"type":"string"}}}"#).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let fake = Fake {
            replies: Mutex::new(VecDeque::from([
                (422, vec![b"synthetic reflected token never read".to_vec()]),
                (200, chunks("bad")),
                (200, chunks(r#"{"answer":"fine"}"#)),
            ])),
            sent: Mutex::new(vec![]),
            drops: drops.clone(),
        };
        let (_tx, rx) = watch::channel(false);
        let answer = call(&fake, input(f, &roles, &schema), rx).await.unwrap();
        assert_eq!(answer.text, r#"{"answer":"fine"}"#);
        assert_eq!(answer.attempts, 2);
        assert!(answer.corrected);
        assert_eq!(
            answer.constraint,
            outcome(false, Some("engine_rejected_422"), true)
        );
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        let captured =
            wtf16::parse(include_str!("../tests/fixtures/role-stream-node.json")).unwrap();
        assert_eq!(
            answer.text,
            captured.get("answer").get("text").as_str().unwrap()
        );
        assert_eq!(answer.constraint, *captured.get("answer").get("constraint"));
        assert_eq!(
            answer.prefix,
            captured.get("answer").get("prefix").as_str().unwrap()
        );
        let sent = fake.sent.lock().unwrap();
        assert_eq!(JValue::Arr(sent.clone()), *captured.get("requests"));
        assert_eq!(sent.len(), 3);
        assert!(sent[0].get("response_format").is_object());
        assert_eq!(sent[1].get("response_format"), &JValue::Undefined);
        assert_eq!(sent[2].get("response_format"), &JValue::Undefined);
        let rendered = wtf16::stringify(&sent[2]).unwrap();
        assert!(rendered.contains("previous answer was not valid"));
        assert!(!rendered.contains("reflected token"));
    }
    #[tokio::test]
    async fn split_utf8_frames_reasoning_and_malformed_events_do_not_pollute_answer() {
        let f = fixture();
        let JValue::Arr(f) = f else { panic!() };
        let roles = role_prompt::ROLES.map(str::to_owned);
        let schema = wtf16::parse(r#"{"type":"object"}"#).unwrap();
        let mut bytes=b"data: broken\n\ndata: {\"choices\":[{\"delta\":{\"reasoning_content\":\"private-thought\"}}]}\n\n".to_vec();
        bytes.extend(chunks(r#"{"answer":"😀"}"#).concat());
        let fake = Fake {
            replies: Mutex::new(VecDeque::from([(
                200,
                bytes.chunks(1).map(<[u8]>::to_vec).collect(),
            )])),
            sent: Mutex::new(vec![]),
            drops: Arc::new(AtomicUsize::new(0)),
        };
        let (_tx, rx) = watch::channel(false);
        let answer = call(&fake, input(&f[0], &roles, &schema), rx)
            .await
            .unwrap();
        assert_eq!(answer.text, r#"{"answer":"😀"}"#);
    }
    struct Blocked {
        entered: tokio::sync::Notify,
        dropped: Arc<AtomicUsize>,
    }
    struct Waiting(Arc<AtomicUsize>);
    impl Drop for Waiting {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Transport for Blocked {
        fn send(&self, _: JValue, _: watch::Receiver<bool>) -> Wait<'_, Result<Response, Error>> {
            Box::pin(async move {
                let _hold = Waiting(self.dropped.clone());
                self.entered.notify_one();
                std::future::pending().await
            })
        }
    }
    #[tokio::test]
    async fn abort_during_actual_header_wait_drops_transport_operation() {
        let f = fixture();
        let JValue::Arr(f) = f else { panic!() };
        let roles = role_prompt::ROLES.map(str::to_owned);
        let schema = object(vec![]);
        let drops = Arc::new(AtomicUsize::new(0));
        let transport = Blocked {
            entered: tokio::sync::Notify::new(),
            dropped: drops.clone(),
        };
        let (tx, rx) = watch::channel(false);
        let work = call(&transport, input(&f[0], &roles, &schema), rx);
        tokio::pin!(work);
        tokio::select! {_=transport.entered.notified()=>{},_= &mut work=>panic!()};
        tx.send_replace(true);
        assert!(matches!(work.await, Err(Error::Aborted)));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
