//! Differential test: every case of `assembler-fixtures.json` (recorded from the frozen Python
//! `ContextAssembler.build` by `generate_assembler_fixtures.py`) is replayed with the same stub
//! store and stub retriever; the messages, the retrieval trace and every store and search call
//! must match.
#![allow(clippy::indexing_slicing, clippy::panic)] // serde_json `Value[...]` on fixture data; a mismatch reports its context

use std::cell::RefCell;
use std::collections::HashSet;

use chrono::NaiveDate;
use diary_core::assembler::{
    Assembler, Hit, Search, SearchFailed, SearchOutcome, Store, StoreError,
};
use diary_core::pyconv;
use diary_core::retrieval_trace::RetrievalTrace;
use serde_json::{json, Value};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct StubStore {
    spec: Value,
    raise_days: HashSet<String>,
    calls: RefCell<Vec<Value>>,
}

impl StubStore {
    fn cut(&self, text: &str, max_chars: i64) -> String {
        if self.spec["ignore_max"].as_bool().unwrap_or(false) {
            text.to_owned()
        } else {
            pyconv::slice(text, None, Some(max_chars))
        }
    }
}

impl Store for StubStore {
    fn day_text(&self, day: NaiveDate, max_chars: i64) -> Result<String, StoreError> {
        let iso = day.format("%Y-%m-%d").to_string();
        self.calls.borrow_mut().push(json!(["day", iso, max_chars]));
        if self.raise_days.contains(&iso) {
            return Err(StoreError);
        }
        let text = self.spec["days"][&iso].as_str().unwrap_or("");
        Ok(self.cut(text, max_chars))
    }

    fn standing_sections_text(&self, max_chars: i64) -> Result<String, StoreError> {
        self.calls
            .borrow_mut()
            .push(json!(["standing", null, max_chars]));
        Ok(self.cut(self.spec["standing"].as_str().unwrap_or(""), max_chars))
    }

    fn memory_text(&self) -> String {
        self.spec["memory"].as_str().unwrap_or("").to_owned()
    }
}

struct StubSearch {
    spec: Value,
    calls: RefCell<Vec<Value>>,
}

impl Search for StubSearch {
    fn search(
        &self,
        query: &str,
        top_k: i64,
        min_score: f64,
        with_ids: bool,
    ) -> Result<SearchOutcome, SearchFailed> {
        self.calls
            .borrow_mut()
            .push(json!([query, top_k, min_score, with_ids]));
        if self.spec["raise"].as_bool().unwrap_or(false) {
            return Err(SearchFailed);
        }
        let rows = self.spec["rows"].as_array().cloned().unwrap_or_default();
        let filtered: Vec<Hit> = rows
            .iter()
            .filter(|r| r["score"].as_f64().is_some_and(|s| s >= min_score))
            .map(|r| Hit {
                id: if with_ids { r["id"].as_i64() } else { None },
                day: r["day"].as_str().unwrap_or("").to_owned(),
                header: r["header"].as_str().unwrap_or("").to_owned(),
                text: r["text"].as_str().unwrap_or("").to_owned(),
                score: r["score"].as_f64().unwrap_or(0.0),
            })
            .collect();
        let n = pyconv::slice_len(filtered.len(), top_k);
        Ok(SearchOutcome {
            hits: filtered.into_iter().take(n).collect(),
            // Python sets diag["status"] only when a diag dict was passed (traced searches).
            status: if with_ids {
                self.spec["status"].as_str().map(str::to_owned)
            } else {
                None
            },
        })
    }
}

fn run(case: &Value, expected: &Value, system_prompt: &str) -> TestResult {
    let name = case["name"].as_str().unwrap_or("?");
    let store = StubStore {
        spec: case["store"].clone(),
        raise_days: case["store"]["raise_days"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        calls: RefCell::new(Vec::new()),
    };
    let search = (!case["retriever"].is_null()).then(|| StubSearch {
        spec: case["retriever"].clone(),
        calls: RefCell::new(Vec::new()),
    });
    let asm = Assembler {
        store: &store,
        search: search.as_ref().map(|s| s as &dyn Search),
        cfg: &case["cfg"],
        system_prompt: system_prompt.to_owned(),
        local_reference: case["local_reference"].as_str().unwrap_or("").to_owned(),
    };
    let day = NaiveDate::parse_from_str(case["day"].as_str().ok_or("day")?, "%Y-%m-%d")?;
    let turns = case["session_turns"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut trace = case["trace"]
        .as_bool()
        .unwrap_or(false)
        .then(RetrievalTrace::new);
    let built = asm.build(
        day,
        case["message"].as_str().ok_or("message")?,
        &turns,
        case["today_header"].as_str(),
        trace.as_mut(),
    );
    match built {
        Ok(messages) => {
            assert!(
                expected["error"].is_null(),
                "{name}: Python raised {}, Rust built messages",
                expected["error"]
            );
            let got: Vec<Value> = messages
                .iter()
                .map(|m| json!({"role": m.role, "content": m.content}))
                .collect();
            let want = expected["messages"].as_array().ok_or("messages")?;
            assert_eq!(got.len(), want.len(), "{name}: message count");
            for (i, (g, w)) in got.iter().zip(want).enumerate() {
                assert_eq!(g["role"], w["role"], "{name}: message {i} role");
                let (g, w) = (
                    g["content"].as_str().unwrap_or(""),
                    w["content"].as_str().unwrap_or(""),
                );
                if g != w {
                    let at = g.chars().zip(w.chars()).take_while(|(a, b)| a == b).count();
                    let ctx = |s: &str| {
                        s.chars()
                            .skip(at.saturating_sub(80))
                            .take(200)
                            .collect::<String>()
                    };
                    panic!(
                        "{name}: message {i} differs at char {at}\n rust: {:?}\n  py:  {:?}",
                        ctx(g),
                        ctx(w)
                    );
                }
            }
        }
        Err(e) => assert!(
            !expected["error"].is_null(),
            "{name}: Rust failed with {e:?}, Python built messages"
        ),
    }
    let got_trace = trace
        .as_ref()
        .map(RetrievalTrace::to_json)
        .unwrap_or(Value::Null);
    if expected["error"].is_null() {
        assert_eq!(got_trace, expected["trace"], "{name}: trace differs");
    }
    assert_eq!(
        Value::Array(store.calls.borrow().clone()),
        expected["store_calls"],
        "{name}: store calls differ"
    );
    let search_calls = search
        .as_ref()
        .map(|s| Value::Array(s.calls.borrow().clone()))
        .unwrap_or(json!([]));
    assert_eq!(
        search_calls, expected["search_calls"],
        "{name}: search calls differ"
    );
    Ok(())
}

#[test]
fn every_case_matches_python() -> TestResult {
    let fx: Value = serde_json::from_str(include_str!("assembler-fixtures.json"))?;
    let prompt = fx["system_prompt"].as_str().ok_or("system_prompt")?;
    let cases = fx["cases"].as_array().ok_or("cases")?;
    assert!(cases.len() >= 200);
    let twins = &fx["degraded_twins"];
    let row_level: Vec<&str> = fx["row_level_degraded"]
        .as_array()
        .ok_or("row_level_degraded")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for case in cases {
        let name = case["name"].as_str().unwrap_or("");
        if row_level.contains(&name) {
            row_level_degraded(case, prompt)?;
            continue;
        }
        let expected = match twins[name].as_str() {
            // D3: the degraded case must produce exactly what Python produced with the focus off.
            Some(twin) => &cases.iter().find(|c| c["name"] == twin).ok_or("twin")?["expected"],
            None => &case["expected"],
        };
        run(case, expected, prompt)?;
    }
    assert!(twins.as_object().is_some_and(|t| t.len() == 6));
    Ok(())
}

/// D3 at row level: the focus refuses a retrieved row after the search. The exchange still builds;
/// the trace shows the focus unchecked and the search used the focused pool (top_k * 4).
fn row_level_degraded(case: &Value, system_prompt: &str) -> TestResult {
    let store = StubStore {
        spec: case["store"].clone(),
        raise_days: HashSet::new(),
        calls: RefCell::new(Vec::new()),
    };
    let search = StubSearch {
        spec: case["retriever"].clone(),
        calls: RefCell::new(Vec::new()),
    };
    let asm = Assembler {
        store: &store,
        search: Some(&search),
        cfg: &case["cfg"],
        system_prompt: system_prompt.to_owned(),
        local_reference: String::new(),
    };
    let day = NaiveDate::parse_from_str(case["day"].as_str().ok_or("day")?, "%Y-%m-%d")?;
    let mut trace = RetrievalTrace::new();
    let messages = asm.build(
        day,
        case["message"].as_str().ok_or("message")?,
        &[],
        None,
        Some(&mut trace),
    );
    assert!(messages.is_ok_and(|m| m.len() == 3));
    let t = trace.to_json();
    assert_eq!(t["focus"]["checked"], json!(false));
    assert_eq!(search.calls.borrow()[0][1], json!(32));
    Ok(())
}
