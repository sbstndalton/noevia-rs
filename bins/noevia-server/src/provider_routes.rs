//! The provider registry's HTTP surface (sbstndalton/noevia#2190 round 6, group B2), ported from
//! core routes/providers.cjs: `GET|POST /api/providers`, `PUT|DELETE /api/providers/{id}` and
//! `DELETE /api/providers/chatgpt`. Permanent, no switch; Node refuses these methods (core
//! rust-providers.cjs `RUST_PROVIDERS_OWNED`), so the web supervisor pairs this front's
//! `rust-providers` capability with that Core. The Rust front is the only writer of
//! `users/<id>/providers.json` and `shared-providers.json` (crates/server-projects
//! `provider_rows`); Node re-reads the private file by its change signal on every chat turn
//! (core workspace.cjs, shared-reread.cjs) and keeps no copy.
//!
//! Order and words follow core: the session gate (401/403/CSRF), then
//!
//! - `GET`: the merged view (the env-derived default row, the shared rows, the account's own),
//!   each as `listRow` shows it: the key masked to its last four characters (never the key), the
//!   default flags (`managed` is MODEL_MANAGER_KIND being `llamacpp` or `lemonade`), the context
//!   window and capabilities (a row without its own gets the built-in OpenAI preset for that
//!   address), `keyUnreadable`, `external` for a trial-terms host and, for the ChatGPT row, its
//!   `kind` and `connection` (the token broker's state). ChatGPT rows are listed only while the
//!   `chatgptOAuth` flag is on, and a token row without a provider row gets its row at the
//!   listing (it is also written when the sign-in is saved).
//! - `POST`: the body (1 MiB, `invalid JSON`, a JSON object), the shared rule (administrators),
//!   the label, the context window, the address (`http(s)://`, 2048 units, the member-origin rule),
//!   the capabilities; the row gets `prov-<ms>-<6 base36>` and its key sealed `enc:v1`.
//! - `PUT`: the default and its `lemonade` alias, an unknown row, a sign-in-managed or `external`
//!   row and a shared row for a member are refused before the body is read; then the fields as
//!   core checks them. A stored key never follows the row to another origin, and omitted keys keep
//!   their stored value.
//! - `DELETE`: the default is refused, a shared row needs an administrator, the ChatGPT row's
//!   tokens are deleted first (a failed delete leaves the row), then the row, then the account's
//!   projects that used it fall back to the default.
//!
//! Every change runs as a spawned task the handler awaits, so a client that leaves mid-request
//! cannot strand the multi-step ones (tokens, row, project fallbacks).
//!
//! Node bugs fixed on the way: a `%`-escape `decodeURIComponent` refuses in the id threw (a 500)
//! and is now `400 no such provider`; a provider save could not lose a concurrent writer's rows
//! because Node rewrote its whole cached list, which the locked read-modify-write here cannot.

use crate::provider_files::{self, CHATGPT_ID, CHATGPT_KIND};
use crate::provider_test_routes::{
    default_provider_id, endpoint_approved, http_prefixed, or_empty, origin_of, read_body,
    trimmed_address, BodyError,
};
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::{wtf16, JValue};
use rand_core::{OsRng, RngCore};
use server_auth::secrets::SecretKeys;
use server_projects::provider_rows;
use server_projects::store::{self, StoreError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroizing;

pub const LIST: &str = "/api/providers";

/// #1209: provider addresses are short; a long one only costs every listing and chat.
const MAX_BASE_URL: usize = 2048;
const NOT_APPROVED: &str =
    "Provider origin is not approved for member connections; contact an administrator.";
const OFF_MESSAGE: &str = "Sign in with ChatGPT is turned off on this server.";
/// core providers.cjs: a context window is a whole number of tokens within these bounds.
const CONTEXT_MIN: f64 = 2048.0;
const CONTEXT_MAX: f64 = 2_000_000.0;
const CONTEXT_MESSAGE: &str = "contextTokens must be a whole number from 2048 to 2000000";
/// Keys shorter than this show no characters at all when masked (#1482).
const MASK_MIN_LENGTH: usize = 12;

// ---------------------------------------------------------------------------------------------
// Answers.

fn answer(status: u16, body: &JValue) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        wtf16::stringify(body).unwrap_or_default(),
        true,
    )
}

fn error(status: u16, message: &str) -> Response<Body> {
    answer(
        status,
        &JValue::obj([("error", JValue::Str(message.into()))]),
    )
}

/// A store failure as the project store words it (core project-store-client.cjs `failure`).
fn store_failure(e: &StoreError) -> (u16, &'static str) {
    match e {
        StoreError::Busy => (503, "The project store is busy. Try again shortly."),
        StoreError::Gone => (
            503,
            "Your account is still being set up; try again in a moment.",
        ),
        StoreError::Corrupt => (500, "providers.json is damaged; nothing was changed."),
        _ => (500, "Internal error"),
    }
}

fn store_failed(e: &StoreError) -> Response<Body> {
    let (status, message) = store_failure(e);
    error(status, message)
}

// ---------------------------------------------------------------------------------------------
// JavaScript helpers.

fn js_trim(s: &str) -> &str {
    s3_sign::js_trim(s)
}

/// `String(v || '').trim().slice(0, max)`.
fn trimmed_slice(v: &JValue, max: usize) -> String {
    wtf16::slice(js_trim(&or_empty(v)), max)
}

fn put(items: &mut Vec<(String, JValue)>, key: &str, value: JValue) {
    wtf16::set(items, key, value);
}

fn has_string_id(v: &JValue, id: &str) -> bool {
    v.get("id").as_str() == Some(id)
}

/// `Number.isInteger(n)` within the context-window bounds.
fn context_ok(n: f64) -> bool {
    n.is_finite() && n.fract() == 0.0 && (CONTEXT_MIN..=CONTEXT_MAX).contains(&n)
}

/// core providers.cjs `parseContextTokens`: `Ok(None)` clears the window.
fn parse_context_tokens(value: &JValue) -> Result<Option<f64>, &'static str> {
    let number = match value {
        JValue::Undefined | JValue::Null => return Ok(None),
        JValue::Str(s) if s.is_empty() => return Ok(None),
        JValue::Str(s) => {
            let t = js_trim(s);
            if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
                t.parse::<f64>().map_err(|_| CONTEXT_MESSAGE)?
            } else {
                return Err(CONTEXT_MESSAGE);
            }
        }
        JValue::Num(n) => *n,
        _ => return Err(CONTEXT_MESSAGE),
    };
    if context_ok(number) {
        Ok(Some(number))
    } else {
        Err(CONTEXT_MESSAGE)
    }
}

/// core providers.cjs `validContextTokens`: a stored window still in bounds.
fn valid_context_tokens(value: &JValue) -> Option<f64> {
    value.as_num().filter(|n| context_ok(*n))
}

/// core providers.cjs `parseCapabilities`: the cleaned object, or the message.
fn parse_capabilities(input: &JValue) -> Result<JValue, String> {
    let JValue::Obj(items) = input else {
        return Err("capabilities must be an object".into());
    };
    const KEYS: [&str; 4] = [
        "reasoningEffortParam",
        "reasoningEffortModels",
        "tokenBudgetField",
        "jsonSchemaParam",
    ];
    for (key, _) in items {
        if !KEYS.contains(&key.as_str()) {
            return Err(format!("unknown capability: {key}"));
        }
    }
    let mut out: Vec<(String, JValue)> = Vec::new();
    match input.get("reasoningEffortParam") {
        JValue::Undefined => {}
        JValue::Bool(b) => out.push(("reasoningEffortParam".into(), JValue::Bool(*b))),
        _ => return Err("reasoningEffortParam must be true or false".into()),
    }
    match input.get("reasoningEffortModels") {
        JValue::Undefined => {}
        JValue::Arr(models) => {
            const MODELS: &str = "reasoningEffortModels must be a list of up to 50 model ids";
            if models.len() > 50 {
                return Err(MODELS.into());
            }
            let mut clean = Vec::new();
            for m in models {
                match m {
                    JValue::Str(s) if !js_trim(s).is_empty() && wtf16::len(s) <= 200 => {
                        clean.push(JValue::Str(js_trim(s).to_string()));
                    }
                    _ => return Err(MODELS.into()),
                }
            }
            out.push(("reasoningEffortModels".into(), JValue::Arr(clean)));
        }
        _ => return Err("reasoningEffortModels must be a list of up to 50 model ids".into()),
    }
    match input.get("tokenBudgetField") {
        JValue::Undefined => {}
        JValue::Str(s) if s == "max_tokens" || s == "max_completion_tokens" => {
            out.push(("tokenBudgetField".into(), JValue::Str(s.clone())));
        }
        _ => {
            return Err("tokenBudgetField must be one of max_tokens, max_completion_tokens".into())
        }
    }
    match input.get("jsonSchemaParam") {
        JValue::Undefined => {}
        JValue::Bool(b) => out.push(("jsonSchemaParam".into(), JValue::Bool(*b))),
        _ => return Err("jsonSchemaParam must be true or false".into()),
    }
    Ok(JValue::Obj(out))
}

/// core providers.cjs `legacyPresetCapabilities`: a row that predates capabilities and points at
/// the OpenAI preset's address keeps that preset's definition.
fn legacy_preset_capabilities(base_url: &JValue) -> JValue {
    let text = js_json::to_js_string(base_url).unwrap_or_default();
    let Ok(url) = url::Url::parse(&text) else {
        return JValue::Obj(Vec::new());
    };
    if url.origin().ascii_serialization() != "https://api.openai.com" {
        return JValue::Obj(Vec::new());
    }
    let mut out = vec![(
        "tokenBudgetField".to_string(),
        JValue::Str("max_completion_tokens".into()),
    )];
    let plain = url.query().is_none_or(str::is_empty)
        && url.fragment().is_none_or(str::is_empty)
        && url.username().is_empty()
        && url.password().is_none();
    if plain && matches!(url.path(), "" | "/" | "/v1" | "/v1/") {
        out.push(("reasoningEffortParam".into(), JValue::Bool(true)));
        out.push((
            "reasoningEffortModels".into(),
            JValue::Arr(vec![JValue::Str("gpt-5.4".into())]),
        ));
    }
    JValue::Obj(out)
}

/// core providers.cjs `effectiveCapabilities(row)`.
fn effective_capabilities(row: &JValue) -> JValue {
    match row.get("capabilities") {
        JValue::Undefined => legacy_preset_capabilities(row.get("baseUrl")),
        stored => parse_capabilities(stored).unwrap_or(JValue::Obj(Vec::new())),
    }
}

/// core providers.cjs `maskKey`: the last four characters and nothing else; a short key shows none.
fn mask_key(key: &str) -> JValue {
    if key.is_empty() || key == "local" {
        return JValue::Null;
    }
    let units: Vec<u16> = key.encode_utf16().collect();
    if units.len() >= MASK_MIN_LENGTH {
        let tail = units.get(units.len() - 4..).unwrap_or(&[]);
        JValue::Str(format!("…{}", String::from_utf16_lossy(tail)))
    } else {
        JValue::Str("••••".into())
    }
}

/// core provider-egress.cjs `isTrialTermsHost(provider)`: true unless the rules say it is not.
fn trial_terms_host(row: &JValue) -> bool {
    let text = js_json::to_js_string(row.get("baseUrl")).unwrap_or_default();
    // withoutUserinfo: the rules never need a name or password.
    let text = match url::Url::parse(&text) {
        Ok(mut u) if !u.username().is_empty() || u.password().is_some() => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        _ => text,
    };
    let provider = provider_egress::Provider {
        kind: row.get("kind").as_str().map(|k| k.encode_utf16().collect()),
        external: matches!(row.get("external"), JValue::Bool(true)),
        base_url: text.encode_utf16().collect(),
        label: Vec::new(),
    };
    provider_egress::trial(Some(&provider)).unwrap_or(true)
}

// ---------------------------------------------------------------------------------------------
// The deployment's default row and the merged view.

/// What core index.cjs reads from the environment for the default provider row.
struct Deployment {
    id: String,
    label: String,
    base_url: String,
    key: Zeroizing<String>,
    /// `MODEL_MANAGER_KIND`, as read (the engine capabilities match it exactly).
    kind: String,
    /// core `modelManager.enabled`: the kind (any case) is `lemonade` or `llamacpp`.
    manager_enabled: bool,
}

fn deployment() -> Deployment {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let kind = env("MODEL_MANAGER_KIND").unwrap_or_else(|| {
        if env("LEMONADE_BASE_URL").is_some() {
            "lemonade".into()
        } else {
            "none".into()
        }
    });
    Deployment {
        id: default_provider_id(),
        label: env("DEFAULT_PROVIDER_LABEL").unwrap_or_else(|| "Local inference".into()),
        base_url: env("INFERENCE_BASE_URL")
            .or_else(|| env("LEMONADE_BASE_URL"))
            .unwrap_or_else(|| "http://host.docker.internal:11434".into()),
        key: Zeroizing::new(
            env("INFERENCE_API_KEY")
                .or_else(|| env("LEMONADE_API_KEY"))
                .unwrap_or_default(),
        ),
        manager_enabled: matches!(kind.to_lowercase().as_str(), "lemonade" | "llamacpp"),
        kind,
    }
}

impl Deployment {
    /// The row core's workspace puts first in every view: `{ ...defaultProvider, shared: true }`.
    fn row(&self) -> JValue {
        let mut items = vec![
            ("id".to_string(), JValue::Str(self.id.clone())),
            ("label".to_string(), JValue::Str(self.label.clone())),
            ("baseUrl".to_string(), JValue::Str(self.base_url.clone())),
            ("apiKey".to_string(), JValue::Str(self.key.to_string())),
            ("shared".to_string(), JValue::Bool(true)),
        ];
        // core providers.cjs `engineCapabilities(kind)`.
        if self.kind == "llamacpp" {
            items.push((
                "capabilities".into(),
                JValue::obj([("jsonSchemaParam", JValue::Bool(true))]),
            ));
        }
        JValue::Obj(items)
    }
}

/// One row of the merged view with its key opened.
struct ViewRow {
    raw: JValue,
    key: Zeroizing<String>,
    /// The stored key could not be opened on this server (#782).
    unreadable: bool,
    from_shared_file: bool,
}

fn load_keys(data_dir: &Path) -> Option<SecretKeys> {
    let previous = std::env::var("SECRETS_KEY_PREVIOUS")
        .ok()
        .map(Zeroizing::new);
    server_auth::secrets::load_existing(data_dir, previous.as_deref().map(String::as_str)).ok()
}

/// core workspace.cjs `openProviderKey`: the opened key, and whether it could not be opened.
fn open_key(raw: &JValue, keys: Option<&SecretKeys>) -> (Zeroizing<String>, bool) {
    let stored = or_empty(raw.get("apiKey"));
    match keys {
        Some(k) => match k.open(&stored, None) {
            Ok((plain, _)) => (plain, false),
            Err(_) => (Zeroizing::new(String::new()), true),
        },
        None => (Zeroizing::new(String::new()), false),
    }
}

fn user_dir_of(data_dir: &Path, user: &str) -> Option<PathBuf> {
    store::user_dir(data_dir, user).ok()
}

/// core workspace.cjs `mergeProviders` as a request sees it: the default row, the shared rows
/// (minus a stale default), then the account's own (a `lemonade` row read as the default, the
/// first of each id, shared and default rows dropped).
fn view(data_dir: &Path, user: &str, keys: Option<&SecretKeys>, dep: &Deployment) -> Vec<ViewRow> {
    let mut out = vec![ViewRow {
        raw: dep.row(),
        key: dep.key.clone(),
        unreadable: false,
        from_shared_file: false,
    }];
    for raw in provider_rows::read_shared(data_dir).unwrap_or_default() {
        if has_string_id(&raw, &dep.id) {
            continue;
        }
        let (key, unreadable) = open_key(&raw, keys);
        out.push(ViewRow {
            raw,
            key,
            unreadable,
            from_shared_file: true,
        });
    }
    if let Some(dir) = user_dir_of(data_dir, user) {
        let mut seen: Vec<JValue> = Vec::new();
        for mut raw in provider_rows::read_user(&dir).unwrap_or_default() {
            if has_string_id(&raw, "lemonade") {
                if let JValue::Obj(items) = &mut raw {
                    put(items, "id", JValue::Str(dep.id.clone()));
                    put(items, "label", JValue::Str(dep.label.clone()));
                }
            }
            let id = raw.get("id").clone();
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            if raw.get("shared").truthy() || has_string_id(&raw, &dep.id) {
                continue;
            }
            let (key, unreadable) = open_key(&raw, keys);
            out.push(ViewRow {
                raw,
                key,
                unreadable,
                from_shared_file: false,
            });
        }
    }
    out
}

fn is_chatgpt(raw: &JValue) -> bool {
    raw.get("kind").as_str() == Some(CHATGPT_KIND)
}

/// core routes/providers.cjs `listRow(pr, authn)`.
fn list_row(row: &ViewRow, dep: &Deployment, connection: &str) -> JValue {
    let raw = &row.raw;
    let is_default = has_string_id(raw, &dep.id);
    let mut out: Vec<(String, JValue)> = vec![
        ("id".into(), raw.get("id").clone()),
        ("label".into(), raw.get("label").clone()),
        ("baseUrl".into(), raw.get("baseUrl").clone()),
        ("apiKeyMasked".into(), mask_key(&row.key)),
        ("isDefault".into(), JValue::Bool(is_default)),
        (
            "managed".into(),
            JValue::Bool(is_default && dep.manager_enabled),
        ),
        ("shared".into(), JValue::Bool(raw.get("shared").truthy())),
        (
            "defaultModel".into(),
            if raw.get("defaultModel").truthy() {
                raw.get("defaultModel").clone()
            } else {
                JValue::Undefined
            },
        ),
    ];
    if let Some(n) = valid_context_tokens(raw.get("contextTokens")) {
        out.push(("contextTokens".into(), JValue::Num(n)));
    }
    let capabilities = effective_capabilities(raw);
    if matches!(&capabilities, JValue::Obj(items) if !items.is_empty()) {
        out.push(("capabilities".into(), capabilities));
    }
    if row.unreadable {
        out.push(("keyUnreadable".into(), JValue::Bool(true)));
    }
    if trial_terms_host(raw) {
        out.push(("external".into(), JValue::Bool(true)));
    }
    if is_chatgpt(raw) {
        put(&mut out, "kind", JValue::Str(CHATGPT_KIND.into()));
        put(&mut out, "external", JValue::Bool(true));
        put(&mut out, "connection", JValue::Str(connection.into()));
    }
    JValue::Obj(out)
}

// ---------------------------------------------------------------------------------------------
// Sealing.

fn nonce() -> [u8; 12] {
    let mut out = [0u8; 12];
    OsRng.fill_bytes(&mut out);
    out
}

/// secrets.cjs `encrypt(value)` (unbound, `enc:v1`); an empty value stays empty.
fn seal_key(keys: Option<&SecretKeys>, plain: &str) -> Result<String, ()> {
    if plain.is_empty() {
        return Ok(String::new());
    }
    keys.ok_or(())?.seal(plain, None, &nonce()).map_err(|_| ())
}

/// core workspace.cjs `sealProviderRow` for every row of a save: a plaintext key, or one sealed
/// under the retired key, is sealed under the current one; a key that opens under the current
/// key, and one that cannot be opened (kept as it was, #782), are left as they are.
fn normalise_keys(rows: &mut [JValue], keys: Option<&SecretKeys>) {
    let Some(keys) = keys else { return };
    for row in rows {
        let JValue::Obj(items) = row else { continue };
        let Some(stored) = items
            .iter()
            .find(|(k, _)| k == "apiKey")
            .and_then(|(_, v)| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        if let Ok((plain, used)) = keys.open(&stored, None) {
            if used.tag() != 1 {
                if let Ok(sealed) = keys.seal(&plain, None, &nonce()) {
                    put(items, "apiKey", JValue::Str(sealed));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The ChatGPT broker, when this build has one.

#[cfg(feature = "rust-chatgpt-status")]
async fn chatgpt_on(app: &Arc<App>) -> bool {
    match app.chatgpt_oauth.as_ref() {
        Some(broker) => broker.enabled(app).await,
        None => false,
    }
}
#[cfg(not(feature = "rust-chatgpt-status"))]
async fn chatgpt_on(_app: &Arc<App>) -> bool {
    false
}

#[cfg(feature = "rust-chatgpt-status")]
async fn connection_of(app: &Arc<App>, user: &str) -> Option<&'static str> {
    app.chatgpt_oauth.as_ref()?.connection(app, user).await
}
#[cfg(not(feature = "rust-chatgpt-status"))]
async fn connection_of(_app: &Arc<App>, _user: &str) -> Option<&'static str> {
    None
}

/// The broker deletes the account's tokens: `Err((status, message))` as core's catch words it.
#[cfg(feature = "rust-chatgpt-status")]
async fn disconnect_tokens(app: &Arc<App>, user: &str) -> Result<(), (u16, String)> {
    let Some(broker) = app.chatgpt_oauth.as_ref() else {
        return Ok(());
    };
    broker.disconnect(app, user, false).await.map_err(|e| {
        if e.status > 0 && e.status < 600 {
            (e.status, e.message.clone())
        } else {
            (503, "ChatGPT sign-in failed.".into())
        }
    })
}
#[cfg(not(feature = "rust-chatgpt-status"))]
async fn disconnect_tokens(_app: &Arc<App>, _user: &str) -> Result<(), (u16, String)> {
    Ok(())
}

fn has_broker(app: &Arc<App>) -> bool {
    #[cfg(feature = "rust-chatgpt-status")]
    {
        app.chatgpt_oauth.is_some()
    }
    #[cfg(not(feature = "rust-chatgpt-status"))]
    {
        let _ = app;
        false
    }
}

// ---------------------------------------------------------------------------------------------
// Routing.

enum Target {
    List,
    Create,
    Edit(String),
    Remove(String),
    /// `DELETE /api/providers/chatgpt`.
    Disconnect,
}

/// Node compared exact pathnames; `/api/providers/<one segment>` for PUT and DELETE (a `chatgpt`
/// PUT is the ChatGPT mount's 404/405, which stays Node's).
fn classify(method: &Method, path: &str) -> Option<Target> {
    if path == LIST {
        return match *method {
            Method::GET => Some(Target::List),
            Method::POST => Some(Target::Create),
            _ => None,
        };
    }
    let segment = path
        .strip_prefix("/api/providers/")
        .filter(|s| !s.is_empty() && !s.contains('/'))?;
    match *method {
        Method::DELETE if segment == "chatgpt" => Some(Target::Disconnect),
        Method::PUT if segment != "chatgpt" => Some(Target::Edit(segment.to_owned())),
        Method::DELETE => Some(Target::Remove(segment.to_owned())),
        _ => None,
    }
}

struct Who {
    user: String,
    admin: bool,
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let Some(target) = classify(req.method(), &path) else {
        return app.proxy.forward(conn.peer, req).await;
    };
    let Some(data_dir) = app.config.data_dir.clone() else {
        return app.proxy.forward(conn.peer, req).await;
    };
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let who = Who {
        user: id.user.id.clone(),
        admin: id.user.role == server_auth::Role::Admin,
    };
    match target {
        Target::List => list(app, data_dir, who).await,
        Target::Create => create(data_dir, who, body).await,
        Target::Edit(segment) => edit(data_dir, who, &segment, body).await,
        Target::Remove(segment) => remove(app, data_dir, who, &segment).await,
        Target::Disconnect => disconnect(app, data_dir, who).await,
    }
}

/// Debug builds only (the tests): a pause between the tokens going and the row going, to leave
/// the multi-step disconnect open for as long as a test needs, for the data directory it names.
#[cfg(debug_assertions)]
pub mod hooks {
    use std::path::PathBuf;
    use std::sync::Mutex;
    pub static PAUSE_AFTER_TOKENS: Mutex<Vec<(PathBuf, u64)>> = Mutex::new(Vec::new());
    pub async fn after_tokens(data_dir: &std::path::Path) {
        let ms = PAUSE_AFTER_TOKENS
            .lock()
            .ok()
            .and_then(|v| v.iter().find(|(d, _)| d == data_dir).map(|(_, ms)| *ms));
        if let Some(ms) = ms {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
    }
}

/// Runs `work` to completion even if the client leaves, and awaits it.
async fn spawned<T: Send + 'static>(
    work: impl std::future::Future<Output = T> + Send + 'static,
) -> Option<T> {
    tokio::spawn(work).await.ok()
}

fn failed() -> Response<Body> {
    error(500, "Internal error")
}

// ---------------------------------------------------------------------------------------------
// GET /api/providers.

async fn list(app: &Arc<App>, data_dir: PathBuf, who: Who) -> Response<Body> {
    let chatgpt = chatgpt_on(app).await;
    let connection = if chatgpt {
        connection_of(app, &who.user).await
    } else {
        None
    };
    let reconcile = matches!(connection, Some("connected" | "reconnect"));
    let user = who.user.clone();
    let rows = tokio::task::spawn_blocking(move || {
        // The broker saved a sign-in whose row was not written (core's reconcileChatGptRow).
        if reconcile {
            if let Err(e) = provider_files::ensure_chatgpt_row(&data_dir, &user) {
                eprintln!("noevia-server: ChatGPT provider row not written ({e:?})");
            }
        }
        let dep = deployment();
        let keys = load_keys(&data_dir);
        view(&data_dir, &user, keys.as_ref(), &dep)
            .into_iter()
            // A ChatGPT row is listed only while the flag is on, and only the account's own.
            .filter(|r| !is_chatgpt(&r.raw) || (chatgpt && !r.raw.get("shared").truthy()))
            .map(|r| list_row(&r, &dep, connection.unwrap_or("disconnected")))
            .collect::<Vec<_>>()
    })
    .await;
    match rows {
        Ok(rows) => answer(200, &JValue::obj([("providers", JValue::Arr(rows))])),
        Err(_) => failed(),
    }
}

// ---------------------------------------------------------------------------------------------
// POST /api/providers.

async fn body_object(body: Body, strict: bool) -> Result<JValue, Box<Response<Body>>> {
    let bytes = match read_body(body).await {
        Ok(b) => b,
        Err(BodyError::TooLarge) => return Err(Box::new(error(413, "Request exceeds size limit"))),
        Err(BodyError::Failed) => return Err(Box::new(error(400, "invalid JSON"))),
    };
    let parsed = wtf16::parse(&String::from_utf8_lossy(&bytes))
        .map_err(|_| Box::new(error(400, "invalid JSON")))?;
    match (&parsed, strict) {
        (JValue::Obj(_), _) => Ok(parsed),
        // core http.cjs `isJsonObject`.
        (_, true) => Err(Box::new(error(400, "request body must be a JSON object"))),
        // PUT: `!body || typeof body !== 'object' || Array.isArray(body)`.
        (_, false) => Err(Box::new(error(400, "invalid JSON"))),
    }
}

fn base36(n: usize) -> String {
    let mut out = String::new();
    for _ in 0..n {
        let mut b = [0u8; 1];
        loop {
            OsRng.fill_bytes(&mut b);
            // 252 = 7 * 36: no modulo bias.
            if b[0] < 252 {
                out.push(char::from_digit(u32::from(b[0] % 36), 36).unwrap_or('0'));
                break;
            }
        }
    }
    out
}

async fn create(data_dir: PathBuf, who: Who, body: Body) -> Response<Body> {
    let body = match body_object(body, true).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let label = trimmed_slice(body.get("label"), 80);
    let base_url = trimmed_address(&or_empty(body.get("baseUrl")));
    let api_key = js_trim(&or_empty(body.get("apiKey"))).to_string();
    let default_model = trimmed_slice(body.get("defaultModel"), 200);
    let shared = matches!(body.get("shared"), JValue::Bool(true));
    if shared && !who.admin {
        return error(403, "administrator required for shared providers");
    }
    if label.is_empty() {
        return error(400, "label required");
    }
    let context = match parse_context_tokens(body.get("contextTokens")) {
        Ok(c) => c,
        Err(e) => return error(400, e),
    };
    if !http_prefixed(&base_url) {
        return error(400, "baseUrl must be an http(s) URL");
    }
    if wtf16::len(&base_url) > MAX_BASE_URL {
        return error(400, "baseUrl is too long");
    }
    // A member must not register an endpoint the server can only reach from its own network.
    if !endpoint_approved(who.admin, &base_url) {
        return error(400, NOT_APPROVED);
    }
    let capabilities = match body.get("capabilities") {
        JValue::Undefined | JValue::Null => None,
        c => match parse_capabilities(c) {
            Ok(v) => Some(v),
            Err(e) => return error(400, &e),
        },
    };
    let id = format!("prov-{}-{}", identity::now_ms(), base36(6));
    let masked = mask_key(&api_key);
    let reply_body = {
        let mut items = vec![
            ("id".to_string(), JValue::Str(id.clone())),
            ("label".to_string(), JValue::Str(label.clone())),
            ("baseUrl".to_string(), JValue::Str(base_url.clone())),
            (
                "defaultModel".to_string(),
                JValue::Str(default_model.clone()),
            ),
            ("apiKeyMasked".to_string(), masked),
        ];
        if let Some(n) = context {
            items.push(("contextTokens".into(), JValue::Num(n)));
        }
        JValue::Obj(items)
    };
    let work = async move {
        tokio::task::spawn_blocking(move || {
            let keys = load_keys(&data_dir);
            let sealed = match seal_key(keys.as_ref(), &api_key) {
                Ok(s) => s,
                Err(()) => {
                    return Err((
                        503,
                        "The credential store is unavailable. Try again shortly.",
                    ))
                }
            };
            let mut row = vec![
                ("id".to_string(), JValue::Str(id)),
                ("label".to_string(), JValue::Str(label)),
                ("baseUrl".to_string(), JValue::Str(base_url)),
                ("apiKey".to_string(), JValue::Str(sealed)),
                ("defaultModel".to_string(), JValue::Str(default_model)),
                ("shared".to_string(), JValue::Bool(shared)),
            ];
            if let Some(n) = context {
                row.push(("contextTokens".into(), JValue::Num(n)));
            }
            if let Some(c) = capabilities {
                row.push(("capabilities".into(), c));
            }
            let row = JValue::Obj(row);
            let change = |rows: &mut Vec<JValue>| {
                rows.push(row);
                normalise_keys(rows, keys.as_ref());
                ((), true)
            };
            let saved = if shared {
                provider_rows::update_shared(&data_dir, server_store::RustProjects::owned(), change)
            } else {
                match store::user_dir(&data_dir, &who.user) {
                    Ok(dir) => provider_rows::update_user(
                        &dir,
                        server_store::RustProjects::owned(),
                        change,
                    ),
                    Err(e) => Err(e),
                }
            };
            saved.map_err(|e| store_failure(&e))
        })
        .await
    };
    match spawned(work).await {
        Some(Ok(Ok(()))) => answer(200, &reply_body),
        Some(Ok(Err((status, message)))) => error(status, message),
        _ => failed(),
    }
}

// ---------------------------------------------------------------------------------------------
// PUT /api/providers/{id}.

/// core routes/providers.cjs `editRefusal` for a row found: why it may not be edited, or none.
fn edit_refusal(raw: &JValue, admin: bool) -> Option<(u16, &'static str)> {
    if is_chatgpt(raw) || matches!(raw.get("external"), JValue::Bool(true)) {
        return Some((
            400,
            "this provider is managed by its sign-in and cannot be edited",
        ));
    }
    if raw.get("shared").truthy() && !admin {
        return Some((403, "administrator required"));
    }
    None
}

enum Edited {
    Refused(u16, String),
    Done(Box<ViewRow>),
}

/// The edit as one step inside the file's lock: the row as it is now, the fields as core checks
/// them, then the changes in core's order.
fn edit_rows(
    rows: &mut [JValue],
    id: &str,
    body: &JValue,
    who: &Who,
    keys: Option<&SecretKeys>,
    from_shared_file: bool,
) -> (Edited, bool) {
    let refuse = |status: u16, message: &str| (Edited::Refused(status, message.to_owned()), false);
    // The first row with this id the view shows (a private file's shared-flagged rows are not in it).
    let Some(at) = rows
        .iter()
        .position(|r| has_string_id(r, id) && (from_shared_file || !r.get("shared").truthy()))
    else {
        return refuse(404, "no such provider");
    };
    let Some(row) = rows.get(at).cloned() else {
        return refuse(404, "no such provider");
    };
    if let Some((status, message)) = edit_refusal(&row, who.admin) {
        return refuse(status, message);
    }
    for field in ["label", "baseUrl", "apiKey", "defaultModel"] {
        if !matches!(
            body.get(field),
            JValue::Undefined | JValue::Null | JValue::Str(_)
        ) {
            return refuse(400, &format!("{field} must be a string"));
        }
    }
    let label = match body.get("label") {
        JValue::Undefined => row.get("label").clone(),
        v => JValue::Str(trimmed_slice(v, 80)),
    };
    let base_url = match body.get("baseUrl") {
        JValue::Undefined => row.get("baseUrl").clone(),
        v => JValue::Str(trimmed_address(&or_empty(v))),
    };
    let new_key = js_trim(&or_empty(body.get("apiKey"))).to_string();
    let default_model = match body.get("defaultModel") {
        JValue::Undefined => JValue::Str(or_empty(row.get("defaultModel"))),
        v => JValue::Str(trimmed_slice(v, 200)),
    };
    if !label.truthy() {
        return refuse(400, "label required");
    }
    let base_text = js_json::to_js_string(&base_url).unwrap_or_default();
    if !http_prefixed(&base_text) {
        return refuse(400, "baseUrl must be an http(s) URL");
    }
    if wtf16::len(&base_text) > MAX_BASE_URL {
        return refuse(400, "baseUrl is too long");
    }
    // The same member-origin rule as POST whenever the address changes.
    if base_url != *row.get("baseUrl") && !endpoint_approved(who.admin, &base_text) {
        return refuse(400, NOT_APPROVED);
    }
    let (stored_key, unreadable) = open_key(&row, keys);
    let same_origin = {
        let new_origin = origin_of(&base_text);
        let old_origin = row.get("baseUrl").as_str().and_then(origin_of);
        new_origin.is_some() && new_origin == old_origin
    };
    // A stored key never follows the provider to another origin (that would send it somewhere the
    // person never typed it for).
    if new_key.is_empty()
        && !stored_key.is_empty()
        && stored_key.as_str() != "local"
        && !same_origin
    {
        return refuse(
            400,
            "Enter the API key again when moving a provider to a different address.",
        );
    }
    let context = match body.get("contextTokens") {
        JValue::Undefined => Ok(valid_context_tokens(row.get("contextTokens"))),
        v => parse_context_tokens(v),
    };
    let context = match context {
        Ok(c) => c,
        Err(e) => return refuse(400, e),
    };
    // Omitted keeps the stored capabilities; null returns to the preset default; an object
    // replaces them.
    let capabilities = match body.get("capabilities") {
        JValue::Null => JValue::Undefined,
        JValue::Undefined => row.get("capabilities").clone(),
        c => match parse_capabilities(c) {
            Ok(v) => v,
            Err(e) => return refuse(400, &e),
        },
    };
    // Validated in full before anything changes. A key that could not be opened is kept for its
    // own origin only: moving the row without a new key forgets it.
    let forget_key = new_key.is_empty() && unreadable && !same_origin;
    let sealed = if new_key.is_empty() {
        None
    } else {
        match seal_key(keys, &new_key) {
            Ok(s) => Some(s),
            Err(()) => {
                return refuse(
                    503,
                    "The credential store is unavailable. Try again shortly.",
                )
            }
        }
    };
    let Some(JValue::Obj(items)) = rows.get_mut(at) else {
        return refuse(404, "no such provider");
    };
    put(items, "label", label);
    put(items, "baseUrl", base_url);
    if let Some(s) = sealed {
        put(items, "apiKey", JValue::Str(s));
    } else if forget_key {
        put(items, "apiKey", JValue::Str(String::new()));
    }
    put(items, "defaultModel", default_model);
    match context {
        Some(n) => put(items, "contextTokens", JValue::Num(n)),
        None => items.retain(|(k, _)| k != "contextTokens"),
    }
    if capabilities.truthy() {
        put(items, "capabilities", capabilities);
    } else {
        items.retain(|(k, _)| k != "capabilities");
    }
    normalise_keys(rows, keys);
    let Some(edited) = rows.get(at).cloned() else {
        return refuse(404, "no such provider");
    };
    let (key, unreadable) = if new_key.is_empty() {
        (stored_key, unreadable && !forget_key)
    } else {
        (Zeroizing::new(new_key), false)
    };
    (
        Edited::Done(Box::new(ViewRow {
            raw: edited,
            key,
            unreadable,
            from_shared_file,
        })),
        true,
    )
}

async fn edit(data_dir: PathBuf, who: Who, segment: &str, body: Body) -> Response<Body> {
    let Some(id) = crate::provider_models_routes::decode_component(segment) else {
        return error(400, "no such provider");
    };
    let dep = deployment();
    if id == dep.id || id == "lemonade" {
        return error(400, "the default provider cannot be edited");
    }
    // The refusals that come before the body is read.
    let (d, u, i) = (data_dir.clone(), who.user.clone(), id.clone());
    let found = tokio::task::spawn_blocking(move || {
        let keys = load_keys(&d);
        let dep = deployment();
        view(&d, &u, keys.as_ref(), &dep)
            .into_iter()
            .find(|r| has_string_id(&r.raw, &i))
            .map(|r| (r.from_shared_file, r.raw))
    })
    .await;
    let Ok(found) = found else { return failed() };
    let Some((from_shared_file, raw)) = found else {
        return error(404, "no such provider");
    };
    if let Some((status, message)) = edit_refusal(&raw, who.admin) {
        return error(status, message);
    }
    let body = match body_object(body, false).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let work = async move {
        tokio::task::spawn_blocking(move || {
            let keys = load_keys(&data_dir);
            let change = |rows: &mut Vec<JValue>| {
                edit_rows(rows, &id, &body, &who, keys.as_ref(), from_shared_file)
            };
            let switch = server_store::RustProjects::owned();
            let result = if from_shared_file {
                provider_rows::update_shared(&data_dir, switch, change)
            } else {
                match store::user_dir(&data_dir, &who.user) {
                    Ok(dir) => provider_rows::update_user(&dir, switch, change),
                    Err(e) => Err(e),
                }
            };
            (result, deployment())
        })
        .await
    };
    match spawned(work).await {
        Some(Ok((Ok(Edited::Done(row)), dep))) => {
            answer(200, &list_row(&row, &dep, "disconnected"))
        }
        Some(Ok((Ok(Edited::Refused(status, message)), _))) => error(status, &message),
        Some(Ok((Err(e), _))) => store_failed(&e),
        _ => failed(),
    }
}

// ---------------------------------------------------------------------------------------------
// DELETE /api/providers/{id} and DELETE /api/providers/chatgpt.

/// Removes `id`'s rows from the file it lives in, then the account's projects fall back.
fn remove_and_fall_back(
    data_dir: &Path,
    user: &str,
    id: &str,
    shared: bool,
) -> Result<bool, StoreError> {
    let removed = provider_files::remove_rows(data_dir, user, id, shared)?;
    if removed {
        provider_files::clear_project_fallbacks(data_dir, user, id)?;
    }
    Ok(removed)
}

async fn remove(app: &Arc<App>, data_dir: PathBuf, who: Who, segment: &str) -> Response<Body> {
    let Some(id) = crate::provider_models_routes::decode_component(segment) else {
        return error(400, "no such provider");
    };
    let dep = deployment();
    if id == dep.id || id == "lemonade" {
        return error(400, "the default provider cannot be removed");
    }
    let (d, u, i) = (data_dir.clone(), who.user.clone(), id.clone());
    let selected = tokio::task::spawn_blocking(move || {
        let keys = load_keys(&d);
        let dep = deployment();
        view(&d, &u, keys.as_ref(), &dep)
            .into_iter()
            .find(|r| has_string_id(&r.raw, &i))
            .map(|r| (r.from_shared_file, r.raw))
    })
    .await;
    let Ok(selected) = selected else {
        return failed();
    };
    if selected
        .as_ref()
        .is_some_and(|(_, raw)| raw.get("shared").truthy() && !who.admin)
    {
        return error(403, "administrator required");
    }
    let app = Arc::clone(app);
    let work = async move {
        // Removing the ChatGPT row is disconnecting: its tokens go too, flag on or off, and first
        // (the broker's), so a failed delete leaves the row rather than tokens without a row.
        if let Some((_, raw)) = &selected {
            if is_chatgpt(raw) && !raw.get("shared").truthy() && has_broker(&app) {
                if let Err((status, message)) = disconnect_tokens(&app, &who.user).await {
                    return error(status, &message);
                }
                #[cfg(debug_assertions)]
                hooks::after_tokens(&data_dir).await;
            }
        }
        let shared_file = selected.as_ref().is_some_and(|(shared, _)| *shared);
        if selected.is_none() {
            return error(404, "no such provider");
        }
        let (d, u) = (data_dir, who.user);
        match tokio::task::spawn_blocking(move || remove_and_fall_back(&d, &u, &id, shared_file))
            .await
        {
            Ok(Ok(true)) => answer(200, &JValue::obj([("ok", JValue::Bool(true))])),
            Ok(Ok(false)) => error(404, "no such provider"),
            Ok(Err(e)) => store_failed(&e),
            Err(_) => failed(),
        }
    };
    spawned(work).await.unwrap_or_else(failed)
}

/// core routes/providers.cjs `handleChatGpt` for DELETE: the flag, then the tokens, then the row
/// and the fallbacks.
async fn disconnect(app: &Arc<App>, data_dir: PathBuf, who: Who) -> Response<Body> {
    if !chatgpt_on(app).await {
        return error(404, OFF_MESSAGE);
    }
    let app = Arc::clone(app);
    let work = async move {
        if let Err((status, message)) = disconnect_tokens(&app, &who.user).await {
            return error(status, &message);
        }
        #[cfg(debug_assertions)]
        hooks::after_tokens(&data_dir).await;
        let (d, u) = (data_dir, who.user);
        let done = tokio::task::spawn_blocking(move || -> Result<(), StoreError> {
            // The row (the account's own) and the projects that pointed at it.
            provider_files::remove_rows(&d, &u, CHATGPT_ID, false)?;
            provider_files::clear_project_fallbacks(&d, &u, CHATGPT_ID)?;
            Ok(())
        })
        .await;
        match done {
            Ok(Ok(())) => answer(
                200,
                &JValue::obj([
                    ("ok", JValue::Bool(true)),
                    ("state", JValue::Str("disconnected".into())),
                ]),
            ),
            // Core's catch: a failure after the tokens went is a 502 "ChatGPT sign-in failed.".
            _ => error(502, "ChatGPT sign-in failed."),
        }
    };
    spawned(work).await.unwrap_or_else(failed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn j(s: &str) -> JValue {
        wtf16::parse(s).unwrap()
    }

    #[test]
    fn context_windows_are_cores_bounds() {
        assert_eq!(parse_context_tokens(&j("null")), Ok(None));
        assert_eq!(parse_context_tokens(&JValue::Undefined), Ok(None));
        assert_eq!(parse_context_tokens(&j(r#""""#)), Ok(None));
        assert_eq!(parse_context_tokens(&j("8192")), Ok(Some(8192.0)));
        assert_eq!(parse_context_tokens(&j(r#"" 8192 ""#)), Ok(Some(8192.0)));
        for bad in [
            "2047", "2000001", "1.5", r#""1.5""#, r#""abc""#, "true", "[]", r#""-5""#,
        ] {
            assert_eq!(parse_context_tokens(&j(bad)), Err(CONTEXT_MESSAGE), "{bad}");
        }
        assert_eq!(valid_context_tokens(&j("4096")), Some(4096.0));
        assert_eq!(valid_context_tokens(&j(r#""4096""#)), None);
        assert_eq!(valid_context_tokens(&j("10")), None);
    }

    #[test]
    fn capabilities_are_cores_parse() {
        let ok = parse_capabilities(&j(
            r#"{"jsonSchemaParam":true,"tokenBudgetField":"max_tokens","reasoningEffortModels":[" a "]}"#,
        ))
        .unwrap();
        assert_eq!(
            js_json::stringify(&ok).unwrap(),
            r#"{"reasoningEffortModels":["a"],"tokenBudgetField":"max_tokens","jsonSchemaParam":true}"#
        );
        for (input, message) in [
            ("null", "capabilities must be an object"),
            ("[]", "capabilities must be an object"),
            (r#"{"x":1}"#, "unknown capability: x"),
            (
                r#"{"reasoningEffortParam":"yes"}"#,
                "reasoningEffortParam must be true or false",
            ),
            (
                r#"{"reasoningEffortModels":[""]}"#,
                "reasoningEffortModels must be a list of up to 50 model ids",
            ),
            (
                r#"{"tokenBudgetField":"n"}"#,
                "tokenBudgetField must be one of max_tokens, max_completion_tokens",
            ),
            (
                r#"{"jsonSchemaParam":1}"#,
                "jsonSchemaParam must be true or false",
            ),
        ] {
            assert_eq!(
                parse_capabilities(&j(input)).unwrap_err(),
                message,
                "{input}"
            );
        }
    }

    #[test]
    fn a_row_without_capabilities_inherits_the_openai_preset() {
        let preset = |base: &str| {
            js_json::stringify(&effective_capabilities(&j(&format!(
                r#"{{"baseUrl":"{base}"}}"#
            ))))
            .unwrap()
        };
        assert_eq!(
            preset("https://api.openai.com/v1"),
            r#"{"tokenBudgetField":"max_completion_tokens","reasoningEffortParam":true,"reasoningEffortModels":["gpt-5.4"]}"#
        );
        assert_eq!(
            preset("https://api.openai.com/other"),
            r#"{"tokenBudgetField":"max_completion_tokens"}"#
        );
        assert_eq!(
            preset("https://api.openai.com/v1?x=1"),
            r#"{"tokenBudgetField":"max_completion_tokens"}"#
        );
        assert_eq!(preset("https://example.test/v1"), "{}");
        assert_eq!(preset("nope"), "{}");
        // Its own capabilities, even {}, are never overridden.
        let own = j(r#"{"baseUrl":"https://api.openai.com/v1","capabilities":{}}"#);
        assert_eq!(
            js_json::stringify(&effective_capabilities(&own)).unwrap(),
            "{}"
        );
    }

    #[test]
    fn keys_are_masked_to_their_last_four_characters() {
        assert_eq!(mask_key(""), JValue::Null);
        assert_eq!(mask_key("local"), JValue::Null);
        assert_eq!(mask_key("short"), JValue::Str("••••".into()));
        assert_eq!(
            mask_key("sk-synthetic-12345678"),
            JValue::Str("…5678".into())
        );
    }

    #[test]
    fn routes_are_the_exact_ones_core_answered() {
        let t = |m: Method, p: &str| classify(&m, p);
        assert!(matches!(
            t(Method::GET, "/api/providers"),
            Some(Target::List)
        ));
        assert!(matches!(
            t(Method::POST, "/api/providers"),
            Some(Target::Create)
        ));
        assert!(t(Method::PUT, "/api/providers").is_none());
        assert!(t(Method::GET, "/api/providers/x").is_none());
        assert!(matches!(
            t(Method::PUT, "/api/providers/x"),
            Some(Target::Edit(_))
        ));
        assert!(matches!(
            t(Method::PUT, "/api/providers/test"),
            Some(Target::Edit(_))
        ));
        assert!(matches!(
            t(Method::DELETE, "/api/providers/x"),
            Some(Target::Remove(_))
        ));
        assert!(matches!(
            t(Method::DELETE, "/api/providers/chatgpt"),
            Some(Target::Disconnect)
        ));
        assert!(t(Method::PUT, "/api/providers/chatgpt").is_none());
        assert!(t(Method::DELETE, "/api/providers/x/models").is_none());
        assert!(t(Method::DELETE, "/api/providers/").is_none());
    }
}
