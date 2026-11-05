//! Native request-time migration checks. Network checks precede atomic checked
//! generation publication; no synchronous guard is held across an await.
use crate::{config, fill::Embed, http::Http, sqlite::SqlStore, Config, Control, Target};
use prompt_framing::embedding_input::{self, Kind, Profile};
use serde_json::{json, Value};

const SAMPLE: [&str; 4] = [
    "The quick brown fox jumps over the lazy dog.",
    "Quarterly budget review moved to Tuesday afternoon.",
    "function add(a, b) { return a + b; }",
    "Heute regnet es, morgen soll die Sonne scheinen.",
];
#[derive(Debug)]
pub struct Error {
    pub status: u16,
    pub body: Value,
}
fn refusal(code: &str, message: &str) -> Error {
    Error {
        status: 409,
        body: json!({"error":message,"code":code}),
    }
}
fn unavailable() -> Error {
    Error {
        status: 503,
        body: json!({"error":"The embedding index is unavailable.","code":"indexUnavailable"}),
    }
}
impl From<config::Error> for Error {
    fn from(e: config::Error) -> Self {
        Self {
            status: e.status,
            body: json!({"error":e.message,"code":e.code}),
        }
    }
}
fn path_id(id: &str) -> bool {
    let lower = id.to_lowercase();
    id.starts_with(['/', '~'])
        || id.starts_with("./")
        || id.starts_with("../")
        || (id.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && id.as_bytes().get(1) == Some(&b':')
            && id
                .as_bytes()
                .get(2)
                .is_some_and(|c| matches!(c, b'/' | b'\\')))
        || [".gguf", ".bin", ".safetensors"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
}
/// Remote response bodies are never exposed. Only the bounded listed ids from
/// the models API form part of the historical public refusal contract.
pub fn served_result(target: &Target, ids: Result<Vec<String>, String>) -> Result<(), Error> {
    let shown: String = target.model.trim().chars().take(120).collect();
    let ids = ids.map_err(|_| {
        refusal(
            "endpointUnverified",
            &format!("Could not confirm that the embeddings server serves {shown}."),
        )
    })?;
    if crate::serves_model(&ids, &target.model) {
        return Ok(());
    }
    let error = if !ids.is_empty() && ids.iter().all(|id| path_id(id)) {
        format!("The embeddings server lists only a model file, not model names, so it cannot be confirmed that it serves {shown}. Start it with --alias {shown} (or load {shown} there).")
    } else {
        format!("The embeddings server does not serve {shown}.")
    };
    let served: Vec<String> = ids
        .iter()
        .take(20)
        .map(|id| id.chars().take(120).collect())
        .collect();
    Err(Error {
        status: 409,
        body: json!({"error":error,"code":"modelNotServed","served":served}),
    })
}
pub fn coverage(store: &SqlStore, config: &Config, target: &Target) -> Result<Value, Error> {
    let indexes = store.checked_indexes().map_err(|_| unavailable())?;
    let (mut chunks, mut vectors, mut unavailable, mut behind) = (0, 0, 0, 0);
    for index in &indexes {
        let Ok((c, v)) = store.coverage(index, target) else {
            unavailable += 1;
            continue;
        };
        chunks += c;
        vectors += v;
        if !target.same_identity(&config.active) {
            // A lost active read must never prove readiness for destruction.
            match store.coverage(index, &config.active) {
                Ok((_, a)) if v < a => behind += 1,
                Ok(_) => {}
                Err(_) => unavailable += 1,
            }
        }
    }
    Ok(
        json!({"indexes":indexes.len(),"chunks":chunks,"vectors":vectors,"unavailable":unavailable,"complete":unavailable==0&&vectors>=chunks,"ready":unavailable==0&&behind==0}),
    )
}
pub fn parity_vectors(a: &[Vec<f32>], b: &[Vec<f32>]) -> Value {
    let mut min = 1.0_f64;
    let mut valid = a.len() == SAMPLE.len() && a.len() == b.len();
    for (a, b) in a.iter().zip(b) {
        if a.is_empty() || a.len() != b.len() {
            valid = false;
            break;
        }
        let (mut dot, mut na, mut nb) = (0., 0., 0.);
        for (&x, &y) in a.iter().zip(b) {
            let (x, y) = (f64::from(x), f64::from(y));
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        let sim = dot / (na.sqrt() * nb.sqrt());
        valid &= sim.is_finite();
        min = min.min(sim);
    }
    json!({"ok":valid&&min>=0.995,"minCosine":if valid {Some((min*1e5).round()/1e5)} else {None}})
}
async fn parity(http: &Http, a: &Target, b: &Target) -> Result<Value, String> {
    let input = |t: &Target| -> Result<Vec<Vec<u16>>, String> {
        let profile = Profile::try_from(t.profile.as_str()).map_err(|_| "unknownProfile")?;
        Ok(SAMPLE
            .iter()
            .map(|s| {
                embedding_input::frame(
                    profile,
                    Kind::Document,
                    &s.encode_utf16().collect::<Vec<_>>(),
                    None,
                )
            })
            .collect())
    };
    let control = Control::default();
    let a = http.embed(a, &input(a)?, &control).await?;
    let b = http.embed(b, &input(b)?, &control).await?;
    Ok(parity_vectors(&a, &b))
}
/// This layer performs migration checks only. The route owns authorization,
/// address validation, Diary operations, audit and managed service serialization.
pub async fn apply(
    store: &SqlStore,
    http: &Http,
    change: config::Change,
    force: bool,
) -> Result<config::Published, Error> {
    let checked = store.config().map_err(|_| unavailable())?;
    match &change {
        config::Change::SetNext(target) => {
            // Match alreadyActive before making any network request.
            if target == &checked.active {
                return Err(refusal(
                    "alreadyActive",
                    "That model, profile and address are already active.",
                ));
            }
            served_result(target, http.served(target).await)?;
        }
        config::Change::Switch => {
            let next = checked
                .next
                .as_ref()
                .ok_or_else(|| refusal("noNext", "No re-index is under way."))?;
            served_result(next, http.served(next).await)?;
            if next.same_identity(&checked.active) {
                let parity = parity(http, &checked.active, next)
                    .await
                    .unwrap_or_else(|_| json!({"ok":false,"error":"embedding HTTP unavailable"}));
                if parity.get("ok") != Some(&Value::Bool(true)) && !force {
                    let mut error=refusal("parityFailed","The new embeddings address does not give the same vectors as the current one.");
                    if let Some(body) = error.body.as_object_mut() {
                        body.insert("parity".into(), parity);
                    }
                    return Err(error);
                }
            } else if !force {
                let cov = coverage(store, &checked, next)?;
                if cov.get("ready") != Some(&Value::Bool(true)) {
                    let mut error = refusal(
                        "notReady",
                        "The new model has not finished indexing every project yet.",
                    );
                    if let Some(body) = error.body.as_object_mut() {
                        body.insert("coverage".into(), cov);
                    }
                    return Err(error);
                }
            }
        }
        _ => {}
    }
    // Repeat readiness under the same config lock used by outbox publication.
    // Preserve the detailed public refusal returned by the final read.
    let require_coverage = matches!(change, config::Change::Switch);
    let mut final_error = None;
    let result = config::publish_checked(&store.root, &store.seed, &checked, change, || {
        if require_coverage && !force {
            if let Some(next) = checked
                .next
                .as_ref()
                .filter(|n| !n.same_identity(&checked.active))
            {
                match coverage(store, &checked, next) {
                    Ok(cov) if cov.get("ready") == Some(&Value::Bool(true)) => {}
                    result => {
                        final_error = Some(match result {
                            Err(e) => e,
                            Ok(cov) => {
                                let mut e = refusal(
                                    "notReady",
                                    "The new model has not finished indexing every project yet.",
                                );
                                if let Some(body) = e.body.as_object_mut() {
                                    body.insert("coverage".into(), cov);
                                }
                                e
                            }
                        });
                        return Err(config::Error {
                            status: 409,
                            code: "notReady",
                            message: "The new model has not finished indexing every project yet.",
                        });
                    }
                }
            }
        }
        Ok(())
    });
    if let Some(error) = final_error {
        return Err(error);
    }
    let published = result.map_err(Error::from)?;
    // Publication is complete and its file guard has been released. Cleanup is
    // best effort as in the legacy owner, but every candidate is revalidated
    // inside the existing tenant/config/outbox/index fences. Enumeration failure
    // grants no deletion authority. A later retry can safely repeat this work.
    if !published.retired.is_empty() {
        if let Ok(indexes) = store.checked_indexes() {
            for target in &published.retired {
                for index in &indexes {
                    let _ = store.drop_orphan(index, target);
                }
            }
        }
    }
    Ok(published)
}

/// Request-time status uses the same generation as its coverage reads. It
/// refuses a mixed-generation view instead of labelling a stale model ready.
pub fn status(
    store: &SqlStore,
    start: &str,
    now: u64,
    options: &crate::Options,
) -> Result<Value, Error> {
    let config = store.config().map_err(|_| unavailable())?;
    let snapshot = std::fs::read(store.root.join("embedding-worker-status.json"))
        .ok()
        .filter(|b| b.len() <= 1024 * 1024)
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(|v| {
            v.get("protocol").and_then(Value::as_str) == Some("rust-embedding-worker-v1")
                && v.get("start").and_then(Value::as_str) == Some(start)
                && v.get("at")
                    .and_then(Value::as_u64)
                    .is_some_and(|at| at <= now && now - at <= 15000)
        });
    let mut result = config::state_json(&config);
    let object = result.as_object_mut().ok_or_else(unavailable)?;
    object.insert(
        "source".into(),
        json!(if store.root.join("embedding-config.json").exists() {
            "saved"
        } else {
            "environment"
        }),
    );
    object.insert("coverage".into(), json!({"active":coverage(store,&config,&config.active)?,"next":config.next.as_ref().map(|t|coverage(store,&config,t)).transpose()?}));
    for key in [
        "running",
        "lastPass",
        "nextServed",
        "nextHeld",
        "configValid",
        "configInvalid",
    ] {
        object.insert(
            key.into(),
            snapshot
                .as_ref()
                .and_then(|s| s.get(key))
                .cloned()
                .unwrap_or(Value::Null),
        );
    }
    object.insert("stale".into(), json!(snapshot.is_none()));
    object.insert("duty".into(), json!(options.duty));
    object.insert("batch".into(), json!(options.batch));
    let current = store.config().map_err(|_| unavailable())?;
    if current.version != config.version
        || current.active != config.active
        || current.next != config.next
        || current.previous != config.previous
    {
        return Err(unavailable());
    }
    Ok(result)
}
