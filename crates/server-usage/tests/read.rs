#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use serde_json::json;
use server_usage::{
    read::{own, AggregateReader},
    summary::{summarize, Clock},
    Fault, FILE_CAP,
};
use std::fs;
use std::path::Path;
const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
fn put(root: &Path, user: &str, data: &[u8]) {
    let p = root.join("users").join(user);
    fs::create_dir_all(&p).unwrap();
    fs::write(p.join("usage.json"), data).unwrap();
}
#[test]
fn tenant_missing_malformed_oversize_and_cache() {
    let d = tempfile::tempdir().unwrap();
    put(
        d.path(),
        A,
        br#"{"days":{"2026-10-10":{"input":1,"replies":0.5}}}"#,
    );
    put(d.path(), B, b"bad");
    let c = Clock::new("UTC", "en-US").unwrap();
    assert_eq!(
        summarize(&own(d.path(), A).unwrap(), &c, 1791633600000, 1).unwrap()["allTime"]["input"],
        json!(1.0)
    );
    assert_eq!(
        summarize(&own(d.path(), B).unwrap(), &c, 1791633600000, 1).unwrap()["allTime"]["input"],
        json!(0.0)
    );
    assert!(matches!(
        own(d.path(), "../escape"),
        Err(Fault::InvalidUser)
    ));
    let r = AggregateReader::default();
    let first = r.get(d.path(), vec![A.into(), B.into()], 1000).unwrap();
    assert_eq!(first.accounts, 2);
    assert_eq!(first.unreadable, 1);
    assert_eq!(first.checked_at, 1000);
    put(d.path(), A, b"{}");
    assert_eq!(
        r.get(d.path(), vec![A.into(), B.into()], 30999)
            .unwrap()
            .checked_at,
        1000
    );
    let fresh = r.get(d.path(), vec![A.into(), B.into()], 31000).unwrap();
    assert_eq!(fresh.unreadable, 2);
    assert_eq!(fresh.checked_at, 31000);
    assert_eq!(
        r.get(d.path(), vec![B.into()], 31001).unwrap().checked_at,
        31001
    );
    fs::remove_file(d.path().join("users").join(B).join("usage.json")).unwrap();
    assert_eq!(
        r.get(d.path(), vec![B.into()], 61001).unwrap().unreadable,
        0
    );
    put(d.path(), B, &vec![b' '; FILE_CAP as usize + 1]);
    assert_eq!(
        r.get(d.path(), vec![B.into()], 91001).unwrap().unreadable,
        1
    );
    assert!(matches!(
        r.get(d.path(), vec![A.into(); 1001], 100000),
        Err(Fault::Limit)
    ));
}
#[cfg(unix)]
#[test]
fn symlink_file_and_user_cannot_read_other_tenant() {
    use std::os::unix::fs::symlink;
    let d = tempfile::tempdir().unwrap();
    put(d.path(), A, br#"{"days":{"2026-10-10":{"input":99}}}"#);
    fs::create_dir(d.path().join("users").join(B)).unwrap();
    symlink(
        d.path().join("users").join(A).join("usage.json"),
        d.path().join("users").join(B).join("usage.json"),
    )
    .unwrap();
    let r = AggregateReader::default();
    assert_eq!(r.get(d.path(), vec![B.into()], 0).unwrap().unreadable, 1);
    fs::remove_file(d.path().join("users").join(B).join("usage.json")).unwrap();
    fs::remove_dir(d.path().join("users").join(B)).unwrap();
    symlink(
        d.path().join("users").join(A),
        d.path().join("users").join(B),
    )
    .unwrap();
    assert_eq!(
        r.get(d.path(), vec![B.into()], 30000).unwrap().unreadable,
        1
    );
}
#[test]
fn total_limit_includes_malformed_input() {
    let d = tempfile::tempdir().unwrap();
    let mut users = Vec::new();
    for i in 0..17 {
        let id = format!("{i:08x}-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        put(d.path(), &id, &vec![b'!'; FILE_CAP as usize]);
        users.push(id);
    }
    assert!(matches!(
        AggregateReader::default().get(d.path(), users, 0),
        Err(Fault::Limit)
    ));
}
