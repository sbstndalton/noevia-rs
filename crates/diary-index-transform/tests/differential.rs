use diary_index_transform::{
    apply_index_edits, bullet_text, check_bullet, match_indices, parse_index, parse_index_bytes,
    register_month, EditOp, Error, IndexFile,
};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct Metadata {
    source_sha256: String,
    python: String,
}

#[derive(Debug, Deserialize)]
struct State {
    preamble: Vec<String>,
    month_links: Vec<String>,
    sections: Vec<(String, Vec<String>)>,
}

impl From<State> for IndexFile {
    fn from(value: State) -> Self {
        Self {
            preamble: value.preamble,
            month_links: value.month_links,
            sections: value.sections,
        }
    }
}

#[derive(Debug, Deserialize)]
struct IndexCase {
    id: usize,
    text: Option<String>,
    before: State,
    label: String,
    filename: String,
    registered: bool,
    after_register: State,
    questions: Vec<Value>,
    timeline: Vec<Value>,
    today: String,
    changed: bool,
    after: State,
    render: String,
}

#[derive(Debug, Deserialize)]
struct HelperCase {
    id: usize,
    bullets: Vec<String>,
    text: String,
    indices: Vec<usize>,
    stripped: Vec<String>,
    checked: Vec<String>,
}

fn op_from_value(value: &Value) -> Result<EditOp, String> {
    let action = value
        .get("action")
        .and_then(Value::as_str)
        .ok_or("missing action")?;
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .ok_or("missing text")?;
    let replacement = value
        .get("replacement")
        .map(|r| {
            if r.is_null() {
                Ok(None)
            } else {
                r.as_str()
                    .map(|s| Some(s.to_owned()))
                    .ok_or("bad replacement")
            }
        })
        .transpose()?;
    let date = value
        .get("date")
        .map(|d| d.as_str().map(str::to_owned).ok_or("bad date"))
        .transpose()?;
    Ok(EditOp {
        action: action.to_owned(),
        text: text.to_owned(),
        replacement,
        date,
    })
}

#[test]
fn shipped_cpython_differential() -> Result<(), Box<dyn std::error::Error>> {
    let mut metadata = None;
    let mut cases = Vec::new();
    let mut helpers = Vec::new();
    for line in include_str!("index-fixtures.jsonl").lines() {
        let row: Value = serde_json::from_str(line)?;
        match row.get("kind").and_then(Value::as_str) {
            Some("meta") => metadata = Some(serde_json::from_value::<Metadata>(row)?),
            Some("index") => cases.push(serde_json::from_value::<IndexCase>(row)?),
            Some("helper") => helpers.push(serde_json::from_value::<HelperCase>(row)?),
            _ => return Err("unknown fixture kind".into()),
        }
    }
    let meta = metadata.ok_or("missing fixture metadata")?;
    assert_eq!(
        meta.source_sha256,
        "4b8809d3b7d4811723404cbc9a40fe4fe0743bc18bbb449a673ec711c48e0a87"
    );
    assert!(meta.python.starts_with("3.12."));
    assert_eq!(cases.len(), 665);
    assert_eq!(helpers.len(), 358);
    for case in cases {
        let mut idx = parse_index(case.text.as_deref())?;
        assert_eq!(idx, IndexFile::from(case.before), "parse case {}", case.id);
        assert_eq!(
            register_month(&mut idx, &case.label, &case.filename)?,
            case.registered,
            "register case {}",
            case.id
        );
        assert_eq!(
            idx,
            IndexFile::from(case.after_register),
            "register state case {}",
            case.id
        );
        let questions = case
            .questions
            .iter()
            .map(op_from_value)
            .collect::<Result<Vec<_>, _>>()?;
        let timeline = case
            .timeline
            .iter()
            .map(op_from_value)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            apply_index_edits(&mut idx, &questions, &timeline, &case.today)?,
            case.changed,
            "edit case {}",
            case.id
        );
        assert_eq!(
            idx,
            IndexFile::from(case.after),
            "edit state case {}",
            case.id
        );
        assert_eq!(idx.render()?, case.render, "render case {}", case.id);
    }
    for case in helpers {
        assert_eq!(
            match_indices(&case.bullets, &case.text),
            case.indices,
            "match case {}",
            case.id
        );
        assert_eq!(
            case.bullets
                .iter()
                .map(|b| bullet_text(b).to_owned())
                .collect::<Vec<_>>(),
            case.stripped,
            "strip case {}",
            case.id
        );
        assert_eq!(
            case.bullets
                .iter()
                .map(|b| check_bullet(b, true))
                .collect::<Vec<_>>(),
            case.checked,
            "check case {}",
            case.id
        );
    }
    Ok(())
}

#[test]
fn explicit_capacity_and_atomic_refusal() -> Result<(), Box<dyn std::error::Error>> {
    let oversized = "x".repeat(1024 * 1024 + 1);
    assert_eq!(parse_index(Some(&oversized)), Err(Error::Capacity));
    assert_eq!(parse_index_bytes(Some(b"\xff")), Err(Error::InvalidUtf8));
    assert_eq!(parse_index_bytes(None)?, parse_index(None)?);
    assert_eq!(
        parse_index(Some(&"x\n".repeat(20_001))),
        Err(Error::Capacity)
    );
    let mut line_bound = parse_index(None)?;
    line_bound.preamble = vec!["x".to_owned(); 24_000 - 11];
    assert!(line_bound.render().is_ok());
    line_bound.preamble.push("x".to_owned());
    assert_eq!(line_bound.render(), Err(Error::Capacity));
    let mut idx = parse_index(None)?;
    let original = idx.clone();
    let operation = EditOp {
        action: "add".to_owned(),
        text: "x".repeat(64 * 1024 + 1),
        replacement: None,
        date: None,
    };
    assert!(apply_index_edits(&mut idx, &[operation], &[], "2026-09-01").is_err());
    assert_eq!(idx, original);
    assert!(register_month(&mut idx, &oversized, "2026-01.md").is_err());
    assert_eq!(idx, original);
    Ok(())
}

#[test]
fn cpython_contextual_greek_lowercase_matching() -> Result<(), Box<dyn std::error::Error>> {
    // CPython 3.12 actual-source checks: final sigma depends on surrounding
    // cased letters, including across combining marks.
    for (bullet, extended, needle, expected_hit) in [
        ("- [ ] ΟΣ", "- [ ] ΟΣ detail", "ος", true),
        ("- [ ] ΟΣ", "- [ ] ΟΣ detail", "οσ", false),
        ("- [ ] ΣΣ", "- [ ] ΣΣ detail", "σς", true),
        ("- [ ] AΣ́", "- [ ] AΣ́ detail", "aς́", true),
    ] {
        let bullets = vec![bullet.to_owned(), extended.to_owned()];
        assert_eq!(
            match_indices(&bullets, needle),
            if expected_hit { vec![0] } else { vec![] }
        );
        let mut idx = parse_index(None)?;
        if let Some((_, section)) = idx.sections.get_mut(0) {
            *section = bullets.clone();
        }
        let op = EditOp {
            action: "resolve".to_owned(),
            text: needle.to_owned(),
            replacement: None,
            date: None,
        };
        assert_eq!(
            apply_index_edits(&mut idx, &[op], &[], "2026-09-01")?,
            expected_hit
        );
        let expected = if expected_hit {
            bullet.replacen("[ ]", "[x]", 1)
        } else {
            bullet.to_owned()
        };
        assert_eq!(
            idx.section("Open Questions"),
            Some([expected, extended.to_owned()].as_slice())
        );
    }
    Ok(())
}

#[test]
fn unicode_newer_than_shipped_python_does_not_retarget_edits() {
    // Unicode 16 added Garay casing; shipped Python 3.12 / Unicode 15
    // leaves these distinct characters unchanged.
    assert!(match_indices(&["- [ ] \u{10d50}".to_owned()], "\u{10d70}").is_empty());
    assert!(match_indices(&["- [ ] \u{1c89}".to_owned()], "\u{1c8a}").is_empty());
}
