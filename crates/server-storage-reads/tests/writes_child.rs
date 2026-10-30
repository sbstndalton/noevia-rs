//! The storage writes through the real owned child (`private-request-child`, the same body as
//! `noevia-server --storage-request-child`) against a synthetic, stateful WebDAV server
//! (sbstndalton/noevia#2191 round 3). Linux only: the child sets its limits before READY.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use server_storage_reads::{
    protocol::{Connection, Kind},
    transport::ChildTransport,
    writes::{self, Condition, Version},
};
use std::{net::TcpListener, path::PathBuf, sync::atomic::AtomicBool};
use zeroize::Zeroizing;
#[path = "support/dav_lab.rs"]
mod dav_lab;
use dav_lab::server;

fn conn(port: u16, secret: &str) -> Connection {
    Connection {
        kind: Kind::WebDav,
        base_url: format!("http://127.0.0.1:{port}/dav/"),
        username: "u".into(),
        secret: Zeroizing::new(secret.as_bytes().to_vec()),
        corpus_root: "Diary".into(),
        bucket: String::new(),
        region: String::new(),
    }
}

fn child() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_private-request-child"))
}

#[test]
fn writes_round_trip_through_the_owned_child() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipped: the owned child runs only on Linux");
        return;
    }
    let (port, dav) = server();
    let c = conn(port, "pw");
    let cancelled = AtomicBool::new(false);
    let mut t = ChildTransport::new(child(), &[], &cancelled);

    assert!(
        !writes::create_folder(&c, "noevia projects", &mut t)
            .unwrap()
            .existed
    );
    assert!(
        writes::create_folder(&c, "noevia projects", &mut t)
            .unwrap()
            .existed
    );
    assert!(
        !writes::create_folder(&c, "noevia projects/proj-1", &mut t)
            .unwrap()
            .existed
    );
    writes::create_folder(&c, "noevia projects/proj-1/Text", &mut t).unwrap();
    // A parent that does not exist: MKCOL 409 is core's 400.
    assert_eq!(
        writes::create_folder(&c, "nowhere/x", &mut t)
            .unwrap_err()
            .status,
        400
    );

    // A 3 MiB body, a name needing encoding, create-only.
    let big: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let name = "noevia projects/proj-1/Text/é #1?.md";
    let w = writes::write_file(&c, name, &big, &Condition::IfNoneMatch, &mut t).unwrap();
    assert!(w.created);
    assert_eq!(w.etag, "\"v1\"");
    assert_eq!(
        dav.lock()
            .unwrap()
            .files
            .get("noevia projects/proj-1/Text/é #1?.md")
            .unwrap()
            .0,
        big
    );
    // Create-only again: 412 is core's 409 changed, nothing overwritten.
    let e = writes::write_file(&c, name, b"other", &Condition::IfNoneMatch, &mut t).unwrap_err();
    assert_eq!((e.status, e.code), (409, Some("changed")));
    assert_eq!(
        writes::file_version(&c, name, &mut t).unwrap(),
        Version::Present("\"v1\"".into())
    );
    // A conditional replace with the current ETag lands (204: not `created`).
    let w = writes::write_file(
        &c,
        name,
        b"v2",
        &Condition::IfMatch("\"v1\"".into()),
        &mut t,
    )
    .unwrap();
    assert!(!w.created);
    // discardWritten with a stale ETag keeps the file; with the current one removes it.
    assert_eq!(
        writes::discard_written(&c, name, "\"v1\"", &mut t),
        (false, "changed")
    );
    assert_eq!(
        writes::discard_written(&c, name, &w.etag, &mut t),
        (true, "")
    );
    assert_eq!(
        writes::file_version(&c, name, &mut t).unwrap(),
        Version::Missing
    );

    // deleteFile refuses a folder and removes a file conditionally.
    writes::write_file(
        &c,
        "noevia projects/proj-1/Text/a.md",
        b"a",
        &Condition::None,
        &mut t,
    )
    .unwrap();
    let e = writes::delete_file(&c, "noevia projects/proj-1/Text", &mut t).unwrap_err();
    assert_eq!((e.status, e.code), (400, Some("folder")));
    assert!(!writes::delete_file(&c, "noevia projects/proj-1/Text/a.md", &mut t).unwrap());
    assert!(writes::delete_file(&c, "noevia projects/proj-1/Text/a.md", &mut t).unwrap());

    // The sweep: a non-empty folder stays, an empty one goes.
    assert_eq!(
        writes::remove_empty_folder(&c, "noevia projects/proj-1", &mut t).unwrap(),
        (false, "not-empty")
    );
    assert_eq!(
        writes::remove_empty_folder(&c, "noevia projects/proj-1/Text", &mut t).unwrap(),
        (true, "")
    );
    assert_eq!(
        writes::remove_empty_folder(&c, "noevia projects/proj-1", &mut t).unwrap(),
        (true, "")
    );
    assert!(dav.lock().unwrap().dirs.contains("noevia projects"));
}

#[test]
fn refusals_auth_redirects_and_traversal_never_reach_or_follow() {
    if !cfg!(target_os = "linux") {
        eprintln!("skipped: the owned child runs only on Linux");
        return;
    }
    let (port, dav) = server();
    let cancelled = AtomicBool::new(false);
    let mut t = ChildTransport::new(child(), &[], &cancelled);
    // A refused login: the storage 401 is kept as `upstream`, core's refusedLogin.
    let bad = conn(port, "wrong");
    let e = writes::write_file(&bad, "a.md", b"x", &Condition::None, &mut t).unwrap_err();
    assert!(e.refused_login());
    assert_eq!(e.status, 502);
    assert!(!e.message.contains("wrong"), "no credential in the answer");
    let e = writes::create_folder(&bad, "x", &mut t).unwrap_err();
    assert_eq!(e.upstream, Some(401));

    let c = conn(port, "pw");
    // A redirect is refused, never followed (one retry, as core's withRetry), and a
    // create-only write reports that it may or may not have landed.
    dav.lock().unwrap().seen.clear();
    let e = writes::write_file(&c, "moved.md", b"x", &Condition::None, &mut t).unwrap_err();
    assert_eq!((e.status, e.code), (502, Some("unreachable")));
    let e = writes::write_file(&c, "moved.md", b"x", &Condition::IfNoneMatch, &mut t).unwrap_err();
    assert_eq!(e.code, Some("unknown"));
    let seen = dav.lock().unwrap().seen.clone();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen.iter().all(|(_, target)| target == "/dav/moved.md"));

    // Traversal and absolute paths: refused before any request.
    dav.lock().unwrap().seen.clear();
    for raw in [
        "../etc/passwd",
        "/abs",
        "a/../../b",
        "a\\..\\b",
        "a/./b",
        "",
    ] {
        assert_eq!(
            writes::write_file(&c, raw, b"x", &Condition::None, &mut t)
                .unwrap_err()
                .status,
            400
        );
        assert_eq!(
            writes::create_folder(&c, raw, &mut t).unwrap_err().status,
            400
        );
        assert_eq!(
            writes::delete_file(&c, raw, &mut t).unwrap_err().status,
            400
        );
    }
    // An encoded segment stays one segment: `%2F` cannot climb.
    writes::write_file(&c, "a%2F..%2Fb.md", b"x", &Condition::None, &mut t).unwrap();
    assert!(dav.lock().unwrap().files.contains_key("a%2F..%2Fb.md"));
    assert_eq!(dav.lock().unwrap().seen.len(), 1);

    // Oversize: refused before the child is started.
    dav.lock().unwrap().seen.clear();
    let over = vec![0u8; server_storage_reads::WRITE_BYTES + 1];
    assert_eq!(
        writes::write_file(&c, "big.bin", &over, &Condition::None, &mut t)
            .unwrap_err()
            .status,
        413
    );
    assert!(dav.lock().unwrap().seen.is_empty());

    // Nothing listens: unreachable after one retry.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = closed.local_addr().unwrap().port();
    drop(closed);
    let e = writes::create_folder(&conn(dead_port, "pw"), "x", &mut t).unwrap_err();
    assert_eq!((e.status, e.code), (502, Some("unreachable")));
}
