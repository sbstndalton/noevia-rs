#![allow(clippy::indexing_slicing)]
//! Sole native runtime owner of the machine registry and remote measurements.
use crate::{identity, inference_machines as registry, reply, App};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, Request, Response, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use serde_json::{json, Value};
use server_store::{Reader, RustInferenceMachines, StoreError};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[derive(Default)]
pub struct Service {
    inner: Mutex<Runtime>,
    mutations: Mutex<()>,
}
#[derive(Default)]
struct Runtime {
    epoch: u64,
    turns: HashMap<String, String>,
    run: Option<Run>,
    active: Option<String>,
    ended_through: u64,
}
struct Run {
    status: Value,
    cancel: tokio::sync::watch::Sender<bool>,
}
fn unavailable() -> registry::Refusal {
    registry::fail(
        503,
        "machinesUnavailable",
        "Inference machines are unavailable.",
    )
}
fn response(result: Result<Value, registry::Refusal>, status: u16) -> Response<Body> {
    let (status, value) = match result {
        Ok(v) => (status, v),
        Err(e) => (e.0, e.1),
    };
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        registry::encode_json(&value),
        true,
    )
}
fn read_row(r: &Reader<'_>, key: &str) -> Result<Option<String>, StoreError> {
    let raw = r.row(
        "SELECT CAST(value AS TEXT) FROM settings WHERE key=?1",
        [key],
        |row| row.get::<_, String>(0),
    )?;
    if raw.as_deref().is_some_and(|raw| {
        matches!(
            js_json::wtf16::parse(raw),
            Err(js_json::wtf16::Refusal::Unsupported)
        )
    }) {
        // Unsupported valid JSON must never be mistaken for a missing/corrupt row
        // and overwritten by a mutation. Keep the existing bytes intact.
        return Err(StoreError::NotReady(
            "machine registry JSON is unsupported".into(),
        ));
    }
    Ok(raw)
}
fn read_registry(r: &Reader<'_>) -> Result<Value, StoreError> {
    Ok(registry::state(read_row(r, registry::KEY)?.as_deref()))
}
pub async fn read(app: &Arc<App>) -> Result<Value, registry::Refusal> {
    let app = app.clone();
    tokio::task::spawn_blocking(move || app.identity.store_blocking()?.read(read_registry))
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())
}
struct Mutation {
    output: Value,
    audit: Option<Value>,
    changed: Vec<String>,
}
async fn commit(
    app: &Arc<App>,
    body: Value,
    actor: String,
    unresolved: bool,
    recheck: Vec<String>,
    proof: Option<(String, String)>,
) -> Result<(Value, Vec<String>), registry::Refusal> {
    let app = app.clone();
    let id = format!("m_{}", &random_id()[..12]);
    tokio::task::spawn_blocking(move || {
        let _serial = app.machines.mutations.lock().map_err(|_| unavailable())?;
        // Match the embedding owner's lock order: config file before cowork.db.
        let root = app.config.data_dir.as_ref().ok_or_else(unavailable)?;
        let _embedding = server_projects::lock::acquire(&root.join("embedding-config.json"))
            .map_err(|_| unavailable())?;
        let writer = app.writes.writer().map_err(|_| unavailable())?;
        let mutation = writer.write(|tx| -> Result<Result<Mutation, registry::Refusal>, StoreError> {
            let mut state = read_registry(&tx.reader())?;
            if let Some((id, address)) = &proof {
                if !registry::machine(&state, id).is_some_and(|m| m["address"] == *address) {
                    return Ok(Err(unavailable()));
                }
            }
            let (active, local, workers) = match context(&app) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
            let original = state.clone();
            if (body["action"] == "assign" && ["embedding", "ocr", "documents"].contains(&registry::text(&body["service"]).as_str())) || body["action"] == "remove" {
                match validate_derived(&state, &body, &active, local.as_deref(), workers) {
                    Ok(true) => return Ok(Ok(Mutation { output: registry::view_with(&state, &active, local.as_deref(), workers), audit: None, changed: Vec::new() })),
                    Ok(false) => (),
                    Err(error) => return Ok(Err(error)),
                }
            }
            let moved = body["action"] == "update" && body.get("address").is_some()
                && registry::machine(&state, &registry::text(&body["id"])).is_some_and(|machine| {
                    registry::address(&body["address"]).is_ok_and(|address| !address.eq_ignore_ascii_case(&registry::text(&machine["address"])))
                });
            let mut output = match registry::mutate(&mut state, &body, &id, unresolved) {
                Ok(value) => value,
                Err(error) => return Ok(Err(error)),
            };
            if body["action"] == "update" && moved {
                if let Some(assignments) = state["assignments"].as_object_mut() {
                    for (service, assignment) in assignments {
                        if registry::chat(service) && assignment["machine"] == registry::text(&body["id"]) {
                            if let Some(fields) = assignment.as_object_mut() {
                                fields.remove("needsConfirm");
                                if recheck.contains(service) { fields.insert("needsConfirm".into(), true.into()); }
                            }
                        }
                    }
                }
            }
            if let Err(error) = validate_derived(&original, &body, &active, local.as_deref(), workers) {
                return Ok(Err(error));
            }
            tx.set_inference_machines(RustInferenceMachines::owned(), false, &registry::encode_json(&state))?;
            let detail = audit_detail(&original, &state, &body, &output, moved, unresolved, &recheck);
            let view = registry::view_with(&state, &active, local.as_deref(), workers);
            output["machines"] = view["machines"].clone();
            output["services"] = view["services"].clone();
            let changed = if body["action"] == "assign" {
                vec![registry::text(&body["service"])]
            } else if moved {
                state["assignments"].as_object().into_iter().flatten()
                    .filter(|(_, assignment)| assignment["machine"] == registry::text(&body["id"]))
                    .map(|(service, _)| service.clone()).collect()
            } else { Vec::new() };
            Ok(Ok(Mutation { output, audit: Some(detail), changed }))
        }).map_err(|_| unavailable())??;
        // Node saves before its synchronous audit: an audit failure does not undo that save.
        if let Some(detail) = mutation.audit {
            writer.write(|tx| {
                tx.execute("INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)", server_store::rusqlite::params![actor, format!("machines.{}", registry::text(&body["action"])), registry::encode_json(&detail), identity::now_ms()])?;
                Ok(())
            }).map_err(|_| registry::Refusal(500, json!({"error":"The inference machines could not be changed."})))?;
        }
        Ok((mutation.output, mutation.changed))
    }).await.map_err(|_| unavailable())?
}

fn random_id() -> String {
    use rand_core::RngCore;
    let mut b = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut b);
    b.iter().map(|b| format!("{b:02x}")).collect()
}
fn client() -> Result<reqwest::Client, registry::Refusal> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(crate::decision_settings_routes::LocalOnly {
            link_local: false,
        }))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|_| unavailable())
}
async fn fetch(
    client: &reqwest::Client,
    url: &str,
    body: Option<Value>,
    cap: usize,
) -> Result<Value, registry::Refusal> {
    let parsed = url::Url::parse(url).map_err(|_| unavailable())?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(registry::fail(
            503,
            "machineUnreachable",
            "The machine could not be reached.",
        ));
    }
    let origin = parsed.origin().ascii_serialization();
    if crate::decision_settings_routes::parse_service_url(&origin, false).is_none() {
        return Err(registry::fail(
            502,
            "publicAddress",
            "The machine address is not on this network.",
        ));
    }
    let request = match body {
        Some(v) => client
            .post(url)
            .header("Content-Type", "application/json")
            .body(registry::encode_json(&v)),
        None => client.get(url),
    };
    let mut response = request.send().await.map_err(|e| {
        registry::fail(
            503,
            if e.is_timeout() {
                "stepTimeout"
            } else if public_error(&e) {
                "publicAddress"
            } else {
                "machineUnreachable"
            },
            "The machine could not be reached.",
        )
    })?;
    if response.status().is_redirection() {
        return Err(registry::fail(
            503,
            "machineUnreachable",
            "The machine could not be reached.",
        ));
    }
    if !response.status().is_success() {
        return Err(registry::fail(
            502,
            "machineAnswer",
            &format!("HTTP {}", response.status().as_u16()),
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        registry::fail(
            503,
            "machineUnreachable",
            "The machine could not be reached.",
        )
    })? {
        if bytes.len() + chunk.len() > cap {
            return Err(registry::fail(
                502,
                "tooLarge",
                "The machine’s answer was too large.",
            ));
        }
        bytes.extend_from_slice(&chunk)
    }
    registry::parse_json(&String::from_utf8_lossy(&bytes)).ok_or_else(|| {
        registry::fail(
            503,
            "invalidMachineJson",
            "The machine could not be reached.",
        )
    })
}
async fn models_at(address: &str, port: u16, name: &str) -> Result<Vec<String>, registry::Refusal> {
    let c = client()?;
    let answer = tokio::time::timeout(
        Duration::from_secs(5),
        fetch(&c, &format!("{address}:{port}/v1/models"), None, 256 * 1024),
    )
    .await;
    let v = match answer {
        Ok(Ok(v)) => v,
        other => {
            let error = match other {
                Ok(Err(e)) => e,
                _ => registry::fail(502, "machineUnreachable", ""),
            };
            let code = error.1["code"].as_str().unwrap_or_default();
            let message=match code{"publicAddress"=>"Machines must be on this network (a LAN or Tailscale address).".to_string(),"invalidMachineJson"|"tooLarge"=>format!("{name} did not list its models on port {port}."),"machineAnswer"=>format!("{name} answered {} on port {port}; it does not look like an OpenAI-compatible server.",registry::text(&error.1["error"])),_=>format!("{name} did not answer on port {port}.")};
            return Err(registry::fail(
                502,
                if code == "publicAddress" {
                    "publicAddress"
                } else {
                    "machineUnreachable"
                },
                &message,
            ));
        }
    };
    let rows = v["data"].as_array().or_else(|| v["models"].as_array());
    let mut out = Vec::new();
    for row in rows.into_iter().flatten() {
        let id = if row.is_string() {
            row
        } else {
            row.get("id")
                .or_else(|| row.get("model"))
                .or_else(|| row.get("name"))
                .unwrap_or(&Value::Null)
        };
        if let Some(id) = id.as_str() {
            if !crate::decision_settings_routes::js_trim(id).is_empty()
                && js_json::wtf16::len(id) <= 200
                && !id.chars().any(|c| c < ' ' || c == '\u{7f}')
                && !out.iter().any(|s| s == id)
            {
                out.push(id.to_string());
                if out.len() == 200 {
                    break;
                }
            }
        }
    }
    Ok(out)
}
async fn validate_address(address: &str) -> Result<bool, registry::Refusal> {
    let Some(parsed) = crate::decision_settings_routes::parse_service_url(address, false) else {
        return Err(registry::fail(
            400,
            "publicAddress",
            "Machines must be on this network.",
        ));
    };
    let Some(name) = parsed.name else {
        return Ok(false);
    };
    let lookup = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((name.as_str(), 0)),
    )
    .await;
    match lookup {
        Ok(Ok(addresses)) => {
            let addresses: Vec<_> = addresses.collect();
            if addresses.is_empty() {
                return Ok(true);
            }
            if let Some(bad) = addresses
                .iter()
                .find(|a| !crate::decision_settings_routes::is_local(a.ip(), false))
            {
                Err(registry::fail(400,"publicAddress",&format!("Machines must be on this network (a LAN or Tailscale address): {name} is not on this network ({} is {}).",bad.ip(),registry::network_reason(Some(bad.ip())))))
            } else {
                Ok(false)
            }
        }
        _ => Ok(true),
    }
}
async fn mutate(
    app: &Arc<App>,
    mut body: Value,
    actor: String,
) -> Result<(Value, Vec<String>), registry::Refusal> {
    let action = registry::text(&body["action"]);
    let mut unresolved = false;
    let mut recheck = Vec::new();
    let mut proof = None;
    if action == "add" {
        registry::name(&body["name"])?;
        unresolved = validate_address(&registry::address(&body["address"])?).await?;
    }
    if action == "update" {
        let state = read(app).await?;
        let id = registry::coerce(&body["id"])?;
        let old = registry::machine(&state, &id).ok_or_else(|| {
            registry::fail(404, "unknownMachine", "That machine is not on the list.")
        })?;
        let captured_name = match body.get("name") {
            Some(value) => Value::from(registry::name(value)?),
            None => old["name"].clone(),
        };
        unresolved = old["unresolved"] == true;
        if body.get("address").is_some() {
            let a = registry::address(&body["address"])?;
            if !a.eq_ignore_ascii_case(&registry::text(&old["address"])) {
                // Node captures these fields before DNS/model awaits, then rechecks existence
                // and duplicates against the current registry before committing them.
                body["name"] = captured_name;
                body["address"] = Value::from(a.clone());
                unresolved = validate_address(&a).await?;
                let mut probes = tokio::task::JoinSet::new();
                for service in registry::SERVICES
                    .iter()
                    .filter(|service| registry::chat(service))
                {
                    let assignment = &state["assignments"][service];
                    if assignment["machine"] == id {
                        if let Ok(port) = registry::port(&assignment["port"]) {
                            let address = a.clone();
                            let service = service.to_string();
                            let model = assignment["model"].clone();
                            probes.spawn(async move {
                                let missing = models_at(&address, port, &address)
                                    .await
                                    .is_ok_and(|models| !models.iter().any(|m| model == *m));
                                (service, missing)
                            });
                        }
                    }
                }
                while let Some(result) = probes.join_next().await {
                    if let Ok((service, true)) = result {
                        recheck.push(service);
                    }
                }
            }
        }
    }
    if action == "assign"
        && registry::chat(&registry::text(&body["service"]))
        && registry::truthy(&body["machine"])
        && body["machine"] != "local"
    {
        let model = registry::clean_model(&body["model"]).ok_or_else(|| {
            registry::fail(
                400,
                "invalidModel",
                "Choose the model this machine serves for the role.",
            )
        })?;
        let state = read(app).await?;
        let m = registry::machine(&state, &registry::text(&body["machine"])).ok_or_else(|| {
            registry::fail(404, "unknownMachine", "That machine is not on the list.")
        })?;
        proof = Some((registry::text(&m["id"]), registry::text(&m["address"])));
        let served = models_at(
            m["address"].as_str().unwrap_or_default(),
            registry::port(&body["port"])?,
            &registry::text(&m["name"]),
        )
        .await?;
        if !served.contains(&model) {
            return Err(registry::Refusal(
                400,
                json!({"error":format!("That machine does not serve {model} on port {}.",registry::text(&body["port"])),"code":"unknownModel","models":served}),
            ));
        }
    }
    commit(app, body, actor, unresolved, recheck, proof).await
}
impl Service {
    fn status(&self) -> Result<Value, registry::Refusal> {
        let r = self.inner.lock().map_err(|_| unavailable())?;
        Ok(r.run
            .as_ref()
            .map(|r| r.status.clone())
            .unwrap_or(Value::Null))
    }
    fn stop(&self, state: &str, code: &str) -> Result<Value, registry::Refusal> {
        let mut r = self.inner.lock().map_err(|_| unavailable())?;
        if let Some(run) = r.run.as_mut() {
            if run.status["state"] == "running" {
                run.status["state"] = state.into();
                run.status["code"] = code.into();
                let _ = run.cancel.send(true);
            }
        }
        Ok(r.run
            .as_ref()
            .map(|r| r.status.clone())
            .unwrap_or(Value::Null))
    }
}
async fn operation(app: &Arc<App>, body: Value) -> Result<Value, registry::Refusal> {
    let op = body["op"].as_str().unwrap_or_default();
    if op == "session" {
        let mut r = app.machines.inner.lock().map_err(|_| unavailable())?;
        if let Some(run) = r.run.take() {
            let _ = run.cancel.send(true);
        }
        // Do not reuse a prior front's numeric session after a native restart.
        // Keep the value exact in the synchronous JavaScript client as well.
        if r.epoch == 0 {
            use rand_core::RngCore;
            r.epoch = (rand_core::OsRng.next_u64() & ((1_u64 << 51) - 1)) + 1;
        } else {
            r.epoch += 1;
        }
        r.turns.clear();
        r.ended_through = 0;
        return Ok(json!({"epoch":r.epoch}));
    }
    if op == "turnBegin" || op == "turnEnd" {
        let sequence = body["sequence"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(unavailable)?;
        let token = registry::text(&body["token"]);
        let mut grant = TurnGuard {
            service: app.machines.clone(),
            epoch: body["epoch"].as_u64().unwrap_or(0),
            sequence,
            token: token.clone(),
            armed: false,
        };
        let wait = {
            let mut r = app.machines.inner.lock().map_err(|_| unavailable())?;
            if r.epoch == 0 || body["epoch"].as_u64() != Some(r.epoch) {
                if op == "turnEnd" {
                    return Ok(json!({"ok":true}));
                }
                return Err(unavailable());
            }
            if op == "turnEnd" {
                r.ended_through = r.ended_through.max(sequence);
                r.turns.remove(&token);
                None
            } else {
                if sequence <= r.ended_through && !r.turns.contains_key(&token) {
                    return Err(unavailable());
                }
                let machine = registry::text(&body["machine"]);
                grant.armed = r.turns.insert(token, machine.clone()).is_none();
                let wait = if r
                    .run
                    .as_ref()
                    .is_none_or(|run| run.status["machine"]["id"] == machine)
                {
                    r.active.clone()
                } else {
                    None
                };
                if let Some(run) = r.run.as_mut() {
                    if run.status["state"] == "running" && run.status["machine"]["id"] == machine {
                        run.status["state"] = "interrupted".into();
                        run.status["code"] = "chatTurn".into();
                        let _ = run.cancel.send(true);
                    }
                }
                wait
            }
        };
        if let Some(id) = wait {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let active = {
                        let r = app.machines.inner.lock().map_err(|_| unavailable())?;
                        r.active.as_deref() == Some(id.as_str())
                    };
                    if !active {
                        return Ok::<(), registry::Refusal>(());
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .map_err(|_| unavailable())??;
        }
        grant.armed = false;
        return Ok(json!({"ok":true}));
    }

    if op == "runtimeSource" {
        let app = app.clone();
        return tokio::task::spawn_blocking(move||app.identity.store_blocking()?.read(|r|{use sha2::{Digest,Sha256};let row=r.row("SELECT typeof(value), hex(CAST(value AS BLOB)) FROM settings WHERE key='inference:machines'",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?;let (kind,bytes)=row.unwrap_or_else(||("missing".into(),String::new()));let value=format!("inference:machines\0{kind}\0{bytes}");Ok(Value::from(format!("{:x}",Sha256::digest(value.as_bytes()))))})).await.map_err(|_|unavailable())?.map_err(|_|unavailable());
    }
    if op == "tuneResult" {
        let app = app.clone();
        let machine = body["machineId"].clone();
        let model = body["model"].clone();
        return tokio::task::spawn_blocking(move || {
            app.identity.store_blocking()?.read(|r| {
                let state = read_registry(r)?;
                if registry::machine(&state, &registry::text(&machine)).is_none() {
                    return Ok(Value::Null);
                }
                let results = read_row(r, "inference:machineTune")?
                    .and_then(|s| registry::parse_json(&s))
                    .unwrap_or(Value::Null);
                Ok(results["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|r| r["machineId"] == machine && r["model"] == model)
                    .cloned()
                    .unwrap_or(Value::Null))
            })
        })
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable());
    }
    if op == "tuneStatus" {
        return app.machines.status();
    }
    if op == "tuneAbort" {
        return abort(app, &registry::text(&body["actor"])).await;
    }
    if op == "tune" {
        return start(app, &body).await;
    }
    if op == "mutate" {
        return mutate(app, body["body"].clone(), registry::text(&body["actor"]))
            .await
            .map(|(out, _)| out);
    }
    let s = read(app).await?;
    Ok(match op {
        "state" => s,
        "view" => {
            let (a, l, w) = context(app)?;
            registry::view_with(&s, &a, l.as_deref(), w)
        }
        "embeddingMachine" => {
            let (a, l, _) = context(app)?;
            registry::embedding(&s, &a, l.as_deref())
        }
        "machine" => registry::machine(&s, &registry::text(&body["id"]))
            .cloned()
            .unwrap_or(Value::Null),
        "chatTarget" => registry::target(&s, &registry::text(&body["role"])),
        "assignmentOf" => {
            if body["service"] == "embedding" {
                let (a, l, _) = context(app)?;
                let e = registry::embedding(&s, &a, l.as_deref());
                let mut out = json!({"machine":e["machine"]});
                if e["machine"] != "local" && !e["port"].is_null() {
                    out["port"] = e["port"].clone()
                }
                out
            } else {
                registry::assignment(&s, &registry::text(&body["service"]))
            }
        }
        "urlFor" => {
            if registry::chat(&registry::text(&body["service"])) || body["service"] == "embedding" {
                return Ok(Value::Null);
            }
            let a = registry::assignment(&s, &registry::text(&body["service"]));
            registry::machine(&s, &registry::text(&a["machine"]))
                .map(|m| Value::from(format!("{}:{}", registry::text(&m["address"]), a["port"])))
                .unwrap_or(Value::Null)
        }
        "rerankTarget" => {
            let a = registry::assignment(&s, "reranker");
            registry::machine(&s,&registry::text(&a["machine"])).map(|m|json!({"url":format!("{}:{}",registry::text(&m["address"]),a["port"]),"protocol":a["protocol"]})).unwrap_or(Value::Null)
        }
        "rerankProtocol" => s["assignments"]["reranker"]["protocol"].clone(),
        _ => {
            return Err(registry::fail(
                400,
                "invalidOperation",
                "Invalid machines operation.",
            ))
        }
    })
}
pub fn router(store: crate::project_store::Store, app: Option<Arc<App>>) -> Router {
    Router::new().route(
        "/v1/inference-machines",
        post(move |headers: HeaderMap, raw: Bytes| {
            let store = store.clone();
            let app = app.clone();
            async move {
                if !crate::project_store::authorised(&store, &headers) {
                    return reply::error(StatusCode::UNAUTHORIZED, "unauthorised", true);
                }
                let Some(app) = app else {
                    return response(Err(unavailable()), 503);
                };
                let Some(body) = registry::parse_json(&String::from_utf8_lossy(&raw)) else {
                    return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true);
                };
                response(operation(&app, body).await, 200)
            }
        }),
    )
}
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != registry::PATH {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = app.identity.clone();
    let id = match tokio::task::spawn_blocking(move || {
        layer.gate_blocking(&creds, registry::PATH, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => return e.into_response(),
        _ => return identity::Rejection::Unavailable.into_response(),
    };
    if id.user.role != server_auth::Role::Admin {
        return reply::error(StatusCode::FORBIDDEN, "Administrator required", true);
    }
    if parts.method == Method::GET {
        let query: url::form_urlencoded::Parse<'_> =
            url::form_urlencoded::parse(parts.uri.query().unwrap_or_default().as_bytes());
        let mut first = HashMap::new();
        for (key, value) in query.into_owned() {
            first.entry(key).or_insert(value);
        }
        let query = first;
        if let Some(machine) = query.get("models") {
            let result=async{let s=read(app).await?;let m=registry::machine(&s,machine).ok_or_else(||registry::fail(404,"unknownMachine","That machine is not on the list."))?;Ok(json!({"models":models_at(&registry::text(&m["address"]),registry::port(&Value::from(query.get("port").cloned().unwrap_or_default()))?,&registry::text(&m["name"])).await?}))}.await;
            return response(result, 200);
        }
        if query.get("tune").is_some_and(|s| s == "1") {
            return response(app.machines.status().map(|v| json!({"tune":v})), 200);
        }
        let result = async {
            let s = read(app).await?;
            let (a, l, w) = context(app)?;
            public_view(
                app,
                registry::view_with(&s, &a, l.as_deref(), w),
                query.get("health").is_some_and(|v| v == "1"),
                Vec::new(),
            )
            .await
        }
        .await;
        return response(result, 200);
    }
    if parts.method != Method::POST {
        return reply::error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed", true);
    }
    let bytes = match tokio::time::timeout(
        Duration::from_secs(10),
        axum::body::to_bytes(body, 1024 * 1024),
    )
    .await
    {
        Ok(Ok(b)) => b,
        _ => return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true),
    };
    let body = if bytes.is_empty() {
        json!({})
    } else {
        match registry::parse_json(&String::from_utf8_lossy(&bytes)) {
            Some(v) => v,
            None => return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true),
        }
    };
    if !body.is_object() {
        return reply::error(StatusCode::BAD_REQUEST, "A JSON object is required", true);
    }
    let mut body = body;
    let action = match registry::coerce(&body["action"]) {
        Ok(s) => s,
        Err(e) => return response(Err(e), 500),
    };
    body["action"] = action.into();
    match body["action"].as_str() {
        Some("tune") => {
            let mut body = body;
            body["actor"] = id.user.id.into();
            response(start(app, &body).await.map(|v| json!({"tune":v})), 202)
        }
        Some("tuneAbort") => response(
            abort(app, &id.user.id).await.map(|v| json!({"tune":v})),
            200,
        ),
        _ => {
            let status = if body["action"] == "add" { 201 } else { 200 };
            let result = match mutate(app, body, id.user.id).await {
                Ok((v, changed)) => public_view(app, v, false, changed).await,
                Err(e) => Err(e),
            };
            response(result, status)
        }
    }
}
fn stamp() -> String {
    server_export::js_date::iso(identity::now_ms() as f64)
}
fn steps(context: Option<u64>) -> Vec<String> {
    let mut steps = vec![
        "props".into(),
        "decode:list".into(),
        "decode:prose".into(),
        "decode:code".into(),
    ];
    let sizes: Vec<_> = [1024, 4096, 16384]
        .into_iter()
        .filter(|n| context.is_none_or(|c| *n <= c * 2 / 3))
        .collect();
    for n in if sizes.is_empty() { vec![1024] } else { sizes } {
        steps.push(format!("prefill:{n}"))
    }
    steps
}
fn numeric(v: &Value) -> Option<f64> {
    v.as_f64().filter(|v| v.is_finite() && *v >= 0.0)
}
fn rounded(v: Option<f64>, digits: i32) -> Value {
    v.and_then(|v| {
        serde_json::Number::from_f64((v * 10_f64.powi(digits)).round() / 10_f64.powi(digits))
    })
    .map(Value::Number)
    .unwrap_or(Value::Null)
}
pub async fn start(app: &Arc<App>, body: &Value) -> Result<Value, registry::Refusal> {
    let service = registry::text(&body["service"]);
    let role = match service.as_str() {
        "chatFast" => "fast",
        "chatSmart" => "smart",
        "chatCode" => "code",
        "chatCreative" => "creative",
        "vision" => "vision",
        _ => {
            return Err(registry::fail(
                400,
                "invalidService",
                "Only a chat role or Vision on an Inference machine can be measured.",
            ))
        }
    };
    if body["confirm"] != true {
        return Err(registry::fail(
            400,
            "confirmRequired",
            "Confirm the measurement: it sends a few short requests to that machine.",
        ));
    }
    let s = read(app).await?;
    let target = registry::target(&s, role);
    if target.is_null() {
        return Err(registry::fail(
            404,
            "notOnMachine",
            "That role runs on this machine; only a role on an Inference machine is measured here.",
        ));
    }
    let machine = registry::text(&target["machine"]["id"]);
    let check = |r: &Runtime| -> Result<(), registry::Refusal> {
        if r.epoch == 0 {
            return Err(unavailable());
        }
        if r.active.is_some() {
            return Err(registry::fail(
                409,
                "tuneRunning",
                "A measurement is already running. Wait for it or stop it first.",
            ));
        }
        if r.turns.values().any(|m| m == &machine) {
            return Err(registry::fail(
                409,
                "machineBusy",
                &format!(
                    "{} is answering a chat now. Try again when it is done.",
                    registry::text(&target["machine"]["name"])
                ),
            ));
        }
        Ok(())
    };
    {
        let r = app.machines.inner.lock().map_err(|_| unavailable())?;
        check(&r)?;
    }
    let Some((200, ready)) = app
        .config
        .research
        .node_call_text(
            "/internal/inference-machines/ready",
            &registry::encode_json(&json!({"target":target})),
            Duration::from_secs(15),
        )
        .await
    else {
        return Err(unavailable());
    };
    let ready = registry::parse_json(&String::from_utf8_lossy(&ready)).ok_or_else(unavailable)?;
    if ready["ok"] != true {
        return Err(registry::fail(
            503,
            ready["code"].as_str().unwrap_or("machineUnreachable"),
            ready["error"]
                .as_str()
                .unwrap_or("The machine could not be reached."),
        ));
    }
    let id = server_projects::source_jobs::new_id();
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    let plan = steps(None);
    let status = json!({"id":id,"service":body["service"],"machine":{"id":target["machine"]["id"],"name":target["machine"]["name"]},"model":target["model"],"state":"running","step":"props","stepIndex":1,"steps":plan.len(),"startedAt":stamp()});
    {
        let mut r = app.machines.inner.lock().map_err(|_| unavailable())?;
        check(&r)?;
        r.active = Some(id.clone());
        r.run = Some(Run {
            status: status.clone(),
            cancel,
        });
    }
    let app = app.clone();
    let guard = RunGuard {
        service: app.machines.clone(),
        id: id.clone(),
    };
    audit_event(
        &app,
        "machines.tune.start",
        &registry::text(&body["actor"]),
        json!({"machine":target["machine"]["id"],"model":target["model"]}),
    )
    .await;
    tokio::spawn(async move {
        let _guard = guard;
        execute(app, id, target, receiver).await;
    });
    Ok(status)
}
async fn script(app: &Arc<App>, id: &str, target: &Value) -> Result<Value, registry::Refusal> {
    let client = client()?;
    let base = registry::text(&target["baseUrl"]);
    let model = target["model"].clone();
    if registry::text(&model)
        .chars()
        .any(|c| js_json::wtf16::lone_unit(c).is_some())
    {
        return Err(registry::fail(
            503,
            "machineUnreachable",
            "The machine could not be reached.",
        ));
    }
    let enter = |name: &str, plan: &[String]| -> Result<(), registry::Refusal> {
        let mut r = app.machines.inner.lock().map_err(|_| unavailable())?;
        let run = r
            .run
            .as_mut()
            .filter(|r| r.status["id"] == id && r.status["state"] == "running")
            .ok_or_else(unavailable)?;
        run.status["step"] = name.into();
        run.status["stepIndex"] = json!(plan
            .iter()
            .position(|s| s == name)
            .map(|n| n + 1)
            .unwrap_or(0));
        run.status["steps"] = json!(plan.len());
        Ok(())
    };
    let mut plan = steps(None);
    enter("props", &plan)?;
    let props = fetch(
        &client,
        &format!(
            "{base}/props?model={}",
            url::form_urlencoded::byte_serialize(registry::text(&model).as_bytes())
                .collect::<String>()
        ),
        None,
        1024 * 1024,
    )
    .await;
    let context = match props {
        Ok(p) => Some(p),
        Err(e) if e.1["code"] == "machineAnswer" => None,
        Err(e) => return Err(e),
    }
    .and_then(|p| {
        let n = p["default_generation_settings"]["n_ctx"]
            .as_f64()
            .or_else(|| {
                p["default_generation_settings"]["n_ctx"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
            });
        n.filter(|n| *n > 0.0 && n.fract() == 0.0).map(|n| n as u64)
    });
    plan = steps(context);
    let workloads=[("list",160,"List the whole numbers from 1 to 60, separated by commas, and nothing else."),("prose",160,"Write one paragraph explaining why community gardens matter to a neighbourhood."),("code",200,"Write a JavaScript function median(values) that returns the median of an array without modifying it. Code only.")];
    let mut decode = Vec::new();
    let mut prefill = Vec::new();
    let mut drafted = 0.0;
    let mut accepted = 0.0;
    for (work, max, prompt) in workloads {
        enter(&format!("decode:{work}"), &plan)?;
        let began = Instant::now();
        let answer=fetch(&client,&format!("{base}/v1/chat/completions"),Some(json!({"model":model,"messages":[{"role":"user","content":prompt}],"max_tokens":max,"temperature":0,"stream":false,"cache_prompt":false})),256*1024).await?;
        let tm = &answer["timings"];
        let server = numeric(&tm["predicted_per_second"]);
        let tokens =
            numeric(&answer["usage"]["completion_tokens"]).or_else(|| numeric(&tm["predicted_n"]));
        let rate = server.or_else(|| {
            tokens
                .filter(|n| *n > 0.0)
                .map(|n| n * 1000.0 / (began.elapsed().as_millis().max(1) as f64))
        });
        decode.push(json!({"id":work,"tokensPerSecond":rounded(rate,1),"source":if server.is_some(){"server"}else{"wall"}}));
        if let Some(n) = numeric(&tm["draft_n"]).filter(|n| *n > 0.0) {
            drafted += n;
            accepted += numeric(&tm["draft_n_accepted"]).unwrap_or(0.0);
        }
    }
    for step in plan.iter().filter(|s| s.starts_with("prefill:")) {
        enter(step, &plan)?;
        let size = step[8..].parse::<usize>().unwrap_or(1024);
        let prompt=format!("Measurement {id}. {}\nReply with OK.","The garden committee reviewed irrigation, seed orders, volunteer rotas and pump maintenance. ".repeat(size.div_ceil(16)));
        let began = Instant::now();
        let answer=fetch(&client,&format!("{base}/v1/chat/completions"),Some(json!({"model":model,"messages":[{"role":"user","content":prompt}],"max_tokens":1,"temperature":0,"stream":false,"cache_prompt":false})),256*1024).await?;
        let tm = &answer["timings"];
        let server = numeric(&tm["prompt_per_second"]);
        let tokens =
            numeric(&tm["prompt_n"]).or_else(|| numeric(&answer["usage"]["prompt_tokens"]));
        let rate = server.or_else(|| {
            tokens
                .filter(|n| *n > 0.0)
                .map(|n| n * 1000.0 / (began.elapsed().as_millis().max(1) as f64))
        });
        prefill.push(json!({"tokens":tokens.unwrap_or(size as f64),"tokensPerSecond":rounded(rate,1),"source":if server.is_some(){"server"}else{"wall"}}));
    }
    let speculative = if drafted > 0.0 {
        json!({"measured":true,"drafted":drafted,"accepted":accepted,"acceptance":rounded(Some(accepted/drafted),3)})
    } else {
        json!({"measured":false,"drafted":0,"accepted":0,"acceptance":null})
    };
    let values: Vec<_> = decode
        .iter()
        .filter_map(|v| v["tokensPerSecond"].as_f64())
        .filter(|v| *v > 0.0)
        .collect();
    let mean = if values.is_empty() {
        None
    } else {
        Some((values.iter().map(|v| v.ln()).sum::<f64>() / (values.len() as f64)).exp())
    };
    let mut recommendations = Vec::new();
    if let (Some(context), Some(rate)) = (
        context,
        prefill
            .iter()
            .rev()
            .filter_map(|p| p["tokensPerSecond"].as_f64())
            .find(|n| *n > 0.0),
    ) {
        let seconds = (context as f64) / rate;
        if seconds > 120.0 {
            let value = ((rate * 120.0 / 1024.0).floor() * 1024.0).max(2048.0) as u64;
            if value < context {
                recommendations.push(json!({"key":"contextTokens","value":value,"flag":format!("--ctx-size {value}"),"reason":"slowFill","fillSeconds":seconds.round()}))
            }
        }
    }
    if speculative["measured"] == true
        && speculative["acceptance"].as_f64().is_some_and(|n| n < 0.5)
    {
        recommendations.push(json!({"key":"speculative","value":"off","flag":"--spec-type none","reason":"lowAcceptance","acceptance":speculative["acceptance"]}))
    }
    Ok(
        json!({"machineId":target["machine"]["id"],"model":model,"measuredAt":stamp(),"contextTokens":context,"decode":{"tokensPerSecond":rounded(mean,1),"workloads":decode},"prefill":prefill,"speculative":speculative,"recommendations":recommendations}),
    )
}
async fn execute(
    app: Arc<App>,
    id: String,
    target: Value,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    let result = tokio::select! {biased;_ = cancel.changed()=>None,r=tokio::time::timeout(Duration::from_secs(8*60),script(&app,&id,&target))=>Some(match r{Ok(r)=>r,Err(_)=>Err(registry::fail(503,"runTimeout","Measurement timed out."))})};
    // Publication and cancellation share the same mutex. The transaction runs on the blocking pool;
    // its synchronous closure fences against an abort or a newer run before writing results.
    let app2 = app.clone();
    let id2 = id.clone();
    let _ = tokio::task::spawn_blocking(move || -> Result<(), StoreError> {
        let mut runtime = app2
            .machines
            .inner
            .lock()
            .map_err(|_| StoreError::Poisoned)?;
        let Some(run) = runtime.run.as_mut().filter(|run| run.status["id"] == id2) else {
            return Ok(());
        };
        if run.status["state"] == "running" {
            match result {
                Some(Ok(value)) => {
                    let saved = app2.writes.writer()?.write(|tx| {
                        let old = read_row(&tx.reader(), "inference:machineTune")?
                            .and_then(|s| registry::parse_json(&s))
                            .unwrap_or(Value::Null);
                        let mut rows: Vec<_> = old["results"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|r| r["machineId"].is_string() && r["model"].is_string())
                            .filter(|r| {
                                r["machineId"] != value["machineId"] || r["model"] != value["model"]
                            })
                            .cloned()
                            .collect();
                        rows.push(value);
                        if rows.len() > 64 {
                            rows.drain(..rows.len() - 64);
                        }
                        tx.set_inference_machines(
                            RustInferenceMachines::owned(),
                            true,
                            &registry::encode_json(&json!({"results":rows})),
                        )?;
                        Ok(())
                    });
                    run.status["state"] = if saved.is_ok() { "done" } else { "failed" }.into();
                }
                Some(Err(e)) => {
                    run.status["state"] = "failed".into();
                    run.status["code"] = if [
                        "stepTimeout",
                        "publicAddress",
                        "machineAnswer",
                        "runTimeout",
                    ]
                    .contains(&e.1["code"].as_str().unwrap_or_default())
                    {
                        e.1["code"].clone()
                    } else {
                        "machineUnreachable".into()
                    };
                }
                None => {
                    run.status["state"] = "aborted".into();
                    run.status["code"] = "aborted".into();
                }
            }
        }
        run.status["finishedAt"] = stamp().into();
        Ok(())
    })
    .await;
}
fn context(app: &Arc<App>) -> Result<(Value, Option<String>, [bool; 2]), registry::Refusal> {
    let seed = embedding_worker::runtime::seed(|key| std::env::var(key).ok())
        .map_err(|_| unavailable())?;
    let root = app.config.data_dir.as_ref().ok_or_else(unavailable)?;
    let config = embedding_worker::runtime::load_config(root, &seed).map_err(|_| unavailable())?;
    let active = json!({"baseUrl":config.active.base_url,"model":config.active.model,"profile":config.active.profile});
    let local = ["EMBEDDING_SERVICE_URL", "EMBEDDING_BASE_URL"]
        .iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .map(|v| v.trim().trim_end_matches('/').to_string())
                .filter(|v| !v.is_empty())
        });
    let workers = ["OCR_BASE_URL", "DOCLING_BASE_URL"]
        .map(|key| std::env::var(key).is_ok_and(|v| !v.trim().is_empty()));
    Ok((active, local, workers))
}

fn validate_derived(
    s: &Value,
    body: &Value,
    active: &Value,
    local: Option<&str>,
    workers: [bool; 2],
) -> Result<bool, registry::Refusal> {
    let action = body["action"].as_str().unwrap_or_default();
    let e = registry::embedding(s, active, local);
    if action == "update" && body.get("address").is_some() {
        let id = registry::text(&body["id"]);
        if let Some(m) = registry::machine(s, &id) {
            if !registry::address(&body["address"])?
                .eq_ignore_ascii_case(&registry::text(&m["address"]))
                && e["machine"] == id
            {
                return Err(registry::fail(409,"embeddingOnMachine","Embeddings run on this machine. Move them to another machine first, then change its address."));
            }
        }
    }
    if action == "remove" && e["machine"] == registry::text(&body["id"]) {
        let view = registry::view_with(s, active, local, workers);
        let using = view["machines"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["id"] == registry::text(&body["id"]))
            .map(|m| m["services"].clone())
            .unwrap_or_else(|| json!(["embedding"]));
        return Err(registry::Refusal(
            409,
            json!({"error":"Move the services on this machine to another machine before removing it.","code":"inUse","services":using}),
        ));
    }
    if action == "assign" {
        let service = registry::text(&body["service"]);
        let machine = registry::text(&body["machine"]);
        if !registry::SERVICES.contains(&service.as_str()) {
            return Ok(false);
        }
        let selected = registry::machine(s, &machine);
        if machine != "local" && selected.is_none() {
            return Ok(false);
        }
        if machine != "local"
            && ["ocr", "documents"].contains(&service.as_str())
            && !workers[usize::from(service == "documents")]
        {
            return Err(registry::fail(409,"workerNotEnabled","Turn this service on in the deployment first; assigning a machine does not switch it on."));
        }
        if service == "embedding" && active["model"].is_string() {
            let port = if machine != "local" {
                Some(registry::port(&body["port"])?)
            } else {
                None
            };
            if e["machine"] == machine
                && (machine == "local" || e["port"].as_u64() == port.map(u64::from))
            {
                return Ok(true);
            }
            let base = if machine == "local" {
                local.map(str::to_string)
            } else {
                selected
                    .map(|m| format!("{}:{}/v1", registry::text(&m["address"]), port.unwrap_or(0)))
            };
            let local_registered = base
                .as_deref()
                .and_then(|s| url::Url::parse(s).ok())
                .is_some_and(|u| {
                    s["machines"].as_array().into_iter().flatten().any(|m| {
                        url::Url::parse(&registry::text(&m["address"]))
                            .is_ok_and(|m| m.scheme() == u.scheme() && m.host_str() == u.host_str())
                    })
                });
            if machine == "local" && (base.is_none() || local_registered) {
                return Err(registry::fail(409,"noLocalEmbedding","This deployment names no embeddings server on this machine, so embeddings cannot move back here from Settings."));
            }
            return Err(registry::Refusal(
                409,
                json!({"error":"Moving embeddings re-indexes projects and the Diary first.","code":"reindexRequired","target":{"baseUrl":base,"model":active["model"],"profile":active["profile"]}}),
            ));
        }
    }
    Ok(false)
}

async fn public_view(
    app: &Arc<App>,
    view: Value,
    health: bool,
    changed: Vec<String>,
) -> Result<Value, registry::Refusal> {
    let Some((200, bytes)) = app
        .config
        .research
        .node_call_text(
            "/internal/inference-machines/probes",
            &registry::encode_json(&json!({"view":view,"health":health,"changed":changed})),
            Duration::from_secs(45),
        )
        .await
    else {
        return Err(unavailable());
    };
    if bytes.len() > 2 * 1024 * 1024 {
        return Err(unavailable());
    }
    let result = registry::parse_json(&String::from_utf8_lossy(&bytes)).ok_or_else(unavailable)?;
    if !result["machines"].is_array() || !result["services"].is_array() {
        return Err(unavailable());
    }
    Ok(result)
}

struct RunGuard {
    service: Arc<Service>,
    id: String,
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        if let Ok(mut r) = self.service.inner.lock() {
            if r.active.as_deref() == Some(self.id.as_str()) {
                r.active = None;
            }
            if let Some(run) = r.run.as_mut().filter(|r| r.status["id"] == self.id) {
                if run.status["state"] == "running" {
                    run.status["state"] = "failed".into();
                    run.status["code"] = "machineUnreachable".into();
                    let _ = run.cancel.send(true);
                }
                if run.status.get("finishedAt").is_none() {
                    run.status["finishedAt"] = stamp().into();
                }
            }
        }
    }
}

async fn audit_event(app: &Arc<App>, action: &str, actor: &str, detail: Value) {
    let app = app.clone();
    let action = action.to_string();
    let actor = actor.to_string();
    let _=tokio::task::spawn_blocking(move||app.writes.writer()?.write(|tx|{tx.execute("INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",server_store::rusqlite::params![actor,action,registry::encode_json(&detail),identity::now_ms()])?;Ok(())})).await;
}
async fn abort(app: &Arc<App>, actor: &str) -> Result<Value, registry::Refusal> {
    let before = app.machines.status()?;
    let status = app.machines.stop("aborted", "aborted")?;
    if before["state"] == "running" {
        audit_event(
            app,
            "machines.tune.abort",
            actor,
            json!({"machine":before["machine"]["id"],"model":before["model"]}),
        )
        .await;
    }
    Ok(status)
}
fn audit_detail(
    before: &Value,
    after: &Value,
    body: &Value,
    out: &Value,
    moved: bool,
    unresolved: bool,
    recheck: &[String],
) -> Value {
    match body["action"].as_str() {
        Some("add") => {
            json!({"id":out["machine"]["id"],"name":out["machine"]["name"],"address":out["machine"]["address"],"unresolved":unresolved})
        }
        Some("update") => {
            let mut value = json!({"id":out["machine"]["id"],"name":out["machine"]["name"],"address":out["machine"]["address"],"moved":moved,"unresolved":unresolved});
            if !recheck.is_empty() {
                value["needsConfirm"] = json!(recheck)
            }
            value
        }
        Some("remove") => {
            let old = registry::machine(before, &registry::text(&body["id"]))
                .cloned()
                .unwrap_or(Value::Null);
            json!({"id":old["id"],"name":old["name"]})
        }
        Some("assign") => {
            let service = registry::text(&body["service"]);
            let a = registry::assignment(after, &service);
            let old = registry::assignment(before, &service);
            let mut value = json!({"service":service,"machine":a["machine"],"from":old["machine"]});
            for key in ["port", "protocol", "model"] {
                if let Some(v) = a.get(key) {
                    value[key] = v.clone()
                }
            }
            value
        }
        _ => Value::Null,
    }
}

fn public_error(error: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = source {
        if error.to_string().contains("publicAddress")
            || error
                .to_string()
                .contains("does not resolve only to addresses on this network")
        {
            return true;
        }
        source = error.source();
    }
    false
}

struct TurnGuard {
    service: Arc<Service>,
    epoch: u64,
    sequence: u64,
    token: String,
    armed: bool,
}
impl Drop for TurnGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(mut runtime) = self.service.inner.lock() {
                if runtime.epoch == self.epoch {
                    runtime.turns.remove(&self.token);
                    runtime.ended_through = runtime.ended_through.max(self.sequence);
                }
            }
        }
    }
}
#[cfg(test)]
mod cancellation_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[tokio::test]
    async fn a_run_reservation_is_released_when_its_future_is_aborted_before_or_after_poll() {
        for poll in [false, true] {
            let service = Arc::new(Service::default());
            let (cancel, _) = tokio::sync::watch::channel(false);
            {
                let mut r = service.inner.lock().unwrap();
                r.active = Some("run".into());
                r.run = Some(Run {
                    status: json!({"id":"run","state":"running"}),
                    cancel,
                });
            }
            let guard = RunGuard {
                service: service.clone(),
                id: "run".into(),
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let _guard = guard;
                let _ = tx.send(());
                std::future::pending::<()>().await;
            });
            if poll {
                rx.await.unwrap();
            }
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(service.inner.lock().unwrap().active.is_none());
            assert_eq!(service.status().unwrap()["state"], "failed");
        }
    }
    #[tokio::test]
    async fn an_unacknowledged_chat_reservation_is_released_and_cannot_be_granted_late() {
        for poll in [false, true] {
            let service = Arc::new(Service::default());
            {
                let mut r = service.inner.lock().unwrap();
                r.epoch = 1;
                r.turns.insert("turn".into(), "machine".into());
            }
            let guard = TurnGuard {
                service: service.clone(),
                epoch: 1,
                sequence: 7,
                token: "turn".into(),
                armed: true,
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let _guard = guard;
                let _ = tx.send(());
                std::future::pending::<()>().await;
            });
            if poll {
                rx.await.unwrap();
            }
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            let r = service.inner.lock().unwrap();
            assert!(r.turns.is_empty());
            assert_eq!(r.ended_through, 7);
        }
    }
}
