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
//! A detached task retains preparation custody when its public caller disappears. Only a terminal
//! actor receipt releases exclusion; unknown delivery keeps the hold and the shared recovery incident.
use crate::model_maint::{lease::Authority, Maint};
use crate::research_routes::Research;
use js_json::{wtf16, JValue};
use server_projects::autotune_state::{Answer, Launch, Outside, Owner, Stage};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Node's private actor-control endpoints (rust-autotune-actor-bridge.cjs).
const ACTOR: &str = "/internal/autotune-actor/";
const SHORT: Duration = Duration::from_secs(5);
const LAUNCH: Duration = Duration::from_secs(15);
/// The longest one resume preparation may take (Core's own 120 s deadline).
const PREPARE: Duration = Duration::from_secs(120);
const MAX_REPLY: usize = 8 * 1024 * 1024;

/// How long one recovery hold may last while an actor operation's outcome is settled (J's calibration
/// recovery uses the same).
const RECOVERY_LEASE: Duration = Duration::from_secs(15 * 60);
const RECOVERY_REASON: &str =
    "Chat is paused while noevia settles an interrupted auto-tune operation.";
const RECONCILE_MAX_WAIT: Duration = Duration::from_secs(15);

pub struct Autotune {
    owner: Owner,
    research: Arc<Research>,
    authority: Authority,
    /// J's maintenance machinery (recovery hold, Node barrier); `None` only in unit tests that never
    /// reach a reconciliation.
    maint: Option<Arc<Maint>>,
    incidents: Mutex<Incidents>,
}

/// The shared recovery incident (J's single-ticket model) and the unknown operations K has open under it.
/// K completes the ticket only if it opened it and its last open operation has settled; an incident some
/// other work opened is theirs to complete.
#[derive(Default)]
struct Incidents {
    open: usize,
    owns_ticket: bool,
}

/// What the owner still owes an actor operation whose outcome was not authenticated.
enum Pending {
    /// A resume preparation: its hold is released (that exact generation) once the operation is terminal.
    Prepare(u64),
    /// A launch: a refused or never-run launch stops the job (no running job without a runner).
    Launch(Launch),
    /// Nothing is held by the owner: the actor's own repair (an acknowledged but unfinished router
    /// re-read) is what exclusion waits for.
    Repair,
}

/// An actor operation's terminal state, from its authenticated receipt.
enum Receipt {
    /// Still running at the actor: not terminal.
    Running,
    /// Terminal: the actor completed it; `launched` is its answer to a launch.
    Settled { launched: bool, needs_repair: bool },
    /// Terminal: the actor never ran it and now never will (it is fenced by this receipt).
    Refused,
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

/// Custody before dispatch and during terminal settlement. Unknown delivery disarms release while
/// retaining the owner's hold under the shared recovery incident.
struct PrepGuard {
    auto: Arc<Autotune>,
    id: u64,
    armed: bool,
}
impl PrepGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for PrepGuard {
    fn drop(&mut self) {
        if self.armed {
            self.auto.owner.resume_prepare_abandon(self.id);
        }
    }
}

impl Autotune {
    pub fn new(dir: &Path, research: Arc<Research>, maint: Arc<Maint>) -> Self {
        Self {
            owner: Owner::new(dir),
            research,
            authority: maint.authority.clone(),
            maint: Some(maint),
            incidents: Mutex::new(Incidents::default()),
        }
    }

    #[cfg(test)]
    fn without_maintenance(dir: &Path, research: Arc<Research>, authority: Authority) -> Self {
        Self {
            owner: Owner::new(dir),
            research,
            authority,
            maint: None,
            incidents: Mutex::new(Incidents::default()),
        }
    }

    fn authority(&self) -> &Authority {
        &self.authority
    }

    /// Test hook: some other work's incident already holds the shared recovery ticket.
    #[doc(hidden)]
    pub fn foreign_incident_for_tests(&self) {
        self.authority().require_recovery();
    }

    /// Unknown operations K still has open (a test and status hook).
    pub fn open_incidents(&self) -> usize {
        self.incidents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open
    }

    /// An actor operation whose outcome the front could not authenticate (a lost response, a timeout, an
    /// acknowledgement that says more repair is owed). Exclusion is kept in the authority at once and the
    /// obligation moves to an owned reconciliation task that survives the caller.
    fn open_incident(self: &Arc<Self>, op: String, pending: Pending, repair: bool) {
        {
            let mut inc = self
                .incidents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let fresh = !self.authority().recovery_pending();
            self.authority().require_recovery();
            if fresh {
                inc.owns_ticket = true;
            }
            inc.open += 1;
        }
        tokio::spawn(Arc::clone(self).reconcile(op, pending, repair));
    }

    /// The actor's authenticated receipt for `op` (`settle`): it also fences an operation the actor never
    /// received, so a late arrival is refused.
    async fn receipt(&self, op: &str, repair: bool) -> Option<Receipt> {
        let (status, r) = self
            .actor(
                "settle",
                JValue::obj([("op", op.into()), ("repair", repair.into())]),
                SHORT,
            )
            .await?;
        if status != 200 {
            return None;
        }
        match r.get("state").as_str()? {
            "running" => Some(Receipt::Running),
            "refused" => Some(Receipt::Refused),
            "settled" => Some(Receipt::Settled {
                launched: r.get("outcome").get("launched").as_bool() == Some(true),
                needs_repair: r.get("outcome").get("needsRepair").as_bool() == Some(true)
                    && r.get("repaired").as_bool() != Some(true),
            }),
            _ => None,
        }
    }

    /// Owns one unknown operation until it is settled: the Node admission barrier and an exclusive recovery
    /// hold (J's recovery path), then an authenticated terminal receipt (or the actor's covered repair);
    /// only then is that exact preparation released, a refused launch stopped, and (if this front opened
    /// the incident and no other K operation is open) the incident completed. Exclusion is never given up
    /// while unresolved, and nothing is cleared because of an unrelated event.
    async fn reconcile(self: Arc<Self>, op: String, pending: Pending, mut repair: bool) {
        let mut wait = Duration::from_millis(250);
        let mut hold: Option<crate::model_maint::Hold> = None;
        let mut applied = false;
        loop {
            // Re-assert the incident and the Node barrier every round: a completion by other work must
            // not leave this operation unguarded.
            let Some(maint) = self.maint.clone() else {
                tokio::time::sleep(wait).await;
                continue;
            };
            let ticket = self.authority().require_recovery();
            if !maint.node.recovery_barrier(true, &ticket).await {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(RECONCILE_MAX_WAIT);
                continue;
            }
            if hold.as_ref().is_none_or(|h| !h.excluded()) {
                if let Some(h) = hold.take() {
                    h.release().await;
                }
                match maint
                    .acquire_recovery(RECOVERY_REASON, RECOVERY_LEASE)
                    .await
                {
                    Ok(h) => hold = Some(h),
                    Err(_) => {
                        tokio::time::sleep(wait).await;
                        wait = (wait * 2).min(RECONCILE_MAX_WAIT);
                        continue;
                    }
                }
            }
            if !applied {
                match self.receipt(&op, repair).await {
                    None | Some(Receipt::Running) => {
                        tokio::time::sleep(wait).await;
                        wait = (wait * 2).min(RECONCILE_MAX_WAIT);
                        continue;
                    }
                    Some(Receipt::Settled {
                        needs_repair: true, ..
                    }) => {
                        repair = true;
                        continue;
                    }
                    Some(terminal) => {
                        let launched = matches!(terminal, Receipt::Settled { launched: true, .. });
                        match &pending {
                            Pending::Prepare(id) => {
                                let id = *id;
                                let _ = self
                                    .blocking(move |me| me.owner.resume_prepare_abandon(id))
                                    .await;
                            }
                            Pending::Launch(l) if !launched => {
                                let l = l.clone();
                                let _ = self.blocking(move |me| me.owner.launch_failed(&l)).await;
                            }
                            Pending::Launch(_) | Pending::Repair => {}
                        }
                        applied = true;
                    }
                }
            }
            // Settled: finish the incident like J's recovery does, only for what this front opened.
            let Some(h) = hold.as_ref() else { continue };
            let mut done = h.excluded();
            if done {
                done = h.gated_now()
                    || match h.recovery_proof() {
                        Some((id, generation)) => {
                            maint.node.complete_recovery(&ticket, id, generation).await
                        }
                        None => false,
                    };
                done = done && h.excluded();
            }
            if done {
                {
                    let mut inc = self
                        .incidents
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    inc.open = inc.open.saturating_sub(1);
                    if inc.open == 0 && inc.owns_ticket {
                        // Only the incident K opened, and only the ticket this round named.
                        if self.authority().recovery_ticket().as_deref() == Some(ticket.as_str()) {
                            self.authority().complete_recovery(&ticket);
                        }
                        inc.owns_ticket = false;
                    }
                }
                if let Some(h) = hold.take() {
                    h.release().await;
                }
                return;
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(RECONCILE_MAX_WAIT);
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
        let me = Arc::clone(self);
        let ask = ask.clone();
        // Dropping the public caller never drops custody of an issued actor effect.
        let task = tokio::spawn(async move { me.resume_prepare_owned(&ask).await });
        tokio::time::timeout(PREPARE, task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_else(unavailable)
    }

    async fn resume_prepare_owned(self: &Arc<Self>, ask: &JValue) -> Answer {
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
                    .map(|p| {
                        let guard = PrepGuard {
                            auto: Arc::clone(me),
                            id: p.id,
                            armed: true,
                        };
                        (p, guard)
                    })
            })
            .await;
        let (prepared, mut guard) = match begun {
            None => return unavailable(),
            Some(Err(answer)) => return answer,
            Some(Ok(p)) => p,
        };
        // The operation's identity: the actor records it, and its receipt is how an unknown outcome is settled.
        let op = server_projects::source_jobs::new_id();
        let gathered = tokio::time::timeout(PREPARE, async {
            match prepared.stage {
                Stage::Listing => {
                    self.actor(
                        "listing",
                        JValue::obj([
                            ("op", op.as_str().into()),
                            ("items", prepared.items.clone()),
                        ]),
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
                    self.actor(
                        "identities",
                        JValue::obj([("op", op.as_str().into()), ("selectors", picks)]),
                        PREPARE,
                    )
                    .await
                }
            }
        })
        .await;
        // No authenticated answer (a lost response, a timeout, a failure): the operation's outcome is
        // unknown, so the preparation hold stays and an owned reconciliation task takes custody of this
        // exact preparation and the recovery incident; it releases them only after the actor's receipt.
        let answered = matches!(&gathered, Ok(Some((200, _))));
        if !answered {
            guard.disarm();
            self.open_incident(op, Pending::Prepare(prepared.id), false);
            return unavailable();
        }
        // An acknowledged answer that says the router's re-read is still owed is terminal for this
        // preparation (the caller gets the retryable refusal as before); the repair that remains is owned
        // by a reconciliation task too, and exclusion lasts until it is done.
        if matches!(&gathered, Ok(Some((200, r))) if r.get("recoveryRequired").as_bool() == Some(true))
        {
            self.open_incident(op, Pending::Repair, true);
        }
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
        self.blocking(move |me| {
            let answer = me.owner.resume_prepare_end(id, result, &fresh);
            guard.disarm();
            answer
        })
        .await
        .unwrap_or_else(unavailable)
    }

    async fn resume_commit(self: &Arc<Self>, ask: &JValue) -> Answer {
        let me = Arc::clone(self);
        let ask = ask.clone();
        tokio::spawn(async move { me.resume_commit_owned(&ask).await })
            .await
            .unwrap_or_else(|_| unavailable())
    }

    async fn resume_commit_owned(self: &Arc<Self>, ask: &JValue) -> Answer {
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
            let op = server_projects::source_jobs::new_id();
            let body = JValue::obj([
                ("op", op.as_str().into()),
                ("epoch", JValue::Num(launch.epoch as f64)),
                ("jobRev", JValue::Num(launch.job_rev as f64)),
                ("job", launch.job.clone()),
                ("history", launch.history.clone()),
            ]);
            match self.actor("launch", body, LAUNCH).await {
                Some((200, r)) if r.get("launched").as_bool() == Some(true) => {}
                Some((200, r)) if r.get("launched").as_bool() == Some(false) => {
                    let _ = self
                        .blocking(move |me| me.owner.launch_failed(&launch))
                        .await;
                    return unavailable();
                }
                _ => {
                    // Accepted but lost response is indistinguishable from undelivered. The running
                    // generation stays (it prevents actor replacement) and an owned reconciliation task
                    // keeps exclusion until the actor's receipt says whether the launch happened.
                    self.open_incident(op, Pending::Launch(launch), false);
                    return unavailable();
                }
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
        let me = Arc::clone(self);
        let ask = ask.clone();
        tokio::spawn(async move { me.cancel_abort_owned(&ask).await })
            .await
            .unwrap_or_else(|_| unavailable())
    }

    async fn cancel_abort_owned(self: &Arc<Self>, ask: &JValue) -> Answer {
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
        "attach" => auto.owner.attach_actor(&body),
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

#[cfg(test)]
mod cancellation_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn an_unreceived_acquisition_result_drops_its_hold() {
        for cancel_before_acquisition in [true, false] {
            let d = tempfile::tempdir().unwrap();
            let research = Research::new(crate::research_routes::Settings {
                upstream: "http://127.0.0.1:1".into(),
                ..Default::default()
            })
            .unwrap();
            let auto = Arc::new(Autotune::without_maintenance(
                d.path(),
                Arc::new(research),
                crate::model_maint::lease::Authority::new(),
            ));
            let attached = auto
                .owner
                .attach("native-autotune-0123456789abcdef.json", true)
                .unwrap();
            let writer = attached.get("writer").as_str().unwrap().to_owned();
            fn parse(s: &str) -> JValue {
                wtf16::parse(s).unwrap()
            }
            let body = parse(&format!(
                r#"{{"writer":"{writer}","jobRev":1,"history":{{}},"job":{{"id":"j","status":"interrupted","models":[]}}}}"#
            ));
            assert_eq!(auto.owner.commit(&body).0, 200);
            let outside = Outside::parse(&parse(
                r#"{"revision":"r1","allowBelowQ5Kv":false,"models":{}}"#,
            ))
            .unwrap();
            let token = auto
                .owner
                .resume_facts(&outside)
                .1
                .get("token")
                .as_str()
                .unwrap()
                .to_owned();
            let (entered, entry) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let me = Arc::clone(&auto);
            let (acquired, acquisition) = tokio::sync::oneshot::channel();
            let task = tokio::task::spawn_blocking(move || {
                let _ = entered.send(());
                if cancel_before_acquisition {
                    released.recv().unwrap();
                }
                let p = me
                    .owner
                    .resume_prepare_begin(&token, &"listing".into(), &JValue::Arr(vec![]), &outside)
                    .unwrap();
                let guard = PrepGuard {
                    auto: me,
                    id: p.id,
                    armed: true,
                };
                let _ = acquired.send(());
                if !cancel_before_acquisition {
                    released.recv().unwrap();
                }
                guard
            });
            entry.await.unwrap();
            let mut acquisition = Some(acquisition);
            if !cancel_before_acquisition {
                acquisition.take().unwrap().await.unwrap();
            }
            // Dropping the receiver while the blocking closure is queued/running cannot cancel it.
            drop(task);
            release.send(()).unwrap();
            if let Some(acquisition) = acquisition {
                acquisition.await.unwrap();
            }
            let mut free = false;
            for _ in 0..100 {
                let result = auto
                    .owner
                    .start_hold(&parse(&format!(r#"{{"writer":"{writer}"}}"#)), true);
                if result.0 == 200 {
                    free = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(free, "abandoned acquisition retained its hold");
        }
    }
}
