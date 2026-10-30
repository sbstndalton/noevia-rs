//! core research-service.cjs's pure parts: the report's two files and the public view of a job.
//!
//! The journal holds a result as parsed JSON whose object keys may have been re-sorted on the way (the
//! store keeps a parsed value). The bytes of `.sources.json` have to be the same each time they are
//! computed, because an explicit retry compares them with the hash recorded before the first write: so the
//! file is written in core's key order at every level, whatever order the value came in. Core computed its
//! retry from the journal's sorted keys and so never matched its own hash for this file (see the README).
use js_json::{wtf16, JValue};
use serde_json::Value;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// A serde value, as a [`JValue`], sorted as it stands.
pub fn to_j(v: &Value) -> JValue {
    match v {
        Value::Null => JValue::Null,
        Value::Bool(b) => JValue::Bool(*b),
        Value::Number(n) => JValue::Num(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => JValue::Str(s.clone()),
        Value::Array(a) => JValue::Arr(a.iter().map(to_j).collect()),
        Value::Object(o) => JValue::Obj(o.iter().map(|(k, v)| (k.clone(), to_j(v))).collect()),
    }
}

/// core `slug(text)`.
pub fn slug(text: &str) -> String {
    let folded: String = text
        .nfkd()
        .filter(|c| !('\u{300}'..='\u{36f}').contains(c))
        .collect::<String>()
        .to_lowercase();
    let mut out = String::new();
    let mut dash = false;
    for c in folded.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else {
            dash = true;
        }
    }
    let cut: String = out.chars().take(60).collect();
    let cut = cut.trim_end_matches('-');
    if cut.is_empty() {
        "research".to_owned()
    } else {
        cut.to_owned()
    }
}

/// `new Date(ms).toISOString().slice(0, 10)`.
pub fn iso_date(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// One file to save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub name: String,
    pub text: String,
}

impl File {
    pub fn hash(&self) -> String {
        sha_hex(self.text.as_bytes())
    }
    pub fn bytes(&self) -> u64 {
        self.text.len() as u64
    }
}

pub fn sha_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `obj` with `keys` first, in that order; absent keys are left out.
fn ordered(v: &Value, keys: &[&str]) -> JValue {
    let Value::Object(obj) = v else {
        return to_j(v);
    };
    let mut items = Vec::new();
    for key in keys {
        if let Some(val) = obj.get(*key) {
            items.push(((*key).to_owned(), to_j(val)));
        }
    }
    for (k, val) in obj {
        if !keys.contains(&k.as_str()) {
            items.push((k.clone(), to_j(val)));
        }
    }
    JValue::Obj(items)
}

fn ordered_list(v: Option<&Value>, keys: &[&str]) -> JValue {
    match v {
        Some(Value::Array(a)) => JValue::Arr(a.iter().map(|x| ordered(x, keys)).collect()),
        _ => JValue::Arr(Vec::new()),
    }
}

const NOTED_KEYS: [&str; 4] = ["question", "text", "reason", "cited"];

fn source_entry(s: &Value) -> JValue {
    let mut items = Vec::new();
    for key in ["id", "kind", "url", "file", "title", "retrievedAt"] {
        if let Some(v) = s.get(key) {
            items.push((key.to_owned(), to_j(v)));
        }
    }
    match s.get("excerpts") {
        Some(Value::Array(_)) => items.push((
            "excerpts".to_owned(),
            ordered_list(s.get("excerpts"), &["sha256", "text"]),
        )),
        Some(other) => items.push(("excerpts".to_owned(), to_j(other))),
        None => {}
    }
    items.push((
        "sentences".to_owned(),
        match s.get("sentences") {
            Some(other) if !other.is_array() => to_j(other),
            found => ordered_list(found, &["id", "text"]),
        },
    ));
    JValue::Obj(items)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        _ => true,
    }
}

fn report_pair(result: &Value, base: &str) -> [File; 2] {
    let markdown = result
        .get("markdown")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let claims = match result.get("claims") {
        Some(c) if truthy(Some(c)) => {
            ordered(c, &["total", "supported", "flagged", "dropped", "uncited"])
        }
        _ => JValue::Null,
    };
    let sources = result
        .get("sources")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(source_entry).collect())
        .unwrap_or_default();
    let mut items: Vec<(String, JValue)> = vec![(
        "question".to_owned(),
        result.get("question").map_or(JValue::Undefined, to_j),
    )];
    items.push((
        "partial".to_owned(),
        JValue::Bool(truthy(result.get("partial"))),
    ));
    items.push((
        "citationValidity".to_owned(),
        result
            .get("citationValidity")
            .map_or(JValue::Undefined, to_j),
    ));
    items.push((
        "webCalls".to_owned(),
        result.get("webCalls").map_or(JValue::Undefined, to_j),
    ));
    items.push(("claims".to_owned(), claims));
    items.push((
        "dropped".to_owned(),
        ordered_list(result.get("dropped"), &NOTED_KEYS),
    ));
    items.push((
        "flagged".to_owned(),
        ordered_list(result.get("flagged"), &NOTED_KEYS),
    ));
    items.push(("sources".to_owned(), JValue::Arr(sources)));
    let text = wtf16::stringify_pretty(&JValue::Obj(items)).unwrap_or_default() + "\n";
    [
        File {
            name: format!("{base}.md"),
            text: markdown,
        },
        File {
            name: format!("{base}.sources.json"),
            text,
        },
    ]
}

/// core `reportFiles(result, when, taken)`: never overwrites an earlier report; the same question on the
/// same day gets a numbered name.
pub fn report_files(result: &Value, when_ms: i64, taken: &dyn Fn(&str) -> bool) -> [File; 2] {
    let question = result.get("question").and_then(Value::as_str).unwrap_or("");
    let stem = format!("Research {} {}", iso_date(when_ms), slug(question));
    let mut base = stem.clone();
    let mut n = 2;
    while taken(&format!("{base}.md")) || taken(&format!("{base}.sources.json")) {
        base = format!("{stem} ({n})");
        n += 1;
    }
    report_pair(result, &base)
}

/// The pair under an existing base name.
pub fn report_files_named(result: &Value, base: &str) -> [File; 2] {
    report_pair(result, base)
}

/// core `hasReport`.
pub fn has_report(result: &Value) -> bool {
    result.get("markdown").is_some_and(Value::is_string)
        && result.get("sources").is_some_and(Value::is_array)
}

/// core `pendingReportFile`: a pending file can be retried only when these are the exact bytes prepared.
pub fn pending_report_file(result: &Value, pending: &Value) -> Option<File> {
    if !has_report(result) {
        return None;
    }
    let name = pending.get("name").and_then(Value::as_str)?;
    let first = if name.ends_with(".sources.json") {
        false
    } else if name.ends_with(".md") {
        true
    } else {
        return None;
    };
    let [md, sources] = report_pair(result, "x");
    let mut file = if first { md } else { sources };
    file.name = name.to_owned();
    (pending.get("bytes").and_then(Value::as_u64) == Some(file.bytes())
        && pending.get("hash").and_then(Value::as_str) == Some(file.hash().as_str()))
    .then_some(file)
}

fn last_segment(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// The project's file that proves a pending write landed: the exact name and bytes, indexed.
pub fn indexed_pending(files: &[JValue], pending: &Value) -> bool {
    let (Some(name), Some(hash), Some(bytes)) = (
        pending.get("name").and_then(Value::as_str),
        pending.get("hash").and_then(Value::as_str),
        pending.get("bytes").and_then(Value::as_f64),
    ) else {
        return false;
    };
    files.iter().any(|f| {
        f.get("name")
            .as_str()
            .is_some_and(|n| last_segment(n) == name)
            && f.get("attachment").get("id").as_str() == Some(hash)
            && f.get("attachment").get("bytes").as_num() == Some(bytes)
    })
}

fn artifact_names(job: &Value) -> Vec<String> {
    job.get("artifacts")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A saved `.sources.json` among the artifacts: it is written last, so the report is whole.
pub fn sources_saved(job: &Value) -> bool {
    artifact_names(job)
        .iter()
        .any(|a| a.ends_with(".sources.json"))
}

fn result_sections(result: &Value) -> f64 {
    result
        .get("sections")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

/// core `savePartial`'s eligibility, and the view's `canSavePartial` without its pending rule.
pub fn can_save(job: &Value) -> bool {
    let status = job.get("status").and_then(Value::as_str).unwrap_or("");
    let Some(r) = job.get("result").filter(|r| truthy(Some(r))) else {
        return false;
    };
    ((status == "cancelled" && result_sections(r) > 0.0) || (status == "failed" && has_report(r)))
        && !sources_saved(job)
}

/// core `view(job, project)`: the public view, no excerpts, bounded markdown. `files`: the project's
/// current list, `None` when the project is gone.
pub fn view(job: &Value, files: Option<&[JValue]>) -> JValue {
    let f = |key: &str| job.get(key).map_or(JValue::Undefined, to_j);
    let null_of = |v: Option<&Value>| v.map_or(JValue::Null, to_j);
    let result = job.get("result").filter(|r| truthy(Some(r)));
    let pending = job.get("pendingFile").filter(|p| truthy(Some(p)));
    let indexed = pending.is_some_and(|p| indexed_pending(files.unwrap_or(&[]), p));
    let result_view = result.map_or(JValue::Null, |r| {
        let claims = r.get("claims").filter(|c| truthy(Some(c)));
        let markdown = r.get("markdown").and_then(Value::as_str).unwrap_or("");
        JValue::obj([
            ("question", null_of(r.get("question"))),
            ("partial", r.get("partial").map_or(JValue::Undefined, to_j)),
            (
                "sections",
                r.get("sections").map_or(JValue::Undefined, to_j),
            ),
            (
                "questions",
                r.get("questions").map_or(JValue::Undefined, to_j),
            ),
            (
                "citationValidity",
                r.get("citationValidity").map_or(JValue::Undefined, to_j),
            ),
            (
                "citations",
                r.get("citations").map_or(JValue::Undefined, to_j),
            ),
            ("claims", claims.map_or(JValue::Null, to_j)),
            (
                "webCalls",
                r.get("webCalls").map_or(JValue::Undefined, to_j),
            ),
            ("markdown", JValue::Str(wtf16::slice(markdown, 200_000))),
            (
                "sources",
                JValue::Num(
                    r.get("sources")
                        .and_then(Value::as_array)
                        .map_or(0.0, |a| a.len() as f64),
                ),
            ),
        ])
    });
    let uncertain = pending.map_or(JValue::Null, |p| {
        let retry = !indexed
            && files.is_some()
            && result.is_some_and(|r| pending_report_file(r, p).is_some());
        JValue::obj([
            ("name", p.get("name").map_or(JValue::Undefined, to_j)),
            ("canReconcile", JValue::Bool(indexed)),
            ("canRetryCreateOnly", JValue::Bool(retry)),
        ])
    });
    JValue::obj([
        ("id", f("id")),
        ("status", f("status")),
        ("stage", f("stage")),
        ("plan", f("plan")),
        ("error", f("error")),
        ("createdAt", f("createdAt")),
        ("updatedAt", f("updatedAt")),
        ("checkpoint", f("checkpoint")),
        (
            "artifacts",
            JValue::Arr(artifact_names(job).into_iter().map(JValue::Str).collect()),
        ),
        ("result", result_view),
        (
            "canSavePartial",
            JValue::Bool(pending.is_none() && can_save(job)),
        ),
        ("saveUncertain", uncertain),
    ])
}
