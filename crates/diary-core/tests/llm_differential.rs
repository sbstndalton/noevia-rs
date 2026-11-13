//! Differential test: `llm-fixtures.json` (recorded from the frozen Python model client by
//! `generate_llm_fixtures.py`) replayed on the Rust protocol: text helpers, the re.IGNORECASE
//! tables, served-model choice, too-large detection and 260 synthetic completions over a fake
//! transport (requests sent, events emitted, reply or failure, provider-progress calls).
#![allow(clippy::indexing_slicing)] // serde_json `Value[...]` on fixture data

use diary_core::llm::{self, Call, HttpResponse, LlmError, Mode, Served, Transport};
use serde_json::{json, Value};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixtures() -> Result<Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_str(include_str!("llm-fixtures.json"))?)
}

#[test]
fn text_helpers_match_python() -> TestResult {
    let fx = fixtures()?;
    let rows = fx["text"].as_array().ok_or("text")?;
    assert!(rows.len() >= 200);
    for row in rows {
        let text = row["text"].as_str().ok_or("text")?;
        assert_eq!(
            json!(llm::strip_reasoning(text, true)),
            row["strip_final"],
            "strip_final {text:?}"
        );
        assert_eq!(
            json!(llm::strip_reasoning(text, false)),
            row["strip_partial"],
            "strip_partial {text:?}"
        );
        let (visible, decision) = llm::strip_log_marker(text);
        assert_eq!(
            json!([visible, decision]),
            row["log_marker"],
            "log_marker {text:?}"
        );
        assert_eq!(
            json!(llm::with_shortened_note(text)),
            row["shortened"],
            "shortened {text:?}"
        );
        for (cap, want) in row["cap"].as_object().ok_or("cap")? {
            let cap = if cap == "None" {
                None
            } else {
                Some(cap.parse::<i64>()?)
            };
            let (got, cut) = llm::cap_visible(text, cap);
            assert_eq!(json!([got, cut]), *want, "cap {cap:?} {text:?}");
        }
    }
    Ok(())
}

#[test]
fn ignorecase_tables_match_python() -> TestResult {
    let fx = fixtures()?;
    let cf = &fx["case_folding"];
    let mut members = Vec::new();
    for (letter, codes) in cf["classes"].as_object().ok_or("classes")? {
        let letter = letter.chars().next().ok_or("letter")?;
        let want: Vec<u32> = codes
            .as_array()
            .ok_or("codes")?
            .iter()
            .filter_map(Value::as_u64)
            .map(|c| c as u32)
            .collect();
        let got: Vec<u32> = (0..=0x10FFFFu32)
            .filter_map(char::from_u32)
            .filter(|c| llm::letter_matches(letter, *c))
            .map(u32::from)
            .collect();
        assert_eq!(got, want, "re.I class of {letter}");
        members.extend(want);
    }
    members.sort_unstable();
    members.dedup();
    let want: Vec<Value> = cf["backref_pairs"].as_array().ok_or("pairs")?.clone();
    let mut got = Vec::new();
    for a in &members {
        for b in &members {
            let (Some(ca), Some(cb)) = (char::from_u32(*a), char::from_u32(*b)) else {
                continue;
            };
            if llm::simple_lower(ca) == llm::simple_lower(cb) {
                got.push(json!([a, b]));
            }
        }
    }
    assert_eq!(got, want, "backreference equality");
    Ok(())
}

#[test]
fn served_model_choice_matches_python() -> TestResult {
    let fx = fixtures()?;
    for case in fx["served"]["entries"].as_array().ok_or("entries")? {
        match llm::served_entries(&case["body"]) {
            Ok(entries) => {
                let got: Vec<Value> = entries
                    .iter()
                    .map(|e| json!({"id": e.id, "loaded": e.loaded, "chat": e.chat}))
                    .collect();
                assert_eq!(json!(got), case["entries"], "{}", case["body"]);
            }
            Err(e) => assert_eq!(json!(e.python_class()), case["error"], "{}", case["body"]),
        }
    }
    for case in fx["served"]["choices"].as_array().ok_or("choices")? {
        let entries: Vec<Served> = case["entries"]
            .as_array()
            .ok_or("entries")?
            .iter()
            .map(|e| Served {
                id: e["id"].as_str().unwrap_or("").to_owned(),
                loaded: e["loaded"].as_bool().unwrap_or(false),
                chat: e["chat"].as_bool().unwrap_or(false),
            })
            .collect();
        let got = llm::choose(
            &entries,
            case["chat_model"].as_str().unwrap_or(""),
            case["fallback"].as_str().unwrap_or(""),
            "LLM_AUX_MODEL",
            "LLM_AUX_FALLBACK_MODEL",
            case["partner"].as_str(),
        );
        match got {
            Ok((model, source, why)) => {
                assert_eq!(json!([model, source, why]), case["choice"], "{case}")
            }
            Err(LlmError::ModelNotServed { setting, message }) => {
                assert_eq!(json!("ModelNotServed"), case["error"], "{case}");
                assert_eq!(json!(setting), case["setting"]);
                assert_eq!(json!(message), case["message"]);
            }
            Err(LlmError::Llm(message)) => {
                assert_eq!(json!("LLMError"), case["error"], "{case}");
                assert_eq!(json!(message), case["message"]);
            }
            Err(other) => return Err(format!("unexpected {other:?}").into()),
        }
    }
    Ok(())
}

#[test]
fn too_large_matches_python() -> TestResult {
    let fx = fixtures()?;
    for case in fx["too_large"].as_array().ok_or("too_large")? {
        let status = u16::try_from(case["status"].as_u64().ok_or("status")?)?;
        let got = llm::too_large(status, case["body"].as_str().ok_or("body")?)
            .map(|t| json!({"message": t.message, "tokens": t.tokens, "limit": t.limit}))
            .unwrap_or(Value::Null);
        assert_eq!(got, case["result"], "{}", case["body"]);
    }
    Ok(())
}

struct Fake {
    responses: Vec<Value>,
    sent: Vec<Value>,
    sleeps: Vec<f64>,
}

impl Transport for Fake {
    fn post(&mut self, path: &str, body: &Value) -> Result<HttpResponse, &'static str> {
        assert_eq!(path, "/chat/completions");
        self.sent.push(body.clone());
        let (status, ctype, text) = if self.responses.is_empty() {
            (
                500,
                "text/plain".to_owned(),
                "no more synthetic answers".to_owned(),
            )
        } else {
            let r = self.responses.remove(0);
            (
                r[0].as_u64().unwrap_or(500) as u16,
                r[1].as_str().unwrap_or("").to_owned(),
                r[2].as_str().unwrap_or("").to_owned(),
            )
        };
        Ok(HttpResponse {
            status,
            content_type: ctype,
            body: text.into_bytes(),
        })
    }
    fn sleep(&mut self, seconds: f64) {
        self.sleeps.push(seconds);
    }
}

fn message_matches(rust: &str, python: &str) -> bool {
    // httpx exception text (URLs, reason phrases) is not reproduced: compare up to the colon.
    match python.split_once(" attempts: ") {
        Some((head, _)) => rust.starts_with(&format!("{head} attempts: ")),
        None => rust == python,
    }
}

#[test]
fn completions_match_python() -> TestResult {
    let fx = fixtures()?;
    let cases = fx["completions"].as_array().ok_or("completions")?;
    assert!(cases.len() >= 250);
    let messages = json!([{"role": "user", "content": "QA question"}]);
    for case in cases {
        let name = case["name"].as_str().unwrap_or("?");
        let mode = match case["mode"].as_str() {
            Some("stream") => Mode::Stream,
            Some("cancellable") => Mode::Cancellable,
            _ => Mode::Plain,
        };
        let stop: Option<Vec<String>> = case["stop"].as_array().map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        });
        let call = Call {
            mode,
            model: "qa-chat",
            messages: &messages,
            temperature: if mode == Mode::Stream { 0.7 } else { 0.2 },
            max_tokens: case["max_tokens"].as_i64(),
            reasoning_tokens: case["reasoning_tokens"]
                .as_i64()
                .unwrap_or(llm::DEFAULT_MAX_REASONING_TOKENS),
            // chat_stream never sends `stop`.
            stop: if mode == Mode::Stream {
                None
            } else {
                stop.as_deref()
            },
            max_retries: case["retries"].as_i64().ok_or("retries")?,
            retry_backoff_s: 0.5,
        };
        let mut fake = Fake {
            responses: case["responses"].as_array().cloned().unwrap_or_default(),
            sent: Vec::new(),
            sleeps: Vec::new(),
        };
        let mut events = Vec::new();
        let mut alive_calls = 0u64;
        let result = llm::run(&mut fake, &call, &mut events, &mut || alive_calls += 1);
        let expected = &case["expected"];
        assert_eq!(
            json!(fake.sent),
            expected["sent"],
            "{name}: requests differ"
        );
        let got_events: Vec<Value> = events.iter().map(llm::Event::to_json).collect();
        assert_eq!(
            json!(got_events),
            expected["events"],
            "{name}: events differ"
        );
        assert_eq!(
            json!(alive_calls),
            expected["alive_calls"],
            "{name}: provider-progress calls differ"
        );
        assert_eq!(
            json!(fake.sleeps),
            expected["sleeps"],
            "{name}: retry waits differ"
        );
        let want = &expected["result"];
        match result {
            Ok(reply) => {
                assert!(
                    want["error"].is_null(),
                    "{name}: Python raised {} ({})",
                    want["error"],
                    want["message"]
                );
                assert_eq!(
                    json!({"content": reply.content, "reasoning": reply.reasoning, "truncated": reply.truncated}),
                    *want,
                    "{name}"
                );
            }
            Err(e) => {
                assert_eq!(
                    json!(e.python_class()),
                    want["error"],
                    "{name}: {e:?} vs {}",
                    want["message"]
                );
                let text = match &e {
                    LlmError::Llm(m) => m.clone(),
                    LlmError::LengthCap => llm::LENGTH_CAP_TEXT.to_owned(),
                    LlmError::EmptyAnswer => llm::EMPTY_ANSWER_TEXT.to_owned(),
                    _ => continue,
                };
                let py = want["message"].as_str().unwrap_or("");
                assert!(
                    message_matches(&text, py),
                    "{name}: message {text:?} vs {py:?}"
                );
            }
        }
    }
    Ok(())
}
