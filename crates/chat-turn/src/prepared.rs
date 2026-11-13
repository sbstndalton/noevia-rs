//! The `prepared` frame noevia-core sends when it hands a turn to the native executor
//! (`server/rust-chat-native.cjs` `preparedTurn`, v1): everything the turn needs after its
//! preparation. Read strictly: a frame of another version, or one missing a field the turn uses, is
//! refused, and the executor then has nothing to run (the front never asks for a hand-off it cannot
//! read: the version is checked before the front's switch sends a turn natively).

use js_json::JValue;

use crate::effort::Provider;

pub const VERSION: f64 = 1.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Title {
    pub chat_id: String,
    pub list_id: Option<String>,
    pub first_message: String,
    pub first_reply: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub started_at: f64,
    pub model: String,
    pub role: Option<String>,
    pub auto: bool,
    pub effort: String,
    pub thinking_off: bool,
    /// The sampling parameters, merged into every request (`...sampling.params`).
    pub sampling: JValue,
    pub provider: Provider,
    pub provider_label: String,
    pub upstream_url: String,
    pub limit_scope: String,
    pub messages: Vec<JValue>,
    pub tools: Vec<JValue>,
    pub routes: Vec<(String, String)>,
    pub writes: Vec<String>,
    pub public: Vec<String>,
    pub automatic_only_writes: Vec<String>,
    pub route_note: String,
    pub template_gate: bool,
    pub chat_id: Option<String>,
    pub context_id: Option<String>,
    pub chat_key: Option<String>,
    pub grant_scope: String,
    pub grant_holder: JValue,
    pub recent_writes: Vec<String>,
    pub title: Title,
    pub message: String,
    pub space_id: Option<String>,
    pub project_id: Option<String>,
    pub decline_ends_reply: bool,
    pub record: bool,
    pub stream_idle_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable(pub String);

fn need<'a>(v: &'a JValue, key: &str) -> Result<&'a JValue, Unreadable> {
    match v.get(key) {
        JValue::Undefined => Err(Unreadable(format!("prepared.{key} is missing"))),
        found => Ok(found),
    }
}

fn text(v: &JValue, key: &str) -> Result<String, Unreadable> {
    need(v, key)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Unreadable(format!("prepared.{key} is not text")))
}

fn opt_text(v: &JValue, key: &str) -> Option<String> {
    v.get(key).as_str().map(str::to_string)
}

fn list(v: &JValue, key: &str) -> Result<Vec<JValue>, Unreadable> {
    match need(v, key)? {
        JValue::Arr(items) => Ok(items.clone()),
        _ => Err(Unreadable(format!("prepared.{key} is not a list"))),
    }
}

fn names(v: &JValue, key: &str) -> Result<Vec<String>, Unreadable> {
    list(v, key)?
        .iter()
        .map(|n| {
            n.as_str()
                .map(str::to_string)
                .ok_or_else(|| Unreadable(format!("prepared.{key} holds a non-name")))
        })
        .collect()
}

fn flag(v: &JValue, key: &str) -> bool {
    v.get(key) == &JValue::Bool(true)
}

impl Prepared {
    /// Reads a `prepared` frame (the parsed `data:` object).
    pub fn read(frame: &JValue) -> Result<Prepared, Unreadable> {
        if frame.get("type").as_str() != Some("prepared")
            || frame.get("v").as_num() != Some(VERSION)
        {
            return Err(Unreadable("not a prepared frame of version 1".into()));
        }
        let provider = need(frame, "provider")?;
        let chat = need(frame, "chat")?;
        let turn = need(frame, "turn")?;
        let title = need(chat, "title")?;
        let routes = list(frame, "routes")?
            .iter()
            .map(|r| match r {
                JValue::Arr(pair) => match (
                    pair.first().and_then(JValue::as_str),
                    pair.get(1).and_then(JValue::as_str),
                    pair.len(),
                ) {
                    (Some(a), Some(b), 2) => Ok((a.to_string(), b.to_string())),
                    _ => Err(Unreadable("prepared.routes holds a bad pair".into())),
                },
                _ => Err(Unreadable("prepared.routes holds a bad pair".into())),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Prepared {
            started_at: need(frame, "startedAt")?
                .as_num()
                .ok_or_else(|| Unreadable("prepared.startedAt is not a number".into()))?,
            model: text(frame, "model")?,
            role: opt_text(frame, "role"),
            auto: flag(frame, "auto"),
            effort: text(frame, "effort")?,
            thinking_off: flag(frame, "thinkingOff"),
            sampling: match frame.get("sampling") {
                o @ JValue::Obj(_) => o.clone(),
                _ => JValue::Obj(Vec::new()),
            },
            provider: Provider {
                id: text(provider, "id")?,
                local_engine: flag(provider, "localEngine"),
                capabilities: match provider.get("capabilities") {
                    JValue::Null => JValue::Undefined,
                    c => c.clone(),
                },
            },
            provider_label: text(provider, "label")?,
            upstream_url: text(frame, "upstreamUrl")?,
            limit_scope: text(frame, "limitScope")?,
            messages: list(frame, "messages")?,
            tools: list(frame, "tools")?,
            routes,
            writes: names(frame, "writes")?,
            public: names(frame, "public")?,
            automatic_only_writes: names(frame, "automaticOnlyWrites")?,
            route_note: opt_text(frame, "routeNote").unwrap_or_default(),
            template_gate: flag(frame, "templateGate"),
            chat_id: opt_text(chat, "id"),
            context_id: opt_text(chat, "contextId"),
            chat_key: opt_text(chat, "key"),
            grant_scope: text(chat, "grantScope")?,
            grant_holder: need(chat, "grantHolder")?.clone(),
            recent_writes: names(chat, "recentWrites")?,
            title: Title {
                chat_id: text(title, "chatId")?,
                list_id: opt_text(title, "listId"),
                first_message: text(title, "firstMessage")?,
                first_reply: flag(title, "firstReply"),
                enabled: flag(title, "enabled"),
            },
            message: text(turn, "message")?,
            space_id: opt_text(turn, "spaceId"),
            project_id: opt_text(turn, "projectId"),
            decline_ends_reply: flag(turn, "declineEndsReply"),
            record: flag(turn, "record"),
            stream_idle_ms: need(turn, "streamIdleMs")?.as_num().unwrap_or(0.0),
        })
    }
}
