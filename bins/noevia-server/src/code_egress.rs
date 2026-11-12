//! The code-sandbox egress control, native (port row V; crates/egress-issuer). The front is the
//! only issuer of the egress proxy's control key and of the signed task grants:
//!
//! - it generates, persists (mode 0600, `code-egress-issuer.json`) and publishes the shared proxy
//!   key (mode 0440, `CODE_EGRESS_CONTROL_KEY_FILE`, exactly the contract the Node publisher had),
//!   and a restart keeps that key. On the first start after the retired Node issuer it adopts Node's
//!   published key; the proxy is still initialized under it by Node's Ready, so the first Initial is
//!   refused and the front rotates the key once, at once (no conflict wait), then is ready;
//! - it keeps the authenticated session with the proxy (Ready, 5 s renewal, recovery, re-key after
//!   signed refusals) from its own start, whether or not Node is running;
//! - Node asks it, over the internal store listener (`x-noevia-store-token`), for a task's grant and
//!   hands the token to the task. Node never sees the key.
//!
//! `POST /v1/egress/status`   `{}` -> 200 `{ configured, available, endpoint }`
//! `POST /v1/egress/grant`    `{ taskId, domains }` -> 200 `{ handle, taskId, domains, token }`
//! `POST /v1/egress/activity` `{ handle, budgetMs? }` -> 200 `{ report }`
//! `POST /v1/egress/revoke`   `{ handle, budgetMs? }` -> 200 `{ confirmed, report }`
//! `POST /v1/egress/forget`   `{ handle, snapshotId }` -> 200 `{ confirmed }`
//!
//! Every failure to issue, report or revoke is 503 `{ error, code: "EGRESS_UNAVAILABLE", reason }`
//! (a body that is not what is asked for is 400). Unreachable proxy, an unprepared key directory,
//! a handle from a previous incarnation: all refuse; nothing is retried on Node's behalf and no
//! grant is silently re-acquired. No key, token or path is ever logged or put in an error.
use crate::config::Config;
use egress_issuer::files::{IssuerFiles, KeyKeeper, Master, Opened, Rng};
use egress_issuer::{
    Client, ClientConfig, ControlError, Handle, Reason, SystemClock, TcpTransport,
};
use serde_json::{json, Value};
use server_auth::secrets::SecretKeys;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use zeroize::Zeroizing;

const DEFAULT_TOKEN_TTL_MS: u64 = 6 * 60 * 60 * 1000;
const DEFAULT_GRANT_LIFETIME_MS: u64 = 2 * 60 * 60 * 1000;
/// How often a failed bring-up (secrets not created yet, key directory not prepared) is retried.
const OPEN_RETRY: std::time::Duration = std::time::Duration::from_secs(5);
const GRANT_KEY_LABEL: &str = "code-egress-grant";
const STATE_FILE: &str = "code-egress-issuer.json";
/// The directory of the private state, inside the directory of the published key.
const STATE_DIR: &str = "issuer";

/// The deployment's egress profile (`CODE_EGRESS_*`), validated as Node's `startEgressFromEnv` did:
/// anything wrong refuses to start.
#[derive(Clone)]
pub struct Settings {
    pub endpoint: String,
    pub key_file: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub ttl_ms: u64,
    pub lifetime_ms: u64,
    pub state_file: PathBuf,
    pub data_dir: PathBuf,
    previous_key_env: Option<Zeroizing<String>>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

fn canonical_absolute(path: &Path) -> bool {
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return false;
    }
    let mut rebuilt = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::RootDir => {}
            Component::Normal(p) => rebuilt.push(p),
            _ => return false,
        }
    }
    rebuilt == path
}

impl Settings {
    /// `Ok(None)`: egress is not configured (`CODE_EGRESS_PORT` unset or `0`).
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let bad = || "the code egress profile is invalid (CODE_EGRESS_*)".to_string();
        let raw_port = get("CODE_EGRESS_PORT").unwrap_or_default();
        let raw_port = raw_port.trim();
        if raw_port.is_empty() || raw_port == "0" {
            return Ok(None);
        }
        let port_ok = raw_port.len() <= 5
            && !raw_port.starts_with('0')
            && raw_port.bytes().all(|c| c.is_ascii_digit())
            && raw_port.parse::<u32>().is_ok_and(|p| p <= 65535);
        let imp = get("CODE_EGRESS_IMPL").unwrap_or_default();
        let imp = imp.trim();
        if !port_ok || !(imp.is_empty() || imp == "rust") {
            return Err(bad());
        }
        let host = get("CODE_EGRESS_RUST_HOST")
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "egress-rs-code".into());
        if host.len() > 253
            || !host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
        {
            return Err(bad());
        }
        let integer = |name: &str, zero: bool| -> Result<u32, String> {
            let v = get(name).unwrap_or_default();
            let ok = (v == "0" || (!v.starts_with('0') && !v.is_empty()))
                && v.len() <= 10
                && v.bytes().all(|c| c.is_ascii_digit());
            let n: u64 = if ok {
                v.parse().map_err(|_| bad())?
            } else {
                return Err(bad());
            };
            if n >= 0xffff_ffff || (!zero && n == 0) {
                return Err(bad());
            }
            Ok(n as u32)
        };
        let uid = integer("CODE_EGRESS_KEY_UID", true)?;
        let gid = integer("CODE_EGRESS_KEY_GID", false)?;
        let positive = |name: &str, fallback: u64| -> Result<u64, String> {
            match get(name).filter(|v| !v.is_empty()) {
                None => Ok(fallback),
                Some(v) => v
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0 && *n <= 9_007_199_254_740_991)
                    .ok_or_else(bad),
            }
        };
        let ttl_ms = positive("CODE_EGRESS_TOKEN_TTL_MS", DEFAULT_TOKEN_TTL_MS)?;
        let lifetime_ms = positive("CODE_EGRESS_GRANT_LIFETIME_MS", DEFAULT_GRANT_LIFETIME_MS)?;
        let key_file = PathBuf::from(get("CODE_EGRESS_CONTROL_KEY_FILE").unwrap_or_default());
        if !canonical_absolute(&key_file) {
            return Err(bad());
        }
        let data_dir = get("UI_DATA_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .ok_or("CODE_EGRESS_PORT (the egress issuer) needs UI_DATA_DIR")?;
        // The private state lives beside the published key, in a directory of its own inside the key
        // directory (the issuer's uid, 0750, already required of it), not in UI_DATA_DIR: that mount
        // is whatever the host made it (the live one is owned by a different uid), and a state
        // directory the issuer does not own is refused. The key directory is the one place the issuer
        // is known to own, on every deployment that runs this profile.
        let state_file = match get("CODE_EGRESS_ISSUER_STATE_FILE").filter(|v| !v.is_empty()) {
            Some(p) => PathBuf::from(p),
            None => key_file
                .parent()
                .map_or_else(PathBuf::new, |d| d.join(STATE_DIR).join(STATE_FILE)),
        };
        if !canonical_absolute(&state_file) {
            return Err(bad());
        }
        Ok(Some(Self {
            endpoint: format!("{host}:{raw_port}"),
            key_file,
            uid,
            gid,
            ttl_ms,
            lifetime_ms,
            state_file,
            data_dir,
            previous_key_env: get("SECRETS_KEY_PREVIOUS")
                .filter(|v| !v.is_empty())
                .map(Zeroizing::new),
        }))
    }
}

enum Phase {
    /// Bring-up has not succeeded yet; the reason is the last one (diagnostics only).
    Pending(Reason),
    Up(Client),
}

pub struct Issuer {
    settings: Settings,
    keys: OnceLock<Arc<SecretKeys>>,
    phase: Mutex<Phase>,
    transport: Arc<dyn egress_issuer::Transport>,
}

fn log(event: &'static str, fields: &[(&'static str, String)]) {
    let mut map = serde_json::Map::new();
    map.insert("event".into(), Value::String(event.into()));
    for (k, v) in fields {
        let value = match (*k, v.as_str()) {
            ("recovered", "true") => Value::Bool(true),
            ("recovered", "false") => Value::Bool(false),
            ("downMs", n) => n
                .parse::<u64>()
                .map_or_else(|_| Value::String(n.into()), Value::from),
            _ => Value::String(v.clone()),
        };
        map.insert((*k).into(), value);
    }
    println!("[egress] {}", Value::Object(map));
}

impl Issuer {
    pub fn new(config: &Config) -> Option<Arc<Self>> {
        Self::with_transport(config.code_egress.clone()?, Arc::new(TcpTransport))
    }

    pub fn with_transport(
        settings: Settings,
        transport: Arc<dyn egress_issuer::Transport>,
    ) -> Option<Arc<Self>> {
        Some(Arc::new(Self {
            settings,
            keys: OnceLock::new(),
            phase: Mutex::new(Phase::Pending(Reason::Unavailable)),
            transport,
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn master(self: &Arc<Self>) -> Master {
        let me = Arc::clone(self);
        Arc::new(move || {
            // Node's createSecretStore loads its keys once at start; so does this, on first need
            // (a first boot may not have created secrets.key yet).
            let keys = match me.keys.get() {
                Some(k) => Arc::clone(k),
                None => {
                    let loaded = server_auth::secrets::load_existing(
                        &me.settings.data_dir,
                        me.settings.previous_key_env.as_deref().map(String::as_str),
                    )
                    .map_err(|_| ControlError(Reason::Key))?;
                    Arc::clone(me.keys.get_or_init(|| Arc::new(loaded)))
                }
            };
            let (derived, _) =
                keys.derive_with(|k| google_token_seal::derive_secret_key(k, GRANT_KEY_LABEL));
            Ok(*derived)
        })
    }

    fn open_once(self: &Arc<Self>) -> Result<(Client, Opened), ControlError> {
        let s = &self.settings;
        let files = IssuerFiles {
            key_file: s.key_file.clone(),
            uid: s.uid,
            gid: s.gid,
            state_file: s.state_file.clone(),
        };
        let rng: Rng = Arc::new(|b: &mut [u8]| {
            use rand_core::{OsRng, RngCore};
            OsRng.fill_bytes(b);
        });
        let (keeper, how) = KeyKeeper::open(files, self.master(), rng)?;
        let key = keeper.key();
        let persisted = keeper.persisted_epoch();
        let client = Client::new(
            ClientConfig::new(s.endpoint.clone(), s.ttl_ms, s.lifetime_ms),
            key,
            Some(keeper),
            persisted,
            Arc::new(SystemClock::new()),
            Arc::clone(&self.transport),
            Arc::new(log),
        )?;
        Ok((client, how))
    }

    /// Brings the issuer up in the background: publication, then the session. Never fatal to the
    /// front: until it succeeds every request is refused.
    pub fn start(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let attempt = Arc::clone(&me);
                let opened = tokio::task::spawn_blocking(move || attempt.open_once()).await;
                match opened {
                    Ok(Ok((client, how))) => {
                        let how = match how {
                            Opened::Fresh => "published",
                            Opened::Kept => "kept",
                            Opened::Restored => "restored",
                            Opened::Superseded => "superseded",
                            Opened::Adopted => "adopted",
                        };
                        log(
                            "egress.issuer_started",
                            &[
                                ("key", how.to_string()),
                                ("endpoint", client.endpoint().to_string()),
                            ],
                        );
                        client.start();
                        *me.lock() = Phase::Up(client);
                        return;
                    }
                    Ok(Err(e)) => {
                        let first = matches!(&*me.lock(), Phase::Pending(Reason::Unavailable));
                        if first {
                            log(
                                "egress.issuer_unavailable",
                                &[("reason", e.0.as_str().to_string())],
                            );
                        }
                        *me.lock() = Phase::Pending(e.0);
                    }
                    Err(_) => {}
                }
                tokio::time::sleep(OPEN_RETRY).await;
            }
        });
    }

    fn client(&self) -> Result<Client, ControlError> {
        match &*self.lock() {
            Phase::Up(c) => Ok(c.clone()),
            Phase::Pending(_) => Err(ControlError(Reason::Unavailable)),
        }
    }

    pub fn status(&self) -> Value {
        let client = self.client().ok();
        json!({
            "configured": true,
            "available": client.as_ref().is_some_and(Client::available),
            "endpoint": self.settings.endpoint,
        })
    }

    pub async fn grant(&self, task: &str, domains: &[String]) -> Result<Handle, ControlError> {
        self.client()?.grant(task, domains).await
    }
    pub async fn activity(&self, handle: &str, budget: Option<f64>) -> Result<Value, ControlError> {
        Ok(self.client()?.activity(handle, budget).await?.to_json())
    }
    pub async fn revoke(&self, handle: &str, budget: Option<f64>) -> Result<Value, ControlError> {
        let done = self.client()?.revoke(handle, budget).await?;
        Ok(json!({ "confirmed": done.confirmed, "report": done.report.to_json() }))
    }
    pub async fn forget(&self, handle: &str, snapshot: &str) -> Result<bool, ControlError> {
        self.client()?.forget(handle, snapshot).await
    }
}

fn unavailable(reason: Reason) -> (u16, Value) {
    (
        503,
        json!({ "error": "native egress is unavailable", "code": "EGRESS_UNAVAILABLE", "reason": reason.as_str() }),
    )
}

fn bad(message: &str) -> (u16, Value) {
    (400, json!({ "error": message }))
}

/// One internal operation, as (status, body). `issuer` is `None` when egress is not configured: 503.
pub async fn internal_op(issuer: Option<Arc<Issuer>>, op: &str, raw: &[u8]) -> (u16, Value) {
    let Some(issuer) = issuer else {
        return unavailable(Reason::Unavailable);
    };
    let Ok(body) = serde_json::from_slice::<Value>(raw) else {
        return bad("invalid JSON");
    };
    let text = |k: &str| body.get(k).and_then(Value::as_str);
    let budget = || -> Result<Option<f64>, (u16, Value)> {
        match body.get("budgetMs") {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_f64()
                .filter(|b| b.is_finite())
                .map(Some)
                .ok_or_else(|| bad("budgetMs must be a number")),
        }
    };
    match op {
        "status" => (200, issuer.status()),
        "grant" => {
            let (Some(task), Some(domains)) = (
                text("taskId"),
                body.get("domains").and_then(Value::as_array),
            ) else {
                return bad("taskId and domains required");
            };
            let Some(domains) = domains
                .iter()
                .map(|d| d.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
            else {
                return unavailable(Reason::Hosts);
            };
            match issuer.grant(task, &domains).await {
                Ok(h) => (
                    200,
                    json!({ "handle": h.id, "taskId": h.task_id, "domains": h.domains, "token": h.token }),
                ),
                Err(e) => unavailable(e.0),
            }
        }
        "activity" | "revoke" => {
            let Some(handle) = text("handle") else {
                return bad("handle required");
            };
            let budget = match budget() {
                Ok(b) => b,
                Err(e) => return e,
            };
            let done = if op == "activity" {
                issuer
                    .activity(handle, budget)
                    .await
                    .map(|report| json!({ "report": report }))
            } else {
                issuer.revoke(handle, budget).await
            };
            match done {
                Ok(v) => (200, v),
                Err(e) => unavailable(e.0),
            }
        }
        "forget" => {
            let (Some(handle), Some(snapshot)) = (text("handle"), text("snapshotId")) else {
                return bad("handle and snapshotId required");
            };
            match issuer.forget(handle, snapshot).await {
                Ok(confirmed) => (200, json!({ "confirmed": confirmed })),
                Err(e) => unavailable(e.0),
            }
        }
        _ => (404, json!({ "error": "not found" })),
    }
}

/// `status` for a deployment without egress.
pub fn unconfigured_status() -> Value {
    json!({ "configured": false, "available": false, "endpoint": null })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> Result<Option<Settings>, String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        Settings::from_lookup(&|k: &str| map.get(k).cloned())
    }
    const BASE: [(&str, &str); 5] = [
        ("CODE_EGRESS_PORT", "8040"),
        (
            "CODE_EGRESS_CONTROL_KEY_FILE",
            "/run/noevia-egress-control/current.key",
        ),
        ("CODE_EGRESS_KEY_UID", "0"),
        ("CODE_EGRESS_KEY_GID", "1006"),
        ("UI_DATA_DIR", "/data"),
    ];
    fn with(extra: &[(&'static str, &'static str)]) -> Result<Option<Settings>, String> {
        let mut pairs: Vec<(&str, &str)> = BASE
            .iter()
            .filter(|(k, _)| !extra.iter().any(|(e, _)| e == k))
            .copied()
            .collect();
        pairs.extend(extra.iter().copied());
        lookup(&pairs)
    }

    #[test]
    fn the_profile_is_validated_as_nodes_start_did() {
        assert!(lookup(&[]).unwrap().is_none());
        assert!(with(&[("CODE_EGRESS_PORT", "0")]).unwrap().is_none());
        assert!(with(&[("CODE_EGRESS_PORT", "")]).unwrap().is_none());
        let s = with(&[]).unwrap().unwrap();
        assert_eq!(
            (s.endpoint.as_str(), s.uid, s.gid, s.ttl_ms, s.lifetime_ms),
            ("egress-rs-code:8040", 0, 1006, 21_600_000, 7_200_000)
        );
        // Beside the published key (the issuer owns that directory), not in UI_DATA_DIR.
        assert_eq!(
            s.state_file,
            std::path::PathBuf::from("/run/noevia-egress-control/issuer/code-egress-issuer.json")
        );
        assert_eq!(
            with(&[("CODE_EGRESS_ISSUER_STATE_FILE", "/elsewhere/state.json")])
                .unwrap()
                .unwrap()
                .state_file,
            std::path::PathBuf::from("/elsewhere/state.json")
        );
        assert_eq!(
            with(&[
                ("CODE_EGRESS_RUST_HOST", "proxy.internal"),
                ("CODE_EGRESS_IMPL", "rust")
            ])
            .unwrap()
            .unwrap()
            .endpoint,
            "proxy.internal:8040"
        );
        for bad in [
            &[("CODE_EGRESS_PORT", "-1")][..],
            &[("CODE_EGRESS_PORT", "65536")],
            &[("CODE_EGRESS_PORT", "1e3")],
            &[("CODE_EGRESS_PORT", "invalid")],
            &[("CODE_EGRESS_IMPL", "node")],
            &[("CODE_EGRESS_KEY_UID", "")],
            &[("CODE_EGRESS_KEY_GID", "0")],
            &[("CODE_EGRESS_KEY_GID", "4294967295")],
            &[("CODE_EGRESS_CONTROL_KEY_FILE", "")],
            &[("CODE_EGRESS_CONTROL_KEY_FILE", "relative/current.key")],
            &[("CODE_EGRESS_CONTROL_KEY_FILE", "/run/../current.key")],
            &[("CODE_EGRESS_RUST_HOST", "http://private-secret/")],
            &[("CODE_EGRESS_TOKEN_TTL_MS", "0")],
            &[("CODE_EGRESS_GRANT_LIFETIME_MS", "abc")],
            &[("UI_DATA_DIR", "")],
        ] {
            let e = with(bad).unwrap_err();
            assert!(!e.contains("private-secret"), "{bad:?}");
        }
    }

    #[test]
    fn the_grant_key_derives_from_the_secrets_key_exactly_as_node_does() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secrets.key"), [1u8; 32]).unwrap();
        let settings = Settings {
            data_dir: dir.path().into(),
            ..with(&[]).unwrap().unwrap()
        };
        let issuer = Issuer::with_transport(settings, Arc::new(TcpTransport)).unwrap();
        let master = issuer.master()().unwrap();
        // node: hkdfSync('sha256', Buffer.alloc(32, 1), Buffer.alloc(0), 'noevia:code-egress-grant', 32)
        let want = "9398603420343524607ca08c28e26b4e932c9a2a86cb5e335d4c809c53d41ca3";
        assert_eq!(egress_issuer::wire::hex(&master), want);
        // No secrets key yet (a first boot): refused, never a made-up master.
        let empty = tempfile::tempdir().unwrap();
        let settings = Settings {
            data_dir: empty.path().into(),
            ..with(&[]).unwrap().unwrap()
        };
        let issuer = Issuer::with_transport(settings, Arc::new(TcpTransport)).unwrap();
        assert_eq!(issuer.master()().unwrap_err().0, Reason::Key);
    }
}
