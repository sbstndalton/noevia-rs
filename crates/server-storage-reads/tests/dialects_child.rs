//! The storage client against two WebDAV server dialects (track S1), through the real owned child
//! (`private-request-child`, the same body as `noevia-server --storage-request-child`): the user's
//! home at `/remote.php/dav/files/<user>/`, and a personal space at `/dav/spaces/<id>/` (the id's `$`
//! raw or percent-encoded). Every address gives the same answers: listing, text read, folder create,
//! the conditional writes, the version read, discard, delete and the empty-folder sweep. The space
//! server ignores `If-None-Match: *` on PUT and `If-Match` on DELETE, so on a space address the client
//! checks first (`writes::checks_first`); its legacy files address does not get that check, and the
//! last test pins what is lost there. Synthetic server and data only (support/dav_dialects.rs).
//! Linux only: the child sets its limits before READY.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use server_storage_reads::{
    protocol::{execute, Connection, Kind, Operation},
    transport::ChildTransport,
    writes::{self, Condition, Version},
};
use std::{path::PathBuf, sync::atomic::AtomicBool};
use zeroize::Zeroizing;
#[path = "support/dav_dialects.rs"]
mod dav_dialects;
use dav_dialects::{server, Dialect, FILES_HOME, SPACE_ID, TOKEN, USER};

fn child() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_private-request-child"))
}

fn conn(port: u16, home: &str, secret: &str) -> Connection {
    Connection {
        kind: Kind::WebDav,
        base_url: format!("http://127.0.0.1:{port}{home}"),
        username: USER.into(),
        secret: Zeroizing::new(secret.as_bytes().to_vec()),
        corpus_root: "Diary".into(),
        bucket: String::new(),
        region: String::new(),
    }
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// The addresses that keep every guarantee: (dialect, home path as configured, what it is).
fn addresses() -> Vec<(Dialect, String, &'static str)> {
    vec![
        (Dialect::Files, FILES_HOME.to_owned(), "files home"),
        (
            Dialect::Spaces,
            format!("/dav/spaces/{SPACE_ID}/"),
            "space, raw id",
        ),
        (
            Dialect::Spaces,
            format!("/dav/spaces/{}", SPACE_ID.replace('$', "%24")),
            "space, encoded id, no trailing slash",
        ),
    ]
}

const PROJECT: &str = "noevia projects/QA proj";
const TEXT: &str = "noevia projects/QA proj/Text";
/// A name that needs percent-encoding and XML escaping (`&`, `'`, `#`, `?`, a space, non-ASCII).
const ODD: &str = "noevia projects/QA proj/Text/qa é a&b's #1?.md";
const NOTE: &str = "noevia projects/QA proj/Text/qa-note.md";

/// One full walk; returns what the caller sees at each step (dialect-independent by contract).
fn walk(c: &Connection, t: &mut ChildTransport<'_>) -> Vec<String> {
    let mut seen = Vec::new();
    let mut log = |step: &str, v: String| seen.push(format!("{step}: {v}"));

    // Folders: created, then 405 = it exists; a missing parent (409) is core's 400.
    for f in ["noevia projects", "noevia projects", PROJECT, TEXT] {
        let r = writes::create_folder(c, f, t).unwrap();
        log("mkcol", format!("{} existed={}", r.path, r.existed));
    }
    let e = writes::create_folder(c, "qa-nowhere/x", t).unwrap_err();
    log("mkcol orphan", format!("{} {:?}", e.status, e.code));

    // Create-only write of an awkward name; the answer's ETag is the one a version read reports.
    let w = writes::write_file(c, ODD, b"QA one", &Condition::IfNoneMatch, t).unwrap();
    assert!(w.created);
    assert!(w.etag.starts_with('"') && w.etag.len() > 2, "{:?}", w.etag);
    log("put create", format!("created={}", w.created));
    let v = writes::file_version(c, ODD, t).unwrap();
    assert_eq!(
        v,
        Version::Present(w.etag.clone()),
        "version = written ETag"
    );
    let e = writes::write_file(c, ODD, b"QA other", &Condition::IfNoneMatch, t).unwrap_err();
    log("put create again", format!("{} {:?}", e.status, e.code));

    // A conditional replace with the ETag the version read gave (204: not created).
    let Version::Present(seen_etag) = v else {
        panic!("a version")
    };
    let w2 =
        writes::write_file(c, ODD, b"QA two", &Condition::IfMatch(seen_etag.clone()), t).unwrap();
    assert!(!w2.created);
    assert_ne!(w2.etag, w.etag);
    log("put replace", format!("created={}", w2.created));
    let e = writes::write_file(
        c,
        ODD,
        b"QA stale",
        &Condition::IfMatch(seen_etag.clone()),
        t,
    )
    .unwrap_err();
    log("put stale", format!("{} {:?}", e.status, e.code));

    // Listing and text read.
    writes::write_file(c, NOTE, b"QA synthetic note", &Condition::None, t).unwrap();
    let listing = execute(c, &units(TEXT), Operation::List, t).unwrap();
    log("list", listing);
    let read = execute(c, &units(NOTE), Operation::Read, t).unwrap();
    log("read", read);
    let missing = execute(
        c,
        &units("noevia projects/QA proj/qa-missing.md"),
        Operation::Read,
        t,
    )
    .unwrap_err();
    log(
        "read missing",
        format!("{} {}", missing.status, missing.message),
    );

    // discardWritten: a stale ETag keeps the file, the current one removes it.
    log(
        "discard stale",
        format!("{:?}", writes::discard_written(c, ODD, &seen_etag, t)),
    );
    log(
        "discard",
        format!("{:?}", writes::discard_written(c, ODD, &w2.etag, t)),
    );
    log(
        "version gone",
        format!("{:?}", writes::file_version(c, ODD, t).unwrap()),
    );

    // deleteFile: a folder is refused, a file goes (with the If-Match of its version), then is missing.
    let e = writes::delete_file(c, TEXT, t).unwrap_err();
    log("delete folder", format!("{} {:?}", e.status, e.code));
    log(
        "delete",
        format!("{:?}", writes::delete_file(c, NOTE, t).unwrap()),
    );
    log(
        "delete again",
        format!("{:?}", writes::delete_file(c, NOTE, t).unwrap()),
    );

    // The sweep: the collection's ETag from PROPFIND is what DELETE's If-Match carries.
    writes::write_file(c, NOTE, b"QA keep", &Condition::None, t).unwrap();
    log(
        "sweep busy",
        format!("{:?}", writes::remove_empty_folder(c, TEXT, t).unwrap()),
    );
    writes::delete_file(c, NOTE, t).unwrap();
    for f in [TEXT, PROJECT, PROJECT] {
        log(
            "sweep",
            format!("{f} {:?}", writes::remove_empty_folder(c, f, t).unwrap()),
        );
    }
    seen
}

#[test]
fn every_dialect_and_address_gives_the_same_answers() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipped: the owned child runs only on Linux");
        return;
    }
    let cancelled = AtomicBool::new(false);
    let mut t = ChildTransport::new(child(), &[], &cancelled);
    let mut first: Option<Vec<String>> = None;
    for (dialect, home, what) in addresses() {
        let (port, dav) = server(dialect);
        let c = conn(port, &home, TOKEN);
        let seen = walk(&c, &mut t);
        assert!(
            dav.lock().unwrap().dirs.contains("noevia projects"),
            "{what}: the sweep keeps the root folder"
        );
        match &first {
            None => first = Some(seen),
            Some(want) => assert_eq!(&seen, want, "{what} ({dialect:?}) differs"),
        }
    }
    let want = first.unwrap();
    // The answers themselves (the same for every address).
    let line =
        |prefix: &str| -> Vec<&String> { want.iter().filter(|l| l.starts_with(prefix)).collect() };
    assert_eq!(
        line("mkcol:"),
        [
            "mkcol: noevia projects existed=false",
            "mkcol: noevia projects existed=true",
            "mkcol: noevia projects/QA proj existed=false",
            "mkcol: noevia projects/QA proj/Text existed=false",
        ]
    );
    assert_eq!(line("mkcol orphan"), ["mkcol orphan: 400 None"]);
    assert_eq!(
        line("put create again"),
        ["put create again: 409 Some(\"changed\")"]
    );
    assert_eq!(line("put stale"), ["put stale: 409 Some(\"changed\")"]);
    assert_eq!(
        line("list"),
        [concat!(
            "list: [",
            r#"{"name":"qa é a&b's #1?.md","path":"noevia projects/QA proj/Text/qa é a&b's #1?.md","isDir":false,"size":6,"ext":".md"},"#,
            r#"{"name":"qa-note.md","path":"noevia projects/QA proj/Text/qa-note.md","isDir":false,"size":17,"ext":".md"}"#,
            "]"
        )]
    );
    assert_eq!(
        line("read:"),
        [r#"read: {"name":"qa-note.md","content":"QA synthetic note","truncated":false}"#]
    );
    assert_eq!(
        line("read missing"),
        ["read missing: 502 storage returned 404"]
    );
    assert_eq!(
        line("discard stale"),
        ["discard stale: (false, \"changed\")"]
    );
    assert_eq!(line("discard:"), ["discard: (true, \"\")"]);
    assert_eq!(line("version gone"), ["version gone: Missing"]);
    assert_eq!(
        line("delete folder"),
        ["delete folder: 400 Some(\"folder\")"]
    );
    assert_eq!(line("delete:"), ["delete: false"]);
    assert_eq!(line("delete again"), ["delete again: true"]);
    assert_eq!(line("sweep busy"), ["sweep busy: (false, \"not-empty\")"]);
    assert_eq!(
        line("sweep:"),
        [
            "sweep: noevia projects/QA proj/Text (true, \"\")",
            "sweep: noevia projects/QA proj (true, \"\")",
            "sweep: noevia projects/QA proj (false, \"missing\")",
        ]
    );
}

#[test]
fn a_refused_token_is_a_refused_login_on_every_dialect() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipped: the owned child runs only on Linux");
        return;
    }
    let cancelled = AtomicBool::new(false);
    let mut t = ChildTransport::new(child(), &[], &cancelled);
    let mut all = addresses();
    all.push((
        Dialect::Spaces,
        FILES_HOME.to_owned(),
        "space server, legacy files home",
    ));
    for (dialect, home, what) in all {
        let (port, _dav) = server(dialect);
        let bad = conn(port, &home, "qa-wrong-token");
        let e = writes::write_file(&bad, NOTE, b"x", &Condition::None, &mut t).unwrap_err();
        assert!(e.refused_login(), "{what}");
        assert!(
            !e.message.contains("qa-wrong-token"),
            "{what}: no credential in the answer"
        );
        let e = writes::create_folder(&bad, "qa-x", &mut t).unwrap_err();
        assert_eq!(e.upstream, Some(401), "{what}");
        let e = execute(&bad, &units("noevia projects"), Operation::List, &mut t).unwrap_err();
        assert_eq!(
            (e.status, e.message.as_str()),
            (502, "storage returned 401"),
            "{what}"
        );
    }
}

#[test]
fn only_a_space_address_checks_first_and_the_legacy_address_loses_two_guards() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipped: the owned child runs only on Linux");
        return;
    }
    let cancelled = AtomicBool::new(false);
    let mut t = ChildTransport::new(child(), &[], &cancelled);
    let (port, dav) = server(Dialect::Spaces);
    let space = conn(port, &format!("/dav/spaces/{SPACE_ID}"), TOKEN);
    let legacy = conn(port, FILES_HOME, TOKEN);
    assert!(writes::checks_first(&space));
    assert!(!writes::checks_first(&legacy));
    assert!(!writes::checks_first(&conn(
        port,
        "/dav/files/qa-user/",
        TOKEN
    )));
    let s3 = Connection {
        kind: Kind::S3,
        ..conn(port, &format!("/dav/spaces/{SPACE_ID}"), TOKEN)
    };
    assert!(!writes::checks_first(&s3));

    writes::create_folder(&space, "qa", &mut t).unwrap();
    let first = writes::write_file(
        &space,
        "qa/a.md",
        b"QA one",
        &Condition::IfNoneMatch,
        &mut t,
    )
    .unwrap();
    assert!(first.created);

    // The space address: a create-only write over an existing file reads its state first and
    // refuses (no PUT is sent); a stale discard is refused, and the file stays.
    dav.lock().unwrap().seen.clear();
    let e = writes::write_file(
        &space,
        "qa/a.md",
        b"QA two",
        &Condition::IfNoneMatch,
        &mut t,
    )
    .unwrap_err();
    assert_eq!((e.status, e.code), (409, Some("changed")));
    let methods: Vec<String> = dav
        .lock()
        .unwrap()
        .seen
        .iter()
        .map(|(m, _)| m.clone())
        .collect();
    assert_eq!(methods, ["PROPFIND"]);
    assert_eq!(
        writes::discard_written(&space, "qa/a.md", "\"qa-stale\"", &mut t),
        (false, "changed")
    );
    assert_eq!(dav.lock().unwrap().files["qa/a.md"].0, b"QA one");
    // A folder at the path counts as existing, too.
    let e = writes::write_file(&space, "qa", b"x", &Condition::IfNoneMatch, &mut t).unwrap_err();
    assert_eq!(e.code, Some("changed"));

    // The legacy files address of the same server sends today's single request, which that server
    // does not hold to: the create-only write overwrites and the stale discard deletes.
    let over = writes::write_file(
        &legacy,
        "qa/a.md",
        b"QA two",
        &Condition::IfNoneMatch,
        &mut t,
    )
    .unwrap();
    assert!(!over.created, "the server answered 204: it overwrote");
    assert_eq!(dav.lock().unwrap().files["qa/a.md"].0, b"QA two");
    assert_eq!(
        writes::discard_written(&legacy, "qa/a.md", "\"qa-stale\"", &mut t),
        (true, "")
    );
    assert!(!dav.lock().unwrap().files.contains_key("qa/a.md"));
}
