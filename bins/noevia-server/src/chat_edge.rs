//! The chat edge (chat port S6, capability `rust-chat-edge`): what core routes/chat.cjs answered
//! before a turn reached its loop, now answered by the front, and the `llm:` budget that turn shares
//! with the Diary local exchange (core routes/diary.cjs). Core removes its limiter in the paired
//! release (rust-chat-edge.cjs) and keeps the rest of its checks, which a request passed here always
//! passes there too.
//!
//! In Node's order, for a POST to exactly `/api/chat` that its request gate let through
//! (chat_turn.rs answers the gate first):
//! 1. The `llm:<user>` budget, before the body is read: core index.cjs `llmRateLimiter`, a fixed
//!    window of 60 s, `LLM_RATE_LIMIT` calls (default 60). Over it: 429 `Too many requests — the
//!    model endpoint is shared; wait a moment and try again`.
//! 2. The body, at most `STORED_HISTORY_BYTES`: over it 413 `This chat is too large to continue;
//!    start a new chat.`, a failed read 400 `could not read the request`.
//! 3. `JSON.parse` of the body as `Buffer#toString('utf8')` decodes it (js_json `raw`: V8's
//!    acceptance, no depth limit, any number, lone surrogates kept): 400 `invalid JSON`, then 400
//!    `request body must be a JSON object`.
//! 4. chat-mode.cjs `requestShapeError`: `mode`, a pinned skill with Cowork
//!    (`skill_pin_unsupported_mode`), `turnToolboxes` (≤ 20) and `turnImageIds` (≤ 12) as lists of
//!    non-empty ids of at most 80 UTF-16 units.
//! 5. Cowork: 403 for an account that is not an administrator, then 409 with the code harness off,
//!    then 409 outside a project, each `{error, mode:'cowork'}`. A start that passes goes to Core's
//!    `/internal/chat/cowork`, which answers its 202 `{mode:'cowork', task}` (or the code service's
//!    refusal) through the unchanged pipeline.
//! 6. `spaceId: 'diary'` with the add-on off: 404 `Diary add-on is disabled`.
//!
//! Node's inference gate answered before all of these, and a request it refused was never counted.
//! So every refusal here is first offered to Core's gate (chat_turn.rs `gate_refusal`), whose 503
//! wins; and a call this front counted is given back when Core's gate refused it (Core marks that
//! answer with [`GATE_REFUSED_HEADER`], which never reaches the client).
//!
//! The Diary local exchange (`POST /api/diary/local-exchange`) stays Node's: the front counts its call
//! on the same budget after Node's gate, add-on and method checks would have passed (signed in, the
//! add-on on, a POST), and answers its 429 `Please wait before sending another message` itself.
use crate::{identity, reply, serve::Conn, App};
use axum::{
    body::Body,
    http::{HeaderMap, Method, Request, Response, StatusCode},
};
use js_json::raw;
use std::sync::Arc;

/// core index.cjs `LLM_RATE_WINDOW_MS`.
pub const LLM_WINDOW_MS: i64 = 60_000;
/// routes/chat.cjs's 429.
pub const CHAT_TOO_MANY: &str =
    "Too many requests — the model endpoint is shared; wait a moment and try again";
/// routes/diary.cjs's 429 for the local exchange.
pub const DIARY_TOO_MANY: &str = "Please wait before sending another message";
/// The Diary inference path that shares the budget (Node's route).
pub const LOCAL_EXCHANGE: &str = "/api/diary/local-exchange";
/// Core rust-chat-edge.cjs `COWORK_PATH`: a Cowork start that passed the checks.
pub const COWORK_PATH: &str = "/internal/chat/cowork";
/// Core rust-chat-edge.cjs `GATE_REFUSED_HEADER`: Core's inference gate refused this call.
pub const GATE_REFUSED_HEADER: &str = "x-noevia-gate-refused";
/// Core rust-chat-edge.cjs `DIARY_GATE_PATH`: the gate probe as the local exchange meets it.
pub const DIARY_GATE_PATH: &str = "/internal/chat/gate?route=diary-local-exchange";
/// chat-mode.cjs `TURN_BOX_CAP` and `TURN_IMAGE_CAP`, and the id length both lists allow.
const TURN_BOX_CAP: usize = 20;
const TURN_IMAGE_CAP: usize = 12;
const ID_UNITS: usize = 80;

/// What the edge reads from the environment, as core reads it once at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// `LLM_RATE_LIMIT`, as a bound on the count (`count > limit` is refused).
    pub llm_limit: u64,
    /// `NOEVIA_FEATURE_CODE_HARNESS` (features.cjs `parseEnv`; it overrides the admin setting).
    pub code_harness_env: Option<bool>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            llm_limit: 60,
            code_harness_env: None,
        }
    }
}

impl Settings {
    /// An invalid `NOEVIA_FEATURE_CODE_HARNESS` refuses startup, as Node does not start with one.
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Settings, String> {
        let spec = server_features::spec(server_features::Feature::CodeHarness);
        Ok(Settings {
            llm_limit: llm_limit(get("LLM_RATE_LIMIT").as_deref()),
            code_harness_env: server_features::parse_env(get(spec.env).as_deref())
                .map_err(str::to_owned)?,
        })
    }
}

/// JavaScript's `Number(s)` for a string.
fn js_number(s: &str) -> f64 {
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = |prefix: [&str; 2], base: u32| {
        prefix.iter().find_map(|p| t.strip_prefix(p)).map(|digits| {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
                f64::NAN
            } else {
                digits
                    .chars()
                    .filter_map(|c| c.to_digit(base))
                    .fold(0.0, |n, d| n * f64::from(base) + f64::from(d))
            }
        })
    };
    if let Some(n) = radix(["0x", "0X"], 16)
        .or_else(|| radix(["0o", "0O"], 8))
        .or_else(|| radix(["0b", "0B"], 2))
    {
        return n;
    }
    let decimal = t
        .bytes()
        .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'));
    if !decimal {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `Math.max(1, Number(process.env.LLM_RATE_LIMIT || 60))` as a bound on an integer count: the
/// largest count that is not refused. `NaN` and `Infinity` refuse nothing.
pub fn llm_limit(raw: Option<&str>) -> u64 {
    let n = match raw {
        None | Some("") => 60.0,
        Some(s) => js_number(s),
    };
    if n.is_nan() {
        return u64::MAX;
    }
    let n = n.max(1.0).floor();
    if n.is_infinite() {
        return u64::MAX;
    }
    if n >= 18_446_744_073_709_551_615.0 {
        u64::MAX
    } else {
        // In range and integral: the cast is exact.
        n as u64
    }
}

/// The bucket key core used (`llm:${userId}`).
pub fn llm_key(user: &str) -> String {
    format!("llm:{user}")
}

/// One call counted on the shared budget: what to give back if Core's gate refuses it.
#[derive(Debug, Clone)]
pub struct Counted {
    key: String,
    window: i64,
    pub limited: bool,
}

/// Counts one call for `user` on the shared `llm:` budget (core `llmRateLimited`).
pub fn charge(app: &App, user: &str) -> Option<Counted> {
    let key = llm_key(user);
    let limit = app.config.chat_edge.llm_limit;
    let mut rate = app.llm_rate.lock().ok()?;
    let c = rate.charge(&key, limit, LLM_WINDOW_MS, identity::now_ms());
    Some(Counted {
        key,
        window: c.window,
        limited: c.limited,
    })
}

/// Gives back a call Core's gate refused: Node never counted it.
pub fn release(app: &App, counted: &Counted) {
    if let Ok(mut rate) = app.llm_rate.lock() {
        rate.release(&counted.key, counted.window, identity::now_ms());
    }
}

/// Whether Core's gate refused this answer; the mark is removed.
pub fn take_gate_refused(headers: &mut HeaderMap) -> bool {
    headers.remove(GATE_REFUSED_HEADER).is_some()
}

/// The account facts the checks read.
#[derive(Debug, Clone, Copy)]
pub struct Account {
    /// `authn.user.role === 'admin'` (a device token acts as a member).
    pub admin: bool,
    /// `authService.diaryEnabled(user)`.
    pub diary: bool,
}

/// What the edge makes of a body that was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// An ordinary turn: relay it to Core's loop.
    Turn,
    /// A Cowork start that passed the refusals: relay it to [`COWORK_PATH`].
    Cowork,
    /// Node's answer: the status and its JSON text.
    Refuse(StatusCode, String),
}

fn refuse(status: StatusCode, error: &str) -> Verdict {
    Verdict::Refuse(status, format!("{{\"error\":{}}}", raw::quote(error)))
}

fn refuse_with(status: StatusCode, error: &str, key: &str, value: &str) -> Verdict {
    Verdict::Refuse(
        status,
        format!(
            "{{\"error\":{},\"{key}\":{}}}",
            raw::quote(error),
            raw::quote(value)
        ),
    )
}

/// The 413 and 400 of a body that could not be read whole.
pub fn unread(over: bool) -> Verdict {
    if over {
        refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            "This chat is too large to continue; start a new chat.",
        )
    } else {
        refuse(StatusCode::BAD_REQUEST, "could not read the request")
    }
}

/// A list `requestShapeError` accepts: at most `cap` ids, each 1..=80 UTF-16 units.
fn id_list(written: &str, cap: usize) -> bool {
    raw::string_lengths(written)
        .is_some_and(|lens| lens.len() <= cap && lens.iter().all(|&n| n != 0 && n <= ID_UNITS))
}

/// routes/chat.cjs from `JSON.parse` to the Diary gate, for a body read whole. `harness` is
/// `features.enabled('codeHarness')`, read only for a Cowork request; `None` (unreadable) leaves the
/// decision to Core, whose own checks then answer.
pub fn check(bytes: &[u8], account: Account, harness: impl FnOnce() -> Option<bool>) -> Verdict {
    let fields = match raw::read(&String::from_utf8_lossy(bytes)) {
        None => return refuse(StatusCode::BAD_REQUEST, "invalid JSON"),
        Some(raw::Top::Other(..)) => {
            return refuse(
                StatusCode::BAD_REQUEST,
                "request body must be a JSON object",
            )
        }
        Some(raw::Top::Object(fields)) => fields,
    };
    let field = |name: &str| {
        fields
            .iter()
            .find(|f| raw::is_key(&f.key, name))
            .map(|f| f.value.as_str())
    };
    let mode = field("mode");
    if mode.is_some_and(|m| m != "\"chat\"" && m != "\"cowork\"") {
        return refuse(
            StatusCode::BAD_REQUEST,
            "mode must be \"chat\" or \"cowork\"",
        );
    }
    let cowork = mode == Some("\"cowork\"");
    if cowork && field("skill").is_some_and(|s| s != "null") {
        return refuse_with(
            StatusCode::BAD_REQUEST,
            "A pinned Skill applies to chat turns only; Cowork tasks do not accept one yet.",
            "code",
            "skill_pin_unsupported_mode",
        );
    }
    if field("turnToolboxes").is_some_and(|v| !id_list(v, TURN_BOX_CAP)) {
        return refuse(
            StatusCode::BAD_REQUEST,
            "turnToolboxes must be a list of at most 20 toolbox ids",
        );
    }
    if field("turnImageIds").is_some_and(|v| !id_list(v, TURN_IMAGE_CAP)) {
        return refuse(
            StatusCode::BAD_REQUEST,
            "turnImageIds must be a list of at most 12 image source ids",
        );
    }
    if cowork {
        if !account.admin {
            return refuse_with(
                StatusCode::FORBIDDEN,
                "Cowork runs the coding harness, which is limited to administrators.",
                "mode",
                "cowork",
            );
        }
        match harness() {
            None => return Verdict::Cowork,
            Some(false) => {
                return refuse_with(
                    StatusCode::CONFLICT,
                    "Cowork needs the coding harness, which is off on this server.",
                    "mode",
                    "cowork",
                )
            }
            Some(true) => {}
        }
        // `typeof projectId !== 'string' || !projectId`.
        if !field("projectId").is_some_and(|p| p.starts_with('"') && p != "\"\"") {
            return refuse_with(
                StatusCode::CONFLICT,
                "Cowork runs inside a project; open a project chat first.",
                "mode",
                "cowork",
            );
        }
        return Verdict::Cowork;
    }
    if field("spaceId") == Some("\"diary\"") && !account.diary {
        return refuse(StatusCode::NOT_FOUND, "Diary add-on is disabled");
    }
    Verdict::Turn
}

/// `features.enabled('codeHarness')` now (env, else the admin setting, else off); `None` when the
/// setting cannot be read.
pub fn harness_blocking(app: &App) -> Option<bool> {
    let env = app.config.chat_edge.code_harness_env;
    let feature = server_features::Feature::CodeHarness;
    let stored = if env.is_some() {
        None
    } else {
        let spec = server_features::spec(feature);
        app.writes
            .writer()
            .ok()?
            .read(|r| {
                r.row(
                    "SELECT CASE WHEN typeof(value)='text' AND value IN ('true','false') THEN value ELSE NULL END FROM settings WHERE key=?1",
                    [spec.setting],
                    |row| row.get::<_, Option<String>>(0),
                )
            })
            .ok()?
            .flatten()
    };
    Some(server_features::evaluate(feature, env, stored.as_deref(), true).enabled)
}

/// A Node-owned request: the local exchange counts its call on the shared budget, anything else is
/// forwarded as before. `path` is the WHATWG pathname.
pub async fn forward_node(
    app: &Arc<App>,
    conn: Conn,
    req: Request<Body>,
    path: Option<&str>,
) -> Response<Body> {
    if path != Some(LOCAL_EXCHANGE) || app.proxy.is_in_process() {
        return app.proxy.forward(conn.peer, req).await;
    }
    let counted = if req.method() == Method::POST {
        match local_exchange_turn(app, conn, req.headers().clone()).await {
            Ok(counted) => counted,
            Err(answer) => return *answer,
        }
    } else {
        None
    };
    let mut res = app.proxy.forward(conn.peer, req).await;
    if take_gate_refused(res.headers_mut()) {
        if let Some(c) = &counted {
            release(app, c);
        }
    }
    res
}

/// The local exchange's turn on the budget: `Ok(None)` when Node answers before the budget (signed
/// out, CSRF, the add-on off), `Ok(Some)` when the call was counted and goes on, `Err` with the 429
/// (or the gate's 503, which wins) when it is over.
async fn local_exchange_turn(
    app: &Arc<App>,
    conn: Conn,
    headers: HeaderMap,
) -> Result<Option<Counted>, Box<Response<Body>>> {
    let creds = server_auth::Creds::from_headers(
        "POST",
        headers.iter().map(|(k, v)| (k.as_str(), v.as_bytes())),
    );
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gated =
        tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, LOCAL_EXCHANGE, now)).await;
    let Ok(Ok(authn)) = gated else {
        return Ok(None);
    };
    if !authn.user.diary_enabled {
        return Ok(None);
    }
    let Some(counted) = charge(app, &authn.user.id) else {
        return Ok(None);
    };
    if !counted.limited {
        return Ok(Some(counted));
    }
    if let Some(mut held) = crate::chat_turn::gate_probe(app, conn, &headers, DIARY_GATE_PATH).await
    {
        take_gate_refused(held.headers_mut());
        release(app, &counted);
        return Err(Box::new(held));
    }
    Err(Box::new(reply::error(
        StatusCode::TOO_MANY_REQUESTS,
        DIARY_TOO_MANY,
        true,
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn llm_rate_limit_reads_as_number_and_math_max_read_it() {
        for (raw, limit) in [
            (None, 60),
            (Some(""), 60),
            (Some("2"), 2),
            (Some(" 5 "), 5),
            (Some("0"), 1),
            (Some("-3"), 1),
            (Some("1.5"), 1),
            (Some("2.9"), 2),
            (Some("0x10"), 16),
            (Some("1e2"), 100),
            (Some(".5"), 1),
            (Some("abc"), u64::MAX),
            (Some("5x"), u64::MAX),
            (Some("Infinity"), u64::MAX),
            (Some("-Infinity"), 1),
            (Some("   "), 1),
            (Some("0x"), u64::MAX),
        ] {
            assert_eq!(llm_limit(raw), limit, "{raw:?}");
        }
    }
}
