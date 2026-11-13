//! Native reranker settings ownership and its authenticated consumer snapshot.
use crate::model_maint::v8_json::{self, W};
use crate::{
    identity, reply,
    reranker_settings::{self, quote, units, Environment, Input, InstalledModel, Settings},
    App,
};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use serde_json::{json, Value};
use server_store::{Reader, RustRerankerSettings, StoreError};
use std::{sync::Arc, time::Duration};
pub const PATH: &str = "/api/models/reranker";
fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Reranker settings are unavailable.",
        true,
    )
}
fn answer(code: u16, body: Value) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        body.to_string(),
        true,
    )
}
fn setting(reader: &Reader<'_>, key: &str) -> Result<Option<String>, StoreError> {
    Ok(reader
        .row(
            "SELECT CAST(value AS BLOB) FROM settings WHERE key=?1",
            [key],
            |r| r.get::<_, Option<Vec<u8>>>(0),
        )?
        .flatten()
        .map(|s| String::from_utf8_lossy(&s).into_owned()))
}
fn machine(raw: Option<&str>) -> Option<(Vec<u16>, String)> {
    let state = v8_json::parse(&units(raw?)).ok()?;
    if !matches!(state.get("version"),W::Num(n) if *n==1.0) {
        return None;
    }
    let W::Arr(machines) = state.get("machines") else {
        return None;
    };
    let a = state.get("assignments").get("reranker");
    let W::Str(id) = a.get("machine") else {
        return None;
    };
    if id == &units("local") {
        return None;
    }
    let W::Num(port) = a.get("port") else {
        return None;
    };
    if !port.is_finite() || port.fract() != 0.0 {
        return None;
    }
    let m = machines.iter().find(|m| {
        matches!(m.get("id"),W::Str(value) if value==id)
            && matches!(m.get("name"), W::Str(_))
            && matches!(m.get("address"), W::Str(_))
    })?;
    let W::Str(address) = m.get("address") else {
        return None;
    };
    let mut url = address.clone();
    url.extend(units(&format!(":{}", js_json::number_to_string(*port))));
    let protocol = if matches!(a.get("protocol"),W::Str(s) if *s==units("llamacpp")) {
        "llamacpp"
    } else {
        "tei"
    };
    Some((url, protocol.into()))
}
fn load(reader: &Reader<'_>, env: Environment) -> Result<Settings, StoreError> {
    let stored = setting(reader, reranker_settings::KEY)?;
    let machines = setting(reader, "inference:machines")?;
    Ok(Settings::new(
        env,
        machine(machines.as_deref()),
        stored.as_deref(),
    ))
}
/// The reranker's effective environment, as `/v1/reranker/settings` gives it to Node, for this
/// process's own retrieval (`native_retrieval`). Blocking: run it on the blocking pool.
pub fn effective_env(app: &App) -> Result<std::collections::HashMap<String, String>, StoreError> {
    app.identity.store_blocking()?.read(|r| {
        Ok(load(r, app.config.reranker.0.clone())?
            .effective_env()
            .into_iter()
            .map(|(k, v)| (k, String::from_utf16_lossy(&v)))
            .collect())
    })
}
pub async fn internal(app: Arc<App>, installed: Option<Vec<InstalledModel>>) -> Response<Body> {
    let result = tokio::task::spawn_blocking(move || {
        app.identity.store_blocking()?.read(|r| {
            let settings = load(r, app.config.reranker.0.clone())?;
            // The reader transaction pins configuration and generation to one SQLite snapshot.
            let generation = setting(r, "rerank:revision")?.unwrap_or_else(|| "0".into());
            let generation = generation
                .parse::<u64>()
                .map_err(|_| StoreError::NotOwned("invalid reranker revision".into()))?;
            // Retain the private client protocol shape; the generation is shared, never App-local.
            let revision = format!("00000000000000000000000000000000:{generation}");
            Ok(format!(
                "{{\"env\":{},\"view\":{},\"revision\":{}}}",
                settings.effective_env_json(),
                settings.view_json(installed.as_deref()),
                quote(&units(&revision))
            ))
        })
    })
    .await;
    match result {
        Ok(Ok(body)) => reply::json_text(StatusCode::OK, body, true),
        _ => unavailable(),
    }
}
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != PATH {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = app.identity.clone();
    let now = identity::now_ms();
    let id = match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, PATH, now)).await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        _ => return unavailable(),
    };
    if id.user.role != server_auth::Role::Admin {
        return reply::error(StatusCode::FORBIDDEN, "administrator required", true);
    }
    let method = parts.method;
    if !matches!(method, Method::GET | Method::PUT | Method::DELETE) {
        return reply::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", true);
    }
    // Node obtained its best-effort catalogue before reading a PUT body.
    let response = crate::model_state_routes::serve(app, "/api/models/installed", true).await;
    let installed = if response.status() == StatusCode::OK {
        axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .ok()
            .and_then(|b| reranker_settings::parse_installed(&b))
    } else {
        None
    };
    let input = if method == Method::PUT {
        let bytes = match tokio::time::timeout(
            Duration::from_secs(10),
            axum::body::to_bytes(body, 1024 * 1024),
        )
        .await
        {
            Ok(Ok(b)) => b,
            _ => {
                return answer(
                    400,
                    json!({"error":"invalid JSON","errorId":"invalidInput"}),
                )
            }
        };
        match reranker_settings::parse_input(&bytes) {
            Some(v) => v,
            None => {
                return answer(
                    400,
                    json!({"error":"invalid JSON","errorId":"invalidInput"}),
                )
            }
        }
    } else {
        Input::default()
    };
    let app = app.clone();
    let actor = id.user.id;
    let result = tokio::task::spawn_blocking(move || -> Result<Result<String, reranker_settings::Refusal>, StoreError> {
        if method == Method::GET {
            return app.identity.store_blocking()?.read(|r| {
                Ok(Ok(load(r, app.config.reranker.0.clone())?.view_json(installed.as_deref())))
            });
        }
        let written = app.writes.writer()?.write(|tx| {
            let mut settings = load(&tx.reader(), app.config.reranker.0.clone())?;
            let (action, detail) = if method == Method::DELETE || input.reset {
                let had = settings.saved.is_some();
                tx.set_reranker_configuration(RustRerankerSettings::owned(), None)?;
                settings.saved = None;
                ("rerank.reset", json!({"hadSaved": had}).to_string())
            } else {
                let next = match settings.validate(&input, installed.as_deref()) {
                    Ok(next) => next,
                    Err(r) => return Ok(Err(r)),
                };
                let picked = settings.options(installed.as_deref()).into_iter()
                    .find(|o| Some(&o.id) == next.choice.as_ref());
                let backend = picked.as_ref().map(|p| quote(&units(p.backend))).unwrap_or_else(|| "null".into());
                let model = picked.as_ref().map(|p| quote(&p.model)).unwrap_or_else(|| "null".into());
                let detail = format!("{{\"enabled\":{},\"backend\":{},\"model\":{}}}", next.enabled, backend, model);
                tx.set_reranker_configuration(RustRerankerSettings::owned(), Some(&next.json()))?;
                settings.saved = Some(next);
                ("rerank.configure", detail)
            };
            tx.execute(
                "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",
                server_store::rusqlite::params![actor, action, detail, identity::now_ms()],
            )?;
            Ok(Ok(settings.view_json(installed.as_deref())))
        })?;
        Ok(written)
    }).await;
    match result {
        Ok(Ok(Ok(body))) => reply::json_text(StatusCode::OK, body, true),
        Ok(Ok(Err(r))) => answer(r.status, r.body),
        _ => unavailable(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_json_field_order() {
        let settings = Settings::new(
            [("RERANK_BACKEND".into(), "tei".into())]
                .into_iter()
                .collect(),
            None,
            None,
        );
        assert_eq!(
            settings.view_json(None),
            r#"{"enabled":false,"choice":"tei","source":"deployment","options":[{"id":"tei","backend":"tei","model":"gte-multilingual-reranker-base"}]}"#
        );
    }
    #[test]
    fn input_accepts_json_parse_overflow_deep_and_surrogate_fields(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use reranker_settings::{parse_input, ChoiceInput};
        for bytes in [
            br#"{"enabled":false,"unused":1e999}"#.as_slice(),
            br#"{"enabled":false,"unused":"\ud800"}"#.as_slice(),
        ] {
            assert_eq!(parse_input(bytes).ok_or("input")?.enabled, Some(false));
        }
        assert_eq!(
            parse_input(br#"{"enabled":false,"choice":"\ud800"}"#)
                .ok_or("input")?
                .choice,
            ChoiceInput::Text(vec![0xd800])
        );
        assert!(parse_input(b"{").is_none());
        let deep = format!(
            "{{\"enabled\":false,\"unused\":{}0{}}}",
            "[".repeat(256),
            "]".repeat(256)
        );
        assert_eq!(
            parse_input(deep.as_bytes()).ok_or("input")?.enabled,
            Some(false)
        );
        Ok(())
    }
    #[test]
    fn machine_row_accepts_legacy_utf16_names_and_javascript_numbers() {
        let parsed = machine(Some(
            r#"{"version":1.0,"machines":[{"id":"synthetic","name":"\ud800","address":"http://synthetic-host"}],"assignments":{"reranker":{"machine":"synthetic","port":9000.0,"protocol":"llamacpp"}},"unused":1e999}"#,
        ));
        assert_eq!(
            parsed,
            Some((units("http://synthetic-host:9000"), "llamacpp".into()))
        );
    }
}
