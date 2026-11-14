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

fn error(status: u16, message: &str) -> Response<Body> {
    reply(
        status,
        js_json::stringify(&JValue::obj([("error", JValue::Str(message.into()))]))
            .unwrap_or_default(),
    )
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
        if status.is_redirection() {
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
    // The arguments are the model's text: read losslessly (a lone surrogate is kept).
    let body = match wtf16::parse(&String::from_utf8_lossy(raw)) {
        Ok(b @ JValue::Obj(_)) => b,
        _ => return error(400, "invalid JSON"),
    };
    let name = match body.get("name") {
        JValue::Str(n) if !n.is_empty() && n.len() <= 256 => n.clone(),
        _ => return error(400, "invalid name"),
    };
    let cap = match body.get("cap") {
        JValue::Num(n) if n.fract() == 0.0 && (1.0..=1_048_576.0).contains(n) => *n as usize,
        JValue::Undefined => kiwix_tools::DEFAULT_CAP,
        _ => return error(400, "invalid cap"),
    };
    let args = body.get("arguments").clone();
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return error(503, "unavailable");
    };
    let rt = tokio::runtime::Handle::current();
    let run = tokio::task::spawn_blocking(move || {
        let root = base.origin().to_string();
        let mut get = |url: &str| -> Result<String, String> {
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
            reply(200, wtf16::stringify(&v).unwrap_or_default())
        }
        Err(_) => error(503, "unavailable"),
    }
}

#[cfg(test)]
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
}
