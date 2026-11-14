//! The `prepared` frame noevia-core sends when it hands a turn to the native executor
//! (`server/rust-chat-native.cjs` `preparedTurn`, v1): everything the turn needs after its
//! preparation. Read strictly: a frame of another version, or one missing a field the turn uses, is
//! refused, and the executor then has nothing to run (the front never asks for a hand-off it cannot
//! read: the version is checked before the front's switch sends a turn natively).

use js_json::JValue;

use crate::effort::Provider;
use crate::host::Skill;

pub const VERSION: f64 = 1.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Title {
    pub chat_id: String,
    pub list_id: Option<String>,
    pub first_message: String,
    pub first_reply: bool,
    pub enabled: bool,
}

/// A project turn's instruction skills (#272, #546) as core settled them before the cut.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Skills {
    /// The skills the prompt carries (pinned or picked automatically), in chat.cjs `loadedSkills` order.
    pub loaded: Vec<Skill>,
    /// The turn's snapshot's enabled skills, in the project's list order: what a tool call may still load.
    pub enabled: Vec<Skill>,
    /// The skill names already taken out of the history (chat.cjs `revokedInHistory`).
    pub revoked_in_history: Vec<String>,
    /// The reply warning's other parts (chat.cjs `replyWarning`): the image warning and the coverage warning.
    pub image_warning: String,
    pub coverage_warning: String,
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
    /// The offered project file edit tools (chat.cjs `projectEditTool`; absent from an older core: none,
    /// as an older core never hands off a turn that offers one).
    pub edit_tools: Vec<String>,
    /// #2446: the offered tools asked about on every call and never under a chat-wide grant (the coworker page
    /// save; `onceTools`, present only when there are any; a core that offers one hands the turn off only to a
    /// front naming the `once` capability).
    pub once_tools: Vec<String>,
    /// #769: the provenance policy is on for this turn (`provenance: true`, present only then; a core that has it
    /// on hands the turn off only to a front naming the `provenance` capability).
    pub provenance: bool,
    /// Roadmap E: what the routed list's `more_tools` widens to, on a turn whose tools routing narrowed (`widen`,
    /// present only then; a core that narrowed hands the turn off only to a front naming `more-tools`).
    pub widen: Option<Widen>,
    pub route_note: String,
    pub template_gate: bool,
    /// The local model manager answers health and load; else the runtime limit is the fallback.
    pub manager_enabled: bool,
    /// The local model manager grants the model's lease.
    pub model_lease: bool,
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
    /// The turn only compacts the chat's context (the Compact action): the fit is forced, then `done`.
    pub compact_only: bool,
    /// #1793: the reply's sources and the injected files the close may add to them (absent: none).
    pub sources: crate::sources::Sources,
    /// The project's instruction skills; `None` without a project (and from an older core, which hands off no
    /// turn whose project has skills).
    pub skills: Option<Skills>,
}

/// The project's whole tool selection, as chat.cjs resolves it when the model calls `more_tools`.
#[derive(Debug, Clone, PartialEq)]
pub struct Widen {
    pub tools: Vec<JValue>,
    pub routes: Vec<(String, String)>,
    pub writes: Vec<String>,
    pub public: Vec<String>,
    pub edit_tools: Vec<String>,
    pub once_tools: Vec<String>,
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

/// Text or absent (`null`/missing); anything else refuses the frame. A chat id Node would carry as a
/// number makes its permission checks throw, which the native gate cannot reproduce.
fn id_text(v: &JValue, key: &str) -> Result<Option<String>, Unreadable> {
    match v.get(key) {
        JValue::Undefined | JValue::Null => Ok(None),
        JValue::Str(s) => Ok(Some(s.clone())),
        _ => Err(Unreadable(format!("prepared.{key} is not text"))),
    }
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

fn skill_list(v: &JValue, key: &str) -> Result<Vec<Skill>, Unreadable> {
    list(v, key)?
        .iter()
        .map(|k| match (k.get("file"), k.get("hash"), k.get("name")) {
            (JValue::Str(file), JValue::Str(hash), JValue::Str(name)) => Ok(Skill {
                file: file.clone(),
                hash: hash.clone(),
                name: name.clone(),
            }),
            _ => Err(Unreadable(format!(
                "prepared.skills.{key} holds a bad skill"
            ))),
        })
        .collect()
}

fn skills(v: &JValue) -> Result<Option<Skills>, Unreadable> {
    match v {
        JValue::Undefined | JValue::Null => Ok(None),
        JValue::Obj(_) => Ok(Some(Skills {
            loaded: skill_list(v, "loaded")?,
            enabled: skill_list(v, "enabled")?,
            revoked_in_history: names(v, "revokedInHistory")?,
            image_warning: opt_text(v, "imageWarning").unwrap_or_default(),
            coverage_warning: opt_text(v, "coverageWarning").unwrap_or_default(),
        })),
        _ => Err(Unreadable("prepared.skills is not an object".into())),
    }
}

fn flag(v: &JValue, key: &str) -> bool {
    v.get(key) == &JValue::Bool(true)
}

/// `[[tool, server]]`: the MCP routes of a tool list.
fn route_pairs(v: &JValue, key: &str) -> Result<Vec<(String, String)>, Unreadable> {
    list(v, key)?
        .iter()
        .map(|r| match r {
            JValue::Arr(pair) => match (
                pair.first().and_then(JValue::as_str),
                pair.get(1).and_then(JValue::as_str),
                pair.len(),
            ) {
                (Some(a), Some(b), 2) => Ok((a.to_string(), b.to_string())),
                _ => Err(Unreadable(format!("prepared.{key} holds a bad pair"))),
            },
            _ => Err(Unreadable(format!("prepared.{key} holds a bad pair"))),
        })
        .collect()
}

fn widen(v: &JValue) -> Result<Option<Widen>, Unreadable> {
    match v {
        JValue::Undefined => Ok(None),
        JValue::Obj(_) => Ok(Some(Widen {
            tools: list(v, "tools")?,
            routes: route_pairs(v, "routes")?,
            writes: names(v, "writes")?,
            public: names(v, "public")?,
            edit_tools: names(v, "editTools")?,
            once_tools: names(v, "onceTools")?,
        })),
        _ => Err(Unreadable("prepared.widen is not an object".into())),
    }
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
        let routes = route_pairs(frame, "routes")?;
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
            edit_tools: match frame.get("editTools") {
                JValue::Undefined => Vec::new(),
                _ => names(frame, "editTools")?,
            },
            once_tools: match frame.get("onceTools") {
                JValue::Undefined => Vec::new(),
                _ => names(frame, "onceTools")?,
            },
            route_note: opt_text(frame, "routeNote").unwrap_or_default(),
            // Present only as `true`; any other value refuses the frame (never read as off).
            provenance: match frame.get("provenance") {
                JValue::Undefined => false,
                JValue::Bool(true) => true,
                _ => return Err(Unreadable("prepared.provenance is not true".into())),
            },
            widen: widen(frame.get("widen"))?,
            template_gate: flag(frame, "templateGate"),
            manager_enabled: flag(frame, "managerEnabled"),
            model_lease: flag(frame, "modelLease"),
            chat_id: id_text(chat, "id")?,
            context_id: id_text(chat, "contextId")?,
            chat_key: id_text(chat, "key")?,
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
            compact_only: flag(turn, "compactOnly"),
            sources: match turn.get("sources") {
                JValue::Undefined | JValue::Null => crate::sources::Sources::default(),
                v => crate::sources::Sources::read(v)
                    .ok_or_else(|| Unreadable("prepared.turn.sources is not readable".into()))?,
            },
            skills: skills(frame.get("skills"))?,
        })
    }
}
