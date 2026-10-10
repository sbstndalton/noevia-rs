//! The built web client, ported from core server/static-files.cjs and server/spa-routes.cjs.
//!
//! Hashed files under /assets/ are immutable for a year; .html and /version.json are no-store
//! (iOS standalone shells, noevia#311; the stale-shell guard); everything else is no-cache with an
//! ETag (the first 27 base64url characters of the body's sha256, `-br`/`-gzip` suffixed for an
//! encoded body). Text files over 1 KiB are offered brotli (quality 8) or gzip (level 9),
//! compressed once per file version and kept in memory. A file of the build wins; otherwise a
//! client place (`/c/<id>`, `/settings/<section>`, ... spa-routes.cjs CLIENT_ROUTES) gets
//! index.html; anything else a JSON 404.
//!
//! Only canonical paths are answered here ([`canonical`]); a path whose URL parsing in Node could
//! differ (dot segments, backslashes, characters WHATWG URL would percent-encode, `//`) is left
//! to Node, which stays the authority on those.

use crate::reply;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode};
use base64::Engine as _;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const REVALIDATE: &str = "no-cache";
const NO_STORE: &str = "no-store";

fn content_type(ext: &str) -> &'static str {
    match ext {
        ".html" => "text/html; charset=utf-8",
        ".js" => "text/javascript; charset=utf-8",
        ".css" => "text/css; charset=utf-8",
        ".svg" => "image/svg+xml",
        ".png" => "image/png",
        ".woff2" => "font/woff2",
        ".woff" => "font/woff",
        ".ico" => "image/x-icon",
        ".json" => "application/json",
        ".txt" => "text/plain; charset=utf-8",
        ".webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

fn compressible(ext: &str) -> bool {
    matches!(
        ext,
        ".html" | ".js" | ".css" | ".svg" | ".json" | ".txt" | ".webmanifest"
    )
}

/// Node path.extname: the last `.` of the file name that is not its first character.
fn extname(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match name.rfind('.') {
        Some(i) if i > 0 => name.get(i..).unwrap_or("").to_string(),
        _ => String::new(),
    }
}

/// True when Node's WHATWG URL parsing leaves this path exactly as received, so resolving it here
/// gives the file Node would serve.
pub fn canonical(path: &str) -> bool {
    if !path.starts_with('/') || path.starts_with("//") || path.contains("//") {
        return false;
    }
    let ok_byte = |b: u8| b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/%".contains(&b);
    if !path.bytes().all(ok_byte) {
        return false;
    }
    path.split('/').all(|seg| {
        let s = seg.to_ascii_lowercase().replace("%2e", ".");
        s != "." && s != ".."
    })
}

/// spa-routes.cjs isClientRoute.
pub fn is_client_route(p: &str) -> bool {
    if p.len() > 700 {
        return false;
    }
    if p.starts_with("/api/") || p == "/api" || p.starts_with("/assets/") {
        return false;
    }
    if p.contains('\\') || p.starts_with("//") {
        return false;
    }
    if p == "/" {
        return true;
    }
    // ^/models/[^/]{1,600}$ (no optional trailing slash).
    if let Some(seg) = p.strip_prefix("/models/") {
        if !seg.is_empty() && seg.chars().count() <= 600 && !seg.contains('/') {
            return true;
        }
    }
    // Every other pattern allows one trailing slash.
    let t = p.strip_suffix('/').unwrap_or(p);
    let Some(t) = t.strip_prefix('/') else {
        return false;
    };
    let segs: Vec<&str> = t.split('/').collect();
    let id = |s: &str| {
        (1..=120).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    let section = |s: &str| {
        let mut b = s.bytes();
        matches!(b.next(), Some(c) if c.is_ascii_lowercase())
            && s.len() <= 40
            && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    };
    const PLACES: &[&str] = &[
        "chat",
        "projects",
        "diary",
        "archived",
        "code",
        "models",
        "settings",
        "customise",
        "customize",
        "plugins",
    ];
    const CUSTOMISE: &[&str] = &["customise", "customize", "plugins"];
    const CUSTOMISE_TABS: &[&str] = &["skills", "connectors", "plugins", "mcp", "connected"];
    const PROJECT_TABS: &[&str] = &["new", "chats", "sources", "research", "code", "browser"];
    match segs.as_slice() {
        [one] => PLACES.contains(one) || *one == "c" || *one == "p" || *one == "device",
        ["c", x] | ["p", x] => id(x),
        ["p", x, tab] => id(x) && PROJECT_TABS.contains(tab),
        ["settings", s] => section(s),
        [c, tab] => CUSTOMISE.contains(c) && CUSTOMISE_TABS.contains(tab),
        _ => false,
    }
}

struct Entry {
    mtime: Option<SystemTime>,
    size: u64,
    body: Bytes,
    ext: String,
    etag: String,
    br: Option<Bytes>,
    gzip: Option<Bytes>,
}

impl Entry {
    fn compressed(&self) -> bool {
        self.br.is_some() || self.gzip.is_some()
    }
}

fn etag_of(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    format!("\"{}\"", b64.get(..27).unwrap_or(&b64))
}

fn brotli(body: &[u8]) -> Option<Bytes> {
    let mut out = Vec::with_capacity(body.len() / 3);
    {
        let mut w = brotli::CompressorWriter::new(&mut out, 4096, 8, 22);
        w.write_all(body).ok()?;
        w.flush().ok()?;
    }
    Some(Bytes::from(out))
}

fn gzip(body: &[u8]) -> Option<Bytes> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(9));
    e.write_all(body).ok()?;
    e.finish().ok().map(Bytes::from)
}

#[derive(Clone)]
pub struct StaticFiles {
    root: PathBuf,
    cache: Arc<Mutex<HashMap<PathBuf, Arc<Entry>>>>,
}

enum Encoding {
    Br,
    Gzip,
}

/// static-files.cjs encodingFor: a coding is accepted unless its first `q=` parameter is 0.
fn accepted(headers: &HeaderMap) -> (bool, bool) {
    let raw = headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(", ");
    let (mut br, mut gz) = (false, false);
    for part in raw.split(',') {
        let mut items = part.split(';').map(|s| s.trim().to_ascii_lowercase());
        let name = items.next().unwrap_or_default();
        let q_zero = items
            .find(|p| p.starts_with("q="))
            .map(|q| {
                let v = q.get(2..).unwrap_or("").trim();
                v.is_empty() || v.parse::<f64>().is_ok_and(|n| n == 0.0)
            })
            .unwrap_or(false);
        if name.is_empty() || q_zero {
            continue;
        }
        if name == "br" {
            br = true;
        }
        if name == "gzip" {
            gz = true;
        }
    }
    (br, gz)
}

impl StaticFiles {
    pub fn new(root: PathBuf) -> Self {
        // Node joins onto an absolute DIST_DIR; a relative one is taken from the working dir.
        let root = std::path::absolute(&root).unwrap_or(root);
        StaticFiles {
            root,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The file of the build a canonical URL path names, if it is one.
    pub fn resolve(&self, url_path: &str) -> Option<PathBuf> {
        let rel = if url_path == "/" {
            "index.html"
        } else {
            // A trailing slash never names a file (Node's stat of `file/` fails).
            if url_path.ends_with('/') {
                return None;
            }
            url_path.trim_start_matches('/')
        };
        let file = self.root.join(rel);
        match std::fs::metadata(&file) {
            Ok(m) if m.is_file() => Some(file),
            _ => None,
        }
    }

    fn entry(&self, file: &Path) -> std::io::Result<Arc<Entry>> {
        let meta = std::fs::metadata(file)?;
        let mtime = meta.modified().ok();
        if let Ok(cache) = self.cache.lock() {
            if let Some(e) = cache.get(file) {
                if e.mtime == mtime && e.size == meta.len() {
                    return Ok(Arc::clone(e));
                }
            }
        }
        let body = Bytes::from(std::fs::read(file)?);
        let ext = extname(file);
        let (br, gzip) = if compressible(&ext) && body.len() > 1024 {
            (brotli(&body), gzip(&body))
        } else {
            (None, None)
        };
        let entry = Arc::new(Entry {
            mtime,
            size: meta.len(),
            etag: etag_of(&body),
            body,
            ext,
            br,
            gzip,
        });
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(file.to_path_buf(), Arc::clone(&entry));
        }
        Ok(entry)
    }

    fn send(
        &self,
        method: &Method,
        headers: &HeaderMap,
        file: &Path,
        url_path: &str,
    ) -> std::io::Result<Response<Body>> {
        let entry = self.entry(file)?;
        let hashed = url_path.starts_with("/assets/");
        let (br_ok, gz_ok) = accepted(headers);
        let encoding = match (&entry.br, &entry.gzip) {
            (Some(_), _) if br_ok => Some(Encoding::Br),
            (_, Some(_)) if gz_ok => Some(Encoding::Gzip),
            _ => None,
        };
        let cache_control = if hashed {
            IMMUTABLE
        } else if entry.ext == ".html" || url_path == "/version.json" {
            NO_STORE
        } else {
            REVALIDATE
        };
        let etag = match encoding {
            Some(Encoding::Br) => format!("{}-br\"", entry.etag.trim_end_matches('"')),
            Some(Encoding::Gzip) => format!("{}-gzip\"", entry.etag.trim_end_matches('"')),
            None => entry.etag.clone(),
        };
        let mut res = Response::new(Body::empty());
        let h = res.headers_mut();
        reply::security_headers(h, false);
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type(&entry.ext)),
        );
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        );
        if let Ok(v) = HeaderValue::from_str(&etag) {
            h.insert(header::ETAG, v);
        }
        if entry.compressed() {
            h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        }
        let tags: Vec<String> = headers
            .get_all(header::IF_NONE_MATCH)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(",")
            .split(',')
            .map(|t| {
                let t = t.trim();
                t.strip_prefix("W/").unwrap_or(t).to_string()
            })
            .collect();
        if tags.iter().any(|t| *t == etag || t == "*") {
            *res.status_mut() = StatusCode::NOT_MODIFIED;
            return Ok(res);
        }
        let body = match encoding {
            Some(Encoding::Br) => entry.br.clone().unwrap_or_default(),
            Some(Encoding::Gzip) => entry.gzip.clone().unwrap_or_default(),
            None => entry.body.clone(),
        };
        let h = res.headers_mut();
        match encoding {
            Some(Encoding::Br) => {
                h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
            }
            Some(Encoding::Gzip) => {
                h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            }
            None => {}
        }
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        if method != Method::HEAD {
            *res.body_mut() = Body::from(body);
        }
        Ok(res)
    }

    /// spa-routes.cjs createStaticFallback for a canonical GET/HEAD path.
    pub fn serve(&self, method: &Method, headers: &HeaderMap, path: &str) -> Response<Body> {
        let found = self.resolve(path);
        if found.is_none() && !is_client_route(path) {
            return reply::error(StatusCode::NOT_FOUND, "not found", false);
        }
        let (file, url_path) = match found {
            Some(f) => (f, path),
            None => (self.root.join("index.html"), "/"),
        };
        match self.send(method, headers, &file, url_path) {
            Ok(res) => res,
            Err(_) => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "read error", false),
        }
    }

    /// Compress the whole build once in the background so the first visitor does not wait.
    pub fn warm(&self) {
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.walk(&me.root.clone()));
    }

    fn walk(&self, dir: &Path) {
        let Ok(items) = std::fs::read_dir(dir) else {
            return;
        };
        for item in items.flatten() {
            let Ok(kind) = item.file_type() else { continue };
            if kind.is_dir() {
                self.walk(&item.path());
            } else if kind.is_file() {
                let _ = self.entry(&item.path());
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn client_routes_match_spa_routes_cjs() {
        for p in [
            "/",
            "/chat",
            "/chat/",
            "/projects",
            "/c",
            "/c/",
            "/c/abc_1-2",
            "/p",
            "/p/x",
            "/p/x/chats",
            "/p/x/browser/",
            "/settings/models",
            "/settings/a-1",
            "/customise/skills",
            "/plugins/connected/",
            "/models/foo",
            "/models/a%2Fb",
            "/device",
            "/device/",
        ] {
            assert!(is_client_route(p), "{p}");
        }
        for p in [
            "/api/x",
            "/api",
            "/assets/a.js",
            "/nope",
            "/c/a/b",
            "/p/x/other",
            "/settings/Models",
            "/settings/1a",
            "/models/a/",
            "//c",
            "/c\\x",
            "/customise/other",
            "/chat//",
            "/x/skills",
        ] {
            assert!(!is_client_route(p), "{p}");
        }
        assert!(!is_client_route(&format!("/c/{}", "a".repeat(121))));
        assert!(is_client_route(&format!("/settings/a{}", "b".repeat(39))));
        assert!(!is_client_route(&format!("/settings/a{}", "b".repeat(40))));
        assert!(!is_client_route(&format!("/models/{}", "a".repeat(701))));
    }

    /// tests/fixtures/client-routes.json is spa-routes.cjs isClientRoute's answer for each path
    /// (generated with node from noevia-core at contracts/http/core.ref).
    #[test]
    fn client_routes_agree_with_node_fixture() {
        let raw = include_str!("../tests/fixtures/client-routes.json");
        let cases: Vec<(String, bool)> = serde_json::from_str(raw).unwrap();
        assert!(cases.len() > 60);
        for (p, want) in cases {
            assert_eq!(is_client_route(&p), want, "{p}");
        }
    }

    #[test]
    fn canonical_paths() {
        for p in [
            "/",
            "/assets/a-1.js",
            "/c/x",
            "/models/a%2Fb",
            "/.well-known/x",
        ] {
            assert!(canonical(p), "{p}");
        }
        for p in [
            "/../x",
            "/a/./b",
            "/a/%2e%2E/b",
            "/a/%2E",
            "//x",
            "/a//b",
            "/a\\b",
            "/a b",
            "/a{b",
            "/a\"b",
            "x",
            "/a|b",
            "/é",
        ] {
            assert!(!canonical(p), "{p}");
        }
    }

    #[test]
    fn accept_encoding_parse() {
        let h = |v: &str| {
            let mut m = HeaderMap::new();
            m.insert(header::ACCEPT_ENCODING, HeaderValue::from_str(v).unwrap());
            accepted(&m)
        };
        assert_eq!(h("gzip, deflate, br"), (true, true));
        assert_eq!(h("br;q=0, gzip"), (false, true));
        assert_eq!(h("br;q=0.0, gzip;q="), (false, false));
        assert_eq!(h("BR;q=0.5"), (true, false));
        assert_eq!(h("*"), (false, false));
        assert_eq!(accepted(&HeaderMap::new()), (false, false));
    }

    #[test]
    fn etag_shape() {
        // Node: '"' + sha256(body).base64url.slice(0, 27) + '"'
        assert_eq!(etag_of(b"hello"), "\"LPJNul-wow4m6DsqxbninhsWHlw\"");
    }

    #[test]
    fn extname_like_node() {
        assert_eq!(extname(Path::new("/a/b.c.js")), ".js");
        assert_eq!(extname(Path::new("/a/.hidden")), "");
        assert_eq!(extname(Path::new("/a/noext")), "");
    }
}
