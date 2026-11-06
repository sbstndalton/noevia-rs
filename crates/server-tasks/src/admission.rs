//! Code's maintenance-only adapter to the shared inference authority. The account-bound
//! front policy resolves local/remote roles; no model is selected or loaded by this permit.
use crate::{
    code::{Admission, Entering, Failure},
    context::{JournalContext, Permit},
};
use engine_admission::{Admission as Shared, Ask, Lease};
use model_stream::Cancel;
use std::sync::Arc;
pub trait Policy: Send + Sync {
    fn paused(&self) -> Result<bool, Failure>;
    fn local(&self, context: &JournalContext) -> Result<bool, Failure>;
}
pub struct Tasks {
    pub shared: Arc<Shared>,
    pub policy: Arc<dyn Policy>,
}
struct Held(Lease);
impl Permit for Held {
    fn valid(&self) -> bool {
        self.0.covered()
    }
}
struct Remote;
impl Permit for Remote {
    fn valid(&self) -> bool {
        true
    }
}
struct Watch {
    cancel: Cancel,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
impl Admission for Tasks {
    fn paused(&self) -> Result<bool, Failure> {
        self.policy.paused()
    }
    fn enter_for<'a>(&'a self, context: Arc<JournalContext>) -> Entering<'a> {
        Box::pin(async move {
            if !self.policy.local(&context)? {
                return Ok(Arc::new(Remote) as Arc<dyn Permit>);
            }
            let cancel = Cancel::new();
            let mut signal = context.cancellation();
            let (stop, stopped) = tokio::sync::oneshot::channel();
            let watch = Watch {
                cancel: cancel.clone(),
                stop: Some(stop),
            };
            let forwarded = cancel.clone();
            tokio::spawn(async move {
                if *signal.borrow() {
                    forwarded.cancel();
                    return;
                }
                tokio::select! {biased;_=signal.changed()=>forwarded.cancel(),_=stopped=>{}}
                drop(context);
            });
            let lease = self
                .shared
                .enter(Ask::Maintenance, &cancel)
                .await
                .map_err(|e| Failure::public(e.status, e.message))?;
            drop(watch);
            Ok(Arc::new(Held(lease)) as Arc<dyn Permit>)
        })
    }
}
