//! Differential test: `pipeline-fixtures.json` (recorded from the frozen Python LoggingPipeline with
//! the real templates by `generate_pipeline_fixtures.py`) replayed with the same scripted aux model
//! and fake store: outcome, progress events, every aux prompt and every store call must match.
#![allow(clippy::indexing_slicing)] // serde_json `Value[...]` on fixture data

use diary_core::pipeline::{self, Aux, AuxBudget, Journal, Pipeline};
use serde_json::{json, Value};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct FakeAux {
    script: Vec<Value>,
    calls: Vec<Value>,
}

impl Aux for FakeAux {
    fn call(&mut self, _budget: AuxBudget, system: &str, prompt: &str) -> Result<String, ()> {
        self.calls.push(json!({"system": system, "prompt": prompt, "temperature": 0.1, "max_tokens": 2048, "stop": null}));
        let reply = if self.script.is_empty() {
            json!({"raise": true})
        } else {
            self.script.remove(0)
        };
        if reply["raise"].as_bool().unwrap_or(false) {
            return Err(());
        }
        Ok(reply["text"].as_str().unwrap_or("").to_owned())
    }
}

struct FakeStore {
    spec: Value,
    calls: Vec<Value>,
}

impl Journal for FakeStore {
    fn log_exchange(
        &mut self,
        day: &str,
        sub_header: &str,
        me: &str,
        claude: &str,
    ) -> Result<String, (bool, String)> {
        self.calls
            .push(json!(["log_exchange", day, sub_header, me, claude]));
        match self.spec["log"].as_str() {
            Some("native") => Err((true, String::new())),
            Some("fail") => Err((false, "synthetic write failure".into())),
            _ => Ok("qa-xid-0001".into()),
        }
    }
    fn index_enabled(&self) -> bool {
        self.spec["index_enabled"].as_bool().unwrap_or(true)
    }
    fn standing_sections_text(&mut self, max_chars: i64) -> Result<String, ()> {
        self.calls.push(json!(["standing", max_chars]));
        if self.spec["standing_raise"].as_bool().unwrap_or(false) {
            return Err(());
        }
        Ok("## Open questions\n- synthetic".into())
    }
    fn update_standing_sections(
        &mut self,
        open: &Value,
        timeline: &Value,
        today: &str,
    ) -> Result<Option<String>, ()> {
        self.calls.push(json!(["update", open, timeline, today]));
        match self.spec["update"].as_str() {
            Some("raise") => Err(()),
            Some("changed") => Ok(Some("qa-journal-1".into())),
            _ => Ok(None),
        }
    }
}

#[test]
fn pipeline_matches_python() -> TestResult {
    let fx: Value = serde_json::from_str(include_str!("pipeline-fixtures.json"))?;
    assert_eq!(json!(pipeline::THINKING_MODEL_HINT), fx["hint"]);
    for row in fx["json_extract"].as_array().ok_or("json_extract")? {
        let got = pipeline::json_extract(row[0].as_str().ok_or("text")?).ok();
        assert_eq!(json!(got), row[1], "json_extract {}", row[0]);
    }
    let p = Pipeline {
        templates: fx["templates"].as_object().cloned().ok_or("templates")?,
    };
    let cases = fx["cases"].as_array().ok_or("cases")?;
    assert!(cases.len() >= 100);
    for case in cases {
        let name = case["name"].as_str().unwrap_or("?");
        let mut aux = FakeAux {
            script: case["script"].as_array().cloned().unwrap_or_default(),
            calls: Vec::new(),
        };
        let mut store = FakeStore {
            spec: case["store"].clone(),
            calls: Vec::new(),
        };
        let mut events = Vec::new();
        let with_progress = case["progress"].as_bool().unwrap_or(false);
        let outcome = p.log_exchange(
            &mut aux,
            &mut store,
            case["user"].as_str().unwrap_or(""),
            case["assistant"].as_str().unwrap_or(""),
            case["topic"].as_str().unwrap_or(""),
            "2031-03-20",
            &mut |e| {
                if with_progress {
                    events.push(e);
                }
            },
        );
        let got = match outcome {
            Ok(o) => json!({"decision": o.decision, "xid": o.xid, "reason": o.reason}),
            Err(_) => json!({"error": "NativeWriteUnavailable"}),
        };
        let expected = &case["expected"];
        assert_eq!(got, expected["result"], "{name}: outcome");
        assert_eq!(json!(events), expected["events"], "{name}: progress events");
        assert_eq!(
            json!(aux.calls),
            expected["aux_calls"],
            "{name}: aux prompts"
        );
        assert_eq!(
            json!(store.calls),
            expected["store_calls"],
            "{name}: store calls"
        );
    }
    Ok(())
}
