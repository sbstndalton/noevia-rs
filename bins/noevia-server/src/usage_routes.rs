//! M6: native usage reporting. No usage writes and no call to Node reporting.
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::response::IntoResponse;
use server_usage::{read::AggregateReader, summary::summarize, Fault};
use std::sync::Arc;
use tokio::sync::Semaphore;

pub struct UsageLayer {
    pub reader: AggregateReader,
    pub admission: Arc<Semaphore>,
}
impl Default for UsageLayer {
    fn default() -> Self {
        Self {
            reader: AggregateReader::default(),
            admission: Arc::new(Semaphore::new(4)),
        }
    }
}
fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Aggregate usage could not be read within its limits.",
        true,
    )
}
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    // Route-table matching accepts trailing slash; Node's exact usage matcher does not.
    if path != "/api/usage" && path != "/api/usage/aggregate" {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
    let Some(data) = app.config.data_dir.clone() else {
        return identity::Rejection::Unavailable.into_response();
    };
    let Ok(permit) = Arc::clone(&app.usage.admission).try_acquire_owned() else {
        return unavailable();
    };
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gatepath = path.clone();
    let id = match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gatepath, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if parts.method != "GET" {
        return reply::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", true);
    }
    let aggregate = path == "/api/usage/aggregate";
    if aggregate && id.user.role != server_auth::Role::Admin {
        return reply::error(StatusCode::FORBIDDEN, "administrator required", true);
    }
    let usage = Arc::clone(&app.usage);
    let clock = app.config.usage_clock.clone();
    let uid = id.user.id;
    let result=tokio::task::spawn_blocking(move||->Result<serde_json::Value,Fault>{
        let _permit=permit;let clock=clock?;
        if aggregate{
            let store=server_store::Store::open(&data).map_err(|_|Fault::Database)?;
            let users=store.read(|r|r.rows("SELECT id FROM users ORDER BY created_at LIMIT 1001",[],|row|row.get::<_,String>(0))).map_err(|_|Fault::Database)?;
            let a=usage.reader.get(&data,users,now)?;
            let mut v=summarize(&a.store,&clock,now,365)?;
            if let Some(m)=v.as_object_mut(){m.insert("aggregate".into(),serde_json::json!({"accounts":a.accounts,"unreadableAccounts":a.unreadable,"checkedAt":a.checked_at}));}
            Ok(v)
        }else{let s=server_usage::read::own(&data,&uid)?;summarize(&s,&clock,now,365)}
    }).await;
    match result {
        Ok(Ok(v)) => reply::json_text(StatusCode::OK, v.to_string(), true),
        _ => unavailable(),
    }
}
