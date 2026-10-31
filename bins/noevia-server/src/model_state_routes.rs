//! `GET /api/models/inference-budget` and `GET /api/models/installed` (sbstndalton/noevia#2262, capability
//! `rust-model-state-reads`). Node refuses both GETs (core server/rust-model-state-reads.cjs); other methods stay
//! Node's, among them the budget's PUT.
//!
//! **The budget's figure is asked of Node, never read from `settings`.** Node reads the row once at boot
//! (inference-budget.cjs `saved`) and refuses loads and runs its watchdog on that copy. A boot read that failed leaves
//! the row in place and Node enforcing the deployment's figure, so an answer from the row would show a limit nothing
//! enforces. A PUT goes through Node, so asking Node sees it at once. The boot fault of `LLAMACPP_MEMORY_LIMIT`
//! (`budgetFault`) never reaches this answer: Node's estimates and the budget do not consult it, because the inference
//! budget is always at least 2 GiB. No answer from Node is a 503, never a guessed figure.
//!
//! **The installed list is built here.** The router's listing, the long-profile pairing, `models.roles.json` and the
//! rows are this front's; the sidecar names and reports (the embedding service's state) and the shared folder scan
//! are in Node's memory and come over the loopback bridge (core rust-model-facts.cjs). Node keeps its own list for its
//! own readers (chat, the auto-router, deletes), so the first loaded model it remembers is passed on the same call.
//!
//! Behaviour changed on purpose:
//! - The `/props` probe of each loaded model is gone. Node's list threw its answers away.
//! - A router row with no string id is left out. Node sent a row without `name`, which made the web client refuse
//!   the whole list.
//! - No 2 s shared answer: one listing and one loopback call per GET, always current, so no write has to drop it.
//! - Node's estimates kept a footprint for 60 s; here each GET reads the GGUF headers again.
use crate::model_sizing::{read_presets, Presets, Sizing};
use crate::reply;
use axum::body::Body;
use axum::http::{Response, StatusCode};
use js_json::{wtf16, JValue};
use prompt_framing::json::Value;
use serde_json::json;
use server_stats::compose::Read;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

pub const BUDGET: &str = "/api/models/inference-budget";
pub const INSTALLED: &str = "/api/models/installed";
pub const ROUTES: &[&str] = &[BUDGET, INSTALLED];

const FACTS_PATH: &str = "/internal/models/facts";
const BUDGET_DEADLINE: Duration = Duration::from_secs(5);
/// Node's own bounds behind this call: 4 s for the sidecar backends, then 8 s for the folder scan.
const INSTALLED_DEADLINE: Duration = Duration::from_secs(15);
const ADMIN_REQUIRED: &str = "Administrator required for shared model profiles";
const LIST_FAILED: &str = "Could not list installed models.";
const BUDGET_UNAVAILABLE: &str =
    "The inference memory budget is unavailable right now. Try again shortly.";
const LIST_UNAVAILABLE: &str = "The installed models are unavailable right now. Try again shortly.";
const MTP_UNVERIFIED: &str = "MTP head presence is unverified: the model manager does not report native MTP for this installation. Check the exact download files; a missing label is not proof that the head is absent.";
/// Node's limits on what it hands the pairing (long-profile.cjs `pairsOf`).
const MAX_PAIR_ROWS: usize = 512;
const MAX_PAIR_ID_BYTES: usize = 512;
const MAX_PAIR_PATH_BYTES: usize = 4096;

fn answer(status: StatusCode, body: &JValue) -> Response<Body> {
    reply::json_text(
        status,
        wtf16::stringify(body).unwrap_or_else(|| "null".into()),
        true,
    )
}

async fn node_facts(
    app: &crate::App,
    body: &serde_json::Value,
    deadline: Duration,
) -> Option<JValue> {
    let (status, bytes) = app
        .config
        .research
        .node_call(FACTS_PATH, body, deadline)
        .await?;
    if status != 200 {
        return None;
    }
    wtf16::parse(&String::from_utf8_lossy(&bytes))
        .ok()
        .filter(JValue::is_object)
}

/// One read, after the session gate.
pub async fn serve(app: &Arc<crate::App>, path: &str, admin: bool) -> Response<Body> {
    if path == BUDGET {
        return budget(app, admin).await;
    }
    let sizing = Arc::clone(&app.config.sizing);
    if sizing.kind() != server_stats::Kind::LlamaCpp {
        return reply::error(StatusCode::BAD_GATEWAY, LIST_FAILED, true);
    }
    let listing = sizing.listing().await;
    match installed_list(app, &sizing, &listing).await {
        Ok(rows) => answer(StatusCode::OK, &JValue::Arr(rows)),
        Err(ListFault::Engine) => reply::error(StatusCode::BAD_GATEWAY, LIST_FAILED, true),
        Err(ListFault::Node) => {
            reply::error(StatusCode::SERVICE_UNAVAILABLE, LIST_UNAVAILABLE, true)
        }
    }
}

async fn budget(app: &Arc<crate::App>, admin: bool) -> Response<Body> {
    if !admin {
        return reply::error(StatusCode::FORBIDDEN, ADMIN_REQUIRED, true);
    }
    let facts = node_facts(app, &json!({ "want": "budget" }), BUDGET_DEADLINE).await;
    let figure = facts.as_ref().map(|f| f.get("budget").clone());
    let Some(JValue::Obj(mut body)) = figure.filter(|b| b.get("budgetGib").as_num().is_some())
    else {
        return reply::error(StatusCode::SERVICE_UNAVAILABLE, BUDGET_UNAVAILABLE, true);
    };
    let budget_gib = JValue::Obj(body.clone())
        .get("budgetGib")
        .as_num()
        .unwrap_or(0.0);
    let sizing = Arc::clone(&app.config.sizing);
    let mut failed = false;
    let models = if sizing.kind() == server_stats::Kind::LlamaCpp {
        let listing = sizing.listing().await;
        match estimates(&sizing, &listing, budget_gib).await {
            Some(rows) => {
                // #1708: a model whose file was deleted is not in the table, as in the Models tab. An unreadable
                // list leaves the rows as they are.
                let gone: HashSet<String> = match installed_list(app, &sizing, &listing).await {
                    Ok(list) => list
                        .iter()
                        .filter(|m| m.get("missingFile").as_bool() == Some(true))
                        .filter_map(|m| m.get("name").as_str().map(str::to_owned))
                        .collect(),
                    Err(_) => HashSet::new(),
                };
                JValue::Arr(
                    rows.into_iter()
                        .filter(|r| r.get("model").as_str().is_none_or(|m| !gone.contains(m)))
                        .collect(),
                )
            }
            None => {
                failed = true;
                JValue::Null
            }
        }
    } else {
        JValue::Null
    };
    body.push(("models".to_owned(), models));
    if failed {
        body.push(("estimateError".to_owned(), JValue::Str("engine".to_owned())));
    }
    answer(StatusCode::OK, &JValue::Obj(body))
}

// ── The router's rows ────────────────────────────────────────────────────────────────────────

/// A string as code units, lone surrogates mapped the way `wtf16::stringify` writes them back.
fn text(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|r| {
            r.unwrap_or_else(|e| {
                wtf16::lone_char(e.unpaired_surrogate()).unwrap_or(char::REPLACEMENT_CHARACTER)
            })
        })
        .collect()
}

fn string_at(row: &Value, key: &str) -> Option<String> {
    match row.get(key) {
        Some(Value::Str(s)) if !s.is_empty() => Some(text(s)),
        _ => None,
    }
}

fn id_of(row: &Value) -> Option<String> {
    string_at(row, "id")
}

fn status_value(row: &Value) -> Option<String> {
    row.get("status").and_then(|s| string_at(s, "value"))
}

/// llamacpp-manager.cjs `isAvailable`.
fn available(row: &Value) -> bool {
    matches!(status_value(row).as_deref(), Some("loaded" | "sleeping"))
}

fn args_of(row: &Value) -> Vec<String> {
    match row.get("status").and_then(|s| s.get("args")) {
        Some(Value::Arr(items)) => items
            .iter()
            .map(|a| match a {
                Value::Str(s) => text(s),
                _ => String::new(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// llamacpp-manager.cjs `nativeLabels`.
fn native_labels(row: &Value) -> Vec<&'static str> {
    let args = args_of(row);
    let has = |names: [&str; 2]| args.iter().any(|a| names.contains(&a.as_str()));
    let mut labels = Vec::new();
    if has(["--embedding", "--embeddings"]) {
        labels.push("embeddings");
    }
    if has(["--rerank", "--reranking"]) {
        labels.push("reranking");
    }
    let image = match row
        .get("architecture")
        .and_then(|a| a.get("input_modalities"))
    {
        Some(Value::Arr(items)) => items
            .iter()
            .any(|m| matches!(m, Value::Str(s) if text(s) == "image")),
        _ => false,
    };
    if image {
        labels.push("vision");
    }
    labels
}

fn labels_json(labels: &[&str]) -> JValue {
    JValue::Arr(
        labels
            .iter()
            .map(|l| JValue::Str((*l).to_owned()))
            .collect(),
    )
}

/// model-system.cjs `modelPathFromArgs`.
fn model_path(args: &[String]) -> Option<&str> {
    let at = args.iter().position(|a| a == "--model" || a == "-m")?;
    args.get(at + 1)
        .map(String::as_str)
        .filter(|p| !p.is_empty())
}

fn meta_number(row: &Value, key: &str) -> Option<f64> {
    match row.get("meta").and_then(|m| m.get(key)) {
        Some(Value::Num(n)) if n.is_finite() => Some(*n),
        _ => None,
    }
}

fn data_rows(listing: &Read) -> Option<&[Value]> {
    match listing.ok_body()?.get("data") {
        Some(Value::Arr(rows)) => Some(rows),
        _ => None,
    }
}

// ── The budget's rows ────────────────────────────────────────────────────────────────────────

/// llamacpp-manager.cjs `inferenceEstimates`: `None` when the listing cannot be read.
async fn estimates(sizing: &Arc<Sizing>, listing: &Read, budget_gib: f64) -> Option<Vec<JValue>> {
    data_rows(listing)?;
    let sizing = Arc::clone(sizing);
    let listing = listing.clone();
    tokio::task::spawn_blocking(move || {
        let roles = crate::model_kind::roles_beside(sizing.presets_path());
        let rows = data_rows(&listing).unwrap_or(&[]);
        rows.iter()
            .filter_map(|row| {
                let id = id_of(row)?;
                let value = status_value(row);
                let args = args_of(row);
                let estimate = sizing
                    .footprint(&id, &listing)
                    .and_then(|t| wtf16::parse(&t).ok());
                let fits = match &estimate {
                    Some(e) if budget_gib > 0.0 => {
                        let total = e.get("totalGib").as_num().filter(|n| n.is_finite());
                        let unbounded = e.get("cacheRamUnbounded").truthy();
                        JValue::Bool(!unbounded && total.is_some_and(|t| t <= budget_gib))
                    }
                    _ => JValue::Null,
                };
                Some(JValue::obj([
                    ("model", JValue::Str(id.clone())),
                    ("loaded", JValue::Bool(value.as_deref() == Some("loaded"))),
                    (
                        "sleeping",
                        JValue::Bool(value.as_deref() == Some("sleeping")),
                    ),
                    ("labels", labels_json(&native_labels(row))),
                    (
                        "system",
                        JValue::Bool(crate::model_kind::is_system_model_at(
                            &roles,
                            &id,
                            model_path(&args),
                        )),
                    ),
                    ("estimate", estimate.unwrap_or(JValue::Null)),
                    ("fits", fits),
                ]))
            })
            .collect()
    })
    .await
    .ok()
}

// ── The installed list ───────────────────────────────────────────────────────────────────────

enum ListFault {
    /// The router did not list: Node's 502.
    Engine,
    /// Node did not answer: what the rows mean cannot be told.
    Node,
}

fn hash_id(id: &str) -> bool {
    (32..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `[(long profile id, its base)]`, long-profile.cjs `pairsOf`.
fn long_pairs(rows: &[Value], presets: Option<&Presets>) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    let mut input = Vec::new();
    for row in rows {
        let Some(id) = id_of(row) else { continue };
        if id.len() > MAX_PAIR_ID_BYTES || input.len() >= MAX_PAIR_ROWS || !seen.insert(id.clone())
        {
            continue;
        }
        let args = args_of(row);
        let model = model_path(&args)
            .map(str::to_owned)
            .or_else(|| presets.and_then(|p| p.files(&id).0))
            .filter(|m| m.len() <= MAX_PAIR_PATH_BYTES);
        input.push(long_profile::Row { id, model });
    }
    long_profile::pairs(&input)
        .into_iter()
        .map(|p| (p.long, p.base))
        .collect()
}

/// A scanned file for `id` (models.cjs `fileFor`: by the section name or model id the scan reports).
fn file_for<'a>(scan: Option<&'a [JValue]>, id: &str) -> Option<&'a JValue> {
    scan?.iter().find(|f| {
        f.get("modelId").as_str() == Some(id)
            || matches!(f.get("sections"), JValue::Arr(s) if s.iter().any(|x| x.as_str() == Some(id)))
    })
}

fn strings(v: &JValue) -> HashSet<String> {
    match v {
        JValue::Arr(items) => items
            .iter()
            .filter_map(|s| s.as_str().map(|t| t.trim().to_owned()))
            .collect(),
        _ => HashSet::new(),
    }
}

fn round1(n: f64) -> f64 {
    llamacpp_autoconfig::js_round(n * 10.0) / 10.0
}

/// models.cjs `modelsInstalledFresh` over the listing `listing`.
async fn installed_list(
    app: &crate::App,
    sizing: &Arc<Sizing>,
    listing: &Read,
) -> Result<Vec<JValue>, ListFault> {
    let raw = data_rows(listing).ok_or(ListFault::Engine)?;
    let rows: Vec<&Value> = raw.iter().filter(|r| id_of(r).is_some()).collect();
    let (pairs, roles) = {
        let sizing = Arc::clone(sizing);
        let listing = listing.clone();
        tokio::task::spawn_blocking(move || {
            let presets = sizing.presets_path().and_then(|p| read_presets(p).ok());
            (
                long_pairs(data_rows(&listing).unwrap_or(&[]), presets.as_ref()),
                crate::model_kind::roles_beside(sizing.presets_path()),
            )
        })
        .await
        .map_err(|_| ListFault::Engine)?
    };
    let long_of: HashMap<&str, &str> = pairs
        .iter()
        .map(|(l, b)| (l.as_str(), b.as_str()))
        .collect();
    let long_for: HashMap<&str, &str> = pairs
        .iter()
        .map(|(l, b)| (b.as_str(), l.as_str()))
        .collect();
    let router_loaded: HashSet<String> = rows
        .iter()
        .filter(|r| available(r))
        .filter_map(|r| id_of(r))
        .collect();
    let listed = |r: &&&Value| {
        id_of(r).is_some_and(|id| !hash_id(&id) && !long_of.contains_key(id.as_str()))
    };
    let first_loaded = rows.iter().filter(listed).find_map(|r| {
        let id = id_of(r)?;
        let serving = native_labels(r)
            .iter()
            .any(|l| *l == "embeddings" || *l == "reranking");
        (router_loaded.contains(&id) && !serving).then_some(id)
    });
    let presetted = rows
        .iter()
        .any(|r| string_at(r, "source").as_deref() == Some("preset"));
    let ids: Vec<String> = rows.iter().filter_map(|r| id_of(r)).collect();
    let mut ask = json!({ "want": "installed", "ids": ids, "scan": presetted });
    if let (Some(first), Some(o)) = (first_loaded, ask.as_object_mut()) {
        o.insert("firstLoaded".into(), first.into());
    }
    let facts = node_facts(app, &ask, INSTALLED_DEADLINE)
        .await
        .ok_or(ListFault::Node)?;
    let sidecars = strings(facts.get("sidecarNames"));
    let sidecar_loaded = strings(facts.get("sidecarLoaded"));
    let scan = match facts.get("scan") {
        JValue::Arr(files) if !files.is_empty() => Some(files.as_slice()),
        _ => None,
    };
    let long_loaded: HashSet<&str> = long_of
        .iter()
        .filter(|(l, _)| router_loaded.contains(**l))
        .map(|(_, b)| *b)
        .collect();
    let mtp = JValue::obj([
        ("supported", JValue::Bool(false)),
        ("enabled", JValue::Bool(false)),
        ("reason", JValue::Str(MTP_UNVERIFIED.to_owned())),
    ]);
    let list = rows
        .iter()
        .filter(listed)
        .filter_map(|row| {
            let id = id_of(row)?;
            let name = id.as_str();
            let file = file_for(scan, name);
            let sidecar = sidecars.contains(name.trim());
            let up = sidecar && sidecar_loaded.contains(name);
            let preset = string_at(row, "source").as_deref() == Some("preset");
            let no_file = scan.is_some() && preset && file.is_none();
            let system =
                crate::model_kind::is_system_model_at(&roles, name, model_path(&args_of(row)));
            let elsewhere =
                !up && preset && ((system && (scan.is_none() || no_file)) || (no_file && sidecar));
            let missing = !up && no_file && !elsewhere;
            let value = status_value(row).unwrap_or_else(|| "unknown".to_owned());
            let router_failed = matches!(
                row.get("status").and_then(|s| s.get("failed")),
                Some(Value::Bool(true))
            );
            let bytes = file
                .and_then(|f| f.get("bytes").as_num())
                .filter(|b| *b > 0.0);
            let size = match bytes {
                Some(b) => JValue::Num(round1(b / 1e9)),
                None => {
                    meta_number(row, "size").map_or(JValue::Null, |s| JValue::Num(round1(s / 1e9)))
                }
            };
            let shape = file
                .map(|f| f.get("shape"))
                .filter(|s| s.truthy())
                .cloned()
                .unwrap_or(JValue::Null);
            let status = if missing {
                "missing".to_owned()
            } else if elsewhere {
                "served-elsewhere".to_owned()
            } else if up {
                "loaded".to_owned()
            } else {
                value
            };
            let mut out: Vec<(String, JValue)> = Vec::with_capacity(20);
            let mut put = |k: &str, v: JValue| out.push((k.to_owned(), v));
            if let Some(long) = long_for.get(name) {
                put("longVariant", JValue::Str((*long).to_owned()));
                put("longLoaded", JValue::Bool(long_loaded.contains(name)));
            }
            put("name", JValue::Str(id.clone()));
            put("sizeGB", size);
            put("shape", shape);
            put("loaded", JValue::Bool(router_loaded.contains(name) || up));
            put("labels", labels_json(&native_labels(row)));
            put("system", JValue::Bool(system));
            put("mtp", mtp.clone());
            put(
                "maxContext",
                meta_number(row, "n_ctx_train")
                    .filter(|n| *n != 0.0)
                    .map_or(JValue::Null, JValue::Num),
            );
            put("suggested", JValue::Bool(false));
            put("status", JValue::Str(status));
            put("failed", JValue::Bool(!up && (router_failed || missing)));
            put("missingFile", JValue::Bool(missing));
            put("servedElsewhere", JValue::Bool(elsewhere));
            put(
                "canDelete",
                JValue::Bool(matches!(row.get("can_remove"), Some(Value::Bool(true)))),
            );
            put("sidecarProtected", JValue::Bool(sidecar));
            put(
                "source",
                string_at(row, "source").map_or(JValue::Null, JValue::Str),
            );
            Some(JValue::Obj(out))
        })
        .collect();
    Ok(list)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hash_ids_are_nodes_pattern() {
        assert!(hash_id(&"a".repeat(32)));
        assert!(hash_id(&"AbC0".repeat(10)));
        assert!(!hash_id(&"a".repeat(31)));
        assert!(!hash_id(&"a".repeat(41)));
        assert!(!hash_id(&format!("{}g", "a".repeat(32))));
    }

    #[test]
    fn a_lone_surrogate_survives_the_round_trip() {
        let t = text(&[0x61, 0xD800]);
        assert_eq!(
            wtf16::stringify(&JValue::Str(t)).unwrap(),
            "\"a\\ud800\"".to_owned()
        );
    }
}
