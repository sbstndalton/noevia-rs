//! The recorded document extractions of noevia-core (`fixtures/node-extraction.v1.json`, written by
//! `record_extraction.cjs`: documents.cjs `extractDocumentText` / `completeDocument` and
//! document-sources.cjs `ingest` on scripted Docling and OCR sidecars), replayed through extract.rs
//! and document_ingest.rs with an engine answering from the same script. Per scenario the result
//! record (as JSON text), the figure bytes, the ordered sidecar calls, the OCR budget left and, for
//! ingest, the source record, the stored result file and the document folder's files must be core's.
//!
//! The scenarios marked `parity: false` are the ones core reads with its pdf.js reader; Rust has no
//! in-process reader, so its documented answer is asserted there instead (extract.rs module docs).
//! [`KNOWN`] lists the scenarios whose replay differs from core today, each with the reason; the test
//! fails when any other scenario differs, and when a known one starts to match (so the list is kept).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::{wtf16, JValue};
use server_projects::document_ingest;
use server_projects::extract::{
    self, Budgeted, Engine, EngineError, FigureBytes, Limits, OcrBudget, OcrCaps,
};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

const FIXTURE: &str = include_str!("fixtures/node-extraction.v1.json");
const PROJECT: &str = "qa-proj-r13-extraction";

/// Scenarios whose replay differs from core, and why (named differences; see noevia issues filed for them).
const KNOWN: &[(&str, &str)] = &[
    (
        "ocr-reply-number-as-text",
        "core matches an OCR reply page by `r.number === p.number` (documents.cjs \
         retryIncompletePages), so a page numbered \"2\" is 'OCR returned nothing'; \
         extract.rs apply_retry coerces with Number() and recovers it",
    ),
    (
        "ingest-new-figures",
        "two figures of one reply with the same index: core writes the first one's bytes to \
         <version>.fig<i> and gives each figure its own hash; extract.rs FigureBytes is keyed \
         by index, so the last bytes win the file and both hashes",
    ),
];

fn json(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "undefined".into())
}

fn num(x: f64) -> JValue {
    JValue::Num(x)
}

fn byte_ref(bytes: &[u8]) -> JValue {
    JValue::obj([
        ("length", num(bytes.len() as f64)),
        ("sha256", JValue::Str(extract::sha_hex(bytes))),
    ])
}

/// The scenario's bytes: `{ text }` as UTF-8, or `{ repeat, length }` (the pattern repeated).
fn bytes_of(spec: &JValue) -> Vec<u8> {
    if let Some(t) = spec.get("text").as_str() {
        return t.as_bytes().to_vec();
    }
    let pattern = spec.get("repeat").as_str().unwrap().as_bytes();
    let length = spec.get("length").as_num().unwrap() as usize;
    pattern.iter().copied().cycle().take(length).collect()
}

fn queues(v: &JValue) -> HashMap<String, VecDeque<JValue>> {
    let JValue::Obj(items) = v else {
        return HashMap::new();
    };
    items
        .iter()
        .map(|(k, q)| {
            let JValue::Arr(list) = q else {
                panic!("a reply queue is an array")
            };
            (k.clone(), list.iter().cloned().collect())
        })
        .collect()
}

fn engine_error(e: &JValue) -> EngineError {
    if e.get("timeout").truthy() {
        return EngineError {
            timed_out: true,
            ..EngineError::new("The operation was aborted due to timeout")
        };
    }
    EngineError {
        message: e.get("message").as_str().unwrap_or("").to_string(),
        status: e.get("status").as_num().map(|s| s as u16),
        permanent: e.get("permanent").truthy(),
        no_ranges: e.get("code").as_str() == Some("no-ranges"),
        timed_out: false,
    }
}

/// The two sidecars, answering from the scenario's script and noting every call as core's recorder did.
struct Script {
    name: String,
    docling: bool,
    ocr: bool,
    multimodal: bool,
    ranges: Option<bool>,
    caps: Option<OcrCaps>,
    docling_replies: RefCell<HashMap<String, VecDeque<JValue>>>,
    ocr_replies: RefCell<HashMap<String, VecDeque<JValue>>>,
    calls: RefCell<Vec<JValue>>,
    unscripted: RefCell<Vec<String>>,
}

impl Script {
    fn new(s: &JValue) -> Self {
        let flags = s.get("flags");
        let script = s.get("script");
        let caps = match script.get("caps") {
            JValue::Obj(_) => {
                let c = script.get("caps");
                Some(OcrCaps {
                    max_bytes: c.get("maxBytes").as_num().map(|n| n as usize),
                    dpis: match c.get("dpis") {
                        JValue::Arr(d) => d
                            .iter()
                            .filter_map(|x| x.as_num())
                            .map(|x| x as u32)
                            .collect(),
                        _ => Vec::new(),
                    },
                })
            }
            _ => None,
        };
        Script {
            name: s.get("name").as_str().unwrap().to_string(),
            docling: flags.get("docling").truthy(),
            ocr: flags.get("ocr").truthy(),
            multimodal: flags.get("multimodal").truthy(),
            ranges: script.get("ranges").as_bool(),
            caps,
            docling_replies: RefCell::new(queues(script.get("docling"))),
            ocr_replies: RefCell::new(queues(script.get("ocr"))),
            calls: RefCell::new(Vec::new()),
            unscripted: RefCell::new(Vec::new()),
        }
    }

    fn take(
        &self,
        which: &RefCell<HashMap<String, VecDeque<JValue>>>,
        kind: &str,
        key: &str,
    ) -> JValue {
        match which
            .borrow_mut()
            .get_mut(key)
            .and_then(VecDeque::pop_front)
        {
            Some(r) => r,
            None => {
                self.unscripted
                    .borrow_mut()
                    .push(format!("{}: {kind} {key}", self.name));
                JValue::obj([(
                    "error",
                    JValue::obj([(
                        "message",
                        JValue::Str(format!("QA script has no {kind} reply for {key}")),
                    )]),
                )])
            }
        }
    }
}

impl Engine for Script {
    fn docling_configured(&self) -> bool {
        self.docling
    }
    fn docling(
        &self,
        name: &str,
        bytes: &[u8],
        figures: bool,
        pages: Option<(u32, u32)>,
    ) -> Result<JValue, EngineError> {
        self.calls.borrow_mut().push(JValue::obj([
            ("fn", JValue::Str("docling".into())),
            ("name", JValue::Str(name.into())),
            ("bytes", byte_ref(bytes)),
            ("figures", JValue::Bool(figures)),
            (
                "pages",
                pages.map_or(JValue::Null, |(a, b)| {
                    JValue::Arr(vec![num(f64::from(a)), num(f64::from(b))])
                }),
            ),
        ]));
        let key = format!(
            "{}{}",
            pages.map_or("whole".to_string(), |(a, b)| format!("{a}-{b}")),
            if figures { "+figures" } else { "" }
        );
        let r = self.take(&self.docling_replies, "docling", &key);
        match r.get("error") {
            JValue::Undefined => Ok(r.get("ok").clone()),
            e => Err(engine_error(e)),
        }
    }
    fn docling_ranges(&self, bytes: usize) -> Option<bool> {
        self.calls.borrow_mut().push(JValue::obj([
            ("fn", JValue::Str("ranges".into())),
            ("bytes", num(bytes as f64)),
        ]));
        self.ranges
    }
    fn ocr_configured(&self) -> bool {
        self.ocr
    }
    fn ocr_pages(
        &self,
        bytes: &[u8],
        pages: &[u32],
        dpi: Option<u32>,
        timeout_ms: Option<u64>,
    ) -> Result<Vec<JValue>, EngineError> {
        self.calls.borrow_mut().push(JValue::obj([
            ("fn", JValue::Str("ocr".into())),
            ("bytes", byte_ref(bytes)),
            (
                "pages",
                JValue::Arr(pages.iter().map(|p| num(f64::from(*p))).collect()),
            ),
            ("dpi", dpi.map_or(JValue::Null, |d| num(f64::from(d)))),
            (
                "timeoutMs",
                timeout_ms.map_or(JValue::Null, |t| num(t as f64)),
            ),
        ]));
        let key = format!(
            "{}@{}",
            pages
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
            dpi.unwrap_or(300)
        );
        let r = self.take(&self.ocr_replies, "ocr", &key);
        match (r.get("error"), r.get("ok")) {
            (JValue::Undefined, JValue::Arr(list)) => Ok(list.clone()),
            (JValue::Undefined, _) => panic!("{}: an OCR reply is an array", self.name),
            (e, _) => Err(engine_error(e)),
        }
    }
    fn ocr_caps(&self) -> Option<OcrCaps> {
        self.calls
            .borrow_mut()
            .push(JValue::obj([("fn", JValue::Str("caps".into()))]));
        self.caps.clone()
    }
    fn multimodal(&self) -> bool {
        self.multimodal
    }
    fn busy_waits(&self) -> Vec<Duration> {
        vec![Duration::ZERO, Duration::ZERO]
    }
}

fn limits_of(l: &JValue) -> Limits {
    let n = |k: &str| l.get(k).as_num().unwrap() as usize;
    Limits {
        read_bytes: n("readBytes"),
        pages: n("pages"),
        text: n("text"),
        range_pages: n("rangePages"),
    }
}

fn budget_after(b: Option<&OcrBudget>) -> JValue {
    b.map_or(JValue::Null, |b| {
        JValue::obj([
            ("remaining", num(b.remaining.get() as f64)),
            ("deferredPages", num(b.deferred_pages.get() as f64)),
            ("deferredDocuments", num(b.deferred_documents.get() as f64)),
        ])
    })
}

/// One field that differs: what, core's value, Rust's value.
type Diff = (String, String, String);

fn check(diffs: &mut Vec<Diff>, what: &str, node: String, rust: String) {
    if node != rust {
        diffs.push((what.into(), node, rust));
    }
}

fn files_of(dir: &std::path::Path) -> JValue {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|r| {
            r.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    JValue::Arr(
        names
            .into_iter()
            .map(|n| {
                let b = std::fs::read(dir.join(&n)).unwrap();
                JValue::obj([
                    ("name", JValue::Str(n)),
                    ("length", num(b.len() as f64)),
                    ("sha256", JValue::Str(extract::sha_hex(&b))),
                ])
            })
            .collect(),
    )
}

/// Replays one scenario; the fields that differ from core.
fn replay(s: &JValue) -> Vec<Diff> {
    let script = Script::new(s);
    let name = s.get("file").get("name").as_str().unwrap();
    let bytes = bytes_of(s.get("file").get("bytes"));
    let limits = limits_of(s.get("limits"));
    let budgeted = s.get("budget").as_num().map(|n| Budgeted {
        inner: &script,
        budget: OcrBudget::new(n as usize),
    });
    let engine: &dyn Engine = match &budgeted {
        Some(b) => b,
        None => &script,
    };
    let node = s.get("node");
    let mut diffs = Vec::new();
    let kind = s.get("kind").as_str().unwrap();
    if kind == "ingest" {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("users").join("qa-r13");
        std::fs::create_dir_all(&user).unwrap();
        let docs = user
            .join("project-documents")
            .join(extract::sha_hex(PROJECT.as_bytes()));
        let want = node.get("ingest");
        if !s.get("stored").is_nullish() {
            std::fs::create_dir_all(&docs).unwrap();
            let version = want.get("version").as_str().unwrap();
            std::fs::write(docs.join(format!("{version}.json")), json(s.get("stored"))).unwrap();
        }
        let previous = match s.get("previous") {
            JValue::Undefined => None,
            p => Some(p),
        };
        let got = document_ingest::ingest(&user, PROJECT, name, &bytes, previous, engine, &limits)
            .expect("ingest");
        check(
            &mut diffs,
            "record",
            want.get("record").as_str().unwrap().into(),
            json(&got.file),
        );
        let version = got
            .file
            .get("document")
            .get("version")
            .as_str()
            .unwrap_or("");
        let stored =
            std::fs::read_to_string(docs.join(format!("{version}.json"))).unwrap_or_default();
        check(
            &mut diffs,
            "resultFile",
            want.get("resultFile").as_str().unwrap().into(),
            stored,
        );
        check(
            &mut diffs,
            "files",
            json(want.get("files")),
            json(&files_of(&docs)),
        );
        check(
            &mut diffs,
            "outcome",
            json(want.get("outcome")),
            json(&JValue::Str(got.outcome.into())),
        );
    } else {
        let mut figs = FigureBytes::new();
        let got = if kind == "extract" {
            extract::extract_document_text(name, &bytes, engine, &limits, &mut figs)
        } else {
            Ok(extract::complete_document(
                name,
                &bytes,
                s.get("input").clone(),
                engine,
                &limits,
                &mut figs,
            ))
        };
        match got {
            Ok(out) => {
                let node_result = match node.get("result").as_str() {
                    Some(t) => t.to_string(),
                    None => format!("threw {}", json(node.get("threw"))),
                };
                check(&mut diffs, "result", node_result, json(&out));
                let fig_bytes: Vec<JValue> = match out.get("figures") {
                    JValue::Arr(list) => list
                        .iter()
                        .filter_map(|f| f.get("index").as_num())
                        .filter_map(|i| figs.get(&(i as i64)).map(|b| byte_ref(b.as_slice())))
                        .collect(),
                    _ => Vec::new(),
                };
                check(
                    &mut diffs,
                    "figureBytes",
                    json(node.get("figureBytes")),
                    json(&JValue::Arr(fig_bytes)),
                );
            }
            Err(extract::Permanent(m)) => {
                let rust = JValue::obj([("status", num(422.0)), ("message", JValue::Str(m))]);
                check(&mut diffs, "threw", json(node.get("threw")), json(&rust));
            }
        }
    }
    check(
        &mut diffs,
        "calls",
        json(node.get("calls")),
        json(&JValue::Arr(script.calls.borrow().clone())),
    );
    check(
        &mut diffs,
        "budgetAfter",
        json(node.get("budgetAfter")),
        json(&budget_after(budgeted.as_ref().map(|b| &b.budget))),
    );
    for u in script.unscripted.borrow().iter() {
        diffs.push(("unscripted".into(), String::new(), u.clone()));
    }
    diffs
}

fn short(t: &str) -> String {
    t.chars().take(1500).collect()
}

#[test]
fn extraction_matches_core() {
    let f = wtf16::parse(FIXTURE).expect("the fixture is JSON");
    let JValue::Arr(scenarios) = f.get("scenarios") else {
        panic!("scenarios is an array")
    };
    assert!(scenarios.len() >= 50, "{} scenarios", scenarios.len());
    let mut report = Vec::new();
    let mut matched = 0usize;
    for s in scenarios {
        let name = s.get("name").as_str().unwrap();
        if !s.get("parity").truthy() {
            continue;
        }
        let diffs = replay(s);
        let known = KNOWN.iter().any(|(k, _)| *k == name);
        match (diffs.is_empty(), known) {
            (true, false) => matched += 1,
            (true, true) => report.push(format!("{name}: listed in KNOWN but now matches core")),
            (false, true) => {}
            (false, false) => {
                for (what, node, rust) in diffs {
                    report.push(format!(
                        "{name} [{what}]\n  core: {}\n  rust: {}",
                        short(&node),
                        short(&rust)
                    ));
                }
            }
        }
    }
    assert!(
        report.is_empty(),
        "{} matched; differences:\n{}",
        matched,
        report.join("\n")
    );
}

/// Core reads a PDF with its pdf.js reader when Docling is not configured; Rust has none and stores
/// the retryable "not configured" failure, so the document stays queued until Docling can read it.
#[test]
fn without_docling_rust_stores_the_documented_retryable_failure() {
    let f = wtf16::parse(FIXTURE).expect("the fixture is JSON");
    let JValue::Arr(scenarios) = f.get("scenarios") else {
        panic!("scenarios is an array")
    };
    let pdfjs: Vec<&JValue> = scenarios
        .iter()
        .filter(|s| !s.get("parity").truthy())
        .collect();
    assert_eq!(pdfjs.len(), 1, "at most one pdf.js scenario is recorded");
    let s = pdfjs[0];
    let node_calls = json(s.get("node").get("calls"));
    assert!(
        node_calls.contains(r#""fn":"readPages""#),
        "core used its pdf.js reader: {node_calls}"
    );
    let script = Script::new(s);
    let bytes = bytes_of(s.get("file").get("bytes"));
    let out = extract::extract_document_text(
        s.get("file").get("name").as_str().unwrap(),
        &bytes,
        &script,
        &limits_of(s.get("limits")),
        &mut FigureBytes::new(),
    )
    .unwrap();
    assert_eq!(
        json(&out),
        r#"{"text":"","pages":0,"pageTexts":[],"truncated":false,"state":"failed","retryable":true,"error":"Document extraction is not configured."}"#
    );
    assert!(script.calls.borrow().is_empty(), "no sidecar is asked");
}
