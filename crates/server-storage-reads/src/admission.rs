//! Nonqueueing admission, held through blocking store and outbound operation.
//! Owner must come from validated auth, never body/path/query input.
use crate::Failure;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
pub struct Admission {
    active: Arc<Mutex<BTreeSet<String>>>,
}
pub struct Permit {
    active: Arc<Mutex<BTreeSet<String>>>,
    owner: String,
}
impl Admission {
    pub fn try_acquire(&self, owner: &str) -> Result<Permit, Failure> {
        if owner.is_empty() || owner.len() > 4096 {
            return Err(Failure::new(503, "Storage reads unavailable"));
        }
        let mut active = self
            .active
            .lock()
            .map_err(|_| Failure::new(503, "Storage reads unavailable"))?;
        if active.len() >= 4 || active.contains(owner) {
            return Err(Failure::new(503, "Storage reads unavailable"));
        }
        active.insert(owner.to_owned());
        Ok(Permit {
            active: Arc::clone(&self.active),
            owner: owner.to_owned(),
        })
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut active = match self.active.lock() {
            Ok(active) => active,
            Err(error) => error.into_inner(),
        };
        active.remove(&self.owner);
    }
}
