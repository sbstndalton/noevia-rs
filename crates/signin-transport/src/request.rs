//! A `/chat/completions` request to the Responses request the backend takes (`toResponsesRequest`,
//! `responseFormat`) and the checks `fetchFor` makes before sending (path, JSON, model, catalogue
//! allowlist, reasoning effort, the model's defaults, the prompt-cache key).

use crate::js;
use crate::texts::{assign, Texts};
use js_json::{js_slice, JValue};
use sha2::{Digest, Sha256};

/// The levels forwarded on any model (`BASIC_EFFORTS`).
pub const BASIC_EFFORTS: [&str; 4] = ["minimal", "low", "medium", "high"];

/// A request this transport answers itself (`errorResponse(status, message)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
    /// `code` of the error body (sign-in errors only).
    pub code: Option<String>,
}

impl Refusal {
    pub fn new(status: u16, message: String) -> Self {
        Refusal {
            status,
            message,
            code: None,
        }
    }
}

fn str_of(v: &JValue) -> Option<&str> {
    v.as_str()
}

/// `textOf(content)`.
fn text_of(content: &JValue) -> String {
    match content {
        JValue::Str(s) => s.clone(),
        JValue::Arr(parts) => parts
            .iter()
            .filter(|p| p.is_object() && p.get("type").as_str() == Some("text"))
            .filter_map(|p| str_of(p.get("text")))
            .collect(),
        _ => String::new(),
    }
}

fn input_text(text: &str) -> JValue {
    JValue::obj([
        ("type", JValue::from("input_text")),
        ("text", JValue::from(text)),
    ])
}

/// `userParts(content)`.
fn user_parts(content: &JValue) -> JValue {
    if let JValue::Str(s) = content {
        return JValue::Arr(vec![input_text(s)]);
    }
    let mut parts = Vec::new();
    if let JValue::Arr(items) = content {
        for part in items.iter().filter(|p| p.is_object()) {
            match (
                part.get("type").as_str(),
                part.get("text").as_str(),
                part.get("image_url").get("url").as_str(),
            ) {
                (Some("text"), Some(text), _) => parts.push(input_text(text)),
                (Some("image_url"), _, Some(url)) => parts.push(JValue::obj([
                    ("type", JValue::from("input_image")),
                    ("image_url", JValue::from(url)),
                ])),
                _ => {}
            }
        }
    }
    if parts.is_empty() {
        parts.push(input_text(""));
    }
    JValue::Arr(parts)
}

/// `toolChoice(choice)`.
fn tool_choice(choice: &JValue) -> Option<JValue> {
    match choice {
        JValue::Str(s) if matches!(s.as_str(), "auto" | "none" | "required") => {
            Some(choice.clone())
        }
        JValue::Obj(_) if choice.get("type").as_str() == Some("function") => {
            choice.get("function").get("name").as_str().map(|name| {
                JValue::obj([
                    ("type", JValue::from("function")),
                    ("name", JValue::from(name)),
                ])
            })
        }
        _ => None,
    }
}

/// `responseFormat(rf)`.
fn response_format(rf: &JValue) -> Option<JValue> {
    if !rf.is_object() {
        return None;
    }
    match rf.get("type").as_str() {
        Some("json_object") => Some(JValue::obj([("type", JValue::from("json_object"))])),
        Some("json_schema") => {
            let js = rf.get("json_schema");
            if !js.is_object() || !js.get("schema").is_object() {
                return None;
            }
            let name = match js.get("name").as_str() {
                Some(n) if !n.is_empty() => js_slice(n, 64),
                _ => "response".to_string(),
            };
            let mut out = vec![
                ("type".to_string(), JValue::from("json_schema")),
                ("name".to_string(), JValue::from(name)),
                ("schema".to_string(), js.get("schema").clone()),
                (
                    "strict".to_string(),
                    JValue::Bool(js.get("strict").as_bool() == Some(true)),
                ),
            ];
            if let Some(d) = js.get("description").as_str() {
                out.push(("description".to_string(), JValue::from(d)));
            }
            Some(JValue::Obj(out))
        }
        _ => None,
    }
}

/// `toResponsesRequest(body)`: the Responses body, built from an allowlist (always streamed, never
/// stored; every turn replays the whole conversation).
pub fn to_responses_request(body: &JValue) -> Vec<(String, JValue)> {
    let mut input = Vec::new();
    if let JValue::Arr(messages) = body.get("messages") {
        for m in messages.iter().filter(|m| m.is_object()) {
            match m.get("role").as_str() {
                Some("system" | "developer") => {
                    let text = text_of(m.get("content"));
                    if !text.is_empty() {
                        input.push(JValue::obj([
                            ("role", JValue::from("developer")),
                            ("content", JValue::Arr(vec![input_text(&text)])),
                        ]));
                    }
                }
                Some("user") => input.push(JValue::obj([
                    ("role", JValue::from("user")),
                    ("content", user_parts(m.get("content"))),
                ])),
                Some("assistant") => {
                    let text = text_of(m.get("content"));
                    if !text.is_empty() {
                        input.push(JValue::obj([
                            ("role", JValue::from("assistant")),
                            (
                                "content",
                                JValue::Arr(vec![JValue::obj([
                                    ("type", JValue::from("output_text")),
                                    ("text", JValue::from(text)),
                                ])]),
                            ),
                        ]));
                    }
                    if let JValue::Arr(calls) = m.get("tool_calls") {
                        for call in calls.iter().filter(|c| c.is_object()) {
                            let (Some(id), Some(name)) = (
                                call.get("id").as_str(),
                                call.get("function").get("name").as_str(),
                            ) else {
                                continue;
                            };
                            if name.is_empty() {
                                continue;
                            }
                            let arguments = match call.get("function").get("arguments").as_str() {
                                Some(a) if !a.is_empty() => a,
                                _ => "{}",
                            };
                            input.push(JValue::obj([
                                ("type", JValue::from("function_call")),
                                ("call_id", JValue::from(id)),
                                ("name", JValue::from(name)),
                                ("arguments", JValue::from(arguments)),
                            ]));
                        }
                    }
                }
                Some("tool") => {
                    if let Some(id) = m.get("tool_call_id").as_str() {
                        let output = match m.get("content") {
                            JValue::Str(s) => s.clone(),
                            JValue::Undefined | JValue::Null => "\"\"".to_string(),
                            other => js_json::stringify(other).unwrap_or_default(),
                        };
                        input.push(JValue::obj([
                            ("type", JValue::from("function_call_output")),
                            ("call_id", JValue::from(id)),
                            ("output", JValue::from(output)),
                        ]));
                    }
                }
                _ => {}
            }
        }
    }
    let mut out: Vec<(String, JValue)> = vec![
        (
            "model".into(),
            JValue::from(js::or_str(body.get("model"), "")),
        ),
        ("instructions".into(), JValue::from("")),
        ("input".into(), JValue::Arr(input)),
        ("stream".into(), JValue::Bool(true)),
        ("store".into(), JValue::Bool(false)),
        (
            "include".into(),
            JValue::Arr(vec![JValue::from("reasoning.encrypted_content")]),
        ),
    ];
    let tools: Vec<JValue> = match body.get("tools") {
        JValue::Arr(tools) => tools
            .iter()
            .filter(|t| t.is_object() && t.get("type").as_str() == Some("function"))
            .filter_map(|t| {
                let f = t.get("function");
                let name = f.get("name").as_str().filter(|n| !n.is_empty())?;
                let parameters = if f.get("parameters").is_object() {
                    f.get("parameters").clone()
                } else {
                    JValue::obj([
                        ("type", JValue::from("object")),
                        ("properties", JValue::obj::<&str>([])),
                    ])
                };
                Some(JValue::obj([
                    ("type", JValue::from("function")),
                    ("name", JValue::from(name)),
                    (
                        "description",
                        f.get("description")
                            .as_str()
                            .map_or(JValue::Undefined, JValue::from),
                    ),
                    ("parameters", parameters),
                    (
                        "strict",
                        JValue::Bool(f.get("strict").as_bool() == Some(true)),
                    ),
                ]))
            })
            .collect(),
        _ => Vec::new(),
    };
    if !tools.is_empty() {
        out.push(("tools".into(), JValue::Arr(tools)));
        out.push((
            "tool_choice".into(),
            tool_choice(body.get("tool_choice")).unwrap_or_else(|| JValue::from("auto")),
        ));
    }
    if let Some(effort) = body
        .get("reasoning_effort")
        .as_str()
        .filter(|e| BASIC_EFFORTS.contains(e))
    {
        out.push((
            "reasoning".into(),
            JValue::obj([
                ("effort", JValue::from(effort)),
                ("summary", JValue::from("auto")),
            ]),
        ));
    }
    if let Some(format) = response_format(body.get("response_format")) {
        out.push(("text".into(), JValue::obj([("format", format)])));
    }
    out
}

/// `uuidFrom(text)`: a UUID-shaped, stable digest (never reversible to the text).
pub fn uuid_from(text: &str) -> String {
    let h: String = Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let part = |a: usize, b: usize| h.get(a..b).unwrap_or("");
    let nibble = u8::from_str_radix(part(16, 17), 16).unwrap_or(0);
    format!(
        "{}-{}-5{}-{:x}{}-{}",
        part(0, 8),
        part(8, 12),
        part(13, 16),
        (nibble & 3) | 8,
        part(17, 20),
        part(20, 32)
    )
}

/// `${base without trailing slashes}/responses`, where every request goes.
pub fn responses_url(base: &str) -> String {
    format!("{}/responses", base.trim_end_matches('/'))
}

/// `/\/chat\/completions$/.test(new URL(url).pathname)`.
pub fn is_chat_path(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| u.path().ends_with("/chat/completions"))
}

/// A request that passed the path, JSON and model checks; the catalogue is read next.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub body: JValue,
    /// `String(body.model || '')`.
    pub model: String,
}

/// The first half of `fetchFor`'s checks, before the catalogue read.
pub fn parse(texts: &Texts, url: &str, raw_body: &str) -> Result<Parsed, Refusal> {
    if !is_chat_path(url) {
        return Err(Refusal::new(404, texts.only_chat()));
    }
    let body = js_json::parse(raw_body).map_err(|_| Refusal::new(400, texts.not_json()))?;
    // JSON.parse('null') makes Node's `body.model` throw (the fetch rejects); refused plainly here.
    if body.is_nullish() {
        return Err(Refusal::new(400, texts.not_json()));
    }
    let model = js::or_str(body.get("model"), "");
    if model.is_empty() {
        return Err(Refusal::new(400, texts.choose_model()));
    }
    Ok(Parsed { body, model })
}

/// What to send upstream.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    /// `JSON.stringify(request)`.
    pub body: String,
    /// `body.stream === true`: the caller answers with a stream, else one completion.
    pub stream: bool,
    pub model: String,
}

fn join(values: &[JValue]) -> String {
    values
        .iter()
        .map(|v| {
            if v.is_nullish() {
                String::new()
            } else {
                js::to_string(v)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The second half of `fetchFor`'s checks: `catalogue` is the broker's answer (`None` when it could
/// not be read), `conversation_id` the [`uuid_from`] of user and conversation.
pub fn plan(
    texts: &Texts,
    parsed: &Parsed,
    catalogue: Option<&JValue>,
    conversation_id: Option<&str>,
) -> Result<Planned, Refusal> {
    let Parsed { body, model } = parsed;
    let model80 = js_slice(model, 80);
    let entries: Option<&[JValue]> = match catalogue {
        Some(JValue::Arr(items)) => Some(items),
        _ => None,
    };
    let slug_is = |m: &JValue| m.get("slug").as_str() == Some(model.as_str());
    if let Some(items) = entries {
        if !items.is_empty() && !items.iter().any(slug_is) {
            let first8: Vec<JValue> = items
                .iter()
                .take(8)
                .map(|m| m.get("slug").clone())
                .collect();
            return Err(Refusal::new(
                400,
                texts.not_on_account(&model80, &join(&first8)),
            ));
        }
    }
    let stream = body.get("stream").as_bool() == Some(true);
    let mut request = to_responses_request(body);
    let info = entries.and_then(|items| items.iter().find(|m| slug_is(m)));
    if let Some(asked) = body
        .get("reasoning_effort")
        .as_str()
        .filter(|a| !a.is_empty() && *a != "default")
    {
        let offered = match info.map(|i| i.get("levels")) {
            Some(JValue::Arr(levels)) if !levels.is_empty() => Some(levels),
            _ => None,
        };
        let basic = BASIC_EFFORTS.contains(&asked);
        let allowed = match offered {
            Some(levels) => levels.iter().any(|l| l.as_str() == Some(asked)),
            None => basic,
        };
        if !allowed {
            let shown = js_slice(asked, 40);
            let message = match (offered, info) {
                (Some(levels), _) => texts.effort_not_offered(&shown, &model80, &join(levels)),
                (None, Some(_)) => texts.effort_not_listed(&shown, &model80),
                (None, None) => texts.list_unreadable(&shown, &model80),
            };
            return Err(Refusal::new(400, message));
        }
        if !basic {
            // { ...(request.reasoning || {}), effort: asked, summary: request.reasoning?.summary || 'auto' }
            let prior = request
                .iter()
                .find(|(k, _)| k == "reasoning")
                .map(|(_, v)| v.clone());
            let summary = prior
                .as_ref()
                .map(|p| p.get("summary").clone())
                .filter(JValue::truthy)
                .unwrap_or_else(|| JValue::from("auto"));
            let mut reasoning = match prior {
                Some(JValue::Obj(items)) => items,
                _ => Vec::new(),
            };
            assign(&mut reasoning, "effort", JValue::from(asked));
            assign(&mut reasoning, "summary", summary);
            assign(&mut request, "reasoning", JValue::Obj(reasoning));
        }
    }
    if let Some(info) = info {
        let mut reasoning = match request.iter().find(|(k, _)| k == "reasoning") {
            Some((_, JValue::Obj(items))) => items.clone(),
            _ => Vec::new(),
        };
        let has_effort = reasoning.iter().any(|(k, v)| k == "effort" && v.truthy());
        if !has_effort && info.get("effort").truthy() {
            assign(&mut reasoning, "effort", info.get("effort").clone());
        }
        let summary = info.get("summary");
        if info.get("summaryParam").truthy() && summary.as_str() != Some("none") {
            let value = if summary.truthy() {
                summary.clone()
            } else {
                JValue::from("auto")
            };
            assign(&mut reasoning, "summary", value);
        } else {
            reasoning.retain(|(k, _)| k != "summary");
        }
        if reasoning.is_empty() {
            request.retain(|(k, _)| k != "reasoning");
        } else {
            assign(&mut request, "reasoning", JValue::Obj(reasoning));
        }
    }
    if let Some(id) = conversation_id {
        assign(&mut request, "prompt_cache_key", JValue::from(id));
    }
    Ok(Planned {
        body: js_json::stringify(&JValue::Obj(request)).unwrap_or_default(),
        stream,
        model: model.clone(),
    })
}

/// Header names of the backend's protocol; the caller's, so no provider name is built in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protocol {
    /// The account id header.
    pub account_header: String,
    /// The header set to `true` for accounts in the restricted government region.
    pub region_header: String,
    pub user_agent: String,
}

/// The session fields a request needs (the broker's answer, never a refresh token).
#[derive(Clone, PartialEq)]
pub struct Session {
    pub access_token: String,
    pub account_id: String,
    pub restricted_region: bool,
    /// Milliseconds since the epoch, when the broker sent one.
    pub expires_at: Option<f64>,
    /// The broker's `generation`, passed back as is when this sign-in is rejected.
    pub generation: JValue,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("access_token", &"<redacted>")
            .field("account_id", &self.account_id)
            .field("restricted_region", &self.restricted_region)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// `upstreamHeaders(t, accept, conversationId)`, in Node's order.
pub fn headers(
    protocol: &Protocol,
    session: &Session,
    accept: &str,
    conversation_id: Option<&str>,
) -> Vec<(String, String)> {
    let mut h = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), accept.to_string()),
        (
            "authorization".to_string(),
            format!("Bearer {}", session.access_token),
        ),
        (protocol.account_header.clone(), session.account_id.clone()),
        ("user-agent".to_string(), protocol.user_agent.clone()),
    ];
    if session.restricted_region {
        h.push((protocol.region_header.clone(), "true".to_string()));
    }
    if let Some(id) = conversation_id {
        h.push(("session-id".to_string(), id.to_string()));
        h.push(("x-client-request-id".to_string(), id.to_string()));
    }
    h
}
