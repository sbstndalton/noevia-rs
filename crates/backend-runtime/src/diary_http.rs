//! Original `/api/diary/files` and `/api/diary/file` contracts over managed storage.
//! Account Writer stays held through the corpus operation: session/account/add-on
//! revocation cannot commit between authentication and disclosure/mutation.
use crate::{
    diary::{Corpus, WriteCondition},
    journal::Journal,
    Control, Fault, Result,
};
use serde_json::{json, Value};
use server_auth::{Authenticator, Creds, Refusal};
use server_store::Writer;
use std::{collections::HashMap, sync::Arc, time::Duration};
pub struct Tenant {
    pub corpus: Corpus,
    pub journal: Arc<Journal>,
}
pub struct DiaryHttp {
    tenants: HashMap<String, Tenant>,
    replay: HashMap<String, crate::replay::Replay>,
    workspace: Option<diary_workspace_transaction::ProtectionConfigDto>,
}
impl DiaryHttp {
    pub fn new(tenants: HashMap<String, Tenant>) -> Result<Self> {
        if tenants.iter().any(|(user, t)| user != t.corpus.tenant_id()) {
            return Err(Fault::Forbidden);
        }
        Ok(Self {
            tenants,
            replay: HashMap::new(),
            workspace: None,
        })
    }
    pub fn with_workspace(
        mut self,
        config: diary_workspace_transaction::ProtectionConfigDto,
    ) -> Self {
        self.workspace = Some(config);
        self
    }
    pub fn with_replay(mut self, layout: crate::replay::Layout) -> Result<Self> {
        for user in self.tenants.keys() {
            self.replay
                .insert(user.clone(), crate::replay::Replay::new(layout.clone())?);
        }
        Ok(self)
    }
    /// Account Writer remains held through the whole pass, including corpus commit.
    /// No revoked/disabled/remote tenant can be acted on by the background worker.
    pub fn replay_pending(&self, writer: &Writer, control: &Control) -> Result<usize> {
        let mut count = 0;
        for (user, replay) in &self.replay {
            control.check()?;
            let tenant = self.tenants.get(user).ok_or(Fault::Unavailable)?;
            let mut result = Ok(0);
            writer.write(|tx| {
                let allowed=tx.reader().row("SELECT 1 FROM users u JOIN user_features f ON f.user_id=u.id LEFT JOIN storage_connections s ON s.user_id=u.id WHERE u.id=? AND u.disabled_at IS NULL AND f.diary_enabled=1 AND (s.kind IS NULL OR s.kind='local')",[user],|r|r.get::<_,i64>(0))?.is_some();
                if allowed { result=replay.pending(&tenant.corpus,&tenant.journal,100,false,control); }
                Ok(())
            }).map_err(|_|Fault::Unavailable)?;
            count += result?;
        }
        Ok(count)
    }
    pub fn journals(&self) -> HashMap<String, Arc<Journal>> {
        self.tenants
            .iter()
            .map(|(u, t)| (u.clone(), Arc::clone(&t.journal)))
            .collect()
    }
    pub fn corpora(&self) -> HashMap<String, Arc<Corpus>> {
        self.tenants
            .iter()
            .map(|(user, t)| (user.clone(), Arc::new(t.corpus.clone())))
            .collect()
    }
    pub fn handles(path: &str) -> bool {
        matches!(
            path,
            "/api/diary/files"
                | "/api/diary/file"
                | "/api/diary/entries/edit"
                | "/api/diary/history"
                | "/api/diary/today"
                | "/api/diary/source"
                | "/api/diary/workspace-trash"
                | "/api/diary/workspace-import"
        )
    }
    fn trash(
        &self,
        tenant: &Tenant,
        method: &str,
        query: &str,
        body: &[u8],
        now: i64,
        control: &Control,
    ) -> Result<(u16, Value)> {
        use sha2::{Digest, Sha256};
        if !matches!(method, "GET" | "POST") {
            return Ok(error(405, "Method not allowed"));
        }
        let config = self.workspace.clone().ok_or(Fault::Unavailable)?;
        let namespace = tenant.corpus.workspace_namespace(control)?;
        let mut request = json!({"version":1,"dbPath":"","expectedTenant":"","operationNamespace":namespace,"op":"trashList","limit":100});
        if method == "GET" {
            if let Some((_, after)) =
                url::form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == "after")
            {
                if !after.is_empty() {
                    request.as_object_mut().ok_or(Fault::Protocol)?.insert(
                        "cursor".into(),
                        json!(uuid::Uuid::parse_str(&after)
                            .map_err(|_| Fault::Invalid)?
                            .to_string()),
                    );
                }
            }
        } else {
            if body.len() > 4096 {
                return Ok(error(413, "Request body too large"));
            }
            if tenant.journal.pending_count()? != 0 {
                return Ok(error(
                    409,
                    "Finish pending Diary writes before changing Trash.",
                ));
            }
            let body: Value = serde_json::from_slice(body).map_err(|_| Fault::Invalid)?;
            let action = body
                .get("action")
                .and_then(Value::as_str)
                .filter(|s| matches!(*s, "trash" | "restore"))
                .ok_or(Fault::Invalid)?;
            let id = uuid::Uuid::parse_str(
                body.get("id")
                    .and_then(Value::as_str)
                    .ok_or(Fault::Invalid)?,
            )
            .map_err(|_| Fault::Invalid)?
            .to_string();
            // A retry keeps the capsule and mutation identities, while restore has its own receipt.
            let hash = Sha256::digest(format!("{namespace}:{id}:{action}"));
            let bytes: [u8; 16] = hash
                .get(..16)
                .ok_or(Fault::Protocol)?
                .try_into()
                .map_err(|_| Fault::Protocol)?;
            let operation = uuid::Uuid::from_bytes(bytes).to_string();
            request = json!({"version":1,"dbPath":"","expectedTenant":"","operationNamespace":namespace,"op":action,"operationId":operation,"capsuleId":id,"now":now as f64/1000.0,"debounce":3.0});
            if action == "trash" {
                let object = request.as_object_mut().ok_or(Fault::Protocol)?;
                object.insert(
                    "source".into(),
                    body.get("path").cloned().ok_or(Fault::Invalid)?,
                );
                object.insert(
                    "sourceVersion".into(),
                    body.get("version").cloned().ok_or(Fault::Invalid)?,
                );
            }
        }
        let mut request: diary_workspace_transaction::Request =
            serde_json::from_value(request).map_err(|_| Fault::Invalid)?;
        request.config = Some(config);
        let reply = tenant.corpus.workspace(request, control)?;
        let mut value = reply.body().to_value().map_err(|_| Fault::Protocol)?;
        if method == "POST" && reply.status() < 300 {
            value
                .as_object_mut()
                .ok_or(Fault::Protocol)?
                .insert("indexPending".into(), json!(true));
        }
        Ok((reply.status(), value))
    }
    pub fn export(
        &self,
        writer: &Writer,
        auth: &Authenticator,
        creds: &Creds,
        user: &str,
        now: i64,
    ) -> Result<Vec<u8>> {
        let control = Control::new(Duration::from_secs(60))?;
        let mut outcome = None;
        writer
            .write(|tx| {
                let identity =
                    auth.gate_in(&tx.reader(), creds, "/api/diary/workspace-export", now)?;
                outcome = Some(match identity {
                    Ok(identity) if identity.user.id == user && identity.user.diary_enabled => {
                        let kind: Option<String> = tx.reader().row(
                            "SELECT kind FROM storage_connections WHERE user_id=?",
                            [user],
                            |r| r.get(0),
                        )?;
                        if kind.as_deref().is_some_and(|k| k != "local") {
                            Err(Fault::Unavailable)
                        } else {
                            self.tenants
                                .get(user)
                                .ok_or(Fault::Unavailable)
                                .and_then(|tenant| {
                                    if tenant.journal.pending_count()? != 0 {
                                        return Err(Fault::Conflict);
                                    }
                                    let settings =
                                        self.replay.get(user).ok_or(Fault::Unavailable)?.settings();
                                    tenant.corpus.export_zip(&settings, &control)
                                })
                        }
                    }
                    Err(Refusal::Unauthorized) => Err(Fault::Unauthorized),
                    Ok(identity) if !identity.user.diary_enabled => Err(Fault::Missing),
                    _ => Err(Fault::Forbidden),
                });
                Ok(())
            })
            .map_err(|_| Fault::Unavailable)?;
        outcome.ok_or(Fault::Unavailable)?
    }
    #[allow(clippy::too_many_arguments)]
    pub fn handle(
        &self,
        writer: &Writer,
        auth: &Authenticator,
        creds: &Creds,
        user: &str,
        method: &str,
        path: &str,
        query: &str,
        body: &[u8],
        now: i64,
    ) -> Result<(u16, Value)> {
        let mut outcome = None;
        let control = Control::new(Duration::from_secs(30))?;
        writer
            .write(|tx| {
                let identity = match auth.gate_in(&tx.reader(), creds, path, now)? {
                    Ok(identity) if identity.user.id == user => identity,
                    Ok(_) | Err(Refusal::Unauthorized) => {
                        outcome = Some(Err(Fault::Unauthorized));
                        return Ok(());
                    }
                    Err(_) => {
                        outcome = Some(Err(Fault::Forbidden));
                        return Ok(());
                    }
                };
                if !identity.user.diary_enabled {
                    outcome = Some(Ok((404, json!({"error":"Diary add-on is disabled"}))));
                    return Ok(());
                }
                let Some(tenant) = self.tenants.get(user) else {
                    outcome = Some(Err(Fault::Unavailable));
                    return Ok(());
                };
                // This adapter is explicitly managed/local. Remote storage uses its full credential
                // and retry-latch protocol and must not silently become local storage.
                let kind: Option<String> = tx.reader().row(
                    "SELECT kind FROM storage_connections WHERE user_id=?",
                    [user],
                    |r| r.get(0),
                )?;
                if kind.as_deref().is_some_and(|k| k != "local") {
                    outcome = Some(Err(Fault::Unavailable));
                    return Ok(());
                }
                outcome = Some(if path == "/api/diary/workspace-import" {
                    if method != "POST" {
                        Ok(error(405, "Method not allowed"))
                    } else if body.len() > 32 * 1024 * 1024 {
                        Ok(error(413, "Browser imports support ZIP files up to 32 MiB"))
                    } else if tenant.journal.pending_count() != Ok(0) {
                        Ok(error(409, "Finish pending Diary writes before importing."))
                    } else {
                        let params: HashMap<_, _> = url::form_urlencoded::parse(query.as_bytes())
                            .into_owned()
                            .collect();
                        let action = params.get("action").map(String::as_str).unwrap_or("");
                        let fingerprint =
                            params.get("fingerprint").map(String::as_str).unwrap_or("");
                        if !matches!(action, "preview" | "apply")
                            || (action == "apply"
                                && (fingerprint.len() != 64
                                    || !fingerprint
                                        .bytes()
                                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))))
                        {
                            Ok(error(400, "Invalid import action or preview fingerprint"))
                        } else {
                            tenant
                                .corpus
                                .import_zip(
                                    body,
                                    params.get("name").map(String::as_str).unwrap_or(""),
                                    (action == "apply").then_some(fingerprint),
                                    &control,
                                )
                                .map(|v| (200, v))
                        }
                    }
                } else if path == "/api/diary/workspace-trash" {
                    self.trash(tenant, method, query, body, now, &control)
                } else if path == "/api/diary/entries/edit" {
                    if method != "POST" {
                        Ok(error(405, "Method not allowed"))
                    } else if body.len() > 1024 * 1024 {
                        Ok(error(413, "Request body too large"))
                    } else {
                        match serde_json::from_slice::<Value>(body) {
                            Ok(value @ Value::Object(_)) => self
                                .replay
                                .get(user)
                                .ok_or(Fault::Unavailable)
                                .and_then(|replay| {
                                    replay.edit(&tenant.corpus, &tenant.journal, &value, &control)
                                }),
                            _ => Ok(error(400, "Invalid JSON body")),
                        }
                    }
                } else if path == "/api/diary/source" {
                    self.replay
                        .get(user)
                        .ok_or(Fault::Unavailable)
                        .and_then(|r| r.months(&tenant.corpus, &control))
                        .map(|v| (200, v))
                } else if matches!(path, "/api/diary/history" | "/api/diary/today") {
                    let month = url::form_urlencoded::parse(query.as_bytes())
                        .find(|(k, _)| k == "month")
                        .map(|(_, v)| v.into_owned());
                    self.replay
                        .get(user)
                        .ok_or(Fault::Unavailable)
                        .and_then(|r| {
                            r.history(
                                &tenant.corpus,
                                month.as_deref(),
                                chrono::Local::now().date_naive(),
                                &control,
                            )
                        })
                        .map(|v| (200, v))
                } else {
                    tenant.handle(method, path, query, body, now, &control)
                });
                Ok(())
            })
            .map_err(|_| Fault::Unavailable)?;
        outcome.ok_or(Fault::Unavailable)?
    }
}
fn error(status: u16, message: &str) -> (u16, Value) {
    (status, json!({"error":message}))
}
fn directory_path(path: &str) -> bool {
    path.is_empty()
        || (path.chars().count() <= 500
            && !path.contains(['\\', '\0'])
            && !path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == ".." || p.starts_with('.')))
}
impl Tenant {
    fn handle(
        &self,
        method: &str,
        path: &str,
        query: &str,
        body: &[u8],
        now: i64,
        control: &Control,
    ) -> Result<(u16, Value)> {
        if path == "/api/diary/files" {
            if method != "GET" {
                return Ok(error(405, "Method not allowed"));
            }
            let relative = url::form_urlencoded::parse(query.as_bytes())
                .find(|(k, _)| k == "path")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            if !directory_path(&relative) {
                return Ok(error(400, "Choose a path inside the diary folder"));
            }
            let entries = self.corpus.list_files(&relative, control)?;
            if entries.len() > 500 {
                return Ok(error(
                    413,
                    "This folder has too many files; choose a subfolder",
                ));
            }
            return Ok((200, json!({"files":entries})));
        }
        if !matches!(method, "POST" | "PUT") {
            return Ok(error(405, "Method not allowed"));
        }
        if body.len() > 1024 * 1024 {
            return Ok(error(413, "Request body too large"));
        }
        let value: Value = match serde_json::from_slice(body) {
            Ok(Value::Object(o)) => Value::Object(o),
            _ => return Ok(error(400, "Invalid JSON body")),
        };
        let Some(relative) = value.get("path").and_then(Value::as_str) else {
            return Ok(error(400, "Invalid path"));
        };
        if diary_workspace_rules::safe_file_path(relative).is_err() {
            return Ok(error(400, "Choose a path inside the diary folder"));
        }
        if method == "POST" {
            let file = self.corpus.read(relative, control)?;
            if file.as_ref().is_some_and(|f| f.content.len() > 512 * 1024) {
                return Ok(error(413, "File exceeds the 512 KiB editor limit"));
            }
            return Ok((
                200,
                match file {
                    Some(file) => {
                        json!({"path":relative,"content":file.content,"version":file.version})
                    }
                    None => json!({"path":relative,"content":null,"version":null}),
                },
            ));
        }
        let Some(content) = value
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 512 * 1024)
        else {
            return Ok(error(
                400,
                "Content and base version required; maximum 512 KiB",
            ));
        };
        let Some(base) = value.get("version") else {
            return Ok(error(
                400,
                "Content and base version required; maximum 512 KiB",
            ));
        };
        let condition =
            match base {
                Value::Null => WriteCondition {
                    if_match: None,
                    if_none_match: true,
                },
                Value::String(s) => WriteCondition {
                    if_match: Some(s),
                    if_none_match: false,
                },
                _ => return Ok(error(
                    409,
                    "File changed elsewhere. Reopen it before saving; your draft has been kept.",
                )),
            };
        let version = self.corpus.put_editor(
            relative,
            content,
            condition,
            now as f64 / 1000.0,
            control,
            || self.journal.mark_dirty(relative),
        );
        match version {
            Ok(version) => Ok((
                200,
                json!({"path":relative,"content":content,"version":version}),
            )),
            Err(Fault::Conflict) => Ok(error(
                409,
                "File changed elsewhere. Reopen it before saving; your draft has been kept.",
            )),
            Err(e) => Err(e),
        }
    }
}
