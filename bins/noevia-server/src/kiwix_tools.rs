//! The chat's Offline Wikipedia tools (core kiwix.cjs, D9), run by the front (Rust port track R5): crate
//! kiwix-tools over this front's HTTP client.
//!
//! Switch: `NOEVIA_RUST_KIWIX_TOOLS=1` (off by default). Off, `POST /v1/kiwix/tool` is a 404 and Node runs
//! the tools itself; on, Node asks here (core kiwix.cjs `native`).
//!
//! `POST /v1/kiwix/tool { name, arguments, cap? }` (the store secret checked by the caller) -> 200
//! `{ text }`: the tool's answer, an `ERROR: …` text included, `null` for a name that is not one of the two
//! tools; 400 for a malformed request; 404 while off; 503 when `KIWIX_URL` is unset or not a plain http(s)
//! origin (Node then has no Offline Wikipedia box to call either).
//!
//! The fetch is core's `get`: no redirect followed (a redirect is `fetch failed`), 10 s for the request and
//! its body (`The operation was aborted due to timeout`), a status outside 200-299 is
//! `offline Wikipedia answered <status>`, and at most 2,000,000 bytes of the body are read as UTF-8.
//!
//! Named difference: a request whose address, once resolved, is not under `KIWIX_URL`'s own path (a read
//! path with a percent-encoded `..` segment, which core's check misses and its fetch resolves) is never
//! sent; the tool answers `ERROR: use a path returned by wikipedia_search`, core's refusal of such a path.
use crate::App;
use axum::{body::Body, http::StatusCode, response::Response};
use js_json::{wtf16, JValue};
use std::sync::Arc;
use std::time::Duration;

/// The switch: `1` hands the Offline Wikipedia tools to the front.
pub const SWITCH: &str = "NOEVIA_RUST_KIWIX_TOOLS";
const REFUSED_PATH: &str = "use a path returned by wikipedia_search";

#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub tools: bool,
    pub url: Option<String>,
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Settings {
        Settings {
            tools: get(SWITCH).as_deref() == Some("1"),
            url: get("KIWIX_URL").filter(|u| !u.is_empty()),
        }
    }
}

fn reply(status: u16, text: String) -> Response<Body> {
    let mut res = Response::new(Body::from(text));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    res.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("application/json"),
    );
    res.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    res
}

fn error_text(message: &str) -> String {
    js_json::stringify(&JValue::obj([("error", JValue::Str(message.into()))])).unwrap_or_default()
}

fn error(status: u16, message: &str) -> Response<Body> {
    reply(status, error_text(message))
}

/// Whether `url` resolves to one of the two addresses the tools use under `base`: an article
/// (`<base>/content/…`) or the search (`<base>/search`).
pub fn stays_under(base: &str, url: &str) -> bool {
    let (Ok(b), Ok(u)) = (url::Url::parse(base), url::Url::parse(url)) else {
        return false;
    };
    let prefix = b.path().trim_end_matches('/');
    let path = u.path();
    b.origin() == u.origin()
        && (path == format!("{prefix}/search") || path.starts_with(&format!("{prefix}/content/")))
}

/// `readCappedText(r, cap)` as kiwix.cjs uses it: at most `cap` bytes as UTF-8 (invalid sequences become
/// U+FFFD; a body cut at the cap loses one U+FFFD at its very end, a character the cut split).
pub fn capped_text(bytes: &[u8], capped: bool) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if capped && text.ends_with('\u{FFFD}') {
        text.pop();
    }
    text
}

/// core's `get`: the body text, or the message its fetch would throw.
async fn fetch(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let run = async {
        let mut res = client
            .get(url)
            .send()
            .await
            .map_err(|_| "fetch failed".to_string())?;
        let status = res.status();
        // undici's `redirect: 'error'` refuses exactly these; any other 3xx is a status like the rest.
        if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Err("fetch failed".to_string());
        }
        if !status.is_success() {
            return Err(kiwix_tools::status_error(status.as_u16()));
        }
        let mut body: Vec<u8> = Vec::new();
        let mut capped = false;
        while let Some(chunk) = res.chunk().await.map_err(|_| "terminated".to_string())? {
            let room = kiwix_tools::BODY_CAP_BYTES.saturating_sub(body.len());
            if chunk.len() > room {
                body.extend_from_slice(chunk.get(..room).unwrap_or_default());
                capped = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(capped_text(&body, capped))
    };
    tokio::time::timeout(Duration::from_millis(kiwix_tools::TIMEOUT_MS), run)
        .await
        .unwrap_or_else(|_| Err("The operation was aborted due to timeout".to_string()))
}

/// `POST /v1/kiwix/tool` after the store secret was checked.
pub async fn internal_op(app: Option<Arc<App>>, raw: &[u8]) -> Response<Body> {
    let Some(app) = app else {
        return error(503, "unavailable");
    };
    if !app.config.kiwix.tools {
        return error(404, "not found");
    }
    let Some(base) = app
        .config
        .kiwix
        .url
        .as_deref()
        .and_then(|u| kiwix_tools::base(u).ok())
    else {
        return error(503, "unavailable");
    };
    answer(&base, raw).await
}

/// One tool call against `base`: the store route's status and JSON text.
pub async fn run(base: &kiwix_tools::Base, raw: &[u8]) -> (u16, String) {
    // The arguments are the model's text: read losslessly (a lone surrogate is kept).
    let body = match wtf16::parse(&String::from_utf8_lossy(raw)) {
        Ok(b @ JValue::Obj(_)) => b,
        _ => return (400, error_text("invalid JSON")),
    };
    let name = match body.get("name") {
        JValue::Str(n) if !n.is_empty() && n.len() <= 256 => n.clone(),
        _ => return (400, error_text("invalid name")),
    };
    let cap = match body.get("cap") {
        JValue::Num(n) if n.fract() == 0.0 && (1.0..=1_048_576.0).contains(n) => *n as usize,
        JValue::Undefined => kiwix_tools::DEFAULT_CAP,
        _ => return (400, error_text("invalid cap")),
    };
    let args = body.get("arguments").clone();
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // Node's fetch ignores HTTP_PROXY and friends; so does this client.
        .no_proxy()
        .build()
    else {
        return (503, error_text("unavailable"));
    };
    let rt = tokio::runtime::Handle::current();
    let base = base.clone();
    let run = tokio::task::spawn_blocking(move || {
        let root = base.origin().to_string();
        let get = |url: &str| -> Result<String, String> {
            if !stays_under(&root, url) {
                return Err(String::new());
            }
            rt.block_on(fetch(&client, url))
        };
        // A refused address answers as core's own refusal of the path, never `ERROR: Error`.
        let mut refused = false;
        let mut guarded = |url: &str| {
            let r = get(url);
            if matches!(&r, Err(m) if m.is_empty()) {
                refused = true;
            }
            r
        };
        let text = kiwix_tools::execute(&base, &name, &args, cap, &mut guarded);
        if refused {
            Some(format!("ERROR: {REFUSED_PATH}"))
        } else {
            text
        }
    })
    .await;
    match run {
        Ok(text) => {
            let v = JValue::obj([("text", text.map_or(JValue::Null, JValue::Str))]);
            (200, wtf16::stringify(&v).unwrap_or_default())
        }
        Err(_) => (503, error_text("unavailable")),
    }
}

async fn answer(base: &kiwix_tools::Base, raw: &[u8]) -> Response<Body> {
    let (status, text) = run(base, raw).await;
    reply(status, text)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn only_addresses_under_the_base_path_are_fetched() {
        let base = "http://kiwix.qa.invalid:8080/wiki";
        assert!(stays_under(
            base,
            "http://kiwix.qa.invalid:8080/wiki/content/a"
        ));
        assert!(stays_under(
            base,
            "http://kiwix.qa.invalid:8080/wiki/search?pattern=x"
        ));
        assert!(!stays_under(
            base,
            "http://kiwix.qa.invalid:8080/wiki/content/../../catalog"
        ));
        assert!(!stays_under(
            base,
            "http://kiwix.qa.invalid:8080/wiki/content/../skin/x.js"
        ));
        assert!(!stays_under(
            base,
            "http://kiwix.qa.invalid:8080/wiki/searchx"
        ));
        assert!(!stays_under(
            base,
            "http://kiwix.qa.invalid:8080/wikipedia/content/a"
        ));
        assert!(!stays_under(
            base,
            "http://other.qa.invalid:8080/wiki/content/a"
        ));
        assert!(stays_under(
            "http://kiwix.qa.invalid",
            "http://kiwix.qa.invalid/content/a"
        ));
        assert!(!stays_under(
            "http://kiwix.qa.invalid",
            "http://kiwix.qa.invalid/content/%2e%2e/catalog"
        ));
    }

    #[test]
    fn a_cut_body_loses_a_split_character() {
        assert_eq!(capped_text("ab\u{e9}".as_bytes(), false), "ab\u{e9}");
        assert_eq!(capped_text(&"ab\u{e9}".as_bytes()[..3], true), "ab");
        assert_eq!(capped_text(&[b'a', 0xff, b'b'], false), "a\u{FFFD}b");
    }

    #[test]
    fn the_switch_is_exactly_one() {
        let s = |v: Option<&str>| {
            Settings::from_lookup(&|k: &str| (k == SWITCH).then(|| v.map(str::to_string)).flatten())
        };
        assert!(!s(None).tools);
        assert!(!s(Some("true")).tools);
        assert!(s(Some("1")).tools);
    }

    /// A synthetic kiwix-serve on 127.0.0.1: QA pages, every request path recorded.
    async fn qa_kiwix() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = listener.accept().await {
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = c.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split(' ').nth(1).unwrap_or("").to_string();
                    log.lock().unwrap().push(path.clone());
                    let (status, body) = if path.starts_with("/search?") {
                        ("200 OK", "<rss><item><title>QA cell</title><link>/content/qa/A/QA_cell</link><description>A QA cell.</description></item></rss>".to_string())
                    } else if path == "/content/qa/A/QA_cell" {
                        ("200 OK", "<html><h1>QA cell</h1><p>The QA cell stores synthetic energy.</p></html>".to_string())
                    } else if path == "/content/qa/A/Moved" {
                        ("302 Found", String::new())
                    } else {
                        ("404 Not Found", String::new())
                    };
                    let extra = if status.starts_with("302") {
                        "location: /content/qa/A/QA_cell\r\n"
                    } else {
                        ""
                    };
                    let out = format!("HTTP/1.1 {status}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    let _ = c.write_all(out.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn text(answer: &(u16, String)) -> String {
        assert_eq!(answer.0, 200, "{}", answer.1);
        js_json::parse(&answer.1)
            .unwrap()
            .get("text")
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_route_runs_the_tools_against_kiwix() {
        let (url, seen) = qa_kiwix().await;
        let base = kiwix_tools::base(&url).unwrap();
        let search = text(
            &run(
                &base,
                br#"{"name":"wikipedia_search","arguments":{"query":"QA cell"}}"#,
            )
            .await,
        );
        assert!(
            search.contains("QA cell") && search.contains("path: /content/qa/A/QA_cell"),
            "{search}"
        );
        let read = text(&run(&base, br#"{"name":"wikipedia_read","arguments":{"path":"/content/qa/A/QA_cell"},"cap":300}"#).await);
        assert!(
            read.contains("The QA cell stores synthetic energy."),
            "{read}"
        );
        // A redirect is a failure, as core's `redirect: 'error'`; a 404 names its status.
        assert_eq!(
            text(
                &run(
                    &base,
                    br#"{"name":"wikipedia_read","arguments":{"path":"/content/qa/A/Moved"}}"#
                )
                .await
            ),
            "ERROR: fetch failed"
        );
        assert_eq!(
            text(
                &run(
                    &base,
                    br#"{"name":"wikipedia_read","arguments":{"path":"/content/qa/A/Missing"}}"#
                )
                .await
            ),
            "ERROR: offline Wikipedia answered 404"
        );
        // The named difference: an encoded `..` that would leave /content/ is never sent.
        let before = seen.lock().unwrap().len();
        assert_eq!(
            text(
                &run(
                    &base,
                    br#"{"name":"wikipedia_read","arguments":{"path":"/content/%2e%2e/catalog"}}"#
                )
                .await
            ),
            "ERROR: use a path returned by wikipedia_search"
        );
        assert_eq!(seen.lock().unwrap().len(), before);
        // Not one of the tools: `null`, as core's `undefined`.
        assert_eq!(
            run(&base, br#"{"name":"wikipedia_other","arguments":{}}"#).await,
            (200, r#"{"text":null}"#.to_string())
        );
        for bad in [
            &br#"[]"#[..],
            br#"{"arguments":{}}"#,
            br#"{"name":"wikipedia_search","cap":0}"#,
            b"{",
        ] {
            assert_eq!(run(&base, bad).await.0, 400);
        }
    }
}
