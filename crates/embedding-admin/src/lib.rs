//! noevia-core's `server/routes/embedding.cjs` (#1636, #1668, #1686) in Rust: the parts of
//! `POST /api/embedding` that decide an answer without touching state.
//!
//! - [`read_request`]: `handle()` and `apply()` up to the first state read. Either the request the
//!   Node code would act on, or the exact 400/409 refusal it would send first.
//! - [`served_probes`]: which embeddings servers the Node code asks `/v1/models` of before it
//!   changes anything (`set-next`). The reachability check itself (`checkEmbeddingAddress`) is
//!   network-bound and stays with the caller, which must run it on each non-empty address first.
//! - [`aggregate_status`]: the status code of an answer whose projects and Diary halves each
//!   succeeded or failed (`apply()`, the last lines).
//!
//! Nothing here reads the migration, the Diary or the embedding service, and nothing calls it yet.
//! The Node code's JavaScript truthiness and `String()` coercions are reproduced where a request
//! body can reach them ([`js_truthy`], [`js_string`]). A non-scalar address is kept as the string
//! JS would build (`[object Object]`), so it is refused downstream, never read as "the engine".

#![forbid(unsafe_code)]

use serde_json::Value;

/// The actions `POST /api/embedding` accepts (`ACTIONS` in embedding.cjs).
pub const ACTIONS: [&str; 7] = [
    "set-next",
    "clear-next",
    "switch",
    "rollback",
    "finish",
    "service-switch",
    "service-off",
];

/// The scopes a non-service action accepts (`SCOPES` in embedding.cjs).
pub const SCOPES: [&str; 3] = ["all", "projects", "diary"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SetNext,
    ClearNext,
    Switch,
    Rollback,
    Finish,
    ServiceSwitch,
    ServiceOff,
}

impl Action {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "set-next" => Some(Self::SetNext),
            "clear-next" => Some(Self::ClearNext),
            "switch" => Some(Self::Switch),
            "rollback" => Some(Self::Rollback),
            "finish" => Some(Self::Finish),
            "service-switch" => Some(Self::ServiceSwitch),
            "service-off" => Some(Self::ServiceOff),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SetNext => "set-next",
            Self::ClearNext => "clear-next",
            Self::Switch => "switch",
            Self::Rollback => "rollback",
            Self::Finish => "finish",
            Self::ServiceSwitch => "service-switch",
            Self::ServiceOff => "service-off",
        }
    }

    /// The service actions skip the scope check and the target (`SERVICE_ACTIONS`).
    pub fn is_service(self) -> bool {
        matches!(self, Self::ServiceSwitch | Self::ServiceOff)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    Projects,
    Diary,
}

impl Scope {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "all" => Some(Self::All),
            "projects" => Some(Self::Projects),
            "diary" => Some(Self::Diary),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Projects => "projects",
            Self::Diary => "diary",
        }
    }

    /// Whether the projects half runs (`scope !== 'diary'`).
    pub fn includes_projects(self) -> bool {
        !matches!(self, Self::Diary)
    }

    /// Whether the Diary half runs (`scope !== 'projects'`).
    pub fn includes_diary(self) -> bool {
        !matches!(self, Self::Projects)
    }
}

/// A refusal or an answer's status and message, as the Node code sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub error: String,
}

impl Reply {
    fn new(status: u16, error: impl Into<String>) -> Self {
        Self {
            status,
            error: error.into(),
        }
    }
}

/// The `target` of a `set-next` request, after the coercions the Node code applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `String(target.model || '').trim()`; empty when absent.
    pub model: String,
    /// The projects' address: `target.baseUrl` trimmed; `None` (the inference engine) when empty.
    pub base_url: Option<String>,
    /// The Diary's address: `target.diaryBaseUrl ?? target.baseUrl`, trimmed; `None` when empty.
    pub diary_base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub action: Action,
    /// Always [`Scope::All`] for the service actions, whose scope is never read.
    pub scope: Scope,
    /// `body.force === true`.
    pub force: bool,
    /// Present only for `set-next`.
    pub target: Option<Target>,
}

/// JavaScript truthiness of an optional JSON value: `undefined`, `null`, `false`, `0`, `NaN` and
/// `""` are falsy; everything else (including `[]` and `{}`) is truthy.
pub fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// JavaScript `String(value ?? '')` for a JSON value, for the scalar and container shapes a JSON
/// request body can hold.
pub fn js_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::Bool(flag)) => flag.to_string(),
        // JavaScript reads every JSON number as a double and prints it with Number::toString.
        Some(Value::Number(number)) => number
            .as_f64()
            .map_or_else(|| number.to_string(), js_number_string),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| js_string(Some(item)))
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

/// ECMAScript `Number::toString(10)` of a finite double: the shortest round-trip digits, plain
/// for decimal exponents from -7 to 20 and in exponent form (`1e+21`, `1e-7`) outside them.
fn js_number_string(n: f64) -> String {
    if n == 0.0 {
        return "0".to_string(); // also -0
    }
    if !n.is_finite() {
        return if n.is_nan() {
            "NaN"
        } else if n > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }
        .to_string();
    }
    let sign = if n < 0.0 { "-" } else { "" };
    // `{:e}` prints the shortest round-trip digits as `d[.ddd]e<exp>`.
    let sci = format!("{:e}", n.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let k = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    let point = exp.parse::<i64>().unwrap_or(0).saturating_add(1); // `n` in the spec
    let body = if k <= point && point <= 21 {
        let zeros = usize::try_from(point - k).unwrap_or(0);
        format!("{digits}{}", "0".repeat(zeros))
    } else if 0 < point && point <= 21 {
        let (int, frac) = digits.split_at(usize::try_from(point).unwrap_or(0));
        format!("{int}.{frac}")
    } else if -6 < point && point <= 0 {
        let zeros = usize::try_from(-point).unwrap_or(0);
        format!("0.{}{digits}", "0".repeat(zeros))
    } else {
        let (first, rest) = digits.split_at(1);
        let e = point - 1;
        let e_sign = if e < 0 { '-' } else { '+' };
        let dot = if rest.is_empty() { "" } else { "." };
        format!("{first}{dot}{rest}e{e_sign}{}", e.unsigned_abs())
    };
    format!("{sign}{body}")
}

/// JavaScript `String.prototype.trim`: strips the ECMAScript WhiteSpace and LineTerminator code
/// points. Unlike `str::trim` it strips U+FEFF and keeps U+0085.
fn js_trim(text: &str) -> &str {
    const fn js_space(c: char) -> bool {
        matches!(
            c,
            '\u{9}'
                | '\u{a}'
                | '\u{b}'
                | '\u{c}'
                | '\u{d}'
                | ' '
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    }
    text.trim_matches(js_space)
}

/// `addr(v)` in apply(): the trimmed text, or `None` when it is empty.
fn address(text: &str) -> Option<String> {
    let trimmed = js_trim(text);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Reads the body of `POST /api/embedding` as `handle()` and `apply()` do, up to the first state
/// read. `body` is the parsed JSON (an unparsable body is the caller's 400 `invalid JSON`).
pub fn read_request(body: &Value) -> Result<Request, Reply> {
    let Value::Object(fields) = body else {
        return Err(Reply::new(400, "A JSON object is required"));
    };
    let action_text = if js_truthy(fields.get("action")) {
        js_string(fields.get("action"))
    } else {
        String::new()
    };
    let Some(action) = Action::parse(&action_text) else {
        return Err(Reply::new(
            400,
            format!("action must be one of {}", ACTIONS.join(", ")),
        ));
    };
    let force = fields.get("force") == Some(&Value::Bool(true));
    if action.is_service() {
        return Ok(Request {
            action,
            scope: Scope::All,
            force,
            target: None,
        });
    }
    let scope = if js_truthy(fields.get("scope")) {
        Scope::parse(&js_string(fields.get("scope")))
            .ok_or_else(|| Reply::new(400, "scope must be all, projects or diary"))?
    } else {
        Scope::All
    };
    let target = match action {
        Action::SetNext => Some(read_target(fields.get("target"))?),
        _ => None,
    };
    Ok(Request {
        action,
        scope,
        force,
        target,
    })
}

fn read_target(value: Option<&Value>) -> Result<Target, Reply> {
    // `!body.target || typeof body.target !== 'object'`: an array is an object to JavaScript.
    let target = match value {
        Some(target @ (Value::Object(_) | Value::Array(_))) => target,
        _ => {
            return Err(Reply::new(
                400,
                "set-next needs a target { baseUrl, model, profile }",
            ))
        }
    };
    let base_url = target.get("baseUrl");
    // `??` falls back only on null and undefined, not on an empty string.
    let diary_source = match target.get("diaryBaseUrl") {
        None | Some(Value::Null) => base_url,
        present => present,
    };
    let model = if js_truthy(target.get("model")) {
        js_string(target.get("model"))
    } else {
        String::new()
    };
    Ok(Target {
        model: js_trim(&model).to_string(),
        base_url: address(&js_string(base_url)),
        diary_base_url: address(&js_string(diary_source)),
    })
}

/// The embeddings servers the Node code asks `/v1/models` of before it changes anything
/// (`set-next` only). `None` is the inference engine. Each address appears once, projects first.
///
/// Call after the reachability check has passed for every non-empty address in the request.
pub fn served_probes(request: &Request) -> Result<Vec<Option<String>>, Reply> {
    let Some(target) = &request.target else {
        return Ok(Vec::new());
    };
    let mut probes: Vec<Option<String>> = Vec::new();
    if request.scope.includes_projects() {
        probes.push(target.base_url.clone());
    }
    if request.scope.includes_diary() {
        // With no address the Diary resolves its own default server, which this check cannot
        // see: fail closed and ask the administrator to name it.
        let Some(diary) = &target.diary_base_url else {
            return Err(Reply::new(
                409,
                "Name the Diary's embeddings server so noevia can check it serves this model.",
            ));
        };
        if !probes
            .iter()
            .any(|probe| probe.as_deref() == Some(diary.as_str()))
        {
            probes.push(Some(diary.clone()));
        }
    }
    Ok(probes)
}

/// The status of an answer from its two halves (`apply()`): 200 when none failed, the failed
/// half's own status (400 to 599) when every half that ran failed, and 207 when only some did.
/// `None` is a half that did not run. A half fails when it carries a truthy `error`.
pub fn aggregate_status(projects: Option<&Value>, diary: Option<&Value>) -> u16 {
    let ran: Vec<&Value> = [projects, diary].into_iter().flatten().collect();
    let failed: Vec<&Value> = ran
        .iter()
        .copied()
        .filter(|half| js_truthy(half.get("error")))
        .collect();
    if failed.is_empty() {
        return 200;
    }
    if failed.len() != ran.len() {
        return 207;
    }
    failed
        .first()
        .and_then(|half| half.get("status"))
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .filter(|status| (400..600).contains(status))
        .unwrap_or(502)
}
