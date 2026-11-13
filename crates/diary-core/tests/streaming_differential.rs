//! Differential test: the frames of `streaming-fixtures.json` (recorded from the frozen Python
//! `exchange_stream` by `generate_streaming_fixtures.py`) rebuilt from the same synthetic events.
//! The opening frame is compared byte for byte; event frames as parsed JSON (D5).
#![allow(clippy::indexing_slicing)] // serde_json `Value[...]` on fixture data

use diary_core::streaming::{self, Failure};
use serde_json::{json, Map, Value};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn parse(frame: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let body = frame
        .strip_prefix("data: ")
        .and_then(|f| f.strip_suffix("\n\n"))
        .ok_or("frame shape")?;
    Ok(serde_json::from_str(body)?)
}

#[test]
fn frames_match_python() -> TestResult {
    let fx: Value = serde_json::from_str(include_str!("streaming-fixtures.json"))?;
    let events = [
        json!({"type": "status", "text": "QA step é"}),
        json!({"type": "reasoning", "text": "never shown"}),
        json!({"type": "delta", "text": "Synthetic 🌿 answer"}),
    ];
    let mut result = Map::new();
    for (k, v) in [
        ("answer", json!("Synthetic answer")),
        ("saved", json!(true)),
        ("xid", json!("qa-0001")),
        ("day", json!("2031-03-20")),
    ] {
        result.insert(k.into(), v);
    }
    for stream in fx["streams"].as_array().ok_or("streams")? {
        let name = stream["name"].as_str().unwrap_or("?");
        let mut frames = vec![streaming::STATUS_FRAME.to_owned()];
        frames.extend(
            events
                .iter()
                .filter(|e| streaming::shown(e))
                .map(streaming::frame),
        );
        let failure = match name {
            "ok" => None,
            "temporal_unavailable" => Some(Failure::TemporalUnavailable),
            "timeout_nothing_saved" => Some(Failure::Timeout {
                nothing_saved: true,
            }),
            "timeout_after_save" => Some(Failure::Timeout {
                nothing_saved: false,
            }),
            "length_cap" => Some(Failure::from(&diary_core::llm::LlmError::LengthCap)),
            "empty_answer" => Some(Failure::from(&diary_core::llm::LlmError::EmptyAnswer)),
            "llm_error" => Some(Failure::from(&diary_core::llm::LlmError::Llm("x".into()))),
            _ => Some(Failure::Other),
        };
        match failure {
            None => {
                frames.push(streaming::frame(&streaming::result_event(&result)));
                frames.push(streaming::frame(&streaming::done_event()));
            }
            Some(f) => frames.push(streaming::frame(&streaming::error_event(f))),
        }
        let want = stream["frames"].as_array().ok_or("frames")?;
        assert_eq!(frames.len(), want.len(), "{name}: frame count");
        assert_eq!(json!(frames[0]), want[0], "{name}: opening frame bytes");
        for (got, want) in frames.iter().zip(want).skip(1) {
            assert_eq!(parse(got)?, parse(want.as_str().ok_or("frame")?)?, "{name}");
        }
        let headers: Value = streaming::HEADERS
            .iter()
            .map(|(k, v)| ((*k).to_owned(), json!(v)))
            .collect::<Map<_, _>>()
            .into();
        assert_eq!(headers, stream["headers"], "{name}: headers");
    }
    Ok(())
}
