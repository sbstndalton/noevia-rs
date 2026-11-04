//! Model assignments describe configuration. Reachability stays with the machines probe.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::{wtf16, JValue};
use serde_json::json;
use std::{sync::Arc, time::Duration};

pub const PATH: &str = "/api/models/assignments";
const FACTS: &str = "/internal/model-assignments/facts";

fn flag(value: &JValue, key: &str) -> bool {
    value.get(key).as_bool() == Some(true)
}

fn installed_entry<'a>(list: &'a [JValue], model: &JValue) -> Option<&'a JValue> {
    list.iter().rev().find(|row| row.get("name") == model)
}

fn installed_state(list: &[JValue], model: &JValue) -> &'static str {
    if !model.truthy() {
        return "off";
    }
    match installed_entry(list, model) {
        Some(row) if flag(row, "missingFile") => "missing",
        None if !list.is_empty() => "missing",
        _ => "active",
    }
}

fn is_model(row: &JValue, label_prefix: &str, name_fragment: &str) -> bool {
    let matches_label = match row.get("labels") {
        JValue::Arr(labels) => labels.iter().any(|label| {
            label
                .as_str()
                .is_some_and(|s| s.to_ascii_lowercase().starts_with(label_prefix))
        }),
        _ => false,
    };
    matches_label
        || row
            .get("name")
            .as_str()
            .is_some_and(|s| s.to_ascii_lowercase().contains(name_fragment))
}

fn embedding_option(row: &JValue, managed: bool) -> JValue {
    let model = row.get("name");
    let name = model.as_str().unwrap_or_default().to_ascii_lowercase();
    let profile = if name.contains("embeddinggemma") || name.contains("embedding-gemma") {
        "embeddinggemma-2"
    } else {
        "nomic-v1"
    };
    JValue::obj([
        ("model", model.clone()),
        ("profile", profile.into()),
        (
            "served",
            if managed {
                JValue::Bool(true)
            } else {
                row.get("served").clone()
            },
        ),
    ])
}

fn embedding_row(facts: &JValue, list: &[JValue], admin: bool) -> JValue {
    let active = facts.get("active");
    let next = facts.get("next");
    let managed = flag(facts, "managed");
    let off = managed && facts.get("enabled").as_bool() == Some(false);
    let state = if next.truthy() {
        "switching"
    } else if off {
        "off"
    } else {
        installed_state(list, active)
    };
    let unloaded = state == "active"
        && installed_entry(list, active).is_some_and(|row| {
            !flag(row, "missingFile") && !flag(row, "servedElsewhere") && flag(row, "unloaded")
        });
    let state = if !off
        && active
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("default"))
    {
        "active"
    } else if unloaded {
        "unloaded"
    } else {
        state
    };
    let mut row = vec![
        ("id", "embedding".into()),
        ("model", active.clone()),
        ("state", state.into()),
    ];
    if next.truthy() {
        row.extend([
            ("next", next.clone()),
            ("ready", JValue::Bool(flag(facts, "ready"))),
        ]);
    }
    if managed {
        row.push(("managed", JValue::Bool(true)));
    }
    row.push((
        "canChange",
        JValue::Bool(admin && flag(facts, "configured")),
    ));
    if admin {
        if managed {
            row.push(("canTurnOff", JValue::Bool(!off && !next.truthy())));
        }
        let options = list
            .iter()
            .filter(|r| !flag(r, "missingFile") && is_model(r, "embedding", "embed"))
            .filter(|r| off || r.get("name") != active)
            .map(|r| embedding_option(r, managed))
            .collect();
        row.extend([
            ("options", JValue::Arr(options)),
            ("baseUrl", facts.get("baseUrl").clone()),
        ]);
    }
    JValue::obj(row)
}

fn reranker_row(facts: &JValue, list: &[JValue], admin: bool) -> JValue {
    let model = facts.get("model");
    let installed = installed_state(list, model);
    let tei = flag(facts, "tei");
    let enabled = flag(facts, "enabled");
    let state = if tei {
        if enabled {
            "active"
        } else {
            "off"
        }
    } else if enabled || installed == "missing" {
        installed
    } else {
        "off"
    };
    let choice = facts.get("choice");
    let can_change = admin && !choice.is_nullish();
    let mut row = vec![
        ("id", "reranker".into()),
        ("model", model.clone()),
        ("state", state.into()),
        ("canChange", JValue::Bool(can_change)),
    ];
    if admin {
        let options = if tei {
            Vec::new()
        } else {
            list.iter()
                .filter(|r| !flag(r, "missingFile") && is_model(r, "rerank", "rerank"))
                .map(|r| r.get("name").clone())
                .collect()
        };
        row.push(("options", JValue::Arr(options)));
    }
    if can_change {
        row.extend([
            ("enabled", choice.get("enabled").clone()),
            ("choice", choice.get("choice").clone()),
            ("source", choice.get("source").clone()),
            ("choices", choice.get("options").clone()),
        ]);
    }
    JValue::obj(row)
}

fn decision_row(id: &str, facts: &JValue, admin: bool) -> JValue {
    let mut row = vec![
        ("id", id.into()),
        ("model", facts.get("model").clone()),
        ("state", facts.get("state").clone()),
        ("canChange", JValue::Bool(false)),
    ];
    if id == "sensitivity" {
        row.push(("shared", JValue::Bool(flag(facts, "shared"))));
    } else if admin {
        for key in ["source", "modelSource", "models", "deploymentModel"] {
            row.push((key, facts.get(key).clone()));
        }
    }
    JValue::obj(row)
}

fn project(facts: &JValue, admin: bool) -> Option<JValue> {
    let (available, list): (bool, &[JValue]) = match facts.get("installed") {
        JValue::Arr(list) => (true, list),
        JValue::Null => (false, &[]),
        _ => return None,
    };
    let mut services = vec![
        embedding_row(facts.get("embedding"), list, admin),
        reranker_row(facts.get("reranker"), list, admin),
        decision_row("decision", facts.get("decision"), admin),
        decision_row("sensitivity", facts.get("sensitivity"), admin),
    ];
    for id in ["ocr", "documents"] {
        services.push(JValue::obj([
            ("id", id.into()),
            ("model", JValue::Null),
            (
                "state",
                if flag(facts, id) { "active" } else { "off" }.into(),
            ),
            ("canChange", JValue::Bool(false)),
        ]));
    }
    Some(JValue::obj([
        ("admin", JValue::Bool(admin)),
        ("services", JValue::Arr(services)),
        ("available", JValue::Bool(available)),
    ]))
}

fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Model assignments are unavailable.",
        true,
    )
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != PATH || req.method() != Method::GET {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let id = match tokio::task::spawn_blocking(move || {
        gate.gate_blocking(&creds, PATH, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let admin = id.user.role == server_auth::Role::Admin;
    let Some((200, bytes)) = app
        .config
        .research
        .node_call(FACTS, &json!({ "admin": admin }), Duration::from_secs(90))
        .await
    else {
        return unavailable();
    };
    let Some(facts) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|s| wtf16::parse(s).ok())
    else {
        return unavailable();
    };
    let Some(body) = project(&facts, admin).as_ref().and_then(wtf16::stringify) else {
        return unavailable();
    };
    reply::json_text(StatusCode::OK, body, true)
}
