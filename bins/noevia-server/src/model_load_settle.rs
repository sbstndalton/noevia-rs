//! Custody of a possibly asynchronous engine mutation, using J's existing authority and Node latch.
//! The incident is acknowledged BEFORE dispatch: H may end its latch immediately on renewal loss.
//! Neither a caller drop nor a failed/expired HTTP request is evidence of engine settlement.
use super::*;
use crate::model_maint::{
    lease::{Grant, Origin},
    Hold, Maint,
};
use std::sync::atomic::AtomicBool;

const REPAIR: Duration = Duration::from_secs(600);
const REASON: &str = "model mutation settlement";

pub(super) enum Operation {
    Load,
    Unload {
        admin: Option<(HeaderName, HeaderValue)>,
        can_force: bool,
    },
}

/// Sending a POST is not evidence that its handler has started. Only an acknowledged response
/// or a post-dispatch transitional observation can establish causality for terminal reads.
#[derive(Clone, Copy)]
#[repr(u8)]
enum Outcome {
    NotDispatched,
    Uncertain,
    Acknowledged,
    Refused,
}

#[derive(Default)]
pub(super) struct Dispatch {
    outcome: std::sync::atomic::AtomicU8,
    transitioned: AtomicBool,
    before: std::sync::Mutex<Option<String>>,
}
impl Dispatch {
    pub(super) fn baseline(&self, reply: &Reply, model: &str) {
        let value = rows(reply)
            .and_then(|r| state_of(r, model))
            .and_then(|m| m.get("status"))
            .and_then(|s| s.get("value"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        *self.before.lock().unwrap_or_else(|p| p.into_inner()) = value;
    }
    pub(super) fn attempting(&self) {
        self.outcome.store(
            Outcome::Uncertain as u8,
            std::sync::atomic::Ordering::SeqCst,
        );
    }
    pub(super) fn acknowledged(&self) {
        self.outcome.store(
            Outcome::Acknowledged as u8,
            std::sync::atomic::Ordering::SeqCst,
        );
    }
    pub(super) fn refused(&self) {
        self.outcome
            .store(Outcome::Refused as u8, std::sync::atomic::Ordering::SeqCst);
    }
    fn state(&self) -> Outcome {
        match self.outcome.load(std::sync::atomic::Ordering::SeqCst) {
            0 => Outcome::NotDispatched,
            2 => Outcome::Acknowledged,
            3 => Outcome::Refused,
            _ => Outcome::Uncertain,
        }
    }
    fn needs_engine(&self) -> bool {
        matches!(self.state(), Outcome::Uncertain | Outcome::Acknowledged)
    }
    fn uncertain(&self) -> bool {
        matches!(self.state(), Outcome::Uncertain)
    }
}

/// An owned J grant, not another lease authority. Its incident survives release, expiry and restart.
struct Claim {
    maint: Arc<Maint>,
    grant: Option<Grant>,
    renew: tokio::task::JoinHandle<()>,
}
impl Claim {
    fn new(maint: &Arc<Maint>, grant: Grant) -> Self {
        let authority = maint.authority.clone();
        let id = grant.id.clone();
        let generation = grant.generation;
        let renew = tokio::spawn(async move {
            loop {
                tokio::time::sleep(authority.renew_every()).await;
                if !authority.renew(&id, Some(generation)) {
                    break;
                }
            }
        });
        Self {
            maint: Arc::clone(maint),
            grant: Some(grant),
            renew,
        }
    }
    fn covered(&self) -> bool {
        self.grant.as_ref().is_some_and(|g| {
            self.maint
                .authority
                .covers(&g.id, g.generation, Duration::ZERO)
        })
    }
    fn transfer(mut self) -> Option<Grant> {
        self.renew.abort();
        self.grant.take()
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        self.renew.abort();
        if let Some(grant) = self.grant.take() {
            self.maint
                .authority
                .release(&grant.id, Some(grant.generation));
        }
    }
}

pub(super) async fn run(
    maint: Arc<Maint>,
    router: router::Router,
    model: String,
    lease: Lease,
    op: Operation,
    gate_verified: bool,
) -> Response<Body> {
    let (send, recv) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if !lease.covered() {
            let _ = send.send(error(503, LEASE_LOST));
            return;
        }
        // Reserve the SAME J authority that all maintenance/recovery uses. This also binds Node's
        // incarnation before any engine request; its restart handshake reconstructs front_held.
        let Ok(grant) = maint
            .authority
            .try_acquire(Origin::Front, Some(REPAIR), REASON)
        else {
            let _ = send.send(error(503, LEASE_LOST));
            return;
        };
        let claim = Claim::new(&maint, grant);
        let ticket = maint.authority.require_recovery();
        let armed = maint.node.recovery_barrier(true, &ticket).await;
        let dispatch = Dispatch::default();
        let mut send = Some(send);
        let outcome = if armed && lease.covered() && claim.covered() {
            let operation = async {
                match &op {
                    Operation::Load => {
                        load_steps(&router, &model, &lease, &dispatch, &|| {
                            lease.covered() && claim.covered()
                        })
                        .await
                    }
                    Operation::Unload { admin, can_force } => {
                        unload_steps(
                            &router,
                            &model,
                            admin.clone(),
                            *can_force,
                            &dispatch,
                            &|| lease.covered() && claim.covered(),
                        )
                        .await
                    }
                }
            };
            tokio::pin!(operation);
            tokio::select! {
                response = &mut operation => response,
                () = lease.lost().cancelled() => {
                    if let Some(send) = send.take() { let _ = send.send(error(503, LEASE_LOST)); }
                    // Do not drop an in-flight POST: its handler may still accept the effect.
                    // Only the caller is answered; custody continues to observe the SAME request.
                    operation.await
                },
            }
        } else {
            error(503, LEASE_LOST)
        };
        if let Some(send) = send.take() {
            let _ = send.send(outcome);
        }
        // H can now relinquish its latch: the acknowledged J incident owns exclusion independently.
        // If Node restarted, J's registration handshake retains that incident before admitting work.
        lease.release().await;
        // Keep the original claim through a bounded settlement window. Unknown results retain the
        // incident; repair then uses fresh coverage instead of extending/reusing a lost grant.
        if armed && !gate_verified && dispatch.needs_engine() {
            let until = Instant::now() + READY_WITHIN;
            while Instant::now() < until {
                if terminal(
                    &router,
                    &model,
                    matches!(op, Operation::Unload { .. }),
                    &dispatch,
                )
                .await
                {
                    break;
                }
                tokio::time::sleep(READY_STEP).await;
            }
        }
        drop(claim);
        reconcile(
            maint,
            router,
            model,
            matches!(op, Operation::Unload { .. }),
            dispatch,
            ticket,
        )
        .await;
    });
    match tokio::time::timeout(LOAD_TIMEOUT, recv).await {
        Ok(Ok(response)) => response,
        _ => error(503, LEASE_LOST),
    }
}

async fn terminal(router: &router::Router, model: &str, unload: bool, dispatch: &Dispatch) -> bool {
    // No POST, or a definitive refusal: fresh Node coverage suffices. Do not create an engine
    // readability dependency for work that is proven not to exist.
    if !dispatch.needs_engine() {
        return true;
    }
    let Ok(reply) = router.models(None).await else {
        return false;
    };
    if !reply.ok {
        return false;
    }
    let Some(list) = rows(&reply) else {
        return false;
    };
    let Some(entry) = state_of(list, model) else {
        return false;
    };
    let status = entry.get("status");
    let value = status.and_then(|s| s.get("value")).and_then(Value::as_str);
    if matches!(value, Some("loading" | "unloading")) {
        let before = dispatch.before.lock().unwrap_or_else(|p| p.into_inner());
        if before.as_deref().is_some_and(|old| Some(old) != value) {
            dispatch
                .transitioned
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        return false;
    }
    if dispatch.uncertain()
        && !dispatch
            .transitioned
            .load(std::sync::atomic::Ordering::SeqCst)
    {
        // An unchanged old unloaded/loaded/failed state says nothing about a queued POST that
        // lost its response. Keep custody until this operation's transition has been observed.
        return false;
    }
    truthy(status.and_then(|s| s.get("failed")))
        || matches!(value, Some("failed"))
        || (unload && value == Some("unloaded"))
        || (!unload && matches!(value, Some("loaded" | "sleeping")))
        || (dispatch
            .transitioned
            .load(std::sync::atomic::Ordering::SeqCst)
            && value == Some("unloaded"))
}

/// Use J's Node-backed renewable Hold even with a gate: clearing Node's incident requires the fresh
/// Node lease/generation proof, which a gate-only hold cannot supply. Gate counted settlement remains
/// active independently, and its HTTP request headers and admission are unchanged.
async fn repair_hold(maint: &Arc<Maint>) -> Option<Hold> {
    let grant = maint.authority.try_recovery(Some(REPAIR), REASON).ok()?;
    let claim = Claim::new(maint, grant);
    let ticket = claim.grant.as_ref()?.id.clone();
    let node = match maint
        .node
        .acquire_kind(Some(REPAIR), Some(&ticket), true)
        .await
    {
        Ok(grant) => grant,
        Err(_) => {
            // The acquire may have landed despite a lost response; return its known ticket.
            maint.node.release_ticket(&ticket).await;
            return None;
        }
    };
    let own = claim.transfer()?;
    Some(Hold::leased(
        Arc::new(AtomicBool::new(false)),
        Arc::clone(&maint.node),
        node,
        Some((maint.authority.clone(), own)),
        REASON,
        Instant::now() + REPAIR,
    ))
}

async fn reconcile(
    maint: Arc<Maint>,
    router: router::Router,
    model: String,
    unload: bool,
    dispatch: Dispatch,
    ticket: String,
) {
    loop {
        if maint.node.recovery_barrier(true, &ticket).await {
            if let Some(hold) = repair_hold(&maint).await {
                let until = Instant::now() + READY_WITHIN;
                while hold.excluded() && Instant::now() < until {
                    // Fresh covered reread, checked on BOTH sides of the await. No mutation retries.
                    if terminal(&router, &model, unload, &dispatch).await && hold.excluded() {
                        if let Some((lease, generation)) = hold.recovery_proof() {
                            if maint
                                .node
                                .complete_recovery(&ticket, lease, generation)
                                .await
                                && hold.excluded()
                                && maint.authority.complete_recovery(&ticket)
                            {
                                hold.release().await;
                                return;
                            }
                        }
                    }
                    tokio::time::sleep(READY_STEP).await;
                }
                // Hold's existing release path acknowledges the still-required incident before its
                // latch is relinquished. Lost coverage never clears the incident.
                hold.release().await;
            }
        }
        tokio::time::sleep(READY_STEP).await;
    }
}
