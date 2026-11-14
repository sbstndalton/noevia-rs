//! Track R12: `POST /api/admin/secrets/rotate` answered by the front, with NOEVIA_RUST_SECRETS_ROTATE=1
//! (lib.rs dispatch guard; unset, or any value but `1`: the request goes to Node as before). Core
//! routes/auth.cjs + secrets-rotate.cjs `runRotation`, in Node's order (crates/secrets-rotate):
//!
//! 1. the provider keys' pass (provider_reseal.rs, scope `all`) and 2. the MCP credentials' pass
//!    (mcp_broker.rs): either failing stops the rotation with a 500 before anything below is changed;
//! 3. the Google Drive token files' pass (gdrive.rs `reseal_users`), soft: a failure is reported;
//! 4. `storage_connections` and `chatgpt_oauth_tokens`: each value re-sealed under the current key,
//!    bound to its account, written by compare-and-swap ([`server_store::WriteTx`]'s two `reseal_*`
//!    statements), and every Drive token file checked, read-only, to open under the current key;
//! 5. the passes merged into the report, the Drive files the front re-sealed counted as rotated, the
//!    audit row `secrets.rotate`, and the report as the reply.
//!
//! Each front pass reaches the merge as the JSON its internal store route answers core's client today,
//! read the way that client reads it (rust-providers-reseal.cjs, rust-mcp-credentials.cjs,
//! project-store-client.cjs `gdriveReseal`), so the report is the one Node would make.
//!
//! Rotations run one at a time, each in its own task. No key, ciphertext or plaintext is ever logged
//! or answered.
//!
//! Differences from Node:
//! - the keys are read from disk at the rotation (Node's own copy was read at its start, so
//!   `previousKey` and step 4 now see a retired key added since);
//! - the 500 for a pass that cannot run, or for an audit row that cannot be written, names it in the
//!   front's words (Node's answer carried the error's own text);
//! - a Drive token file that cannot be read (a directory named `x.sealed`), or a Drive token path that
//!   is a file, fails with a fixed reason instead of Node's file-system message;
//! - Node's client gave up on a front pass after 120 s (providers) or 45 s (MCP, Drive: then soft,
//!   "unreachable"); here each pass runs to its end, and the whole rotation runs to its end even when the
//!   client goes away (Node's did too, in its process), with its audit row.
//!
//! Precondition, as with Node's own rotation: restart after swapping the key files, then rotate. Node
//! reads `secrets.key` once at its start; this rotation seals under the key on disk now, so a Node that
//! still holds the old key can no longer open what was re-sealed until it restarts.
//!
//! Rollback (switch unset again): everything written here is in the formats Node writes and reads
//! (`enc:v2:` bound to the account under the current `secrets.key`, the same audit row), so Node's
//! own rotation and every Node reader carry on as before (given the precondition above); keep
//! `secrets.key.previous` until a rotation reports no failures, as with Node's.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::{wtf16, JValue};
use secrets_rotate::{KeyError, 
    audit_detail, drive_check, drive_front_error, merge, rotate, Counts, DriveFront, Failure, Keys,
    Pass, Row, Status, Table, AUDIT_ACTION, DRIVE_TABLE,
};
use server_auth::secrets::SecretKeys;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroizing;

pub const ROUTE: &str = "/api/admin/secrets/rotate";
/// The deployment switch.
pub const SWITCH: &str = "NOEVIA_RUST_SECRETS_ROTATE";

/// One rotation at a time (a second waits for the first).
static RUNNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn error(status: StatusCode, message: &str) -> Response<Body> {
    reply::error(status, message, true)
}

/// The key store as the rotation uses it: the keys on disk now.
struct DiskKeys(SecretKeys);

impl Keys for DiskKeys {
    fn open(
        &self,
        value: &str,
        user: Option<&str>,
    ) -> Result<(Zeroizing<String>, secret_envelope::KeyUsed), KeyError> {
        self.0.open(value, user).map_err(|_| KeyError)
    }
    fn seal(&self, plain: &str, user: Option<&str>) -> Result<String, KeyError> {
        self.0
            .seal(plain, user, &crate::mcp_broker::random::<12>())
            .map_err(|_| KeyError)
    }
}

/// `{ ok: true, tables, failures }` as core's client of a front pass reads it: every table's five counts
/// (whole numbers from 0), the first 1,000 failures with `table` as a string, `ref` kept as `keep_ref`
/// says, the reason cut to 200 UTF-16 units. `None` where that client throws.
fn pass_of(
    body: &JValue,
    keep_ref: impl Fn(&JValue) -> JValue,
    names: Option<&[&str]>,
) -> Option<Pass> {
    if body.get("ok") != &JValue::Bool(true) {
        return None;
    }
    let JValue::Obj(tables) = body.get("tables") else {
        return None;
    };
    let JValue::Arr(failures) = body.get("failures") else {
        return None;
    };
    let count = |c: &JValue, k: &str| -> Option<u64> {
        let n = c.get(k).as_num()?;
        (n >= 0.0 && n.fract() == 0.0 && n <= 9_007_199_254_740_991.0).then_some(n as u64)
    };
    let mut out = Pass::default();
    for (name, c) in tables {
        // rust-mcp-credentials.cjs: only the four MCP tables.
        if names.is_some_and(|allowed| !allowed.contains(&name.as_str())) {
            return None;
        }
        out.tables.push((
            name.clone(),
            Counts {
                current: count(c, "current")?,
                rotated: count(c, "rotated")?,
                upgraded: count(c, "upgraded")?,
                empty: count(c, "empty")?,
                failed: count(c, "failed")?,
            },
        ));
    }
    for f in failures.iter().take(1000) {
        out.failures.push(Failure {
            table: js_json::to_js_string(f.get("table")).unwrap_or_default(),
            reference: keep_ref(f.get("ref")),
            // `String(f && f.error || '')`.
            error: if f.get("error").truthy() {
                js_json::to_js_string(f.get("error")).unwrap_or_default()
            } else {
                String::new()
            },
        });
    }
    Some(out.normalised())
}

/// rust-mcp-credentials.cjs `TABLES`.
const MCP_TABLES: [&str; 4] = [
    "mcp_oauth_tokens",
    "mcp_oauth_clients",
    "directory_mcp_user_keys",
    "directory_mcp_servers",
];

/// rust-providers-reseal.cjs: `ref` a safe integer, else `null`.
fn provider_ref(r: &JValue) -> JValue {
    match r {
        JValue::Num(n) if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 => r.clone(),
        _ => JValue::Null,
    }
}

/// rust-mcp-credentials.cjs: `ref` an array (each `String()`, the first two), else `null`.
fn mcp_ref(r: &JValue) -> JValue {
    match r {
        JValue::Arr(items) => JValue::Arr(
            items
                .iter()
                .take(2)
                .map(|v| JValue::Str(js_json::to_js_string(v).unwrap_or_default()))
                .collect(),
        ),
        _ => JValue::Null,
    }
}

/// secrets-rotate.cjs `sqlTable`: the account's value in `column`, compare-and-swap writes.
struct SqlTable<'a> {
    app: &'a App,
    table: &'static str,
    column: &'static str,
}

impl Table for SqlTable<'_> {
    fn name(&self) -> &str {
        self.table
    }
    fn rows(&mut self) -> Result<Vec<Row>, String> {
        let writer = self
            .app
            .writes
            .writer()
            .map_err(|_| "the database could not be opened".to_string())?;
        let (table, column) = (self.table, self.column);
        writer
            .read(|r| {
                let present = r
                    .row(
                        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                        [table],
                        |row| row.get::<_, i64>(0),
                    )?
                    .is_some();
                if !present {
                    return Ok(Vec::new());
                }
                // The statement core runs (no ORDER BY: the table's own order).
                let sql = format!("SELECT user_id, {column} AS value FROM {table} WHERE {column} IS NOT NULL AND {column} != ''");
                r.rows(&sql, [], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
            })
            .map(|rows| {
                rows.into_iter()
                    .map(|(user, value)| Row {
                        reference: JValue::Arr(vec![JValue::Str(user.clone())]),
                        value,
                        user: Some(user),
                    })
                    .collect()
            })
            .map_err(|_| "the table could not be read".to_string())
    }
    fn write(&mut self, row: &Row, value: &str) -> Result<bool, String> {
        let writer = self
            .app
            .writes
            .writer()
            .map_err(|_| "the database could not be opened".to_string())?;
        let user = row.user.as_deref().unwrap_or_default();
        let changed = writer.write(|tx| match self.table {
            "storage_connections" => tx.reseal_storage_connection(
                server_store::RustStorageIntegrations::owned(),
                user,
                value,
                &row.value,
            ),
            _ => tx.reseal_chatgpt_tokens(
                server_store::RustChatgptOAuth::owned(),
                user,
                value,
                &row.value,
            ),
        });
        match changed {
            Ok(n) => Ok(n > 0),
            Err(_) => Err("the row could not be written".to_string()),
        }
    }
}

/// secrets-rotate.cjs `googleDriveUserFiles`: every `*.sealed` file in `google-drive-users`, in
/// `Array#sort` order, checked to open under the current key; never written.
struct DriveFiles {
    dir: PathBuf,
    sealer: Option<google_token_seal::Sealer>,
}

impl Table for DriveFiles {
    fn name(&self) -> &str {
        DRIVE_TABLE
    }
    fn rows(&mut self) -> Result<Vec<Row>, String> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err("the Google Drive token folder could not be read".to_string()),
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sealed"))
            .collect();
        names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        Ok(names
            .into_iter()
            .map(|n| Row {
                reference: JValue::Str(n.clone()),
                value: n,
                user: None,
            })
            .collect())
    }
    fn write(&mut self, _row: &Row, _value: &str) -> Result<bool, String> {
        Err("the Google Drive token files are the Drive broker's to write".to_string())
    }
    fn check(&mut self, row: &Row) -> Option<Result<Status, String>> {
        let Ok(bytes) = std::fs::read(self.dir.join(&row.value)) else {
            return Some(Err("the token file could not be read".to_string()));
        };
        let text = String::from_utf8_lossy(&bytes);
        let sealer = self.sealer.as_ref();
        Some(drive_check(&text, |t| {
            sealer.ok_or(KeyError)?.open(t).map_err(|_| KeyError)
        }))
    }
}

fn internal(message: &str) -> Response<Body> {
    error(StatusCode::INTERNAL_SERVER_ERROR, message)
}

pub async fn serve(app: &Arc<App>, req: Request<Body>) -> Response<Body> {
    let (parts, _body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let now = identity::now_ms();
    let layer = Arc::clone(&app.identity);
    let id =
        match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, ROUTE, now)).await {
            Ok(Ok(id)) => id,
            Ok(Err(r)) => return r.into_response(),
            Err(_) => return identity::Rejection::Unavailable.into_response(),
        };
    // routes/auth.cjs: every /api/admin/ path is refused to members first.
    if id.user.role != server_auth::Role::Admin {
        return error(StatusCode::FORBIDDEN, "administrator required");
    }
    // The rotation runs to its end in its own task, whatever happens to this request (a client that
    // goes away, a proxy timeout): the passes, the audit row and the one-at-a-time lock all belong to
    // the work, not to the connection.
    let app = Arc::clone(app);
    let actor = id.user.id.clone();
    match tokio::spawn(async move { run(app, actor).await }).await {
        Ok(response) => response,
        Err(_) => internal("secrets rotation could not finish"),
    }
}

/// One whole rotation, in core's order.
async fn run(app: Arc<App>, actor: String) -> Response<Body> {
    let Some(data_dir) = app.config.data_dir.clone() else {
        return internal("secrets rotation is not available");
    };
    let Some(mcp) = app.mcp.as_ref().map(Arc::clone) else {
        return internal("the Rust front's MCP credential re-seal is not available");
    };
    let _one = RUNNING.lock().await;
    // 1. The provider keys.
    let dir = data_dir.clone();
    let providers = tokio::task::spawn_blocking(move || {
        let keys = crate::provider_routes::load_keys(&dir)?;
        let report = crate::provider_reseal::run(
            &dir,
            &crate::provider_reseal::Scope::All,
            &keys,
            crate::provider_reseal::fresh_nonce,
        );
        // The bytes the internal store answers, read in their written order.
        wtf16::parse(&report.json().to_string()).ok()
    })
    .await;
    let Some(providers) = providers
        .ok()
        .flatten()
        .and_then(|b| pass_of(&b, provider_ref, None))
    else {
        return internal(
            "the Rust front could not re-seal the provider keys (credential store unavailable)",
        );
    };
    // 2. The MCP credentials.
    let Some(mcp_pass) = (match mcp.reseal(&app).await {
        Ok(report) => pass_of(&report.json(), mcp_ref, Some(&MCP_TABLES)),
        Err(_) => None,
    }) else {
        return internal(
            "the Rust front could not re-seal the MCP credentials (credential store unavailable)",
        );
    };
    // 3. The Google Drive token files (soft).
    let drive_front = match crate::gdrive::of(&app) {
        None => DriveFront::Error(drive_front_error(Some(503))),
        Some(drive) => match tokio::task::spawn_blocking(move || drive.reseal_users()).await {
            Ok(Ok(r)) => DriveFront::Counts(Counts {
                current: r.current as u64,
                rotated: r.rotated as u64,
                upgraded: 0,
                empty: r.empty as u64,
                failed: r.failed as u64,
            }),
            Ok(Err(fail)) => DriveFront::Error(drive_front_error(Some(fail.status))),
            Err(_) => DriveFront::Error(drive_front_error(Some(502))),
        },
    };
    // 4 and 5: this process's tables, the merge, the audit row.
    let done = tokio::task::spawn_blocking(move || -> Result<String, &'static str> {
        let keys = DiskKeys(
            crate::provider_routes::load_keys(&data_dir)
                .ok_or("secrets rotation could not finish (credential store unavailable)")?,
        );
        let report = own_tables(&app, &data_dir, &keys);
        let report = merge(report, &providers, &mcp_pass, &drive_front, keys.0.has_previous());
        let detail = audit_detail(&report);
        // authService.audit throws in Node when the row cannot be written: a 500 there too.
        let audited = app.writes.writer().ok().map(|w| {
            w.write(|tx| {
                tx.execute(
                    "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",
                    server_store::rusqlite::params![actor, AUDIT_ACTION, detail, identity::now_ms()],
                )
            })
        });
        match audited {
            Some(Ok(_)) => Ok(report.json()),
            _ => Err("the secrets rotation finished, but its audit row could not be written"),
        }
    })
    .await;
    match done {
        Ok(Ok(text)) => reply::json_text(StatusCode::OK, text, true),
        Ok(Err(message)) => internal(message),
        Err(_) => internal("secrets rotation could not finish"),
    }
}

/// secrets.cjs `rotate({ tables: rotationTables(...) })`.
fn own_tables(app: &App, data_dir: &Path, keys: &DiskKeys) -> secrets_rotate::Report {
    let (current, _) = keys
        .0
        .derive_with(|k| google_token_seal::derive_secret_key(k, google_token_seal::USER_LABEL));
    let mut storage = SqlTable {
        app,
        table: "storage_connections",
        column: "secret",
    };
    let mut chatgpt = SqlTable {
        app,
        table: "chatgpt_oauth_tokens",
        column: "data_enc",
    };
    let mut drive = DriveFiles {
        dir: data_dir.join("google-drive-users"),
        sealer: google_token_seal::Sealer::new(current.as_slice()).ok(),
    };
    rotate(keys, &mut [&mut storage, &mut chatgpt, &mut drive])
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn pass_reading_follows_core_clients() {
        let body = wtf16::parse(r#"{"ok":true,"tables":{"b":{"current":1,"rotated":0,"upgraded":0,"empty":0,"failed":1},"a":{"current":0,"rotated":0,"upgraded":0,"empty":0,"failed":0}},"failures":[{"table":"b","ref":1.5,"error":"x"}]}"#).unwrap();
        let pass = pass_of(&body, provider_ref, None).unwrap();
        assert_eq!(
            pass.tables
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert_eq!(pass.failures[0].reference, JValue::Null);
        let bad = wtf16::parse(r#"{"ok":true,"tables":{"a":{"current":-1,"rotated":0,"upgraded":0,"empty":0,"failed":0}},"failures":[]}"#).unwrap();
        assert!(pass_of(&bad, provider_ref, None).is_none());
        let other = wtf16::parse(r#"{"ok":true,"tables":{"qa_table":{"current":0,"rotated":0,"upgraded":0,"empty":0,"failed":0}},"failures":[{"table":"qa_table","ref":null,"error":null}]}"#).unwrap();
        assert!(pass_of(&other, mcp_ref, Some(&MCP_TABLES)).is_none());
        assert_eq!(pass_of(&other, mcp_ref, None).unwrap().failures[0].error, "");
        assert_eq!(
            mcp_ref(&wtf16::parse(r#"["u",2,"x"]"#).unwrap()),
            wtf16::parse(r#"["u","2"]"#).unwrap()
        );
    }
}
