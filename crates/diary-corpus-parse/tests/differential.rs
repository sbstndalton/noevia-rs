use diary_corpus_parse::{parse_chunks, parse_diary};
use serde_json::{json, Value};

#[test]
fn matches_actual_python_parser_and_chunk_projection() -> Result<(), Box<dyn std::error::Error>> {
    let oracle: Value = serde_json::from_str(include_str!("python-corpus.json"))?;
    let fixtures = oracle
        .get("fixtures")
        .and_then(Value::as_array)
        .ok_or("fixtures")?;
    assert_eq!(fixtures.len(), 455);
    assert_eq!(oracle.get("unicode"), Some(&json!("15.0.0")));
    assert!(oracle
        .get("python")
        .and_then(Value::as_str)
        .is_some_and(|v| v.starts_with("3.12.")));
    for fixture in fixtures {
        let input = fixture.get("input").and_then(Value::as_str);
        let days = parse_diary(input.unwrap_or_default());
        let actual = json!(days
            .iter()
            .map(|day| json!({
                "header": day.header, "date": day.date,
                "subsections": day.subsections.iter().map(|sub| json!({
                    "header": sub.header, "time": sub.time, "topic": sub.topic,
                    "exchanges": sub.exchanges.iter().map(|ex| json!({
                        "me": ex.me, "claude": ex.claude, "xid": ex.xid
                    })).collect::<Vec<_>>()
                })).collect::<Vec<_>>()
            }))
            .collect::<Vec<_>>());
        assert_eq!(
            Some(&actual),
            fixture.get("days"),
            "parsed {:?}",
            fixture.get("name")
        );
        let chunks = json!(parse_chunks(input)
            .iter()
            .map(|chunk| json!([chunk.day, chunk.header, chunk.text, chunk.content_hash]))
            .collect::<Vec<_>>());
        assert_eq!(
            Some(&chunks),
            fixture.get("chunks"),
            "chunks {:?}",
            fixture.get("name")
        );
    }
    Ok(())
}
