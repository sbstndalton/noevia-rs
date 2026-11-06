//! Request-time embedding ownership. The bounded actor owns accepted work across
//! caller cancellation. Docker mutations carry the shared maintenance permit to
//! the native loader; no public operation dispatches to a Node executor.
use crate::{identity, reply, App};
use axum::{
    body::{to_bytes, Body},
    http::{header, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use embedding_worker::{
    config::Change,
    http::{Diary, Http},
    migration, runtime,
    service::{self, Service},
    service_owner,
    sqlite::{open_vec_required, SqlStore},
    Options, Target,
};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, time::Duration};
pub const PATH: &str = "/api/embedding";
pub const FEATURE: &str = "noevia-embedding-routes-authority/1";
const ACTIONS: &str = "action must be one of set-next, clear-next, switch, rollback, finish, service-switch, service-off";
const WAIT: Duration = Duration::from_secs(180);
type Answer = (u16, Value);
pub struct Settings {
    store: Option<SqlStore>,
    http: Option<Http>,
    diary: Option<Diary>,
    diary_configured: bool,
    service_url: String,
    start: String,
    options: Options,
    owner: tokio::sync::OnceCell<service_owner::Client<Http>>,
}
impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EmbeddingSettings")
    }
}
impl Settings {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let value = |key: &str| get(key).filter(|s| !s.is_empty());
        let store = value("UI_DATA_DIR").map(|root| SqlStore {
            root: root.into(),
            rag_dir: value("RAG_DIR").map(PathBuf::from),
            seed: runtime::seed(&get).unwrap_or_else(|_| runtime::invalid_seed()),
            open_vec: open_vec_required,
        });
        let loader = value("MODEL_LOADER_URL").unwrap_or_default();
        let inference = value("INFERENCE_BASE_URL")
            .unwrap_or_else(|| "http://host.docker.internal:11434".into());
        let http = Http::new(
            &loader,
            value("MODEL_LOADER_TOKEN"),
            &inference,
            value("INFERENCE_API_KEY").filter(|s| s != "local"),
        )
        .ok()
        .and_then(|http| {
            match (
                value("NOEVIA_LEGACY_UPSTREAM"),
                value("NOEVIA_PROJECT_STORE_TOKEN"),
            ) {
                (Some(core), Some(token)) => http.with_admission(&core, &token).ok(),
                _ => Some(http),
            }
        });
        let diary_base =
            value("DIARY_BASE_URL").unwrap_or_else(|| server_diary::config::DEFAULT_BASE.into());
        let diary = (!diary_base.is_empty())
            .then(|| {
                Diary::new(
                    &diary_base,
                    value("DIARY_AUTH_TOKEN").map(|t| t.trim().to_owned()),
                )
            })
            .and_then(Result::ok);
        let number = |key: &str, fallback: f64| {
            get(key)
                .map(|s| {
                    if s.trim().is_empty() {
                        0.
                    } else {
                        s.parse().unwrap_or(f64::NAN)
                    }
                })
                .unwrap_or(fallback)
        };
        Self {
            store,
            http,
            diary,
            diary_configured: !diary_base.is_empty(),
            service_url: value("EMBEDDING_SERVICE_URL")
                .unwrap_or_else(|| "http://cowork-embed:8023".into()),
            start: value("NOEVIA_START_ID").unwrap_or_default(),
            options: Options::new(
                number("EMBEDDING_REINDEX_DUTY", 0.5),
                number("EMBEDDING_REINDEX_BATCH", 4.),
                50.,
                60000.,
                300000.,
            ),
            owner: tokio::sync::OnceCell::new(),
        }
    }
    fn status(&self) -> Result<Value, Answer> {
        migration::status(
            self.store.as_ref().ok_or_else(unavailable)?,
            &self.start,
            now(),
            &self.options,
        )
        .map_err(|e| (e.status, e.body))
    }
    async fn diary(&self, http: &Http, body: Option<&Value>) -> Value {
        let Some(diary) = &self.diary else {
            return if self.diary_configured {
                json!({"error":"The Diary could not be reached."})
            } else {
                json!({"error":"The Diary sidecar is not configured.","code":"notConfigured"})
            };
        };
        match http.diary(diary, body).await {
            Ok((status, body)) if (200..300).contains(&status) => body,
            Ok((status, _)) => {
                json!({"error":format!("The Diary answered HTTP {status}."),"status":status})
            }
            Err(_) => {
                json!({"error":"The Diary could not be reached (embedding HTTP unavailable)."})
            }
        }
    }
    async fn diary_targets(&self, http: &Http) -> Option<Value> {
        if !self.diary_configured {
            return Some(json!({"active":null,"next":null,"previous":null}));
        }
        let state = self.diary(http, None).await;
        (state.get("error").is_none() && state.get("active").is_some_and(Value::is_object))
            .then_some(state)
    }
}
fn now() -> u64 {
    identity::now_ms().max(0) as u64
}
fn unavailable() -> Answer {
    (
        503,
        json!({"error":"The embedding service is unavailable. Try again shortly."}),
    )
}
fn err(status: u16, message: impl Into<String>, code: Option<&str>) -> Answer {
    let mut value = json!({"error":message.into()});
    if let Some(code) = code {
        if let Some(v) = value.as_object_mut() {
            v.insert("code".into(), json!(code));
        }
    }
    (status, value)
}
fn text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => a.iter().map(text).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}
fn field(body: &Value, key: &str) -> String {
    text(body.get(key).unwrap_or(&Value::Null))
}
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.),
        _ => true,
    }
}
fn response(answer: Answer) -> Response<Body> {
    let mut response = reply::json_text(
        StatusCode::from_u16(answer.0).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        answer.1.to_string(),
        true,
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
fn present(v: &Value, key: &str) -> bool {
    v.get(key).is_some_and(truthy)
}
async fn mutate_service(
    app: &App,
    http: &Http,
    service: &mut Service,
    models: &Value,
    hold: &crate::model_maint::Hold,
) -> Result<(), Answer> {
    if !service.supports_operations() || !hold.may_start(Duration::from_secs(65)) {
        return Err(unavailable());
    }
    let mut operation = hold
        .begin_operation("embedding-service", None)
        .ok_or_else(unavailable)?;
    let client = http
        .with_operation(&serde_json::to_value(&operation.fence).map_err(|_| unavailable())?)
        .map_err(|_| unavailable())?;
    let answer = service.serve(&client, models, now).await;
    // An HTTP deadline never proves Docker settlement. The actor retains the
    // exact hold while the durable destination receipt is awaited, with no retry.
    let terminal = operation
        .authority
        .wait_for_settlement(&operation.fence, Duration::from_secs(180))
        .await;
    operation.settled = terminal
        .as_ref()
        .is_some_and(|r| matches!(r.state.as_str(), "settled" | "conflict"));
    if !operation.settled {
        let _ = app;
        return Err(unavailable());
    }
    if terminal.is_some_and(|r| r.state == "conflict") {
        return Err(unavailable());
    }
    answer.map_err(|e| err(e.status, e.message, Some(e.code)))?;
    Ok(())
}
fn audit(app: &App, user: &str, action: &str, detail: &Value) {
    if let Ok(store) = app.writes.writer() {
        let _=store.write(|tx|{tx.execute("INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,NULL,?2,?3,?4)",server_store::rusqlite::params![user,action,detail.to_string(),identity::now_ms()])?;Ok(())});
    }
}
async fn apply(
    settings: &Settings,
    app: &App,
    http: &Http,
    service: &mut Service,
    body: &Value,
    user: &str,
) -> Answer {
    let action = field(body, "action");
    let scope = body
        .get("scope")
        .filter(|v| truthy(v))
        .map(text)
        .unwrap_or_else(|| "all".into());
    if matches!(action.as_str(), "service-off" | "service-switch") {
        return service_action(settings, app, http, service, body, user).await;
    }
    if !matches!(
        action.as_str(),
        "set-next" | "clear-next" | "switch" | "rollback" | "finish"
    ) {
        return err(400, ACTIONS, None);
    }
    if !matches!(scope.as_str(), "all" | "projects" | "diary") {
        return err(400, "scope must be all, projects or diary", None);
    }
    let target = body.get("target").unwrap_or(&Value::Null);
    if action == "set-next" {
        if target.is_null() || !matches!(target, Value::Object(_) | Value::Array(_)) {
            return err(
                400,
                "set-next needs a target { baseUrl, model, profile }",
                None,
            );
        }
        for key in ["baseUrl", "diaryBaseUrl"] {
            let raw = field(target, key);
            if raw.trim().is_empty() {
                continue;
            }
            if let Err(reason) = Http::check_address(raw.trim()).await {
                return err(400,format!("The embeddings server must be on this network: it receives the text of every project and the Diary ({reason})."),Some("publicEndpoint"));
            }
        }
        let model = target
            .get("model")
            .filter(|v| truthy(v))
            .map(text)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let base = field(target, "baseUrl").trim().to_owned();
        let diary_base = target
            .get("diaryBaseUrl")
            .filter(|v| !v.is_null())
            .map(text)
            .unwrap_or_else(|| base.clone())
            .trim()
            .to_owned();
        if scope != "projects" && diary_base.is_empty() {
            return err(
                409,
                "Name the Diary's embeddings server so noevia can check it serves this model.",
                Some("diaryEndpointRequired"),
            );
        }
        let mut addresses = vec![];
        if scope != "diary" {
            addresses.push(base);
        }
        if scope != "projects" && !addresses.contains(&diary_base) {
            addresses.push(diary_base);
        }
        for base in addresses {
            let t = Target {
                model: model.clone(),
                profile: String::new(),
                base_url: (!base.is_empty()).then_some(base),
            };
            if let Err(e) = migration::served_result(&t, http.served(&t).await) {
                return (e.status, e.body);
            }
        }
    }
    let mut out = serde_json::Map::new();
    if scope != "diary" {
        let result = async {
            let change = match action.as_str() {
                "set-next" => Change::SetNext(normalize(target)?),
                "clear-next" => Change::ClearNext,
                "switch" => Change::Switch,
                "rollback" => Change::Rollback,
                _ => Change::Finish,
            };
            let store = settings.store.as_ref().ok_or_else(unavailable)?;
            migration::apply(store, http, change, body.get("force") == Some(&json!(true)))
                .await
                .map_err(|e| (e.status, e.body))?;
            settings.status()
        }
        .await;
        let value = match result {
            Ok(v) => v,
            Err((status, mut v)) => {
                if let Some(v) = v.as_object_mut() {
                    v.insert("status".into(), json!(status));
                }
                v
            }
        };
        out.insert("projects".into(), value);
    }
    if scope != "projects" {
        let mut request = json!({"action":action});
        if let Some(request) = request.as_object_mut() {
            if target.is_object() || target.is_array() {
                let mut translated = serde_json::Map::new();
                for key in ["model", "profile"] {
                    if let Some(value) = target.get(key) {
                        translated.insert(key.into(), value.clone());
                    }
                }
                translated.insert(
                    "base_url".into(),
                    target
                        .get("diaryBaseUrl")
                        .filter(|v| !v.is_null())
                        .or_else(|| target.get("baseUrl").filter(|v| !v.is_null()))
                        .cloned()
                        .unwrap_or_else(|| json!("")),
                );
                request.insert("target".into(), Value::Object(translated));
            }
            if body.get("force") == Some(&json!(true)) {
                request.insert("force".into(), json!(true));
            }
        }
        out.insert("diary".into(), settings.diary(http, Some(&request)).await);
    }
    audit(
        app,
        user,
        &format!("embedding.{action}"),
        &json!({"scope":scope,"model":target.get("model"),"profile":target.get("profile"),"force":body.get("force")==Some(&json!(true)),"projects":out.get("projects").map(|v|if present(v,"error"){"refused"}else{"ok"}).unwrap_or("skipped"),"diary":out.get("diary").map(|v|if present(v,"error"){"refused"}else{"ok"}).unwrap_or("skipped")}),
    );
    if matches!(
        action.as_str(),
        "finish" | "clear-next" | "rollback" | "switch"
    ) && out.values().any(|v| !present(v, "error"))
    {
        sync_service(settings, app, http, service).await;
    }
    let failed: Vec<_> = out.values().filter(|v| present(v, "error")).collect();
    let status = if failed.is_empty() {
        200
    } else if failed.len() == out.len() {
        failed
            .first()
            .and_then(|v| v.get("status"))
            .and_then(Value::as_u64)
            .filter(|s| (400..600).contains(s))
            .unwrap_or(502) as u16
    } else {
        207
    };
    (status, Value::Object(out))
}

fn normalize(value: &Value) -> Result<Target, Answer> {
    let model = field(value, "model").trim().to_owned();
    if model.is_empty()
        || model.encode_utf16().count() > 200
        || model.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(err(
            400,
            "An embedding target needs a model name (no spaces, at most 200 characters).",
            Some("invalidTarget"),
        ));
    }
    let seed = runtime::seed(|_| None).map_err(|_| unavailable())?;
    let profile = value
        .get("profile")
        .filter(|v| !v.is_null())
        .map(text)
        .unwrap_or_else(|| seed.active.profile.clone())
        .trim()
        .to_owned();
    let profile = if profile.is_empty() {
        seed.active.profile
    } else {
        profile
    };
    if prompt_framing::embedding_input::Profile::try_from(profile.as_str()).is_err() {
        return Err(err(
            400,
            format!(
                "Unknown embedding profile \"{profile}\" (known: {}).",
                prompt_framing::embedding_input::PROFILE_NAMES.join(", ")
            ),
            Some("invalidTarget"),
        ));
    }
    let base = field(value, "baseUrl").trim().to_owned();
    if !base.is_empty() {
        let url = reqwest::Url::parse(&base).map_err(|_| {
            err(
                400,
                "The embeddings address must be an http(s) URL.",
                Some("invalidTarget"),
            )
        })?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(err(400,"The embeddings address must be a plain http(s) URL, with no credentials, query or fragment.",Some("invalidTarget")));
        }
    }
    Ok(Target {
        model,
        profile,
        base_url: (!base.is_empty()).then(|| base.trim_end_matches('/').to_owned()),
    })
}
async fn sync_service(settings: &Settings, app: &App, http: &Http, service: &mut Service) {
    if !service.managed() || !service.enabled() {
        return;
    }
    let Some(diary) = settings.diary_targets(http).await else {
        return;
    };
    let Ok(projects) = settings.status() else {
        return;
    };
    let Some(models) = service::served_set(
        service,
        Some(&settings.service_url),
        &projects,
        &diary,
        true,
    )
    .filter(|v| !v.is_empty()) else {
        return;
    };
    let Ok(hold) = app
        .config
        .maint
        .acquire("embedding service sync", Duration::from_secs(300), false)
        .await
    else {
        return;
    };
    let _ = mutate_service(app, http, service, &json!(models), &hold).await;
}
async fn service_action(
    settings: &Settings,
    app: &App,
    http: &Http,
    service: &mut Service,
    body: &Value,
    user: &str,
) -> Answer {
    if !service.managed() {
        return err(409,"The embedding service is not managed by noevia on this server (see docs/deployment.md, #1668).",Some("notManaged"));
    }
    let c = match settings.status() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let off = field(body, "action") == "service-off";
    let model = field(body, "model").trim().to_owned();
    if !off && model.is_empty() {
        return err(400, "service-switch needs a model", None);
    }
    if present(&c, "next") {
        return err(
            409,
            if off {
                "A re-index is under way. Cancel it before turning embedding off."
            } else {
                "A re-index is already under way. Finish or cancel it first."
            },
            Some("reindexUnderway"),
        );
    }
    if present(&c, "previous") {
        return err(
            409,
            if off {
                "Finish the last embedding switch (or roll it back) before turning embedding off."
            } else {
                "Finish the last embedding switch (or roll it back) before switching again."
            },
            Some("finishFirst"),
        );
    }
    if off {
        let Some(d) = settings.diary_targets(http).await else {
            return err(409,"The Diary's embedding settings could not be read, so turning embedding off could stop a switch it has under way. Try again when the Diary answers.",Some("diaryUnknown"));
        };
        if present(&d, "next") || present(&d, "previous") {
            return err(409,"The Diary has not finished its embedding switch. Finish it (or roll it back) before turning embedding off.",Some("finishFirst"));
        }
        // Settings may change while Diary is read. Refuse before stopping anything.
        let Ok(current) = settings.status() else {
            return unavailable();
        };
        if current.get("active") != c.get("active")
            || current.get("next") != c.get("next")
            || current.get("previous") != c.get("previous")
        {
            return unavailable();
        }
        let Ok(hold) = app
            .config
            .maint
            .acquire("embedding service change", Duration::from_secs(300), false)
            .await
        else {
            return unavailable();
        };
        if let Err(e) = mutate_service(app, http, service, &json!([]), &hold).await {
            return e;
        }
        audit(app, user, "embedding.service-off", &json!({}));
        return (200, json!({"service":service.status(now())}));
    }
    let file = match http.resolve_file(&model).await {
        Ok(Some(file)) => file,
        _ => {
            return err(
                404,
                format!("{model} is not an installed model file."),
                Some("modelNotInstalled"),
            )
        }
    };
    let active = c.get("active").unwrap_or(&Value::Null);
    let on_service = service::same_address(
        Some(&settings.service_url),
        active.get("baseUrl").and_then(Value::as_str),
    );
    let address = if on_service {
        field(active, "baseUrl")
    } else {
        settings.service_url.clone()
    };
    let from_off = !service.enabled();
    let mut keep = vec![];
    if on_service && !from_off {
        let Some(d) = settings.diary_targets(http).await else {
            return err(409,"The Diary's embedding settings could not be read, so the switch could drop a model it still uses. Try again when the Diary answers.",Some("diaryUnknown"));
        };
        let want = service::served_set(service, Some(&settings.service_url), &c, &d, true);
        let known = service::served_set(service, Some(&settings.service_url), &c, &d, false);
        let overflow = |v: &Option<Vec<Value>>| {
            v.as_ref().is_none_or(|v| {
                v.len()
                    + usize::from(
                        !v.iter()
                            .any(|v| v.get("model").and_then(Value::as_str) == Some(&model)),
                    )
                    > 2
            })
        };
        if overflow(&want) && !overflow(&known) {
            return err(409,"The Diary uses its default embedding server, so noevia cannot tell whether it shares the embedding service. Give the Diary an explicit embeddings address in Settings, then switch again.",Some("diaryAddressUnknown"));
        }
        keep=match want{Some(v)=>v,None=>return err(409,"The embedding service already serves two models in use. Finish or cancel the switch under way first.",Some("tooManyModels"))};
    }
    if keep
        .iter()
        .any(|v| v.get("model").and_then(Value::as_str) == Some(&model))
    {
        return err(
            409,
            format!("{model} is already the embedding model."),
            Some("alreadyActive"),
        );
    }
    if keep.len() + 1 > 2 {
        return err(409,format!("Projects and the Diary use {}; the embedding service serves at most two models. Finish or roll back the other switch first.",keep.iter().filter_map(|v|v.get("model").and_then(Value::as_str)).collect::<Vec<_>>().join(" and ")),Some("tooManyModels"));
    }
    let before = if from_off {
        json!([])
    } else {
        service
            .status(now())
            .and_then(|s| s.get("models").cloned())
            .unwrap_or_else(|| json!([]))
    };
    keep.push(json!({"model":model,"file":file}));
    let Ok(hold) = app
        .config
        .maint
        .acquire("embedding service change", Duration::from_secs(300), false)
        .await
    else {
        return unavailable();
    };
    let Ok(current) = settings.status() else {
        return unavailable();
    };
    if current.get("active") != c.get("active")
        || current.get("next") != c.get("next")
        || current.get("previous") != c.get("previous")
    {
        return unavailable();
    }
    if let Err(e) = mutate_service(app, http, service, &json!(keep), &hold).await {
        return e;
    }
    if !service
        .wait_loaded(http, std::slice::from_ref(&model), WAIT, now)
        .await
    {
        if !hold.may_start(Duration::from_secs(65)) {
            return unavailable();
        }
        let _ = mutate_service(app, http, service, &before, &hold).await;
        return (
            504,
            json!({"error":format!("The embedding service did not load {model} in time. Nothing was re-indexed; try again or pick another model."),"code":"serviceNotLoaded","service":service.status(now())}),
        );
    }
    if !hold.may_start(Duration::from_secs(1)) {
        return unavailable();
    }
    let profile = body
        .get("profile")
        .cloned()
        .unwrap_or_else(|| active.get("profile").cloned().unwrap_or(Value::Null));
    if from_off
        && on_service
        && active.get("model").and_then(Value::as_str) == Some(&model)
        && active.get("profile") == Some(&profile)
    {
        return (
            200,
            json!({"service":service.status(now()),"projects":settings.status().ok()}),
        );
    }
    // Separate operation: container settlement is already proven. Keep its hold
    // continuously through the final target validation and configuration write.
    let request = json!({"action":"set-next","scope":"all","target":{"baseUrl":address,"diaryBaseUrl":address,"model":model,"profile":body.get("profile")}});
    let mut result = Box::pin(apply(settings, app, http, service, &request, user)).await;
    if let Some(out) = result.1.as_object_mut() {
        out.insert("service".into(), json!(service.status(now())));
    }
    result
}
pub async fn serve(app: &Arc<App>, req: Request<Body>) -> Response<Body> {
    let (parts, input) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = app.identity.clone();
    let identity = match tokio::task::spawn_blocking(move || {
        gate.gate_blocking(&creds, PATH, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => return e.into_response(),
        Err(_) => return response(unavailable()),
    };
    if identity.user.role != server_auth::Role::Admin {
        return response(err(403, "Administrator required", None));
    }
    let get = parts.method == Method::GET;
    if !get && parts.method != Method::POST {
        return response(err(405, "Method not allowed", None));
    }
    let body = if get {
        Value::Null
    } else {
        match to_bytes(input, 1024 * 1024)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        {
            Some(body) if body.is_object() => body,
            Some(_) => return response(err(400, "A JSON object is required", None)),
            None => return response(err(400, "invalid JSON", None)),
        }
    };
    if !get
        && !matches!(
            field(&body, "action").as_str(),
            "set-next"
                | "clear-next"
                | "switch"
                | "rollback"
                | "finish"
                | "service-switch"
                | "service-off"
        )
    {
        return response(err(400, ACTIONS, None));
    }
    let settings = &app.config.embedding;
    let Some(http) = settings.http.clone() else {
        return response(unavailable());
    };
    let owner = settings
        .owner
        .get_or_init(|| async move { service_owner::start(http).0 })
        .await
        .clone();
    let app = app.clone();
    let user = identity.user.id;
    let result=tokio::time::timeout(Duration::from_secs(240),owner.execute(move|service,http|Box::pin(async move{
        let settings=&app.config.embedding;
        service.refresh(http,now).await;
        let answer=if get{match settings.status(){Ok(projects)=>{let mut out=json!({"projects":projects,"diary":settings.diary(http,None).await});if service.managed(){if let Some(map)=out.as_object_mut(){map.insert("service".into(),json!(service.status(now())));}}(200,out)},Err(e)=>e}}else{apply(settings,&app,http,service,&body,&user).await};
        json!({"status":answer.0,"body":answer.1})
    }))).await;
    match result {
        Ok(Ok(v)) => response((
            v.get("status").and_then(Value::as_u64).unwrap_or(503) as u16,
            v.get("body").cloned().unwrap_or(Value::Null),
        )),
        _ => response(unavailable()),
    }
}
