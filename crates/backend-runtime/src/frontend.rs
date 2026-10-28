//! Assemble the original Rust HTTP front and real native execution in one process.
//! Existing native route handlers, static files and connection/code-network gates remain
//! intact. Every old proxy delegation invokes this native runtime, never Node/Python.
//! Unsupported remaining contracts remain absent, so this is not release qualification.
use crate::{api::Api, Fault, Result};
use std::sync::Arc;
pub fn assemble(
    config: noevia_server::config::Config,
    api: Arc<Api>,
) -> Result<Arc<noevia_server::App>> {
    let mut app = noevia_server::App::new(config);
    let unique = Arc::get_mut(&mut app).ok_or(Fault::Unavailable)?;
    let executor: noevia_server::legacy_proxy::NativeExecutor = Arc::new(move |peer, request| {
        let api = Arc::clone(&api);
        Box::pin(async move { api.serve_request(peer, request).await })
    });
    unique.proxy = noevia_server::legacy_proxy::LegacyProxy::in_process(
        unique.config.upstream.clone(),
        unique.config.trust_proxy,
        executor,
    );
    Ok(app)
}
