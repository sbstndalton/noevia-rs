//! Replays requests recorded from noevia-core's own `routes/embedding.cjs` (see
//! `generate_route_fixtures.cjs`): the answer, and what the route asked of the migration, the
//! address check, the served-model probe, the Diary, the audit log and the service sync.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::cell::{Cell, RefCell};

use embedding_admin::Action;
use embedding_migration::routes::{
    apply, diary_half, get, DiaryCall, Outcome, Projects, RouteHost,
};
use embedding_migration::target::Refusal;
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/node-routes.v1.json");

struct Stub {
    case: Value,
    migration: RefCell<Vec<Value>>,
    diary: RefCell<Vec<Value>>,
    served: RefCell<Vec<Value>>,
    addresses: RefCell<Vec<Value>>,
    audit: RefCell<Vec<Value>>,
    sync: Cell<u64>,
}

fn answer(spec: &Value) -> Result<Value, Refusal> {
    if let Some(t) = spec.get("throw") {
        let code = t["code"]
            .as_str()
            .map(|c| &*Box::leak(c.to_owned().into_boxed_str()));
        let mut extra = Map::new();
        for k in ["coverage", "parity", "served"] {
            if let Some(v) = t.get(k) {
                extra.insert(k.into(), v.clone());
            }
        }
        return Err(Refusal {
            status: t["status"].as_u64().map(|s| u16::try_from(s).unwrap()),
            code,
            message: t["message"].as_str().unwrap().to_owned(),
            extra,
        });
    }
    Ok(spec
        .get("ok")
        .cloned()
        .unwrap_or(json!({ "qa": "migration-status" })))
}

impl Projects for Stub {
    fn act(
        &mut self,
        action: Action,
        target: Option<&Value>,
        force: bool,
    ) -> Result<Value, Refusal> {
        let (name, call) = match action {
            Action::SetNext => (
                "setNext",
                json!(["setNext", target.cloned().unwrap_or(Value::Null)]),
            ),
            Action::ClearNext => ("clearNext", json!(["clearNext"])),
            Action::Switch => ("switchToNext", json!(["switchToNext", { "force": force }])),
            Action::Rollback => ("rollback", json!(["rollback"])),
            Action::Finish => ("finish", json!(["finish"])),
            _ => panic!("service action reached the migration"),
        };
        self.migration.borrow_mut().push(call);
        answer(&self.case["migration"][name])
    }
    fn served_models(&self, base_url: Option<&str>, model: &str) -> Option<Vec<Value>> {
        self.served.borrow_mut().push(json!([base_url, model]));
        match self.case["served"].get(base_url.unwrap_or("")) {
            None => Some(vec![json!(model)]),
            Some(v) => v.as_array().cloned(),
        }
    }
    fn status(&self) -> Value {
        json!({ "qa": "status" })
    }
}

impl RouteHost for Stub {
    fn check_address(&self, address: &str) -> Result<(), String> {
        self.addresses.borrow_mut().push(json!(address));
        if address.contains("203.0.113.") {
            return Err("qa public address".into());
        }
        Ok(())
    }
    fn diary(&self, method: &str, body: Option<&Value>) -> Value {
        if self.case["noDiary"] == true {
            return diary_half(DiaryCall::NotConfigured);
        }
        self.diary
            .borrow_mut()
            .push(json!([method, body.cloned().unwrap_or(Value::Null)]));
        let spec = &self.case["diary"][method];
        if let Some(e) = spec.get("throw") {
            return diary_half(DiaryCall::Failed(e.as_str().unwrap().to_owned()));
        }
        if spec.is_null() {
            return diary_half(DiaryCall::Answered {
                ok: true,
                status: 200,
                body: json!({ "qa": "diary-ok" }),
            });
        }
        diary_half(DiaryCall::Answered {
            ok: spec["ok"] == true,
            status: u16::try_from(spec["status"].as_u64().unwrap()).unwrap(),
            body: spec["body"].clone(),
        })
    }
    fn audit(&self, event: &str, user: &str, detail: Value) {
        self.audit
            .borrow_mut()
            .push(json!([event, user, null, detail]));
    }
    fn sync_service(&self) {
        self.sync.set(self.sync.get() + 1);
    }
    fn service_status(&self) -> Option<Value> {
        None
    }
}

#[test]
fn every_recorded_request_is_answered_as_node_answered_it() {
    let f: Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(
        f["sources"]["server/routes/embedding.cjs"],
        "ae34b6be30f2b8880a5c6c0c141ed9d1e709e87ef92202bb3cc0e79465db9854"
    );
    let cases = f["cases"].as_array().unwrap();
    assert!(cases.len() >= 40);
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let mut stub = Stub {
            case: c.clone(),
            migration: RefCell::default(),
            diary: RefCell::default(),
            served: RefCell::default(),
            addresses: RefCell::default(),
            audit: RefCell::default(),
            sync: Cell::new(0),
        };
        let rec = &c["rec"];
        if c["get"] == true {
            let body = get(&stub, &stub);
            assert_eq!(body, c["response"], "{name}");
            assert_eq!(
                Value::Array(stub.diary.take()),
                rec["diary"],
                "{name}: diary"
            );
            continue;
        }
        let action = c["body"]["action"].as_str().unwrap_or("");
        let host = Stub {
            case: c.clone(),
            migration: RefCell::default(),
            diary: RefCell::default(),
            served: RefCell::default(),
            addresses: RefCell::default(),
            audit: RefCell::default(),
            sync: Cell::new(0),
        };
        let outcome = apply(&c["body"], "qa-admin", &mut stub, &host);
        if action.starts_with("service-") {
            assert_eq!(outcome, Outcome::Service, "{name}");
            assert_eq!(c["status"], 409, "{name}: Node without a managed service");
            continue;
        }
        let Outcome::Reply(status, body) = outcome else {
            panic!("{name}: not answered")
        };
        assert_eq!(json!(status), c["status"], "{name}: status");
        assert_eq!(body, c["response"], "{name}: body");
        assert_eq!(
            Value::Array(stub.migration.take()),
            rec["migration"],
            "{name}: migration calls"
        );
        assert_eq!(
            Value::Array(stub.served.take()),
            rec["served"],
            "{name}: served probes"
        );
        assert_eq!(
            Value::Array(host.addresses.take()),
            rec["addresses"],
            "{name}: address checks"
        );
        assert_eq!(
            Value::Array(host.diary.take()),
            rec["diary"],
            "{name}: diary calls"
        );
        assert_eq!(
            Value::Array(host.audit.take()),
            rec["audit"],
            "{name}: audit"
        );
        assert_eq!(
            host.sync.get(),
            rec["sync"].as_u64().unwrap(),
            "{name}: service sync"
        );
    }
}
