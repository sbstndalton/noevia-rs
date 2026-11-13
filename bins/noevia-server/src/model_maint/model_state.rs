//! R3-0: this front is the one writer of the model state Core used to keep in memory: the last
//! model the engine reported loaded (Core's LAST_LOADED_MODEL, server/models.cjs) and the 30 s
//! evidence identity cache (Core's identityCache, llamacpp-manager.cjs forgetIdentity). Neither is
//! persisted, so there is no format to share; the two processes agree by one call: the front clears
//! its own copy and then asks Core to clear its, over the internal bridge
//! (`POST /internal/model-state/forget`, core rust-model-state-bridge.cjs). Core asks for the same
//! clear through the project-store listener (`POST /v1/model-state/forget`, auto_roles_routes.rs).
use super::Maint;
use std::sync::Mutex;

/// The model named as the no-model-selected default.
#[derive(Debug, Default)]
pub struct LastLoaded(Mutex<Option<String>>);

impl LastLoaded {
    pub fn get(&self) -> Option<String> {
        self.0.lock().ok().and_then(|g| g.clone())
    }
    /// Names `name` only while nothing is named (Core's `if (!LAST_LOADED_MODEL) ... = firstLoaded`).
    pub fn adopt(&self, name: &str) -> bool {
        let Ok(mut g) = self.0.lock() else {
            return false;
        };
        if name.is_empty() || g.is_some() {
            return false;
        }
        *g = Some(name.to_owned());
        true
    }
    /// Clears it only when it is still `name` (a load in between is left alone).
    pub fn clear(&self, name: &str) -> bool {
        let Ok(mut g) = self.0.lock() else {
            return false;
        };
        if g.as_deref() == Some(name) {
            *g = None;
            return true;
        }
        false
    }
}

impl Maint {
    /// The last model the engine reported loaded, if one was named.
    pub fn last_loaded(&self) -> Option<String> {
        self.last_loaded.get()
    }
    pub fn adopt_last_loaded(&self, name: &str) -> bool {
        self.last_loaded.adopt(name)
    }
    pub fn clear_last_loaded(&self, name: &str) -> bool {
        self.last_loaded.clear(name)
    }
    /// Drops the cached identity of `model` (Core's modelManager.forgetIdentity); `true` when one was held.
    pub fn forget_identity(&self, model: &str) -> bool {
        self.identities
            .lock()
            .map(|mut c| {
                let before = c.len();
                c.retain(|(k, _, _)| k != model);
                c.len() != before
            })
            .unwrap_or(false)
    }
    /// What a deleted model must leave behind nothing of, here: the last-loaded default (when it is
    /// that model) and its cached identity. `true` when the last-loaded name was cleared.
    pub fn forget_model_local(&self, model: &str) -> bool {
        self.forget_identity(model);
        self.clear_last_loaded(model)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn last_loaded_is_adopted_once_and_cleared_only_by_its_own_name() {
        let s = LastLoaded::default();
        assert_eq!(s.get(), None);
        assert!(!s.adopt(""));
        assert!(s.adopt("qa-a"));
        assert!(!s.adopt("qa-b"));
        assert_eq!(s.get().as_deref(), Some("qa-a"));
        assert!(!s.clear("qa-b"));
        assert_eq!(s.get().as_deref(), Some("qa-a"));
        assert!(s.clear("qa-a"));
        assert_eq!(s.get(), None);
        assert!(!s.clear("qa-a"));
        assert!(s.adopt("qa-b"));
    }

    #[test]
    fn forgetting_a_model_drops_its_identity_and_its_last_loaded_name_only() {
        let env: std::collections::HashMap<String, String> = [
            ("MODEL_MANAGER_KIND", "llamacpp"),
            ("MODEL_MANAGER_BASE_URL", "http://127.0.0.1:9/v1"),
            ("INFERENCE_GATE_ADMIN_TOKEN", "synthetic-admin-token"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let get = |k: &str| env.get(k).cloned();
        let engine = std::sync::Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
        let sizing = std::sync::Arc::new(crate::model_sizing::Sizing::from_lookup(
            engine.kind(),
            std::sync::Arc::clone(&engine),
            &get,
        ));
        let maint = Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap();
        maint.cache_identity("qa-a", None);
        maint.cache_identity("qa-b", None);
        assert!(maint.adopt_last_loaded("qa-a"));
        assert!(maint.forget_model_local("qa-a"));
        assert_eq!(maint.last_loaded(), None);
        assert!(!maint.forget_identity("qa-a"), "already gone");
        assert!(maint.forget_identity("qa-b"));
        assert!(!maint.forget_model_local("qa-b"));
    }
}
