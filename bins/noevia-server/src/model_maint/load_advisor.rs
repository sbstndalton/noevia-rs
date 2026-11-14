//! The auto-tune load advisor (sbstndalton/noevia#2550 item 2), ported from core load-advisor.cjs
//! `createLoadAdvisor().judge` and the private decision client it uses (decision-endpoint.cjs
//! `createDecisionEndpoint({ env }).choice`, decision-models.cjs, decision-typesafe.cjs and
//! service-endpoint.cjs's connect-time address rule). Built once from the front's environment
//! (`COWORK_DECISION_URL`, `COWORK_DECISION_MODEL`, `COWORK_DECISION_API_KEY`), as Node's
//! `createLoadAdvisor()` reads `process.env`.
//!
//! `judge({ cause, evidence })`: Rust's load-verdict rules first (autotune_runner::decide); only
//! when they find nothing and there is engine text is the decision service asked which of the
//! planner's outcomes the text describes. The verdict uses that label only at a confidence of at
//! least 0.6; `classification.advisor` says what happened: `not_asked`, `unavailable` (no usable
//! URL), `busy` (another question is open), `timeout` (no answer within 2 s), `error` (a refusal,
//! a status, a bad answer), `used` or `ignored`. Nothing here loads, unloads or runs a model.
//!
//! The client: one question at a time per advisor; `POST <origin>/v1/decisions` with Node's body
//! (the TypeSafe body and a bearer key for `kev-0.8b`), no redirect followed, no environment
//! proxy, one connection per request, names resolved at connect time and refused unless every
//! answer is on this network (link-local allowed: the decision service's rule); a non-2xx status,
//! an empty or over-8 KiB body, text that is not JSON or an answer that fails Node's strict score
//! checks is an error. A dropped or refused connection gets one immediate retry. Every request
//! waits in its service's lane (one per origin, this process); once sent it holds the lane until
//! the service answers, at most 5 s, even when the asker has given up.
//!
//! Differences from Node, by design or necessity:
//! - The lane is this process's: Core's chat decisions wait in Core's lane, so a question that
//!   overlaps one of Core's gets the single-worker service's http-503, i.e. `advisor: 'error'` and
//!   the calibrator's own cause stands (Node's "no advice" outcome too). Nothing coordinates the
//!   two processes.
//! - A compressed reply (`Content-Encoding: gzip|deflate|br`), which Node's fetch decodes, is read
//!   as it came (and is not JSON, so `error`). The decision services send plain JSON.
//!
//! The API key is never logged, printed or put in an error.

use autotune_runner::decide;
use js_json::raw::{self, Top};
use js_json::JValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// load-advisor.cjs `ADVICE_BUDGET_MS`.
pub const ADVICE_BUDGET: Duration = Duration::from_millis(2000);
/// decision-endpoint.cjs `SLOT_HOLD_MAX_MS`.
pub const SLOT_HOLD_MAX: Duration = Duration::from_millis(5000);
/// decision-endpoint.cjs exchange's response cap.
const RESPONSE_CAP: usize = 8192;

const QUESTION: &str = "A local model server failed while loading or serving a model during automatic tuning. Which of these does its error text describe?";
/// load-advisor.cjs `OPTIONS` (never a time out: only a rule may name it, noevia#1046).
const OPTIONS: [(&str, &str); 5] = [
    (
        "oom",
        "Out of memory: memory for the model, its cache or its buffers could not be allocated",
    ),
    (
        "load_failed",
        "The model file or its format could not be loaded",
    ),
    (
        "recall_failed",
        "The prompt did not fit the context size the model was started with",
    ),
    (
        "template",
        "The model's chat template could not be parsed or applied",
    ),
    ("unknown", "None of these, or the text does not say"),
];

/// decision-models.cjs PROFILES['kev-0.8b']: the TypeSafe protocol, its served model id and its
/// calibration temperature.
const TYPESAFE_MODEL_ID: &str = "kev-0.8b";
const TYPESAFE_SERVED_MODEL: &str = "ollaya.dev/library/kev:0.8b";
const TYPESAFE_TEMPERATURE: f64 = 0.45;
/// decision-typesafe.cjs `TIE_EPSILON`.
const TIE_EPSILON: f64 = 1e-9;

/// What `ask` came back with (load-advisor.cjs: `{ status, advice? }`).
enum Asked {
    Unavailable,
    Busy,
    Timeout,
    Error,
    Answered { label: String, confidence: f64 },
}

impl Asked {
    fn status(&self) -> &'static str {
        match self {
            Asked::Unavailable => "unavailable",
            Asked::Busy => "busy",
            Asked::Timeout => "timeout",
            Asked::Error => "error",
            Asked::Answered { .. } => "answered",
        }
    }
}

/// A failed exchange. The reason is Node's text-free cause code; only `transient` changes what
/// happens (one retry), the advisor reports every failure as `error`.
#[derive(Debug)]
struct Failure {
    #[allow(dead_code)]
    reason: &'static str,
    transient: bool,
}

fn failure(reason: &'static str) -> Failure {
    Failure {
        reason,
        transient: false,
    }
}

/// The configured decision service. No `Debug`: it holds the key.
struct Endpoint {
    url: String,
    typesafe: bool,
    key: Option<String>,
    client: reqwest::Client,
    lane: Arc<tokio::sync::Mutex<()>>,
}

/// One lane per service origin for the whole process (decision-endpoint.cjs `laneFor`): every
/// advisor made for the same URL waits in the same queue.
fn lane_for(base_url: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LANES: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let lanes = LANES.get_or_init(Default::default);
    match lanes.lock() {
        Ok(mut map) => Arc::clone(
            map.entry(base_url.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        ),
        Err(_) => Arc::new(tokio::sync::Mutex::new(())),
    }
}

impl Endpoint {
    /// decision-endpoint.cjs `createDecisionEndpoint({ env })`: `None` when the URL is unset or not
    /// a private service address (Node: `null`, so `unavailable`).
    fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Option<Endpoint> {
        let raw = get("COWORK_DECISION_URL").unwrap_or_default();
        let parsed = crate::decision_settings_routes::parse_service_url(&raw, true)?;
        // profileFor: `id.trim().toLowerCase()`, unknown or unset is the default profile.
        let typesafe = get("COWORK_DECISION_MODEL").is_some_and(|id| {
            crate::decision_settings_routes::js_trim(&id).to_lowercase() == TYPESAFE_MODEL_ID
        });
        // KEY_RE `^[\x21-\x7e]{1,256}$` on the trimmed key, and only for a TypeSafe server.
        let key = if typesafe {
            get("COWORK_DECISION_API_KEY")
                .map(|k| crate::decision_settings_routes::js_trim(&k).to_owned())
                .filter(|k| {
                    (1..=256).contains(&k.len()) && k.bytes().all(|b| (0x21..=0x7e).contains(&b))
                })
        } else {
            None
        };
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            // Node's service fetch uses `agent: false`: a new connection for every request.
            .pool_max_idle_per_host(0)
            .dns_resolver(Arc::new(crate::decision_settings_routes::LocalOnly {
                link_local: true,
            }))
            .build()
            .ok()?;
        Some(Endpoint {
            url: format!("{}/v1/decisions", parsed.base_url),
            typesafe,
            key,
            client,
            lane: lane_for(&parsed.base_url),
        })
    }

    /// The request body for the advisor's question (`choice({ state, question, options })`).
    fn body(&self, text: &str) -> String {
        let state = js_json::stringify(&JValue::obj([("engineError", JValue::from(text))]))
            .unwrap_or_default();
        let value = if self.typesafe {
            // decision-typesafe.cjs toTypeSafe.
            JValue::obj([
                ("model", JValue::from(TYPESAFE_SERVED_MODEL)),
                ("state", JValue::from(state)),
                (
                    "questions",
                    JValue::obj([(
                        "d",
                        JValue::obj([
                            ("type", JValue::from("choice")),
                            ("instructions", JValue::from(QUESTION)),
                            (
                                "criteria",
                                JValue::obj(OPTIONS.iter().map(|(id, l)| (*id, JValue::from(*l)))),
                            ),
                        ]),
                    )]),
                ),
            ])
        } else {
            JValue::obj([
                ("state", JValue::from(state)),
                ("question", JValue::from(QUESTION)),
                (
                    "options",
                    JValue::Arr(
                        OPTIONS
                            .iter()
                            .map(|(id, l)| {
                                JValue::obj([
                                    ("id", JValue::from(*id)),
                                    ("label", JValue::from(*l)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ])
        };
        js_json::stringify(&value).unwrap_or_default()
    }

    /// `choice(...)`: waits for the lane, sends (once more on a transient connect failure), and
    /// reads the answer as `(selected, scores[selected])`. Dropping this future (the asker's
    /// `deadline`) leaves the queue, or, once sent, leaves the exchange to finish in the lane.
    async fn choice(
        &self,
        text: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(String, f64), Failure> {
        let body = self.body(text);
        let guard = Arc::clone(&self.lane).lock_owned().await;
        // #1592: a turn granted as the asker's deadline passed sends nothing; the lane goes on.
        if tokio::time::Instant::now() >= deadline {
            return Err(failure("aborted"));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let client = self.client.clone();
        let url = self.url.clone();
        let key = self.key.clone();
        tokio::spawn(async move {
            let sent = tokio::time::timeout(SLOT_HOLD_MAX, async {
                match exchange(&client, &url, &body, key.as_deref()).await {
                    // A caller that has gone gets no retry (Node: `callerSignal.aborted`).
                    Err(e) if e.transient && !tx.is_closed() => {
                        exchange(&client, &url, &body, key.as_deref()).await
                    }
                    other => other,
                }
            })
            .await
            .unwrap_or(Err(failure("aborted")));
            drop(guard);
            let _ = tx.send(sent);
        });
        let text = rx.await.unwrap_or(Err(failure("aborted")))?;
        if crate::decision_settings_routes::js_trim(&text).is_empty() {
            return Err(failure("empty-response"));
        }
        let top = raw::read(&text).ok_or(failure("parse"))?;
        let (selected, scores) = if self.typesafe {
            let (selected, scores) = from_typesafe(&top).ok_or(failure("invalid-result"))?;
            (selected, tempered(scores, TYPESAFE_TEMPERATURE))
        } else {
            laya_answer(&top).ok_or(failure("invalid-result"))?
        };
        let confidence = checked(&selected, &scores).ok_or(failure("invalid-result"))?;
        Ok((selected, confidence))
    }
}

/// One POST; the response text (lossy UTF-8, as Node's `Buffer.toString('utf8')`), size-capped.
async fn exchange(
    client: &reqwest::Client,
    url: &str,
    body: &str,
    key: Option<&str>,
) -> Result<String, Failure> {
    let mut request = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let mut response = request
        .body(body.to_owned())
        .send()
        .await
        .map_err(|e| Failure {
            reason: "network",
            transient: transient(&e),
        })?;
    let status = response.status().as_u16();
    // public-fetch.cjs sendWithLookup refuses a redirect: a network failure, never retried.
    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        return Err(failure("network"));
    }
    if !(200..=299).contains(&status) {
        return Err(failure("http"));
    }
    // A null-body status (Node: `response.body` is null).
    if matches!(status, 204 | 205) {
        return Err(failure("empty-response"));
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len() + chunk.len() > RESPONSE_CAP {
                    return Err(failure("response-too-large"));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return Err(failure("read")),
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// decision-endpoint.cjs TRANSIENT_CODES: a reset, refused, broken or aborted socket, or a
/// connection closed before any response (Node's "socket hang up", ECONNRESET). A refusal by the
/// address rule, a name that does not resolve or a TLS error is the answer.
fn transient(e: &reqwest::Error) -> bool {
    if e.is_timeout() {
        return false;
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
            ) {
                return true;
            }
        }
        if let Some(h) = err.downcast_ref::<hyper::Error>() {
            if h.is_incomplete_message() {
                return true;
            }
        }
        source = err.source();
    }
    false
}

fn field<'a>(fields: &'a [raw::Field], name: &str) -> Option<&'a raw::Field> {
    fields.iter().find(|f| raw::is_key(&f.key, name))
}

/// A Laya answer `{ selected, scores }` (scores in the object's own order); `None` where Node's
/// checks fail or throw.
fn laya_answer(top: &Top) -> Option<(String, Vec<(String, JValue)>)> {
    let Top::Object(fields) = top else {
        return None;
    };
    let selected = raw::string_of(&field(fields, "selected")?.value)?;
    match raw::tree(&field(fields, "scores")?.value)? {
        JValue::Obj(items) => Some((selected, items)),
        _ => None,
    }
}

/// decision-typesafe.cjs `fromTypeSafe(json, ids)`: `answers.d.probabilities` over exactly the
/// option ids, renormalised, the strict winner selected (an exact tie is refused).
fn from_typesafe(top: &Top) -> Option<(String, Vec<(String, JValue)>)> {
    let Top::Object(fields) = top else {
        return None;
    };
    let Top::Object(answers) = raw::read(&field(fields, "answers")?.value)? else {
        return None;
    };
    let Top::Object(d) = raw::read(&field(&answers, "d")?.value)? else {
        return None;
    };
    let JValue::Obj(probs) = raw::tree(&field(&d, "probabilities")?.value)? else {
        return None;
    };
    if probs.len() != OPTIONS.len() {
        return None;
    }
    let mut values = Vec::with_capacity(OPTIONS.len());
    for (id, _) in OPTIONS {
        let v = probs.iter().find(|(k, _)| k == id)?.1.as_num()?;
        if !(v.is_finite() && v >= 0.0) {
            return None;
        }
        values.push(v);
    }
    let mut total = 0.0;
    for v in &values {
        total += v;
    }
    if total.is_nan() || total <= 0.0 {
        return None;
    }
    let scores: Vec<(String, f64)> = OPTIONS
        .iter()
        .zip(&values)
        .map(|((id, _), v)| ((*id).to_owned(), v / total))
        .collect();
    let mut ranked = scores.clone();
    // Array.prototype.sort is stable, as is sort_by.
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let first = ranked.first()?;
    if let Some(second) = ranked.get(1) {
        if first.1 - second.1 < TIE_EPSILON {
            return None;
        }
    }
    Some((
        first.0.clone(),
        scores
            .into_iter()
            .map(|(id, s)| (id, JValue::Num(s)))
            .collect(),
    ))
}

/// decision-models.cjs `tempered(scores, T)`: `max(s, 1e-9) ** (1 / T)`, renormalised.
fn tempered(scores: Vec<(String, JValue)>, temperature: f64) -> Vec<(String, JValue)> {
    let exponent = 1.0 / temperature;
    let w: Vec<f64> = scores
        .iter()
        .map(|(_, s)| s.as_num().unwrap_or(f64::NAN).max(1e-9).powf(exponent))
        .collect();
    let mut z = 0.0;
    for x in &w {
        z += x;
    }
    scores
        .into_iter()
        .zip(w)
        .map(|((id, _), w)| (id, JValue::Num(w / z)))
        .collect()
}

/// decision-endpoint.cjs choice's result checks: the selected id is an option; one score per
/// option, each a number in [0, 1], summing to 1 within 0.002 (in the object's order); the
/// selected one the strict maximum. `Some(scores[selected])` when they hold.
fn checked(selected: &str, scores: &[(String, JValue)]) -> Option<f64> {
    if !OPTIONS.iter().any(|(id, _)| *id == selected) || scores.len() != OPTIONS.len() {
        return None;
    }
    let score = |id: &str| scores.iter().find(|(k, _)| k == id)?.1.as_num();
    for (id, _) in OPTIONS {
        let s = score(id)?;
        if !(s.is_finite() && (0.0..=1.0).contains(&s)) {
            return None;
        }
    }
    let mut sum = 0.0;
    for (_, v) in scores {
        sum += v.as_num()?;
    }
    if (sum - 1.0).abs() > 0.002 {
        return None;
    }
    let mine = score(selected)?;
    if OPTIONS
        .iter()
        .any(|(id, _)| *id != selected && score(id).is_some_and(|s| s >= mine))
    {
        return None;
    }
    Some(mine)
}

/// Clears the advisor's open question when `ask` ends, however it ends.
struct Open<'a>(&'a AtomicBool);

impl Drop for Open<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// load-advisor.cjs `createLoadAdvisor({ env })`.
pub struct LoadAdvisor {
    endpoint: Option<Endpoint>,
    open: AtomicBool,
}

impl std::fmt::Debug for LoadAdvisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadAdvisor")
            .field("configured", &self.endpoint.is_some())
            .finish()
    }
}

impl LoadAdvisor {
    /// From the environment Node's advisor reads (`COWORK_DECISION_URL`, `COWORK_DECISION_MODEL`,
    /// `COWORK_DECISION_API_KEY`).
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Self {
        LoadAdvisor {
            endpoint: Endpoint::from_lookup(get),
            open: AtomicBool::new(false),
        }
    }

    /// The decision service's label and confidence for the engine text, or why there is none.
    async fn ask(&self, text: &str) -> Asked {
        let Some(endpoint) = &self.endpoint else {
            return Asked::Unavailable;
        };
        if self.open.swap(true, Ordering::SeqCst) {
            return Asked::Busy;
        }
        let _open = Open(&self.open);
        let deadline = tokio::time::Instant::now() + ADVICE_BUDGET;
        match tokio::time::timeout_at(deadline, endpoint.choice(text, deadline)).await {
            Err(_) => Asked::Timeout,
            // Node: an error once its signal was aborted (the budget) reads as a timeout.
            Ok(Err(_)) if tokio::time::Instant::now() >= deadline => Asked::Timeout,
            Ok(Err(_)) => Asked::Error,
            Ok(Ok((label, confidence))) => Asked::Answered {
                label,
                confidence: confidence.clamp(0.0, 1.0),
            },
        }
    }

    /// load-advisor.cjs `judge({ cause, evidence })`: `{ outcome, reason, classification }`, or
    /// `null` when the verdict is unavailable (the caller keeps its own outcome). Never fails.
    pub async fn judge(&self, request: &JValue) -> JValue {
        let request = decide::load_request(request);
        let Some(mut v) = decide::load_verdict(&request) else {
            return JValue::Null;
        };
        let mut advisor = "not_asked";
        if v.get("ask").truthy() {
            let text = request
                .get("evidence")
                .get("text")
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let asked = self.ask(&text).await;
            advisor = asked.status();
            if let Asked::Answered { label, confidence } = asked {
                match decide::load_verdict(&decide::with_advice(&request, &label, confidence)) {
                    Some(advised) => {
                        advisor = if advised.get("adviceUsed").truthy() {
                            "used"
                        } else {
                            "ignored"
                        };
                        v = advised;
                    }
                    None => advisor = "error",
                }
            }
        }
        decide::judgement(&v, advisor)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Every replay scenario names its service by an IP literal; a host name goes through the
    /// connect-time address check (`LocalOnly`) and is asked like any other local service.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_local_service_named_by_host_name_is_asked() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new().fallback(|| async {
            (
                [("content-type", "application/json")],
                r#"{"selected":"oom","scores":{"oom":0.7,"load_failed":0.1,"recall_failed":0.1,"template":0.05,"unknown":0.05}}"#,
            )
        });
        tokio::spawn(async move { axum::serve(listener, app).await });
        let url = format!("http://localhost:{port}");
        let advisor =
            LoadAdvisor::from_lookup(&|k: &str| (k == "COWORK_DECISION_URL").then(|| url.clone()));
        let request = js_json::wtf16::parse(
            r#"{"cause":"load","evidence":{"status":500,"exitCode":1,"text":"qa-engine stopped: unexpected state 42 in worker qa-1","crash":false}}"#,
        )
        .unwrap();
        let out = advisor.judge(&request).await;
        assert_eq!(
            out.get("classification").get("advisor").as_str(),
            Some("used"),
            "{out:?}"
        );
    }
}
