//! MCP tool discovery, owned by the front (sbstndalton/noevia row T): core `mcp-wiring.cjs`
//! `discoverOneServer` / `discoverMcpTools`, `mcp-boxes.cjs` and the discovery half of `mcpState`.
//!
//! What the actor owns: connecting to every configured server (the operator's `MCP_SERVERS`, then the
//! directory's), listing its tools, converting them, binding the curated boxes, each server's last
//! outcome, the refresh cadence (a ten-minute lifetime, a short one for a run that found nothing, and
//! the steps a failing remote server is retried on) and the coalescing of concurrent callers. What it
//! does not own: running a tool (core's executor still does; it asks this actor which server offers a
//! name through [`Snapshot`], over `POST /v1/mcp/discovery`).
//!
//! # Rules carried over from core
//!
//! - Discovery lists the catalogue with **service credentials only**: a static bearer token, the
//!   directory's shared key, a discovery-only capability token for the in-process server, or the
//!   sign-in / key of the administrator who added a per-account server. A requesting account's
//!   credential is never attached, and the snapshot does not vary by caller. The per-account
//!   credential is attached by the executor at call time.
//! - A disabled administrator's sign-in or key is not trusted for discovery.
//! - A directory server's address is re-checked for public reachability at every discovery.
//! - One server failing never removes another's tools; a failure is per server.
//! - Concurrent callers share one run. A forced call that arrives during a run waits for it and then
//!   runs again, so it never reuses a discovery that started before the change it was asked for.
//! - Failure is remembered: a run with no boxes at all for the failure lifetime; a remote server that
//!   failed for `5 s, 15 s, 45 s, 120 s` (one step per consecutive failed run, the last holding);
//!   the in-process server failing is retried at the next request.
//! - A changed directory table, or a changed readiness of an administrator's credential, forces the
//!   next call to rediscover.
//!
//! # Differences from core (recorded)
//!
//! - The run is a spawned task owned by the actor: a caller that goes away (a request cancelled at any
//!   await) neither cancels nor leaks the run; the in-flight marker is cleared by a guard even if the
//!   task panics, and every waiter then gets [`Unavailable`] (fail closed).
//! - Servers are reported in configured order (core's `Map` held them in the order they finished,
//!   which nothing could rely on); the snapshot's `error` is still the first error to arrive.
//!   Tools are flattened in configured order.
use crate::mcp_manifest::{self, Offered};
use js_json::JValue;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// core `discoveryTtlMs`.
pub const TTL_MS: i64 = 10 * 60 * 1000;
/// core `discoveryFailTtlMs` default.
pub const FAIL_TTL_MS: i64 = 30 * 1000;
/// core `discoveryRetryMs`.
pub const RETRY_MS: [i64; 4] = [5000, 15000, 45000, 120000];

/// The cadence (shortened by tests).
#[derive(Clone, Debug)]
pub struct Timing {
    pub ttl_ms: i64,
    pub fail_ttl_ms: i64,
    pub retry_ms: Vec<i64>,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            ttl_ms: TTL_MS,
            fail_ttl_ms: FAIL_TTL_MS,
            retry_ms: RETRY_MS.to_vec(),
        }
    }
}

/// One configured server, in core's `MCP_SERVERS` shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSpec {
    pub id: String,
    pub url: String,
    /// `none`, `nextcloud`, `bearer`, `internal`, `directory`, `personal` or `oauth`.
    pub auth: String,
    pub directory: bool,
    pub title: Option<String>,
    pub added_by: Option<String>,
    pub token_env: Option<String>,
}

/// What an account is ready for (its live sign-ins and the keys it holds).
#[derive(Clone, Debug, Default)]
pub struct Ready {
    pub signed_in: HashSet<String>,
    pub keys: HashSet<String>,
}

/// The world the actor works in. Every method is a leaf: the rules are the actor's.
pub trait Backend: Send + Sync + 'static {
    fn now_ms(&self) -> i64;
    /// `features.diaryMcpWrite`'s construction value (it decides the Diary box's tools).
    fn diary_mcp_write(&self) -> impl Future<Output = bool> + Send;
    /// The operator's servers, then the directory's. A directory table that cannot be read is
    /// **no** directory servers (fails closed), never the last list.
    fn servers(&self) -> impl Future<Output = Vec<ServerSpec>> + Send;
    /// Changes when the directory table does (a row, a flag or a sealed key); `"unreadable"` while it
    /// cannot be read.
    fn directory_signal(&self) -> impl Future<Output = String> + Send;
    /// A coherent directory identity and server list. Backends with a transactional row read
    /// override this; the default refuses a list bracketed by different identities.
    fn configuration(&self) -> impl Future<Output = Option<(String, Vec<ServerSpec>)>> + Send {
        async {
            let before = self.directory_signal().await;
            let servers = self.servers().await;
            let after = self.directory_signal().await;
            (before == after).then_some((after, servers))
        }
    }
    /// An account's readiness for the per-account servers (empty when it cannot be read).
    fn account_ready(&self, user: &str) -> impl Future<Output = Ready> + Send;
    /// The value of a static-token environment variable, if set and not empty.
    fn static_token(&self, env: &str) -> Option<String>;
    /// A directory server's shared key headers (empty when it has none).
    fn directory_headers(&self, id: &str) -> impl Future<Output = Vec<(String, String)>> + Send;
    /// A discovery-only capability token for the in-process server, or `None` if it cannot be minted.
    fn internal_discovery_token(&self) -> Option<String>;
    fn user_disabled(&self, user: &str) -> impl Future<Output = bool> + Send;
    /// The administrator's access token for an OAuth server: `Ok(None)` when they have not signed in.
    fn oauth_token(
        &self,
        user: &str,
        server: &str,
    ) -> impl Future<Output = Result<Option<String>, String>> + Send;
    /// The administrator's key headers for a per-account server: `Ok(None)` when they hold none.
    fn key_headers(
        &self,
        user: &str,
        server: &str,
    ) -> impl Future<Output = Result<Option<Vec<(String, String)>>, String>> + Send;
    /// Whether a directory server's address is (still) a public one.
    fn url_public(&self, url: &str) -> impl Future<Output = bool> + Send;
    /// `connect` + `listTools` + `disconnect`: the tools as the server sent them, or the failure in
    /// core's words (`MCP 502: …`, `fetch failed`, …).
    fn list_tools(
        &self,
        server: &ServerSpec,
        headers: &[(String, String)],
    ) -> impl Future<Output = Result<Vec<JValue>, String>> + Send;
    /// One log line (no credential, no response body).
    fn log(&self, line: String) {
        eprintln!("{line}");
    }
}

/// A discovery that could not produce an answer: every waiter fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unavailable;

/// One tool a server offers, converted.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub server: String,
    pub tool: JValue,
    /// `annotations.readOnlyHint`, when the server said.
    pub read_only: Option<bool>,
}

/// One server's last discovery.
#[derive(Clone, Debug)]
pub struct Found {
    pub spec: ServerSpec,
    pub error: Option<String>,
    pub discovered_at: i64,
    pub tool_count: usize,
}

/// Everything discovery knows, published whole under one generation.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub generation: u64,
    pub incarnation: String,
    /// `MCP_ENABLED`: at least one server is configured.
    pub enabled: bool,
    pub error: Option<String>,
    pub discovered_at: i64,
    pub failed_at: i64,
    pub remote_fail_streak: u32,
    /// Every configured server, in configured order.
    pub servers: Vec<Found>,
    /// Every (server, tool) offer, servers in configured order.
    pub offers: Vec<Entry>,
    pub boxes: Vec<JValue>,
    /// The curated boxes this run bound against (core's `MCP_TOOLBOX_MANIFEST`), whether or not
    /// anything bound.
    pub manifest: Vec<JValue>,
}

impl Snapshot {
    /// `mcpState.tools`: one entry per name, the first server's.
    pub fn tools(&self) -> Vec<&Entry> {
        let mut seen = HashSet::new();
        self.offers
            .iter()
            .filter(|e| seen.insert(e.name.as_str()))
            .collect()
    }

    pub fn server(&self, id: &str) -> Option<&Found> {
        self.servers.iter().find(|f| f.spec.id == id)
    }

    /// The wire form `POST /v1/mcp/discovery` answers.
    pub fn to_json(&self) -> JValue {
        let opt = |v: &Option<String>| v.clone().map_or(JValue::Null, JValue::Str);
        JValue::obj([
            ("version", JValue::Num(1.0)),
            ("generation", JValue::Num(self.generation as f64)),
            ("incarnation", JValue::Str(self.incarnation.clone())),
            ("enabled", JValue::Bool(self.enabled)),
            ("error", opt(&self.error)),
            ("discoveredAt", JValue::Num(self.discovered_at as f64)),
            ("failedAt", JValue::Num(self.failed_at as f64)),
            (
                "remoteFailStreak",
                JValue::Num(f64::from(self.remote_fail_streak)),
            ),
            (
                "servers",
                JValue::Arr(
                    self.servers
                        .iter()
                        .map(|f| {
                            let mut items = vec![
                                ("id", JValue::Str(f.spec.id.clone())),
                                ("url", JValue::Str(f.spec.url.clone())),
                                ("auth", JValue::Str(f.spec.auth.clone())),
                            ];
                            if f.spec.directory {
                                items.push(("directory", JValue::Bool(true)));
                            }
                            if let Some(t) = &f.spec.title {
                                items.push(("title", JValue::Str(t.clone())));
                            }
                            if let Some(a) = &f.spec.added_by {
                                items.push(("addedBy", JValue::Str(a.clone())));
                            }
                            if let Some(e) = &f.spec.token_env {
                                items.push(("tokenEnv", JValue::Str(e.clone())));
                            }
                            items.push(("error", opt(&f.error)));
                            items.push(("discoveredAt", JValue::Num(f.discovered_at as f64)));
                            items.push(("toolCount", JValue::Num(f.tool_count as f64)));
                            JValue::obj(items)
                        })
                        .collect(),
                ),
            ),
            (
                "offers",
                JValue::Arr(
                    self.offers
                        .iter()
                        .map(|e| {
                            JValue::obj([
                                ("name", JValue::Str(e.name.clone())),
                                ("server", JValue::Str(e.server.clone())),
                                ("readOnly", e.read_only.map_or(JValue::Null, JValue::Bool)),
                                ("tool", e.tool.clone()),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("boxes", JValue::Arr(self.boxes.clone())),
        ])
    }
}

type Done = Option<Result<Arc<Snapshot>, Unavailable>>;

struct State {
    current: Arc<Snapshot>,
    discovered_at: i64,
    failed_at: i64,
    streak: u32,
    discovered_with: Option<String>,
    synced_with: Option<String>,
    inflight: Option<watch::Receiver<Done>>,
    generation: u64,
    epoch: u64,
}

/// The discovery actor. Hold it in an `Arc`.
pub struct Actor<B: Backend> {
    backend: Arc<B>,
    timing: Timing,
    incarnation: String,
    state: Mutex<State>,
    /// One preparation at a time: the configuration read, the credential read, the final signal read and the
    /// acceptance of what they saw. Ordering by request start is not ordering by observation, so a later
    /// starter could read an older table and still be accepted after a newer observation published; here the
    /// observation order IS the acceptance order. Never held across a listing or a wait.
    prepare: tokio::sync::Mutex<()>,
    /// This actor's own `Arc` (a run is a spawned task that owns one).
    pub(crate) me: std::sync::Weak<Actor<B>>,
}

/// Clears the in-flight marker and tells every waiter, whatever happens to the run (a panic included).
struct Finish<B: Backend> {
    actor: Arc<Actor<B>>,
    tx: watch::Sender<Done>,
    sent: bool,
    epoch: u64,
}

impl<B: Backend> Finish<B> {
    fn done(mut self, snapshot: Result<Arc<Snapshot>, Unavailable>) {
        let mut st = self.actor.lock();
        let snapshot = if st.epoch == self.epoch {
            st.inflight = None;
            snapshot
        } else {
            Err(Unavailable)
        };
        let _ = self.tx.send(Some(snapshot));
        self.sent = true;
    }
}

impl<B: Backend> Drop for Finish<B> {
    fn drop(&mut self) {
        if !self.sent {
            let mut st = self.actor.lock();
            if st.epoch == self.epoch {
                st.inflight = None;
            }
            let _ = self.tx.send(Some(Err(Unavailable)));
        }
    }
}

impl<B: Backend> Actor<B> {
    pub fn new(backend: Arc<B>, timing: Timing) -> Arc<Self> {
        Arc::new_cyclic(|me| Actor {
            me: me.clone(),
            backend,
            timing,
            incarnation: crate::mcp_broker::random::<16>()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            state: Mutex::new(State {
                current: Arc::new(Snapshot::default()),
                discovered_at: 0,
                failed_at: 0,
                streak: 0,
                discovered_with: None,
                synced_with: None,
                inflight: None,
                generation: 0,
                epoch: 0,
            }),
            prepare: tokio::sync::Mutex::new(()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The snapshot as it stands, with no discovery.
    pub fn current(&self) -> Arc<Snapshot> {
        Arc::clone(&self.lock().current)
    }

    /// The credential readiness discovery depends on (core `credentialKey`).
    async fn credential_key(&self, servers: &[ServerSpec]) -> String {
        let mut ready: HashMap<String, Ready> = HashMap::new();
        for sv in servers
            .iter()
            .filter(|s| s.auth == "oauth" || s.auth == "personal")
        {
            let user = sv.added_by.clone().unwrap_or_default();
            if let std::collections::hash_map::Entry::Vacant(slot) = ready.entry(user) {
                let r = self.backend.account_ready(slot.key()).await;
                slot.insert(r);
            }
        }
        servers
            .iter()
            .filter(|s| s.auth == "oauth" || s.auth == "personal")
            .map(|sv| {
                let user = sv.added_by.clone().unwrap_or_default();
                let ok = ready.get(&user).is_some_and(|r| {
                    if sv.auth == "oauth" {
                        r.signed_in.contains(&sv.id)
                    } else {
                        r.keys.contains(&sv.id)
                    }
                });
                format!("{}:{ok}", sv.id)
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    /// `discoverMcpTools(force)`.
    pub async fn discover(self: &Arc<Self>, mut force: bool) -> Result<Arc<Snapshot>, Unavailable> {
        loop {
            // Preparation and acceptance are one critical section (see `prepare`): what is read is accepted before
            // anyone else reads, so no observation can be accepted after a newer one.
            let preparing = self.prepare.lock().await;
            let (signal, servers) = self.backend.configuration().await.ok_or(Unavailable)?;
            let credentials = self.credential_key(&servers).await;
            if self.backend.directory_signal().await != signal {
                return Err(Unavailable);
            }
            let now = self.backend.now_ms();
            let curated_when_empty = if servers.is_empty() {
                mcp_manifest::manifest(self.backend.diary_mcp_write().await)
            } else {
                Vec::new()
            };
            let (waiting, start) = {
                let mut st = self.lock();
                let changed = st.synced_with.as_ref().is_some_and(|seen| *seen != signal)
                    || st
                        .discovered_with
                        .as_ref()
                        .is_some_and(|seen| *seen != credentials);
                if changed {
                    force = true;
                    st.epoch += 1;
                    st.inflight = None;
                    st.discovered_at = 0;
                    st.failed_at = 0;
                    st.current = Arc::new(Snapshot::default());
                }
                st.synced_with = Some(signal);
                if servers.is_empty() {
                    // Nothing may stay offered from a list that is gone.
                    st.epoch += 1;
                    st.inflight = None;
                    st.generation += 1;
                    let snapshot = Arc::new(Snapshot {
                        generation: st.generation,
                        incarnation: self.incarnation.clone(),
                        discovered_at: st.current.discovered_at,
                        failed_at: st.current.failed_at,
                        remote_fail_streak: st.current.remote_fail_streak,
                        manifest: curated_when_empty,
                        ..Snapshot::default()
                    });
                    st.current = Arc::clone(&snapshot);
                    return Ok(snapshot);
                }
                let has_boxes = !st.current.boxes.is_empty();
                let fresh = now - st.discovered_at < self.timing.ttl_ms;
                if !force && fresh && has_boxes {
                    return Ok(Arc::clone(&st.current));
                }
                let recent_failure = !has_boxes && now - st.failed_at < self.timing.fail_ttl_ms;
                if !force && recent_failure {
                    return Ok(Arc::clone(&st.current));
                }
                if let Some(rx) = st.inflight.clone() {
                    (Some(rx), None)
                } else {
                    let (tx, rx) = watch::channel(None);
                    st.inflight = Some(rx.clone());
                    st.discovered_with = Some(credentials);
                    st.epoch += 1;
                    (None, Some((tx, rx, st.epoch)))
                }
            };
            drop(preparing);
            if let Some(rx) = waiting {
                let outcome = wait(rx).await;
                if force {
                    // A forced refresh must not reuse a discovery that started before the change.
                    force = true;
                    continue;
                }
                return outcome;
            }
            let Some((tx, rx, epoch)) = start else {
                return Err(Unavailable);
            };
            let me = Arc::clone(self);
            tokio::spawn(async move {
                let finish = Finish {
                    actor: Arc::clone(&me),
                    tx,
                    sent: false,
                    epoch,
                };
                let snapshot = me.run(servers, epoch).await;
                finish.done(snapshot);
            });
            return wait(rx).await;
        }
    }

    /// One whole discovery, then the state it leaves.
    async fn run(
        self: &Arc<Self>,
        servers: Vec<ServerSpec>,
        epoch: u64,
    ) -> Result<Arc<Snapshot>, Unavailable> {
        let mut set = tokio::task::JoinSet::new();
        for (index, server) in servers.iter().cloned().enumerate() {
            let backend = Arc::clone(&self.backend);
            set.spawn(async move {
                let outcome = discover_one(&*backend, &server).await;
                (index, backend.now_ms(), outcome)
            });
        }
        let mut per_server: Vec<Option<Vec<Entry>>> = vec![None; servers.len()];
        let mut arrived: Vec<(usize, Found)> = Vec::new();
        let mut finished: HashSet<usize> = HashSet::new();
        while let Some(joined) = set.join_next().await {
            let Ok((index, at, outcome)) = joined else {
                continue;
            };
            let Some(server) = servers.get(index) else {
                continue;
            };
            finished.insert(index);
            match outcome {
                Ok(entries) => {
                    self.backend.log(format!(
                        "[mcp:{}] discovered {} tools at {}",
                        server.id,
                        entries.len(),
                        server.url
                    ));
                    arrived.push((
                        index,
                        Found {
                            spec: server.clone(),
                            error: None,
                            discovered_at: at,
                            tool_count: entries.len(),
                        },
                    ));
                    if let Some(slot) = per_server.get_mut(index) {
                        *slot = Some(entries);
                    }
                }
                Err(message) => {
                    self.backend.log(format!(
                        "[mcp:{}] discovery failed for {}: {message}",
                        server.id, server.url
                    ));
                    arrived.push((
                        index,
                        Found {
                            spec: server.clone(),
                            error: Some(message),
                            discovered_at: at,
                            tool_count: 0,
                        },
                    ));
                }
            }
        }
        // A server whose task panicked is a failed server, not a missing one.
        for (index, server) in servers.iter().enumerate() {
            if !finished.contains(&index) {
                arrived.push((
                    index,
                    Found {
                        spec: server.clone(),
                        error: Some("discovery failed".into()),
                        discovered_at: self.backend.now_ms(),
                        tool_count: 0,
                    },
                ));
            }
        }
        // The first error to arrive is the snapshot's; the servers themselves are in configured order.
        let error = arrived.iter().find_map(|(_, f)| f.error.clone());
        let found: Vec<Found> = {
            let mut ordered = arrived;
            ordered.sort_by_key(|(index, _)| *index);
            ordered.into_iter().map(|(_, f)| f).collect()
        };

        // One registry, in configured order; the first server offering a name keeps it in `tools`.
        let mut offers: Vec<Entry> = Vec::new();
        for (index, server) in servers.iter().enumerate() {
            let Some(Some(entries)) = per_server.get(index) else {
                continue;
            };
            for entry in entries {
                if let Some(held) = offers.iter().find(|o| o.name == entry.name) {
                    self.backend.log(format!(
                        "[mcp] \"{}\" offered by both \"{}\" and \"{}\"; keeping \"{}\"",
                        entry.name, held.server, server.id, held.server
                    ));
                }
                offers.push(entry.clone());
            }
        }

        // Each directory server becomes one box holding every tool it offers (and only its own).
        let diary_write = self.backend.diary_mcp_write().await;
        let curated = mcp_manifest::manifest(diary_write);
        let mut manifest = curated.clone();
        for (index, server) in servers.iter().enumerate().filter(|(_, s)| s.directory) {
            let names: Vec<String> = match per_server.get(index) {
                Some(Some(entries)) => entries.iter().map(|e| e.name.clone()).collect(),
                _ => Vec::new(),
            };
            manifest.push(mcp_manifest::directory_box(
                &server.id,
                server.title.as_deref().unwrap_or_default(),
                &names,
            ));
        }
        let mut by_server: HashMap<String, HashMap<String, Offered<'_>>> = HashMap::new();
        for (index, server) in servers.iter().enumerate() {
            let map = by_server.entry(server.id.clone()).or_default();
            if let Some(Some(entries)) = per_server.get(index) {
                for entry in entries {
                    map.insert(entry.name.clone(), Offered { tool: &entry.tool });
                }
            }
        }
        let known: HashSet<&str> = found.iter().map(|f| f.spec.id.as_str()).collect();
        let mut warnings = Vec::new();
        let boxes =
            mcp_manifest::bind_boxes(&manifest, &by_server, &|id| known.contains(id), &mut |w| {
                warnings.push(w)
            });
        for w in warnings {
            self.backend.log(w);
        }

        let internal_down = found
            .iter()
            .any(|f| f.spec.auth == "internal" && f.error.is_some());
        let remote_down = found
            .iter()
            .any(|f| f.spec.auth != "internal" && f.error.is_some());
        let now = self.backend.now_ms();
        let tool_total = offers
            .iter()
            .map(|o| o.name.as_str())
            .collect::<HashSet<_>>()
            .len();
        self.backend.log(format!(
            "[mcp] {tool_total} tools across {} server(s); {} curated boxes available",
            servers.len(),
            boxes.len()
        ));

        let mut st = self.lock();
        if st.epoch != epoch {
            return Err(Unavailable);
        }
        st.streak = if remote_down { st.streak + 1 } else { 0 };
        let retry = if remote_down && !self.timing.retry_ms.is_empty() {
            let step = (st.streak as usize).clamp(1, self.timing.retry_ms.len());
            let wait = self
                .timing
                .retry_ms
                .get(step - 1)
                .copied()
                .unwrap_or(self.timing.ttl_ms);
            self.timing.ttl_ms.min(wait)
        } else {
            self.timing.ttl_ms
        };
        // `discovered_at` is set so the next unforced request is fresh for exactly `retry`.
        st.discovered_at = if internal_down {
            0
        } else {
            now - (self.timing.ttl_ms - retry)
        };
        st.failed_at = if boxes.is_empty() { now } else { 0 };
        st.generation += 1;
        let snapshot = Arc::new(Snapshot {
            generation: st.generation,
            incarnation: self.incarnation.clone(),
            enabled: !servers.is_empty(),
            error,
            discovered_at: st.discovered_at,
            failed_at: st.failed_at,
            remote_fail_streak: st.streak,
            servers: found,
            offers,
            boxes,
            manifest: curated,
        });
        st.current = Arc::clone(&snapshot);
        Ok(snapshot)
    }
}

async fn wait(mut rx: watch::Receiver<Done>) -> Result<Arc<Snapshot>, Unavailable> {
    loop {
        let done = rx.borrow_and_update().clone();
        if let Some(outcome) = done {
            return outcome;
        }
        if rx.changed().await.is_err() {
            // The sender is gone without an answer.
            let last = rx.borrow().clone();
            return last.unwrap_or(Err(Unavailable));
        }
    }
}

/// core `discoverOneServer`: the converted tools one server offers, or why it did not.
async fn discover_one<B: Backend>(backend: &B, server: &ServerSpec) -> Result<Vec<Entry>, String> {
    // A directory server is someone else's: its name must still point at a public address.
    if server.directory && !backend.url_public(&server.url).await {
        return Err("its address no longer resolves to a public host".into());
    }
    let mut headers: Vec<(String, String)> = match server.auth.as_str() {
        "directory" => backend.directory_headers(&server.id).await,
        "internal" => backend
            .internal_discovery_token()
            .map(|t| vec![("Authorization".to_owned(), format!("Bearer {t}"))])
            .unwrap_or_default(),
        "bearer" => match server
            .token_env
            .as_deref()
            .and_then(|e| backend.static_token(e))
        {
            Some(token) => vec![("Authorization".to_owned(), format!("Bearer {token}"))],
            None => Vec::new(),
        },
        _ => Vec::new(),
    };
    let added_by = server.added_by.clone().filter(|a| !a.is_empty());
    if server.auth == "personal" {
        // The tool list is read with the key of the administrator who added the server.
        if let Some(admin) = &added_by {
            if backend.user_disabled(admin).await {
                return Err("waiting for an active administrator to add a key".into());
            }
        }
        let admin = added_by.clone().unwrap_or_default();
        match backend
            .key_headers(&admin, &server.id)
            .await
            .map_err(|_| "credential unavailable".to_owned())?
        {
            Some(h) if !h.is_empty() => headers = h,
            _ => {
                return Err("waiting for the administrator who added it to enter their key".into())
            }
        }
    }
    if server.auth == "oauth" {
        if let Some(admin) = &added_by {
            if backend.user_disabled(admin).await {
                return Err("waiting for an active administrator to sign in".into());
            }
        }
        let admin = added_by.clone().unwrap_or_default();
        match backend
            .oauth_token(&admin, &server.id)
            .await
            .map_err(|_| "credential unavailable".to_owned())?
        {
            Some(token) => {
                headers = vec![("Authorization".to_owned(), format!("Bearer {token}"))];
            }
            None => {
                return Err("waiting for the administrator who added it to sign in".into());
            }
        }
    }
    let listed = backend
        .list_tools(server, &headers)
        .await
        .map_err(|_| "fetch failed".to_owned())?;
    let mut entries: Vec<Entry> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for tool in &listed {
        match crate::mcp_tools::definition(tool) {
            Ok(def) => {
                let entry = Entry {
                    name: def.name,
                    server: server.id.clone(),
                    tool: def.tool,
                    read_only: tool.get("annotations").get("readOnlyHint").as_bool(),
                };
                // A map by name: the later definition replaces the earlier, in the earlier's place.
                match entries.iter_mut().find(|e| e.name == entry.name) {
                    Some(held) => *held = entry,
                    None => entries.push(entry),
                }
            }
            Err(reason) => dropped.push(format!(
                "{}: {reason}",
                tool.get("name").as_str().unwrap_or("(unnamed)")
            )),
        }
    }
    if !dropped.is_empty() {
        backend.log(format!(
            "[mcp:{}] dropped {} unconvertible tools: {}",
            server.id,
            dropped.len(),
            dropped.join(" | ")
        ));
    }
    Ok(entries)
}
