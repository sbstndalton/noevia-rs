//! Native DAV storage sink. Lock order is account Writer -> corpus Writer.
//! The account write transaction stays open through the corpus commit, so an account,
//! sharing, feature or app-password revocation cannot commit between its check and
//! the operation. Cross-database crash atomicity/audit uncertainty still needs release proof.
use crate::{
    diary::{Corpus, WriteCondition},
    Control, Fault, Result,
};
use server_dav::{Backend, BackendFuture, Capabilities, Entry, File, Identity, Scope};
use server_store::{Store, StoreError, Writer};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub struct DavBackend {
    store: Arc<Store>,
    writer: Arc<Writer>,
    tenants: HashMap<String, Arc<Corpus>>,
    scope: Scope,
    workspace_config: Option<diary_workspace_transaction::ProtectionConfigDto>,
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(i64::MAX)
}
fn mapped(fault: Fault) -> server_dav::Fault {
    match fault {
        Fault::Missing => server_dav::Fault::NotFound,
        Fault::Forbidden | Fault::Unauthorized => server_dav::Fault::Forbidden,
        Fault::Conflict => server_dav::Fault::Conflict,
        Fault::Capacity => server_dav::Fault::TooMany,
        _ => server_dav::Fault::Unavailable,
    }
}
impl DavBackend {
    pub fn new(
        store: Arc<Store>,
        writer: Arc<Writer>,
        tenants: HashMap<String, Arc<Corpus>>,
        scope: Scope,
    ) -> Result<Self> {
        if tenants
            .iter()
            .any(|(user, corpus)| user != corpus.tenant_id())
        {
            return Err(Fault::Forbidden);
        }
        Ok(Self {
            store,
            writer,
            tenants,
            scope,
            workspace_config: None,
        })
    }
    /// Explicit original corpus protection settings are required before advertising mutations.
    pub fn with_workspace(
        mut self,
        config: diary_workspace_transaction::ProtectionConfigDto,
    ) -> Self {
        self.workspace_config = Some(config);
        self
    }
    fn fenced<T>(
        &self,
        identity: &Identity,
        operation: impl FnOnce(&Corpus, &Control) -> Result<T>,
    ) -> std::result::Result<T, server_dav::Fault> {
        let corpus = self
            .tenants
            .get(&identity.user_id)
            .ok_or(server_dav::Fault::Forbidden)?;
        let control = Control::new(Duration::from_secs(30)).map_err(mapped)?;
        let mut outcome = None;
        self.writer.write(|tx|{
            let allowed=tx.reader().row("SELECT 1 FROM app_passwords a JOIN users u ON u.id=a.user_id JOIN dav_settings d ON d.user_id=u.id JOIN user_features f ON f.user_id=u.id LEFT JOIN storage_connections s ON s.user_id=u.id WHERE a.id=?1 AND a.user_id=?2 AND a.scope=?3 AND u.disabled_at IS NULL AND d.scope=?3 AND f.diary_enabled=1 AND (s.kind IS NULL OR s.kind='local') LIMIT 1",[identity.credential_id.as_str(),identity.user_id.as_str(),self.scope.as_str()],|r|r.get::<_,i64>(0))?.is_some();
            if !allowed{outcome=Some(Err(Fault::Forbidden));return Ok(());}
            control.check().map_err(|_|StoreError::NotReady("DAV deadline".into()))?;
            outcome=Some(operation(corpus,&control));
            if outcome.as_ref().is_some_and(std::result::Result::is_ok){tx.execute("UPDATE app_passwords SET last_used_at=? WHERE id=? AND user_id=?",server_store::rusqlite::params![now_ms(),identity.credential_id,identity.user_id])?;}
            Ok(())
        }).map_err(|_|server_dav::Fault::Unavailable)?;
        outcome
            .ok_or(server_dav::Fault::Unavailable)?
            .map_err(mapped)
    }
    /// All six existing workspace mutation kernels can use the same live revocation fence.
    /// The caller retains the request's stable operation identity across an uncertain reply.
    pub fn operate(
        &self,
        identity: &Identity,
        request: diary_workspace_transaction::Request,
    ) -> std::result::Result<diary_workspace_transaction::Reply, server_dav::Fault> {
        self.fenced(identity, |corpus, control| {
            corpus.workspace(request, control)
        })
    }
}
impl Backend for DavBackend {
    fn workspace_enabled(&self) -> bool {
        self.workspace_config.is_some()
    }
    fn workspace<'a>(
        &'a self,
        identity: &'a Identity,
        body: serde_json::Value,
    ) -> BackendFuture<'a, std::result::Result<(u16, serde_json::Value), server_dav::Fault>> {
        Box::pin(async move {
            let config = self
                .workspace_config
                .clone()
                .ok_or(server_dav::Fault::Unavailable)?;
            self.fenced(identity, |corpus, control| {
                let op = body
                    .get("op")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(Fault::Invalid)?;
                if !matches!(op, "stat" | "delete" | "move" | "copy") {
                    return Err(Fault::Invalid);
                }
                let namespace = corpus.workspace_namespace(control)?;
                // IDs are allocated once per admitted HTTP operation, before the kernel. No
                // automatic transport retry can re-execute an uncertain mutation.
                let mut request = serde_json::json!({"version":1,"dbPath":"","expectedTenant":"",
                    "operationNamespace":namespace,"operationId":uuid::Uuid::new_v4().to_string(),
                    "capsuleId":uuid::Uuid::new_v4().to_string(),"op":op,"source":body.get("path"),
                    "now":now_ms() as f64 / 1000.0,"debounce":3.0});
                for (source, target) in [
                    ("version", "sourceVersion"),
                    ("destination", "destination"),
                    ("destinationVersion", "destinationVersion"),
                    ("overwrite", "overwrite"),
                ] {
                    if let Some(value) = body.get(source) {
                        request
                            .as_object_mut()
                            .ok_or(Fault::Protocol)?
                            .insert(target.into(), value.clone());
                    }
                }
                let mut request: diary_workspace_transaction::Request =
                    serde_json::from_value(request).map_err(|_| Fault::Invalid)?;
                request.config = Some(config);
                let reply = corpus.workspace(request, control)?;
                let body = reply.body().to_value().map_err(|_| Fault::Protocol)?;
                Ok((reply.status(), body))
            })
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { mkdir: true }
    }
    fn authenticate<'a>(
        &'a self,
        username: &'a str,
        password: &'a str,
        scope: Scope,
    ) -> BackendFuture<'a, std::result::Result<Option<Identity>, server_dav::Fault>> {
        Box::pin(async move {
            if scope != self.scope {
                return Ok(None);
            }
            server_auth::app_passwords::verify_dav(&self.store, username, password, scope.as_str())
                .map(|identity| {
                    identity.map(|i| Identity {
                        user_id: i.user_id,
                        credential_id: i.credential_id,
                    })
                })
                .map_err(|_| server_dav::Fault::Unavailable)
        })
    }
    fn allowed<'a>(
        &'a self,
        identity: &'a Identity,
        scope: Scope,
    ) -> BackendFuture<'a, std::result::Result<bool, server_dav::Fault>> {
        Box::pin(async move {
            if scope != self.scope {
                return Ok(false);
            }
            match self.fenced(identity, |_, _| Ok(())) {
                Ok(()) => Ok(true),
                Err(server_dav::Fault::Forbidden) => Ok(false),
                Err(e) => Err(e),
            }
        })
    }
    fn list<'a>(
        &'a self,
        identity: &'a Identity,
        parent: &'a str,
    ) -> BackendFuture<'a, std::result::Result<Vec<Entry>, server_dav::Fault>> {
        Box::pin(
            async move { self.fenced(identity, |corpus, control| corpus.list(parent, control)) },
        )
    }
    fn read<'a>(
        &'a self,
        identity: &'a Identity,
        path: &'a str,
    ) -> BackendFuture<'a, std::result::Result<Option<File>, server_dav::Fault>> {
        Box::pin(async move {
            self.fenced(identity, |corpus, control| {
                Ok(corpus.read(path, control)?.map(|file| File {
                    content: file.content,
                    version: file.version,
                }))
            })
        })
    }
    fn write<'a>(
        &'a self,
        identity: &'a Identity,
        path: &'a str,
        content: &'a str,
        condition: server_dav::WriteCondition,
    ) -> BackendFuture<'a, std::result::Result<String, server_dav::Fault>> {
        Box::pin(async move {
            if content.len() > 512 * 1024 || !path.to_ascii_lowercase().ends_with(".md") {
                return Err(server_dav::Fault::Forbidden);
            }
            self.fenced(identity, |corpus, control| {
                let condition = match &condition {
                    server_dav::WriteCondition::Match(version) => WriteCondition {
                        if_match: Some(version),
                        if_none_match: false,
                    },
                    server_dav::WriteCondition::Create => WriteCondition {
                        if_match: None,
                        if_none_match: true,
                    },
                };
                corpus.put(
                    path,
                    content,
                    condition,
                    now_ms() as f64 / 1000.0,
                    control,
                    || Ok(()),
                )
            })
        })
    }
    fn mkdir<'a>(
        &'a self,
        identity: &'a Identity,
        path: &'a str,
    ) -> BackendFuture<'a, std::result::Result<(), server_dav::Fault>> {
        Box::pin(async move {
            self.fenced(identity, |corpus, control| {
                corpus.mkdir(path, now_ms() as f64 / 1000.0, control)
            })
        })
    }
}
