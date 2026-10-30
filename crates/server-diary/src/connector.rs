//! The Diary connectors (sbstndalton/noevia#2206): core `diary-connectors.cjs` and the connector
//! half of `routes/diary.cjs`.
//!
//! A connector is a credential a person gives another app (a notes editor, a script) so it can list,
//! read and write Markdown files in their Diary without a session. The credential is shown once when
//! it is made; the table keeps only its SHA-256, so there is no secret to seal and nothing to reseal
//! (`diary_connectors`: id, user_id, name, token_hash, created_at, in core's own shape, so a rollback
//! reads every row). The front is the table's only writer (server-store `Access::InsertDelete`; core
//! refuses every Node write, rust-auth.cjs `FRONT_TABLES`).
//!
//! * [`verify`]: a bearer token is a connector if it has the token shape and its digest belongs to a
//!   connector of an enabled account with the Diary add-on on.
//! * [`list`], [`create`], [`revoke`]: `/api/profile/diary-connectors`. A create and a revoke write
//!   their audit event in the same transaction as the row.
//! * [`operate`]: `POST /api/diary-connector`, `list`/`read`/`write` of one Markdown file, as the
//!   connector's account, through the Diary client (so with that account's tenant headers, 428 retry
//!   and member-origin policy). A write passes on exactly one write, only with the version the
//!   connector last read, and is checked against revocation again right before it is sent.

use crate::client::{CallError, JsonReply};
use crate::routes::Diary;
use crate::tenant::Tenant;
use bytes::Bytes;
use js_json::{wtf16, JValue};
use rand_core::{OsRng, RngCore};
use reqwest::Method;
use server_auth::Role;
use server_store::rusqlite::types::Value;
use server_store::{Store, StoreError, Writer};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// A name's longest length in UTF-16 units, as `name.length>80` counts it.
pub const NAME_MAX: usize = 80;
/// Connectors one account may hold.
pub const PER_ACCOUNT_CAP: usize = 10;
/// A file's longest content, in UTF-8 bytes.
pub const CONTENT_MAX: usize = 512 * 1024;
/// `callDiaryFile`'s deadline for one sidecar call.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const BAD_PATH: &str = "Choose a Markdown path inside the Diary";
const BAD_WRITE: &str = "Content (maximum 512 KiB) and the exact read version are required";
const INTERRUPTED: &str =
    "Diary request interrupted; read the current version before retrying a write";

/// A thrown error of core's: a status (when it has one) and a message. [`Fault::reply`] is what
/// http.cjs `errorResponse` makes of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    pub status: u16,
    pub message: String,
}

impl Fault {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Fault {
            status,
            message: message.into(),
        }
    }

    /// `errorResponse(err)`: a 4xx keeps its status and message; anything else is a generic answer
    /// (a 5xx keeps its status).
    pub fn reply(&self) -> (u16, String) {
        let (status, message) = if (400..500).contains(&self.status) {
            (self.status, self.message.as_str())
        } else if (500..600).contains(&self.status) {
            (self.status, "Internal error")
        } else {
            (500, "Internal error")
        };
        let mut text = String::from("{\"error\":");
        js_json::quote(message, &mut text);
        text.push('}');
        (status, text)
    }

    fn internal() -> Self {
        Fault::new(500, "Internal error")
    }
}

impl From<StoreError> for Fault {
    fn from(_: StoreError) -> Self {
        Fault::internal()
    }
}

fn digest(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `/^nv_diary_[a-f0-9]{32}\.[A-Za-z0-9_-]{43}$/`
pub fn token_shaped(token: &str) -> bool {
    let Some(rest) = token.strip_prefix("nv_diary_") else {
        return false;
    };
    let Some((id, secret)) = rest.split_once('.') else {
        return false;
    };
    id_shaped(id)
        && secret.len() == 43
        && secret
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

/// `/^[a-f0-9]{32}$/`: the id in `/api/profile/diary-connectors/{id}`.
pub fn id_shaped(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// `!!row?.diary_enabled`
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Integer(i) => *i != 0,
        Value::Real(f) => *f != 0.0 && !f.is_nan(),
        Value::Text(t) => !t.is_empty(),
        Value::Blob(_) => true,
    }
}

/// `auth.diaryEnabled(userId)`
fn diary_enabled(r: &server_store::Reader<'_>, user_id: &str) -> Result<bool, StoreError> {
    Ok(r.row(
        "SELECT diary_enabled FROM user_features WHERE user_id=?1",
        [user_id],
        |row| row.get::<_, Value>(0),
    )?
    .is_some_and(|v| truthy(&v)))
}

/// Who a verified credential is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub id: String,
    pub user_id: String,
}

/// `createCredentials(auth).verify(token)`.
pub fn verify(store: &Store, token: &str) -> Result<Option<Verified>, StoreError> {
    if !token_shaped(token) {
        return Ok(None);
    }
    let hash = digest(token);
    store.read(|r| {
        let row = r.row(
            "SELECT c.id,c.user_id FROM diary_connectors c JOIN users u ON c.user_id=u.id WHERE c.token_hash=?1 AND u.disabled_at IS NULL",
            [&hash],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        Ok(match row {
            Some((id, user_id)) if diary_enabled(r, &user_id)? => Some(Verified { id, user_id }),
            _ => None,
        })
    })
}

/// A connector as the list shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub id: String,
    pub name: String,
    pub created_at: Value,
}

fn value_json(v: &Value) -> JValue {
    match v {
        Value::Integer(i) => JValue::Num(*i as f64),
        Value::Real(f) => JValue::Num(*f),
        Value::Text(t) => JValue::Str(t.clone()),
        _ => JValue::Null,
    }
}

impl Listed {
    pub fn json(&self) -> JValue {
        JValue::obj([
            ("id", JValue::Str(self.id.clone())),
            ("name", JValue::Str(self.name.clone())),
            ("createdAt", value_json(&self.created_at)),
        ])
    }
}

fn list_in(r: &server_store::Reader<'_>, user_id: &str) -> Result<Vec<Listed>, StoreError> {
    r.rows(
        "SELECT id,name,created_at FROM diary_connectors WHERE user_id=?1 ORDER BY rowid",
        [user_id],
        |row| {
            Ok(Listed {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
            })
        },
    )
}

/// `list(userId)`
pub fn list(store: &Store, user_id: &str) -> Result<Vec<Listed>, StoreError> {
    store.read(|r| list_in(r, user_id))
}

/// Why a create was refused. Core threw a bare `Error` for all three (a generic 500 whose message
/// the person never saw); each is now a 4xx that says what to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Name,
    Unavailable,
    TooMany,
}

impl Refusal {
    pub fn fault(self) -> Fault {
        match self {
            Refusal::Name => Fault::new(400, "Connector name required (maximum 80 characters)"),
            Refusal::Unavailable => Fault::new(403, "Diary account unavailable"),
            Refusal::TooMany => Fault::new(409, "Revoke an existing connector first"),
        }
    }
}

/// A connector that was just made: its id and the credential, shown once.
pub struct Created {
    pub id: String,
    pub token: zeroize::Zeroizing<String>,
}

fn random_hex(bytes: usize) -> Result<String, Fault> {
    let mut raw = vec![0u8; bytes];
    OsRng
        .try_fill_bytes(&mut raw)
        .map_err(|_| Fault::internal())?;
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

/// `create(userId, name)`: the name is trimmed and at most 80 characters long (before trimming, as
/// core counted); the account must exist, be enabled and have the Diary on; at most ten connectors.
pub fn create(
    writer: &Writer,
    user_id: &str,
    name: &JValue,
    now_ms: i64,
) -> Result<Result<Created, Refusal>, Fault> {
    use base64::Engine as _;
    let Some(raw) = name.as_str() else {
        return Ok(Err(Refusal::Name));
    };
    let units: Vec<u16> = raw.encode_utf16().collect();
    let trimmed = String::from_utf16_lossy(policy_leaves::js_trim(&units));
    if trimmed.is_empty() || wtf16::len(raw) > NAME_MAX {
        return Ok(Err(Refusal::Name));
    }
    let id = random_hex(16)?;
    let mut secret = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut secret)
        .map_err(|_| Fault::internal())?;
    let token = zeroize::Zeroizing::new(format!(
        "nv_diary_{id}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret)
    ));
    let hash = digest(&token);
    let detail = wtf16::stringify(&JValue::obj([
        ("id", JValue::Str(id.clone())),
        ("name", JValue::Str(trimmed.clone())),
    ]))
    .ok_or_else(Fault::internal)?;
    let outcome = writer.write(|tx| {
        let r = tx.reader();
        let account = r.row(
            "SELECT id FROM users WHERE id=?1 AND disabled_at IS NULL",
            [user_id],
            |row| row.get::<_, String>(0),
        )?;
        if account.is_none() || !diary_enabled(&r, user_id)? {
            return Ok(Err(Refusal::Unavailable));
        }
        if list_in(&r, user_id)?.len() >= PER_ACCOUNT_CAP {
            return Ok(Err(Refusal::TooMany));
        }
        tx.execute(
            "INSERT INTO diary_connectors VALUES(?1,?2,?3,?4,?5)",
            (&id, user_id, &trimmed, &hash, now_ms),
        )?;
        audit(tx, "diary-connector.create", user_id, &detail, now_ms)?;
        Ok(Ok(()))
    })?;
    Ok(outcome.map(|()| Created { id, token }))
}

/// auth.cjs `audit(action, userId, userId, detail)`.
fn audit(
    tx: &server_store::WriteTx<'_>,
    action: &str,
    user_id: &str,
    detail: &str,
    now_ms: i64,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?2,?3,?4,?5)",
        (user_id, user_id, action, detail, now_ms),
    )?;
    Ok(())
}

/// The audit event of a connector's write: which credential, which path, how many bytes; never the
/// content (core `routes/diary.cjs`).
pub fn audit_write(
    writer: &Writer,
    who: &Verified,
    path: &JValue,
    bytes: usize,
    now_ms: i64,
) -> Result<(), StoreError> {
    let detail = wtf16::stringify(&JValue::obj([
        ("credentialId", JValue::Str(who.id.clone())),
        ("path", path.clone()),
        ("bytes", JValue::Num(bytes as f64)),
    ]))
    .unwrap_or_else(|| "{}".to_owned());
    writer.write(|tx| audit(tx, "diary-connector.write", &who.user_id, &detail, now_ms))
}

/// `revoke(userId, id)`: whether a connector of this account was removed.
pub fn revoke(writer: &Writer, user_id: &str, id: &str, now_ms: i64) -> Result<bool, StoreError> {
    writer.write(|tx| {
        let removed = tx.execute(
            "DELETE FROM diary_connectors WHERE user_id=?1 AND id=?2",
            (user_id, id),
        )? > 0;
        if removed {
            let detail = wtf16::stringify(&JValue::obj([("id", JValue::Str(id.to_owned()))]))
                .unwrap_or_else(|| "{}".to_owned());
            audit(tx, "diary-connector.revoke", user_id, &detail, now_ms)?;
        }
        Ok(removed)
    })
}

/// `safePath(value, dir)`: a Markdown file (or, with `dir`, a folder, possibly the root) inside the
/// Diary, no empty or hidden segment, no backslash or control character.
pub fn safe_path(value: &JValue, dir: bool) -> Result<String, Fault> {
    let bad = || Fault::new(400, BAD_PATH);
    let Some(path) = value.as_str() else {
        return Err(bad());
    };
    if wtf16::len(path) > 500
        || path
            .chars()
            .any(|c| c == '\\' || c.is_ascii_control() || c == '\u{7f}')
    {
        return Err(bad());
    }
    if !dir && !path.to_ascii_lowercase().ends_with(".md") {
        return Err(bad());
    }
    if !path.is_empty() && path.split('/').any(|p| p.is_empty() || p.starts_with('.')) {
        return Err(bad());
    }
    if !dir && path.is_empty() {
        return Err(bad());
    }
    Ok(path.to_owned())
}

/// `Buffer.byteLength(text)`: a lone surrogate is encoded as U+FFFD, three bytes.
pub fn byte_length(text: &str) -> usize {
    text.chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                3
            } else {
                c.len_utf8()
            }
        })
        .sum()
}

/// `encodeURIComponent`
fn encode_component(text: &str) -> String {
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

/// `/^[a-f0-9]{64}$/`
fn version_shaped(v: &str) -> bool {
    v.len() == 64 && v.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// The role of an enabled account that has the Diary on, for the tenant's member-origin policy.
fn account_role(store: &Store, user_id: &str) -> Result<Option<Role>, StoreError> {
    store.read(|r| {
        let role = r.row(
            "SELECT role FROM users WHERE id=?1 AND disabled_at IS NULL",
            [user_id],
            |row| row.get::<_, String>(0),
        )?;
        let Some(role) = role else {
            return Ok(None);
        };
        if !diary_enabled(r, user_id)? {
            return Ok(None);
        }
        Ok(Some(if role == "admin" {
            Role::Admin
        } else {
            Role::Member
        }))
    })
}

impl Diary {
    /// `callDiaryFile(userId, endpoint, method, body)`: one call to the sidecar as this account.
    async fn call_file(
        &self,
        store: &Arc<Store>,
        data_dir: &Path,
        user_id: &str,
        method: Method,
        endpoint: &str,
        body: Option<String>,
    ) -> Result<JValue, Fault> {
        let (s, d, u) = (
            Arc::clone(store),
            data_dir.to_path_buf(),
            user_id.to_owned(),
        );
        let config = self.sidecar().config().clone();
        let tenant = tokio::task::spawn_blocking(move || {
            let role = account_role(&s, &u).map_err(Fault::from)?;
            let Some(role) = role else {
                return Err(Fault::new(403, "Diary unavailable"));
            };
            Tenant::load_user(&u, role, &s, &d, &config).map_err(|_| Fault::internal())
        })
        .await
        .map_err(|_| Fault::internal())??;
        let reply: Result<JsonReply, CallError> = self
            .sidecar()
            .fetch_json(
                &tenant,
                method,
                &format!("/api{endpoint}"),
                body.map(Bytes::from),
                CALL_TIMEOUT,
            )
            .await;
        // A thrown fetch error carries no status: a generic 500.
        let reply = reply.map_err(|_| Fault::internal())?;
        if reply.ok {
            return Ok(reply.body.unwrap_or(JValue::Undefined));
        }
        let status = if reply.status == 0 { 502 } else { reply.status };
        let detail = reply
            .body
            .as_ref()
            .map(|b| b.get("detail"))
            .filter(|d| d.truthy());
        let message = detail
            .and_then(|d| js_json::to_js_string(d).ok())
            .unwrap_or_else(|| INTERRUPTED.to_owned());
        Err(Fault::new(status, message))
    }

    /// `operate(identity, body, files, allowed)` for `POST /api/diary-connector`. The sidecar calls
    /// run to the end even if the client goes away (a write is not abandoned half-sent), as they did
    /// in core, which never tied them to the client's connection.
    ///
    /// `audit` is the write's audit entry (blocking work, run off the async threads). It runs in the
    /// same detached task, straight after a write the sidecar accepted, so a client that disconnects
    /// after the sidecar write cannot cost the audit row: the handler's own future is the one that
    /// gets dropped, never this one. It does not run for a read or a list, for a refused or failed
    /// write, or when `audit` is `None`; a failure inside it is the closure's to log (the write has
    /// happened and must not turn into an error the connector would retry).
    pub async fn operate_connector(
        &self,
        store: Arc<Store>,
        data_dir: std::path::PathBuf,
        token: String,
        who: Verified,
        body: JValue,
        audit: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<JValue, Fault> {
        let this = self.clone();
        tokio::spawn(async move {
            let wrote = body.get("action").as_str() == Some("write");
            let result = this.operate(&store, &data_dir, &token, &who, &body).await;
            if let (true, Ok(_), Some(audit)) = (wrote, &result, audit) {
                // The audit is blocking SQLite; a panic inside it must not lose the result.
                let _ = tokio::task::spawn_blocking(audit).await;
            }
            result
        })
        .await
        .map_err(|_| Fault::internal())?
    }

    async fn operate(
        &self,
        store: &Arc<Store>,
        data_dir: &Path,
        token: &str,
        who: &Verified,
        body: &JValue,
    ) -> Result<JValue, Fault> {
        let allowed = || async {
            let (s, t) = (Arc::clone(store), token.to_owned());
            let still = tokio::task::spawn_blocking(move || verify(&s, &t))
                .await
                .map_err(|_| Fault::internal())?
                .map_err(Fault::from)?;
            Ok::<bool, Fault>(still.is_some())
        };
        if !allowed().await? {
            return Err(Fault::new(403, "Connector was revoked or Diary disabled"));
        }
        let action = body.get("action").as_str();
        let wanted = match body.get("path") {
            JValue::Null | JValue::Undefined => JValue::Str(String::new()),
            other => other.clone(),
        };
        let path = safe_path(&wanted, action == Some("list"))?;
        let user = who.user_id.as_str();
        match action {
            Some("list") => {
                let endpoint = format!("/files?path={}", encode_component(&path));
                let reply = self
                    .call_file(store, data_dir, user, Method::GET, &endpoint, None)
                    .await?;
                // `(await ...).files` of a `null` body is a TypeError.
                if reply.is_nullish() {
                    return Err(Fault::internal());
                }
                Ok(JValue::obj([("files", reply.get("files").clone())]))
            }
            Some("read") => {
                let text = wtf16::stringify(&JValue::obj([("path", JValue::Str(path))]))
                    .ok_or_else(Fault::internal)?;
                self.call_file(store, data_dir, user, Method::POST, "/file", Some(text))
                    .await
            }
            Some("write") => {
                let content = body.get("content").as_str();
                let version = body.get("version");
                let version_ok = match version {
                    JValue::Null => true,
                    JValue::Str(v) => version_shaped(v),
                    _ => false,
                };
                let Some(content) = content.filter(|c| byte_length(c) <= CONTENT_MAX && version_ok)
                else {
                    return Err(Fault::new(400, BAD_WRITE));
                };
                // No blind overwrite or automatic retry. The companion checks version and ETag
                // atomically; the credential is checked once more right before the send.
                if !allowed().await? {
                    return Err(Fault::new(403, "Connector revoked"));
                }
                let text = wtf16::stringify(&JValue::obj([
                    ("path", JValue::Str(path)),
                    ("content", JValue::Str(content.to_owned())),
                    ("version", version.clone()),
                ]))
                .ok_or_else(Fault::internal)?;
                self.call_file(store, data_dir, user, Method::PUT, "/file", Some(text))
                    .await
            }
            _ => Err(Fault::new(400, "Unknown Diary action")),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn s(t: &str) -> JValue {
        JValue::Str(t.to_owned())
    }

    #[test]
    fn a_token_has_core_shape_and_nothing_else() {
        let good = format!("nv_diary_{}.{}", "a1".repeat(16), "A".repeat(43));
        assert!(token_shaped(&good));
        for bad in [
            format!("nv_diary_{}.{}", "A1".repeat(16), "A".repeat(43)),
            format!("nv_diary_{}.{}", "a1".repeat(16), "A".repeat(42)),
            format!("nv_diary_{}.{}", "a1".repeat(16), "A".repeat(44)),
            format!("nv_diary_{}.{}=", "a1".repeat(16), "A".repeat(42)),
            format!("nv_diary_{}x{}", "a1".repeat(16), "A".repeat(43)),
            format!("{good}\n"),
            format!(" {good}"),
            String::new(),
        ] {
            assert!(!token_shaped(&bad), "{bad:?}");
        }
    }

    #[test]
    fn safe_path_is_cores() {
        assert_eq!(safe_path(&s("a/b.md"), false).unwrap(), "a/b.md");
        assert_eq!(safe_path(&s("A/B.MD"), false).unwrap(), "A/B.MD");
        assert_eq!(safe_path(&s(""), true).unwrap(), "");
        assert_eq!(safe_path(&s("Notes"), true).unwrap(), "Notes");
        for (v, dir) in [
            ("", false),
            ("a.txt", false),
            ("a/.hidden.md", false),
            (".git/x.md", false),
            ("a//b.md", false),
            ("/a.md", false),
            ("a/", true),
            ("a\\b.md", false),
            ("a\u{0}b.md", false),
            ("a\nb.md", false),
            ("a\u{7f}.md", false),
        ] {
            assert_eq!(safe_path(&s(v), dir).unwrap_err().status, 400, "{v:?}");
        }
        assert!(safe_path(&JValue::Num(1.0), true).is_err());
        assert!(safe_path(&JValue::Null, true).is_err());
        let long = format!("{}.md", "a".repeat(497));
        assert!(safe_path(&s(&long), false).is_ok());
        assert!(safe_path(&s(&format!("{long}x.md")), false).is_err());
    }

    #[test]
    fn errors_answer_as_error_response_does() {
        assert_eq!(
            Fault::new(409, "Stale").reply(),
            (409, r#"{"error":"Stale"}"#.to_owned())
        );
        assert_eq!(
            Fault::new(503, "secret detail").reply(),
            (503, r#"{"error":"Internal error"}"#.to_owned())
        );
        assert_eq!(
            Fault::new(302, "x").reply(),
            (500, r#"{"error":"Internal error"}"#.to_owned())
        );
    }

    #[test]
    fn content_length_is_utf8_bytes_with_a_lone_surrogate_as_three() {
        assert_eq!(byte_length("aé€😀"), 1 + 2 + 3 + 4);
        let lone = wtf16::lone_char(0xD800).unwrap().to_string();
        assert_eq!(byte_length(&lone), 3);
    }
}
