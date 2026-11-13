//! Replays `tests/fixtures/mcp-call-node.v1.json` through the front's tool-call executor (mcp_call.rs).
//! The fixture is written by `tests/mcp-call-oracle.cjs` from core's REAL `executeMcpToolCall` (the
//! checkout in `tests/fixtures/mcp-call-oracle.core-ref`; CI regenerates it and compares). For every
//! call: the text the model gets, every transport round (address, headers, tool, arguments) and every
//! broker question must be core's. Synthetic data only.
#![cfg(feature = "rust-storage-reads")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use js_json::JValue;
use noevia_server::mcp_call::{
    execute, mint_call_token, Backend, Credential, Failed, Request, Session,
};
use noevia_server::mcp_discovery_actor::ServerSpec;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Mutex;
use tool_rate_limit::RateLimit;

fn fixture() -> Value {
    let path = std::env::var("NOEVIA_MCP_CALL_FIXTURE").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mcp-call-node.v1.json"
        )
        .into()
    });
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn jv(v: &Value) -> JValue {
    js_json::parse(&v.to_string()).unwrap()
}

fn sv(v: &JValue) -> Value {
    match v {
        JValue::Undefined => Value::Null,
        v => serde_json::from_str(&js_json::stringify(v).unwrap()).unwrap(),
    }
}

fn pairs(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn object(h: &[(String, String)]) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in h {
        m.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(m)
}

struct Scripted {
    f: Value,
    key: Vec<u8>,
    now: f64,
    transport: Mutex<VecDeque<Value>>,
    keys: Mutex<VecDeque<Value>>,
    sessions: Mutex<VecDeque<Value>>,
    rounds: Mutex<Vec<Value>>,
    broker: Mutex<Vec<Value>>,
}

fn credential<T>(answer: Value, ready: impl FnOnce(&Value) -> T) -> Credential<T> {
    match &answer {
        Value::Null => Credential::Absent,
        Value::String(s) if s == "transient" => Credential::Transient,
        v => Credential::Ready(ready(v)),
    }
}

impl Backend for Scripted {
    fn now_ms(&self) -> f64 {
        self.now
    }
    async fn server(&self, id: &str) -> Option<ServerSpec> {
        let s = self.f["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)?;
        let text = |k: &str| s[k].as_str().map(str::to_owned);
        Some(ServerSpec {
            id: text("id").unwrap(),
            url: text("url").unwrap(),
            auth: text("auth").unwrap(),
            directory: s["directory"] == true,
            title: text("title"),
            added_by: text("addedBy"),
            token_env: text("tokenEnv"),
        })
    }
    async fn key_headers(&self, user: &str, server: &str) -> Credential<Vec<(String, String)>> {
        self.broker
            .lock()
            .unwrap()
            .push(json!({"op": "key", "user": user, "server": server}));
        let answer = self
            .keys
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted key");
        credential(answer, pairs)
    }
    async fn oauth_session(
        &self,
        user: &str,
        server: &str,
        rejected: Option<&str>,
    ) -> Credential<Session> {
        self.broker
            .lock()
            .unwrap()
            .push(json!({"op": "session", "user": user, "server": server, "rejected": rejected}));
        let answer = self
            .sessions
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted session");
        credential(answer, |v| Session {
            access: v["accessToken"].as_str().unwrap().into(),
            generation: v["generation"].as_str().unwrap().into(),
        })
    }
    async fn reject(&self, user: &str, server: &str, generation: &str) {
        self.broker.lock().unwrap().push(
            json!({"op": "reject", "user": user, "server": server, "generation": generation}),
        );
    }
    async fn directory_headers(&self, id: &str) -> Vec<(String, String)> {
        pairs(&self.f["sharedHeaders"][id])
    }
    fn static_token(&self, env: &str) -> Option<String> {
        self.f["env"][env]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    }
    fn internal_token(
        &self,
        uid: Option<&str>,
        pid: Option<&str>,
        write: bool,
        target: Option<&str>,
    ) -> Option<String> {
        Some(mint_call_token(
            &self.key,
            uid,
            pid,
            write,
            target,
            self.now as i64,
            self.f["jti"].as_str().unwrap(),
        ))
    }
    async fn url_public(&self, url: &str) -> bool {
        !self.f["notPublic"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u == url)
    }
    async fn call(
        &self,
        server: &ServerSpec,
        headers: &[(String, String)],
        name: &str,
        arguments: &JValue,
    ) -> Result<JValue, Failed> {
        let outcome = self
            .transport
            .lock()
            .unwrap()
            .pop_front()
            .expect("a scripted round");
        let mut round = json!({"url": server.url, "headers": object(headers)});
        let failed = |status: &Value, retry: &Value| Failed {
            http_status: Some(u16::try_from(status.as_u64().unwrap()).unwrap()),
            retry_after: retry.as_str().map(str::to_owned),
            message: format!("MCP {status}: qa detail"),
        };
        if let Some(connect) = outcome.get("connect") {
            self.rounds.lock().unwrap().push(round);
            return Err(failed(&connect["http"], &Value::Null));
        }
        round["name"] = json!(name);
        round["args"] = sv(arguments);
        self.rounds.lock().unwrap().push(round);
        if let Some(status) = outcome.get("http") {
            return Err(failed(status, &outcome["retryAfter"]));
        }
        if let Some(message) = outcome.get("error") {
            return Err(Failed {
                http_status: None,
                retry_after: None,
                message: message.as_str().unwrap().into(),
            });
        }
        Ok(outcome.get("result").map_or(JValue::Undefined, jv))
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[tokio::test]
async fn every_recorded_call_answers_as_cores_executor_did() {
    let f = fixture();
    assert_eq!(f["version"], 1);
    let pin = include_str!("fixtures/mcp-call-oracle.core-ref").trim();
    assert_eq!(f["coreRef"].as_str(), Some(pin));
    let marker = f["reduceMarker"].as_str().unwrap().to_owned();
    let key = hex(f["internalKey"].as_str().unwrap());
    let mut total = 0;
    for scenario in f["scenarios"].as_array().unwrap() {
        let limits = Mutex::new(RateLimit::default());
        for (n, call) in scenario["calls"].as_array().unwrap().iter().enumerate() {
            let at = format!("{} / call {n}", scenario["name"]);
            let script = &call["script"];
            let queue =
                |k: &str| Mutex::new(script[k].as_array().unwrap().iter().cloned().collect());
            let b = Scripted {
                f: f.clone(),
                key: key.clone(),
                now: call["at"].as_f64().unwrap(),
                transport: queue("transport"),
                keys: queue("keys"),
                sessions: queue("sessions"),
                rounds: Mutex::new(Vec::new()),
                broker: Mutex::new(Vec::new()),
            };
            let request = Request::from_json(&jv(&call["request"])).unwrap();
            let answer = execute(&b, &limits, &request).await;
            let text = if answer.reduce {
                format!("{marker}{}", answer.text)
            } else {
                answer.text.clone()
            };
            assert_eq!(text, call["text"].as_str().unwrap(), "{at}");
            assert_eq!(
                Value::Array(b.broker.into_inner().unwrap()),
                call["broker"],
                "{at}"
            );
            let rounds = b.rounds.into_inner().unwrap();
            let want = call["rounds"].as_array().unwrap();
            assert_eq!(rounds.len(), want.len(), "{at}");
            for (ours, core) in rounds.iter().zip(want) {
                assert_eq!(ours["url"], core["url"], "{at}");
                assert_eq!(ours["headers"], core["headers"], "{at}: headers");
                // A round refused at initialize never reached the tool.
                if core.get("name").is_some() {
                    assert_eq!(ours["name"], core["name"], "{at}");
                    assert_eq!(ours["args"], core["args"], "{at}");
                }
            }
            assert!(
                b.transport.into_inner().unwrap().is_empty(),
                "{at}: unused rounds"
            );
            total += 1;
        }
    }
    assert!(total >= 77, "{total} calls");
}
