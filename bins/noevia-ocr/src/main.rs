#![forbid(unsafe_code)]
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::{process::Command, sync::Semaphore, time::Instant};
const INPUT: usize = 25 * 1024 * 1024;
const REDUCE_INPUT: usize = 60 * 1024 * 1024;
// sbstndalton/noevia#1867: /extract renders only the asked pages (pdftoppm -f/-l), so it accepts a PDF
// up to OCR_EXTRACT_MAX_BYTES (default 96 MiB, core's PROJECT_DOCUMENT_READ_BYTES default), streamed to
// the temp file in CHUNK pieces and never held whole. Same contract as the Python worker.
const EXTRACT_DEFAULT: usize = 96 * 1024 * 1024;
const CHUNK: usize = 1024 * 1024;
// 30 s per read and, in all, 30 s plus one second per BODY_RATE bytes.
const BODY_SECONDS: u64 = 30;
const BODY_RATE: usize = 256 * 1024;
const MAX_PAGE: u64 = 100_000;
// #1865/#1867: X-OCR-DPI, exactly 300 (default), 400 or 600: (dpi, longest side px, seconds per step).
const DPIS: [(u32, u32, u64); 3] = [(300, 3500, 60), (400, 4700, 120), (600, 7000, 180)];
fn extract_limit() -> usize {
    std::env::var("OCR_EXTRACT_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(EXTRACT_DEFAULT)
}
fn health() -> Value {
    json!({"service":"ocr","ranges":true,"maxBytes":extract_limit(),"dpis":[300,400,600]})
}
/// The render DPI from an X-OCR-DPI value: 300 when absent or empty, None unless exactly 300/400/600.
fn dpi_of(raw: Option<&str>) -> Option<u32> {
    match raw.unwrap_or("").trim_matches([' ', '\t']) {
        "" | "300" => Some(300),
        "400" => Some(400),
        "600" => Some(600),
        _ => None,
    }
}
fn render_args(page: u64, dpi: u32, pdf: &Path, image: &Path) -> (Vec<String>, u64) {
    let (dpi, side, step) = DPIS
        .iter()
        .copied()
        .find(|(d, _, _)| *d == dpi)
        .unwrap_or(DPIS[0]);
    (
        strings(&[
            "-f",
            &page.to_string(),
            "-l",
            &page.to_string(),
            "-singlefile",
            "-scale-to",
            &side.to_string(),
            "-r",
            &dpi.to_string(),
            "-gray",
            "-png",
            &pdf.to_string_lossy(),
            &image.to_string_lossy(),
        ]),
        step,
    )
}
const INVALID: &str = "Invalid document size or page selection.";
const OCR_ERROR: &str = "OCR failed or reached its processing limit; refresh to retry.";
const REDUCE_ERROR: &str = "PDF could not be reduced below 25 MB or extracted within limits. Split or compress it locally and retry.";
const DOCX_ERROR: &str =
    "DOCX is malformed, encrypted or exceeds its processing limits; the original is retained.";
const PDF_NOTE: &str = "Compressed PDF: images may have reduced quality and interactive features may change. The original remains on your computer.";
const TEXT_NOTE: &str = "Text-only PDF extraction: images, scanned-page text, layout and interactive features are omitted. The original remains on your computer.";
fn reply(status: u16, body: Value) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(body),
    )
        .into_response()
}
fn error(status: u16, message: &str) -> Response {
    reply(status, json!({"error": message}))
}
fn selection(
    path: &str,
    length: Option<&str>,
    header: Option<&str>,
    dpi: Option<&str>,
) -> Option<(usize, Vec<u64>, u32)> {
    let size = length.unwrap_or("0").trim().parse::<usize>().ok()?;
    let limit = match path {
        "/reduce-pdf" => REDUCE_INPUT,
        "/extract" => extract_limit(),
        _ => INPUT,
    };
    if size == 0 || size > limit {
        return None;
    }
    let pages: Value = if path == "/extract" {
        serde_json::from_str(header.unwrap_or("[]")).ok()?
    } else {
        json!([1])
    };
    let list = pages.as_array()?;
    if list.is_empty() || list.len() > 50 {
        return None;
    }
    let mut result = Vec::new();
    for value in list {
        let n = value.as_u64()?;
        if !(1..=MAX_PAGE).contains(&n) || result.contains(&n) {
            return None;
        }
        result.push(n);
    }
    // The DPI is read (and refused when invalid) only where pages are rendered.
    let dpi = if path == "/extract" {
        dpi_of(dpi)?
    } else {
        300
    };
    Some((size, result, dpi))
}
// Child streams go to bounded private files, never an unbounded capture_output buffer.
// Kill and reap on every timeout, output-limit failure and cancelled request.
struct ProcessGroup(nix::unistd::Pid);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(self.0, nix::sys::signal::Signal::SIGKILL);
    }
}
async fn run(
    program: &str,
    args: &[String],
    root: &Path,
    limit: u64,
    deadline: Instant,
    seconds: u64,
    watched: &[(&Path, u64)],
) -> Result<Vec<u8>, ()> {
    let stdout = root.join("stdout");
    let stderr = root.join("stderr");
    let out = std::fs::File::create(&stdout).map_err(|_| ())?;
    let err = std::fs::File::create(&stderr).map_err(|_| ())?;
    if Instant::now() >= deadline {
        return Err(());
    }
    let mut child = Command::new(program)
        .process_group(0)
        .args(args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ())?;
    let pid = child.id().and_then(|id| i32::try_from(id).ok()).ok_or(())?;
    let _group = ProcessGroup(nix::unistd::Pid::from_raw(pid));
    let end = deadline.min(Instant::now() + Duration::from_secs(seconds));
    loop {
        let exceeded = std::fs::metadata(&stdout).is_ok_and(|m| m.len() > limit)
            || std::fs::metadata(&stderr).is_ok_and(|m| m.len() > 1024 * 1024)
            || watched
                .iter()
                .any(|(p, n)| std::fs::metadata(p).is_ok_and(|m| m.len() > *n));
        if exceeded || Instant::now() >= end {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(());
        }
        if let Some(status) = child.try_wait().map_err(|_| ())? {
            if !status.success() {
                return Err(());
            }
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(_group);
    if std::fs::metadata(&stderr).is_ok_and(|m| m.len() > 1024 * 1024)
        || watched
            .iter()
            .any(|(p, n)| std::fs::metadata(p).is_ok_and(|m| m.len() > *n))
    {
        return Err(());
    }
    let data = read_bounded(&stdout, limit).await?;
    if data.len() as u64 > limit {
        return Err(());
    }
    Ok(data)
}
async fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, ()> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await.map_err(|_| ())?;
    let mut data = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut data)
        .await
        .map_err(|_| ())?;
    if data.len() as u64 > limit {
        return Err(());
    }
    Ok(data)
}
fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).to_owned()).collect()
}
fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data)
        .trim_matches(docx_text::is_python_space)
        .to_owned()
}
/// Copy exactly `size` body bytes to `file`, one frame (at most CHUNK) at a time; Err when the body
/// ends early, errs, stalls BODY_SECONDS, or outlasts its overall deadline.
async fn receive(body: axum::body::Body, file: &Path, size: usize) -> Result<(), ()> {
    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;
    let mut out = tokio::fs::File::create(file).await.map_err(|_| ())?;
    let mut body = http_body_util::Limited::new(body, size);
    let deadline = Instant::now() + Duration::from_secs(BODY_SECONDS + (size / BODY_RATE) as u64);
    let mut received = 0usize;
    while received < size {
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(BODY_SECONDS));
        let frame = match tokio::time::timeout(wait, body.frame()).await {
            Ok(Some(Ok(frame))) => frame,
            _ => return Err(()),
        };
        if let Ok(data) = frame.into_data() {
            for piece in data.chunks(CHUNK) {
                out.write_all(piece).await.map_err(|_| ())?;
            }
            received = received.checked_add(data.len()).ok_or(())?;
        }
    }
    out.flush().await.map_err(|_| ())?;
    if received == size {
        Ok(())
    } else {
        Err(())
    }
}
async fn extract(root: &Path, pdf: &Path, pages: &[u64], dpi: u32) -> Result<Value, ()> {
    let image = root.join("page");
    let png = root.join("page.png");
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut results = Vec::new();
    for page in pages {
        let (args, step) = render_args(*page, dpi, pdf, &image);
        let output = async {
            run(
                "pdftoppm",
                &args,
                root,
                1024 * 1024,
                deadline,
                step,
                &[(&png, 64 * 1024 * 1024)],
            )
            .await?;
            run(
                "tesseract",
                &strings(&[
                    &png.to_string_lossy(),
                    "stdout",
                    "-l",
                    "eng+deu",
                    "--psm",
                    "3",
                ]),
                root,
                4 * 1024 * 1024,
                deadline,
                step,
                &[],
            )
            .await
        }
        .await;
        results.push(match output { Ok(bytes) => { let t=text(&bytes); json!({"number":page,"text":t.chars().take(200000).collect::<String>(),"truncated":t.chars().count()>200000}) }, Err(()) => json!({"number":page,"error":OCR_ERROR}) });
        let _ = tokio::fs::remove_file(&png).await;
    }
    Ok(json!({"pages":results}))
}
fn info_pages(data: &[u8]) -> Result<u64, ()> {
    let info = String::from_utf8_lossy(data);
    if info.lines().any(|l| {
        l.strip_prefix("Encrypted:")
            .is_some_and(|s| s.trim_start().starts_with("yes"))
    }) {
        return Err(());
    }
    let pages = info
        .lines()
        .find_map(|l| {
            l.strip_prefix("Pages:")
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<u64>().ok())
        })
        .ok_or(())?;
    if !(1..=300).contains(&pages) {
        return Err(());
    }
    Ok(pages)
}
async fn page_count(pdf: &Path, root: &Path, deadline: Instant) -> Result<u64, ()> {
    info_pages(
        &run(
            "pdfinfo",
            &strings(&[&pdf.to_string_lossy()]),
            root,
            1024 * 1024,
            deadline,
            15,
            &[],
        )
        .await?,
    )
}
async fn reduce(data: &[u8]) -> Result<Value, ()> {
    if !data.starts_with(b"%PDF-") {
        return Err(());
    }
    let root = tempfile::tempdir().map_err(|_| ())?;
    let original = root.path().join("input.pdf");
    let native = root.path().join("text.txt");
    let output = root.path().join("reduced.pdf");
    tokio::fs::write(&original, data).await.map_err(|_| ())?;
    let deadline = Instant::now() + Duration::from_secs(160);
    let pages = page_count(&original, root.path(), deadline).await?;
    let text_ok = run(
        "pdftotext",
        &strings(&[
            "-enc",
            "UTF-8",
            "-layout",
            &original.to_string_lossy(),
            &native.to_string_lossy(),
        ]),
        root.path(),
        1024 * 1024,
        deadline,
        45,
        &[(&native, 190000)],
    )
    .await
    .is_ok()
        && std::fs::metadata(&native).is_ok_and(|m| m.len() > 0 && m.len() <= 190000);
    let compressed = run(
        "gs",
        &strings(&[
            "-dSAFER",
            "-dBATCH",
            "-dNOPAUSE",
            "-sDEVICE=pdfwrite",
            "-dCompatibilityLevel=1.6",
            "-dPDFSETTINGS=/ebook",
            "-dDetectDuplicateImages=true",
            "-dCompressFonts=true",
            &format!("-sOutputFile={}", output.to_string_lossy()),
            &original.to_string_lossy(),
        ]),
        root.path(),
        1024 * 1024,
        deadline,
        120,
        &[(&output, INPUT as u64)],
    )
    .await
    .is_ok();
    if compressed
        && std::fs::metadata(&output).is_ok_and(|m| m.len() > 0 && m.len() <= INPUT as u64)
        && page_count(&output, root.path(), deadline).await == Ok(pages)
    {
        let bytes = read_bounded(&output, INPUT as u64).await?;
        return Ok(json!({"kind":"pdf","dataBase64":STANDARD.encode(bytes),"note":PDF_NOTE}));
    }
    if text_ok {
        let bytes = read_bounded(&native, 190000).await?;
        let normalized = std::str::from_utf8(&bytes)
            .map_err(|_| ())?
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let t = normalized.trim_matches(docx_text::is_python_space);
        if !t.is_empty() {
            return Ok(
                json!({"kind":"text","text":format!("[{TEXT_NOTE}]\n\n{t}"),"note":TEXT_NOTE}),
            );
        }
    }
    Err(())
}
async fn handle(State(slot): State<Arc<Semaphore>>, req: Request) -> Response {
    let path = req.uri().path().to_owned();
    if req.method() == axum::http::Method::GET {
        // #1867: /health says what this worker reads; the 404 body is unchanged.
        return if req.uri() == "/health" {
            reply(200, health())
        } else {
            reply(404, json!({"service":"ocr"}))
        };
    }
    if req.method() != axum::http::Method::POST {
        return error(501, "Unsupported method");
    }
    if !["/extract", "/extract-docx", "/reduce-pdf"].contains(&req.uri().to_string().as_str()) {
        return error(404, "not found");
    }
    let Some((size, pages, dpi)) = selection(
        &path,
        req.headers()
            .get("content-length")
            .and_then(|s| s.to_str().ok()),
        req.headers()
            .get("x-ocr-pages")
            .and_then(|s| s.to_str().ok()),
        req.headers()
            .get("x-ocr-dpi")
            .map(|s| s.to_str().unwrap_or("invalid")),
    ) else {
        return error(400, INVALID);
    };
    let Ok(_permit) = slot.try_acquire() else {
        return error(503, "OCR is busy; refresh to retry.");
    };
    if path == "/extract" {
        // Streamed to the temp file a chunk at a time (#1867), never held whole.
        let Ok(root) = tempfile::Builder::new().prefix("noevia-ocr-").tempdir() else {
            return reply(
                200,
                json!({"pages":pages.iter().map(|p|json!({"number":p,"error":OCR_ERROR})).collect::<Vec<_>>()}),
            );
        };
        let pdf = root.path().join("input.pdf");
        if receive(req.into_body(), &pdf, size).await.is_err() {
            return error(400, "Incomplete document body.");
        }
        return match extract(root.path(), &pdf, &pages, dpi).await {
            Ok(v) => reply(200, v),
            Err(()) => reply(
                200,
                json!({"pages":pages.iter().map(|p|json!({"number":p,"error":OCR_ERROR})).collect::<Vec<_>>()}),
            ),
        };
    }
    let body = tokio::time::timeout(Duration::from_secs(30), to_bytes(req.into_body(), size)).await;
    let data = match body {
        Ok(Ok(data)) if data.len() == size => data,
        _ => return error(400, "Incomplete document body."),
    };
    match path.as_str() {
        "/extract-docx" => match docx_text::extract_docx(&data) {
            Ok(e) => reply(
                200,
                json!({"text":e.text,"truncated":e.truncated,"scope":docx_text::SCOPE}),
            ),
            Err(_) => error(422, DOCX_ERROR),
        },
        "/reduce-pdf" => match reduce(&data).await {
            Ok(v) => reply(200, v),
            Err(()) => error(422, REDUCE_ERROR),
        },
        _ => error(404, "not found"),
    }
}
fn healthy_status(data: &[u8]) -> bool {
    let line = String::from_utf8_lossy(data);
    let mut tokens = line.lines().next().unwrap_or_default().split_whitespace();
    matches!(tokens.next(), Some("HTTP/1.0" | "HTTP/1.1")) && tokens.next() == Some("200")
}
fn router() -> Router {
    Router::new()
        .fallback(any(handle))
        .with_state(Arc::new(Semaphore::new(1)))
}
// One request per connection eliminates idle keep-alive occupancy. Bound headers,
// connections and the full connection lifetime (30 s body + 600 s OCR + reply headroom).
#[derive(Clone, Copy)]
struct ServeLimits {
    connections: usize,
    header: Duration,
    lifetime: Duration,
}
async fn serve(listener: tokio::net::TcpListener, limits: ServeLimits) -> std::io::Result<()> {
    use tower::ServiceExt;
    let connections = Arc::new(Semaphore::new(limits.connections));
    let app = router();
    loop {
        let permit = connections
            .clone()
            .acquire_owned()
            .await
            .map_err(std::io::Error::other)?;
        let (stream, _) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = hyper::service::service_fn(
                move |request: axum::http::Request<hyper::body::Incoming>| {
                    let app = app.clone();
                    async move { app.oneshot(request.map(axum::body::Body::new)).await }
                },
            );
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(limits.header)
                .max_headers(32)
                .max_buf_size(32768)
                .keep_alive(false);
            let connection =
                builder.serve_connection(hyper_util::rt::TokioIo::new(stream), service);
            let _ = tokio::time::timeout(limits.lifetime, connection).await;
        });
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--healthcheck") {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut stream = tokio::net::TcpStream::connect("127.0.0.1:8030").await?;
            stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await?;
            let mut data = Vec::new();
            stream.take(4096).read_to_end(&mut data).await?;
            if !healthy_status(&data) {
                return Err(std::io::Error::other("OCR unavailable"));
            }
            Ok::<(), std::io::Error>(())
        })
        .await??;
        return Ok(());
    }
    if std::env::args().any(|a| a == "--features") {
        if cfg!(feature = "native-ocr") {
            println!("native-ocr");
            return Ok(());
        }
        return Err("native-ocr feature is not enabled".into());
    }
    if !cfg!(feature = "native-ocr") {
        return Err("native-ocr feature is not enabled".into());
    }
    let address = std::env::var("NOEVIA_OCR_LISTEN").unwrap_or_else(|_| "0.0.0.0:8030".to_owned());
    let listener = tokio::net::TcpListener::bind(address).await?;
    serve(
        listener,
        ServeLimits {
            connections: 32,
            header: Duration::from_secs(30),
            // #1867: plus the overall body deadline of the largest accepted /extract body.
            lifetime: Duration::from_secs(665 + (extract_limit() / BODY_RATE) as u64),
        },
    )
    .await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_and_rust_health_versions() {
        assert!(healthy_status(b"HTTP/1.0 200 OK\r\n"));
        assert!(healthy_status(b"HTTP/1.1 200 OK\r\n"));
        assert!(!healthy_status(b"HTTP/1.1 2000 BAD\r\n"));
        assert!(!healthy_status(b"HTTP/1.1 503 Busy\r\n"));
    }
    #[test]
    fn validation_boundaries() {
        for h in [
            "[]", "[true]", "[1.0]", "[0]", "[100001]", "[1,1]", "{}", "null",
        ] {
            assert!(selection("/extract", Some("1"), Some(h), None).is_none());
        }
        assert_eq!(
            selection("/extract", Some("1"), Some("[300,1]"), None),
            Some((1, vec![300, 1], 300))
        );
        // #1867: pages of long documents, bodies over 25 MiB on /extract only, exact DPIs.
        assert_eq!(
            selection(
                "/extract",
                Some("26214401"),
                Some("[301,100000]"),
                Some(" 400\t")
            ),
            Some((26214401, vec![301, 100000], 400))
        );
        assert!(selection("/extract", Some("100663297"), Some("[1]"), None).is_none());
        assert!(selection("/extract-docx", Some("26214401"), None, None).is_none());
        for dpi in ["+400", "4_00", "0400", "400.0", "72", "1200", "invalid"] {
            assert!(
                selection("/extract", Some("1"), Some("[1]"), Some(dpi)).is_none(),
                "{dpi}"
            );
        }
        assert_eq!(
            selection("/extract-docx", Some("1"), None, Some("junk")),
            Some((1, vec![1], 300))
        );
        assert_eq!(dpi_of(Some("")), Some(300));
        assert_eq!(dpi_of(Some("600")), Some(600));
    }
    #[test]
    fn a_retry_really_asks_the_renderer_for_more_pixels() {
        let (pdf, image) = (Path::new("/in.pdf"), Path::new("/page"));
        let side = |args: &[String], flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
                .cloned()
                .unwrap_or_default()
        };
        let (base, base_step) = render_args(7, 300, pdf, image);
        let (finer, finer_step) = render_args(7, 400, pdf, image);
        let (finest, _) = render_args(7, 600, pdf, image);
        assert_eq!(
            (side(&base, "-scale-to"), side(&base, "-r")),
            ("3500".into(), "300".into())
        );
        assert_eq!(
            (side(&finer, "-scale-to"), side(&finer, "-r")),
            ("4700".into(), "400".into())
        );
        assert_eq!(
            (side(&finest, "-scale-to"), side(&finest, "-r")),
            ("7000".into(), "600".into())
        );
        assert_eq!(
            (side(&base, "-f"), side(&base, "-l")),
            ("7".into(), "7".into())
        );
        assert!(finer_step > base_step);
        assert_eq!(
            health(),
            json!({"service":"ocr","ranges":true,"maxBytes":EXTRACT_DEFAULT,"dpis":[300,400,600]})
        );
    }
    proptest::proptest! { #[test] fn arbitrary_pages_never_escape_bounds(s in ".{0,4096}") { if let Some((_,pages,dpi))=selection("/extract",Some("1"),Some(&s),Some(&s)) { proptest::prop_assert!(!pages.is_empty() && pages.len()<=50); proptest::prop_assert!([300,400,600].contains(&dpi)); for p in pages {proptest::prop_assert!((1..=MAX_PAGE).contains(&p));} } } }
    #[test]
    fn encrypted_and_page_limits() {
        assert_eq!(info_pages(b"Pages: 300\nEncrypted: no"), Ok(300));
        for s in [
            b"Pages: 301".as_slice(),
            b"Pages: 0",
            b"Pages: 1\nEncrypted: yes",
            b"junk",
        ] {
            assert!(info_pages(s).is_err());
        }
    }
    #[tokio::test]
    async fn timeout_reaps_child() {
        let root = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
        assert!(run(
            "sleep",
            &strings(&["10"]),
            root.path(),
            10,
            Instant::now() + Duration::from_millis(10),
            1,
            &[]
        )
        .await
        .is_err());
    }
}

#[cfg(test)]
mod contract {
    use super::*;
    use axum::body::Body;
    use std::io::{Read, Write};
    use tower::ServiceExt;
    async fn native(
        method: &str,
        path: &str,
        length: Option<&str>,
        pages: Option<&str>,
        body: &[u8],
    ) -> (u16, Value) {
        native_dpi(method, path, length, pages, None, body).await
    }
    async fn native_dpi(
        method: &str,
        path: &str,
        length: Option<&str>,
        pages: Option<&str>,
        dpi: Option<&str>,
        body: &[u8],
    ) -> (u16, Value) {
        let mut request = axum::http::Request::builder().method(method).uri(path);
        if let Some(d) = dpi {
            request = request.header("X-OCR-DPI", d);
        }
        if let Some(n) = length {
            request = request.header("Content-Length", n);
        }
        if let Some(p) = pages {
            request = request.header("X-OCR-Pages", p);
        }
        let request = request
            .body(Body::from(body.to_vec()))
            .unwrap_or_else(|e| unreachable!("{e}"));
        let response = router()
            .oneshot(request)
            .await
            .unwrap_or_else(|e| unreachable!("{e}"));
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 40 * 1024 * 1024)
            .await
            .unwrap_or_else(|e| unreachable!("{e}"));
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or_else(|e| unreachable!("{e}")),
        )
    }
    fn legacy(
        port: u16,
        method: &str,
        path: &str,
        length: Option<&str>,
        pages: Option<&str>,
        body: &[u8],
    ) -> (u16, Value) {
        legacy_dpi(port, method, path, length, pages, None, body)
    }
    #[allow(clippy::too_many_arguments)]
    fn legacy_dpi(
        port: u16,
        method: &str,
        path: &str,
        length: Option<&str>,
        pages: Option<&str>,
        dpi: Option<&str>,
        body: &[u8],
    ) -> (u16, Value) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))
            .unwrap_or_else(|e| unreachable!("{e}"));
        stream
            .set_read_timeout(Some(Duration::from_secs(180)))
            .unwrap_or_else(|e| unreachable!("{e}"));
        stream
            .set_write_timeout(Some(Duration::from_secs(30)))
            .unwrap_or_else(|e| unreachable!("{e}"));
        write!(stream, "{method} {path} HTTP/1.0\r\nHost: localhost\r\n")
            .unwrap_or_else(|e| unreachable!("{e}"));
        if let Some(n) = length {
            write!(stream, "Content-Length: {n}\r\n").unwrap_or_else(|e| unreachable!("{e}"));
        }
        if let Some(p) = pages {
            write!(stream, "X-OCR-Pages: {p}\r\n").unwrap_or_else(|e| unreachable!("{e}"));
        }
        if let Some(d) = dpi {
            write!(stream, "X-OCR-DPI: {d}\r\n").unwrap_or_else(|e| unreachable!("{e}"));
        }
        stream
            .write_all(b"\r\n")
            .unwrap_or_else(|e| unreachable!("{e}"));
        stream
            .write_all(body)
            .unwrap_or_else(|e| unreachable!("{e}"));
        stream
            .shutdown(std::net::Shutdown::Write)
            .unwrap_or_else(|e| unreachable!("{e}"));
        let mut bytes = Vec::new();
        stream
            .take(40 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .unwrap_or_else(|e| unreachable!("{e}"));
        let split = bytes
            .windows(4)
            .position(|b| b == b"\r\n\r\n")
            .unwrap_or_else(|| unreachable!("missing response headers"));
        if path == "/health" {
            assert!(
                healthy_status(&bytes),
                "actual Python health status rejected"
            );
        }
        let status = String::from_utf8_lossy(bytes.get(..split).unwrap_or_default())
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();
        let body = serde_json::from_slice(bytes.get(split + 4..).unwrap_or_default())
            .unwrap_or_else(|e| unreachable!("{e}"));
        (status, body)
    }
    struct Reference(std::process::Child);
    impl Drop for Reference {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[tokio::test]
    async fn legacy_http_differential() {
        let Ok(reference) = std::env::var("NOEVIA_OCR_REFERENCE") else {
            eprintln!("SKIP legacy/real-engine differential: set NOEVIA_OCR_REFERENCE to pinned noevia-services/ocr (CI requires it)");
            return;
        };
        let socket =
            std::net::TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| unreachable!("{e}"));
        let port = socket
            .local_addr()
            .unwrap_or_else(|e| unreachable!("{e}"))
            .port();
        drop(socket);
        let mut child=Reference(std::process::Command::new("python3").args(["-c","import sys; from server import Handler, ThreadingHTTPServer; ThreadingHTTPServer(('127.0.0.1', int(sys.argv[1])), Handler).serve_forever()",&port.to_string()]).current_dir(&reference).env("DOCX_TEXT_IMPL","python").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap_or_else(|e|unreachable!("{e}")));
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(child
                .0
                .try_wait()
                .unwrap_or_else(|e| unreachable!("{e}"))
                .is_none());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        for (method, path, length, pages, body) in [
            ("GET", "/health", None, None, b"".as_slice()),
            ("GET", "/health?x=1", None, None, b""),
            ("GET", "/missing", None, None, b""),
            ("POST", "/missing", Some("1"), None, b"x"),
            ("POST", "/extract?x=1", Some("1"), Some("[1]"), b"x"),
            ("POST", "/extract", None, Some("[1]"), b""),
            ("POST", "/extract", Some("0"), Some("[1]"), b""),
            ("POST", "/extract", Some("100663297"), Some("[1]"), b""),
            ("POST", "/extract-docx", Some("26214401"), None, b""),
            ("POST", "/reduce-pdf", Some("62914561"), None, b""),
            ("POST", "/extract", Some("1"), Some("[true]"), b"x"),
            ("POST", "/extract", Some("1"), Some("[1.0]"), b"x"),
            ("POST", "/extract", Some("1"), Some("[1,1]"), b"x"),
            ("POST", "/extract", Some("1"), Some("[100001]"), b"x"),
            ("POST", "/extract", Some("1"), Some("[0]"), b"x"),
            ("POST", "/extract", Some("1"), Some("{}"), b"x"),
            ("POST", "/extract", Some("1"), Some("null"), b"x"),
            ("POST", "/extract", Some("2"), Some("[1]"), b"x"),
            ("POST", "/extract-docx", Some("1"), Some("junk"), b"x"),
            ("POST", "/reduce-pdf", Some("1"), None, b"x"),
        ] {
            assert_eq!(
                native(method, path, length, pages, body).await,
                legacy(port, method, path, length, pages, body),
                "{method} {path} {length:?} {pages:?}"
            );
        }
        // #1867: the X-OCR-DPI contract, refused or read exactly as the Python worker does.
        for (path, dpi) in [
            ("/extract", "72"),
            ("/extract", "+400"),
            ("/extract", "4_00"),
            ("/extract", "400.0"),
            ("/extract", "x"),
            ("/extract-docx", "junk"),
        ] {
            let pages = (path == "/extract").then_some("[1]");
            assert_eq!(
                native_dpi("POST", path, Some("1"), pages, Some(dpi), b"x").await,
                legacy_dpi(port, "POST", path, Some("1"), pages, Some(dpi), b"x"),
                "{path} DPI {dpi:?}"
            );
        }
        let cases: Value = serde_json::from_str(include_str!(
            "../../../crates/docx-text/tests/fixtures/docx-text.v1.json"
        ))
        .unwrap_or_else(|e| unreachable!("{e}"));
        for case in cases
            .get("cases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|c| c.get("kind").and_then(Value::as_str) == Some("case"))
        {
            let bytes = STANDARD
                .decode(case.get("docx").and_then(Value::as_str).unwrap_or_default())
                .unwrap_or_else(|e| unreachable!("{e}"));
            let length = bytes.len().to_string();
            let expected = legacy(port, "POST", "/extract-docx", Some(&length), None, &bytes);
            let actual = native("POST", "/extract-docx", Some(&length), None, &bytes).await;
            // Existing docx-text deliberately hardens ambiguous ZIP/XML; those recorded stricter cases remain explicit.
            if case.get("python") == case.get("rust") {
                assert_eq!(actual, expected, "DOCX {:?}", case.get("name"));
            }
        }
        if std::env::var("NOEVIA_OCR_REAL_ENGINES").as_deref() != Ok("1") {
            eprintln!("SKIP real engines: CI sets NOEVIA_OCR_REAL_ENGINES=1");
            return;
        }
        let root = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
        let status = std::process::Command::new("python3")
            .arg(Path::new(&reference).join("synthetic_pdfs.py"))
            .current_dir(root.path())
            .status()
            .unwrap_or_else(|e| unreachable!("{e}"));
        assert!(status.success());
        for (name, pages) in [
            ("scanned.pdf", "[1]"),
            ("mixed-page.pdf", "[1]"),
            ("mixed-pages.pdf", "[2]"),
            ("malformed.pdf", "[1]"),
            ("encrypted.pdf", "[1]"),
        ] {
            let bytes =
                std::fs::read(root.path().join(name)).unwrap_or_else(|e| unreachable!("{e}"));
            let length = bytes.len().to_string();
            let expected = legacy(port, "POST", "/extract", Some(&length), Some(pages), &bytes);
            let actual = native("POST", "/extract", Some(&length), Some(pages), &bytes).await;
            assert_eq!(actual, expected, "OCR {name}");
            if name == "scanned.pdf" {
                let output = serde_json::to_string(&actual.1).unwrap_or_default();
                for token in ["REF-2042", "42.15", "-7.20", "34.95"] {
                    assert!(output.contains(token), "real scan lost {token}");
                }
                let native_text = std::process::Command::new("pdftotext")
                    .arg(root.path().join(name))
                    .arg("-")
                    .output()
                    .unwrap_or_else(|e| unreachable!("{e}"));
                assert!(native_text.status.success());
                assert!(
                    text(&native_text.stdout).is_empty(),
                    "negative control: scanned fixture acquired a native text layer"
                );
                let mut mutant = actual.clone();
                mutant.1 = json!({"pages":[{"number":1,"text":"","truncated":false}]});
                assert_ne!(
                    mutant, expected,
                    "negative control failed to detect dropped OCR"
                );
            }
        }
        for name in ["malformed.pdf", "encrypted.pdf"] {
            let bytes =
                std::fs::read(root.path().join(name)).unwrap_or_else(|e| unreachable!("{e}"));
            let length = bytes.len().to_string();
            assert_eq!(
                native("POST", "/reduce-pdf", Some(&length), None, &bytes).await,
                legacy(port, "POST", "/reduce-pdf", Some(&length), None, &bytes)
            );
        }
        let status=std::process::Command::new("python3").args(["-c","import synthetic_pdfs,sys; open(sys.argv[1],'wb').write(synthetic_pdfs.oversized_text_pdf())"]).arg(root.path().join("oversized.pdf")).current_dir(&reference).status().unwrap_or_else(|e|unreachable!("{e}"));
        assert!(status.success());
        let bytes = std::fs::read(root.path().join("oversized.pdf"))
            .unwrap_or_else(|e| unreachable!("{e}"));
        assert!(bytes.len() > INPUT);
        let length = bytes.len().to_string();
        let expected = legacy(port, "POST", "/reduce-pdf", Some(&length), None, &bytes);
        let actual = native("POST", "/reduce-pdf", Some(&length), None, &bytes).await;
        assert_eq!(actual.0, expected.0);
        assert_eq!(actual.1.get("kind"), Some(&json!("pdf")));
        assert_eq!(actual.1.get("kind"), expected.1.get("kind"));
        assert_eq!(actual.1.get("note"), expected.1.get("note"));
        for (name, result) in [("rust", actual), ("python", expected)] {
            let output = STANDARD
                .decode(
                    result
                        .1
                        .get("dataBase64")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
                .unwrap_or_else(|e| unreachable!("{e}"));
            assert!(output.len() <= INPUT);
            let path = root.path().join(format!("{name}.pdf"));
            std::fs::write(&path, output).unwrap_or_else(|e| unreachable!("{e}"));
            let native_text = std::process::Command::new("pdftotext")
                .arg(path)
                .arg("-")
                .output()
                .unwrap_or_else(|e| unreachable!("{e}"));
            assert!(native_text.status.success());
            assert!(text(&native_text.stdout).contains("SYNTHETIC NATIVE TEXT"));
        }
        // #1867: a finer retry agrees with Python and really renders more pixels.
        let scanned =
            std::fs::read(root.path().join("scanned.pdf")).unwrap_or_else(|e| unreachable!("{e}"));
        let length = scanned.len().to_string();
        let expected = legacy_dpi(
            port,
            "POST",
            "/extract",
            Some(&length),
            Some("[1]"),
            Some("400"),
            &scanned,
        );
        let actual = native_dpi(
            "POST",
            "/extract",
            Some(&length),
            Some("[1]"),
            Some("400"),
            &scanned,
        )
        .await;
        assert_eq!(actual, expected, "OCR at 400 dpi");
        assert!(serde_json::to_string(&actual.1)
            .unwrap_or_default()
            .contains("REF-2042"));
        let mut longest = Vec::new();
        for dpi in [300, 400] {
            let image = root.path().join(format!("render-{dpi}"));
            let (args, _) = render_args(1, dpi, &root.path().join("scanned.pdf"), &image);
            let status = std::process::Command::new("pdftoppm")
                .args(&args)
                .status()
                .unwrap_or_else(|e| unreachable!("{e}"));
            assert!(status.success());
            let png =
                std::fs::read(image.with_extension("png")).unwrap_or_else(|e| unreachable!("{e}"));
            let field = |at: usize| {
                u32::from_be_bytes(
                    png.get(at..at + 4)
                        .and_then(|b| b.try_into().ok())
                        .unwrap_or_default(),
                )
            };
            longest.push(field(16).max(field(20)));
        }
        assert_eq!(
            longest,
            vec![3500, 4700],
            "the 400 dpi retry must render a larger page"
        );
        // #1867: ranged OCR of the image-only page of a PDF over 25 MiB, through both owners.
        let large = root.path().join("large.pdf");
        let status = std::process::Command::new("python3")
            .args(["-c", "import synthetic_pdfs,sys; synthetic_pdfs.write_ballast_pdf(sys.argv[1], 27 * 1024 * 1024)"])
            .arg(&large)
            .current_dir(&reference)
            .status()
            .unwrap_or_else(|e| unreachable!("{e}"));
        assert!(status.success());
        let bytes = std::fs::read(&large).unwrap_or_else(|e| unreachable!("{e}"));
        assert!(bytes.len() > INPUT);
        let length = bytes.len().to_string();
        let expected = legacy(port, "POST", "/extract", Some(&length), Some("[3]"), &bytes);
        let actual = native("POST", "/extract", Some(&length), Some("[3]"), &bytes).await;
        assert_eq!(actual, expected, "ranged OCR of a >25 MiB PDF");
        let output = serde_json::to_string(&actual.1).unwrap_or_default();
        for token in ["REF-2042", "42.15", "-7.20", "34.95"] {
            assert!(output.contains(token), "ranged OCR lost {token}: {output}");
        }
    }
    #[tokio::test]
    async fn busy_and_early_refusal_release() {
        let slot = Arc::new(Semaphore::new(1));
        let permit = slot.acquire().await.unwrap_or_else(|e| unreachable!("{e}"));
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/extract-docx")
            .header("Content-Length", "1")
            .body(Body::from("x"))
            .unwrap_or_else(|e| unreachable!("{e}"));
        assert_eq!(
            handle(State(slot.clone()), request).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(permit);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/extract-docx")
            .header("Content-Length", "2")
            .body(Body::from("x"))
            .unwrap_or_else(|e| unreachable!("{e}"));
        assert_eq!(
            handle(State(slot.clone()), request).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(slot.available_permits(), 1);
    }
    #[tokio::test]
    async fn output_and_deadline_caps_fail_closed() {
        let root = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
        assert!(run(
            "sh",
            &strings(&["-c", "printf 'this is too much output'"]),
            root.path(),
            1,
            Instant::now() + Duration::from_secs(2),
            1,
            &[]
        )
        .await
        .is_err());
        assert!(run(
            "nonexistent-noevia-synthetic",
            &[],
            root.path(),
            10,
            Instant::now(),
            1,
            &[]
        )
        .await
        .is_err());
        assert_eq!(text("\u{1c}\u{85}ä\u{3000}".as_bytes()), "ä");
    }
}

#[cfg(test)]
mod resource_regressions {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn listener_bounds_connections_and_stalled_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| unreachable!("{e}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|e| unreachable!("{e}"));
        let task = tokio::spawn(serve(
            listener,
            ServeLimits {
                connections: 1,
                header: Duration::from_millis(250),
                lifetime: Duration::from_secs(2),
            },
        ));
        let mut stalled = tokio::net::TcpStream::connect(address)
            .await
            .unwrap_or_else(|e| unreachable!("{e}"));
        stalled
            .write_all(b"GET /health HTTP/1.1\r\nHost:")
            .await
            .unwrap_or_else(|e| unreachable!("{e}"));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut second = tokio::net::TcpStream::connect(address)
            .await
            .unwrap_or_else(|e| unreachable!("{e}"));
        second
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap_or_else(|e| unreachable!("{e}"));
        let mut byte = [0u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(50), second.read(&mut byte))
                .await
                .is_err(),
            "second connection bypassed cap"
        );
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), second.read_to_end(&mut output))
            .await
            .unwrap_or_else(|e| unreachable!("{e}"))
            .unwrap_or_else(|e| unreachable!("{e}"));
        assert!(
            healthy_status(&output),
            "slot was not recovered after header timeout"
        );
        let read = tokio::time::timeout(Duration::from_secs(1), stalled.read(&mut byte))
            .await
            .unwrap_or_else(|e| unreachable!("{e}"));
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "stalled headers did not close"
        );
        task.abort();
        let _ = task.await;
    }
    fn assert_not_running(pid: &str) {
        let result = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap_or_else(|e| unreachable!("{e}"));
        let state = String::from_utf8_lossy(&result.stdout);
        assert!(
            state.trim().is_empty() || state.trim_start().starts_with('Z'),
            "engine descendant {pid} still executing ({state})"
        );
    }
    #[tokio::test]
    async fn successful_parent_cannot_leave_descendant_writing_output() {
        let root = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
        let pid = root.path().join("child.pid");
        let result = run(
            "sh",
            &strings(&[
                "-c",
                "sleep 10 & echo $! > \"$1\"; printf ok",
                "sh",
                &pid.to_string_lossy(),
            ]),
            root.path(),
            10,
            Instant::now() + Duration::from_secs(2),
            1,
            &[],
        )
        .await;
        assert_eq!(result, Ok(b"ok".to_vec()));
        let descendant = std::fs::read_to_string(pid).unwrap_or_else(|e| unreachable!("{e}"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_not_running(descendant.trim());
    }
    #[tokio::test]
    async fn cancellation_kills_process_group_and_caps_file_reads() {
        let root = tempfile::tempdir().unwrap_or_else(|e| unreachable!("{e}"));
        let pid = root.path().join("child.pid");
        let path = root.path().to_path_buf();
        let pid_arg = pid.clone();
        let task = tokio::spawn(async move {
            run(
                "sh",
                &strings(&[
                    "-c",
                    "sleep 10 & echo \"$$ $!\" > \"$1\"; wait",
                    "sh",
                    &pid_arg.to_string_lossy(),
                ]),
                &path,
                10,
                Instant::now() + Duration::from_secs(20),
                20,
                &[],
            )
            .await
        });
        for _ in 0..100 {
            if std::fs::metadata(&pid).is_ok_and(|m| m.len() > 0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let ids = std::fs::read_to_string(&pid).unwrap_or_else(|e| unreachable!("{e}"));
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        for id in ids.split_whitespace() {
            assert_not_running(id);
        }
        let output = root.path().join("oversized");
        std::fs::write(&output, vec![b'x'; 1024 * 1024]).unwrap_or_else(|e| unreachable!("{e}"));
        assert!(read_bounded(&output, 7).await.is_err());
        assert_eq!(
            read_bounded(&output, 1024 * 1024).await.map(|v| v.len()),
            Ok(1024 * 1024)
        );
    }
}
