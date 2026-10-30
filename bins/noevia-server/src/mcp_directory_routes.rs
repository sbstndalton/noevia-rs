//! The admin MCP directory (sbstndalton/noevia#2202 round 2), ported from core
//! `routes/mcp-directory.cjs`. Capability `rust-mcp-directory`; core's `rust-mcp-directory.cjs`
//! refuses these routes with `RUST_MCP_DIRECTORY_OWNED`.
//!
//! Administrators only (the session gate first: device tokens refused, CSRF checked on writes; then
//! `403 Administrator required`):
//!
//! - `GET    /api/admin/mcp-directory`                  the servers, each with its last discovery
//! - `POST   /api/admin/mcp-directory`  `{ registryName, headers?, keyMode? }`  add a registry server
//! - `POST   /api/admin/mcp-directory/custom/preview`   validate and list the tools without saving
//! - `POST   /api/admin/mcp-directory/custom`           add any server by its address
//! - `PUT    /api/admin/mcp-directory/{id}/keys`        replace the shared key (checked first)
//! - `PUT    /api/admin/mcp-directory/{id}/oauth-client` a hand-registered app
//! - `DELETE /api/admin/mcp-directory/{id}`
//!
//! A candidate server is probed and its tools listed **by this front** (`mcp_egress::list_tools`:
//! initialize, tools/list over JSON or event-stream replies, public https only, no redirect, caps and
//! deadlines), so an add never waits on Node. Each state-changing request runs as one spawned task
//! that the handler awaits: a client that goes away strands nothing half-done. What Node's chat
//! client discovers (each server's tool count, the first tools, an error) comes from
//! `mcp-discovery.json` (mcp_discovery.rs); a server Node has not discovered yet shows no tools,
//! except the one this request has just listed itself.
//!
//! A custom add is rolled back when the server's tools changed between the preview and the
//! connect (a second listing with the stored credentials must show the catalogue the preview
//! showed); the rollback forgets the key the adding administrator stored, so adding the same server
//! again does not inherit it.
use crate::mcp_broker::{check_header_values, declared_of, Fail, Mcp};
use crate::mcp_directory::{Found, New, Row};
use crate::mcp_routes::{answer, error, failed, read_object};
use crate::mcp_tools;
use crate::{identity, plugin_directory_routes, App};
use axum::body::Body;
use axum::http::{Request, Response};
use axum::response::IntoResponse;
use js_json::JValue;
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub const LIST: &str = "/api/admin/mcp-directory";
pub const CUSTOM: &str = "/api/admin/mcp-directory/custom";
pub const CUSTOM_PREVIEW: &str = "/api/admin/mcp-directory/custom/preview";
pub const ONE: &str = "/api/admin/mcp-directory/{*}";
pub const KEYS: &str = "/api/admin/mcp-directory/{*}/keys";
pub const OAUTH_CLIENT: &str = "/api/admin/mcp-directory/{id}/oauth-client";
/// The routes this module answers (routes.toml gives Rust every method of each).
pub const ROUTES: &[&str] = &[LIST, CUSTOM, CUSTOM_PREVIEW, ONE, KEYS, OAUTH_CLIENT];

const PREVIEW_TTL_MS: i64 = 5 * 60 * 1000;
const NOT_ADMIN: &str = "Administrator required";

/// core's id shape in these routes: `[a-z0-9-]+`.
fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

enum Op {
    List,
    Add,
    Custom { preview: bool },
    Keys(String),
    OauthClient(String),
    Delete(String),
    NotAllowed,
}

/// core matched exact, anchored paths; anything else under the prefix is a 405 for an administrator.
fn op_for(method: &str, path: &str) -> Op {
    let rest = path.strip_prefix(LIST).unwrap_or("");
    match (method, rest) {
        ("GET", "") => Op::List,
        ("POST", "") => Op::Add,
        ("POST", "/custom/preview") => Op::Custom { preview: true },
        ("POST", "/custom") => Op::Custom { preview: false },
        _ => {
            let mut parts = rest.strip_prefix('/').unwrap_or("").split('/');
            match (method, parts.next(), parts.next(), parts.next()) {
                ("PUT", Some(id), Some("keys"), None) if id_ok(id) => Op::Keys(id.to_owned()),
                ("PUT", Some(id), Some("oauth-client"), None) if id_ok(id) => {
                    Op::OauthClient(id.to_owned())
                }
                ("DELETE", Some(id), None, None) if id_ok(id) => Op::Delete(id.to_owned()),
                _ => Op::NotAllowed,
            }
        }
    }
}

struct Ctx {
    app: Arc<App>,
    mcp: Arc<Mcp>,
    actor: String,
}

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

/// `String(v || '')` for the values a form sends.
fn str_of(v: &JValue) -> String {
    match v {
        JValue::Str(t) => t.clone(),
        JValue::Num(_) | JValue::Bool(true) => js_json::to_js_string(v).unwrap_or_default(),
        _ => String::new(),
    }
}

/// JS `String.prototype.trim` on the characters that matter here.
fn trim(t: &str) -> &str {
    t.trim_matches(crate::mcp_broker::js_ws)
}

fn with_fields(base: JValue, extra: impl IntoIterator<Item = (&'static str, JValue)>) -> JValue {
    match base {
        JValue::Obj(mut pairs) => {
            for (k, v) in extra {
                pairs.push((k.to_owned(), v));
            }
            JValue::Obj(pairs)
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------------------------
// The preview token: the administrator, the form and the catalogue the preview showed, signed with
// a key this process made, good for five minutes.

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let hashed;
    let key: &[u8] = if key.len() > 64 {
        hashed = Sha256::digest(key);
        hashed.as_slice()
    } else {
        key
    };
    let mut block = [0u8; 64];
    for (slot, byte) in block.iter_mut().zip(key) {
        *slot = *byte;
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn signature(key: &[u8], actor: &str, form: &[String], catalogue: &str, expires: i64) -> String {
    let doc = JValue::Arr(vec![
        s(actor),
        JValue::Arr(form.iter().map(|f| s(f)).collect()),
        s(catalogue),
        JValue::Num(expires as f64),
    ]);
    hex(&hmac_sha256(
        key,
        js_json::stringify(&doc).unwrap_or_default().as_bytes(),
    ))
}

fn issue_preview(key: &[u8], now: i64, actor: &str, form: &[String], catalogue: &str) -> String {
    let expires = now + PREVIEW_TTL_MS;
    format!(
        "{expires}.{}",
        signature(key, actor, form, catalogue, expires)
    )
}

fn matches_preview(
    key: &[u8],
    now: i64,
    token: &str,
    actor: &str,
    form: &[String],
    catalogue: &str,
) -> bool {
    let Some((expires, sig)) = token.split_once('.') else {
        return false;
    };
    if expires.len() != 13
        || !expires.bytes().all(|b| b.is_ascii_digit())
        || sig.len() != 64
        || !sig
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return false;
    }
    let Ok(expires) = expires.parse::<i64>() else {
        return false;
    };
    if expires < now {
        return false;
    }
    let expected = signature(key, actor, form, catalogue, expires);
    // Constant time over equal-length hex.
    expected
        .bytes()
        .zip(sig.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

// ---------------------------------------------------------------------------------------------

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let Some(mcp) = app.mcp.as_ref().map(Arc::clone) else {
        return app.proxy.forward(conn.peer, req).await;
    };
    let method = req.method().as_str().to_owned();
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || gate.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if id.user.role != server_auth::Role::Admin {
        return error(403, NOT_ADMIN);
    }
    let ctx = Ctx {
        app: Arc::clone(app),
        mcp,
        actor: id.user.id.clone(),
    };
    match op_for(&method, &path) {
        Op::NotAllowed => error(405, "method not allowed"),
        Op::List => match describe(&ctx, &[]).await {
            Ok(servers) => answer(200, &JValue::obj([("servers", servers)])),
            Err(e) => failed(&e),
        },
        op => {
            // A body that is not a JSON object is the client's mistake (core readObject).
            let body = match op {
                Op::Delete(_) => JValue::Obj(Vec::new()),
                _ => match read_object(body).await {
                    Ok(b) => b,
                    Err(()) => return error(400, "invalid JSON"),
                },
            };
            // The whole operation is one task the client cannot abandon half-way.
            let task = tokio::spawn(async move {
                match op {
                    Op::Add => add_registry(&ctx, &body).await,
                    Op::Custom { preview } => custom(&ctx, &body, preview).await,
                    Op::Keys(id) => replace_keys(&ctx, &id, &body).await,
                    Op::OauthClient(id) => app_client(&ctx, &id, &body).await,
                    Op::Delete(id) => remove(&ctx, &id).await,
                    Op::List | Op::NotAllowed => error(405, "method not allowed"),
                }
            });
            task.await
                .unwrap_or_else(|_| error(500, "The request did not finish."))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The listing.

/// core `describe()`: every directory server, with what Node's last discovery found (a tool count,
/// the first 40 tools, an error). `own` are servers this request has just listed itself, which win
/// over a discovery that may predate them.
async fn describe(ctx: &Ctx, own: &[(String, Found)]) -> Result<JValue, Fail> {
    let rows = ctx.mcp.blocking(&ctx.app, |me, a| me.dir_rows(a)).await??;
    let catalogue = crate::mcp_discovery::read(&ctx.app).await;
    let redirect = ctx
        .mcp
        .blocking(&ctx.app, |me, a| me.redirect_uri(a))
        .await?
        .unwrap_or_else(|_| "/api/mcp-oauth/callback".to_owned());
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let (count, tools, truncated, err) = match own.iter().find(|(id, _)| *id == row.id) {
            Some((_, found)) => {
                let (tools, truncated) = mcp_tools::shown(&found.tools);
                (
                    JValue::Num(found.tools.len() as f64),
                    tools,
                    truncated,
                    JValue::Null,
                )
            }
            None => match catalogue.as_ref().and_then(|c| c.server(&row.id)) {
                Some(info) => (
                    info.tool_count
                        .map_or(JValue::Null, |n| JValue::Num(n as f64)),
                    JValue::Arr(
                        info.tools
                            .iter()
                            .map(|(n, d)| JValue::obj([("name", s(n)), ("description", s(d))]))
                            .collect(),
                    ),
                    info.tools_truncated,
                    info.error.as_deref().map_or(JValue::Null, s),
                ),
                None => (JValue::Null, JValue::Arr(Vec::new()), false, JValue::Null),
            },
        };
        let mut extra = vec![
            ("toolCount", count),
            ("tools", tools),
            ("toolsTruncated", JValue::Bool(truncated)),
            ("error", err),
        ];
        if row.oauth {
            let info = ctx
                .mcp
                .client_info(&ctx.app, &row.id)
                .await
                .unwrap_or(JValue::Null);
            extra.push(("oauthClient", info));
            extra.push(("redirectUri", s(&redirect)));
        }
        out.push(with_fields(ctx.mcp.list_json(&row), extra));
    }
    Ok(JValue::Arr(out))
}

async fn servers_reply(
    ctx: &Ctx,
    status: u16,
    extra: Vec<(&'static str, JValue)>,
    own: &[(String, Found)],
) -> Response<Body> {
    match describe(ctx, own).await {
        Ok(servers) => {
            let mut pairs: Vec<(&'static str, JValue)> = extra;
            pairs.push(("servers", servers));
            answer(status, &JValue::obj(pairs))
        }
        Err(e) => failed(&e),
    }
}

async fn row_of(ctx: &Ctx, id: &str) -> Result<Option<Row>, Fail> {
    let id = id.to_owned();
    ctx.mcp
        .blocking(&ctx.app, move |me, a| me.dir_find(a, &id))
        .await?
}

async fn add_blocking(ctx: &Ctx, new: New) -> Result<Row, Fail> {
    let actor = ctx.actor.clone();
    ctx.mcp
        .blocking(&ctx.app, move |me, a| me.add_row(a, &new, &actor))
        .await?
}

/// The failure of a registry or listing, as core worded it.
fn listing_failure(has_key: bool, why: &str) -> Response<Body> {
    error(
        422,
        &format!(
            "{} did not answer as an MCP server: {}",
            if has_key {
                "The server did not accept that key, or"
            } else {
                "The server"
            },
            js_json::js_slice(why, 200)
        ),
    )
}

/// An OAuth server was added: send the administrator to sign in (or ask for the app by hand).
async fn start_oauth_add(ctx: &Ctx, url: &str, challenge: &str, new: New) -> Response<Body> {
    let added = match add_blocking(ctx, new).await {
        Ok(r) => r,
        Err(e) => return failed(&e),
    };
    let server = ctx.mcp.list_json(&added);
    match ctx
        .mcp
        .start(&ctx.app, &ctx.actor, &added.id, url, challenge, "add")
        .await
    {
        Ok(sign_in) => {
            servers_reply(
                ctx,
                202,
                vec![("signIn", JValue::Str(sign_in)), ("server", server)],
                &[],
            )
            .await
        }
        Err(e) if e.needs_client => {
            let redirect = ctx
                .mcp
                .blocking(&ctx.app, |me, a| me.redirect_uri(a))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_else(|| "/api/mcp-oauth/callback".to_owned());
            servers_reply(
                ctx,
                202,
                vec![
                    ("needsClient", JValue::Bool(true)),
                    ("issuer", e.issuer.as_deref().map_or(JValue::Undefined, s)),
                    ("redirectUri", s(&redirect)),
                    ("server", server),
                ],
                &[],
            )
            .await
        }
        Err(e) => {
            let id = added.id.clone();
            let _ = ctx
                .mcp
                .blocking(&ctx.app, {
                    let actor = ctx.actor.clone();
                    move |me, a| me.remove_row(a, &id, &actor)
                })
                .await;
            let _ = ctx.mcp.forget_all(&ctx.app, &added.id).await;
            error(
                422,
                &format!("The server needs a sign-in noevia cannot do: {}", e.message),
            )
        }
    }
}

/// core `keepAdminKey`: a personal server's adding administrator keeps the key they typed as their
/// own. Anything that goes wrong removes the server again, so none is left that no one can list the
/// tools of.
async fn keep_admin_key(
    ctx: &Ctx,
    added: &Row,
    pending: Vec<(String, String)>,
) -> Option<Response<Body>> {
    if !added.personal {
        return None;
    }
    let (actor, id) = (ctx.actor.clone(), added.id.clone());
    let stored = ctx
        .mcp
        .blocking(&ctx.app, move |me, a| {
            me.store_key(a, &actor, &id, &pending)
        })
        .await
        .and_then(|r| r);
    let Err(e) = stored else { return None };
    let (actor, id) = (ctx.actor.clone(), added.id.clone());
    let _ = ctx
        .mcp
        .blocking(&ctx.app, move |me, a| me.remove_row(a, &id, &actor))
        .await;
    let _ = ctx.mcp.forget_all(&ctx.app, &added.id).await;
    let status = if (400..600).contains(&e.status) {
        e.status
    } else {
        503
    };
    Some(error(status, &e.message))
}

// ---------------------------------------------------------------------------------------------
// POST /api/admin/mcp-directory

async fn add_registry(ctx: &Ctx, body: &JValue) -> Response<Body> {
    let name = body.get("registryName").as_str().unwrap_or("").to_owned();
    let item = match ctx.app.plugin_directory.find_registry_server(&name).await {
        Ok(Some(item)) => item,
        Ok(None) => return error(404, "That server is not in the MCP registry."),
        Err(_) => return error(502, "The MCP registry could not be reached right now."),
    };
    if !item.get("installable").truthy() {
        let why = item
            .get("notInstallable")
            .as_str()
            .filter(|w| !w.is_empty())
            .unwrap_or("That server cannot be added.");
        return error(422, why);
    }
    let url = item.get("remoteUrl").as_str().unwrap_or("").to_owned();
    if !ctx.mcp.egress().url_allowed(&url).await {
        return error(422, "That server’s address is not a public host.");
    }
    let declared_items = match item.get("headers") {
        h @ JValue::Arr(_) => h.clone(),
        _ => JValue::Arr(Vec::new()),
    };
    let values = match body.get("headers") {
        h if h.truthy() => h.clone(),
        _ => JValue::Obj(Vec::new()),
    };
    let pending = match check_header_values(&declared_of(&declared_items), &values) {
        Ok(p) => p,
        Err(e) => return failed(&e),
    };
    let has_key = !pending.is_empty();
    let registry_name = item.get("id").as_str().unwrap_or("").to_owned();
    let title = item.get("name").as_str().unwrap_or("").to_owned();
    let found = match ctx.mcp.discover_server(&url, &pending).await {
        Ok(f) => f,
        Err(why) => {
            // No key and the server asks for sign-in: add it as an OAuth server and send the
            // administrator to sign in.
            let (status, challenge) = if has_key {
                (0, String::new())
            } else {
                ctx.mcp.egress().probe_auth(&url).await
            };
            if status == 401 {
                let new = New {
                    registry_name,
                    title,
                    url: url.clone(),
                    declared: JValue::Arr(Vec::new()),
                    values: JValue::Obj(Vec::new()),
                    oauth: true,
                    personal: false,
                };
                return start_oauth_add(ctx, &url, &challenge, new).await;
            }
            return listing_failure(has_key, &why);
        }
    };
    if found.tools.is_empty() {
        return error(
            422,
            "The server answered but offers no tools noevia can use.",
        );
    }
    let new = New {
        registry_name,
        title,
        url,
        declared: declared_items,
        values,
        oauth: false,
        personal: has_key && body.get("keyMode").as_str() == Some("personal"),
    };
    let added = match add_blocking(ctx, new).await {
        Ok(r) => r,
        Err(e) => return failed(&e),
    };
    if let Some(refusal) = keep_admin_key(ctx, &added, pending).await {
        return refusal;
    }
    let count = found.tools.len();
    let server = with_fields(
        ctx.mcp.list_json(&added),
        [("toolCount", JValue::Num(count as f64))],
    );
    servers_reply(
        ctx,
        201,
        vec![("server", server)],
        &[(added.id.clone(), found)],
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// POST /api/admin/mcp-directory/custom[/preview]

async fn custom(ctx: &Ctx, body: &JValue, preview: bool) -> Response<Body> {
    let title: String = js_json::js_slice(trim(&str_of(body.get("title"))), 80);
    let url = trim(&str_of(body.get("url"))).to_owned();
    if title.is_empty() {
        return error(400, "Give the server a name.");
    }
    let qa = ctx.mcp.egress().qa_loopback();
    if !crate::mcp_egress::hosted_url_ok(&url, qa) {
        return error(400, "The address must be an https URL (no placeholders).");
    }
    if !ctx.mcp.egress().url_allowed(&url).await {
        return error(422, "That address is not a public host.");
    }
    let header_name = trim(&str_of(body.get("headerName"))).to_owned();
    let header_value = str_of(body.get("headerValue"));
    if !header_name.is_empty()
        && (!plugin_directory_routes::plain_header_name(&header_name)
            || plugin_directory_routes::reserved_header(&header_name))
    {
        return error(
            400,
            &format!("{header_name} cannot be used as a sign-in header."),
        );
    }
    let declared_items = if header_name.is_empty() {
        JValue::Arr(Vec::new())
    } else {
        JValue::Arr(vec![JValue::obj([
            ("name", s(&header_name)),
            ("required", JValue::Bool(true)),
            ("secret", JValue::Bool(true)),
            ("description", s("")),
            ("template", JValue::Null),
        ])])
    };
    let values = if header_name.is_empty() {
        JValue::Obj(Vec::new())
    } else {
        JValue::obj([(header_name.clone(), s(&header_value))])
    };
    let pending = match check_header_values(&declared_of(&declared_items), &values) {
        Ok(p) => p,
        Err(e) => return failed(&e),
    };
    let has_key = !pending.is_empty();
    let registry_name = format!("url:{url}");
    let personal = body.get("keyMode").as_str() != Some("shared");
    let form = vec![
        title.clone(),
        url.clone(),
        header_name.clone(),
        header_value.clone(),
        if personal { "personal" } else { "shared" }.to_owned(),
    ];
    let key = ctx.mcp.preview_key;
    let token = str_of(body.get("previewToken"));
    let consent = |catalogue: &str| {
        matches_preview(
            &key,
            identity::now_ms(),
            &token,
            &ctx.actor,
            &form,
            catalogue,
        )
    };
    let found = match ctx.mcp.discover_server(&url, &pending).await {
        Ok(f) => f,
        Err(why) => {
            let (status, challenge) = if has_key {
                (0, String::new())
            } else {
                ctx.mcp.egress().probe_auth(&url).await
            };
            if status == 401 {
                if preview {
                    return answer(
                        200,
                        &JValue::obj([
                            ("requiresSignIn", JValue::Bool(true)),
                            ("toolCount", JValue::Null),
                            ("tools", JValue::Arr(Vec::new())),
                            ("toolsTruncated", JValue::Bool(false)),
                            (
                                "previewToken",
                                s(&issue_preview(
                                    &key,
                                    identity::now_ms(),
                                    &ctx.actor,
                                    &form,
                                    "sign-in",
                                )),
                            ),
                        ]),
                    );
                }
                if !consent("sign-in") {
                    return error(
                        409,
                        "The server or access details changed since preview. Preview again before connecting.",
                    );
                }
                let new = New {
                    registry_name,
                    title,
                    url: url.clone(),
                    declared: JValue::Arr(Vec::new()),
                    values: JValue::Obj(Vec::new()),
                    oauth: true,
                    personal: false,
                };
                return start_oauth_add(ctx, &url, &challenge, new).await;
            }
            return listing_failure(has_key, &why);
        }
    };
    if found.tools.is_empty() {
        return error(
            422,
            "The server answered but offers no tools noevia can use.",
        );
    }
    let digest = mcp_tools::catalogue_digest(&found.tools);
    if preview {
        let (tools, truncated) = mcp_tools::shown(&found.tools);
        return answer(
            200,
            &JValue::obj([
                ("requiresSignIn", JValue::Bool(false)),
                ("toolCount", JValue::Num(found.tools.len() as f64)),
                ("tools", tools),
                ("toolsTruncated", JValue::Bool(truncated)),
                (
                    "previewToken",
                    s(&issue_preview(
                        &key,
                        identity::now_ms(),
                        &ctx.actor,
                        &form,
                        &digest,
                    )),
                ),
            ]),
        );
    }
    if !consent(&digest) {
        return error(
            409,
            "The server or its tools changed since preview. Preview again before connecting.",
        );
    }
    let new = New {
        registry_name,
        title,
        url: url.clone(),
        declared: declared_items,
        values,
        oauth: false,
        personal: has_key && body.get("keyMode").as_str() == Some("personal"),
    };
    let added = match add_blocking(ctx, new).await {
        Ok(r) => r,
        Err(e) => return failed(&e),
    };
    if let Some(refusal) = keep_admin_key(ctx, &added, pending).await {
        return refusal;
    }
    // The server must still offer exactly what the preview showed, read again with the credentials as
    // they are now stored. A listing that fails, or shows other tools, leaves nothing behind.
    let rediscovered = relist(ctx, &added).await;
    let kept = rediscovered
        .as_ref()
        .is_some_and(|again| consent(&mcp_tools::catalogue_digest(&again.tools)));
    if !kept {
        // A concurrent delete may already have removed it; either way the answer is the same 409.
        let row = added.clone();
        let _ = ctx
            .mcp
            .blocking(&ctx.app, move |me, a| me.remove_added(a, &row))
            .await;
        // The administrator's key goes with the server it was for. An id is derived from the server's
        // name, so adding the same server again would otherwise inherit a key never kept on purpose.
        let _ = ctx.mcp.forget_all(&ctx.app, &added.id).await;
        return error(
            409,
            "The server’s tools changed while connecting. It was not kept; preview again.",
        );
    }
    let count = found.tools.len();
    let server = with_fields(
        ctx.mcp.list_json(&added),
        [("toolCount", JValue::Num(count as f64))],
    );
    servers_reply(
        ctx,
        201,
        vec![("server", server)],
        &[(added.id.clone(), found)],
    )
    .await
}

/// The server's tools again, with the credentials as stored: the shared key opened from its row, or
/// the adding administrator's own key.
async fn relist(ctx: &Ctx, added: &Row) -> Option<Found> {
    let (id, actor) = (added.id.clone(), ctx.actor.clone());
    let headers = ctx
        .mcp
        .blocking(
            &ctx.app,
            move |me, a| -> Result<Vec<(String, String)>, Fail> {
                let Some(row) = me.dir_find(a, &id)? else {
                    return Err(crate::mcp_broker::fail("No such server.", 404));
                };
                if row.personal {
                    Ok(me.read_key(a, &actor, &id)?.flatten().unwrap_or_default())
                } else {
                    Ok(me.shared_headers(&row))
                }
            },
        )
        .await
        .ok()?
        .ok()?;
    if added.personal && headers.is_empty() {
        return None;
    }
    ctx.mcp.discover_server(&added.url, &headers).await.ok()
}

// ---------------------------------------------------------------------------------------------
// PUT /api/admin/mcp-directory/{id}/keys

async fn replace_keys(ctx: &Ctx, id: &str, body: &JValue) -> Response<Body> {
    let row = match row_of(ctx, id).await {
        Ok(Some(r)) => r,
        Ok(None) => return error(404, "No such server."),
        Err(e) => return failed(&e),
    };
    // The headers this server declared when it was added; for a registry server whose declaration
    // was never stored (older rows), the registry's current one. A custom server has only its own:
    // core asked the registry about `url:...` names too, found nothing, and cleared the key.
    let mut declared_items = row.declared.clone();
    if matches!(&declared_items, JValue::Arr(a) if a.is_empty())
        && !row.registry_name.starts_with("url:")
    {
        match ctx
            .app
            .plugin_directory
            .find_registry_server(&row.registry_name)
            .await
        {
            Ok(Some(item)) => {
                if let h @ JValue::Arr(_) = item.get("headers") {
                    declared_items = h.clone();
                }
            }
            Ok(None) => {}
            Err(_) => return error(502, "The MCP registry could not be reached right now."),
        }
    }
    let declared = declared_of(&declared_items);
    let values = match body.get("headers") {
        h if h.truthy() => h.clone(),
        _ => JValue::Obj(Vec::new()),
    };
    let pending = match check_header_values(&declared, &values) {
        Ok(p) => p,
        Err(e) => return failed(&e),
    };
    let found = match ctx.mcp.discover_server(&row.url, &pending).await {
        Ok(f) => f,
        Err(why) => {
            return error(
                422,
                &format!(
                    "The server did not accept that key: {}",
                    js_json::js_slice(&why, 200)
                ),
            )
        }
    };
    let (rid, actor) = (row.id.clone(), ctx.actor.clone());
    let saved = ctx
        .mcp
        .blocking(&ctx.app, move |me, a| {
            me.set_row_keys(a, &rid, &declared, &values, &actor)
        })
        .await
        .and_then(|r| r);
    if let Err(e) = saved {
        return failed(&e);
    }
    servers_reply(ctx, 200, Vec::new(), &[(row.id.clone(), found)]).await
}

// ---------------------------------------------------------------------------------------------
// PUT /api/admin/mcp-directory/{id}/oauth-client

async fn app_client(ctx: &Ctx, id: &str, body: &JValue) -> Response<Body> {
    let row = match row_of(ctx, id).await {
        Ok(Some(r)) if r.oauth => r,
        Ok(_) => return error(404, "No such sign-in server."),
        Err(e) => return failed(&e),
    };
    let (_, challenge) = ctx.mcp.egress().probe_auth(&row.url).await;
    let client_id = str_of(body.get("clientId"));
    let client_secret = str_of(body.get("clientSecret"));
    if let Err(e) = ctx
        .mcp
        .set_client(
            &ctx.app,
            &row.id,
            &row.url,
            &client_id,
            &client_secret,
            &challenge,
        )
        .await
    {
        return failed(&e);
    }
    let (actor, sid) = (ctx.actor.clone(), row.id.clone());
    let _ = ctx
        .mcp
        .blocking(&ctx.app, move |me, a| {
            me.audit(
                a,
                Some(&actor),
                "mcp.oauth.client",
                &JValue::obj([("serverId", JValue::Str(sid.clone()))]),
            );
        })
        .await;
    match ctx
        .mcp
        .start(&ctx.app, &ctx.actor, &row.id, &row.url, &challenge, "add")
        .await
    {
        Ok(sign_in) => servers_reply(ctx, 200, vec![("signIn", JValue::Str(sign_in))], &[]).await,
        Err(e) => failed(&e),
    }
}

// ---------------------------------------------------------------------------------------------
// DELETE /api/admin/mcp-directory/{id}

async fn remove(ctx: &Ctx, id: &str) -> Response<Body> {
    // Every account's sign-in, the registered app and every account's key go first: if that cannot be
    // done, the server stays (a row removed with credentials left behind could not be retried).
    if let Err(e) = ctx.mcp.forget_all(&ctx.app, id).await {
        let status = if (400..600).contains(&e.status) {
            e.status
        } else {
            503
        };
        return error(status, &e.message);
    }
    let (rid, actor) = (id.to_owned(), ctx.actor.clone());
    let removed = ctx
        .mcp
        .blocking(&ctx.app, move |me, a| me.remove_row(a, &rid, &actor))
        .await
        .and_then(|r| r);
    if let Err(e) = removed {
        let status = if (400..600).contains(&e.status) {
            e.status
        } else {
            400
        };
        return error(status, &e.message);
    }
    servers_reply(ctx, 200, Vec::new(), &[]).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn hmac_is_the_published_vector() {
        // RFC 4231 test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than the block is hashed first (RFC 4231 test case 6).
        let long = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&long),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn a_preview_token_vouches_for_one_administrator_one_form_and_one_catalogue() {
        let key = [3u8; 32];
        let form = vec![
            "t".to_owned(),
            "https://x/mcp".to_owned(),
            String::new(),
            String::new(),
            "personal".to_owned(),
        ];
        let now = 1_800_000_000_000;
        let token = issue_preview(&key, now, "admin-1", &form, "abc");
        assert!(matches_preview(
            &key,
            now + 1000,
            &token,
            "admin-1",
            &form,
            "abc"
        ));
        assert!(
            !matches_preview(&key, now + 1000, &token, "admin-2", &form, "abc"),
            "another administrator"
        );
        assert!(
            !matches_preview(&key, now + 1000, &token, "admin-1", &form, "abd"),
            "another catalogue"
        );
        let mut other = form.clone();
        other[3] = "secret".into();
        assert!(
            !matches_preview(&key, now + 1000, &token, "admin-1", &other, "abc"),
            "another form"
        );
        assert!(
            !matches_preview(&[4u8; 32], now + 1000, &token, "admin-1", &form, "abc"),
            "another process"
        );
        assert!(
            !matches_preview(
                &key,
                now + PREVIEW_TTL_MS + 1,
                &token,
                "admin-1",
                &form,
                "abc"
            ),
            "expired"
        );
        for bad in [
            "",
            "x",
            "1.2",
            &token[..token.len() - 1],
            &format!("{token}0"),
        ] {
            assert!(
                !matches_preview(&key, now + 1000, bad, "admin-1", &form, "abc"),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_path_is_one_of_the_operations_or_not_allowed() {
        assert!(matches!(op_for("GET", LIST), Op::List));
        assert!(matches!(op_for("POST", LIST), Op::Add));
        assert!(matches!(
            op_for("POST", CUSTOM),
            Op::Custom { preview: false }
        ));
        assert!(matches!(
            op_for("POST", CUSTOM_PREVIEW),
            Op::Custom { preview: true }
        ));
        assert!(
            matches!(op_for("PUT", "/api/admin/mcp-directory/dir-x/keys"), Op::Keys(id) if id == "dir-x")
        );
        assert!(matches!(
            op_for("PUT", "/api/admin/mcp-directory/dir-x/oauth-client"),
            Op::OauthClient(_)
        ));
        assert!(matches!(
            op_for("DELETE", "/api/admin/mcp-directory/dir-x"),
            Op::Delete(_)
        ));
        // core's patterns were anchored: these were its 405.
        for (m, p) in [
            ("PUT", LIST),
            ("GET", CUSTOM),
            ("DELETE", "/api/admin/mcp-directory/Dir-X"),
            ("DELETE", "/api/admin/mcp-directory/dir-x/keys"),
            ("PUT", "/api/admin/mcp-directory/dir-x/keys/"),
            ("PUT", "/api/admin/mcp-directory/UP/keys"),
            ("GET", "/api/admin/mcp-directory/"),
            ("POST", "/api/admin/mcp-directory/dir-x"),
        ] {
            assert!(matches!(op_for(m, p), Op::NotAllowed), "{m} {p}");
        }
    }
}
