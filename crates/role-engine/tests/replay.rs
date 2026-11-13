//! Every row of tests/fixtures/role-engine.v1.json, recorded from noevia-core's role-engine.cjs by
//! tools/gen-fixtures.cjs, replayed through the port and compared as JSON text in the fixture's
//! tagged form: the pure functions row by row, and each scenario's results, the requests its stubs
//! saw (url, method, headers, exact body bytes, reads, cancel), the dependency calls in order and
//! the log entries. The scenarios run through [`role_engine::block_on`], as a blocking caller would.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::{wtf16, JValue};
use role_engine::*;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

const TAG: &str = "$js";

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/role-engine.v1.json")).expect("fixture JSON")
}

fn arr(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

fn s(v: &JValue) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

/// The fixture's tagged form back to JS values.
fn decode(v: &JValue) -> JValue {
    match v {
        JValue::Arr(items) => JValue::Arr(items.iter().map(decode).collect()),
        JValue::Obj(items) => match v.get(TAG).as_str() {
            Some("undefined") => JValue::Undefined,
            Some("NaN") => JValue::Num(f64::NAN),
            Some("Infinity") => JValue::Num(f64::INFINITY),
            Some("-Infinity") => JValue::Num(f64::NEG_INFINITY),
            Some(other) => panic!("unknown tag {other}"),
            None => JValue::Obj(items.iter().map(|(k, x)| (k.clone(), decode(x))).collect()),
        },
        other => other.clone(),
    }
}

/// JS values to the tagged form, as JSON text.
fn text(v: &JValue) -> String {
    fn enc(v: &JValue) -> JValue {
        let tag = |t: &str| JValue::obj([(TAG, t.into())]);
        match v {
            JValue::Undefined => tag("undefined"),
            JValue::Num(n) if n.is_nan() => tag("NaN"),
            JValue::Arr(items) => JValue::Arr(items.iter().map(enc).collect()),
            JValue::Obj(items) => {
                JValue::Obj(items.iter().map(|(k, x)| (k.clone(), enc(x))).collect())
            }
            other => other.clone(),
        }
    }
    stringify(&enc(v))
}

fn base64(t: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let digits: Vec<u8> = t.bytes().filter(|c| *c != b'=').map(val).collect();
    let mut out = Vec::new();
    for c in digits.chunks(4) {
        let n = c
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, d)| n | (u32::from(*d) << (18 - 6 * i)));
        out.extend_from_slice(&n.to_be_bytes()[1..c.len()]);
    }
    out
}

fn chunk_bytes(c: &JValue) -> Vec<u8> {
    match c {
        JValue::Str(t) => t.as_bytes().to_vec(),
        other => base64(other.get("b64").as_str().unwrap()),
    }
}

#[test]
fn sources_match_the_recorded_node_files() {
    let f = fixture();
    for (name, sha) in SOURCES {
        assert_eq!(f.get("sources").get(name).as_str(), Some(sha), "{name}");
    }
}

#[test]
fn pure_rows() {
    let f = fixture();
    let mut n = 0;
    for row in arr(f.get("pure")) {
        let a = decode(row.get("args"));
        let got = match row.get("fn").as_str().unwrap() {
            "schemaConstraint" => schema_constraint(
                a.get("enabled") == &JValue::Bool(true),
                a.get("provider"),
                &s(a.get("model")),
                a.get("thinking") == &JValue::Bool(true),
                a.get("schema"),
                &s(a.get("name")),
            )
            .to_json(),
            "buildMessages" => {
                let c = a.get("correction");
                build_messages(
                    &s(a.get("dossier")),
                    &s(a.get("instructions")),
                    &s(a.get("fields")),
                    (!c.is_nullish()).then_some(c),
                )
            }
            "prefixHash" => prefix_hash(
                &s(a.get("model")),
                a.get("thinking") == &JValue::Bool(true),
                &s(a.get("dossier")),
            )
            .into(),
            "loadedFromModelList" => {
                let keep: Vec<String> = arr(a.get("keep")).iter().map(s).collect();
                let ids = loaded_from_model_list(
                    a.get("body"),
                    &keep,
                    &section_roles_of(&s(a.get("roles"))),
                );
                JValue::Arr(ids.into_iter().map(JValue::from).collect())
            }
            "isNonChatRow" => {
                is_non_chat_row(&section_roles_of(&s(a.get("roles"))), a.get("row")).into()
            }
            "sseDeltas" => sse(arr(a.get("chunks")), a.get("take").as_num()),
            other => panic!("unknown fn {other}"),
        };
        assert_eq!(
            text(&got),
            text(row.get("out")),
            "{} {}",
            row.get("fn").as_str().unwrap(),
            text(row.get("args"))
        );
        n += 1;
    }
    assert!(n > 400, "{n} rows");
}

/// `for await (const d of sseDeltas(...))` with a consumer that stops after `take` deltas.
fn sse(chunks: &[JValue], take: Option<f64>) -> JValue {
    let clock = std::cell::Cell::new(1000.0);
    let now = || {
        clock.set(clock.get() + 5.0);
        clock.get()
    };
    let mut m = Meter {
        start: now(),
        ..Meter::default()
    };
    let mut dec = SseDecoder::default();
    let (mut deltas, mut error, mut reads, mut i) = (Vec::new(), JValue::Null, 0, 0);
    loop {
        match dec.next(&mut m, &now) {
            None => {
                reads += 1;
                match chunks.get(i) {
                    Some(c) => dec.push(&chunk_bytes(c), &mut m),
                    None => {
                        m.complete = true;
                        break;
                    }
                }
                i += 1;
            }
            Some(SseItem::Done) => break,
            Some(SseItem::EngineError) => {
                error = "engine_error".into();
                break;
            }
            Some(SseItem::Delta(d)) => {
                deltas.push(JValue::from(d));
                if take.is_some_and(|t| deltas.len() as f64 >= t) {
                    break;
                }
            }
        }
    }
    let meter = JValue::obj([
        ("start", JValue::Num(m.start)),
        ("bytes", JValue::Num(m.bytes as f64)),
        ("deltas", JValue::Num(m.deltas as f64)),
        ("reasoningDeltas", JValue::Num(m.reasoning_deltas as f64)),
        (
            "firstDeltaMs",
            m.first_delta_ms.map_or(JValue::Null, JValue::Num),
        ),
        ("complete", m.complete.into()),
        ("timings", JValue::Obj(m.timings)),
    ]);
    JValue::obj([
        ("deltas", JValue::Arr(deltas)),
        ("error", error),
        ("m", meter),
        ("reads", JValue::Num(f64::from(reads))),
        ("cancelled", true.into()),
        ("clock", JValue::Num(clock.get())),
    ])
}

// ── Scenario stubs ─────────────────────────────────────────────────────────────────────────

/// `list[min(i++, len - 1)]`, undefined for an empty or missing list.
struct Seq(Mutex<(Vec<JValue>, usize)>);

impl Seq {
    fn of(v: &JValue) -> Option<Seq> {
        (!v.is_nullish()).then(|| Seq(Mutex::new((arr(v).to_vec(), 0))))
    }
    fn next(&self) -> JValue {
        let mut g = self.0.lock().unwrap();
        let (list, i) = &mut *g;
        let v = list
            .get((*i).min(list.len().saturating_sub(1)))
            .cloned()
            .unwrap_or(JValue::Undefined);
        *i += 1;
        v
    }
}

type Record = Arc<Mutex<Vec<(String, JValue)>>>;

struct Stub {
    sc: JValue,
    calls: Mutex<Vec<String>>,
    log: Mutex<Vec<JValue>>,
    requests: Mutex<Vec<Record>>,
    clock: Mutex<f64>,
    engines: Seq,
    budgets: Option<Seq>,
    admissions: Option<Seq>,
    loadeds: Option<Seq>,
    readies: Option<Seq>,
    queues: [Mutex<VecDeque<JValue>>; 2],
    current: Arc<Mutex<Cancel>>,
}

struct StubBody {
    chunks: VecDeque<JValue>,
    rec: Option<Record>,
    current: Arc<Mutex<Cancel>>,
}

fn bump(rec: &Option<Record>, key: &str, f: impl FnOnce(&JValue) -> JValue) {
    if let Some(r) = rec {
        let mut r = r.lock().unwrap();
        if let Some(slot) = r.iter_mut().find(|(k, _)| k == key) {
            slot.1 = f(&slot.1);
        }
    }
}

impl Body for StubBody {
    async fn read(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        bump(&self.rec, "reads", |n| {
            JValue::Num(n.as_num().unwrap() + 1.0)
        });
        while let Some(c) = self.chunks.pop_front() {
            if c.get("abort").truthy() {
                self.current.lock().unwrap().cancel();
                continue;
            }
            return Ok(Some(chunk_bytes(&c)));
        }
        Ok(None)
    }
    fn cancel(&mut self) {
        bump(&self.rec, "cancelled", |_| true.into());
    }
}

impl Stub {
    fn new(sc: &JValue) -> Stub {
        let q = |k: &str| Mutex::new(arr(sc.get(k)).iter().cloned().collect());
        let engines = Seq::of(sc.get("engine")).unwrap_or_else(|| {
            Seq(Mutex::new((
                vec![JValue::obj([
                    ("baseUrl", "http://engine.invalid/v1/".into()),
                    ("apiKey", "qa-key".into()),
                    ("model", "qa-coder-q4".into()),
                    ("provider", JValue::Null),
                ])],
                0,
            )))
        });
        Stub {
            sc: sc.clone(),
            calls: Mutex::default(),
            log: Mutex::default(),
            requests: Mutex::default(),
            clock: Mutex::new(1000.0),
            engines,
            budgets: Seq::of(sc.get("budget")),
            admissions: Seq::of(sc.get("admission")),
            loadeds: Seq::of(sc.get("loadedModels")),
            readies: Seq::of(sc.get("machines").get("ready")),
            queues: [q("fetch"), q("machineFetch")],
            current: Arc::new(Mutex::new(Cancel::new())),
        }
    }

    fn call(&self, c: String) {
        self.calls.lock().unwrap().push(c);
    }

    fn transport(&self, which: usize, req: Request) -> Result<Response<StubBody>, TransportError> {
        let headers = JValue::Obj(
            req.headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone().into()))
                .collect(),
        );
        let rec: Record = Arc::new(Mutex::new(vec![
            (
                "transport".into(),
                if which == 0 { "engine" } else { "machine" }.into(),
            ),
            ("url".into(), req.url.into()),
            ("method".into(), req.method.into()),
            ("headers".into(), headers),
            ("body".into(), req.body.map_or(JValue::Null, JValue::from)),
            ("reads".into(), JValue::Num(0.0)),
            ("cancelled".into(), false.into()),
        ]));
        self.requests.lock().unwrap().push(rec.clone());
        let next = self.queues[which]
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_default()?;
        if next.get("throw").truthy() {
            return Err(TransportError::default());
        }
        if next.get("abortThenThrow").truthy() {
            self.current.lock().unwrap().cancel();
            return Err(TransportError::default());
        }
        let status = next.get("status").as_num().unwrap() as u16;
        let current = self.current.clone();
        if let JValue::Arr(chunks) = next.get("chunks") {
            return Ok(Response {
                status,
                body: StubBody {
                    chunks: chunks.iter().cloned().collect(),
                    rec: Some(rec),
                    current,
                },
            });
        }
        let t = match next.get("text") {
            JValue::Str(t) => t.clone(),
            _ => stringify(next.get("json")),
        };
        Ok(Response {
            status,
            body: StubBody {
                chunks: VecDeque::from([JValue::Str(t)]),
                rec: None,
                current,
            },
        })
    }
}

trait OkOrDefault<T> {
    fn ok_or_default(self) -> Result<T, TransportError>;
}
impl<T> OkOrDefault<T> for Option<T> {
    fn ok_or_default(self) -> Result<T, TransportError> {
        self.ok_or_else(TransportError::default)
    }
}

fn opt_str(v: &JValue) -> Option<String> {
    v.as_str().map(str::to_owned)
}

impl Deps for Stub {
    type Body = StubBody;
    fn engine(&self) -> Option<Endpoint> {
        self.call("engine".into());
        let e = self.engines.next();
        if e.as_str() == Some("throw") {
            return None;
        }
        Some(Endpoint {
            base_url: opt_str(e.get("baseUrl")),
            api_key: opt_str(e.get("apiKey")),
            model: opt_str(e.get("model")),
            provider: e.get("provider").clone(),
            external: e.get("external") == &JValue::Bool(true),
            machine: (!e.get("machine").is_nullish()).then(|| e.get("machine").clone()),
        })
    }
    async fn fetch(
        &self,
        req: Request,
        _cancel: Cancel,
    ) -> Result<Response<StubBody>, TransportError> {
        self.transport(0, req)
    }
    async fn keep(&self) -> Option<Vec<String>> {
        self.call("keep".into());
        match self.sc.get("keep") {
            JValue::Str(_) => None,
            JValue::Arr(k) => Some(k.iter().filter_map(opt_str).collect()),
            _ => Some(vec!["qa-rerank".into()]),
        }
    }
    async fn loaded_models(&self) -> Option<Result<JValue, ()>> {
        let seq = self.loadeds.as_ref()?;
        self.call("loadedModels".into());
        let v = seq.next();
        Some(Ok(if v.as_str() == Some("notArray") {
            JValue::obj([("ids", JValue::Arr(Vec::new()))])
        } else {
            v
        }))
    }
    async fn admit<'a, T: Send + 'a>(
        &'a self,
        work: Work<'a, T>,
        cancel: Option<Cancel>,
    ) -> Result<T, AdmissionError> {
        let Some(seq) = &self.admissions else {
            return Ok(work.await);
        };
        self.call(format!(
            "admit:{}",
            if cancel.is_some() { "signal" } else { "none" }
        ));
        match seq.next().as_str() {
            Some("throw429") => Err(AdmissionError { status: Some(429) }),
            Some("throw") => Err(AdmissionError { status: None }),
            Some("abort") => {
                self.current.lock().unwrap().cancel();
                Ok(work.await)
            }
            _ => Ok(work.await),
        }
    }
    async fn budget_refusal(&self, model: &str) -> Option<BudgetRefusal> {
        let seq = self.budgets.as_ref()?;
        self.call(format!("budget:{model}"));
        let v = seq.next();
        v.is_object().then(|| BudgetRefusal {
            error: opt_str(v.get("error")).filter(|e| !e.is_empty()),
        })
    }
    fn has_machines(&self) -> bool {
        !self.sc.get("machines").is_nullish()
    }
    async fn machine_ready(&self, target: &JValue) -> Result<MachineReady, ()> {
        self.call(format!("ready:{}", s(target.get("baseUrl"))));
        let v = self.readies.as_ref().map_or(JValue::Undefined, Seq::next);
        match v.as_str() {
            Some("abort") => {
                self.current.lock().unwrap().cancel();
                Ok(MachineReady {
                    ok: true,
                    code: None,
                    error: None,
                })
            }
            _ if !v.is_object() => Err(()),
            _ => Ok(MachineReady {
                ok: v.get("ok").truthy(),
                code: opt_str(v.get("code")),
                error: opt_str(v.get("error")),
            }),
        }
    }
    async fn machine_fetch(
        &self,
        req: Request,
        _cancel: Cancel,
    ) -> Result<Response<StubBody>, TransportError> {
        self.transport(1, req)
    }
    fn machine_connect_error(&self, target: &JValue, _e: &TransportError) -> Option<String> {
        if !self.sc.get("machines").get("connectError").truthy() {
            return None;
        }
        self.call("connectError".into());
        Some(format!(
            "QA {} is unreachable.",
            s(target.get("machine").get("name"))
        ))
    }
    fn log(&self, entry: JValue) {
        self.log.lock().unwrap().push(entry);
    }
    fn now(&self) -> f64 {
        let mut c = self.clock.lock().unwrap();
        *c += 5.0;
        *c
    }
}

fn budgets(v: &JValue) -> HashMap<String, RoleBudget> {
    let JValue::Obj(items) = v else {
        return HashMap::new();
    };
    items
        .iter()
        .map(|(k, b)| {
            (
                k.clone(),
                RoleBudget {
                    max_tokens: b.get("maxTokens").as_num(),
                    max_bytes: b.get("maxBytes").as_num(),
                },
            )
        })
        .collect()
}

async fn run(sc: &JValue) -> Vec<(&'static str, JValue)> {
    let engine = RoleEngine::with_role_defaults(Stub::new(sc), budgets(sc.get("roleDefaults")));
    let mut session = None;
    let mut results = Vec::new();
    for step in arr(sc.get("steps")).iter().map(decode) {
        let c = Cancel::new();
        *engine.deps().current.lock().unwrap() = c.clone();
        if step.get("abort").as_str() == Some("before") {
            c.cancel();
        }
        let cancel = (!step.get("abort").is_nullish()).then_some(c);
        match step.get("op").as_str().unwrap() {
            "pin" => {
                let req = PinRequest {
                    task_id: s(step.get("taskId")),
                    model: opt_str(step.get("model")),
                    thinking: step.get("thinking") == &JValue::Bool(true),
                    roles: matches!(step.get("roles"), JValue::Arr(_))
                        .then(|| arr(step.get("roles")).iter().map(s).collect()),
                    cancel,
                };
                match engine.pin_model(req).await {
                    Ok(sess) => {
                        results.push(JValue::obj([
                            ("ok", true.into()),
                            ("session", sess.to_json()),
                        ]));
                        session = Some(sess);
                    }
                    Err(f) => results.push(f.to_json()),
                }
            }
            "call" => {
                let mut req = CallRequest::new(
                    &s(step.get("role")),
                    step.get("state").clone(),
                    &s(step.get("instructions")),
                    step.get("schema").clone(),
                );
                req.constrain = step.get("constrain") == &JValue::Bool(true);
                req.schema_name = opt_str(step.get("schemaName"));
                req.max_tokens = step.get("maxTokens").as_num();
                req.max_bytes = step.get("maxBytes").as_num();
                req.cancel = cancel;
                let r = session.as_ref().expect("a pinned session").call(req).await;
                results.push(r.map_or_else(|f| f.to_json(), |a| a.to_json()));
            }
            _ => results.push(JValue::Arr(session.as_ref().expect("a session").timings())),
        }
    }
    let d = engine.deps();
    let requests = d
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| JValue::Obj(r.lock().unwrap().clone()))
        .collect();
    let calls = d
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| JValue::from(c.clone()))
        .collect();
    let log = d.log.lock().unwrap().clone();
    vec![
        ("results", JValue::Arr(results)),
        ("requests", JValue::Arr(requests)),
        ("calls", JValue::Arr(calls)),
        ("log", JValue::Arr(log)),
    ]
}

#[test]
fn scenarios_replay_node() {
    let f = fixture();
    let JValue::Obj(scenarios) = f.get("scenarios") else {
        panic!("no scenarios")
    };
    let mut failures = Vec::new();
    for (name, sc) in scenarios {
        for (key, got) in block_on(run(sc)).expect("a runtime") {
            let want = sc.get(key);
            if text(&got) != text(want) {
                failures.push(format!(
                    "{name}.{key}\n  node: {}\n  rust: {}",
                    text(want),
                    text(&got)
                ));
            }
        }
    }
    assert!(scenarios.len() >= 55, "{} scenarios", scenarios.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
