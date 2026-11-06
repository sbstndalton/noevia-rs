//! Explicit gate settlement handshake. Call again after a connection failure; a health answer,
//! configured credential or endpoint name never establishes settlement ownership.
use crate::model_maint::router::Router;
use reqwest::header::{HeaderName, HeaderValue};
use std::time::Duration;

pub async fn verified(router: &Router, admin: Option<(HeaderName, HeaderValue)>) -> bool {
    let Some(admin) = admin else { return false };
    let Ok(reply) = router
        .request_with(
            reqwest::Method::GET,
            "/gate/v1/capabilities",
            None,
            None,
            &[admin],
            Duration::from_secs(5),
        )
        .await
    else {
        return false;
    };
    if reply.status != 200 {
        return false;
    }
    let Some(body) = reply.body else { return false };
    body.get("protocol").and_then(serde_json::Value::as_str) == Some("noevia-gate-capabilities/1")
        && body
            .get("capabilities")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .filter(|v| v.as_str() == Some("noevia-model-load-settlement/1"))
                    .count()
                    == 1
            })
}

/// Latest authenticated observation for this configured engine. Each use probes again, so a
/// reconnect or replacement at the same address cannot inherit an earlier gate proof.
#[derive(Clone)]
pub struct Capability(std::sync::Arc<Observation>);

struct Observation {
    router: Option<Router>,
    admin: Option<(HeaderName, HeaderValue)>,
    // Low bit is proof; upper bits order concurrent probes (including the startup probe).
    state: std::sync::atomic::AtomicU64,
}

impl Capability {
    pub fn start(router: Option<Router>, admin: Option<(HeaderName, HeaderValue)>) -> Self {
        let this = Self(std::sync::Arc::new(Observation {
            router,
            admin,
            state: std::sync::atomic::AtomicU64::new(0),
        }));
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let probe = this.clone();
            rt.spawn(async move {
                probe.refresh().await;
            });
        }
        this
    }

    pub fn is_verified(&self) -> bool {
        self.0.state.load(std::sync::atomic::Ordering::SeqCst) & 1 != 0
    }

    pub async fn refresh(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        let mut previous = self.0.state.load(SeqCst);
        let ticket = loop {
            let ticket = (previous & !1).wrapping_add(2);
            match self
                .0
                .state
                .compare_exchange(previous, ticket, SeqCst, SeqCst)
            {
                Ok(_) => break ticket,
                Err(current) => previous = current,
            }
        };
        let proof = match self.0.router.as_ref() {
            Some(router) => verified(router, self.0.admin.clone()).await,
            None => false,
        };
        // A late reply from an earlier connection cannot overwrite a newer observation.
        self.0
            .state
            .compare_exchange(ticket, ticket | u64::from(proof), SeqCst, SeqCst)
            .is_ok()
            && proof
    }
}
