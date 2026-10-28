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

/// Dedicated DAV router retains the same address-based code-network denial as the UI.
pub fn dav_router(
    front: &Arc<noevia_server::App>,
    service: Arc<server_dav::DavService<crate::dav::DavBackend>>,
) -> axum::Router {
    let guard = Arc::clone(&front.code_net);
    axum::Router::new().fallback(
        move |axum::extract::ConnectInfo(conn): axum::extract::ConnectInfo<
            noevia_server::serve::Conn,
        >,
              request: axum::http::Request<axum::body::Body>| {
            let guard = Arc::clone(&guard);
            let service = Arc::clone(&service);
            async move {
                if guard.enabled()
                    && match conn.local {
                        Some(local) => guard.refuses(local).await,
                        None => true,
                    }
                {
                    return guard.deny();
                }
                service.handle(request, conn.peer.ip()).await
            }
        },
    )
}
