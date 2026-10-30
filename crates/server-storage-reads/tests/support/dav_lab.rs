//! A synthetic, stateful WebDAV server for the storage write tests (sbstndalton/noevia#2191
//! round 3): MKCOL (405 existing, 409 no parent), PUT (If-None-Match `*`, If-Match, ETag `"v<n>"`),
//! DELETE (If-Match; a collection's ETag is `"v1"` empty, `"v2"` not), PROPFIND Depth 0/1. Basic
//! `u:pw` only (else 401); any path starting `moved` answers a 302 elsewhere. Shared by
//! server-storage-reads tests/writes_child.rs and noevia-server tests/project_storage.rs.
#![allow(dead_code, clippy::unwrap_used, clippy::indexing_slicing)]
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
pub struct Dav {
    pub files: BTreeMap<String, (Vec<u8>, u64)>,
    pub dirs: BTreeSet<String>,
    pub next: u64,
    pub seen: Vec<(String, String)>,
}

fn decode(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            let h = u8::from_str_radix(&p[i + 1..i + 3], 16).unwrap();
            out.push(h);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn respond(stream: &mut TcpStream, status: u16, headers: &str, body: &[u8]) {
    let mut out = format!(
        "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    let _ = stream.write_all(&out);
}

fn handle(mut stream: TcpStream, dav: &Mutex<Dav>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 65536];
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap().split(' ');
    let method = first.next().unwrap().to_owned();
    let target = first.next().unwrap().to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map_or(0, |v| v.parse().unwrap());
    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => body.extend_from_slice(&buf[..n]),
        }
    }
    let mut d = dav.lock().unwrap();
    d.seen.push((method.clone(), target.clone()));
    // Basic u:pw only.
    if headers.get("authorization").map(String::as_str) != Some("Basic dTpwdw==") {
        return respond(&mut stream, 401, "", b"");
    }
    let Some(path) = target.strip_prefix("/dav/") else {
        return respond(&mut stream, 404, "", b"");
    };
    let path = decode(path);
    if path.starts_with("moved") {
        return respond(
            &mut stream,
            302,
            "Location: http://127.0.0.1:9/dav/elsewhere\r\n",
            b"",
        );
    }
    let key = path.trim_end_matches('/').to_owned();
    let parent = key.rsplit_once('/').map_or("", |(p, _)| p).to_owned();
    let parent_ok = parent.is_empty() || d.dirs.contains(&parent);
    let etag_of = |n: u64| format!("\"v{n}\"");
    let if_match = headers.get("if-match").cloned();
    match method.as_str() {
        "MKCOL" => {
            if d.dirs.contains(&key) || d.files.contains_key(&key) {
                respond(&mut stream, 405, "", b"")
            } else if !parent_ok {
                respond(&mut stream, 409, "", b"")
            } else {
                d.dirs.insert(key);
                respond(&mut stream, 201, "", b"")
            }
        }
        "PUT" => {
            if !parent_ok || d.dirs.contains(&key) {
                return respond(&mut stream, 409, "", b"");
            }
            let existing = d.files.get(&key).map(|(_, e)| *e);
            if headers.get("if-none-match").map(String::as_str) == Some("*") && existing.is_some() {
                return respond(&mut stream, 412, "", b"");
            }
            if let Some(m) = &if_match {
                if existing.map(etag_of).as_ref() != Some(m) {
                    return respond(&mut stream, 412, "", b"");
                }
            }
            d.next += 1;
            let n = d.next;
            d.files.insert(key, (body, n));
            let status = if existing.is_some() { 204 } else { 201 };
            respond(
                &mut stream,
                status,
                &format!("ETag: {}\r\n", etag_of(n)),
                b"",
            )
        }
        "DELETE" => {
            if let Some((_, e)) = d.files.get(&key) {
                if if_match.is_some_and(|m| m != etag_of(*e)) {
                    return respond(&mut stream, 412, "", b"");
                }
                d.files.remove(&key);
                respond(&mut stream, 204, "", b"")
            } else if d.dirs.contains(&key) {
                let busy = d
                    .files
                    .keys()
                    .chain(d.dirs.iter())
                    .any(|k| k.starts_with(&format!("{key}/")));
                if if_match.as_deref() != Some(&etag_of(if busy { 2 } else { 1 })) {
                    return respond(&mut stream, 412, "", b"");
                }
                d.dirs.remove(&key);
                respond(&mut stream, 204, "", b"")
            } else {
                respond(&mut stream, 404, "", b"")
            }
        }
        "PROPFIND" => {
            let depth = headers.get("depth").cloned().unwrap_or_default();
            let entry = |href: &str, dir: bool, etag: &str| {
                format!(
                    "<d:response><d:href>/dav/{href}</d:href><d:propstat><d:prop><d:resourcetype>{}</d:resourcetype><d:getetag>{}</d:getetag></d:prop></d:propstat></d:response>",
                    if dir { "<d:collection/>" } else { "" },
                    etag.replace('"', "&quot;")
                )
            };
            let mut xml = String::from("<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\">");
            if let Some((_, e)) = d.files.get(&key) {
                xml.push_str(&entry(&key, false, &etag_of(*e)));
            } else if d.dirs.contains(&key) {
                let children: Vec<String> = d
                    .files
                    .keys()
                    .chain(d.dirs.iter())
                    .filter(|k| k.rsplit_once('/').map_or("", |(p, _)| p) == key)
                    .cloned()
                    .collect();
                xml.push_str(&entry(
                    &format!("{key}/"),
                    true,
                    &etag_of(if children.is_empty() { 1 } else { 2 }),
                ));
                if depth == "1" {
                    for c in children {
                        xml.push_str(&entry(&c, false, "\"c\""));
                    }
                }
            } else {
                return respond(&mut stream, 404, "", b"");
            }
            xml.push_str("</d:multistatus>");
            respond(
                &mut stream,
                207,
                "Content-Type: application/xml\r\n",
                xml.as_bytes(),
            )
        }
        _ => respond(&mut stream, 405, "", b""),
    }
}

pub fn server() -> (u16, Arc<Mutex<Dav>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let dav = Arc::new(Mutex::new(Dav::default()));
    let shared = Arc::clone(&dav);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || handle(stream, &shared));
        }
    });
    (port, dav)
}
