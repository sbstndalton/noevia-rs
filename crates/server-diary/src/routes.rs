//! core `routes/diary.cjs` for the five routes that are a proxy and a policy, `workspace-export.cjs`
//! and `workspace-import.cjs`:
//!
//! | route | core |
//! |---|---|
//! | any method `/api/diary/source` | the month list, an empty one when the sidecar fails |
//! | any method `/api/diary/today` | one day or month of the log and the standing text |
//! | `POST /api/diary/storage-import` | 4 KiB body, 300 s |
//! | `GET /api/diary/workspace-export` | 300 s, 272 MiB, streamed |
//! | `POST /api/diary/workspace-import` | 32 MiB body, 120 s, 24 MiB reply |
//!
//! Each is gated on the person's Diary add-on first (404), then on its method (405). The
//! caller has already passed the request gate (session or token, origin, CSRF) and hands over the
//! validated [`Identity`]; this module never reads credentials from a request.
//!
//! `GET /api/diary/storage-status` and `GET`/`POST /api/diary/exchanges` (sbstndalton/noevia#2206) are
//! here too; the connector credentials and the file bridge are [`crate::connector`].
//!
//! Kept on Node, with the reason, in contracts/http/README.md: `POST /api/diary/local-exchange` (the
//! shared model throttle and the inference gate, both Node memory).

use crate::body::{ChannelBody, StreamError};
use crate::client::{parse_or_text, read_capped, CallError, Capped, JsonReply, Sidecar};
use crate::config::DiaryConfig;
use crate::exchanges;
use crate::tenant::{workspace_dir, Content, StorageIdentity, Tenant};
use bytes::Bytes;
use http_body::Body;
use http_body_util::BodyExt;
use js_json::{wtf16, JValue};
use reqwest::Method;
use server_auth::Identity;
use server_store::Store;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{timeout_at, Instant};
use url::Url;

/// Node's caps and deadlines for these routes. [`Limits::default`] is exactly core's; tests lower
/// them to reach a cap or a deadline without moving 272 MiB or waiting five minutes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// `fetchJson(..., 15000)` for the month list and the day.
    pub read_timeout: Duration,
    /// `diaryFetchJson(..., 300000)` for storage-import.
    pub storage_import_timeout: Duration,
    /// `readBody(req, 4096)` for storage-import.
    pub storage_import_body: usize,
    /// How long a small body may take to arrive. Core set none of its own (the server's request
    /// timeout, 20 minutes, applied); a body of 4 KiB gets the import's two minutes here.
    pub small_body_deadline: Duration,
    /// workspace-export.cjs: the one deadline for the whole download, and its cap (272 MiB).
    pub export_deadline: Duration,
    pub export_cap: usize,
    /// `readCappedJson(upstream, 64 * 1024)` for an export's failure body.
    pub export_error_cap: usize,
    /// workspace-import.cjs `MAX_UPLOAD` (32 MiB), `MAX_REPLY` (3 x the 8 MiB manifest cap, #920)
    /// and the one deadline for upload and reply.
    pub import_max_upload: usize,
    pub import_max_reply: usize,
    pub import_deadline: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            read_timeout: Duration::from_secs(15),
            storage_import_timeout: Duration::from_secs(300),
            storage_import_body: 4096,
            small_body_deadline: Duration::from_secs(120),
            export_deadline: Duration::from_secs(300),
            export_cap: 272 * 1024 * 1024,
            export_error_cap: 64 * 1024,
            import_max_upload: 32 * 1024 * 1024,
            import_max_reply: 3 * 8 * 1024 * 1024,
            import_deadline: Duration::from_secs(120),
        }
    }
}

/// `readObject(req, 4096)` of the exchanges' dismiss.
const EXCHANGE_BODY: usize = 4096;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub const EXPORT_FAILED: &str =
    "Workspace export could not finish. Retry when storage is available.";
pub const IMPORT_UNAVAILABLE: &str =
    "Import response unavailable. Retry with the same file and folder to safely check whether it completed.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Source,
    Today,
    StorageImport,
    WorkspaceExport,
    WorkspaceImport,
    /// `GET /api/diary/storage-status` (sbstndalton/noevia#2206).
    StorageStatus,
    /// `GET` and `POST /api/diary/exchanges` (sbstndalton/noevia#2206).
    Exchanges,
}

impl Route {
    pub const PATHS: [&'static str; 7] = [
        "/api/diary/source",
        "/api/diary/today",
        "/api/diary/storage-import",
        "/api/diary/workspace-export",
        "/api/diary/workspace-import",
        "/api/diary/storage-status",
        "/api/diary/exchanges",
    ];

    /// The exact pathname only: core compares `p === '/api/diary/...'`.
    pub fn from_path(path: &str) -> Option<Self> {
        match path {
            "/api/diary/source" => Some(Route::Source),
            "/api/diary/today" => Some(Route::Today),
            "/api/diary/storage-import" => Some(Route::StorageImport),
            "/api/diary/workspace-export" => Some(Route::WorkspaceExport),
            "/api/diary/workspace-import" => Some(Route::WorkspaceImport),
            "/api/diary/storage-status" => Some(Route::StorageStatus),
            "/api/diary/exchanges" => Some(Route::Exchanges),
            _ => None,
        }
    }
}

/// The hold on an account's AUTOMATIC storage calls, as the status view shows it (core
/// `storage-latch.cjs` `status`). The latch is a durable file both processes use (noevia-server
/// reads it with crates/server-projects `storage_latch`), so this crate only asks.
pub trait StorageLatch: Send + Sync {
    /// The hold for this account's saved connection, or none. Blocking (SQLite). A latch that
    /// cannot be read is no hold: the status is then as the sidecar gave it, as core's was.
    fn held(&self, user_id: &str, storage: &StorageIdentity<'_>) -> Option<Held>;
}

/// `{ kind, nextAttemptAt }`: `login` or `throttled`, and when the next automatic attempt is due
/// (epoch ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    pub kind: &'static str,
    pub next_attempt_at: i64,
}

/// Who is asking, from the gate and the front's store.
#[derive(Clone)]
pub struct Ctx {
    pub identity: Identity,
    pub store: Arc<Store>,
    pub data_dir: PathBuf,
    /// The storage-login latch, for the status route.
    pub latch: Option<Arc<dyn StorageLatch>>,
    /// The container start (NOEVIA_START_ID): the set of running exchanges Node published belongs to
    /// the start that wrote it.
    pub start_id: Option<String>,
}

/// The request line and the headers this module reads.
pub struct Request {
    pub method: String,
    /// `req.url` as received: the path and the query.
    pub target: String,
    /// The `Content-Length` header text, if any.
    pub content_length: Option<String>,
}

pub enum ReplyBody {
    /// `json(res, status, body)`: `application/json`, `Cache-Control: no-store`.
    Json(String),
    /// The workspace archive; the status is 200 and the headers are fixed by the caller.
    Zip(ChannelBody),
}

pub struct Reply {
    pub status: u16,
    pub body: ReplyBody,
}

fn json_reply(status: u16, value: &JValue) -> Reply {
    Reply {
        status,
        body: ReplyBody::Json(wtf16::stringify(value).unwrap_or_else(|| "null".to_owned())),
    }
}

fn error_reply(status: u16, message: &str) -> Reply {
    json_reply(
        status,
        &JValue::obj([("error", JValue::Str(message.to_owned()))]),
    )
}

/// What core's `errorResponse` made of an error with no status: a generic 500.
fn internal() -> Reply {
    error_reply(500, "Internal error")
}

fn disabled() -> Reply {
    error_reply(404, "Diary add-on is disabled")
}

fn not_allowed() -> Reply {
    error_reply(405, "Method not allowed")
}

/// `encodeURIComponent` (the text came from a percent-decoder, so it is valid UTF-8).
fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `new URL(req.url, base)`: `.search` and `.searchParams.get('month')`.
struct Query {
    search: String,
    month: Option<String>,
    day: Option<String>,
}

fn query_of(target: &str) -> Query {
    let parsed = Url::parse("http://h")
        .ok()
        .and_then(|base| Url::options().base_url(Some(&base)).parse(target).ok());
    match parsed {
        Some(url) => Query {
            search: match url.query() {
                Some(q) if !q.is_empty() => format!("?{q}"),
                _ => String::new(),
            },
            month: url
                .query_pairs()
                .find(|(k, _)| k == "month")
                .map(|(_, v)| v.into_owned()),
            day: url
                .query_pairs()
                .find(|(k, _)| k == "day")
                .map(|(_, v)| v.into_owned()),
        },
        None => Query {
            search: String::new(),
            month: None,
            day: None,
        },
    }
}

/// `value || fallback`
fn truthy_or<'a>(value: &'a JValue, fallback: &'a JValue) -> &'a JValue {
    if value.truthy() {
        value
    } else {
        fallback
    }
}

/// `r.body?.detail || fallback` for a sidecar error reply.
fn detail_or(body: Option<&JValue>, fallback: &str) -> JValue {
    let fallback = JValue::Str(fallback.to_owned());
    match body {
        Some(b) => truthy_or(b.get("detail"), &fallback).clone(),
        None => fallback,
    }
}

/// Cheap to clone (the HTTP client and the configuration are shared), so work that must not be
/// abandoned when a client goes away can be handed to its own task.
#[derive(Clone)]
pub struct Diary {
    sidecar: Sidecar,
    limits: Limits,
}

impl Diary {
    pub fn new(config: DiaryConfig) -> Result<Self, String> {
        Self::with_limits(config, Limits::default())
    }

    pub fn with_limits(config: DiaryConfig, limits: Limits) -> Result<Self, String> {
        Ok(Self {
            sidecar: Sidecar::new(config)?,
            limits,
        })
    }

    pub fn sidecar(&self) -> &Sidecar {
        &self.sidecar
    }

    /// The tenant for this request (SQLite and files, so off the async threads).
    async fn tenant(&self, ctx: &Ctx) -> Result<Arc<Tenant>, CallError> {
        let identity = ctx.identity.clone();
        let store = Arc::clone(&ctx.store);
        let data_dir = ctx.data_dir.clone();
        let config = self.sidecar.config().clone();
        tokio::task::spawn_blocking(move || Tenant::load(&identity, &store, &data_dir, &config))
            .await
            .map_err(|_| CallError::Transport)?
            .map(Arc::new)
            .map_err(CallError::from)
    }

    /// Answers `route`. `body` is the request body, read only where core read it.
    pub async fn serve<B>(&self, route: Route, ctx: Ctx, request: Request, body: B) -> Reply
    where
        B: Body<Data = Bytes> + Unpin + Send,
    {
        let method = request.method.as_str();
        // core mounts the exchanges on GET and POST only; any other method matches nothing and falls
        // through to the unknown-path answer, before the add-on is looked at.
        if route == Route::Exchanges && !matches!(method, "GET" | "POST") {
            return error_reply(404, "not found");
        }
        if !ctx.identity.user.diary_enabled {
            return disabled();
        }
        let query = query_of(&request.target);
        match route {
            Route::Source => self.source(&ctx).await,
            Route::Today => self.today(&ctx, query.month.as_deref()).await,
            Route::StorageImport => {
                if method != "POST" {
                    return not_allowed();
                }
                self.storage_import(&ctx, body).await
            }
            Route::WorkspaceExport => {
                if method != "GET" {
                    return not_allowed();
                }
                self.workspace_export(&ctx).await
            }
            Route::WorkspaceImport => {
                if method != "POST" {
                    return not_allowed();
                }
                self.workspace_import(&ctx, &request, &query.search, body)
                    .await
            }
            Route::StorageStatus => {
                if method != "GET" {
                    return not_allowed();
                }
                self.storage_status(&ctx).await
            }
            Route::Exchanges => self.exchanges(&ctx, method, query.day, body).await,
        }
    }

    /// `GET /api/diary/storage-status`: the sidecar's view of the person's storage, and, while a
    /// refused or throttled login holds this account's automatic storage calls (the backup among
    /// them), the hold and when the next attempt is due (#1589), instead of looking healthy.
    async fn storage_status(&self, ctx: &Ctx) -> Reply {
        let reply = async {
            let tenant = self.tenant(ctx).await?;
            let reply = self
                .sidecar
                .fetch_json(
                    &tenant,
                    Method::GET,
                    "/api/storage-status",
                    None,
                    Duration::from_secs(60),
                )
                .await?;
            // Only a reply that is an object can carry the hold.
            let held = match (&reply.body, reply.ok, &ctx.latch) {
                (Some(JValue::Obj(_)), true, Some(latch)) => {
                    let (latch, tenant, user) = (
                        Arc::clone(latch),
                        Arc::clone(&tenant),
                        ctx.identity.user.id.clone(),
                    );
                    tokio::task::spawn_blocking(move || {
                        latch.held(&user, &tenant.storage_identity())
                    })
                    .await
                    .ok()
                    .flatten()
                }
                _ => None,
            };
            Ok::<_, CallError>((reply, held))
        }
        .await;
        match reply {
            Ok((r, held)) if r.ok => {
                let body = r.body.unwrap_or(JValue::Undefined);
                let body = match (held, body) {
                    (Some(held), JValue::Obj(mut fields)) => {
                        wtf16::set(
                            &mut fields,
                            "automaticStorage",
                            JValue::obj([
                                ("held", JValue::Bool(true)),
                                ("kind", JValue::Str(held.kind.to_owned())),
                                ("nextAttemptAt", JValue::Num(held.next_attempt_at as f64)),
                            ]),
                        );
                        JValue::Obj(fields)
                    }
                    (_, body) => body,
                };
                json_reply(r.status, &body)
            }
            Ok((r, _)) => json_reply(
                r.status,
                &JValue::obj([(
                    "error",
                    detail_or(r.body.as_ref(), "Diary storage request failed"),
                )]),
            ),
            Err(_) => internal(),
        }
    }

    /// `GET` and `POST /api/diary/exchanges`: the Diary tab's recovery records for a day, and the
    /// `dismiss` of an uncertain or failed one so it stops raising the banner. Only the record's own
    /// `acknowledgedAt` changes; the diary is never touched.
    async fn exchanges<B>(&self, ctx: &Ctx, method: &str, day: Option<String>, body: B) -> Reply
    where
        B: Body<Data = Bytes> + Unpin + Send,
    {
        let Some(dir) = workspace_dir(&ctx.data_dir, &ctx.identity.user.id) else {
            return internal();
        };
        let start = ctx.start_id.clone();
        let failed = |fault: exchanges::Fault| error_reply(fault.status, fault.message);
        if method == "GET" {
            let listed = tokio::task::spawn_blocking(move || {
                exchanges::list(&dir, day.as_deref(), start.as_deref())
            })
            .await;
            return match listed {
                Ok(Ok(rows)) => json_reply(200, &JValue::obj([("exchanges", JValue::Arr(rows))])),
                Ok(Err(fault)) => failed(fault),
                Err(_) => error_reply(500, "Could not read recovery records"),
            };
        }
        let deadline = Instant::now() + self.limits.small_body_deadline;
        let raw = match read_body(body, EXCHANGE_BODY, deadline).await {
            Ok(bytes) => bytes,
            Err(BodyFault::TooLarge) => return error_reply(413, "Request exceeds size limit"),
            Err(_) => return internal(),
        };
        let text = String::from_utf8_lossy(&raw);
        let parsed = if text.is_empty() {
            JValue::Obj(Vec::new())
        } else {
            match wtf16::parse(&text) {
                Ok(v) => v,
                Err(wtf16::Refusal::Invalid) => return error_reply(400, "invalid JSON"),
                Err(wtf16::Refusal::Unsupported) => return internal(),
            }
        };
        if !parsed.is_object() {
            return error_reply(400, "request body must be a JSON object");
        }
        if parsed.get("action").as_str() != Some("dismiss") {
            return error_reply(400, "Unknown action");
        }
        let now = now_ms();
        let done = tokio::task::spawn_blocking(move || {
            exchanges::acknowledge(
                &dir,
                parsed.get("day"),
                parsed.get("id"),
                start.as_deref(),
                now,
            )
        })
        .await;
        match done {
            Ok(Ok(row)) => json_reply(
                200,
                &JValue::obj([(
                    "exchange",
                    JValue::obj([
                        ("id", row.get("id").clone()),
                        ("day", row.get("day").clone()),
                        ("state", row.get("state").clone()),
                        ("acknowledgedAt", row.get("acknowledgedAt").clone()),
                    ]),
                )]),
            ),
            Ok(Err(fault)) => failed(fault),
            Err(_) => error_reply(500, "Could not update the recovery record"),
        }
    }

    /// `corpusSource.listMonths()`: the real months, sorted; any failure is an empty list.
    async fn source(&self, ctx: &Ctx) -> Reply {
        let months = self.list_months(ctx).await.unwrap_or_default();
        json_reply(
            200,
            &JValue::obj([
                ("source", JValue::Str("sidecar".to_owned())),
                ("months", JValue::Arr(months)),
            ]),
        )
    }

    async fn list_months(&self, ctx: &Ctx) -> Result<Vec<JValue>, CallError> {
        let reply = self.read_json(ctx, "/api/months").await?;
        let Some(JValue::Arr(items)) = reply
            .body
            .as_ref()
            .filter(|_| reply.ok)
            .map(|b| b.get("months").clone())
        else {
            return Ok(Vec::new());
        };
        // `/^\d{4}-\d{2}$/`: ASCII digits only, nothing after the end.
        let month_shaped = |id: &str| {
            let b = id.as_bytes();
            b.len() == 7
                && b.iter().enumerate().all(|(i, c)| {
                    if i == 4 {
                        *c == b'-'
                    } else {
                        c.is_ascii_digit()
                    }
                })
        };
        let mut months: Vec<(String, JValue)> = items
            .iter()
            .filter_map(|m| {
                let id = m.get("id").as_str().filter(|id| month_shaped(id))?;
                let id_value = JValue::Str(id.to_owned());
                // `label: m.label || m.id`
                let label = truthy_or(m.get("label"), &id_value).clone();
                Some((
                    id.to_owned(),
                    JValue::obj([("id", id_value), ("label", label)]),
                ))
            })
            .collect();
        // localeCompare over these ASCII ids is their byte order; the sort is stable.
        months.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(months.into_iter().map(|(_, v)| v).collect())
    }

    /// `corpusSource.readMonth(month)`: a failure is core's thrown `Error('sidecar <status>')`, a 500.
    async fn today(&self, ctx: &Ctx, month: Option<&str>) -> Reply {
        let path = match month.filter(|m| !m.is_empty()) {
            Some(m) => format!("/api/day?month={}", encode_uri_component(m)),
            None => "/api/day".to_owned(),
        };
        let reply = match self.read_json(ctx, &path).await {
            Ok(r) if r.ok => r,
            _ => return internal(),
        };
        let body = reply.body.unwrap_or(JValue::Undefined);
        let empty = JValue::Str(String::new());
        // `(body.log ?? body.today_log) || ''` and `body.standing || ''`
        let log = match body.get("log") {
            JValue::Undefined | JValue::Null => body.get("today_log"),
            other => other,
        };
        json_reply(
            200,
            &JValue::obj([
                ("todayLog", truthy_or(log, &empty).clone()),
                ("standing", truthy_or(body.get("standing"), &empty).clone()),
            ]),
        )
    }

    async fn read_json(&self, ctx: &Ctx, path: &str) -> Result<JsonReply, CallError> {
        let tenant = self.tenant(ctx).await?;
        self.sidecar
            .fetch_json(&tenant, Method::GET, path, None, self.limits.read_timeout)
            .await
    }

    async fn storage_import<B>(&self, ctx: &Ctx, body: B) -> Reply
    where
        B: Body<Data = Bytes> + Unpin + Send,
    {
        let deadline = Instant::now() + self.limits.small_body_deadline;
        let raw = match read_body(body, self.limits.storage_import_body, deadline).await {
            Ok(bytes) => bytes,
            Err(BodyFault::TooLarge) => return error_reply(413, "Request exceeds size limit"),
            Err(_) => return internal(),
        };
        // `readBody` decodes UTF-8 (bad bytes become U+FFFD) and fetch sends that string.
        let text = Bytes::from(String::from_utf8_lossy(&raw).into_owned());
        // The import is the sidecar's to finish: a client that goes away must not cut the call off
        // half-sent (core never tied it to the client's connection), so it runs on its own task.
        let (this, owned) = (self.clone(), ctx.clone());
        let reply = tokio::spawn(async move {
            let tenant = this.tenant(&owned).await?;
            this.sidecar
                .fetch_json(
                    &tenant,
                    Method::POST,
                    "/api/storage-import",
                    Some(text),
                    this.limits.storage_import_timeout,
                )
                .await
        })
        .await
        .unwrap_or(Err(CallError::Transport));
        match reply {
            Ok(r) if r.ok => json_reply(r.status, &r.body.unwrap_or(JValue::Undefined)),
            Ok(r) => json_reply(
                r.status,
                &JValue::obj([(
                    "error",
                    detail_or(r.body.as_ref(), "Diary storage request failed"),
                )]),
            ),
            Err(_) => internal(),
        }
    }

    async fn workspace_export(&self, ctx: &Ctx) -> Reply {
        let failed = || error_reply(502, EXPORT_FAILED);
        let deadline = Instant::now() + self.limits.export_deadline;
        let attempt = async {
            let tenant = self.tenant(ctx).await?;
            let url = self.sidecar.url("/api/workspace-export")?;
            self.sidecar
                .send(
                    &tenant,
                    Method::GET,
                    &url,
                    Content::Json(None),
                    None,
                    || deadline,
                )
                .await
        };
        let Ok((mut upstream, _)) = attempt.await else {
            return failed();
        };
        if !upstream.status().is_success() {
            let status = upstream.status().as_u16();
            // `readCappedJson(upstream, 64 KiB).catch(() => ({}))`
            let body =
                match read_capped(&mut upstream, self.limits.export_error_cap, deadline).await {
                    Ok(Capped::Bytes(bytes)) => wtf16::parse(&String::from_utf8_lossy(&bytes))
                        .unwrap_or(JValue::Obj(Vec::new())),
                    _ => JValue::Obj(Vec::new()),
                };
            // `typeof body.detail` on a JSON `null` is a TypeError, which core's catch answered 502.
            if matches!(body, JValue::Null) {
                return failed();
            }
            let message = match body.get("detail") {
                JValue::Str(detail) => detail.clone(),
                _ => "Workspace export failed. Retry after checking storage.".to_owned(),
            };
            return error_reply(status, &message);
        }
        let zip = upstream
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/zip"));
        if !zip {
            return failed();
        }
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(pump(upstream, tx, deadline, self.limits.export_cap));
        Reply {
            status: 200,
            body: ReplyBody::Zip(ChannelBody::new(rx)),
        }
    }

    async fn workspace_import<B>(
        &self,
        ctx: &Ctx,
        request: &Request,
        search: &str,
        body: B,
    ) -> Reply
    where
        B: Body<Data = Bytes> + Unpin + Send,
    {
        let too_large = || error_reply(413, "Browser imports support ZIP files up to 32 MiB");
        let unavailable = || error_reply(502, IMPORT_UNAVAILABLE);
        let deadline = Instant::now() + self.limits.import_deadline;
        // Number(req.headers['content-length']) > MAX_UPLOAD
        if request
            .content_length
            .as_deref()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .is_some_and(|n| n > self.limits.import_max_upload as f64)
        {
            return too_large();
        }
        let payload = match read_body(body, self.limits.import_max_upload, deadline).await {
            Ok(bytes) => Bytes::from(bytes),
            Err(BodyFault::TooLarge) => return too_large(),
            // The deadline passed while the upload was still arriving: core destroyed the socket.
            Err(BodyFault::Timeout) => return error_reply(408, "Request timed out"),
            Err(BodyFault::Broken) => return unavailable(),
        };
        let attempt = async {
            let tenant = self.tenant(ctx).await?;
            let url = self
                .sidecar
                .url(&format!("/api/workspace-import{search}"))?;
            self.sidecar
                .send(
                    &tenant,
                    Method::POST,
                    &url,
                    Content::Zip,
                    Some(&payload),
                    || deadline,
                )
                .await
        };
        let Ok((mut response, _)) = attempt.await else {
            return unavailable();
        };
        let status = response.status().as_u16();
        let ok = response.status().is_success();
        let bytes = match read_capped(&mut response, self.limits.import_max_reply, deadline).await {
            Ok(Capped::Bytes(bytes)) => bytes,
            _ => return unavailable(),
        };
        // JSON.parse throws on anything that is not JSON, and core's catch answered 502.
        let parsed = match parse_or_text(&bytes) {
            Ok(JValue::Str(_)) | Err(_) => return unavailable(),
            Ok(v) => v,
        };
        if ok {
            return json_reply(status, &parsed);
        }
        // `typeof body.detail === 'string'` on null throws.
        if matches!(parsed, JValue::Null) {
            return unavailable();
        }
        let message = match parsed.get("detail") {
            JValue::Str(detail) => detail.clone(),
            _ => "Workspace import failed".to_owned(),
        };
        error_reply(status, &message)
    }
}

enum BodyFault {
    TooLarge,
    Timeout,
    Broken,
}

/// Reads a request body of at most `cap` bytes before `deadline`.
async fn read_body<B>(mut body: B, cap: usize, deadline: Instant) -> Result<Vec<u8>, BodyFault>
where
    B: Body<Data = Bytes> + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let frame = timeout_at(deadline, body.frame())
            .await
            .map_err(|_| BodyFault::Timeout)?;
        match frame {
            None => return Ok(bytes),
            Some(Err(_)) => return Err(BodyFault::Broken),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    if data.len() > cap.saturating_sub(bytes.len()) {
                        return Err(BodyFault::TooLarge);
                    }
                    bytes.extend_from_slice(&data);
                }
            }
        }
    }
}

/// Copies the archive to the client: at most `cap` bytes, before `deadline`. A stream that is cut
/// short ends with an error (the client's connection is cut), never with a clean end.
async fn pump(
    mut upstream: reqwest::Response,
    tx: mpsc::Sender<Result<Bytes, StreamError>>,
    deadline: Instant,
    cap: usize,
) {
    let copy = async {
        let mut total = 0usize;
        loop {
            // A client that leaves while the sidecar is quiet stops the copy at once, as Node's
            // `res` close did, instead of holding the sidecar's stream until the deadline.
            let next = tokio::select! {
                next = upstream.chunk() => next,
                () = tx.closed() => return Ok(()),
            };
            match next {
                Ok(Some(chunk)) => {
                    total = total.saturating_add(chunk.len());
                    if total > cap {
                        return Err(StreamError("Export exceeds download limit"));
                    }
                    // A client that went away drops the receiver; stopping closes the sidecar's stream.
                    if tx.send(Ok(chunk)).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => return Ok(()),
                Err(_) => return Err(StreamError("Export interrupted")),
            }
        }
    };
    let outcome = match timeout_at(deadline, copy).await {
        Ok(outcome) => outcome,
        Err(_) => Err(StreamError("Export deadline exceeded")),
    };
    if let Err(e) = outcome {
        let _ = tx.send(Err(e)).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn encode_uri_component_matches_js() {
        assert_eq!(encode_uri_component("2026-09"), "2026-09");
        assert_eq!(encode_uri_component("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(encode_uri_component("!~*'()-_."), "!~*'()-_.");
        assert_eq!(encode_uri_component("\u{fffd}"), "%EF%BF%BD");
    }

    #[test]
    fn query_is_whatwg_search_and_first_month() {
        let q = query_of("/api/diary/today?month=2026-08&month=x");
        assert_eq!(q.search, "?month=2026-08&month=x");
        assert_eq!(q.month.as_deref(), Some("2026-08"));
        assert_eq!(query_of("/api/diary/today?").search, "");
        assert_eq!(query_of("/api/diary/today").month, None);
        assert_eq!(query_of("/x?month=a+b%20c").month.as_deref(), Some("a b c"));
        assert_eq!(query_of("/x?month=").month.as_deref(), Some(""));
        // WHATWG normalises a space in the query as `url.search` does.
        assert_eq!(query_of("/x?a=b c").search, "?a=b%20c");
    }

    #[test]
    fn exact_paths_only() {
        for p in Route::PATHS {
            assert!(Route::from_path(p).is_some());
            assert!(Route::from_path(&format!("{p}/")).is_none());
        }
        assert!(Route::from_path("/api/diary/history").is_none());
        assert!(Route::from_path("/api/diary/local-exchange").is_none());
        assert!(Route::from_path("/api/diary/storage-statusx").is_none());
    }
}
