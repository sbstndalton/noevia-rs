//! The chat's Google Drive tools, run by the front (track R5): core `gdrive-tools.cjs` (`execute`,
//! `describeTarget`) over `gdrive-files.cjs` (the seven file operations) and the access part of
//! `gdrive-broker.cjs` (an authorized request: one refresh past a refused token, a second refusal drops that
//! sign-in's cached token). The token is this front's own (`gdrive.rs`); the reply shaping, the write plan
//! and the read-before-update gate (#659) are the `drive-tools` crate's.
//!
//! Switch: `NOEVIA_RUST_DRIVE_TOOLS=1` (off by default). Off, `POST /v1/gdrive/tool` and
//! `POST /v1/gdrive/target` are 404s and Node runs the tools itself; on, Node asks here.
//!
//! Proven against core: `tests/fixtures/gdrive-tools-node.v1.json` is recorded from core's real tools,
//! file operations and broker client by `tests/gdrive-tools-oracle.cjs` and replayed through [`execute`]
//! and [`describe_target`] by `tests/gdrive_tools_oracle.rs` (every HTTP request with its headers and
//! body, every refused sign-in, every answer).
//!
//! Text is JS text: the request and Drive's replies are read with `js_json::wtf16`, so a lone surrogate (a
//! model's broken emoji, a file name cut in half) is kept as core keeps it. It is written back as core
//! writes it: as the `\udxxx` escape in JSON, as U+FFFD bytes in a file's content, and it makes an address
//! or a header impossible (core's `encodeURIComponent` and fetch throw: the generic text).
//!
//! Named differences: a read cut that would split a character's two UTF-16 units leaves the character out
//! (`drive-tools` `shape_read`); a Drive `mimeType` that names an `Object.prototype` member (`toString`,
//! `__proto__`) is a plain unknown type here (core's lookup would find the prototype's function); a
//! redirect from Drive is not followed (the generic text; core's fetch follows it, which would carry the
//! access token to wherever it points).
use drive_tools::{ReadGate, Tool, UpdateCheck};
use js_json::wtf16;
use js_json::JValue;
use std::future::Future;
use std::sync::Mutex;

/// core `MAX_READ`: bytes pulled from Drive per read.
pub const MAX_READ: usize = 64 * 1024;
/// core's `readCappedJson(r, 8 MiB)`.
const JSON_CAP: usize = 8 * 1024 * 1024;
const FIELDS: &str = "id,name,mimeType,size,modifiedTime,createdTime,webViewLink,trashed,version";
const UNREACHABLE: &str = "Google Drive could not be reached.";
const NOT_CONNECTED: &str =
    "ERROR: Google Drive is not connected for this account. Connect it in Settings → Connectors.";
/// core `describeTarget`'s refusal of an id that is not Drive's.
const NOT_A_FILE_ID_TAIL: &str = " is not a Google Drive file id. The Google Drive tools only change files in the connected Google Drive; to change a file uploaded to this project, use the project file tools (project_append_file or project_replace_text)";
/// The longest chat or exchange key taken (Node's are ids).
const MAX_KEY: usize = 256;

/// The calling account, as Node's request scope names it.
#[derive(Clone, Debug, PartialEq)]
pub struct User {
    /// `String(user.id)`.
    pub id: String,
    /// `user.role === 'admin'`.
    pub admin: bool,
}

/// An access token for one account and the generation of the sign-in it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub access: String,
    pub generation: String,
}

/// The token broker's refusal (core's `{ ok: false, status, error, code }`); all `None` when it could not
/// be asked.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Refusal {
    pub status: Option<u16>,
    pub error: Option<String>,
    pub code: Option<String>,
}

/// One Drive API request.
#[derive(Clone, Debug, PartialEq)]
pub struct Http {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// The most body bytes to keep.
    pub cap: usize,
}

/// Its answer: the status and the body, `capped` when there was more than `cap`.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub capped: bool,
}

/// What the tools need from the process they run in.
pub trait Backend: Send + Sync {
    fn now_ms(&self) -> f64;
    /// core's multipart boundary: `noevia` and 16 random hex digits.
    fn boundary(&self) -> String;
    fn api(&self) -> String;
    fn upload(&self) -> String;
    /// Whether this account's Drive is connected (core `drive.state().state === 'connected'`).
    fn connected(&self, user: &User) -> impl Future<Output = bool> + Send;
    fn token(
        &self,
        user: &User,
        rejected: Option<&str>,
    ) -> impl Future<Output = Result<Session, Refusal>> + Send;
    fn reject(&self, user: &User, generation: &str) -> impl Future<Output = ()> + Send;
    /// One request with `Authorization: Bearer <bearer>`; `Err` when it did not complete.
    fn send(&self, req: Http, bearer: &str) -> impl Future<Output = Result<Reply, ()>> + Send;
}

/// A failure the model is told about: core's `publicMessage`, or none (the generic text).
#[derive(Clone, Debug, PartialEq)]
struct Fail(Option<String>);

fn fail(message: impl Into<String>) -> Fail {
    Fail(Some(message.into()))
}

/// A JS `TypeError` or any other throw without a public message.
const THROWN: Fail = Fail(None);

/// What the tools keep between calls: the reads each reply made (#659).
#[derive(Debug, Default)]
pub struct State {
    reads: Mutex<ReadGate>,
}

fn gate<T>(state: &State, f: impl FnOnce(&mut ReadGate) -> T) -> T {
    let mut held = state.reads.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut held)
}

/// `String(v)`, or a throw (an object that cannot become a string).
fn s(v: &JValue) -> Result<String, Fail> {
    js_json::to_js_string(v).map_err(|_| THROWN)
}

/// core `clean(v, name, max)`.
fn clean(v: &JValue, name: &str, max: usize) -> Result<String, Fail> {
    let text = match v {
        JValue::Str(t) => {
            let units: Vec<u16> = t.encode_utf16().collect();
            String::from_utf16_lossy(policy_leaves::js_trim(&units))
        }
        _ => String::new(),
    };
    if text.is_empty() {
        return Err(fail(format!("{name} is required")));
    }
    if wtf16::len(&text) > max {
        return Err(fail(format!("{name} is too long")));
    }
    Ok(text)
}

/// core `quote(s)`: a Drive query literal.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// `/^[\w-]+$/` (also core's account id rule).
pub(crate) fn word_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// core `fileId(v)`.
fn file_id(v: &JValue) -> Result<String, Fail> {
    let id = clean(v, "fileId", 200)?;
    if !word_id(&id) {
        return Err(fail("fileId is not a Drive file id"));
    }
    Ok(id)
}

/// `encodeURIComponent(s)`: a lone surrogate throws (`URIError`).
fn enc(s: &str) -> Result<String, Fail> {
    if s.chars().any(|c| wtf16::lone_unit(c).is_some()) {
        return Err(THROWN);
    }
    Ok(crate::gdrive::encode_uri_component(s))
}

/// `Buffer.from(s, 'utf8')`: a lone surrogate is written as U+FFFD.
fn utf8(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect::<String>()
        .into_bytes()
}

/// JS `Number(v)`.
fn number(v: &JValue) -> Result<f64, Fail> {
    Ok(match v {
        JValue::Num(n) => *n,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Null => 0.0,
        JValue::Undefined => f64::NAN,
        JValue::Str(t) => server_account::util::js_number(t),
        other => server_account::util::js_number(&s(other)?),
    })
}

/// core `EXPORTS[mimeType]` (the key is `String(mimeType)`).
fn export_of(mime: &JValue) -> Result<Option<&'static str>, Fail> {
    Ok(match s(mime)?.as_str() {
        "application/vnd.google-apps.document" => Some("text/plain"),
        "application/vnd.google-apps.spreadsheet" => Some("text/csv"),
        "application/vnd.google-apps.presentation" => Some("text/plain"),
        _ => None,
    })
}

/// core `TEXTUAL.test(meta.mimeType || '')`.
fn textual(mime: &JValue) -> Result<bool, Fail> {
    let t = if mime.truthy() {
        s(mime)?
    } else {
        String::new()
    };
    const APPS: [&str; 10] = [
        "json",
        "xml",
        "javascript",
        "x-yaml",
        "yaml",
        "csv",
        "x-sh",
        "sql",
        "toml",
        "markdown",
    ];
    Ok(t.starts_with("text/")
        || t.strip_prefix("application/")
            .is_some_and(|rest| APPS.iter().any(|a| rest.starts_with(a))))
}

/// `` `${f[k]}` `` when `f[k]` is truthy (an array may be truthy and still write as `""`).
fn present(f: &JValue, k: &str) -> Result<Option<String>, Fail> {
    let v = f.get(k);
    if v.truthy() {
        Ok(Some(s(v)?))
    } else {
        Ok(None)
    }
}

/// core `line(f)` (`f.name` of `null` throws, as in JS).
fn line(f: &JValue) -> Result<String, Fail> {
    if f.is_nullish() {
        return Err(THROWN);
    }
    let mime = present(f, "mimeType")?.unwrap_or_else(|| "file".into());
    let size = present(f, "size")?
        .map(|v| format!(" · {v} bytes"))
        .unwrap_or_default();
    let changed = present(f, "modifiedTime")?
        .map(|v| format!(" · changed {v}"))
        .unwrap_or_default();
    Ok(format!(
        "- {} (id {}) · {mime}{size}{changed}",
        s(f.get("name"))?,
        s(f.get("id"))?
    ))
}

/// core `gdrive-broker.cjs` `brokerError(r)`: the text of a refused token.
fn refusal_text(r: &Refusal) -> String {
    let known = |c: &str| {
        match c {
        "disconnected" => Some("Google Drive is not connected."),
        "reconnect" => Some("Google no longer accepts this connection (access was removed in your Google account). Connect again."),
        "transient" => Some("Google could not be reached to refresh the connection."),
        "unavailable" => Some("Google Drive is unavailable right now. Try again in a moment."),
        _ => None,
    }
    };
    if let Some(text) = r.code.as_deref().and_then(known) {
        return text.into();
    }
    match (&r.error, r.status) {
        (Some(e), Some(_)) => js_json::js_slice(e, 300),
        _ => "Google Drive is unavailable right now. Try again in a moment.".into(),
    }
}

/// core `request(url, init)`: an authorized request. A 401 asks the broker once more past the refused
/// token; a second 401 drops that sign-in's cached token, and Google's answer stands.
async fn request<B: Backend>(b: &B, user: &User, req: Http) -> Result<Reply, Fail> {
    let token = b
        .token(user, None)
        .await
        .map_err(|r| fail(refusal_text(&r)))?;
    let first = b
        .send(req.clone(), &token.access)
        .await
        .map_err(|()| THROWN)?;
    if first.status != 401 {
        return Ok(first);
    }
    let next = b
        .token(user, Some(&token.access))
        .await
        .map_err(|r| fail(refusal_text(&r)))?;
    let again = b.send(req, &next.access).await.map_err(|()| THROWN)?;
    if again.status == 401 {
        b.reject(user, &next.generation).await;
    }
    Ok(again)
}

/// core `call(url, init)`: a JSON answer (`null` for a 204), or Drive's failure in core's words.
async fn call<B: Backend>(
    b: &B,
    user: &User,
    method: &'static str,
    url: String,
    content_type: Option<&str>,
    body: Option<Vec<u8>>,
) -> Result<JValue, Fail> {
    let headers = content_type
        .map(|c| vec![("Content-Type".to_string(), c.to_string())])
        .unwrap_or_default();
    let r = request(
        b,
        user,
        Http {
            method,
            url,
            headers,
            body,
            cap: JSON_CAP,
        },
    )
    .await?;
    if !(200..300).contains(&r.status) {
        return Err(fail(if r.status == 404 {
            "Google Drive has no such file, or noevia cannot see it.".to_string()
        } else {
            format!("Google Drive answered {}.", r.status)
        }));
    }
    if r.status == 204 {
        return Ok(JValue::Null);
    }
    if r.capped {
        return Err(THROWN);
    }
    wtf16::parse(&String::from_utf8_lossy(&r.body)).map_err(|_| THROWN)
}

async fn metadata<B: Backend>(b: &B, user: &User, id: &JValue) -> Result<JValue, Fail> {
    let id = file_id(id)?;
    let url = format!("{}/files/{}?fields={}", b.api(), enc(&id)?, enc(FIELDS)?);
    call(b, user, "GET", url, None, None).await
}

/// core `list(q, { limit })`: the `files` of one page (`r.files || []`).
async fn list<B: Backend>(b: &B, user: &User, q: &str, limit: &JValue) -> Result<JValue, Fail> {
    let n = number(limit)?;
    let n = if n.is_nan() || n == 0.0 { 10.0 } else { n };
    // Not NaN by now: `Math.min(25, Math.max(1, n))`.
    let n = n.clamp(1.0, 25.0);
    let url = format!(
        "{}/files?q={}&orderBy={}&pageSize={}&fields={}",
        b.api(),
        enc(q)?,
        enc("modifiedTime desc")?,
        js_json::number_to_string(n),
        enc(&format!("files({FIELDS})"))?
    );
    let r = call(b, user, "GET", url, None, None).await?;
    if r.is_nullish() {
        return Err(THROWN);
    }
    let files = r.get("files");
    Ok(if files.truthy() {
        files.clone()
    } else {
        JValue::Arr(Vec::new())
    })
}

/// `r.length ? r.map(line) : <none>` over what `list` gave.
fn lines(files: &JValue) -> Result<Option<Vec<String>>, Fail> {
    match files {
        JValue::Arr(items) if items.is_empty() => Ok(None),
        JValue::Arr(items) => items.iter().map(line).collect::<Result<_, _>>().map(Some),
        // A string or an object with a truthy `length` has no `map`: a TypeError.
        JValue::Str(_) => Err(THROWN),
        JValue::Obj(_) if files.get("length").truthy() => Err(THROWN),
        _ => Ok(None),
    }
}

/// The scope a read is remembered in (core `scopeOf`).
pub struct Scope<'a> {
    pub chat_key: Option<&'a str>,
    pub exchange_key: Option<&'a str>,
}

/// core `remember(user, scope, id, version, complete)`: `id` and `version` as JS values.
fn remember(
    state: &State,
    user: &User,
    scope: &str,
    id: &JValue,
    version: &JValue,
    complete: bool,
    now: f64,
) -> Result<(), Fail> {
    if version.is_nullish() || !id.truthy() {
        return Ok(());
    }
    let (id, version) = (s(id)?, s(version)?);
    gate(state, |g| {
        g.remember(&user.id, scope, &id, Some(&version), complete, now as u64)
    });
    Ok(())
}

/// The calling account, or the text core's `forUser` throws (`Sign in first.`, or a plain error for an
/// id outside `[\w-]`).
fn account(user: Option<&User>) -> Result<&User, Fail> {
    let user = user
        .filter(|u| !u.id.is_empty())
        .ok_or(fail("Sign in first."))?;
    if !word_id(&user.id) {
        return Err(THROWN);
    }
    Ok(user)
}

fn reply_of(f: Fail) -> String {
    drive_tools::error_reply(Some(f.0.as_deref().unwrap_or(UNREACHABLE)))
}

/// core `execute(user, name, args, { chatKey, exchangeKey })` with the chat's tool-result `cap`.
pub async fn execute<B: Backend>(
    b: &B,
    state: &State,
    user: Option<&User>,
    name: &str,
    args: &JValue,
    scope: Scope<'_>,
    cap: usize,
) -> String {
    match run(b, state, user, name, args, scope, cap).await {
        Ok(text) => text,
        Err(f) => reply_of(f),
    }
}

async fn run<B: Backend>(
    b: &B,
    state: &State,
    user: Option<&User>,
    name: &str,
    args: &JValue,
    scope: Scope<'_>,
    cap: usize,
) -> Result<String, Fail> {
    let user = account(user)?;
    if !b.connected(user).await {
        return Ok(NOT_CONNECTED.into());
    }
    let scope = drive_tools::scope_key(scope.exchange_key, scope.chat_key);
    let Some(tool) = Tool::from_name(name) else {
        return Ok(format!("ERROR: unknown Drive tool {name}"));
    };
    match tool {
        Tool::SearchFiles => {
            let q = clean(args.get("query"), "query", 200)?;
            // `query.trim()`: the same text `clean` gave (it is a string by now).
            let q = format!(
                "trashed=false and (name contains {} or fullText contains {})",
                quote(&q),
                quote(&q)
            );
            let files = list(b, user, &q, args.get("limit")).await?;
            Ok(match lines(&files)? {
                Some(l) => format!("Found {}:\n{}", l.len(), l.join("\n")),
                None => drive_tools::NO_MATCHES.into(),
            })
        }
        Tool::ListRecent => {
            let files = list(b, user, "trashed=false", args.get("limit")).await?;
            Ok(match lines(&files)? {
                Some(l) => l.join("\n"),
                None => drive_tools::NO_FILES.into(),
            })
        }
        Tool::GetMetadata => {
            let f = metadata(b, user, args.get("fileId")).await?;
            let first = line(&f)?;
            Ok(match present(&f, "webViewLink")? {
                Some(link) => format!("{first}\nLink: {link}"),
                None => first,
            })
        }
        Tool::ReadFile => {
            let meta = metadata(b, user, args.get("fileId")).await?;
            if meta.is_nullish() {
                return Err(THROWN);
            }
            let mime = meta.get("mimeType");
            let export = export_of(mime)?;
            if export.is_none() && !textual(mime)? {
                let kind = if mime.truthy() {
                    s(mime)?
                } else {
                    "a binary file".into()
                };
                return Err(fail(format!(
                    "{} is {kind}; only text files and Google Docs, Sheets and Slides can be read.",
                    s(meta.get("name"))?
                )));
            }
            let id = s(meta.get("id"))?;
            let (url, headers) = match export {
                Some(as_type) => (
                    format!(
                        "{}/files/{}/export?mimeType={}",
                        b.api(),
                        enc(&id)?,
                        enc(as_type)?
                    ),
                    Vec::new(),
                ),
                None => (
                    format!("{}/files/{}?alt=media", b.api(), enc(&id)?),
                    vec![("Range".to_string(), format!("bytes=0-{MAX_READ}"))],
                ),
            };
            let r = request(
                b,
                user,
                Http {
                    method: "GET",
                    url,
                    headers,
                    body: None,
                    cap: MAX_READ,
                },
            )
            .await?;
            if !(200..300).contains(&r.status) && r.status != 206 {
                return Err(fail(format!("Google Drive answered {}.", r.status)));
            }
            let text = String::from_utf8_lossy(&r.body);
            let shaped = drive_tools::shape_read(&s(meta.get("name"))?, &text, r.capped, cap);
            remember(
                state,
                user,
                &scope,
                meta.get("id"),
                meta.get("version"),
                shaped.complete,
                b.now_ms(),
            )?;
            Ok(shaped.text)
        }
        Tool::CreateFile => {
            let mime = match args.get("mimeType") {
                JValue::Str(m) if creatable(m) => m.clone(),
                _ => "text/plain".into(),
            };
            let body = match args.get("content") {
                JValue::Str(c) => c.clone(),
                _ => String::new(),
            };
            let boundary = b.boundary();
            let file_name = clean(args.get("name"), "name", 255)?;
            let head = wtf16::stringify(&JValue::obj([
                ("name", JValue::Str(file_name)),
                ("mimeType", JValue::Str(mime.clone())),
            ]))
            .ok_or(THROWN)?;
            let mut multipart = format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{head}\r\n--{boundary}\r\nContent-Type: {mime}; charset=UTF-8\r\n\r\n").into_bytes();
            multipart.extend_from_slice(&utf8(&body));
            multipart.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
            let url = format!(
                "{}/files?uploadType=multipart&fields={}",
                b.upload(),
                enc(FIELDS)?
            );
            let content_type = format!("multipart/related; boundary={boundary}");
            let f = call(b, user, "POST", url, Some(&content_type), Some(multipart)).await?;
            if f.is_nullish() {
                return Err(THROWN);
            }
            remember(
                state,
                user,
                &scope,
                f.get("id"),
                f.get("version"),
                true,
                b.now_ms(),
            )?;
            let created = format!("Created {} (id {})", s(f.get("name"))?, s(f.get("id"))?);
            Ok(match present(&f, "webViewLink")? {
                Some(link) => format!("{created}: {link}"),
                None => created,
            })
        }
        Tool::UpdateFile => {
            let id = args.get("fileId").as_str();
            let checked = gate(state, |g| {
                drive_tools::check_update(g, &user.id, &scope, id, b.now_ms() as u64)
            });
            let expect = match checked {
                Err(refusal) => return Ok(refusal.into()),
                Ok(UpdateCheck::ExpectVersion(v)) => Some(v),
                Ok(UpdateCheck::NotDriveId) => None,
            };
            let meta = metadata(b, user, args.get("fileId")).await?;
            if meta.is_nullish() {
                return Err(THROWN);
            }
            let mime = meta.get("mimeType");
            if export_of(mime)?.is_some() || !textual(mime)? {
                return Err(fail(format!(
                    "{} is not a plain text file, so noevia will not overwrite it.",
                    s(meta.get("name"))?
                )));
            }
            if let Some(expect) = expect {
                let version = meta.get("version");
                if matches!(version, JValue::Undefined) || s(version)? != expect {
                    return Err(fail(format!("{} changed in Google Drive after it was read in this chat, so it was not overwritten. Nothing was changed. Read it again and make the edit on the current text.", s(meta.get("name"))?)));
                }
            }
            let content = match args.get("content") {
                JValue::Str(c) => utf8(c),
                _ => Vec::new(),
            };
            let url = format!(
                "{}/files/{}?uploadType=media&fields={}",
                b.upload(),
                enc(&s(meta.get("id"))?)?,
                enc(FIELDS)?
            );
            let content_type = format!("{}; charset=UTF-8", s(mime)?);
            let f = call(b, user, "PATCH", url, Some(&content_type), Some(content)).await?;
            if f.is_nullish() {
                return Err(THROWN);
            }
            remember(
                state,
                user,
                &scope,
                f.get("id"),
                f.get("version"),
                true,
                b.now_ms(),
            )?;
            Ok(drive_tools::updated_reply(
                &s(f.get("name"))?,
                &s(f.get("id"))?,
            ))
        }
        Tool::TrashFile => {
            let id = file_id(args.get("fileId"))?;
            let url = format!("{}/files/{}?fields={}", b.api(), enc(&id)?, enc(FIELDS)?);
            let f = call(
                b,
                user,
                "PATCH",
                url,
                Some("application/json"),
                Some(br#"{"trashed":true}"#.to_vec()),
            )
            .await?;
            if f.is_nullish() {
                return Err(THROWN);
            }
            Ok(drive_tools::trashed_reply(&s(f.get("name"))?))
        }
    }
}

/// core create's `/^text\/[\w.+-]+$|^application\/json$/`.
fn creatable(mime: &str) -> bool {
    mime == "application/json"
        || mime.strip_prefix("text/").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
        })
}

/// core `describeTarget(user, name, rawArgs)`: `null`, `{ target, kind }` or `{ error }`, as the JSON the
/// approval card is built from.
pub async fn describe_target<B: Backend>(
    b: &B,
    user: Option<&User>,
    name: &str,
    raw: &JValue,
) -> JValue {
    let Some(tool) = Tool::from_name(name).filter(|t| t.is_write()) else {
        return JValue::Null;
    };
    let parsed;
    let args = match raw {
        JValue::Str(text) => {
            let text = if text.is_empty() { "{}" } else { text.as_str() };
            match wtf16::parse(text) {
                Ok(v) => {
                    parsed = v;
                    &parsed
                }
                Err(_) => return JValue::Null,
            }
        }
        other => other,
    };
    if !matches!(args, JValue::Obj(_)) {
        return JValue::Null;
    }
    let error = |e: String| JValue::obj([("error", JValue::Str(e))]);
    let trimmed = |k: &str| match args.get(k) {
        JValue::Str(t) => {
            let units: Vec<u16> = t.encode_utf16().collect();
            String::from_utf16_lossy(policy_leaves::js_trim(&units))
        }
        _ => String::new(),
    };
    if tool == Tool::CreateFile {
        let name = trimmed("name");
        if name.is_empty() {
            return JValue::Null;
        }
        return JValue::obj([
            ("target", JValue::Str(wtf16::slice(&name, 255))),
            ("kind", JValue::Str("drive-new".into())),
        ]);
    }
    if !matches!(tool, Tool::UpdateFile | Tool::TrashFile) {
        return JValue::Null;
    }
    let file_id = trimmed("fileId");
    if !(word_id(&file_id) && file_id.len() <= 200) {
        let shown = wtf16::stringify(&JValue::Str(wtf16::slice(&file_id, 200))).unwrap_or_default();
        return error(format!("{shown}{NOT_A_FILE_ID_TAIL}"));
    }
    let generic =
        |f: Fail| error(f.0.unwrap_or_else(|| "Google Drive could not be reached".into()));
    let user = match account(user) {
        Ok(u) => u,
        Err(f) => return generic(f),
    };
    if !b.connected(user).await {
        return error("Google Drive is not connected for this account".into());
    }
    let meta = match metadata(b, user, &JValue::Str(file_id)).await {
        Ok(m) if !m.is_nullish() => m,
        Ok(_) => return generic(THROWN),
        Err(f) => return generic(f),
    };
    match (s(meta.get("name")), s(meta.get("id"))) {
        (Ok(n), Ok(i)) => JValue::obj([
            ("target", JValue::Str(drive_tools::existing_target(&n, &i))),
            ("kind", JValue::Str("drive".into())),
        ]),
        _ => generic(THROWN),
    }
}

// ── in the running front ──

/// [`Backend`] over this front's Drive broker.
struct AppBackend {
    drive: std::sync::Arc<crate::gdrive::Drive>,
}

impl AppBackend {
    fn slot(&self, user: &User) -> Result<crate::gdrive::Slot, crate::gdrive::Fail> {
        self.drive.slot_for(&user.id, user.admin)
    }
}

/// The broker's failure as core's client saw it over the internal store.
fn refusal_of(e: &crate::gdrive::Fail) -> Refusal {
    Refusal {
        status: Some(e.status),
        error: Some(e.message.clone()),
        code: e.code.map(str::to_owned),
    }
}

impl Backend for AppBackend {
    fn now_ms(&self) -> f64 {
        crate::gdrive::now_ms() as f64
    }
    fn boundary(&self) -> String {
        format!(
            "noevia{}",
            crate::gdrive::hex_of(&crate::gdrive::random::<8>())
        )
    }
    fn api(&self) -> String {
        self.drive.settings.api_base.clone()
    }
    fn upload(&self) -> String {
        self.drive.settings.upload_base.clone()
    }
    async fn connected(&self, user: &User) -> bool {
        self.slot(user)
            .is_ok_and(|slot| self.drive.state(&slot).state == "connected")
    }
    async fn token(&self, user: &User, rejected: Option<&str>) -> Result<Session, Refusal> {
        let slot = self.slot(user).map_err(|e| refusal_of(&e))?;
        match self.drive.token(&slot, rejected).await {
            Ok((access, _, generation)) => Ok(Session {
                access: access.to_string(),
                generation,
            }),
            Err(e) => Err(refusal_of(&e)),
        }
    }
    async fn reject(&self, user: &User, generation: &str) {
        if let Ok(slot) = self.slot(user) {
            self.drive.reject(&slot, generation);
        }
    }
    async fn send(&self, req: Http, bearer: &str) -> Result<Reply, ()> {
        self.drive.tool_send(req, bearer).await
    }
}

/// `POST /v1/gdrive/tool { user, admin, name, arguments, chatKey?, exchangeKey?, cap? }` -> `{ text }` and
/// `POST /v1/gdrive/target { user, admin, name, arguments }` -> `{ target }` (core `describeTarget`'s
/// value: `null`, `{ target, kind }` or `{ error }`). `user` is the account id or `null`; `admin` has been
/// checked against the users table by the caller. 404 while `NOEVIA_RUST_DRIVE_TOOLS` is off.
pub async fn op(
    drive: std::sync::Arc<crate::gdrive::Drive>,
    op: &str,
    user: Option<User>,
    body: &JValue,
) -> Result<String, &'static str> {
    let name = match body.get("name") {
        JValue::Str(n) if !n.is_empty() && n.len() <= 256 => n.clone(),
        _ => return Err("invalid name"),
    };
    let key = |k: &str| match body.get(k) {
        JValue::Str(v) if v.len() <= MAX_KEY => Ok(Some(v.clone())),
        JValue::Null | JValue::Undefined => Ok(None),
        _ => Err("invalid scope"),
    };
    let b = AppBackend {
        drive: std::sync::Arc::clone(&drive),
    };
    let args = body.get("arguments");
    if op == "target" {
        let target = describe_target(&b, user.as_ref(), &name, args).await;
        return answer(JValue::obj([("target", target)]));
    }
    let cap = match body.get("cap") {
        JValue::Num(n) if n.fract() == 0.0 && (1.0..=1_048_576.0).contains(n) => *n as usize,
        JValue::Undefined => 8000,
        _ => return Err("invalid cap"),
    };
    let (chat, exchange) = (key("chatKey")?, key("exchangeKey")?);
    let scope = Scope {
        chat_key: chat.as_deref(),
        exchange_key: exchange.as_deref(),
    };
    let text = execute(&b, &drive.tools, user.as_ref(), &name, args, scope, cap).await;
    answer(JValue::obj([("text", JValue::Str(text))]))
}

/// The answer as JSON text, lone surrogates written as `\udxxx` (as `JSON.stringify` writes them).
fn answer(v: JValue) -> Result<String, &'static str> {
    wtf16::stringify(&v).ok_or("unwritable answer")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_surrogate_is_written_as_core_writes_it() {
        let text = wtf16::parse(r#""qa \ud83d é""#).unwrap();
        let JValue::Str(text) = text else {
            unreachable!()
        };
        // Buffer.from(s, 'utf8'): U+FFFD for the lone half.
        assert_eq!(utf8(&text), "qa \u{FFFD} é".as_bytes());
        // encodeURIComponent throws on it; a whole character is encoded.
        assert_eq!(enc(&text), Err(THROWN));
        assert_eq!(enc("qa é").unwrap(), "qa%20%C3%A9");
        // `.length` counts the half as one unit.
        assert_eq!(wtf16::len(&text), 6);
    }
}
