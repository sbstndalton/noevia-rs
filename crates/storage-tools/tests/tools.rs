#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! storage-tools over a fake transport serving a small synthetic tree (QA names only).

use js_json::JValue;
use server_storage_reads::protocol::{
    decoded_fixture, execute, Connection, Kind, Method, Operation, Request, Response, Transport,
    TransportFailure,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use storage_tools::{
    manifest_line, parse, private_refusal, run, Call, Mode, CREATE_FOLDER, DELETE_FILE, FIND_FILES,
    LIST_FOLDER, MAX_CONTENT_BYTES, MAX_DEPTH, MAX_FOLDERS, MAX_MATCHES, PRIVATE_REFUSAL,
    READ_FILE, TOOLS, WRITE_FILE, WRITE_UNSUPPORTED,
};
use zeroize::Zeroizing;

/// The connection base, with a raw `$` as some servers' space URLs have.
const BASE: &str = "https://qa-storage.invalid/dav/spaces/qa$1/";
const HREF_BASE: &str = "/dav/spaces/qa$1/";
const USER: &str = "qa-user-fixture";
const SECRET: &str = "qa-secret-fixture";

fn connection() -> Connection {
    Connection {
        kind: Kind::WebDav,
        base_url: BASE.into(),
        username: USER.into(),
        secret: Zeroizing::new(SECRET.as_bytes().to_vec()),
        corpus_root: "Diary".into(),
        bucket: String::new(),
        region: String::new(),
    }
}

/// A synthetic tree: relative path -> `None` for a folder, `Some(text)` for a file. Every file
/// has the version `etag` (none when empty); `stale` makes every `If-Match` fail (412).
struct Fake {
    now: Duration,
    step: Duration,
    tree: BTreeMap<String, Option<String>>,
    failing: BTreeSet<String>,
    requests: Vec<(Method, String)>,
    /// Each request's headers, the Authorization header left out.
    headers: Vec<Vec<(String, String)>>,
    etag: String,
    stale: bool,
}

impl Fake {
    fn new(entries: &[(&str, Option<&str>)]) -> Self {
        Self {
            now: Duration::ZERO,
            step: Duration::from_millis(10),
            tree: entries
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.map(str::to_owned)))
                .collect(),
            failing: BTreeSet::new(),
            requests: Vec::new(),
            headers: Vec::new(),
            etag: "\"qa-v1\"".into(),
            stale: false,
        }
    }
    /// The folder listings (Depth 1 PROPFINDs) requested.
    fn listed(&self) -> Vec<String> {
        self.requests
            .iter()
            .zip(&self.headers)
            .filter(|((m, _), h)| *m == Method::Propfind && header(h, "Depth") != Some("0"))
            .map(|((_, p), _)| p.clone())
            .collect()
    }
    /// A Depth 0 PROPFIND: the state of one path.
    fn state(&self, path: &str) -> (u16, String) {
        if self.failing.contains(path) {
            return (500, String::new());
        }
        let prop = match self.tree.get(path) {
            None => return (404, String::new()),
            Some(None) => "<d:resourcetype><d:collection/></d:resourcetype>".to_owned(),
            Some(Some(_)) if self.etag.is_empty() => "<d:resourcetype/>".to_owned(),
            Some(Some(_)) => {
                let etag = &self.etag;
                format!("<d:resourcetype/><d:getetag>{etag}</d:getetag>")
            }
        };
        (
            207,
            format!(
                "<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>{HREF_BASE}{path}</d:href><d:propstat><d:prop>{prop}</d:prop></d:propstat></d:response></d:multistatus>"
            ),
        )
    }
    fn put(&mut self, path: &str, headers: &[(String, String)], body: &[u8]) -> (u16, String) {
        let exists = self.tree.contains_key(path);
        if header(headers, "If-None-Match") == Some("*") && exists {
            return (412, String::new());
        }
        if let Some(wanted) = header(headers, "If-Match") {
            if self.stale || !exists || wanted != self.etag {
                return (412, String::new());
            }
        }
        if self.tree.get(path) == Some(&None) {
            return (405, String::new());
        }
        self.tree.insert(
            path.to_owned(),
            Some(String::from_utf8_lossy(body).into_owned()),
        );
        (if exists { 204 } else { 201 }, "\"qa-v2\"".to_owned())
    }
    fn mkcol(&mut self, path: &str) -> (u16, String) {
        if self.tree.contains_key(path) {
            return (405, String::new());
        }
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        if !parent.is_empty() && self.tree.get(parent) != Some(&None) {
            return (409, String::new());
        }
        self.tree.insert(path.to_owned(), None);
        (201, String::new())
    }
    fn delete(&mut self, path: &str) -> (u16, String) {
        match self.tree.get(path) {
            Some(Some(_)) => {
                self.tree.remove(path);
                (204, String::new())
            }
            _ => (404, String::new()),
        }
    }
    fn listing(&self, dir: &str) -> (u16, String) {
        if self.failing.contains(dir) {
            return (500, String::new());
        }
        if !dir.is_empty() && self.tree.get(dir) != Some(&None) {
            return (404, String::new());
        }
        let own = if dir.is_empty() {
            HREF_BASE.to_owned()
        } else {
            format!("{HREF_BASE}{dir}/")
        };
        let mut out = String::from("<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\">");
        out.push_str(&format!(
            "<d:response><d:href>{own}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>"
        ));
        for (path, node) in &self.tree {
            let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
            if parent != dir {
                continue;
            }
            match node {
                None => out.push_str(&format!(
                    "<d:response><d:href>{HREF_BASE}{path}/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>"
                )),
                Some(text) => {
                    let size = text.len();
                    out.push_str(&format!(
                        "<d:response><d:href>{HREF_BASE}{path}</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>{size}</d:getcontentlength></d:prop></d:propstat></d:response>"
                    ));
                }
            }
        }
        out.push_str("</d:multistatus>");
        (207, out)
    }
    fn file(&self, path: &str) -> (u16, String) {
        match self.tree.get(path) {
            Some(Some(text)) if !self.failing.contains(path) => (200, text.clone()),
            _ => (404, String::new()),
        }
    }
}

impl Transport for Fake {
    fn elapsed(&self) -> Duration {
        self.now
    }
    fn amz_date(&self) -> String {
        "20261114T000000Z".into()
    }
    fn attempt(&mut self, request: Request) -> Result<Response, TransportFailure> {
        self.now += self.step;
        let rel = request
            .url
            .strip_prefix(BASE)
            .ok_or(TransportFailure::Other)?
            .trim_matches('/')
            .to_owned();
        let headers: Vec<(String, String)> = request
            .headers
            .iter()
            .filter(|(k, _)| k != "Authorization")
            .map(|(k, v)| (k.clone(), String::clone(v)))
            .collect();
        self.requests.push((request.method, rel.clone()));
        self.headers.push(headers.clone());
        let (status, body) = match request.method {
            Method::Propfind if header(&headers, "Depth") == Some("0") => self.state(&rel),
            Method::Propfind => self.listing(&rel),
            Method::Get => self.file(&rel),
            Method::Put => self.put(&rel, &headers, &request.body),
            Method::Mkcol => self.mkcol(&rel),
            Method::Delete => self.delete(&rel),
        };
        Ok(Response {
            status,
            body: decoded_fixture(body.as_bytes(), request.body_cap)
                .map_err(|_| TransportFailure::Body)?,
        })
    }
    fn delay(&mut self, delay: Duration) -> Result<(), TransportFailure> {
        self.now += delay;
        Ok(())
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn tree() -> Fake {
    Fake::new(&[
        ("Notes", None),
        ("Notes/qa-plan.md", Some("# QA plan\n")),
        ("Notes/qa-list.txt", Some("one\ntwo\n")),
        ("Notes/Archive", None),
        ("Notes/Archive/old-qa-plan.MD", Some("old")),
        ("Broken", None),
        ("Broken/hidden-plan.md", Some("never listed")),
        ("Diary", None),
        ("Diary/2026-11.md", Some("synthetic private entry")),
        ("Diary2", None),
        ("Diary2/qa-plan-copy.md", Some("copy")),
        ("readme-qa.txt", Some("hello")),
    ])
}

fn assert_no_leak(text: &str) {
    for secret in ["qa-storage.invalid", "/dav/spaces", USER, SECRET] {
        assert!(!text.contains(secret), "{secret} leaked into {text}");
    }
}

fn find_json(fake: &mut Fake, args: &str, skip: Option<&str>) -> JValue {
    let call = parse(FIND_FILES, args).unwrap();
    let out = run(&call, &connection(), fake, skip).unwrap();
    assert_no_leak(&out);
    js_json::parse(&out).unwrap()
}

fn match_paths(value: &JValue) -> Vec<String> {
    let JValue::Arr(items) = value.get("matches") else {
        panic!("matches is not an array");
    };
    items
        .iter()
        .map(|m| m.get("path").as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn manifest_line_lists_three_read_tools() {
    let line = js_json::parse(&manifest_line()).unwrap();
    assert_eq!(line.get("id").as_str(), Some("storage-files"));
    assert_eq!(line.get("server").as_str(), Some("noevia"));
    assert_eq!(line.get("label").as_str(), Some("Storage files"));
    assert!(line.get("description").as_str().is_some());
    let names = |key: &str| -> Vec<String> {
        let JValue::Arr(items) = line.get(key) else {
            panic!("{key} is not an array");
        };
        items
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };
    let reads = vec![
        "storage_list_folder".to_owned(),
        "storage_read_file".to_owned(),
        "storage_find_files".to_owned(),
    ];
    let mut tools = reads.clone();
    tools.extend([
        "storage_write_file".to_owned(),
        "storage_create_folder".to_owned(),
        "storage_delete_file".to_owned(),
    ]);
    assert_eq!(names("tools"), tools);
    assert_eq!(names("reads"), reads);
    assert_eq!(TOOLS.len(), 6);
    for tool in TOOLS {
        assert_eq!(
            tool.write,
            !reads.contains(&tool.name.to_owned()),
            "{}",
            tool.name
        );
        if tool.write {
            assert!(tool.description.contains("approval"), "{}", tool.name);
        }
        let schema = js_json::parse(tool.input_schema).unwrap();
        assert_eq!(schema.get("type").as_str(), Some("object"));
        assert!(schema.get("properties").is_object());
        assert!(tool.description.len() < 200);
    }
}

#[test]
fn parse_accepts_good_shapes() {
    assert_eq!(
        parse(LIST_FOLDER, "{}").unwrap(),
        Call::List {
            path: String::new()
        }
    );
    assert_eq!(
        parse(LIST_FOLDER, "").unwrap(),
        Call::List {
            path: String::new()
        }
    );
    assert_eq!(
        parse(LIST_FOLDER, r#"{"path":"Notes"}"#).unwrap(),
        Call::List {
            path: "Notes".into()
        }
    );
    assert_eq!(
        parse(LIST_FOLDER, r#"{"path":null}"#).unwrap(),
        Call::List {
            path: String::new()
        }
    );
    assert_eq!(
        parse(READ_FILE, r#"{"path":"Notes/qa-plan.md"}"#).unwrap(),
        Call::Read {
            path: "Notes/qa-plan.md".into()
        }
    );
    assert_eq!(
        parse(FIND_FILES, r#"{"name":"plan"}"#).unwrap(),
        Call::Find {
            name: Some("plan".into()),
            extension: None,
            folder: String::new()
        }
    );
    assert_eq!(
        parse(FIND_FILES, r#"{"extension":".MD","folder":"Notes"}"#).unwrap(),
        Call::Find {
            name: None,
            extension: Some("md".into()),
            folder: "Notes".into()
        }
    );
    assert_eq!(
        parse(FIND_FILES, r#"{"name":"plan","extension":"md"}"#).unwrap(),
        Call::Find {
            name: Some("plan".into()),
            extension: Some("md".into()),
            folder: String::new()
        }
    );
    let at_cap = format!("{{\"path\":\"{}\"}}", "a".repeat(500));
    assert!(parse(LIST_FOLDER, &at_cap).is_ok());
}

#[test]
fn parse_refuses_bad_shapes() {
    let long = format!("{{\"path\":\"{}\"}}", "a".repeat(501));
    let long_name = format!("{{\"name\":\"{}\"}}", "a".repeat(501));
    let cases = [
        ("storage_write_file", r#"{"path":"x.md"}"#),
        ("nc_files_list", "{}"),
        (LIST_FOLDER, "[]"),
        (LIST_FOLDER, r#""Notes""#),
        (LIST_FOLDER, "null"),
        (LIST_FOLDER, "not json"),
        (LIST_FOLDER, r#"{"path":1}"#),
        (LIST_FOLDER, r#"{"path":["Notes"]}"#),
        (LIST_FOLDER, r#"{"path":"Notes","recursive":true}"#),
        (LIST_FOLDER, long.as_str()),
        (READ_FILE, "{}"),
        (READ_FILE, r#"{"path":""}"#),
        (READ_FILE, r#"{"path":null}"#),
        (READ_FILE, r#"{"path":true}"#),
        (READ_FILE, r#"{"file":"x.md"}"#),
        (FIND_FILES, "{}"),
        (FIND_FILES, r#"{"folder":"Notes"}"#),
        (FIND_FILES, r#"{"name":"","extension":""}"#),
        (FIND_FILES, r#"{"name":5}"#),
        (FIND_FILES, r#"{"extension":"a/b"}"#),
        (FIND_FILES, r#"{"extension":"a\\b"}"#),
        (FIND_FILES, r#"{"extension":"abcdefghijklmnopq"}"#),
        (FIND_FILES, r#"{"extension":"*.md"}"#),
        (FIND_FILES, r#"{"extension":"tar.gz"}"#),
        (FIND_FILES, r#"{"extension":"..md"}"#),
        (FIND_FILES, r#"{"extension":"m d"}"#),
        (FIND_FILES, r#"{"name":"x","folder":7}"#),
        (FIND_FILES, r#"{"name":"x","path":"Notes"}"#),
        (FIND_FILES, long_name.as_str()),
    ];
    for (tool, args) in cases {
        let error = parse(tool, args).err().unwrap_or_else(|| {
            panic!("{tool} {args} was accepted");
        });
        assert!(!error.message.is_empty());
        assert!(error.message.len() < 120, "{}", error.message);
        assert_eq!(error.to_string(), error.message);
    }
    // Exactly sixteen characters is still an extension.
    assert!(parse(FIND_FILES, r#"{"extension":"abcdefghijklmnop"}"#).is_ok());
    let huge = format!("{{\"path\":\"a\",\"x\":\"{}\"}}", "b".repeat(9000));
    assert!(parse(LIST_FOLDER, &huge).is_err());
}

#[test]
fn private_refusal_covers_the_diary_folder_only() {
    let list = |path: &str| Call::List { path: path.into() };
    let read = |path: &str| Call::Read { path: path.into() };
    let find = |folder: &str| Call::Find {
        name: Some("plan".into()),
        extension: None,
        folder: folder.into(),
    };
    for call in [
        list("Diary"),
        list("Diary/"),
        list("/Diary"),
        list("diary"),
        list("DIARY/2026"),
        list(" Diary "),
        list("Diary//2026"),
        read("Diary/2026-11.md"),
        read("diary\\2026-11.md"),
        find("Diary"),
        find("dIaRy/sub"),
    ] {
        assert_eq!(
            private_refusal(&call, Kind::WebDav, "Diary").as_deref(),
            Some(PRIVATE_REFUSAL),
            "{call:?}"
        );
    }
    for root in ["/Diary/", "diary", "Diary/", "DIARY"] {
        assert!(
            private_refusal(&list("Diary/x"), Kind::WebDav, root).is_some(),
            "{root}"
        );
    }
    for call in [
        list(""),
        list("Notes"),
        list("Diary2"),
        list("Diary2/Diary"),
        list("MyDiary"),
        read("Diary.md"),
        read("Notes/Diary/x.md"),
        find(""),
        find("Notes"),
    ] {
        assert_eq!(
            private_refusal(&call, Kind::WebDav, "Diary"),
            None,
            "{call:?}"
        );
    }
    // A nested Diary folder.
    assert!(private_refusal(&list("Private/Diary/x"), Kind::WebDav, "Private/Diary").is_some());
    assert!(private_refusal(&list("Private"), Kind::WebDav, "Private/Diary").is_none());
    // A root that normalises to nothing (or is refused outright) refuses every call: fail closed.
    let too_big = "x".repeat(70_000);
    for root in [
        "",
        "/",
        "//",
        "   ",
        "..",
        "Diary/../x",
        "./",
        too_big.as_str(),
    ] {
        for call in [list(""), list("Notes"), read("Notes/qa-plan.md"), find("")] {
            assert!(
                private_refusal(&call, Kind::WebDav, root).is_some(),
                "{root:?} {call:?}"
            );
        }
    }
}

#[test]
fn list_and_read_return_the_engines_json() {
    let c = connection();
    let units = |s: &str| s.encode_utf16().collect::<Vec<u16>>();

    let mut fake = tree();
    let out = run(
        &parse(LIST_FOLDER, r#"{"path":"Notes"}"#).unwrap(),
        &c,
        &mut fake,
        None,
    )
    .unwrap();
    let mut direct = tree();
    let expected = execute(&c, &units("Notes"), Operation::List, &mut direct).unwrap();
    assert_eq!(out, expected);
    assert!(out.contains(r#""path":"Notes/qa-plan.md""#), "{out}");
    assert!(
        out.contains(r#""name":"Archive","path":"Notes/Archive","isDir":true"#),
        "{out}"
    );
    assert_no_leak(&out);

    let mut fake = tree();
    let root = run(
        &parse(LIST_FOLDER, "{}").unwrap(),
        &c,
        &mut fake,
        Some("Diary"),
    )
    .unwrap();
    assert!(root.contains(r#""path":"readme-qa.txt""#), "{root}");
    assert_eq!(fake.listed(), vec![String::new()]);

    let mut fake = tree();
    let out = run(
        &parse(READ_FILE, r#"{"path":"Notes/qa-plan.md"}"#).unwrap(),
        &c,
        &mut fake,
        None,
    )
    .unwrap();
    let mut direct = tree();
    let expected = execute(&c, &units("Notes/qa-plan.md"), Operation::Read, &mut direct).unwrap();
    assert_eq!(out, expected);
    assert_eq!(
        out,
        "{\"name\":\"qa-plan.md\",\"content\":\"# QA plan\\n\",\"truncated\":false}"
    );
    assert_no_leak(&out);

    // Engine failures pass through as their fixed public text.
    let mut fake = tree();
    let error = run(
        &parse(READ_FILE, r#"{"path":"Notes/missing.md"}"#).unwrap(),
        &c,
        &mut fake,
        None,
    )
    .unwrap_err();
    assert_eq!(error.message, "storage returned 404");
    let error = run(
        &parse(READ_FILE, r#"{"path":"../x.md"}"#).unwrap(),
        &c,
        &mut fake,
        None,
    )
    .unwrap_err();
    assert_eq!(error.message, "invalid path");
    let error = run(
        &parse(LIST_FOLDER, r#"{"path":"../x"}"#).unwrap(),
        &c,
        &mut fake,
        None,
    )
    .unwrap_err();
    assert_eq!(
        error.message,
        "Use a relative path inside the storage folder."
    );

    // With `skip`, a path inside it is refused before any request.
    let mut fake = tree();
    for call in [
        parse(LIST_FOLDER, r#"{"path":"diary"}"#).unwrap(),
        parse(READ_FILE, r#"{"path":"Diary/2026-11.md"}"#).unwrap(),
        parse(FIND_FILES, r#"{"name":"x","folder":"Diary/"}"#).unwrap(),
    ] {
        let error = run(&call, &c, &mut fake, Some("/Diary/")).unwrap_err();
        assert_eq!(error.message, PRIVATE_REFUSAL);
    }
    let error = run(&parse(LIST_FOLDER, "{}").unwrap(), &c, &mut fake, Some("")).unwrap_err();
    assert_eq!(error.message, PRIVATE_REFUSAL);
    assert!(fake.requests.is_empty());
}

#[test]
fn find_by_name_and_by_extension() {
    let mut fake = tree();
    let value = find_json(&mut fake, r#"{"name":"PLAN"}"#, None);
    let mut paths = match_paths(&value);
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "Broken/hidden-plan.md",
            "Diary2/qa-plan-copy.md",
            "Notes/Archive/old-qa-plan.MD",
            "Notes/qa-plan.md"
        ]
    );
    assert_eq!(value.get("complete").as_bool(), Some(true));
    // Root, Broken, Diary, Diary2, Notes, Notes/Archive.
    assert_eq!(value.get("foldersSearched").as_num(), Some(6.0));
    let JValue::Arr(items) = value.get("matches") else {
        panic!("no matches");
    };
    let plan = items
        .iter()
        .find(|m| m.get("path").as_str() == Some("Notes/qa-plan.md"))
        .unwrap();
    assert_eq!(plan.get("size").as_num(), Some(10.0));

    let mut fake = tree();
    let value = find_json(&mut fake, r#"{"extension":"txt"}"#, None);
    let mut paths = match_paths(&value);
    paths.sort();
    assert_eq!(paths, vec!["Notes/qa-list.txt", "readme-qa.txt"]);

    // With or without the dot, any case; name and extension together; from a start folder.
    let mut fake = tree();
    let value = find_json(
        &mut fake,
        r#"{"extension":".md","name":"qa","folder":"Notes"}"#,
        None,
    );
    let mut paths = match_paths(&value);
    paths.sort();
    assert_eq!(
        paths,
        vec!["Notes/Archive/old-qa-plan.MD", "Notes/qa-plan.md"]
    );
    assert_eq!(
        fake.listed(),
        vec!["Notes".to_owned(), "Notes/Archive".to_owned()]
    );
}

#[test]
fn find_skips_the_private_folder_without_a_request() {
    let mut fake = tree();
    let value = find_json(&mut fake, r#"{"extension":"md"}"#, Some("diary/"));
    let paths = match_paths(&value);
    assert!(!paths.iter().any(|p| p.starts_with("Diary/")), "{paths:?}");
    assert!(paths.contains(&"Diary2/qa-plan-copy.md".to_owned()));
    assert_eq!(value.get("complete").as_bool(), Some(true));
    let listed = fake.listed();
    assert!(
        !listed
            .iter()
            .any(|p| p == "Diary" || p.starts_with("Diary/")),
        "{listed:?}"
    );
    assert!(listed.contains(&"Diary2".to_owned()));
    assert!(fake.requests.iter().all(|(m, _)| *m == Method::Propfind));
}

#[test]
fn find_stops_at_the_folder_cap() {
    let names: Vec<String> = (0..MAX_FOLDERS + 5)
        .map(|i| format!("qa-folder-{i:02}"))
        .collect();
    let entries: Vec<(&str, Option<&str>)> = names.iter().map(|n| (n.as_str(), None)).collect();
    let mut fake = Fake::new(&entries);
    let value = find_json(&mut fake, r#"{"name":"x"}"#, None);
    assert_eq!(value.get("complete").as_bool(), Some(false));
    assert_eq!(fake.listed().len(), MAX_FOLDERS);
    assert_eq!(
        value.get("foldersSearched").as_num(),
        Some(MAX_FOLDERS as f64)
    );
}

#[test]
fn find_stops_at_the_depth_cap() {
    let mut paths = Vec::new();
    let mut dir = String::new();
    for level in 1..=MAX_DEPTH + 2 {
        dir = if dir.is_empty() {
            format!("d{level}")
        } else {
            format!("{dir}/d{level}")
        };
        paths.push((dir.clone(), None));
        paths.push((format!("{dir}/qa-note-{level}.md"), Some("x")));
    }
    let entries: Vec<(&str, Option<&str>)> = paths.iter().map(|(p, v)| (p.as_str(), *v)).collect();
    let mut fake = Fake::new(&entries);
    let value = find_json(&mut fake, r#"{"extension":"md"}"#, None);
    assert_eq!(value.get("complete").as_bool(), Some(false));
    // The start folder (depth 0) and depths 1..=MAX_DEPTH are listed, nothing deeper.
    assert_eq!(fake.listed().len(), MAX_DEPTH + 1);
    assert_eq!(match_paths(&value).len(), MAX_DEPTH);
    assert!(fake
        .listed()
        .iter()
        .all(|p| p.matches('/').count() < MAX_DEPTH));
}

#[test]
fn find_stops_at_the_match_cap() {
    let names: Vec<String> = (0..MAX_MATCHES + 5)
        .map(|i| format!("qa-note-{i:02}.md"))
        .collect();
    let entries: Vec<(&str, Option<&str>)> =
        names.iter().map(|n| (n.as_str(), Some("x"))).collect();
    let mut fake = Fake::new(&entries);
    let value = find_json(&mut fake, r#"{"extension":"md"}"#, None);
    assert_eq!(match_paths(&value).len(), MAX_MATCHES);
    assert_eq!(value.get("complete").as_bool(), Some(false));

    let exact: Vec<(&str, Option<&str>)> = entries.iter().take(MAX_MATCHES).copied().collect();
    let mut fake = Fake::new(&exact);
    let value = find_json(&mut fake, r#"{"extension":"md"}"#, None);
    assert_eq!(match_paths(&value).len(), MAX_MATCHES);
    assert_eq!(value.get("complete").as_bool(), Some(true));
}

#[test]
fn find_stops_at_the_deadline() {
    let mut fake = Fake::new(&[
        ("a", None),
        ("b", None),
        ("c", None),
        ("d", None),
        ("e", None),
    ]);
    fake.step = Duration::from_secs(21);
    let value = find_json(&mut fake, r#"{"name":"x"}"#, None);
    assert_eq!(value.get("complete").as_bool(), Some(false));
    // Listings start at 0 s, 21 s and 42 s; at 63 s no new one starts.
    assert_eq!(fake.listed().len(), 3);
}

#[test]
fn find_skips_a_failing_sub_folder() {
    let mut fake = tree();
    fake.failing.insert("Broken".into());
    let value = find_json(&mut fake, r#"{"name":"plan"}"#, None);
    let paths = match_paths(&value);
    assert!(paths.contains(&"Notes/qa-plan.md".to_owned()));
    assert!(!paths.iter().any(|p| p.starts_with("Broken/")));
    assert_eq!(value.get("complete").as_bool(), Some(false));
    assert_eq!(value.get("foldersSearched").as_num(), Some(5.0));
}

#[test]
fn find_fails_when_the_start_folder_fails() {
    let c = connection();
    let mut fake = tree();
    let call = parse(FIND_FILES, r#"{"name":"plan","folder":"Missing"}"#).unwrap();
    let error = run(&call, &c, &mut fake, None).unwrap_err();
    assert_eq!(error.message, "storage returned 404");
    assert_no_leak(&error.message);

    let mut fake = tree();
    fake.failing.insert(String::new());
    let call = parse(FIND_FILES, r#"{"name":"plan"}"#).unwrap();
    let error = run(&call, &c, &mut fake, None).unwrap_err();
    assert_eq!(error.message, "storage returned 500");
    assert_eq!(fake.listed().len(), 1);

    let call = parse(FIND_FILES, r#"{"name":"plan","folder":"../x"}"#).unwrap();
    let error = run(&call, &c, &mut tree(), None).unwrap_err();
    assert_eq!(
        error.message,
        "Use a relative path inside the storage folder."
    );
}

#[test]
fn no_output_or_error_names_the_connection() {
    let c = connection();
    let calls = [
        (LIST_FOLDER, "{}"),
        (LIST_FOLDER, r#"{"path":"Notes"}"#),
        (LIST_FOLDER, r#"{"path":"Missing"}"#),
        (READ_FILE, r#"{"path":"readme-qa.txt"}"#),
        (READ_FILE, r#"{"path":"Missing.md"}"#),
        (READ_FILE, r#"{"path":"Notes/qa.bin"}"#),
        (FIND_FILES, r#"{"name":"qa"}"#),
        (FIND_FILES, r#"{"name":"qa","folder":"Missing"}"#),
    ];
    for (tool, args) in calls {
        let mut fake = tree();
        fake.failing.insert("Broken".into());
        let text = match run(&parse(tool, args).unwrap(), &c, &mut fake, Some("Diary")) {
            Ok(out) => out,
            Err(error) => error.message,
        };
        assert_no_leak(&text);
    }
    assert_no_leak(&manifest_line());
}

#[test]
fn non_dav_connections_are_closed_when_the_diary_folder_matters() {
    let calls = [
        parse(LIST_FOLDER, "{}").unwrap(),
        parse(LIST_FOLDER, r#"{"path":"Notes"}"#).unwrap(),
        parse(READ_FILE, r#"{"path":"Notes/qa-plan.md"}"#).unwrap(),
        parse(FIND_FILES, r#"{"name":"plan"}"#).unwrap(),
    ];
    for kind in [Kind::S3, Kind::Local] {
        let mut c = connection();
        c.kind = kind;
        for call in &calls {
            assert_eq!(
                private_refusal(call, kind, "Diary").as_deref(),
                Some(PRIVATE_REFUSAL),
                "{kind:?} {call:?}"
            );
            let mut fake = tree();
            let error = run(call, &c, &mut fake, Some("Diary")).unwrap_err();
            assert_eq!(error.message, PRIVATE_REFUSAL);
            assert!(fake.requests.is_empty());
        }
    }
    for call in &calls {
        assert_eq!(private_refusal(call, Kind::Nextcloud, "Diary"), None);
    }
}

#[test]
fn find_lists_a_folder_once_under_white_space_names() {
    let mut fake = Fake::new(&[
        ("Notes", None),
        ("Notes/qa-plan.md", Some("x")),
        ("Notes/ ", None),
        (" ", None),
        ("qa-root-plan.md", Some("y")),
    ]);
    let value = find_json(&mut fake, r#"{"name":"plan"}"#, None);
    let mut paths = match_paths(&value);
    paths.sort();
    assert_eq!(paths, vec!["Notes/qa-plan.md", "qa-root-plan.md"]);
    let listed = fake.listed();
    assert_eq!(
        listed.iter().filter(|p| p.as_str() == "Notes").count(),
        1,
        "{listed:?}"
    );
    assert_eq!(
        listed.iter().filter(|p| p.is_empty()).count(),
        1,
        "{listed:?}"
    );
    assert_eq!(value.get("complete").as_bool(), Some(true));
}

/// Run one write call on the synthetic tree; the answer or the error message, checked for leaks.
fn write_run(
    fake: &mut Fake,
    tool: &str,
    args: &str,
    skip: Option<&str>,
) -> Result<String, String> {
    let call = parse(tool, args).unwrap();
    let result = run(&call, &connection(), fake, skip).map_err(|e| e.message);
    match &result {
        Ok(text) | Err(text) => assert_no_leak(text),
    }
    result
}

fn methods(fake: &Fake) -> Vec<Method> {
    fake.requests.iter().map(|(m, _)| *m).collect()
}

#[test]
fn parse_write_shapes() {
    assert_eq!(
        parse(
            WRITE_FILE,
            r#"{"path":"Notes/qa-new.md","content":"","mode":"create"}"#
        )
        .unwrap(),
        Call::Write {
            path: "Notes/qa-new.md".into(),
            content: String::new(),
            mode: Mode::Create
        }
    );
    assert_eq!(
        parse(
            WRITE_FILE,
            r#"{"path":"a.md","content":"x\ny","mode":"replace"}"#
        )
        .unwrap(),
        Call::Write {
            path: "a.md".into(),
            content: "x\ny".into(),
            mode: Mode::Replace
        }
    );
    assert_eq!(
        parse(CREATE_FOLDER, r#"{"path":"Notes/qa-2026"}"#).unwrap(),
        Call::CreateFolder {
            path: "Notes/qa-2026".into()
        }
    );
    assert_eq!(
        parse(DELETE_FILE, r#"{"path":"Notes/qa-old.md"}"#).unwrap(),
        Call::DeleteFile {
            path: "Notes/qa-old.md".into()
        }
    );
    let at_cap = format!(
        "{{\"path\":\"a.md\",\"content\":\"{}\",\"mode\":\"create\"}}",
        "a".repeat(MAX_CONTENT_BYTES)
    );
    assert!(parse(WRITE_FILE, &at_cap).is_ok());
    let too_big = format!(
        "{{\"path\":\"a.md\",\"content\":\"{}\",\"mode\":\"create\"}}",
        "a".repeat(MAX_CONTENT_BYTES + 1)
    );
    let long_path = format!(
        "{{\"path\":\"{}\",\"content\":\"x\",\"mode\":\"create\"}}",
        "a".repeat(501)
    );
    let cases = [
        (WRITE_FILE, too_big.as_str()),
        (WRITE_FILE, long_path.as_str()),
        (WRITE_FILE, r#"{"path":"a.md","mode":"create"}"#),
        (
            WRITE_FILE,
            r#"{"path":"a.md","content":null,"mode":"create"}"#,
        ),
        (WRITE_FILE, r#"{"path":"a.md","content":5,"mode":"create"}"#),
        (WRITE_FILE, r#"{"path":"a.md","content":"x"}"#),
        (
            WRITE_FILE,
            r#"{"path":"a.md","content":"x","mode":"append"}"#,
        ),
        (
            WRITE_FILE,
            r#"{"path":"a.md","content":"x","mode":"Create"}"#,
        ),
        (WRITE_FILE, r#"{"path":"a.md","content":"x","mode":1}"#),
        (WRITE_FILE, r#"{"content":"x","mode":"create"}"#),
        (WRITE_FILE, r#"{"path":"","content":"x","mode":"create"}"#),
        (WRITE_FILE, r#"{"path":".","content":"x","mode":"create"}"#),
        (WRITE_FILE, r#"{"path":"/","content":"x","mode":"create"}"#),
        (
            WRITE_FILE,
            r#"{"path":"a.md","content":"x","mode":"create","etag":"x"}"#,
        ),
        (WRITE_FILE, "[]"),
        (CREATE_FOLDER, "{}"),
        (CREATE_FOLDER, r#"{"path":""}"#),
        (CREATE_FOLDER, r#"{"path":"."}"#),
        (CREATE_FOLDER, r#"{"path":" / "}"#),
        (CREATE_FOLDER, r#"{"path":"a/../.."}"#),
        (CREATE_FOLDER, r#"{"path":7}"#),
        (DELETE_FILE, "{}"),
        (DELETE_FILE, r#"{"path":"."}"#),
        (DELETE_FILE, r#"{"path":"  "}"#),
        (DELETE_FILE, r#"{"path":"a.md","recursive":true}"#),
    ];
    for (tool, args) in cases {
        let error = parse(tool, args)
            .err()
            .unwrap_or_else(|| panic!("{tool} {} was accepted", args.len()));
        assert!(!error.message.is_empty());
        assert!(error.message.len() < 120, "{}", error.message);
    }
}

#[test]
fn write_create_sends_if_none_match_and_refuses_an_existing_file() {
    let mut fake = tree();
    let out = write_run(
        &mut fake,
        WRITE_FILE,
        r#"{"path":"Notes/qa-new.md","content":"hello","mode":"create"}"#,
        Some("Diary"),
    )
    .unwrap();
    assert_eq!(out, r#"{"path":"Notes/qa-new.md","created":true}"#);
    assert_eq!(methods(&fake), vec![Method::Put]);
    assert_eq!(fake.requests[0].1, "Notes/qa-new.md");
    assert_eq!(header(&fake.headers[0], "If-None-Match"), Some("*"));
    assert_eq!(header(&fake.headers[0], "If-Match"), None);
    assert_eq!(
        fake.tree.get("Notes/qa-new.md"),
        Some(&Some("hello".to_owned()))
    );

    // A 412: the file exists; nothing is overwritten and the PUT is not repeated.
    let mut fake = tree();
    let error = write_run(
        &mut fake,
        WRITE_FILE,
        r#"{"path":"Notes/qa-plan.md","content":"overwritten","mode":"create"}"#,
        Some("Diary"),
    )
    .unwrap_err();
    assert_eq!(
        error,
        "\"Notes/qa-plan.md\" already exists; nothing was overwritten."
    );
    assert_eq!(methods(&fake), vec![Method::Put]);
    assert_eq!(
        fake.tree.get("Notes/qa-plan.md"),
        Some(&Some("# QA plan\n".to_owned()))
    );
}

#[test]
fn write_replace_reads_the_version_then_sends_if_match() {
    let replace = r#"{"path":"Notes/qa-plan.md","content":"new text","mode":"replace"}"#;
    let mut fake = tree();
    let out = write_run(&mut fake, WRITE_FILE, replace, Some("Diary")).unwrap();
    assert_eq!(out, r#"{"path":"Notes/qa-plan.md","created":false}"#);
    assert_eq!(methods(&fake), vec![Method::Propfind, Method::Put]);
    assert_eq!(header(&fake.headers[0], "Depth"), Some("0"));
    assert_eq!(fake.requests[0].1, "Notes/qa-plan.md");
    assert_eq!(header(&fake.headers[1], "If-Match"), Some("\"qa-v1\""));
    assert_eq!(header(&fake.headers[1], "If-None-Match"), None);
    assert_eq!(
        fake.tree.get("Notes/qa-plan.md"),
        Some(&Some("new text".to_owned()))
    );

    // A 412: it changed between the version read and the write.
    let mut fake = tree();
    fake.stale = true;
    let error = write_run(&mut fake, WRITE_FILE, replace, Some("Diary")).unwrap_err();
    assert_eq!(
        error,
        "\"Notes/qa-plan.md\" changed in storage meanwhile; nothing was written."
    );
    assert_eq!(methods(&fake), vec![Method::Propfind, Method::Put]);
    assert_eq!(
        fake.tree.get("Notes/qa-plan.md"),
        Some(&Some("# QA plan\n".to_owned()))
    );

    // Missing, a folder, or no version to guard with: refused before any PUT.
    let mut fake = tree();
    let error = write_run(
        &mut fake,
        WRITE_FILE,
        r#"{"path":"Notes/qa-missing.md","content":"x","mode":"replace"}"#,
        Some("Diary"),
    )
    .unwrap_err();
    assert_eq!(
        error,
        "\"Notes/qa-missing.md\" does not exist; use mode \"create\"."
    );
    assert_eq!(methods(&fake), vec![Method::Propfind]);

    let mut fake = tree();
    let error = write_run(
        &mut fake,
        WRITE_FILE,
        r#"{"path":"Notes/Archive","content":"x","mode":"replace"}"#,
        Some("Diary"),
    )
    .unwrap_err();
    assert_eq!(error, "\"Notes/Archive\" is a folder; nothing was written.");
    assert_eq!(methods(&fake), vec![Method::Propfind]);

    let mut fake = tree();
    fake.etag = String::new();
    let error = write_run(&mut fake, WRITE_FILE, replace, Some("Diary")).unwrap_err();
    assert_eq!(
        error,
        "\"Notes/qa-plan.md\" has no version to guard the replace; nothing was written."
    );
    assert_eq!(methods(&fake), vec![Method::Propfind]);
    assert_eq!(header(&fake.headers[0], "Depth"), Some("0"));

    // A storage failure while reading the version passes through without the connection.
    let mut fake = tree();
    fake.failing.insert("Notes/qa-plan.md".into());
    let error = write_run(&mut fake, WRITE_FILE, replace, Some("Diary")).unwrap_err();
    assert_eq!(error, "storage returned 500");
}

#[test]
fn create_folder_answers_whether_it_existed() {
    let mut fake = tree();
    let out = write_run(
        &mut fake,
        CREATE_FOLDER,
        r#"{"path":"Notes/qa-2026"}"#,
        Some("Diary"),
    )
    .unwrap();
    assert_eq!(out, r#"{"path":"Notes/qa-2026","existed":false}"#);
    assert_eq!(methods(&fake), vec![Method::Mkcol]);
    assert_eq!(fake.tree.get("Notes/qa-2026"), Some(&None));

    let mut fake = tree();
    let out = write_run(
        &mut fake,
        CREATE_FOLDER,
        r#"{"path":"Notes/"}"#,
        Some("Diary"),
    )
    .unwrap();
    assert_eq!(out, r#"{"path":"Notes","existed":true}"#);

    let mut fake = tree();
    let error = write_run(
        &mut fake,
        CREATE_FOLDER,
        r#"{"path":"qa-missing/child"}"#,
        Some("Diary"),
    )
    .unwrap_err();
    assert_eq!(error, "could not create \"qa-missing/child\" (409)");
}

#[test]
fn delete_file_reads_the_version_first_and_refuses_folders() {
    let mut fake = tree();
    let out = write_run(
        &mut fake,
        DELETE_FILE,
        r#"{"path":"Notes/qa-list.txt"}"#,
        Some("Diary"),
    )
    .unwrap();
    assert_eq!(out, r#"{"path":"Notes/qa-list.txt","deleted":true}"#);
    assert_eq!(methods(&fake), vec![Method::Propfind, Method::Delete]);
    assert_eq!(header(&fake.headers[0], "Depth"), Some("0"));
    assert_eq!(header(&fake.headers[1], "If-Match"), Some("\"qa-v1\""));
    assert!(!fake.tree.contains_key("Notes/qa-list.txt"));

    let mut fake = tree();
    let out = write_run(
        &mut fake,
        DELETE_FILE,
        r#"{"path":"Notes/qa-gone.txt"}"#,
        Some("Diary"),
    )
    .unwrap();
    assert_eq!(out, r#"{"path":"Notes/qa-gone.txt","deleted":false}"#);
    assert_eq!(methods(&fake), vec![Method::Propfind]);

    let mut fake = tree();
    let error =
        write_run(&mut fake, DELETE_FILE, r#"{"path":"Notes"}"#, Some("Diary")).unwrap_err();
    assert!(error.contains("it is a folder"), "{error}");
    assert_eq!(methods(&fake), vec![Method::Propfind]);
    assert_eq!(fake.tree.get("Notes"), Some(&None));
}

#[test]
fn writes_are_refused_inside_the_diary_folder() {
    let calls = [
        (
            WRITE_FILE,
            r#"{"path":"Diary/qa.md","content":"x","mode":"create"}"#,
        ),
        (
            WRITE_FILE,
            r#"{"path":"diary/2026-11.md","content":"x","mode":"replace"}"#,
        ),
        (
            WRITE_FILE,
            r#"{"path":"DIARY/sub/qa.md","content":"x","mode":"create"}"#,
        ),
        (CREATE_FOLDER, r#"{"path":"Diary"}"#),
        (CREATE_FOLDER, r#"{"path":"Diary/"}"#),
        (CREATE_FOLDER, r#"{"path":"dIaRy/qa-sub"}"#),
        (DELETE_FILE, r#"{"path":"Diary/2026-11.md"}"#),
        (DELETE_FILE, r#"{"path":"diary\\2026-11.md"}"#),
        (DELETE_FILE, r#"{"path":" Diary/2026-11.md "}"#),
    ];
    for (tool, args) in calls {
        let call = parse(tool, args).unwrap();
        for root in ["Diary", "/Diary/", "diary", "Diary/"] {
            assert_eq!(
                private_refusal(&call, Kind::WebDav, root).as_deref(),
                Some(PRIVATE_REFUSAL),
                "{args} {root}"
            );
            let mut fake = tree();
            let error = run(&call, &connection(), &mut fake, Some(root)).unwrap_err();
            assert_eq!(error.message, PRIVATE_REFUSAL, "{args} {root}");
            assert!(fake.requests.is_empty());
            assert_eq!(
                fake.tree.get("Diary/2026-11.md"),
                Some(&Some("synthetic private entry".to_owned()))
            );
        }
        // A skip that normalises to nothing refuses every write.
        let mut fake = tree();
        let error = run(&call, &connection(), &mut fake, Some("/")).unwrap_err();
        assert_eq!(error.message, PRIVATE_REFUSAL);
        assert!(fake.requests.is_empty());
    }
    // A sibling with the same prefix is not the Diary folder.
    let mut fake = tree();
    let out = write_run(
        &mut fake,
        WRITE_FILE,
        r#"{"path":"Diary2/qa-new.md","content":"x","mode":"create"}"#,
        Some("Diary"),
    )
    .unwrap();
    assert_eq!(out, r#"{"path":"Diary2/qa-new.md","created":true}"#);
}

#[test]
fn writes_are_refused_on_s3_and_local_connections() {
    let calls = [
        parse(
            WRITE_FILE,
            r#"{"path":"Notes/qa.md","content":"x","mode":"create"}"#,
        )
        .unwrap(),
        parse(
            WRITE_FILE,
            r#"{"path":"Notes/qa-plan.md","content":"x","mode":"replace"}"#,
        )
        .unwrap(),
        parse(CREATE_FOLDER, r#"{"path":"Notes/qa-2026"}"#).unwrap(),
        parse(DELETE_FILE, r#"{"path":"Notes/qa-plan.md"}"#).unwrap(),
    ];
    for kind in [Kind::S3, Kind::Local] {
        let mut c = connection();
        c.kind = kind;
        for call in &calls {
            for skip in [None, Some("Diary")] {
                let mut fake = tree();
                let error = run(call, &c, &mut fake, skip).unwrap_err();
                assert_eq!(
                    error.message, WRITE_UNSUPPORTED,
                    "{kind:?} {call:?} {skip:?}"
                );
                assert!(fake.requests.is_empty());
            }
            assert!(private_refusal(call, kind, "Diary").is_some());
        }
    }
}
