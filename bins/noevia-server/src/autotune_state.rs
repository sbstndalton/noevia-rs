//! The front's auto-tune state authority: the owner (server_projects::autotune_state), the calls it
//! owes the Node tune actor, and the five public routes' view of both.
//!
//! The public routes (status, settings, resume, cancel, clear) used to ask five Node bridges, which
//! read and wrote the actor's in-memory state. They now call [`Autotune::call`] with the same
//! logical operations and get the same answers from the owner. The Node actor remains the runner:
//! it reaches the owner through the store channel (`/v1/autotune-state/{attach,commit,start,sync}`,
//! [`store_op`]) and answers a few private calls here ([`ACTOR`]): the facts only Node has
//! (models.ini revision, system roles, router listing, model identities), the abort of a running
//! tune and the start of a resumed one.
//!
//! No lock is held across an await: the owner is a synchronous state machine, called from the
//! blocking pool (it writes the state file), and every call to the actor is made between owner calls.
//! A hold the owner takes for an operation (a preparation) is released by a guard if the caller's
//! future is dropped at any await.
use crate::research_routes::Research;
use js_json::{wtf16, JValue};
use server_projects::autotune_state::{Answer, Fault, Outside, Owner, Stage};
use std::{path::Path, sync::Arc, time::Duration};

/// Node's private actor-control endpoints (rust-autotune-actor-bridge.cjs).
const ACTOR: &str = "/internal/autotune-actor/";
const SHORT: Duration = Duration::from_secs(5);
const LAUNCH: Duration = Duration::from_secs(15);
/// The longest one resume preparation may take (Core's own 120 s deadline).
const PREPARE: Duration = Duration::from_secs(120);
const MAX_REPLY: usize = 8 * 1024 * 1024;

pub struct Autotune {
    owner: Owner,
    research: Arc<Research>,
}

impl std::fmt::Debug for Autotune {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Autotune").finish_non_exhaustive()
    }
}

fn hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn unavailable() -> Answer {
    (
        503,
        JValue::obj([("error", "Auto-tune state is unavailable.".into())]),
    )
}
fn invalid(message: &str) -> Answer {
    (400, JValue::obj([("error", message.into())]))
}
fn not_applied() -> Answer {
    (409, JValue::obj([("applied", false.into())]))
}
fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "null".into())
}

/// Releases a preparation's hold if its future is dropped before it settles.
struct PrepGuard<'a> {
    auto: &'a Autotune,
    id: u64,
    armed: bool,
}
impl Drop for PrepGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.auto.owner.resume_prepare_abandon(self.id);
        }
    }
}

impl Autotune {
    pub fn new(dir: &Path, research: Arc<Research>) -> Self {
        Self {
            owner: Owner::new(dir),
            research,
        }
    }

    pub fn owner(&self) -> &Owner {
        &self.owner
    }

    /// One private call to the tune actor: `(status, JSON)`; `None`: no answer or over the cap.
    async fn actor(&self, op: &str, body: JValue, timeout: Duration) -> Option<(u16, JValue)> {
        let (status, bytes) = self
            .research
            .node_call_text(&format!("{ACTOR}{op}"), text(&body), timeout)
            .await?;
        if bytes.len() > MAX_REPLY {
            return None;
        }
        let parsed = wtf16::parse(std::str::from_utf8(&bytes).ok()?).ok()?;
        Some((status, parsed))
    }

    async fn outside(&self, names: &[String]) -> Option<Outside> {
        let models = JValue::Arr(names.iter().map(|n| JValue::from(n.as_str())).collect());
        let (status, reply) = self
            .actor("outside", JValue::obj([("models", models)]), SHORT)
            .await?;
        (status == 200).then(|| Outside::parse(&reply)).flatten()
    }

    /// Runs `f` on the blocking pool (the owner writes the state file).
    async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Arc<Self>) -> T + Send + 'static,
    ) -> Option<T> {
        let me = Arc::clone(self);
        tokio::task::spawn_blocking(move || f(&me)).await.ok()
    }

    /// The bridge operations of the five retired Node bridges, by their old path tails
    /// (`read/facts`, `settings/{facts,commit}`, `resume/{facts,prepare,commit}`,
    /// `cancel/{facts,abort}`, `clear/{facts,clear}`). `None`: the answer is "unavailable".
    pub async fn call(self: &Arc<Self>, path: &str, body: &serde_json::Value) -> Option<Answer> {
        let ask = wtf16::parse(&body.to_string()).ok()?;
        let answer = match path {
            "read/facts" => self.read_facts(&ask).await,
            "settings/facts" | "settings/commit" => {
                self.settings(path.ends_with("commit"), &ask).await
            }
            "resume/facts" => self.resume_facts(&ask).await,
            "resume/prepare" => self.resume_prepare(&ask).await,
            "resume/commit" => self.resume_commit(&ask).await,
            "cancel/facts" => self.cancel_facts(&ask).await,
            "cancel/abort" => self.cancel_abort(&ask).await,
            "clear/facts" => self.clear(false, &ask).await,
            "clear/clear" => self.clear(true, &ask).await,
            _ => return None,
        };
        (answer.0 != 503).then_some(answer)
    }

    /// `Some(answer)` when the owner is not bound (503) or the actor is not available (404-ish).
    fn gate(&self) -> Option<Answer> {
        match self.owner.availability() {
            None => Some(unavailable()),
            Some(false) => Some((200, JValue::obj([("available", false.into())]))),
            Some(true) => None,
        }
    }

    async fn read_facts(self: &Arc<Self>, ask: &JValue) -> Answer {
        if let Some(a) = self.gate() {
            return a;
        }
        let JValue::Obj(fields) = ask else {
            return invalid("Invalid status facts request.");
        };
        let JValue::Arr(keys) = ask.get("keys") else {
            return invalid("Invalid status facts request.");
        };
        if fields.iter().any(|(k, _)| k != "keys")
            || keys.len() > 2
            || keys.iter().any(|k| match k {
                JValue::Str(s) => wtf16::len(s) > 65536,
                _ => true,
            })
        {
            return invalid("Invalid status facts request.");
        }
        let Some(outside) = self.outside(&[]).await else {
            return unavailable();
        };
        let keys = keys.clone();
        let below = outside_below(&outside);
        self.blocking(move |me| me.owner.status_facts(&keys, below))
            .await
            .unwrap_or_else(unavailable)
    }

    async fn settings(self: &Arc<Self>, commit: bool, ask: &JValue) -> Answer {
        if let Some(a) = self.gate() {
            return a;
        }
        let JValue::Obj(fields) = ask else {
            return invalid("Invalid settings state request.");
        };
        let allowed: &[&str] = if commit {
            &["model", "allowQ5Kv", "token"]
        } else {
            &["model"]
        };
        let model = ask.get("model");
        let model_ok = match model {
            JValue::Undefined => !commit,
            JValue::Str(s) => !s.is_empty() && wtf16::len(s) <= 200,
            _ => false,
        };
        if fields.iter().any(|(k, _)| !allowed.contains(&k.as_str()))
            || !model_ok
            || (commit
                && (ask.get("allowQ5Kv").as_bool().is_none()
                    || !ask.get("token").as_str().is_some_and(hex64)))
        {
            return invalid("Invalid settings state request.");
        }
        // The readiness question (no model) needs nothing from the actor.
        let names = Owner::settings_names(model);
        let outside = if names.is_empty() {
            None
        } else {
            match self.outside(&names).await {
                Some(o) => Some(o),
                None => return unavailable(),
            }
        };
        let model = model.clone();
        if commit {
            let Some(outside) = outside else {
                return invalid("Invalid settings state request.");
            };
            let allow = ask.get("allowQ5Kv").as_bool().unwrap_or(false);
            let token = ask.get("token").as_str().unwrap_or_default().to_owned();
            let name = model.as_str().unwrap_or_default().to_owned();
            self.blocking(move |me| me.owner.settings_commit(&name, allow, &token, &outside))
                .await
                .unwrap_or_else(unavailable)
        } else {
            self.blocking(move |me| me.owner.settings_facts(&model, outside.as_ref()))
                .await
                .unwrap_or_else(unavailable)
        }
    }

    async fn resume_outside(&self) -> Option<Outside> {
        let names = self.owner.resume_names()?;
        self.outside(&names).await
    }

    async fn resume_facts(self: &Arc<Self>, ask: &JValue) -> Answer {
        if let Some(a) = self.gate() {
            return a;
        }
        if !matches!(ask, JValue::Obj(f) if f.is_empty()) {
            return invalid("Invalid resume state request.");
        }
        let Some(outside) = self.resume_outside().await else {
            return unavailable();
        };
        self.blocking(move |me| me.owner.resume_facts(&outside))
            .await
            .unwrap_or_else(unavailable)
    }

    async fn resume_prepare(self: &Arc<Self>, ask: &JValue) -> Answer {
        if let Some(a) = self.gate() {
            return a;
        }
        let JValue::Obj(fields) = ask else {
            return invalid("Invalid resume state request.");
        };
        let Some(token) = ask.get("token").as_str().filter(|t| hex64(t)) else {
            return invalid("Invalid resume state request.");
        };
        if fields
            .iter()
            .any(|(k, _)| !matches!(k.as_str(), "token" | "stage" | "selectors"))
        {
            return invalid("Invalid resume state request.");
        }
        let (token, stage, selectors) = (
            token.to_owned(),
            ask.get("stage").clone(),
            ask.get("selectors").clone(),
        );
        let Some(outside) = self.resume_outside().await else {
            return unavailable();
        };
        let begun = self
            .blocking(move |me| {
                me.owner
                    .resume_prepare_begin(&token, &stage, &selectors, &outside)
            })
            .await;
        let prepared = match begun {
            None => return unavailable(),
            Some(Err(answer)) => return answer,
            Some(Ok(p)) => p,
        };
        let mut guard = PrepGuard {
            auto: self,
            id: prepared.id,
            armed: true,
        };
        let gathered = tokio::time::timeout(PREPARE, async {
            match prepared.stage {
                Stage::Listing => {
                    self.actor(
                        "listing",
                        JValue::obj([("items", prepared.items.clone())]),
                        PREPARE,
                    )
                    .await
                }
                Stage::Identities => {
                    let picks = JValue::Arr(
                        prepared
                            .selectors
                            .iter()
                            .map(|(index, field, id)| {
                                JValue::obj([
                                    ("index", JValue::Num(*index as f64)),
                                    ("field", field.as_str().into()),
                                    ("id", id.as_str().into()),
                                ])
                            })
                            .collect(),
                    );
                    self.actor("identities", JValue::obj([("selectors", picks)]), PREPARE)
                        .await
                }
            }
        })
        .await;
        // `Err(true)`: the actor says the restored settings were not re-read yet; `Err(false)`: the
        // preparation went stale (timeout, or the actor saw its caller go away).
        let result = match gathered {
            Err(_) => Err(false),
            Ok(None) => {
                return unavailable();
            }
            Ok(Some((200, r))) if r.get("fault").as_str() == Some("reload") => Err(true),
            Ok(Some((200, r))) if r.get("applied").as_bool() == Some(false) => Err(false),
            Ok(Some((200, r))) => Ok(r),
            Ok(Some(_)) => return unavailable(),
        };
        let id = prepared.id;
        let Some(fresh) = self.resume_outside().await else {
            return unavailable();
        };
        guard.armed = false;
        self.blocking(move |me| me.owner.resume_prepare_end(id, result, &fresh))
            .await
            .unwrap_or_else(unavailable)
    }

    async fn resume_commit(self: &Arc<Self>, ask: &JValue) -> Answer {
        if let Some(a) = self.gate() {
            return a;
        }
        let JValue::Obj(fields) = ask else {
            return invalid("Invalid resume state request.");
        };
        let Some(token) = ask.get("token").as_str().filter(|t| hex64(t)) else {
            return invalid("Invalid resume state request.");
        };
        if fields
            .iter()
            .any(|(k, _)| !matches!(k.as_str(), "token" | "plan"))
        {
            return invalid("Invalid resume state request.");
        }
        let (token, plan) = (token.to_owned(), ask.get("plan").clone());
        let Some(outside) = self.resume_outside().await else {
            return unavailable();
        };
        let Some(done) = self
            .blocking(move |me| me.owner.resume_commit(&token, &plan, &outside))
            .await
        else {
            return unavailable();
        };
        if done.answer.0 != 200 {
            return done.answer;
        }
        if let Some(launch) = done.launch {
            let body = JValue::obj([
                ("epoch", JValue::Num(launch.epoch as f64)),
                ("jobRev", JValue::Num(launch.job_rev as f64)),
                ("job", launch.job.clone()),
                ("history", launch.history.clone()),
            ]);
            let started = matches!(
                self.actor("launch", body, LAUNCH).await,
                Some((200, r)) if r.get("launched").as_bool() == Some(true)
            );
            if !started {
                // No running job without a runner: the job stops, and the caller is told.
                let _ = self
                    .blocking(move |me| me.owner.launch_failed(&launch))
                    .await;
                return unavailable();
            }
        } else {
            // A queue filter: the scan cache the old bridge cleared after any applied commit.
            let _ = self
                .actor("invalidate", JValue::Obj(Vec::new()), SHORT)
                .await;
        }
        done.answer
    }

    async fn cancel_facts(self: &Arc<Self>, ask: &JValue) -> Answer {
        if !matches!(ask, JValue::Obj(f) if f.is_empty()) {
            return invalid("Invalid cancellation state request.");
        }
        self.blocking(|me| me.owner.cancel_facts())
            .await
            .unwrap_or_else(unavailable)
    }

    async fn cancel_abort(self: &Arc<Self>, ask: &JValue) -> Answer {
        let JValue::Obj(fields) = ask else {
            return invalid("Invalid cancellation state request.");
        };
        let Some(token) = ask.get("token").as_str().filter(|t| hex64(t)) else {
            return invalid("Invalid cancellation state request.");
        };
        if fields.iter().any(|(k, _)| k != "token") {
            return invalid("Invalid cancellation state request.");
        }
        let token = token.to_owned();
        let Some(begun) = self.blocking(move |me| me.owner.cancel_begin(&token)).await else {
            return unavailable();
        };
        let ticket = match begun {
            Ok(t) => t,
            Err(answer) => return answer,
        };
        // The abort reaches the running actor through its abort signal; the owner has recorded the
        // generation-matched request. An actor that did not take it withdraws the request.
        let delivered = self
            .actor(
                "abort",
                JValue::obj([
                    ("jobId", ticket.job_id.clone()),
                    ("epoch", JValue::Num(ticket.epoch as f64)),
                ]),
                SHORT,
            )
            .await;
        match delivered {
            Some((200, r)) if r.get("aborted").as_bool() == Some(true) => ticket.reply.clone(),
            other => {
                let _ = self.blocking(move |me| me.owner.cancel_undo(&ticket)).await;
                match other {
                    Some((200, r)) if r.get("aborted").as_bool() == Some(false) => not_applied(),
                    _ => unavailable(),
                }
            }
        }
    }

    async fn clear(self: &Arc<Self>, clear: bool, ask: &JValue) -> Answer {
        let JValue::Obj(fields) = ask else {
            return invalid("Invalid dismissal state request.");
        };
        let token = ask.get("token").as_str().filter(|t| hex64(t));
        if fields.iter().any(|(k, _)| k != "token")
            || (clear && token.is_none())
            || (!clear && !fields.is_empty())
        {
            return invalid("Invalid dismissal state request.");
        }
        let token = token.unwrap_or_default().to_owned();
        self.blocking(move |me| {
            if clear {
                me.owner.clear(&token)
            } else {
                me.owner.clear_facts()
            }
        })
        .await
        .unwrap_or_else(unavailable)
    }
}

fn outside_below(o: &Outside) -> bool {
    o.allow_below()
}

// ---- the actor's side: the store channel ----------------------------------------------------------

/// `POST /v1/autotune-state/{attach,commit,start,sync}` (project_store.rs): the Node actor's calls.
pub async fn store_op(auto: Arc<Autotune>, op: &'static str, raw: Vec<u8>) -> (u16, JValue) {
    let Some(body) = std::str::from_utf8(&raw)
        .ok()
        .and_then(|t| wtf16::parse(t).ok())
        .filter(|b| matches!(b, JValue::Obj(_)))
    else {
        return (400, JValue::obj([("error", "invalid JSON".into())]));
    };
    tokio::task::spawn_blocking(move || match op {
        "attach" => {
            let base = body.get("base").as_str().unwrap_or_default();
            let Some(available) = body.get("available").as_bool() else {
                return (400, JValue::obj([("error", "invalid attach".into())]));
            };
            match auto.owner.attach(base, available) {
                Ok(snapshot) => (200, snapshot),
                Err(Fault::Unbound | Fault::State) => (
                    503,
                    JValue::obj([("error", "auto-tune state unavailable".into())]),
                ),
            }
        }
        "commit" => auto.owner.commit(&body),
        "start" => match body.get("op").as_str() {
            Some("begin") => auto.owner.start_hold(&body, true),
            Some("end") => auto.owner.start_hold(&body, false),
            _ => (400, JValue::obj([("error", "invalid start op".into())])),
        },
        "sync" => auto.owner.sync(&body),
        _ => (404, JValue::obj([("error", "not found".into())])),
    })
    .await
    .unwrap_or_else(|_| (500, JValue::obj([("error", "Internal error".into())])))
}
