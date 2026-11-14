//! sbstndalton/noevia#2615 item 3: a document goes to the extraction services as a sized stream
//! (upload-body.cjs `uploadInit`): an explicit Content-Length, no chunked encoding (the sidecars
//! refuse one), and exactly the document's bytes, also across many slices and for an empty file.
//! Synthetic sidecars only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::extraction::HttpEngine;
use server_projects::extract::Engine;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The tests set the process environment (`DOCLING_BASE_URL`, `OCR_BASE_URL`): one at a time.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const PAGES: &str = r#"{"total":1,"pages":[{"number":1,"text":"Synthetic total 42","status":"native","method":"docling"}]}"#;

/// What the service saw of one request: its path, its Content-Length, whether it was chunked, and
/// the sha256 of the body it read.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    path: String,
    length: Option<usize>,
    chunked: bool,
    digest: String,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A service as the sidecars are: a request without a Content-Length (or a chunked one) is refused
/// with 411; any other is read to its length and answered with `PAGES`.
async fn service(seen: Arc<Mutex<Vec<Seen>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = vec![0u8; 65536];
                let (head, mut body) = loop {
                    let Ok(n) = s.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break (
                            String::from_utf8_lossy(&buf[..i]).to_string(),
                            buf[i + 4..].to_vec(),
                        );
                    }
                };
                let header = |name: &str| {
                    head.lines()
                        .filter_map(|l| l.split_once(": "))
                        .find(|(k, _)| k.eq_ignore_ascii_case(name))
                        .map(|(_, v)| v.trim().to_string())
                };
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split(' ').nth(1))
                    .unwrap_or("")
                    .to_string();
                let length = header("content-length").and_then(|v| v.parse::<usize>().ok());
                let chunked = header("transfer-encoding").is_some();
                if let Some(len) = length.filter(|_| !chunked) {
                    while body.len() < len {
                        match s.read(&mut chunk).await {
                            Ok(n) if n > 0 => body.extend_from_slice(&chunk[..n]),
                            _ => break,
                        }
                    }
                }
                seen.lock().unwrap().push(Seen {
                    path,
                    length,
                    chunked,
                    digest: digest(&body),
                });
                let (status, text) = if length.is_none() || chunked {
                    (411, "{}")
                } else {
                    (200, PAGES)
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    url
}

/// A synthetic document of `len` bytes that is not one repeated slice.
fn document(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 1021) as u8).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_document_request_is_sized_and_carries_exactly_the_document() {
    let _env = ENV.lock().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let url = service(seen.clone()).await;
    std::env::set_var("DOCLING_BASE_URL", &url);
    std::env::set_var("OCR_BASE_URL", &url);
    // Empty, smaller than a slice, exactly two slices, and a ragged tail across three.
    let sizes = [0usize, 5, 2 * 1024 * 1024, 2 * 1024 * 1024 + 4097];
    let docs: Vec<Vec<u8>> = sizes.iter().map(|n| document(*n)).collect();
    let engine = HttpEngine::new(tokio::runtime::Handle::current(), None)
        .unwrap()
        .with_liveness(Duration::from_secs(30), 3);
    let run = docs.clone();
    let outcomes = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for doc in &run {
                out.push(
                    engine
                        .docling("a.pdf", doc, false, None)
                        .map(|_| ())
                        .map_err(|e| e.message),
                );
                out.push(
                    engine
                        .ocr_pages(doc, &[1], None, None)
                        .map(|_| ())
                        .map_err(|e| e.message),
                );
                out.push(engine.extract_docx(doc).map(|_| ()).map_err(|e| e.message));
                out.push(
                    engine
                        .reduce_pdf(doc)
                        .expect("an OCR worker is configured")
                        .map(|_| ())
                        .map_err(|e| e.message),
                );
            }
            out
        }),
    )
    .await
    .expect("the requests must end within 60 s")
    .unwrap();
    std::env::remove_var("DOCLING_BASE_URL");
    std::env::remove_var("OCR_BASE_URL");
    assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");
    let seen = seen.lock().unwrap().clone();
    let mut expected = Vec::new();
    for doc in &docs {
        for path in ["/extract", "/extract", "/extract-docx", "/reduce-pdf"] {
            expected.push(Seen {
                path: path.to_string(),
                length: Some(doc.len()),
                chunked: false,
                digest: digest(doc),
            });
        }
    }
    assert_eq!(seen, expected);
}
