//! Project document extraction (sbstndalton/noevia#2191 round 5, `rust-document-extraction`): core
//! documents.cjs's Docling path, answer for answer, over an [`Engine`] (the Docling and OCR sidecars;
//! noevia-server's HTTP client in production, a synthetic one in tests).
//!
//! Docling (services/docling), with the OCR sidecar for the per-page retry of pages Docling left
//! incomplete, is THE extraction engine for new project documents. There is no in-process PDF
//! reader: where core's pdf.js walk (documents-native.cjs) would have run (Docling not configured,
//! or a PDF too large for one request whose worker does not read page ranges), the result is the
//! retryable failure core stores for an unreachable worker, and the document's job stays queued
//! ([`crate::extract_queue`]) until Docling can read it.
//!
//! What is Node's, kept as it is:
//! - The routing: plain-text names are never documents; with Docling configured its FORMATS are,
//!   without it only `.pdf` (a PDF is accepted and queued).
//! - The first pass: one whole-document request, or for a PDF over 25 MiB the first range when the
//!   worker reads ranges (`/health` `"ranges": true`, `rangedMaxBytes`).
//! - The result record (`assemble`, `doclingPages`, the figures check), byte for byte as
//!   JSON.stringify writes it, with its key order.
//! - The completion pass (`completeDocument`, marker `ranges-ocr-retry-v1`): the pages past the
//!   first window in ranges through Docling, up to the page and text ceilings
//!   (source-limits.cjs `documentLimits`), then each incomplete page retried alone through OCR at
//!   400 dpi (at most 20 pages, 3 per request, the busy waits, the attempt bound).
//! - The large-document lock: one PDF over 25 MiB at a time on the host, a waiter gives up
//!   after two minutes as "busy" (503), which is a passing failure. In-process it is a Mutex; across
//!   processes it is [`LARGE_LOCK_FILE`] in the data directory, which core's large-document-lock.cjs
//!   takes too (review #2191 r4+5, #697: Node and the front must never extract two at once).
use crate::files::js_string;
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// compact project/RAG text, not the page store
pub const EXTRACT_CAP: usize = 200_000;
pub const PAGE_TEXT_CAP: usize = 200_000;
/// The most one request to a sidecar carries (their LIMIT, 25 MiB); also the large-document line.
pub const WHOLE_BYTES: usize = 25 * 1024 * 1024;
pub const OCR_MAX_BYTES: usize = 25 * 1024 * 1024;
pub const COMPLETION: &str = "ranges-ocr-retry-v1";
pub const RETRY_PAGE_CAP: usize = 20;
pub const RETRY_BATCH: usize = 3;
pub const RETRY_DPI: u32 = 400;
pub const RETRY_ATTEMPTS: f64 = 3.0;
const RETRY_STATUSES: [&str; 4] = ["degraded", "failed", "unreadable", "ocr-needed"];
const COMPLETE_STATUSES: [&str; 3] = ["native", "blank", "ocr"];
const FIGURE_CAP: usize = 64;
const FIGURE_BYTES: usize = 4 * 1024 * 1024;
/// docling.cjs VERSION: part of the cache key.
pub const DOCLING_VERSION: &str = "docling-2.129-layout-tableformer-tesseract-v3-native-fallback";
/// docling.cjs FORMATS (services/docling SUPPORTED).
pub const DOCLING_FORMATS: [&str; 18] = [
    ".pdf", ".docx", ".pptx", ".xlsx", ".odt", ".odp", ".ods", ".html", ".htm", ".md", ".epub",
    ".csv", ".png", ".jpg", ".jpeg", ".tiff", ".tif", ".bmp",
];
const PLAIN_TEXT: [&str; 15] = [
    "txt", "md", "markdown", "csv", "json", "yaml", "yml", "ts", "tsx", "js", "jsx", "py", "sh",
    "css", "",
];
pub const NOT_CONFIGURED: &str = "Document extraction is not configured.";
pub const NO_RANGES: &str =
    "The document extractor does not read page ranges yet; update the docling service.";
const OCR_LINE: &str = "[OCR transcription — verify numbers against original]\n";

/// A failure of one sidecar request, as core's clients raise it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineError {
    /// `err.message`.
    pub message: String,
    /// `err.status` (503 busy, 400 refused, the OCR worker's HTTP status).
    pub status: Option<u16>,
    /// docling.cjs `permanent` (415, 422): the document, not the worker.
    pub permanent: bool,
    /// docling.cjs `code: 'no-ranges'`.
    pub no_ranges: bool,
    /// The request's own deadline ran out (an AbortSignal timeout in Node).
    pub timed_out: bool,
}

impl EngineError {
    pub fn new(message: &str) -> Self {
        EngineError {
            message: message.to_string(),
            ..Default::default()
        }
    }
}

/// What the OCR worker says it reads (ocr.cjs `capabilities`): `{ maxBytes?, dpis? }`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OcrCaps {
    pub max_bytes: Option<usize>,
    pub dpis: Vec<u32>,
}

/// The two sidecars. Every method is called at most as often as core calls its client.
pub trait Engine {
    /// document-workers.cjs `configured('documents')`.
    fn docling_configured(&self) -> bool;
    /// docling.cjs `extractDocument(name, bytes, { figures, pages })`: the parsed reply.
    fn docling(
        &self,
        name: &str,
        bytes: &[u8],
        figures: bool,
        pages: Option<(u32, u32)>,
    ) -> Result<JValue, EngineError>;
    /// docling.cjs `rangesSupported({ bytes })`: `Some(true|false)`, `None` when it cannot be asked.
    fn docling_ranges(&self, bytes: usize) -> Option<bool>;
    /// document-workers.cjs `configured('ocr')`.
    fn ocr_configured(&self) -> bool;
    /// ocr.cjs `extractPages(bytes, pages, { dpi, timeoutMs })`: the reply's `pages`.
    fn ocr_pages(
        &self,
        bytes: &[u8],
        pages: &[u32],
        dpi: Option<u32>,
        timeout_ms: Option<u64>,
    ) -> Result<Vec<JValue>, EngineError>;
    /// ocr.cjs `capabilities()`; `None` when the worker cannot be asked.
    fn ocr_caps(&self) -> Option<OcrCaps>;
    /// multimodal-rag.cjs `enabled()` (NOEVIA_MULTIMODAL_RAG).
    fn multimodal(&self) -> bool;
    /// The host-wide large-document lock file ([`LARGE_LOCK_FILE`] in UI_DATA_DIR); `None` holds
    /// the lock in this process only (tests).
    fn large_lock_file(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// The busy waits (BUSY_WAITS_MS, 5 s and 15 s); a test shortens them.
    fn busy_waits(&self) -> Vec<Duration> {
        vec![Duration::from_secs(5), Duration::from_secs(15)]
    }
    /// The OCR retry pages the source sync this read belongs to may still spend (documents.cjs
    /// `ocrBudget`, noevia#2191 round 8); `None` for an upload or a queued job, which have none.
    fn ocr_budget(&self) -> Option<&OcrBudget> {
        None
    }
    /// docx.cjs `extract(bytes)`: the OCR worker's `/extract-docx`, the parsed reply `{ text, truncated }`.
    fn extract_docx(&self, _bytes: &[u8]) -> Result<JValue, EngineError> {
        Err(EngineError::new(
            "DOCX reader is unavailable. Re-upload after the document worker is configured.",
        ))
    }
    /// ocr.cjs's `/reduce-pdf` (pdf-reduce.cjs): the PDF compressed to fit, or its text, by the
    /// OCR worker; the parsed reply. `None` when no OCR worker is configured.
    fn reduce_pdf(&self, _bytes: &[u8]) -> Option<Result<JValue, EngineError>> {
        None
    }
}

/// documents.cjs `ocrBudget()` and its running counts (#1871, #1878): the OCR retry pages one source
/// sync may spend across all its documents (`PROJECT_SYNC_OCR_PAGES`, default 40), and what it pushed to
/// the next sync.
#[derive(Debug)]
pub struct OcrBudget {
    pub remaining: std::cell::Cell<usize>,
    pub deferred_pages: std::cell::Cell<usize>,
    pub deferred_documents: std::cell::Cell<usize>,
}

impl OcrBudget {
    pub fn new(pages: usize) -> Self {
        OcrBudget {
            remaining: std::cell::Cell::new(pages),
            deferred_pages: std::cell::Cell::new(0),
            deferred_documents: std::cell::Cell::new(0),
        }
    }
    /// Takes up to `want` pages; the number granted.
    fn take(&self, want: usize) -> usize {
        let granted = want.min(self.remaining.get());
        self.remaining.set(self.remaining.get() - granted);
        granted
    }
}

/// An engine that spends one sync's OCR retry allowance ([`OcrBudget`]); everything else is `inner`.
pub struct Budgeted<'a> {
    pub inner: &'a dyn Engine,
    pub budget: OcrBudget,
}

impl Engine for Budgeted<'_> {
    fn docling_configured(&self) -> bool {
        self.inner.docling_configured()
    }
    fn docling(
        &self,
        name: &str,
        bytes: &[u8],
        figures: bool,
        pages: Option<(u32, u32)>,
    ) -> Result<JValue, EngineError> {
        self.inner.docling(name, bytes, figures, pages)
    }
    fn docling_ranges(&self, bytes: usize) -> Option<bool> {
        self.inner.docling_ranges(bytes)
    }
    fn ocr_configured(&self) -> bool {
        self.inner.ocr_configured()
    }
    fn ocr_pages(
        &self,
        bytes: &[u8],
        pages: &[u32],
        dpi: Option<u32>,
        timeout_ms: Option<u64>,
    ) -> Result<Vec<JValue>, EngineError> {
        self.inner.ocr_pages(bytes, pages, dpi, timeout_ms)
    }
    fn ocr_caps(&self) -> Option<OcrCaps> {
        self.inner.ocr_caps()
    }
    fn multimodal(&self) -> bool {
        self.inner.multimodal()
    }
    fn large_lock_file(&self) -> Option<std::path::PathBuf> {
        self.inner.large_lock_file()
    }
    fn busy_waits(&self) -> Vec<Duration> {
        self.inner.busy_waits()
    }
    fn ocr_budget(&self) -> Option<&OcrBudget> {
        Some(&self.budget)
    }
    fn extract_docx(&self, bytes: &[u8]) -> Result<JValue, EngineError> {
        self.inner.extract_docx(bytes)
    }
    fn reduce_pdf(&self, bytes: &[u8]) -> Option<Result<JValue, EngineError>> {
        self.inner.reduce_pdf(bytes)
    }
}

/// source-limits.cjs `documentLimits(env)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub read_bytes: usize,
    pub pages: usize,
    pub text: usize,
    pub range_pages: usize,
}

fn positive(raw: Option<String>, fallback: usize) -> usize {
    match raw {
        Some(r) if !r.is_empty() => match r.trim().parse::<f64>() {
            Ok(n) if n > 0.0 && n.fract() == 0.0 && n <= 9_007_199_254_740_991.0 => n as usize,
            _ => fallback,
        },
        _ => fallback,
    }
}

impl Limits {
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok();
        Limits {
            read_bytes: positive(get("PROJECT_DOCUMENT_READ_BYTES"), 96 * 1024 * 1024),
            pages: positive(get("PROJECT_DOCUMENT_PAGE_LIMIT"), 1000),
            text: positive(get("PROJECT_DOCUMENT_TEXT_LIMIT"), 8_000_000),
            range_pages: positive(get("PROJECT_DOCUMENT_RANGE_PAGES"), 25).min(200),
        }
    }
}

/// documents.cjs `EXTRACTOR_VERSION` for the Docling backend (the only one here).
pub fn extractor_version(figures: bool) -> String {
    format!(
        "docling-pages-v1:{DOCLING_VERSION}{}",
        if figures { ":figures-v1" } else { "" }
    )
}

/// Whether figures are asked for: documents.cjs `figuresOn()`.
pub fn figures_on(engine: &dyn Engine) -> bool {
    engine.docling_configured() && engine.multimodal()
}

fn suffix(name: &str) -> String {
    match name.rfind('.') {
        Some(i)
            if name[i + 1..].bytes().all(|b| b.is_ascii_alphanumeric()) && i + 1 < name.len() =>
        {
            name[i..].to_ascii_lowercase()
        }
        _ => String::new(),
    }
}

fn plain_text(name: &str) -> bool {
    let s = suffix(name);
    !s.is_empty() && PLAIN_TEXT.contains(&&s[1..])
}

pub fn is_pdf(name: &str) -> bool {
    suffix(name) == ".pdf"
}

/// documents.cjs `isDocument(name, doclingEnabled)`. Without Docling only a PDF (which is queued
/// until Docling reads it: there is no in-process reader).
pub fn is_document(name: &str, docling: bool) -> bool {
    if plain_text(name) {
        return false;
    }
    if docling {
        DOCLING_FORMATS.contains(&suffix(name).as_str())
    } else {
        is_pdf(name)
    }
}

/// documents.cjs `documentExtensions(doclingEnabled)`.
pub fn document_extensions(docling: bool) -> Vec<&'static str> {
    if docling {
        DOCLING_FORMATS.to_vec()
    } else {
        vec![".pdf"]
    }
}

// ── small JS helpers ─────────────────────────────────────────────────────

fn set(v: &mut JValue, key: &str, value: JValue) {
    if let JValue::Obj(items) = v {
        wtf16::set(items, key, value);
    }
}

fn remove(v: &mut JValue, key: &str) {
    if let JValue::Obj(items) = v {
        items.retain(|(k, _)| k != key);
    }
}

fn s(v: &JValue, key: &str) -> String {
    v.get(key).as_str().unwrap_or("").to_string()
}

fn n(v: &JValue, key: &str) -> f64 {
    js_number(v.get(key))
}

/// `Number(v)` for the values a reply holds (NaN where JS gives NaN).
fn js_number(v: &JValue) -> f64 {
    match v {
        JValue::Num(x) => *x,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Null => 0.0,
        JValue::Str(t) => {
            let t = crate::js::trim(t);
            if t.is_empty() {
                0.0
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        _ => f64::NAN,
    }
}

/// `Number(x) || fallback`.
fn or_num(x: f64, fallback: f64) -> f64 {
    if x == 0.0 || x.is_nan() {
        fallback
    } else {
        x
    }
}

fn len16(t: &str) -> usize {
    wtf16::len(t)
}

fn num_text(x: f64) -> String {
    js_json::number_to_string(x)
}

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn pages_of(out: &JValue) -> Vec<JValue> {
    match out.get("pageTexts") {
        JValue::Arr(a) => a.clone(),
        _ => Vec::new(),
    }
}

// ── the record ───────────────────────────────────────────────────────────

/// documents.cjs `assemble(pageTexts, totalPages, { retryable, error, extraTruncated, stoppedAt })`.
pub fn assemble(
    page_texts: Vec<JValue>,
    total: f64,
    retryable: bool,
    error: Option<String>,
    extra_truncated: bool,
    stopped_at: Option<JValue>,
) -> JValue {
    let full = page_texts
        .iter()
        .map(|p| {
            let text = s(p, "text");
            let shown = if text.is_empty() {
                format!("({})", js_string(p.get("status")))
            } else {
                text
            };
            format!("[Page {}]\n{}", num_text(n(p, "number")), shown)
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let readable = page_texts.iter().any(|p| !s(p, "text").is_empty());
    let missing = total > page_texts.len() as f64;
    let incomplete = page_texts
        .iter()
        .any(|p| !COMPLETE_STATUSES.contains(&s(p, "status").as_str()))
        || missing;
    let truncated = len16(&full) > EXTRACT_CAP
        || page_texts.iter().any(|p| p.get("truncated").truthy())
        || missing
        || extra_truncated;
    let state = if !readable && incomplete {
        "failed"
    } else if incomplete || truncated {
        "partial"
    } else {
        "ready"
    };
    let mut out = obj(vec![
        (
            "text",
            JValue::Str(if readable {
                wtf16::slice(&full, EXTRACT_CAP)
            } else {
                String::new()
            }),
        ),
        ("pages", JValue::Num(total)),
        ("pageTexts", JValue::Arr(page_texts)),
        ("truncated", JValue::Bool(truncated)),
        ("state", JValue::Str(state.into())),
        ("retryable", JValue::Bool(retryable)),
        ("error", error.map(JValue::Str).unwrap_or(JValue::Undefined)),
    ]);
    if let Some(stop) = stopped_at {
        set(&mut out, "stoppedAt", stop);
    }
    out
}

fn reason_ok(r: &str) -> bool {
    (1..=40).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
}

/// documents.cjs `doclingPages(body, figures)`.
pub fn docling_pages(body: &JValue, figures: bool) -> Vec<JValue> {
    let JValue::Arr(pages) = body.get("pages") else {
        return Vec::new();
    };
    pages
        .iter()
        .map(|p| {
            let text = if p.get("text").truthy() {
                js_string(p.get("text"))
            } else {
                String::new()
            };
            let status = if p.get("status").truthy() {
                js_string(p.get("status"))
            } else {
                "failed".into()
            };
            let method = if p.get("method").as_str() == Some("pdfium") {
                "pdfium"
            } else {
                "docling"
            };
            let mut page = obj(vec![
                ("number", JValue::Num(js_number(p.get("number")))),
                ("text", JValue::Str(text)),
                ("status", JValue::Str(status)),
                ("method", JValue::Str(method.into())),
                ("truncated", JValue::Bool(p.get("truncated").truthy())),
            ]);
            if let Some(r) = p.get("reason").as_str().filter(|r| reason_ok(r)) {
                set(&mut page, "reason", JValue::Str(r.into()));
            }
            if figures && p.get("ocr").as_bool() == Some(true) {
                set(&mut page, "ocr", JValue::Bool(true));
            }
            page
        })
        .collect()
}

/// multimodal-rag.cjs `scannedPage(page)`.
fn scanned_page(page: &JValue) -> bool {
    page.get("ocr").as_bool() == Some(true)
        || s(page, "status") == "ocr"
        || js_string(page.get("method"))
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|w| w == "ocr")
}

/// Figure bytes waiting to be stored (documents.cjs keeps them on the figure as `bytes`); keyed by
/// the figure's index.
pub type FigureBytes = HashMap<i64, Vec<u8>>;

fn magic(mime: &str, b: &[u8]) -> Option<&'static str> {
    match mime {
        "image/png" if b.len() > 8 && b.get(..4) == Some(&[0x89, 0x50, 0x4e, 0x47][..]) => {
            Some("png")
        }
        "image/jpeg" if b.len() > 3 && b.get(..3) == Some(&[0xff, 0xd8, 0xff][..]) => Some("jpg"),
        "image/webp"
            if b.len() > 12
                && b.get(0..4) == Some(&b"RIFF"[..])
                && b.get(8..12) == Some(&b"WEBP"[..]) =>
        {
            Some("webp")
        }
        _ => None,
    }
}

/// documents.cjs `parseFigures(list, pageTexts)`: the figures a reply carries, checked. Each kept
/// figure is `{ page, index, mime, ext, caption?, text? }` with its bytes in `bytes`.
fn parse_figures(list: &JValue, pages: &[JValue], bytes_out: &mut FigureBytes) -> Vec<JValue> {
    let JValue::Arr(list) = list else {
        return Vec::new();
    };
    let by: HashMap<String, &JValue> = pages
        .iter()
        .map(|p| (num_text(n(p, "number")), p))
        .collect();
    let mut out = Vec::new();
    for f in list.iter().take(FIGURE_CAP) {
        let mime = f.get("mime").as_str().unwrap_or("");
        if !["image/png", "image/jpeg", "image/webp"].contains(&mime) {
            continue;
        }
        let page = js_number(f.get("page"));
        let index = js_number(f.get("index"));
        let page_ok = page.fract() == 0.0 && page.is_finite();
        let Some(on) = by.get(&num_text(page)).filter(|_| page_ok) else {
            continue;
        };
        if !(index.fract() == 0.0 && (0.0..=999.0).contains(&index)) {
            continue;
        }
        if scanned_page(on) {
            continue;
        }
        let Some(data) = f.get("data").as_str() else {
            continue;
        };
        if len16(data) > FIGURE_BYTES.div_ceil(3) * 4 + 4 {
            continue;
        }
        let raw = crate::js::base64_loose(data);
        if raw.is_empty() || raw.len() > FIGURE_BYTES {
            continue;
        }
        let Some(ext) = magic(mime, &raw) else {
            continue;
        };
        let mut fig = obj(vec![
            ("page", JValue::Num(page)),
            ("index", JValue::Num(index)),
            ("mime", JValue::Str(mime.into())),
            ("ext", JValue::Str(ext.into())),
        ]);
        if let Some(c) = f.get("caption").as_str() {
            let c = crate::js::trim(c);
            if !c.is_empty() {
                set(&mut fig, "caption", JValue::Str(wtf16::slice(c, 300)));
            }
        }
        if let Some(t) = f.get("text").as_str() {
            let t = crate::js::trim(t);
            if !t.is_empty() {
                set(&mut fig, "text", JValue::Str(wtf16::slice(t, 4000)));
            }
        }
        bytes_out.insert(index as i64, raw);
        out.push(fig);
    }
    out
}

/// The outcome of a first pass that is not a record: core's 422 (the original is stored and
/// labelled with this message).
#[derive(Debug, Clone, PartialEq)]
pub struct Permanent(pub String);

fn first_range_pages(bytes: usize, name: &str) -> bool {
    bytes > WHOLE_BYTES && is_pdf(name)
}

/// documents.cjs `extractWithDocling`.
fn extract_with_docling(
    name: &str,
    bytes: &[u8],
    engine: &dyn Engine,
    figures: bool,
    range_pages: usize,
    fig_bytes: &mut FigureBytes,
) -> Result<JValue, Permanent> {
    let ranged = first_range_pages(bytes.len(), name);
    let pages = ranged.then_some((1u32, range_pages as u32));
    let body = match engine.docling(name, bytes, figures, pages) {
        Ok(b) => b,
        Err(e) if e.permanent => return Err(Permanent(e.message)),
        Err(e) => {
            let mut out = assemble(Vec::new(), 0.0, true, None, false, None);
            set(&mut out, "state", JValue::Str("failed".into()));
            set(
                &mut out,
                "error",
                JValue::Str(wtf16::slice(&message_or(&e), 300)),
            );
            return Ok(out);
        }
    };
    let page_texts = docling_pages(&body, figures);
    let count = page_texts.len() as f64;
    let total = or_num(n(&body, "total"), count);
    let extra = !ranged && body.get("truncatedPages").truthy();
    let mut out = assemble(page_texts.clone(), total, false, None, extra, None);
    if figures {
        let found = parse_figures(body.get("figures"), &page_texts, fig_bytes);
        if !found.is_empty() {
            set(&mut out, "figures", JValue::Arr(found));
        }
    }
    Ok(out)
}

fn message_or(e: &EngineError) -> String {
    if e.message.is_empty() {
        "Error".into()
    } else {
        e.message.clone()
    }
}

/// The retryable failure of a document no engine could read now (core's shape for an unreachable
/// worker: nothing extracted, `retryable: true`).
pub fn unavailable(message: &str) -> JValue {
    let mut out = assemble(Vec::new(), 0.0, true, None, false, None);
    set(&mut out, "state", JValue::Str("failed".into()));
    set(&mut out, "error", JValue::Str(wtf16::slice(message, 300)));
    out
}

/// document-sources.cjs ingest's record of a thrown extraction (`status` 422 / 400).
pub fn thrown(message: &str, retryable: bool) -> JValue {
    obj(vec![
        ("text", JValue::Str(String::new())),
        ("pages", JValue::Num(0.0)),
        ("pageTexts", JValue::Arr(Vec::new())),
        ("state", JValue::Str("failed".into())),
        ("truncated", JValue::Bool(false)),
        ("retryable", JValue::Bool(retryable)),
        ("error", JValue::Str(message.to_string())),
    ])
}

/// documents.cjs `extractDocumentText(name, bytes)` on the Docling path: the result record (with
/// any new figures' bytes in `fig_bytes`), or the 422 of a document Docling refused for good.
/// Taken under the large-document lock for a document over 25 MiB.
pub fn extract_document_text(
    name: &str,
    bytes: &[u8],
    engine: &dyn Engine,
    limits: &Limits,
    fig_bytes: &mut FigureBytes,
) -> Result<JValue, Permanent> {
    let _held = match large_lock(bytes.len(), engine.large_lock_file().as_deref()) {
        Ok(h) => h,
        Err(busy) => return Ok(unavailable(&busy)),
    };
    if !engine.docling_configured() {
        return Ok(unavailable(NOT_CONFIGURED));
    }
    let large = first_range_pages(bytes.len(), name);
    if large && engine.docling_ranges(bytes.len()) != Some(true) {
        // core would read it with pdf.js; here it waits for a worker that reads ranges.
        return Ok(unavailable(NO_RANGES));
    }
    let figures = figures_on(engine);
    let out = extract_with_docling(name, bytes, engine, figures, limits.range_pages, fig_bytes)?;
    Ok(complete_unheld(name, bytes, out, engine, limits, fig_bytes))
}

/// documents.cjs `completeDocument(name, bytes, out)` for a record this side wrote (no reopen of a
/// size-refused or shared-memory failure: those only come from core's own reader).
pub fn complete_document(
    name: &str,
    bytes: &[u8],
    out: JValue,
    engine: &dyn Engine,
    limits: &Limits,
    fig_bytes: &mut FigureBytes,
) -> JValue {
    let _held = match large_lock(bytes.len(), engine.large_lock_file().as_deref()) {
        Ok(h) => h,
        // A busy lock leaves the record as it is (no marker), so the job tries again.
        Err(_) => return out,
    };
    complete_unheld(name, bytes, out, engine, limits, fig_bytes)
}

fn complete_unheld(
    name: &str,
    bytes: &[u8],
    out: JValue,
    engine: &dyn Engine,
    limits: &Limits,
    fig_bytes: &mut FigureBytes,
) -> JValue {
    if out.get("completion").as_str() == Some(COMPLETION)
        || out.get("retryable").truthy()
        || pages_of(&out).is_empty()
    {
        return out;
    }
    if !is_pdf(name) {
        let mut out = out;
        set(&mut out, "completion", JValue::Str(COMPLETION.into()));
        return out;
    }
    complete_unlocked(name, bytes, out, engine, limits, fig_bytes)
}

fn is_busy(e: &EngineError) -> bool {
    e.status == Some(503)
}

/// documents.cjs `isPassing(err)`.
fn is_passing(e: &EngineError) -> bool {
    if is_busy(e) || e.status == Some(400) {
        return true;
    }
    let m = e.message.to_ascii_lowercase();
    [
        "busy",
        "unavailable",
        "not configured",
        "fetch failed",
        "econnrefused",
        "econnreset",
        "enotfound",
        "eai_again",
        "ehostunreach",
    ]
    .iter()
    .any(|w| m.contains(w))
}

/// documents.cjs `patiently(fn, waits)`.
fn patiently<T>(
    waits: &[Duration],
    mut f: impl FnMut() -> Result<T, EngineError>,
) -> Result<T, EngineError> {
    let mut attempt = 0usize;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                let Some(wait) = waits.get(attempt).filter(|_| is_busy(&e)) else {
                    return Err(e);
                };
                std::thread::sleep(*wait);
                attempt += 1;
            }
        }
    }
}

fn text_length(pages: &[JValue]) -> usize {
    pages.iter().map(|p| len16(&s(p, "text"))).sum()
}

fn complete_unlocked(
    name: &str,
    bytes: &[u8],
    out: JValue,
    engine: &dyn Engine,
    limits: &Limits,
    fig_bytes: &mut FigureBytes,
) -> JValue {
    let waits = engine.busy_waits();
    let ocr_enabled = engine.ocr_configured();
    let figures = figures_on(engine);
    let mut caps: Option<Option<OcrCaps>> = None;
    let mut caps_now = || -> Option<OcrCaps> {
        if caps.is_none() {
            caps = Some(engine.ocr_caps());
        }
        caps.clone().flatten()
    };
    let mut page_texts = pages_of(&out);
    page_texts.sort_by(|a, b| {
        n(a, "number")
            .partial_cmp(&n(b, "number"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let highest = page_texts.last().map(|p| n(p, "number")).unwrap_or(0.0);
    let first_method = page_texts
        .first()
        .map(|p| s(p, "method"))
        .unwrap_or_default();
    let fill_method = if first_method == "native" {
        "native"
    } else {
        "docling"
    };
    let mut i = 0usize;
    let mut k = 1.0;
    while k <= highest {
        if page_texts.get(i).map(|p| n(p, "number")) == Some(k) {
            i += 1;
            k += 1.0;
            continue;
        }
        page_texts.insert(
            i,
            obj(vec![
                ("number", JValue::Num(k)),
                ("text", JValue::Str(String::new())),
                ("status", JValue::Str("failed".into())),
                ("method", JValue::Str(fill_method.into())),
            ]),
        );
        i += 1;
        k += 1.0;
    }
    let total = or_num(n(&out, "pages"), page_texts.len() as f64);
    let mut stopped_at: Option<JValue> = None;
    let mut found_figures: Option<Vec<JValue>> = match out.get("figures") {
        JValue::Arr(a) => Some(a.clone()),
        _ => None,
    };
    let mut passing = false;
    let mut range_attempts: Option<f64> = None;

    // 1. The pages past what the first pass read, a range at a time (through Docling only).
    let last = total.min(limits.pages as f64);
    let mut next = page_texts.len() as f64 + 1.0;
    let mut used = text_length(&page_texts);
    let via_docling = engine.docling_configured()
        && next <= last
        && page_texts.iter().any(|p| {
            let m = s(p, "method");
            m != "native" && m != "native+ocr"
        })
        && engine.docling_ranges(bytes.len()) == Some(true);
    let stop_error = |page: f64, error: &str, attempts: &mut Option<f64>| -> (JValue, bool) {
        let tries = or_num(n(&out, "rangeAttempts"), 0.0) + 1.0;
        *attempts = Some(tries);
        (
            obj(vec![
                ("page", JValue::Num(page)),
                ("limit", JValue::Str("error".into())),
                ("error", JValue::Str(wtf16::slice(error, 200))),
            ]),
            tries < RETRY_ATTEMPTS,
        )
    };
    while next <= last {
        if used >= limits.text {
            stopped_at = Some(obj(vec![
                ("page", JValue::Num(next - 1.0)),
                ("limit", JValue::Str("text".into())),
            ]));
            break;
        }
        if !via_docling {
            // core reads these with pdf.js; here they wait for Docling to read ranges.
            let (stop, again) = stop_error(next - 1.0, NO_RANGES, &mut range_attempts);
            stopped_at = Some(stop);
            passing = again;
            break;
        }
        let end = last.min(next + limits.range_pages as f64 - 1.0);
        let got = patiently(&waits, || {
            engine.docling(name, bytes, figures, Some((next as u32, end as u32)))
        });
        let got = match got {
            Ok(body) => {
                let got = docling_pages(&body, figures);
                if figures {
                    let have = found_figures.clone().unwrap_or_default();
                    let offset = have.iter().map(|f| n(f, "index") + 1.0).fold(0.0, f64::max);
                    let room = FIGURE_CAP.saturating_sub(have.len());
                    let mut fresh = FigureBytes::new();
                    let extra: Vec<JValue> = parse_figures(body.get("figures"), &got, &mut fresh)
                        .into_iter()
                        .take(room)
                        .map(|mut f| {
                            let old = n(&f, "index");
                            let new = offset + old;
                            if let Some(b) = fresh.remove(&(old as i64)) {
                                fig_bytes.insert(new as i64, b);
                            }
                            set(&mut f, "index", JValue::Num(new));
                            f
                        })
                        .collect();
                    if !extra.is_empty() {
                        let mut all = have;
                        all.extend(extra);
                        found_figures = Some(all);
                    }
                }
                got
            }
            Err(e) => {
                let (stop, again) = stop_error(next - 1.0, &message_or(&e), &mut range_attempts);
                stopped_at = Some(stop);
                passing = is_passing(&e) && again;
                break;
            }
        };
        let mut by: HashMap<String, JValue> = HashMap::new();
        for p in got {
            let num = n(&p, "number");
            if num >= next && num <= end {
                by.insert(num_text(num), p);
            }
        }
        let mut k = next;
        while k <= end {
            let mut page = by.remove(&num_text(k)).unwrap_or_else(|| {
                obj(vec![
                    ("number", JValue::Num(k)),
                    ("text", JValue::Str(String::new())),
                    ("status", JValue::Str("failed".into())),
                    ("method", JValue::Str("docling".into())),
                ])
            });
            let room = limits.text.saturating_sub(used);
            let text = s(&page, "text");
            if !text.is_empty() && len16(&text) > room {
                set(&mut page, "text", JValue::Str(wtf16::slice(&text, room)));
                set(&mut page, "truncated", JValue::Bool(true));
                set(&mut page, "status", JValue::Str("truncated".into()));
            }
            used += len16(&s(&page, "text"));
            page_texts.push(page);
            k += 1.0;
        }
        next = end + 1.0;
    }
    if stopped_at.is_none() && (page_texts.len() as f64) < total {
        stopped_at = Some(obj(vec![
            ("page", JValue::Num(page_texts.len() as f64)),
            ("limit", JValue::Str("pages".into())),
        ]));
    }

    // 2. Each page still incomplete, alone, through the local OCR worker, finer than the first pass.
    let any_retry = page_texts
        .iter()
        .any(|p| RETRY_STATUSES.contains(&s(p, "status").as_str()));
    let capabilities = if ocr_enabled && any_retry {
        caps_now()
    } else {
        None
    };
    let room = limits.text.saturating_sub(text_length(&page_texts));
    let deferred = retry_incomplete_pages(
        bytes,
        &mut page_texts,
        ocr_enabled,
        engine,
        room,
        &waits,
        capabilities.as_ref(),
    );
    if page_texts.iter().any(retry_due) || deferred {
        passing = true;
    }

    let assembled = assemble(page_texts, total, false, None, false, stopped_at);
    let mut rest = out.clone();
    remove(&mut rest, "ocrPagesSent");
    remove(&mut rest, "stoppedAt");
    if let (JValue::Obj(items), JValue::Obj(add)) = (&mut rest, assembled) {
        for (k, v) in add {
            wtf16::set(items, &k, v);
        }
    }
    // documents.cjs: `error: ocrError || out.error` (no OCR error on this path), errorId kept.
    let error = out.get("error").clone();
    set(&mut rest, "error", error);
    let error_id = out.get("errorId").clone();
    set(
        &mut rest,
        "errorId",
        if error_id.truthy() {
            error_id
        } else {
            JValue::Undefined
        },
    );
    set(
        &mut rest,
        "completion",
        if passing {
            JValue::Undefined
        } else {
            JValue::Str(COMPLETION.into())
        },
    );
    if let Some(a) = range_attempts {
        set(&mut rest, "rangeAttempts", JValue::Num(a));
    }
    if let Some(f) = found_figures.filter(|f| !f.is_empty()) {
        set(&mut rest, "figures", JValue::Arr(f));
    }
    rest
}

fn retry_due(p: &JValue) -> bool {
    RETRY_STATUSES.contains(&s(p, "status").as_str())
        && p.get("retry").get("outcome").as_str() == Some("failed")
        && p.get("retry").get("again").as_bool() == Some(true)
}

const RETRY_TOO_LARGE: &str = "the PDF is larger than the OCR service accepts";

fn size_blocked(p: &JValue) -> bool {
    p.get("retry").get("outcome").as_str() == Some("failed")
        && js_string(p.get("retry").get("error")).starts_with(RETRY_TOO_LARGE)
}

fn max_bytes_of(caps: Option<&OcrCaps>) -> usize {
    caps.and_then(|c| c.max_bytes)
        .unwrap_or(0)
        .max(OCR_MAX_BYTES)
}

/// documents.cjs `retryTimeoutMs(pages)`.
pub fn retry_timeout_ms(pages: usize) -> u64 {
    (30_000 + pages as u64 * 240_000).clamp(330_000, 610_000)
}

/// documents.cjs `retryIncompletePages`; true when the sync's OCR budget pushed pages to the next sync.
fn retry_incomplete_pages(
    bytes: &[u8],
    pages: &mut [JValue],
    ocr_enabled: bool,
    engine: &dyn Engine,
    mut room: usize,
    waits: &[Duration],
    capabilities: Option<&OcrCaps>,
) -> bool {
    let finer = capabilities.is_some_and(|c| c.dpis.contains(&RETRY_DPI));
    let max_bytes = max_bytes_of(capabilities);
    let already_ocred = |p: &JValue| {
        !finer
            && (p.get("ocrSent").as_bool() == Some(true)
                || (ocr_enabled && s(p, "status") == "unreadable" && s(p, "method") == "native"))
    };
    let due = |p: &JValue| {
        p.get("retry").is_nullish()
            || !p.get("retry").truthy()
            || retry_due(p)
            || (size_blocked(p) && bytes.len() <= max_bytes)
    };
    let candidates: Vec<usize> = pages
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            RETRY_STATUSES.contains(&s(p, "status").as_str()) && due(p) && !already_ocred(p)
        })
        .map(|(i, _)| i)
        .collect();
    if candidates.is_empty() {
        return false;
    }
    let mark = |pages: &mut [JValue], which: &[usize], retry: &JValue| {
        for (i, p) in pages.iter_mut().enumerate() {
            if which.contains(&i) {
                set(p, "retry", retry.clone());
            }
        }
    };
    if !ocr_enabled {
        mark(
            pages,
            &candidates,
            &obj(vec![("outcome", JValue::Str("unavailable".into()))]),
        );
        return false;
    }
    if bytes.len() > max_bytes {
        let error = format!("{RETRY_TOO_LARGE} ({} MB)", max_bytes / (1024 * 1024));
        mark(
            pages,
            &candidates,
            &obj(vec![
                ("outcome", JValue::Str("failed".into())),
                ("error", JValue::Str(error)),
            ]),
        );
        return false;
    }
    let tried = pages
        .iter()
        .enumerate()
        .filter(|(i, p)| {
            let outcome = p.get("retry").get("outcome").as_str().unwrap_or("");
            p.get("retry").truthy()
                && outcome != "skipped"
                && outcome != "unavailable"
                && !size_blocked(p)
                && !candidates.contains(i)
        })
        .count();
    let cap = RETRY_PAGE_CAP.saturating_sub(tried);
    let capped: Vec<usize> = candidates.iter().copied().take(cap).collect();
    let skipped: Vec<usize> = candidates.iter().copied().skip(capped.len()).collect();
    mark(
        pages,
        &skipped,
        &obj(vec![("outcome", JValue::Str("skipped".into()))]),
    );
    // #1871: and within what this sync may still spend on OCR (all its documents together). Pages
    // past it are left as they are, and the document stays open (no completion marker), so a later
    // sync goes on.
    let allowed = engine
        .ocr_budget()
        .map_or(capped.len(), |b| b.take(capped.len()));
    let chosen: Vec<usize> = capped.iter().copied().take(allowed).collect();
    let deferred = chosen.len() < capped.len();
    if deferred {
        note_deferral(engine.ocr_budget(), capped.len() - chosen.len());
    }
    for batch in chosen.chunks(RETRY_BATCH) {
        let numbers: Vec<u32> = pages
            .iter()
            .enumerate()
            .filter(|(i, _)| batch.contains(i))
            .map(|(_, p)| n(p, "number") as u32)
            .collect();
        let timeout = retry_timeout_ms(batch.len());
        let got = patiently(waits, || {
            engine.ocr_pages(bytes, &numbers, Some(RETRY_DPI), Some(timeout))
        });
        let results = match got {
            Ok(r) => r,
            Err(e) => {
                let lower = e.message.to_ascii_lowercase();
                let timed_out =
                    e.timed_out || lower.contains("timeout") || lower.contains("aborted");
                let reason = if timed_out {
                    "OCR took too long".to_string()
                } else {
                    wtf16::slice(&message_or(&e), 200)
                };
                let passing_failure = is_passing(&e) || timed_out;
                for (i, p) in pages.iter_mut().enumerate() {
                    if !batch.contains(&i) {
                        continue;
                    }
                    let attempts = or_num(n(p.get("retry"), "attempts"), 0.0) + 1.0;
                    let mut retry = obj(vec![
                        ("outcome", JValue::Str("failed".into())),
                        ("dpi", JValue::Num(f64::from(RETRY_DPI))),
                        ("error", JValue::Str(reason.clone())),
                        ("attempts", JValue::Num(attempts)),
                    ]);
                    if passing_failure && attempts < RETRY_ATTEMPTS {
                        set(&mut retry, "again", JValue::Bool(true));
                    }
                    set(p, "retry", retry);
                }
                continue;
            }
        };
        for (i, p) in pages.iter_mut().enumerate() {
            if batch.contains(&i) {
                apply_retry(p, &results, &mut room);
            }
        }
    }
    deferred
}

/// #1878: pages the OCR budget pushed to the next sync, one content-free line per document.
fn note_deferral(budget: Option<&OcrBudget>, pages: usize) {
    let mut sync = String::new();
    if let Some(b) = budget {
        b.deferred_pages.set(b.deferred_pages.get() + pages);
        b.deferred_documents.set(b.deferred_documents.get() + 1);
        sync = format!(
            " syncPagesDeferred={} syncDocumentsDeferred={}",
            b.deferred_pages.get(),
            b.deferred_documents.get()
        );
    }
    eprintln!(
        "[documents] OCR budget deferred pages to the next sync: pagesDeferred={pages} documentsDeferred=1{sync}"
    );
}

/// One page's OCR retry answer, merged in place (retryIncompletePages's inner loop).
fn apply_retry(p: &mut JValue, results: &[JValue], room: &mut usize) {
    let dpi = || JValue::Num(f64::from(RETRY_DPI));
    let number = n(p, "number");
    let result = results
        .iter()
        .find(|r| r.is_object() && n(r, "number") == number);
    let Some(result) = result.filter(|r| !r.get("error").truthy()) else {
        let error = match result {
            Some(r) => js_string(r.get("error")),
            None => "OCR returned nothing for this page".into(),
        };
        set(
            p,
            "retry",
            obj(vec![
                ("outcome", JValue::Str("failed".into())),
                ("dpi", dpi()),
                ("error", JValue::Str(wtf16::slice(&error, 200))),
            ]),
        );
        return;
    };
    let text = result
        .get("text")
        .as_str()
        .map(|t| crate::js::trim(t).to_string())
        .unwrap_or_default();
    if text.is_empty() {
        set(
            p,
            "retry",
            obj(vec![
                ("outcome", JValue::Str("empty".into())),
                ("dpi", dpi()),
            ]),
        );
        return;
    }
    let old = s(p, "text");
    let addition = format!("{}{OCR_LINE}{text}", if old.is_empty() { "" } else { "\n" });
    let available = PAGE_TEXT_CAP.saturating_sub(len16(&old)).min(*room);
    if available == 0 {
        set(
            p,
            "retry",
            obj(vec![
                ("outcome", JValue::Str("failed".into())),
                ("dpi", dpi()),
                ("error", JValue::Str("text limit reached".into())),
            ]),
        );
        return;
    }
    let from = p.get("status").clone();
    let add_len = len16(&addition);
    set(
        p,
        "text",
        JValue::Str(format!("{old}{}", wtf16::slice(&addition, available))),
    );
    *room = room.saturating_sub(add_len.min(available));
    let truncated = result.get("truncated").truthy() || add_len > available;
    set(p, "truncated", JValue::Bool(truncated));
    set(
        p,
        "status",
        JValue::Str(if truncated { "truncated" } else { "ocr" }.into()),
    );
    let method = s(p, "method");
    let method = if method.is_empty() {
        "native".to_string()
    } else {
        method
    };
    set(p, "method", JValue::Str(format!("{method}+ocr")));
    remove(p, "reason");
    set(
        p,
        "retry",
        obj(vec![
            ("outcome", JValue::Str("recovered".into())),
            ("dpi", dpi()),
            ("from", from),
        ]),
    );
}

// ── the large-document lock ──────────────────────────────────────────────

/// large-document-lock.cjs LARGE_BYTES.
pub const LARGE_BYTES: usize = 25 * 1024 * 1024;
const BUSY: &str = "Large-document processing is busy; refresh to retry.";

static LARGE: Mutex<bool> = Mutex::new(false);
static FREED: Condvar = Condvar::new();

/// The host-wide lock file in UI_DATA_DIR, shared with core's large-document-lock.cjs. It is an
/// SQLite file: the holder keeps a `BEGIN IMMEDIATE` transaction open (SQLite's RESERVED lock, one
/// writer per file across every process and connection), and a holder that dies frees it with its
/// file locks. A waiter polls [`FILE_POLL`] apart with no busy timeout, so it never blocks past its
/// deadline. Both sides keep this protocol: the file name, `BEGIN IMMEDIATE`, nothing ever written.
pub const LARGE_LOCK_FILE: &str = "large-document.lock";
const FILE_POLL: Duration = Duration::from_millis(200);

thread_local! {
    /// The thread that holds the lock. A source sync takes it around the storage read and the
    /// extraction of one document, and the extraction takes it again: the inner take passes straight
    /// through (large-document-lock.cjs is re-entrant within one async call chain; this is one thread).
    static HELD_HERE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The hold of one large document; released on drop (the file's transaction first).
pub struct LargeHold(bool, Option<rusqlite::Connection>);

impl Drop for LargeHold {
    fn drop(&mut self) {
        if let Some(conn) = self.1.take() {
            let _ = conn.execute_batch("ROLLBACK");
            drop(conn);
        }
        if self.0 {
            HELD_HERE.with(|h| h.set(false));
            if let Ok(mut held) = LARGE.lock() {
                *held = false;
            }
            FREED.notify_one();
        }
    }
}

fn acquire_wait() -> Duration {
    let ms = positive(
        std::env::var("LARGE_DOCUMENT_ACQUIRE_WAIT_MS").ok(),
        120_000,
    );
    Duration::from_millis(ms as u64)
}

/// The file lock, polled until `deadline`; `None` past it. An error opening the file (not a busy
/// one) is also `None`: the lock is never skipped.
fn take_file(file: &std::path::Path, deadline: Instant) -> Option<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(file).ok()?;
    conn.busy_timeout(Duration::ZERO).ok()?;
    loop {
        match conn.execute_batch("BEGIN IMMEDIATE") {
            Ok(()) => return Some(conn),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) => {}
            Err(_) => return None,
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        std::thread::sleep(left.min(FILE_POLL));
    }
}

/// large-document-lock.cjs `ifLarge(size, fn)`: a document over 25 MiB waits for the one hold (this
/// process's, then the host's in `file`), at most the acquire wait (two minutes), then is "busy" (a
/// passing failure; the job tries again).
pub fn large_lock(size: usize, file: Option<&std::path::Path>) -> Result<LargeHold, String> {
    if size <= LARGE_BYTES || HELD_HERE.with(std::cell::Cell::get) {
        return Ok(LargeHold(false, None));
    }
    let deadline = Instant::now() + acquire_wait();
    {
        let mut held = LARGE.lock().map_err(|_| BUSY.to_string())?;
        while *held {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(BUSY.into());
            }
            let (next, _) = FREED
                .wait_timeout(held, left)
                .map_err(|_| BUSY.to_string())?;
            held = next;
        }
        *held = true;
    }
    let mut hold = LargeHold(true, None);
    HELD_HERE.with(|h| h.set(true));
    if let Some(file) = file {
        // Dropping `hold` on failure frees this process's turn for the next waiter.
        hold.1 = Some(take_file(file, deadline).ok_or_else(|| BUSY.to_string())?);
    }
    Ok(hold)
}

/// SHA-256 hex.
pub fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::expect_used
)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    pub(crate) struct Fake {
        pub docling: bool,
        pub ocr: bool,
        pub ranges: Option<bool>,
        pub replies: RefCell<Vec<Result<JValue, EngineError>>>,
        pub ocr_replies: RefCell<Vec<Result<Vec<JValue>, EngineError>>>,
        pub asked: RefCell<Vec<Option<(u32, u32)>>>,
        pub caps: Option<OcrCaps>,
    }
    impl Engine for Fake {
        fn docling_configured(&self) -> bool {
            self.docling
        }
        fn docling(
            &self,
            _: &str,
            _: &[u8],
            _: bool,
            pages: Option<(u32, u32)>,
        ) -> Result<JValue, EngineError> {
            self.asked.borrow_mut().push(pages);
            let mut r = self.replies.borrow_mut();
            if r.is_empty() {
                Err(EngineError::new("fetch failed"))
            } else {
                r.remove(0)
            }
        }
        fn docling_ranges(&self, _: usize) -> Option<bool> {
            self.ranges
        }
        fn ocr_configured(&self) -> bool {
            self.ocr
        }
        fn ocr_pages(
            &self,
            _: &[u8],
            _: &[u32],
            _: Option<u32>,
            _: Option<u64>,
        ) -> Result<Vec<JValue>, EngineError> {
            let mut r = self.ocr_replies.borrow_mut();
            if r.is_empty() {
                Err(EngineError::new("fetch failed"))
            } else {
                r.remove(0)
            }
        }
        fn ocr_caps(&self) -> Option<OcrCaps> {
            self.caps.clone()
        }
        fn multimodal(&self) -> bool {
            false
        }
        fn busy_waits(&self) -> Vec<Duration> {
            vec![Duration::ZERO, Duration::ZERO]
        }
    }

    /// A [`Fake`] that also answers the OCR worker's `/reduce-pdf` and `/extract-docx` once each.
    pub(crate) struct Wrap<'a> {
        pub inner: &'a Fake,
        pub reduce: RefCell<Option<Result<JValue, EngineError>>>,
        pub docx: RefCell<Option<Result<JValue, EngineError>>>,
    }
    impl<'a> Wrap<'a> {
        pub fn new(inner: &'a Fake) -> Self {
            Wrap {
                inner,
                reduce: RefCell::new(None),
                docx: RefCell::new(None),
            }
        }
    }
    impl Engine for Wrap<'_> {
        fn docling_configured(&self) -> bool {
            self.inner.docling_configured()
        }
        fn docling(
            &self,
            name: &str,
            bytes: &[u8],
            figures: bool,
            pages: Option<(u32, u32)>,
        ) -> Result<JValue, EngineError> {
            self.inner.docling(name, bytes, figures, pages)
        }
        fn docling_ranges(&self, bytes: usize) -> Option<bool> {
            self.inner.docling_ranges(bytes)
        }
        fn ocr_configured(&self) -> bool {
            self.inner.ocr_configured() || self.reduce.borrow().is_some()
        }
        fn ocr_pages(
            &self,
            bytes: &[u8],
            pages: &[u32],
            dpi: Option<u32>,
            timeout_ms: Option<u64>,
        ) -> Result<Vec<JValue>, EngineError> {
            self.inner.ocr_pages(bytes, pages, dpi, timeout_ms)
        }
        fn ocr_caps(&self) -> Option<OcrCaps> {
            self.inner.ocr_caps()
        }
        fn multimodal(&self) -> bool {
            false
        }
        fn busy_waits(&self) -> Vec<Duration> {
            vec![Duration::ZERO, Duration::ZERO]
        }
        fn reduce_pdf(&self, _: &[u8]) -> Option<Result<JValue, EngineError>> {
            self.reduce.borrow_mut().take()
        }
        fn extract_docx(&self, _: &[u8]) -> Result<JValue, EngineError> {
            self.docx
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Err(EngineError::new("DOCX reader is unavailable.")))
        }
    }

    fn parse(t: &str) -> JValue {
        wtf16::parse(t).unwrap()
    }
    fn json(v: &JValue) -> String {
        wtf16::stringify(v).unwrap()
    }
    const LIMITS: Limits = Limits {
        read_bytes: 1 << 30,
        pages: 1000,
        text: 8_000_000,
        range_pages: 25,
    };

    /// A sync's OCR allowance (#1871): pages past it are left as they are, the document stays open (no
    /// completion marker, so a later sync goes on), and the deferral is counted for the log (#1878).
    #[test]
    fn the_syncs_ocr_budget_defers_pages_and_leaves_the_document_open() {
        let fake = Fake {
            docling: true,
            ocr: true,
            ..Default::default()
        };
        // The first pass: three pages Docling could not read, nothing else.
        fake.replies.borrow_mut().push(Ok(parse(
            r#"{"total":3,"pages":[{"number":1,"text":"","status":"ocr-needed","method":"docling"},{"number":2,"text":"","status":"ocr-needed","method":"docling"},{"number":3,"text":"","status":"ocr-needed","method":"docling"}]}"#,
        )));
        // The OCR worker reads the two pages it is given.
        fake.ocr_replies.borrow_mut().push(Ok(vec![
            parse(r#"{"number":1,"text":"page one"}"#),
            parse(r#"{"number":2,"text":"page two"}"#),
        ]));
        let engine = Budgeted {
            inner: &fake,
            budget: OcrBudget::new(2),
        };
        let out = extract_document_text(
            "a.pdf",
            b"%PDF-1",
            &engine,
            &LIMITS,
            &mut FigureBytes::new(),
        )
        .unwrap();
        assert_eq!(engine.budget.remaining.get(), 0);
        assert_eq!(engine.budget.deferred_pages.get(), 1);
        assert_eq!(engine.budget.deferred_documents.get(), 1);
        let pages = pages_of(&out);
        let outcome = |i: usize| s(&pages[i].get("retry").clone(), "outcome");
        assert_eq!(
            (outcome(0), outcome(1)),
            ("recovered".into(), "recovered".into())
        );
        assert!(
            pages[2].get("retry").is_nullish(),
            "the third page waits for the next sync"
        );
        assert!(
            out.get("completion").as_str() != Some(COMPLETION),
            "a deferred document stays open"
        );
        // Without a budget (an upload, a queued job) nothing is deferred and the pass completes.
        let plain = Fake {
            docling: true,
            ocr: true,
            ..Default::default()
        };
        plain.replies.borrow_mut().push(Ok(parse(
            r#"{"total":1,"pages":[{"number":1,"text":"","status":"ocr-needed","method":"docling"}]}"#,
        )));
        plain
            .ocr_replies
            .borrow_mut()
            .push(Ok(vec![parse(r#"{"number":1,"text":"page one"}"#)]));
        let out =
            extract_document_text("a.pdf", b"%PDF-1", &plain, &LIMITS, &mut FigureBytes::new())
                .unwrap();
        assert_eq!(out.get("completion").as_str(), Some(COMPLETION));
    }

    #[test]
    fn routing_matches_core() {
        assert!(is_document("a.PDF", false));
        assert!(!is_document("a.docx", false));
        assert!(is_document("a.docx", true));
        assert!(!is_document("SKILL.md", true));
        assert!(!is_document("data.csv", true));
        assert!(is_document("scan.png", true));
        assert!(!is_document("x.zip", true));
        assert!(!is_document("noext", true));
    }

    #[test]
    fn a_clean_reply_is_a_completed_ready_record() {
        let fake = Fake {
            docling: true,
            ..Default::default()
        };
        fake.replies.borrow_mut().push(Ok(parse(
            r#"{"total":2,"pages":[{"number":1,"text":"one","status":"native","method":"docling"},{"number":2,"text":"two","status":"native","method":"pdfium","reason":"x","truncated":0}]}"#,
        )));
        let out =
            extract_document_text("a.pdf", b"%PDF-1", &fake, &LIMITS, &mut FigureBytes::new())
                .unwrap();
        assert_eq!(
            json(&out),
            r#"{"text":"[Page 1]\none\n\n[Page 2]\ntwo","pages":2,"pageTexts":[{"number":1,"text":"one","status":"native","method":"docling","truncated":false},{"number":2,"text":"two","status":"native","method":"pdfium","truncated":false,"reason":"x"}],"truncated":false,"state":"ready","retryable":false,"completion":"ranges-ocr-retry-v1"}"#
        );
    }

    #[test]
    fn docling_down_is_the_retryable_failure_core_stores() {
        let fake = Fake {
            docling: true,
            ..Default::default()
        };
        let out = extract_document_text("a.pdf", b"%PDF", &fake, &LIMITS, &mut FigureBytes::new())
            .unwrap();
        assert_eq!(
            json(&out),
            r#"{"text":"","pages":0,"pageTexts":[],"truncated":false,"state":"failed","retryable":true,"error":"fetch failed"}"#
        );
        let none = Fake::default();
        let out = extract_document_text("a.pdf", b"%PDF", &none, &LIMITS, &mut FigureBytes::new())
            .unwrap();
        assert_eq!(out.get("error").as_str(), Some(NOT_CONFIGURED));
        assert!(out.get("retryable").truthy());
    }

    #[test]
    fn a_refusal_is_permanent() {
        let fake = Fake {
            docling: true,
            ..Default::default()
        };
        fake.replies.borrow_mut().push(Err(EngineError {
            message: "This document could not be read; the original is stored.".into(),
            permanent: true,
            ..Default::default()
        }));
        let out = extract_document_text("a.pdf", b"%PDF", &fake, &LIMITS, &mut FigureBytes::new());
        assert!(matches!(out, Err(Permanent(_))));
    }

    #[test]
    fn pages_past_the_window_come_in_ranges_and_incomplete_pages_go_to_ocr() {
        let fake = Fake {
            docling: true,
            ocr: true,
            ranges: Some(true),
            caps: Some(OcrCaps {
                max_bytes: None,
                dpis: vec![400],
            }),
            ..Default::default()
        };
        fake.replies.borrow_mut().push(Ok(parse(
            r#"{"total":3,"pages":[{"number":1,"text":"a","status":"native"},{"number":2,"text":"","status":"failed"}]}"#,
        )));
        fake.replies.borrow_mut().push(Ok(parse(
            r#"{"total":3,"pages":[{"number":3,"text":"c","status":"native"}]}"#,
        )));
        fake.ocr_replies
            .borrow_mut()
            .push(Ok(vec![parse(r#"{"number":2,"text":" two "}"#)]));
        let out = extract_document_text("a.pdf", b"%PDF", &fake, &LIMITS, &mut FigureBytes::new())
            .unwrap();
        assert_eq!(*fake.asked.borrow(), vec![None, Some((3, 3))]);
        let p2 = &pages_of(&out)[1];
        assert_eq!(s(p2, "status"), "ocr");
        assert_eq!(s(p2, "method"), "docling+ocr");
        assert_eq!(s(p2, "text"), format!("{OCR_LINE}two"));
        assert_eq!(
            json(p2.get("retry")),
            r#"{"outcome":"recovered","dpi":400,"from":"failed"}"#
        );
        assert_eq!(s(&out, "state"), "ready");
        assert_eq!(s(&out, "completion"), COMPLETION);
    }

    #[test]
    fn a_busy_ocr_retry_leaves_the_record_open() {
        let fake = Fake {
            docling: true,
            ocr: true,
            ..Default::default()
        };
        fake.replies.borrow_mut().push(Ok(parse(
            r#"{"pages":[{"number":1,"text":"","status":"failed"}]}"#,
        )));
        let out = extract_document_text("a.pdf", b"%PDF", &fake, &LIMITS, &mut FigureBytes::new())
            .unwrap();
        let p = &pages_of(&out)[0];
        assert_eq!(
            json(p.get("retry")),
            r#"{"outcome":"failed","dpi":400,"error":"fetch failed","attempts":1,"again":true}"#
        );
        assert!(out.get("completion").is_nullish());
        assert_eq!(s(&out, "state"), "failed");
    }

    #[test]
    fn without_ocr_the_retry_is_unavailable() {
        let fake = Fake {
            docling: true,
            ..Default::default()
        };
        fake.replies.borrow_mut().push(Ok(parse(
            r#"{"pages":[{"number":1,"text":"x","status":"degraded"}]}"#,
        )));
        let out = extract_document_text("a.pdf", b"%PDF", &fake, &LIMITS, &mut FigureBytes::new())
            .unwrap();
        assert_eq!(
            json(pages_of(&out)[0].get("retry")),
            r#"{"outcome":"unavailable"}"#
        );
        assert_eq!(s(&out, "state"), "partial");
    }

    #[test]
    fn a_non_pdf_is_marked_complete_without_a_pass() {
        let fake = Fake {
            docling: true,
            ..Default::default()
        };
        fake.replies.borrow_mut().push(Ok(parse(
            r#"{"pages":[{"number":1,"text":"slide","status":"native"}]}"#,
        )));
        let out = extract_document_text("a.pptx", b"PK", &fake, &LIMITS, &mut FigureBytes::new())
            .unwrap();
        assert_eq!(s(&out, "completion"), COMPLETION);
        assert_eq!(fake.asked.borrow().len(), 1);
    }

    /// Serialises the tests that take the process-wide lock.
    pub(crate) static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn the_large_lock_waits_then_is_busy() {
        let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("LARGE_DOCUMENT_ACQUIRE_WAIT_MS", "50");
        let first = large_lock(LARGE_BYTES + 1, None).unwrap();
        assert!(large_lock(10, None).is_ok());
        // Another thread waits, then is busy.
        let other = std::thread::spawn(|| large_lock(LARGE_BYTES + 1, None).err())
            .join()
            .unwrap();
        assert_eq!(other.as_deref(), Some(BUSY));
        // The holder's own thread passes straight through (a sync's read, then its extraction), and
        // the inner take never lets go of the outer one.
        let inner = large_lock(LARGE_BYTES + 1, None).unwrap();
        drop(inner);
        let still = std::thread::spawn(|| large_lock(LARGE_BYTES + 1, None).err())
            .join()
            .unwrap();
        assert_eq!(still.as_deref(), Some(BUSY), "the outer hold is still held");
        drop(first);
        assert!(large_lock(LARGE_BYTES + 1, None).is_ok());
    }

    /// The child side of the two-process test: when NOEVIA_TEST_LARGE_LOCK_HOLD names a lock file,
    /// takes the lock in it, writes `<file>.held`, and keeps it until `<file>.release` appears
    /// (at most 30 s). Otherwise it does nothing.
    #[test]
    fn large_lock_child_holds() {
        let Ok(file) = std::env::var("NOEVIA_TEST_LARGE_LOCK_HOLD") else {
            return;
        };
        let file = std::path::PathBuf::from(file);
        let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let hold = large_lock(LARGE_BYTES + 1, Some(&file)).unwrap();
        std::fs::write(file.with_extension("held"), b"1").unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        while !file.with_extension("release").exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(hold);
    }

    /// Review #2191 r4+5 (#697): the lock was in-process only, so another process (Node's
    /// large-document-lock.cjs, or a second front) could extract a large document at the same time.
    #[test]
    fn the_large_lock_is_held_across_processes() {
        let _s = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join(LARGE_LOCK_FILE);
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "extract::tests::large_lock_child_holds",
                "--nocapture",
            ])
            .env("NOEVIA_TEST_LARGE_LOCK_HOLD", &file)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(20);
        while !file.with_extension("held").exists() {
            assert!(Instant::now() < until, "the child never took the lock");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::env::set_var("LARGE_DOCUMENT_ACQUIRE_WAIT_MS", "300");
        let started = Instant::now();
        assert_eq!(
            large_lock(LARGE_BYTES + 1, Some(&file)).err().as_deref(),
            Some(BUSY),
            "another process holds the lock"
        );
        assert!(started.elapsed() >= Duration::from_millis(250));
        // A failed file take frees this process's turn.
        assert!(large_lock(LARGE_BYTES + 1, None).is_ok());
        std::fs::write(file.with_extension("release"), b"1").unwrap();
        assert!(child.wait().unwrap().success());
        std::env::set_var("LARGE_DOCUMENT_ACQUIRE_WAIT_MS", "5000");
        let hold = large_lock(LARGE_BYTES + 1, Some(&file));
        assert!(hold.is_ok(), "free once the other process lets go");
    }
}
