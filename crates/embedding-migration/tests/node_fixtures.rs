//! Replays scenarios recorded from noevia-core's own `embedding-config.cjs` and
//! `embedding-migration.cjs` (see `generate_fixtures.cjs` and the source hashes in the fixture):
//! every answer, refusal, dropped vector slot, worker kick, log line and state-file byte.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::cell::{Cell, RefCell};

use embedding_migration::config::{EmbeddingConfig, Options, Which};
use embedding_migration::migration::{Host, IndexCoverage, Migration, Pacing};
use embedding_migration::target::{Profiles, Refusal, Target};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/node-migration.v1.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

struct Stub {
    rag: Value,
    snapshot: Value,
    drops: RefCell<Vec<Value>>,
    kicks: Cell<u64>,
    logs: RefCell<Vec<String>>,
}

impl Host for Stub {
    type Index = String;
    fn served_models(&self, t: &Target) -> Option<Vec<Value>> {
        let key = t.base_url.clone().unwrap_or_default();
        self.rag["served"]
            .get(&key)
            .and_then(Value::as_array)
            .cloned()
    }
    fn indexes(&self) -> Vec<String> {
        self.rag["indexes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    }
    fn coverage(&self, index: &String, t: &Target) -> Option<IndexCoverage> {
        let key = format!("{}/{}@{index}", t.profile, t.model);
        let v = self.rag["coverage"]
            .get(&key)
            .or_else(|| self.rag.get("coverageDefault"))?;
        v.as_object()?;
        Some(IndexCoverage {
            chunks: v["chunks"].as_u64().unwrap(),
            vectors: v["vectors"].as_u64().unwrap(),
        })
    }
    fn drop_slot(&self, index: &String, t: &Target) -> Result<(), String> {
        self.drops
            .borrow_mut()
            .push(json!([index, t.profile, t.model]));
        let fails = self.rag["dropFails"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == index.as_str()));
        if fails {
            return Err("qa slot locked".into());
        }
        Ok(())
    }
    fn embed(&self, texts: &[&str], t: &Target) -> Result<Vec<Vec<f64>>, String> {
        let key = t.base_url.clone().unwrap_or_default();
        match &self.rag["embed"][key.as_str()] {
            Value::String(e) => Err(e.clone()),
            Value::Array(rows) => Ok(rows
                .iter()
                .take(texts.len())
                .map(|r| {
                    r.as_array()
                        .unwrap()
                        .iter()
                        .map(|x| x.as_f64().unwrap())
                        .collect()
                })
                .collect()),
            other => panic!("no embed stub for {key}: {other}"),
        }
    }
    fn kick(&self) {
        self.kicks.set(self.kicks.get() + 1);
    }
    fn snapshot(&self) -> Option<Value> {
        (!self.snapshot.is_null()).then(|| self.snapshot.clone())
    }
    fn log(&self, line: &str) {
        self.logs.borrow_mut().push(line.to_owned());
    }
}

/// Numbers compared as JavaScript sees them (one number type).
fn norm(v: &Value) -> Value {
    match v {
        Value::Number(n) => json!(n.as_f64().unwrap()),
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), norm(v))).collect()),
        other => other.clone(),
    }
}

fn scrub(text: &str, dir: &str) -> String {
    text.replace(dir, "<DIR>")
}

fn scrub_value(v: &Value, dir: &str) -> Value {
    serde_json::from_str(&scrub(&v.to_string(), &dir.replace('\\', "\\\\"))).unwrap()
}

fn error_json(r: &Refusal) -> Value {
    let mut out = json!({ "message": r.message, "status": r.status, "code": r.code });
    for (k, v) in &r.extra {
        out[k.as_str()] = v.clone();
    }
    out
}

/// A damaged-file message keeps everything but the JSON parser's own wording.
fn parse_prefix(message: &str) -> &str {
    message.split(" cannot be read (").next().unwrap()
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f["version"], 1);
    assert_eq!(
        f["sources"]["server/embedding-migration.cjs"],
        "b97076fc242eaf337f1e16813942fe5edc342e7c6594787c00aa4af752c24bdd"
    );
    assert!(f["scenarios"].as_array().unwrap().len() >= 40);
}

#[test]
fn every_recorded_scenario_replays_as_node_ran_it() {
    let f = fixture();
    let profiles = Profiles {
        default: f["profiles"]["default"].as_str().unwrap().to_owned(),
        known: f["profiles"]["known"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect(),
    };
    let mut steps_checked = 0;
    for s in f["scenarios"].as_array().unwrap() {
        let name = s["name"].as_str().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_str().unwrap().to_owned();
        let state_file = tmp.path().join("embedding-config.json");
        if let Some(text) = s["file"].as_str() {
            std::fs::write(&state_file, text).unwrap();
        }
        let env = s["env"].clone();
        let get = |k: &str| env.get(k).and_then(Value::as_str).map(str::to_owned);
        let (config, load_logs) = EmbeddingConfig::load(Options {
            env: &get,
            state_file: Some(state_file.clone()),
            fallback_model: "default",
            profiles: profiles.clone(),
            native_owner: true,
        });
        let parse_error = s["parseError"].as_bool().unwrap();
        let want_logs: Vec<String> = s["load"]["logs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        let got_logs: Vec<String> = load_logs.iter().map(|l| scrub(l, &dir)).collect();
        if parse_error {
            assert_eq!(got_logs.len(), want_logs.len(), "{name}: load logs");
            assert_eq!(
                parse_prefix(&got_logs[0]),
                parse_prefix(&want_logs[0]),
                "{name}: load log"
            );
        } else {
            assert_eq!(got_logs, want_logs, "{name}: load logs");
        }
        let host = Stub {
            rag: s["rag"].clone(),
            snapshot: s["snapshot"].clone(),
            drops: RefCell::default(),
            kicks: Cell::new(0),
            logs: RefCell::default(),
        };
        let mut m = Migration::new(config, host, Pacing::from_env(&get));
        for (i, rec) in s["steps"].as_array().unwrap().iter().enumerate() {
            let step = &rec["step"];
            let op = step["op"].as_str().unwrap();
            let result = match op {
                "status" => Ok(m.status()),
                "setNext" => m.set_next(step.get("target")),
                "clearNext" => m.clear_next(),
                "switch" => m.switch_to_next(step["force"] == true),
                "rollback" => m.rollback(),
                "finish" => m.finish(),
                "refuseSeeded" => {
                    let r = &step["reasons"];
                    m.config.refuse_seeded(
                        r["active"].as_str(),
                        r["next"].as_str(),
                        step["transient"] == true,
                    );
                    Ok(m.status())
                }
                "release" => {
                    let which = if step["which"] == "active" {
                        Which::Active
                    } else {
                        Which::Next
                    };
                    m.config.release_seeded(which);
                    Ok(m.status())
                }
                other => panic!("unknown op {other}"),
            };
            let at = format!("{name} step {i} ({op})");
            let want = &rec["result"];
            // Node bug noevia#2696: a locked configuration's set-next called the writer outside the
            // publication lock and threw a plain error (500). The port answers core's 409 badState.
            if name == "refuse-seeded-active" && op == "setNext" {
                assert_eq!(
                    want["error"]["message"],
                    "Embedding configuration requires the publication lock",
                    "{at}: Node's recording"
                );
                assert!(want["error"]["status"].is_null(), "{at}");
                let err = result.unwrap_err();
                assert_eq!(
                    (err.status, err.code),
                    (Some(409), Some("badState")),
                    "{at}"
                );
                assert_eq!(err.message, "the address is public", "{at}");
            } else {
                match result {
                    Ok(v) => {
                        assert!(
                            want.get("ok").is_some(),
                            "{at}: Node refused with {want}, Rust answered {v}"
                        );
                        let (mut got, mut want) = (norm(&scrub_value(&v, &dir)), norm(&want["ok"]));
                        if parse_error {
                            for o in [&mut got, &mut want] {
                                let d = o["damaged"].as_str().map(|d| parse_prefix(d).to_owned());
                                o["damaged"] = json!(d);
                            }
                        }
                        assert_eq!(got, want, "{at}");
                    }
                    Err(e) => {
                        let got = scrub_value(&error_json(&e), &dir);
                        let want = &want["error"];
                        assert!(
                            !want.is_null(),
                            "{at}: Rust refused with {got}, Node answered"
                        );
                        if parse_error && e.code == Some("badState") {
                            assert_eq!(
                                parse_prefix(got["message"].as_str().unwrap()),
                                parse_prefix(want["message"].as_str().unwrap()),
                                "{at}"
                            );
                            assert_eq!(
                                (&got["status"], &got["code"]),
                                (&want["status"], &want["code"]),
                                "{at}"
                            );
                        } else {
                            assert_eq!(norm(&got), norm(want), "{at}");
                        }
                    }
                }
            }
            assert_eq!(
                Value::Array(m.host.drops.take()),
                rec["drops"],
                "{at}: dropped slots"
            );
            assert_eq!(
                m.host.kicks.replace(0),
                rec["kicks"].as_u64().unwrap(),
                "{at}: kicks"
            );
            let logs: Vec<Value> = m
                .host
                .logs
                .take()
                .iter()
                .map(|l| json!(scrub(l, &dir)))
                .collect();
            assert_eq!(Value::Array(logs), rec["logs"], "{at}: logs");
            let file = std::fs::read_to_string(&state_file).ok();
            assert_eq!(file.as_deref(), rec["file"].as_str(), "{at}: state file");
            steps_checked += 1;
        }
    }
    assert!(steps_checked >= 100, "only {steps_checked} steps");
}
