//! Replays `tests/fixtures/gdrive-tools-node.v1.json` through the front's Drive tools (gdrive_tools.rs).
//! The fixture is written by `tests/gdrive-tools-oracle.cjs` from core's REAL `gdrive-tools.cjs`,
//! `gdrive-files.cjs` and `gdrive-broker.cjs` (the checkout in `tests/fixtures/gdrive-tools-oracle.core-ref`;
//! CI regenerates it and compares). For every step: the answer, every HTTP request (method, address,
//! headers, body) and every refused sign-in must be core's. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use js_json::JValue;
use noevia_server::gdrive_tools::{
    describe_target, execute, Backend, Http, Refusal, Reply, Scope, Session, State, User,
};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

fn fixture() -> Value {
    let path = std::env::var("NOEVIA_GDRIVE_TOOLS_FIXTURE").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/gdrive-tools-node.v1.json"
        )
        .into()
    });
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn jv(v: &Value) -> JValue {
    js_json::parse(&v.to_string()).unwrap()
}

fn sv(v: &JValue) -> Value {
    serde_json::from_str(&js_json::stringify(v).unwrap()).unwrap()
}

struct Scripted {
    f: Value,
    now: f64,
    http: Mutex<VecDeque<Value>>,
    sessions: Mutex<VecDeque<Value>>,
    /// The broker's token counter per account (core's oracle broker rotates past a refused token).
    tokens: Mutex<HashMap<String, u32>>,
    requests: Mutex<Vec<Value>>,
    rejected: Mutex<Vec<Value>>,
}

impl Backend for Scripted {
    fn now_ms(&self) -> f64 {
        self.now
    }
    fn boundary(&self) -> String {
        self.f["boundary"].as_str().unwrap().into()
    }
    fn api(&self) -> String {
        self.f["api"].as_str().unwrap().into()
    }
    fn upload(&self) -> String {
        self.f["upload"].as_str().unwrap().into()
    }
    async fn connected(&self, user: &User) -> bool {
        self.f["states"][user.id.as_str()] == "connected"
    }
    async fn token(&self, user: &User, rejected: Option<&str>) -> Result<Session, Refusal> {
        if let Some(s) = self.sessions.lock().unwrap().pop_front() {
            if s == "throw" {
                return Err(Refusal::default());
            }
            let r = &s["refuse"];
            return Err(Refusal {
                status: r["status"].as_u64().map(|n| n as u16),
                error: r["error"].as_str().map(str::to_owned),
                code: r["code"].as_str().map(str::to_owned),
            });
        }
        let mut tokens = self.tokens.lock().unwrap();
        let n = tokens.entry(user.id.clone()).or_insert(1);
        let of = |n: u32| format!("qa-token-{}-{n}", user.id);
        if rejected == Some(of(*n).as_str()) {
            *n += 1;
        }
        Ok(Session {
            access: of(*n),
            generation: format!("gen-{}-{}", user.id, *n),
        })
    }
    async fn reject(&self, user: &User, generation: &str) {
        self.rejected
            .lock()
            .unwrap()
            .push(json!({"user": user.id, "admin": user.admin, "generation": generation}));
    }
    async fn send(&self, req: Http, bearer: &str) -> Result<Reply, ()> {
        let mut headers = Map::new();
        for (k, v) in &req.headers {
            headers.insert(k.clone(), json!(v));
        }
        headers.insert("Authorization".into(), json!(format!("Bearer {bearer}")));
        self.requests.lock().unwrap().push(json!({
            "method": req.method,
            "url": req.url,
            "headers": headers,
            "body": req.body.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()),
        }));
        let r = self
            .http
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted response");
        let status = u16::try_from(r["status"].as_u64().unwrap()).unwrap();
        let mut body: Vec<u8> = if let Some(j) = r.get("json") {
            j.to_string().into_bytes()
        } else if let Some(h) = r["hex"].as_str() {
            (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
                .collect()
        } else {
            r["text"].as_str().unwrap_or("").as_bytes().to_vec()
        };
        if status == 204 {
            body.clear();
        }
        let capped = body.len() > req.cap;
        body.truncate(req.cap);
        Ok(Reply {
            status,
            body,
            capped,
        })
    }
}

#[tokio::test]
async fn every_recorded_step_answers_as_cores_tools_did() {
    let f = fixture();
    assert_eq!(f["version"], 1);
    let pin = include_str!("fixtures/gdrive-tools-oracle.core-ref").trim();
    assert_eq!(f["coreRef"].as_str(), Some(pin));
    let mut total = 0;
    for scenario in f["scenarios"].as_array().unwrap() {
        let state = State::default();
        let tokens = Mutex::new(HashMap::new());
        for (n, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
            let at = format!("{} / step {n}", scenario["name"]);
            let script = &step["script"];
            let queue =
                |k: &str| Mutex::new(script[k].as_array().unwrap().iter().cloned().collect());
            let b = Scripted {
                f: f.clone(),
                now: step["at"].as_f64().unwrap(),
                http: queue("http"),
                sessions: queue("sessions"),
                tokens: Mutex::new(std::mem::take(&mut *tokens.lock().unwrap())),
                requests: Mutex::new(Vec::new()),
                rejected: Mutex::new(Vec::new()),
            };
            let r = &step["request"];
            let user = r["user"].as_object().map(|u| User {
                id: js_json::to_js_string(&jv(&u["id"])).unwrap(),
                admin: u["role"] == "admin",
            });
            let name = r["name"].as_str().unwrap();
            let args = jv(&r["arguments"]);
            let answer = if r["op"] == "execute" {
                let scope = Scope {
                    chat_key: r["chatKey"].as_str(),
                    exchange_key: r["exchangeKey"].as_str(),
                };
                let cap = r["cap"].as_u64().unwrap() as usize;
                json!(execute(&b, &state, user.as_ref(), name, &args, scope, cap).await)
            } else {
                sv(&describe_target(&b, user.as_ref(), name, &args).await)
            };
            assert_eq!(answer, step["answer"], "{at}");
            assert_eq!(
                Value::Array(b.requests.into_inner().unwrap()),
                step["requests"],
                "{at}: requests"
            );
            assert_eq!(
                Value::Array(b.rejected.into_inner().unwrap()),
                step["rejected"],
                "{at}: rejected"
            );
            assert!(
                b.http.into_inner().unwrap().is_empty(),
                "{at}: unused responses"
            );
            *tokens.lock().unwrap() = b.tokens.into_inner().unwrap();
            total += 1;
        }
    }
    assert!(total >= 99, "{total} steps");
}
