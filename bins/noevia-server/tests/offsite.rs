//! The off-site backup (sbstndalton/noevia#2190 round 7) through the real front: the seven admin
//! routes, the engine over a temporary data directory (a real SQLite database included), the copy to
//! a synthetic Google Drive and the nightly schedule. The destination is a temporary folder; Google is
//! a synthetic server on 127.0.0.1; nothing here contacts a real service, the real backup or the NAS.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
#[path = "gdrive_support/mod.rs"]
mod support;
use support::*;

use noevia_server::offsite_engine::{Engine, Pace};
use noevia_server::offsite_store::{DirStore, Store};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ROOT: &str = "/api/admin/offsite-backup";

struct World {
    data: tempfile::TempDir,
    outside: tempfile::TempDir,
    dest: PathBuf,
    key: PathBuf,
    up: Upstream,
    g: Google,
    f: Front,
}

async fn world(extra: &[(&str, String)]) -> World {
    let data = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let dest = outside.path().join("backups");
    std::fs::create_dir_all(&dest).unwrap();
    let key = outside.path().join("backup.key");
    std::fs::write(&key, format!("{BACKUP_KEY_HEX}\n")).unwrap();
    seed(data.path());
    std::fs::write(data.path().join("notes.txt"), "synthetic notes").unwrap();
    std::fs::create_dir_all(data.path().join("users/a")).unwrap();
    std::fs::write(
        data.path().join("users/a/projects.json"),
        r#"{"projects":[]}"#,
    )
    .unwrap();
    let up = upstream().await;
    let g = install_oauth(&up, "admin@example.test", Duration::ZERO);
    let mut env = vec![
        ("OFFSITE_BACKUP_DIR", dest.display().to_string()),
        ("OFFSITE_BACKUP_KEY_FILE", key.display().to_string()),
        ("NOEVIA_FEATURE_OFFSITE_BACKUP", "1".to_string()),
    ];
    env.extend(extra.iter().map(|(k, v)| (*k, v.clone())));
    let f = front(data.path(), &up.base(), &env).await;
    World {
        data,
        outside,
        dest,
        key,
        up,
        g,
        f,
    }
}

fn files_under(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let next = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if e.file_type().unwrap().is_dir() {
                walk(&e.path(), &next, out);
            } else {
                out.push(next);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out.sort();
    out
}

// ---------------------------------------------------------------------------------------------
// A small synthetic Drive.

#[derive(Clone, Default)]
struct Dfile {
    id: String,
    name: String,
    folder: bool,
    parent: Option<String>,
    body: Vec<u8>,
    store_id: Option<String>,
}

#[derive(Clone, Default)]
struct DriveModel {
    files: Arc<Mutex<Vec<Dfile>>>,
    next: Arc<std::sync::atomic::AtomicUsize>,
    authorised: Arc<Mutex<Vec<String>>>,
}

impl DriveModel {
    fn list(&self) -> Vec<Dfile> {
        self.files.lock().unwrap().clone()
    }
    fn children(&self, folder: &str) -> Vec<Dfile> {
        self.list()
            .into_iter()
            .filter(|f| f.parent.as_deref() == Some(folder))
            .collect()
    }
    fn add(&self, f: Dfile) {
        self.files.lock().unwrap().push(f);
    }
    fn handle(&self, r: &Seen) -> (Duration, Vec<u8>) {
        if let Some(h) = r.headers.lines().find(|l| l.starts_with("authorization:")) {
            self.authorised.lock().unwrap().push(h.to_string());
        }
        let url = url::Url::parse(&format!("http://x{}", r.path)).unwrap();
        let path = url.path().to_string();
        let q: std::collections::HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let mut files = self.files.lock().unwrap();
        let file_json = |f: &Dfile| {
            let mut v =
                serde_json::json!({"id": f.id, "name": f.name, "size": f.body.len().to_string()});
            if let Some(s) = &f.store_id {
                v["appProperties"] = serde_json::json!({"noeviaStoreId": s});
            }
            v
        };
        match (r.method.as_str(), path.as_str()) {
            ("GET", "/drive/v3/files") => {
                let query = q.get("q").cloned().unwrap_or_default();
                if query.contains("mimeType='application/vnd.google-apps.folder'") {
                    let list: Vec<_> = files
                        .iter()
                        .filter(|f| f.folder && f.name.contains("noevia-offsite"))
                        .map(file_json)
                        .collect();
                    return reply("200 OK", &serde_json::json!({"files": list}).to_string());
                }
                let parent = query.split('\'').nth(1).unwrap_or("").to_string();
                let list: Vec<_> = files
                    .iter()
                    .filter(|f| f.parent.as_deref() == Some(&parent))
                    .map(file_json)
                    .collect();
                reply("200 OK", &serde_json::json!({"files": list}).to_string())
            }
            ("GET", p) if p.starts_with("/drive/v3/files/") => {
                let id = &p["/drive/v3/files/".len()..];
                match files.iter().find(|f| f.id == id) {
                    None => reply("404 Not Found", "{}"),
                    Some(f) if q.get("alt").map(String::as_str) == Some("media") => {
                        bytes_reply(&f.body)
                    }
                    Some(f) => reply(
                        "200 OK",
                        &serde_json::json!({"id": f.id, "name": f.name, "trashed": false})
                            .to_string(),
                    ),
                }
            }
            ("POST", "/drive/v3/files") => {
                let meta = j(&r.body);
                let id = format!("folder-{}", self.next.fetch_add(1, Ordering::SeqCst));
                files.push(Dfile {
                    id: id.clone(),
                    name: meta["name"].as_str().unwrap().into(),
                    folder: true,
                    store_id: meta["appProperties"]["noeviaStoreId"]
                        .as_str()
                        .map(str::to_owned),
                    ..Dfile::default()
                });
                reply("200 OK", &serde_json::json!({"id": id}).to_string())
            }
            ("PATCH", p) if p.starts_with("/drive/v3/files/") => {
                let id = &p["/drive/v3/files/".len()..];
                let meta = j(&r.body);
                if let Some(f) = files.iter_mut().find(|f| f.id == id) {
                    f.store_id = meta["appProperties"]["noeviaStoreId"]
                        .as_str()
                        .map(str::to_owned);
                }
                reply("200 OK", &serde_json::json!({"id": id}).to_string())
            }
            ("DELETE", p) if p.starts_with("/drive/v3/files/") => {
                let id = &p["/drive/v3/files/".len()..];
                files.retain(|f| f.id != id);
                (
                    Duration::ZERO,
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec(),
                )
            }
            ("POST", "/upload/drive/v3/files") => {
                let boundary = r
                    .body
                    .lines()
                    .next()
                    .unwrap()
                    .trim_start_matches("--")
                    .to_string();
                let bytes = &r.bytes;
                let first_end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let meta_end = bytes[first_end..]
                    .windows(boundary.len() + 4)
                    .position(|w| w == format!("\r\n--{boundary}").as_bytes())
                    .unwrap()
                    + first_end;
                let meta = j(std::str::from_utf8(&bytes[first_end..meta_end]).unwrap());
                let second = bytes[meta_end..]
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .unwrap()
                    + meta_end
                    + 4;
                let tail = format!("\r\n--{boundary}--");
                let body = bytes[second..bytes.len() - tail.len()].to_vec();
                let id = format!("file-{}", self.next.fetch_add(1, Ordering::SeqCst));
                files.push(Dfile {
                    id: id.clone(),
                    name: meta["name"].as_str().unwrap().into(),
                    parent: meta["parents"][0].as_str().map(str::to_owned),
                    body,
                    ..Dfile::default()
                });
                reply("200 OK", &serde_json::json!({"id": id}).to_string())
            }
            _ => reply("404 Not Found", "{}"),
        }
    }
}

/// OAuth (device grant) plus the Drive model on one synthetic server.
fn install_drive(up: &Upstream, granted: Arc<std::sync::atomic::AtomicUsize>, drive: DriveModel) {
    let id_token = jwt(r#"{"email":"admin@example.test"}"#);
    up.set(move |r: &Seen| match r.path.as_str() {
        "/device/code" => reply("200 OK", r#"{"device_code":"dev-1","user_code":"ABCD-EFGH","verification_url":"https://www.google.com/device","expires_in":1800,"interval":1}"#),
        "/token" if r.body.contains("grant_type=urn") => {
            if granted.load(Ordering::SeqCst) == 0 {
                reply("428 Precondition Required", r#"{"error":"authorization_pending"}"#)
            } else {
                reply("200 OK", &format!(r#"{{"access_token":"access-0","refresh_token":"refresh-0","expires_in":3600,"id_token":"{id_token}"}}"#))
            }
        }
        "/token" => reply("200 OK", r#"{"access_token":"access-1","expires_in":3600}"#),
        "/revoke" => reply("200 OK", "{}"),
        _ => drive.handle(r),
    });
}

async fn connect_drive(w: &World) {
    let r = send(
        w.f.addr,
        "POST",
        &format!("{ROOT}/google/connect"),
        ADMIN,
        "",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.json()["state"], "pending");
    w.g.granted.store(1, Ordering::SeqCst);
    let started = Instant::now();
    while read_backup_file(w.data.path()).is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the backup slot never connected"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits for the background copy a run starts (the page's `google.copy.state`).
async fn wait_copy(a: std::net::SocketAddr, state: &str) {
    let started = Instant::now();
    loop {
        let s = get(a, ROOT, ADMIN).await.json();
        if s["google"]["copy"]["state"] == state && s["busy"].is_null() {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the copy never reached {state}: {s}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn only_an_administrator_and_only_the_documented_methods() {
    let w = world(&[]).await;
    let a = w.f.addr;
    for path in [
        ROOT,
        &format!("{ROOT}/run"),
        &format!("{ROOT}/recovery-key"),
    ] {
        assert_eq!(get(a, path, "").await.status, 401, "{path}");
        let r = get(a, path, MEMBER).await;
        assert_eq!(
            (r.status, r.json()["error"].as_str().map(str::to_owned)),
            (403, Some("Administrator required".into())),
            "{path}"
        );
    }
    assert_eq!(
        http(a, "POST", &format!("{ROOT}/run"), ADMIN, "", b"")
            .await
            .status,
        403,
        "the CSRF check is the gate's"
    );
    let r = send(a, "POST", ROOT, ADMIN, "").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str().map(str::to_owned)),
        (405, Some("method not allowed".into()))
    );
    for action in [
        "run",
        "verify",
        "copy",
        "google/connect",
        "google/disconnect",
    ] {
        assert_eq!(
            get(a, &format!("{ROOT}/{action}"), ADMIN).await.status,
            405,
            "{action}"
        );
    }
    assert_eq!(
        send(a, "POST", &format!("{ROOT}/recovery-key"), ADMIN, "")
            .await
            .status,
        405
    );
    assert_eq!(send(a, "DELETE", ROOT, ADMIN, "").await.status, 405);
    assert!(w.up.seen().is_empty(), "nothing outbound for any of them");
}

#[tokio::test]
async fn the_status_says_why_it_is_off_or_not_configured() {
    let w = world(&[("NOEVIA_FEATURE_OFFSITE_BACKUP", "0".into())]).await;
    let s = get(w.f.addr, ROOT, ADMIN).await.json();
    assert_eq!(
        (
            s["enabled"].as_bool(),
            s["ready"].as_bool(),
            s["reasonCode"].as_str()
        ),
        (Some(false), Some(false), Some("off")),
        "{s}"
    );
    assert_eq!(
        s["reason"],
        "Off-site backups are turned off (Settings → Features)."
    );
    assert_eq!(s["reasonGaps"], serde_json::json!([]));
    for action in ["run", "verify", "copy"] {
        let r = send(w.f.addr, "POST", &format!("{ROOT}/{action}"), ADMIN, "").await;
        assert_eq!(
            (r.status, r.json()["error"].as_str().map(str::to_owned)),
            (
                409,
                Some("Off-site backups are turned off (Settings → Features).".into())
            ),
            "{action}"
        );
    }
    // Enabled but no destination.
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let up = upstream().await;
    let f = front(
        d.path(),
        &up.base(),
        &[("NOEVIA_FEATURE_OFFSITE_BACKUP", "1".into())],
    )
    .await;
    let s = get(f.addr, ROOT, ADMIN).await.json();
    assert_eq!(s["reasonCode"], "notConfigured");
    assert_eq!(
        s["reasonGaps"],
        serde_json::json!([
            "OFFSITE_BACKUP_S3_ENDPOINT",
            "OFFSITE_BACKUP_S3_BUCKET",
            "OFFSITE_BACKUP_S3_ACCESS_KEY_ID",
            "OFFSITE_BACKUP_S3_SECRET_ACCESS_KEY",
            "OFFSITE_BACKUP_KEY_FILE"
        ])
    );
    assert!(s["reason"]
        .as_str()
        .unwrap()
        .starts_with("Not configured: set OFFSITE_BACKUP_S3_ENDPOINT"));
    assert!(
        s["google"].is_null() && s["destination"].is_null() && s["destinationFolder"].is_null()
    );
    let r = send(f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "").await;
    assert_eq!(r.status, 409);
    // An admin-saved flag (the settings row) turns it on when the environment says nothing.
    let d3 = tempfile::tempdir().unwrap();
    let db = seed(d3.path());
    db.execute(
        "INSERT OR REPLACE INTO settings(key,value) VALUES('feature:offsiteBackup','true')",
        [],
    )
    .unwrap();
    let f3 = front(d3.path(), &up.base(), &[]).await;
    assert_eq!(get(f3.addr, ROOT, ADMIN).await.json()["enabled"], true);
}

#[tokio::test]
async fn the_status_of_a_configured_folder_destination_has_core_shape() {
    let w = world(&[("OFFSITE_BACKUP_MIRROR", "gdrive:noevia".into())]).await;
    let s = get(w.f.addr, ROOT, ADMIN).await.json();
    let dest = w.dest.display().to_string();
    assert_eq!(s["enabled"], true);
    assert_eq!(s["ready"], true);
    assert!(s["reason"].is_null() && s["reasonCode"].is_null());
    assert_eq!(s["busy"], serde_json::Value::Null);
    assert_eq!(s["schedule"], "Daily at 03:00 (server time)");
    assert_eq!(s["scheduleHour"], 3);
    assert_eq!(
        s["retention"],
        "Keeps 7 daily, 4 weekly and 6 monthly snapshots"
    );
    assert_eq!(
        s["retentionPolicy"],
        serde_json::json!({"daily":7,"weekly":4,"monthly":6})
    );
    assert_eq!(
        s["destinationFolder"],
        serde_json::json!({"path": dest, "mirror": "gdrive:noevia"})
    );
    assert_eq!(
        s["destination"],
        format!("Folder {dest}, mirrored to gdrive:noevia")
    );
    assert_eq!(s["paths"], 1);
    assert_eq!(s["google"]["state"], "disconnected");
    assert_eq!(s["google"]["copyEnabled"], true);
    assert!(s["google"]["copy"].is_null());
    assert!(s["google"].get("owner").is_none());
    for k in ["lastBackup", "lastVerify", "lastError", "snapshots"] {
        assert!(s[k].is_null(), "{k}");
    }
    // The key order is core's, which the page relies on for nothing but we keep for diffs.
    let text = get(w.f.addr, ROOT, ADMIN).await.text;
    let order: Vec<usize> = [
        "\"enabled\"",
        "\"ready\"",
        "\"reason\"",
        "\"reasonCode\"",
        "\"reasonGaps\"",
        "\"busy\"",
        "\"schedule\"",
        "\"scheduleHour\"",
        "\"retention\"",
        "\"retentionPolicy\"",
        "\"destinationFolder\"",
        "\"destination\"",
        "\"google\"",
        "\"paths\"",
        "\"lastBackup\"",
        "\"lastVerify\"",
        "\"lastError\"",
        "\"snapshots\"",
    ]
    .iter()
    .map(|k| text.find(k).unwrap())
    .collect();
    assert!(order.windows(2).all(|p| p[0] < p[1]), "{text}");
}

#[tokio::test]
async fn a_run_backs_up_the_data_directory_including_a_live_sqlite_database_and_the_restore_test_passes(
) {
    let w = world(&[("OFFSITE_BACKUP_HOUR", "9".into())]).await;
    let data = w.data.path();
    // A second database in WAL mode with an open writer, a symlink out of the tree and journal files.
    let live = server_store::rusqlite::Connection::open(data.join("live.sqlite3")).unwrap();
    live.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE t(a); INSERT INTO t VALUES(1),(2),(3);",
    )
    .unwrap();
    std::os::unix::fs::symlink("/etc/passwd", data.join("link")).unwrap();
    std::fs::write(data.join("cowork.db-wal.keep-wal"), "x").unwrap();
    // A file bigger than one chunk, deterministic.
    let mut state = 12345u32;
    let big: Vec<u8> = (0..9 * 1024 * 1024u32)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect();
    std::fs::write(data.join("big.bin"), &big).unwrap();

    let r = send(w.f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "").await;
    assert_eq!(r.status, 200, "{}", r.text);
    let last = r.json();
    assert_eq!(
        last["id"].as_str().unwrap().len(),
        "2023-11-14T22-13-20-000Z-edb2f8aa".len()
    );
    assert!(last["at"].as_i64().unwrap() > 1_700_000_000_000);
    assert!(last["files"].as_u64().unwrap() >= 6, "{last}");
    assert!(last["uploadedBytes"].as_u64().unwrap() > 9 * 1024 * 1024);
    let listed = files_under(&w.dest);
    assert!(listed.contains(&"config".to_string()));
    assert_eq!(
        listed
            .iter()
            .filter(|f| f.starts_with("snapshots/"))
            .count(),
        1
    );
    assert!(
        listed.iter().filter(|f| f.starts_with("data/")).count() >= 5,
        "chunks of every file, big.bin in three"
    );
    assert!(
        listed.iter().all(|f| !f.contains("tmp-")),
        "no temp file is left in the destination"
    );
    // Every object is sealed: no plaintext anywhere in the destination.
    for f in &listed {
        let bytes = std::fs::read(w.dest.join(f)).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("synthetic notes"),
            "{f}"
        );
    }
    let s = get(w.f.addr, ROOT, ADMIN).await.json();
    assert_eq!(s["lastBackup"]["id"], last["id"]);
    assert_eq!(s["snapshots"], 1);
    assert!(s["lastError"].is_null());

    // The restore test: the newest snapshot restored into a scratch directory and verified.
    let r = send(w.f.addr, "POST", &format!("{ROOT}/verify"), ADMIN, "").await;
    assert_eq!(r.status, 200, "{}", r.text);
    let v = r.json();
    assert_eq!(v["id"], last["id"]);
    assert_eq!(v["files"], last["files"]);
    assert!(v["verifiedAt"].as_i64().unwrap() > 0);
    assert!(get(w.f.addr, ROOT, ADMIN).await.json()["lastVerify"]["id"] == last["id"]);

    // An independent restore: the files come back byte for byte, the SQLite copies open and hold the rows.
    let key = [9u8; 32];
    let engine = Engine::new(
        Store::Dir(DirStore::new(&w.dest).unwrap()),
        &key,
        vec![data.to_path_buf()],
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();
    let target = out.path().join("restored");
    let files = engine
        .restore(last["id"].as_str().unwrap(), &target)
        .await
        .unwrap();
    assert_eq!(files as u64, last["files"].as_u64().unwrap());
    let root = target.join("0");
    assert_eq!(
        std::fs::read(root.join("notes.txt")).unwrap(),
        b"synthetic notes"
    );
    assert_eq!(
        std::fs::read(root.join("users/a/projects.json")).unwrap(),
        br#"{"projects":[]}"#
    );
    assert_eq!(std::fs::read(root.join("big.bin")).unwrap(), big);
    assert!(!root.join("link").exists(), "a symlink is never followed");
    assert!(root.join("cowork.db").exists() && root.join("live.sqlite3").exists());
    let copy = server_store::rusqlite::Connection::open(root.join("live.sqlite3")).unwrap();
    let n: i64 = copy
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 3, "a consistent copy of the live WAL database");
    let cow = server_store::rusqlite::Connection::open(root.join("cowork.db")).unwrap();
    let users: i64 = cow
        .query_row("SELECT count(*) FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(users, 3);
    // Restoring into a directory that is not empty is refused, and a missing snapshot is a 404.
    assert_eq!(
        engine
            .restore(last["id"].as_str().unwrap(), &target)
            .await
            .unwrap_err()
            .status,
        409
    );
    assert_eq!(
        engine
            .restore("nope", &out.path().join("n"))
            .await
            .unwrap_err()
            .status,
        404
    );

    // A second run uploads only what changed (the keyed chunk ids deduplicate), retention keeps both.
    std::fs::write(data.join("notes.txt"), "synthetic notes, edited").unwrap();
    let again = send(w.f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "")
        .await
        .json();
    assert!(
        again["uploadedBytes"].as_u64().unwrap() < 4_000_000,
        "{again}"
    );
    // Retention keeps the newest snapshot of a day: the first of today is forgotten, its unique chunk
    // pruned, and the chunks the new snapshot shares with it stay.
    assert_eq!(get(w.f.addr, ROOT, ADMIN).await.json()["snapshots"], 1);
    let now_listed = files_under(&w.dest);
    assert_eq!(
        now_listed
            .iter()
            .filter(|f| f.starts_with("snapshots/"))
            .count(),
        1
    );
    assert!(!now_listed.contains(&format!("snapshots/{}", last["id"].as_str().unwrap())));
    let latest = get(w.f.addr, ROOT, ADMIN).await.json()["lastBackup"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        send(w.f.addr, "POST", &format!("{ROOT}/verify"), ADMIN, "")
            .await
            .json()["id"],
        latest.as_str()
    );
}

#[tokio::test]
async fn a_store_another_key_wrote_is_refused_and_the_failure_is_recorded() {
    let w = world(&[]).await;
    // A store sealed under a different key (or by another tool): nothing is written to it.
    let other = offsite_backup_format::Keys::derive(&[1u8; 32]).unwrap();
    std::fs::write(
        w.dest.join("config"),
        other.seal(offsite_backup_format::config_json(5).as_bytes(), &[2; 12]),
    )
    .unwrap();
    let r = send(w.f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str().map(str::to_owned)),
        (
            422,
            Some("A backup object failed authentication (wrong key or tampered data).".into())
        )
    );
    assert_eq!(files_under(&w.dest), ["config"], "no snapshot, no chunk");
    let s = get(w.f.addr, ROOT, ADMIN).await.json();
    assert_eq!(s["lastError"]["during"], "backup");
    assert_eq!(
        s["lastError"]["message"],
        "A backup object failed authentication (wrong key or tampered data)."
    );
    assert!(
        s["busy"].is_null(),
        "the busy mark is released after a failure"
    );
    // A format this build does not know.
    std::fs::write(
        w.dest.join("config"),
        Keys9().seal(br#"{"format":"restic-v9"}"#, &[2; 12]),
    )
    .unwrap();
    let r = send(w.f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str().map(str::to_owned)),
        (
            409,
            Some("The destination holds a different backup format.".into())
        )
    );
    // Nothing to verify yet.
    std::fs::remove_file(w.dest.join("config")).unwrap();
    let r = send(w.f.addr, "POST", &format!("{ROOT}/verify"), ADMIN, "").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str().map(str::to_owned)),
        (409, Some("There is no snapshot to verify yet.".into()))
    );
}

#[allow(non_snake_case)]
fn Keys9() -> offsite_backup_format::Keys {
    offsite_backup_format::Keys::derive(&[9u8; 32]).unwrap()
}

#[tokio::test]
async fn a_damaged_chunk_fails_the_restore_test() {
    let w = world(&[]).await;
    assert_eq!(
        send(w.f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "")
            .await
            .status,
        200
    );
    let chunk = files_under(&w.dest)
        .into_iter()
        .find(|f| f.starts_with("data/"))
        .unwrap();
    let mut bytes = std::fs::read(w.dest.join(&chunk)).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(w.dest.join(&chunk), bytes).unwrap();
    let r = send(w.f.addr, "POST", &format!("{ROOT}/verify"), ADMIN, "").await;
    assert_eq!(r.status, 422, "{}", r.text);
    assert!(r.json()["error"]
        .as_str()
        .unwrap()
        .contains("failed authentication"));
    let s = get(w.f.addr, ROOT, ADMIN).await.json();
    assert_eq!(s["lastError"]["during"], "restore test");
    // The scratch directory was removed.
    let leftovers = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("noevia-restore-test-")
                && e.metadata()
                    .map(|m| m.modified().unwrap().elapsed().unwrap() < Duration::from_secs(30))
                    .unwrap_or(false)
        })
        .count();
    assert_eq!(leftovers, 0);
}

#[tokio::test]
async fn the_recovery_key_is_a_download_and_only_that() {
    let w = world(&[]).await;
    let r = get(w.f.addr, &format!("{ROOT}/recovery-key"), ADMIN).await;
    assert_eq!(r.status, 200);
    assert!(
        r.head.contains("content-type: text/plain; charset=utf-8"),
        "{}",
        r.head
    );
    assert!(r
        .head
        .contains("content-disposition: attachment; filename=\"noevia-backup-recovery-key.txt\""));
    assert!(r.head.contains("cache-control: no-store"));
    assert_eq!(r.text, format!("noevia backup recovery key\n\nKeep this in a password manager. Without it, no backup can be restored.\nDo not store it in the same Google Drive as the backups.\n\n{BACKUP_KEY_HEX}\n"));
    // The key is in no other answer: not the status, not a run's, not an error's.
    for path in [ROOT.to_string(), format!("{ROOT}/google/connect")] {
        let r = if path == ROOT {
            get(w.f.addr, &path, ADMIN).await
        } else {
            send(w.f.addr, "POST", &path, ADMIN, "").await
        };
        assert!(!r.text.contains(BACKUP_KEY_HEX), "{path}");
    }
    // A key kept inside a backed-up folder is refused with core's words, not served.
    std::fs::write(w.data.path().join("inside.key"), BACKUP_KEY_HEX).unwrap();
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    std::fs::write(d.path().join("inside.key"), BACKUP_KEY_HEX).unwrap();
    let f = front(
        d.path(),
        &w.up.base(),
        &[
            ("OFFSITE_BACKUP_DIR", w.dest.display().to_string()),
            (
                "OFFSITE_BACKUP_KEY_FILE",
                d.path().join("inside.key").display().to_string(),
            ),
            ("NOEVIA_FEATURE_OFFSITE_BACKUP", "1".into()),
        ],
    )
    .await;
    let r = get(f.addr, &format!("{ROOT}/recovery-key"), ADMIN).await;
    assert_eq!(
        (r.status, r.json()["error"].as_str().map(str::to_owned)),
        (
            409,
            Some("The backup key must be kept outside the backed-up folders.".into())
        )
    );
    // A missing key file.
    std::fs::remove_file(&w.key).unwrap();
    let r = get(w.f.addr, &format!("{ROOT}/recovery-key"), ADMIN).await;
    assert_eq!(
        (r.status, r.json()["error"].as_str().map(str::to_owned)),
        (409, Some("The backup key could not be read.".into()))
    );
}

#[tokio::test]
async fn an_overlapping_destination_is_refused_before_anything_is_written() {
    let data = tempfile::tempdir().unwrap();
    seed(data.path());
    let inside = data.path().join("backups");
    std::fs::create_dir_all(&inside).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let key = outside.path().join("k");
    std::fs::write(&key, BACKUP_KEY_HEX).unwrap();
    let up = upstream().await;
    let f = front(
        data.path(),
        &up.base(),
        &[
            ("OFFSITE_BACKUP_DIR", inside.display().to_string()),
            ("OFFSITE_BACKUP_KEY_FILE", key.display().to_string()),
            ("NOEVIA_FEATURE_OFFSITE_BACKUP", "1".into()),
        ],
    )
    .await;
    let r = send(f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str().map(str::to_owned)),
        (
            409,
            Some("The backup folder must not overlap the folders being backed up.".into())
        )
    );
    assert!(files_under(&inside).is_empty());
    let r = send(f.addr, "POST", &format!("{ROOT}/google/connect"), ADMIN, "").await;
    assert_eq!(
        r.status, 409,
        "the Google connect needs a usable destination too"
    );
    // A destination that does not exist.
    let f2 = front(
        data.path(),
        &up.base(),
        &[
            (
                "OFFSITE_BACKUP_DIR",
                outside.path().join("missing").display().to_string(),
            ),
            ("OFFSITE_BACKUP_KEY_FILE", key.display().to_string()),
            ("NOEVIA_FEATURE_OFFSITE_BACKUP", "1".into()),
        ],
    )
    .await;
    let r = send(f2.addr, "POST", &format!("{ROOT}/run"), ADMIN, "").await;
    assert_eq!(r.status, 409);
    assert!(r.json()["error"]
        .as_str()
        .unwrap()
        .ends_with("does not exist."));
}

#[tokio::test]
async fn google_connect_and_the_other_administrators_code() {
    let w = world(&[]).await;
    let a = w.f.addr;
    let r = send(a, "POST", &format!("{ROOT}/google/connect"), ADMIN, "").await;
    assert_eq!(r.status, 200, "{}", r.text);
    let g = r.json();
    assert_eq!(
        (g["state"].as_str(), g["userCode"].as_str()),
        (Some("pending"), Some("ABCD-EFGH"))
    );
    assert!(
        g.get("owner").is_none(),
        "the owner is stripped from the answer"
    );
    // The starter sees the code in the status; another administrator sees neither it nor the owner.
    let mine = get(a, ROOT, ADMIN).await.json();
    assert_eq!(mine["google"]["userCode"], "ABCD-EFGH");
    assert!(mine["google"].get("owner").is_none());
    let theirs = get(a, ROOT, ADMIN2).await.json();
    assert_eq!(theirs["google"]["state"], "pending");
    assert_eq!(
        theirs["google"]["message"],
        "Another administrator is connecting Google Drive."
    );
    for k in ["userCode", "verificationUrl", "expiresAt", "owner"] {
        assert!(theirs["google"].get(k).is_none(), "{k}");
    }
    for action in ["google/connect", "google/disconnect"] {
        let r = send(a, "POST", &format!("{ROOT}/{action}"), ADMIN2, "").await;
        assert_eq!(
            (r.status, r.json()["error"].as_str().map(str::to_owned)),
            (
                409,
                Some("Another administrator is connecting Google Drive.".into())
            ),
            "{action}"
        );
    }
    assert_eq!(
        w.up.count("/device/code"),
        1,
        "the second administrator started nothing"
    );
    // The starter finishes it; the slot is theirs, sealed under the backup key.
    w.g.granted.store(1, Ordering::SeqCst);
    let started = Instant::now();
    while read_backup_file(w.data.path()).is_none() {
        assert!(started.elapsed() < Duration::from_secs(20));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let s = get(a, ROOT, ADMIN).await.json();
    assert_eq!(
        (s["google"]["state"].as_str(), s["google"]["email"].as_str()),
        (Some("connected"), Some("admin@example.test"))
    );
    // Nothing of the connection but its state is in the status of another administrator.
    let theirs = get(a, ROOT, ADMIN2).await.json();
    assert_eq!(theirs["google"]["state"], "connected");
    assert!(!theirs.to_string().contains("refresh-0"));
    // Disconnect revokes, deletes and answers the new state.
    let r = send(a, "POST", &format!("{ROOT}/google/disconnect"), ADMIN, "").await;
    assert_eq!(
        (r.status, r.json()["state"].as_str().map(str::to_owned)),
        (200, Some("disconnected".into()))
    );
    assert!(read_backup_file(w.data.path()).is_none());
    assert!(w
        .up
        .seen()
        .iter()
        .any(|s| s.path == "/revoke" && s.body.contains("token=refresh-0")));
}

#[tokio::test]
async fn the_copy_to_drive_uploads_first_prunes_second_and_never_trusts_the_wrong_folder() {
    let w = world(&[]).await;
    let drive = DriveModel::default();
    install_drive(&w.up, Arc::clone(&w.g.granted), drive.clone());
    let a = w.f.addr;
    // Not connected: nothing to copy, nothing asked of Google.
    let r = send(a, "POST", &format!("{ROOT}/copy"), ADMIN, "").await;
    assert_eq!((r.status, r.text.as_str()), (200, "{}"));
    connect_drive(&w).await;
    // Connected but no backup yet: it waits, and the page is told why (a translatable id).
    let r = send(a, "POST", &format!("{ROOT}/copy"), ADMIN, "").await;
    assert_eq!(r.status, 409);
    assert_eq!((r.json()["error"].as_str(), r.json()["errorId"].as_str()), (Some("The local backup store looks empty, so nothing was copied (this protects the copy on Drive)."), Some("storeEmpty")));
    let s = get(a, ROOT, ADMIN).await.json();
    assert_eq!(s["google"]["copy"]["state"], "refused");
    assert_eq!(s["google"]["copy"]["messageId"], "storeEmpty");

    // A run copies in the background once it has released its mark.
    assert_eq!(
        send(a, "POST", &format!("{ROOT}/run"), ADMIN, "")
            .await
            .status,
        200
    );
    let started = Instant::now();
    loop {
        let s = get(a, ROOT, ADMIN).await.json();
        if s["google"]["copy"]["state"] == "ok" {
            assert!(s["google"]["copy"]["message"]
                .as_str()
                .unwrap()
                .starts_with("Copied 1 snapshots."));
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "{s}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let folders: Vec<Dfile> = drive.list().into_iter().filter(|f| f.folder).collect();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].name, "noevia-offsite");
    let local = files_under(&w.dest);
    let mut remote: Vec<String> = drive
        .children(&folders[0].id)
        .into_iter()
        .map(|f| f.name)
        .collect();
    remote.sort();
    assert_eq!(
        remote, local,
        "every object, named by its key, in one flat folder"
    );
    // The folder is stamped with a hash of the store's config object, and remembered in the token file.
    let config = std::fs::read(w.dest.join("config")).unwrap();
    use sha2::Digest as _;
    let store_id: String = sha2::Sha256::digest(&config)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(folders[0].store_id.as_deref(), Some(store_id.as_str()));
    let saved = read_backup_file(w.data.path()).unwrap();
    assert_eq!(
        (saved["folderId"].as_str(), saved["storeId"].as_str()),
        (Some(folders[0].id.as_str()), Some(store_id.as_str()))
    );
    assert!(drive
        .authorised
        .lock()
        .unwrap()
        .iter()
        .all(|h| h == "authorization: bearer access-0"));

    // Copy again: nothing new to send. The route answers core's result object.
    let before = w.up.count("/upload/drive/v3/files");
    let r = send(a, "POST", &format!("{ROOT}/copy"), ADMIN, "").await;
    assert_eq!(r.status, 200, "{}", r.text);
    let c = r.json();
    assert_eq!(
        (
            c["uploaded"].as_u64(),
            c["removed"].as_u64(),
            c["folder"].as_str(),
            c["newFolder"].as_bool()
        ),
        (Some(0), Some(0), Some("noevia-offsite"), Some(false)),
        "{c}"
    );
    assert_eq!(w.up.count("/upload/drive/v3/files"), before);

    // A pruned local snapshot leaves stale files on Drive: pruned after the upload when it is a small share.
    for i in 0..30 {
        drive.add(Dfile {
            id: format!("old-{i}"),
            name: format!("data/zz/old{i}"),
            parent: Some(folders[0].id.clone()),
            body: vec![1; 3],
            ..Dfile::default()
        });
    }
    let r = send(a, "POST", &format!("{ROOT}/copy"), ADMIN, "").await;
    assert_eq!(
        r.status,
        502,
        "30 stale files of {} is more than a quarter: nothing is removed",
        30 + local.len()
    );
    assert_eq!(r.json()["errorId"], "staleShare");
    assert_eq!(r.json()["errorParams"]["stale"], 30);
    assert_eq!(drive.children(&folders[0].id).len(), local.len() + 30);
    assert_eq!(
        get(a, ROOT, ADMIN).await.json()["google"]["copy"]["state"],
        "failed"
    );
    drive
        .files
        .lock()
        .unwrap()
        .retain(|f| !f.id.starts_with("old-") || f.id == "old-0" || f.id == "old-1");
    let c = send(a, "POST", &format!("{ROOT}/copy"), ADMIN, "")
        .await
        .json();
    assert_eq!(c["removed"], 2, "a small share of stale files is retention");
    assert_eq!(drive.children(&folders[0].id).len(), local.len());

    // A same-named object of another size is corruption, never overwritten.
    drive
        .files
        .lock()
        .unwrap()
        .iter_mut()
        .find(|f| f.name == "config")
        .unwrap()
        .body
        .push(0);
    let r = send(a, "POST", &format!("{ROOT}/copy"), ADMIN, "").await;
    assert_eq!(
        (r.status, r.json()["errorId"].as_str().map(str::to_owned)),
        (409, Some("differentStore".into()))
    );
    drive
        .files
        .lock()
        .unwrap()
        .iter_mut()
        .find(|f| f.name == "config")
        .unwrap()
        .body
        .pop();
    let victim = local.iter().find(|f| f.starts_with("data/")).unwrap();
    drive
        .files
        .lock()
        .unwrap()
        .iter_mut()
        .find(|f| &f.name == victim)
        .unwrap()
        .body
        .push(0);
    let r = send(a, "POST", &format!("{ROOT}/copy"), ADMIN, "").await;
    assert_eq!(
        (
            r.status,
            r.json()["errorId"].as_str().map(str::to_owned),
            r.json()["errorParams"]["name"].as_str().map(str::to_owned)
        ),
        (502, Some("fileDiffers".into()), Some("data".into()))
    );
    drive
        .files
        .lock()
        .unwrap()
        .iter_mut()
        .find(|f| &f.name == victim)
        .unwrap()
        .body
        .pop();
}

#[tokio::test]
async fn a_folder_of_another_store_is_never_pruned_and_the_copy_goes_to_a_sibling() {
    let w = world(&[]).await;
    let drive = DriveModel::default();
    install_drive(&w.up, Arc::clone(&w.g.granted), drive.clone());
    connect_drive(&w).await;
    // A folder an earlier, different backup store left on Drive.
    drive.add(Dfile {
        id: "other".into(),
        name: "noevia-offsite".into(),
        folder: true,
        store_id: Some("someone-elses-store".into()),
        ..Dfile::default()
    });
    drive.add(Dfile {
        id: "other-1".into(),
        name: "config".into(),
        parent: Some("other".into()),
        body: vec![1, 2, 3],
        ..Dfile::default()
    });
    assert_eq!(
        send(w.f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "")
            .await
            .status,
        200
    );
    wait_copy(w.f.addr, "ok").await;
    let folders: Vec<Dfile> = drive.list().into_iter().filter(|f| f.folder).collect();
    let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        ["noevia-offsite", "noevia-offsite (2)"],
        "the new store went to a sibling"
    );
    assert_eq!(
        drive.children("other").len(),
        1,
        "the old folder is untouched"
    );
    assert!(drive
        .children(&folders[1].id)
        .iter()
        .any(|f| f.name == "config"));
    let again = send(w.f.addr, "POST", &format!("{ROOT}/copy"), ADMIN, "")
        .await
        .json();
    assert_eq!(
        (
            again["folder"].as_str(),
            again["newFolder"].as_bool(),
            again["uploaded"].as_u64()
        ),
        (Some("noevia-offsite (2)"), Some(false), Some(0)),
        "{again}"
    );

    // An unstamped (older) folder whose config is identical is adopted and stamped.
    let w2 = world(&[]).await;
    let drive2 = DriveModel::default();
    install_drive(&w2.up, Arc::clone(&w2.g.granted), drive2.clone());
    connect_drive(&w2).await;
    let off = send(
        w2.f.addr,
        "PUT",
        "/api/connectors/gdrive/backup-copy",
        ADMIN,
        r#"{"enabled":false}"#,
    )
    .await;
    assert_eq!(off.status, 200);
    assert_eq!(
        send(w2.f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "")
            .await
            .status,
        200
    );
    let config = std::fs::read(w2.dest.join("config")).unwrap();
    drive2.add(Dfile {
        id: "legacy".into(),
        name: "noevia-offsite".into(),
        folder: true,
        ..Dfile::default()
    });
    drive2.add(Dfile {
        id: "legacy-config".into(),
        name: "config".into(),
        parent: Some("legacy".into()),
        body: config,
        ..Dfile::default()
    });
    let on = send(
        w2.f.addr,
        "PUT",
        "/api/connectors/gdrive/backup-copy",
        ADMIN,
        r#"{"enabled":true}"#,
    )
    .await;
    assert_eq!(on.status, 200);
    wait_copy(w2.f.addr, "ok").await;
    let folders: Vec<Dfile> = drive2.list().into_iter().filter(|f| f.folder).collect();
    assert_eq!(folders.len(), 1, "adopted, not duplicated");
    assert!(folders[0].store_id.is_some(), "and stamped now");
    assert!(
        drive2.children("legacy").len() > 1,
        "the rest of the store was uploaded into it"
    );
}

#[tokio::test]
async fn a_second_job_is_refused_while_one_runs() {
    let w = world(&[]).await;
    let drive = DriveModel::default();
    install_drive(&w.up, Arc::clone(&w.g.granted), drive.clone());
    connect_drive(&w).await;
    assert_eq!(
        send(w.f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "")
            .await
            .status,
        200
    );
    wait_copy(w.f.addr, "ok").await;
    // Make Drive slow so the copy is in flight.
    let slow = drive.clone();
    let id_token = jwt(r#"{"email":"admin@example.test"}"#);
    let _ = id_token;
    w.up.set(move |r: &Seen| {
        if r.path.starts_with("/upload/") {
            let (_, text) = slow.handle(r);
            return (Duration::from_millis(1500), text);
        }
        if r.path == "/token" {
            return reply("200 OK", r#"{"access_token":"access-1","expires_in":3600}"#);
        }
        slow.handle(r)
    });
    // Everything uploaded already; delete one remote object so the copy has something to send.
    let folder = drive.list().into_iter().find(|f| f.folder).unwrap();
    let victim = drive
        .children(&folder.id)
        .into_iter()
        .find(|f| f.name.starts_with("data/"))
        .unwrap();
    drive.files.lock().unwrap().retain(|f| f.id != victim.id);
    let before = w.up.count("/upload/");
    let (a, copy) = (
        w.f.addr,
        tokio::spawn({
            let addr = w.f.addr;
            async move { send(addr, "POST", &format!("{ROOT}/copy"), ADMIN, "").await }
        }),
    );
    w.up.wait_for("/upload/", before + 1).await;
    let busy = get(a, ROOT, ADMIN).await.json();
    assert_eq!(busy["busy"], "copy to Google Drive");
    for action in ["run", "verify", "copy"] {
        let r = send(a, "POST", &format!("{ROOT}/{action}"), ADMIN, "").await;
        assert_eq!(
            (r.status, r.json()["error"].as_str().map(str::to_owned)),
            (
                409,
                Some("A copy to Google Drive is already running.".into())
            ),
            "{action}"
        );
    }
    assert_eq!(copy.await.unwrap().status, 200);
    assert!(get(a, ROOT, ADMIN).await.json()["busy"].is_null());
}

#[tokio::test]
async fn the_copy_setting_gates_the_copy_and_turning_it_on_copies() {
    let w = world(&[]).await;
    let drive = DriveModel::default();
    install_drive(&w.up, Arc::clone(&w.g.granted), drive.clone());
    connect_drive(&w).await;
    let r = send(
        w.f.addr,
        "PUT",
        "/api/connectors/gdrive/backup-copy",
        ADMIN,
        r#"{"enabled":false}"#,
    )
    .await;
    assert_eq!(
        (r.status, r.json()["backup"]["copyEnabled"].as_bool()),
        (200, Some(false)),
        "{}",
        r.text
    );
    assert_eq!(
        send(w.f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "")
            .await
            .status,
        200
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        w.up.count("/upload/"),
        0,
        "a run does not copy while the copy is off"
    );
    assert_eq!(
        send(w.f.addr, "POST", &format!("{ROOT}/copy"), ADMIN, "")
            .await
            .text,
        "{}"
    );
    let r = send(
        w.f.addr,
        "PUT",
        "/api/connectors/gdrive/backup-copy",
        ADMIN,
        r#"{"enabled":true}"#,
    )
    .await;
    assert_eq!(r.json()["backup"]["copyEnabled"], true);
    let started = Instant::now();
    while drive.list().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "turning the copy on did not copy"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn the_nightly_schedule_runs_once_outside_the_quiet_windows() {
    let w = world(&[("OFFSITE_BACKUP_HOUR", "3".into())]).await;
    let service = Arc::clone(w.f.app.offsite.as_ref().unwrap());
    let app = Arc::clone(&w.f.app);
    let at = |h: i64, m: i64| {
        let days = offsite_backup_format::days_from_civil(2026, 10, 30);
        days * 86_400_000 + h * 3_600_000 + m * 60_000
    };
    let tick = |now: i64| {
        let (service, app) = (Arc::clone(&service), Arc::clone(&app));
        async move { service.tick_at(&app, Arc::new(move || now)).await }
    };
    // 02:59 UTC: before the hour. 04:10 UTC: the appdata window (UTC server). Neither starts a run.
    assert!(!tick(at(2, 59)).await);
    assert!(!tick(at(4, 10)).await, "inside the appdata backup window");
    assert!(files_under(&w.dest).is_empty());
    // 03:30 UTC (a UTC server): the hour, outside both windows (the NAS window is 06:20-08:30 UTC in October).
    assert!(tick(at(3, 30)).await, "a run starts");
    assert_eq!(get(w.f.addr, ROOT, ADMIN).await.json()["snapshots"], 1);
    // It does not run again the same night: the last backup is under 20 hours old. (The run stamped
    // the real clock; the test's clock is a date in October, so stamp the test's.)
    let status_path = w.data.path().join("offsite-backup-status.json");
    let mut status = j(&std::fs::read_to_string(&status_path).unwrap());
    status["lastBackup"]["at"] = serde_json::json!(at(3, 30));
    std::fs::write(&status_path, status.to_string()).unwrap();
    assert!(!tick(at(3, 45)).await);
    assert!(!tick(at(5, 0)).await);
    assert_eq!(
        files_under(&w.dest)
            .iter()
            .filter(|f| f.starts_with("snapshots/"))
            .count(),
        1
    );
    assert!(
        !tick(at(3, 30) + 19 * 3_600_000).await,
        "19 hours later is the same grace window of no day"
    );
    let first_id = get(w.f.addr, ROOT, ADMIN).await.json()["lastBackup"]["id"].clone();
    assert!(tick(at(3, 30) + 24 * 3_600_000).await, "the next night");
    assert_ne!(
        get(w.f.addr, ROOT, ADMIN).await.json()["lastBackup"]["id"],
        first_id
    );
    // The NAS window (02:20-04:30 New York, 06:20-08:30 UTC in October) holds a due run back.
    let w2 = world(&[("OFFSITE_BACKUP_HOUR", "6".into())]).await;
    let (s2, a2) = (
        Arc::clone(w2.f.app.offsite.as_ref().unwrap()),
        Arc::clone(&w2.f.app),
    );
    let now = at(6, 40);
    assert!(
        !s2.tick_at(&a2, Arc::new(move || now)).await,
        "06:40 UTC is 02:40 in New York"
    );
    let later = at(8, 31);
    assert!(
        s2.tick_at(&a2, Arc::new(move || later)).await,
        "catches up once the window has ended"
    );
    // A server on New York time holds the appdata window at 04:05-04:25 local (08:05-08:25 UTC).
    let w3 = world(&[
        ("OFFSITE_BACKUP_HOUR", "3".into()),
        ("TZ", "America/New_York".into()),
    ])
    .await;
    let (s3, a3) = (
        Arc::clone(w3.f.app.offsite.as_ref().unwrap()),
        Arc::clone(&w3.f.app),
    );
    let inside = at(8, 10);
    assert!(!s3.tick_at(&a3, Arc::new(move || inside)).await);
    let after = at(8, 31);
    assert!(s3.tick_at(&a3, Arc::new(move || after)).await);
    // Disabled or unconfigured: never.
    let w4 = world(&[("NOEVIA_FEATURE_OFFSITE_BACKUP", "0".into())]).await;
    let (s4, a4) = (
        Arc::clone(w4.f.app.offsite.as_ref().unwrap()),
        Arc::clone(&w4.f.app),
    );
    let now = at(3, 30);
    assert!(!s4.tick_at(&a4, Arc::new(move || now)).await);
    let _ = (w.outside.path(), Pace::none());
}

#[tokio::test]
async fn a_store_node_wrote_is_read_and_restored_by_rust() {
    // tests/fixtures/offsite-node-store was written by core's offsite-backup.cjs + offsite-dir.cjs
    // (key [9;32], six files incl. an empty one, a duplicate pair, a Unicode name, a symlink and a
    // -wal file that are not in it). Rust lists, restores and verifies it byte for byte.
    let store = DirStore::new(
        &std::fs::canonicalize(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/offsite-node-store"
        ))
        .unwrap(),
    )
    .unwrap();
    let engine = Engine::new(Store::Dir(store), &[9u8; 32], vec![]).unwrap();
    let snaps = engine.snapshots().await.unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(
        (
            snaps[0].id.as_str(),
            snaps[0].time,
            snaps[0].files,
            snaps[0].incomplete
        ),
        (
            "2023-11-14T22-13-20-000Z-edb2f8aa",
            1_700_000_000_000,
            6,
            false
        )
    );
    assert_eq!(snaps[0].bytes, 337);
    let out = tempfile::tempdir().unwrap();
    let target = out.path().join("r");
    assert_eq!(engine.restore(&snaps[0].id, &target).await.unwrap(), 6);
    let root = target.join("0");
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"hello");
    assert_eq!(
        std::fs::read(root.join("sub/b.bin")).unwrap(),
        (0..300u32)
            .map(|i| ((i * 7) % 256) as u8)
            .collect::<Vec<u8>>()
    );
    assert_eq!(
        std::fs::read(root.join("sub/deep/ünïcode name.txt")).unwrap(),
        b"unicode path"
    );
    assert_eq!(std::fs::read(root.join("empty.txt")).unwrap(), b"");
    assert_eq!(std::fs::read(root.join("dup1.txt")).unwrap(), b"same bytes");
    assert_eq!(std::fs::read(root.join("dup2.txt")).unwrap(), b"same bytes");
    assert!(!root.join("link").exists() && !root.join("ignored.db-wal").exists());
    // The wrong key opens nothing.
    let wrong = Engine::new(
        Store::Dir(
            DirStore::new(
                &std::fs::canonicalize(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/offsite-node-store"
                ))
                .unwrap(),
            )
            .unwrap(),
        ),
        &[8u8; 32],
        vec![],
    )
    .unwrap();
    assert!(
        wrong.snapshots().await.unwrap().is_empty(),
        "every manifest fails authentication and is skipped"
    );
    assert!(wrong.verify(out.path()).await.is_err());
}

#[tokio::test]
async fn retention_forgets_old_snapshots_and_prunes_only_unreferenced_chunks() {
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("f.txt"), "one").unwrap();
    let engine = Engine::new(
        Store::Dir(DirStore::new(dest.path()).unwrap()),
        &[9u8; 32],
        vec![src.path().to_path_buf()],
    )
    .unwrap();
    let first = engine.backup(&Pace::none()).await.unwrap();
    assert!(!first.incomplete && first.files == 1 && first.uploaded_chunks == 1);
    // An orphan chunk (uploaded by a run that never wrote its manifest) is pruned; referenced ones are not.
    std::fs::create_dir_all(dest.path().join("data/ff")).unwrap();
    std::fs::write(
        dest.path().join(format!("data/ff/{}", "f".repeat(64))),
        b"orphan",
    )
    .unwrap();
    // Eleven more snapshots on distinct days, written directly so the test needs no clock.
    let keys = offsite_backup_format::Keys::derive(&[9u8; 32]).unwrap();
    let manifest = |t: i64| offsite_backup_format::Manifest {
        time: t,
        roots: 1,
        files: vec![],
        incomplete: false,
    };
    let t0 = 1_767_268_800_000i64;
    for i in 0..40 {
        let t = t0 + i * 86_400_000;
        let id = offsite_backup_format::snapshot_id(t, [0, 0, 0, i as u8]);
        std::fs::write(
            dest.path().join(format!("snapshots/{id}")),
            keys.seal(manifest(t).to_json().as_bytes(), &[1; 12]),
        )
        .unwrap();
    }
    let kept = engine.forget().await.unwrap();
    // 41 snapshots: the real one (now), and 40 daily ones from 2026-01-01. The newest is always kept.
    assert!(kept.kept < 41 && kept.removed.len() == 41 - kept.kept);
    assert!(kept.pruned_chunks >= 1);
    let listed = files_under(dest.path());
    assert!(
        !listed.iter().any(|f| f.starts_with("data/ff/")),
        "the orphan chunk is gone"
    );
    // The first real snapshot was the newest here, so its one chunk is still referenced.
    assert!(listed.iter().any(|f| f.starts_with("data/")), "{listed:?}");
    let _ = first;
}

#[tokio::test]
async fn a_scheduled_run_waits_out_a_window_that_opens_while_it_runs() {
    use offsite_backup_format::schedule::Zone;
    use std::sync::atomic::AtomicI64;
    let at = |h: i64, m: i64| {
        offsite_backup_format::days_from_civil(2026, 10, 30) * 86_400_000
            + h * 3_600_000
            + m * 60_000
    };
    let clock = Arc::new(AtomicI64::new(at(4, 5)));
    let flips = Arc::new(Mutex::new(Vec::new()));
    let pace = {
        let (c, f) = (Arc::clone(&clock), Arc::clone(&flips));
        Pace {
            zone: Some(Zone::Utc),
            clock: Arc::new(move || c.load(Ordering::SeqCst)),
            step: Duration::from_millis(20),
            waiting: Arc::new(move |w| f.lock().unwrap().push(w)),
        }
    };
    // Outside every window nothing waits.
    clock.store(at(3, 0), Ordering::SeqCst);
    pace.checkpoint().await;
    assert!(
        flips.lock().unwrap().is_empty(),
        "outside every window nothing is announced as waiting"
    );
    // Inside the appdata window (04:05-04:25 on a UTC server): it holds until the clock leaves it.
    clock.store(at(4, 10), Ordering::SeqCst);
    let c2 = Arc::clone(&clock);
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        c2.store(at(4, 26), Ordering::SeqCst);
    });
    let started = Instant::now();
    pace.checkpoint().await;
    release.await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(140),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        *flips.lock().unwrap(),
        [true, false],
        "the status was told it waits, then that it stopped"
    );

    // The same pace inside a real backup: the run does not upload a file while the window is open.
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("f.txt"), "x").unwrap();
    let engine = Arc::new(
        Engine::new(
            Store::Dir(DirStore::new(dest.path()).unwrap()),
            &[9u8; 32],
            vec![src.path().to_path_buf()],
        )
        .unwrap(),
    );
    clock.store(at(4, 10), Ordering::SeqCst);
    let (e, p) = (Arc::clone(&engine), pace.clone());
    let run = tokio::spawn(async move { e.backup(&p).await });
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!run.is_finished(), "held back inside the window");
    assert!(
        files_under(dest.path()).iter().all(|f| f == "config"),
        "only the key check was written"
    );
    clock.store(at(5, 0), Ordering::SeqCst);
    let done = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(done.files, 1);
}

#[tokio::test]
async fn a_database_that_cannot_be_copied_consistently_is_skipped_and_the_snapshot_says_so() {
    let src = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("ok.txt"), "fine").unwrap();
    // Named like a database, but not one: never raw-read into the backup.
    std::fs::write(src.path().join("broken.db"), "this is not a sqlite file").unwrap();
    let engine = Engine::new(
        Store::Dir(DirStore::new(dest.path()).unwrap()),
        &[9u8; 32],
        vec![src.path().to_path_buf()],
    )
    .unwrap();
    let r = engine.backup(&Pace::none()).await.unwrap();
    assert!(r.incomplete);
    assert_eq!(r.files, 1);
    let snaps = engine.snapshots().await.unwrap();
    assert!(snaps[0].incomplete);
    for f in files_under(dest.path()) {
        assert!(
            !String::from_utf8_lossy(&std::fs::read(dest.path().join(&f)).unwrap())
                .contains("this is not a sqlite file")
        );
    }
    // A running backup refuses a second one and retention while it runs.
    let again = Arc::new(engine);
    let big = src.path().join("big.bin");
    std::fs::write(&big, vec![7u8; 6 * 1024 * 1024]).unwrap();
    let (e1, e2) = (Arc::clone(&again), Arc::clone(&again));
    let first = tokio::spawn(async move { e1.backup(&Pace::none()).await });
    let started = Instant::now();
    while !e2.is_running() && !first.is_finished() {
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::task::yield_now().await;
    }
    if e2.is_running() {
        assert_eq!(e2.backup(&Pace::none()).await.err().unwrap().status, 409);
        assert_eq!(e2.forget().await.err().unwrap().status, 409);
    }
    first.await.unwrap().unwrap();
}

#[test]
fn a_device_token_is_refused_on_every_route_the_front_took() {
    // core's device-auth scope (BROWSER_ONLY) travels with the routes: administration and the
    // Connectors writes need a signed-in browser, whoever answers them.
    for (m, p) in [
        ("GET", "/api/admin/offsite-backup"),
        ("POST", "/api/admin/offsite-backup/run"),
        ("POST", "/api/admin/offsite-backup/verify"),
        ("POST", "/api/admin/offsite-backup/copy"),
        ("POST", "/api/admin/offsite-backup/google/connect"),
        ("POST", "/api/admin/offsite-backup/google/disconnect"),
        ("GET", "/api/admin/offsite-backup/recovery-key"),
        ("POST", "/api/connectors/gdrive/connect"),
        ("POST", "/api/connectors/gdrive/disconnect"),
        ("PUT", "/api/connectors/gdrive/policy"),
        ("PUT", "/api/connectors/gdrive/backup-copy"),
    ] {
        assert!(server_auth::identity::browser_only_path(p, m), "{m} {p}");
    }
}

/// A synthetic S3-compatible service (path-style) on 127.0.0.1: a map of objects, ListObjectsV2
/// with a small page size so the continuation is exercised, and a record of every Authorization.
fn install_s3(
    up: &Upstream,
    objects: Arc<Mutex<std::collections::BTreeMap<String, Vec<u8>>>>,
    page: usize,
) {
    up.set(move |r: &Seen| {
        let url = url::Url::parse(&format!("http://x{}", r.path)).unwrap();
        let q: std::collections::HashMap<String, String> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        let mut store = objects.lock().unwrap();
        let path = url.path().to_string();
        match r.method.as_str() {
            "PUT" => {
                store.insert(path, r.bytes.clone());
                reply("200 OK", "")
            }
            "DELETE" => {
                store.remove(&path);
                (Duration::ZERO, b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec())
            }
            "GET" if q.contains_key("list-type") => {
                let prefix = format!("/b/{}", q.get("prefix").cloned().unwrap_or_default());
                let keys: Vec<&String> = store.keys().filter(|k| k.starts_with(&prefix)).collect();
                let start = q.get("continuation-token").and_then(|t| t.parse::<usize>().ok()).unwrap_or(0);
                let slice: Vec<&&String> = keys.iter().skip(start).take(page).collect();
                let truncated = start + page < keys.len();
                let mut xml = String::from("<?xml version=\"1.0\"?><ListBucketResult>");
                for k in slice {
                    // The bucket is the first path segment; keys come back without it, as S3 does.
                    xml.push_str(&format!("<Contents><Key>{}</Key></Contents>", k.trim_start_matches("/b/").replace('&', "&amp;")));
                }
                xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
                if truncated {
                    xml.push_str(&format!("<NextContinuationToken>{}</NextContinuationToken>", start + page));
                }
                xml.push_str("</ListBucketResult>");
                (Duration::ZERO, format!("HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}", xml.len()).into_bytes())
            }
            "GET" => match store.get(&path) {
                Some(b) => bytes_reply(b),
                None => (Duration::ZERO, b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()),
            },
            _ => reply("405 No", ""),
        }
    });
}

#[tokio::test]
async fn an_s3_compatible_destination_signs_lists_in_pages_and_restores() {
    let data = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let key = outside.path().join("backup.key");
    std::fs::write(&key, BACKUP_KEY_HEX).unwrap();
    seed(data.path());
    std::fs::write(data.path().join("notes.txt"), "synthetic notes").unwrap();
    let up = upstream().await;
    let objects = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    install_s3(&up, Arc::clone(&objects), 2);
    let env = [
        ("OFFSITE_BACKUP_S3_ENDPOINT", up.base()),
        ("OFFSITE_BACKUP_S3_BUCKET", "b".to_string()),
        (
            "OFFSITE_BACKUP_S3_ACCESS_KEY_ID",
            "AKIASYNTHETIC".to_string(),
        ),
        (
            "OFFSITE_BACKUP_S3_SECRET_ACCESS_KEY",
            "synthetic-s3-secret-do-not-log".to_string(),
        ),
        ("OFFSITE_BACKUP_S3_PREFIX", "/noevia//backup/".to_string()),
        ("OFFSITE_BACKUP_KEY_FILE", key.display().to_string()),
        ("NOEVIA_FEATURE_OFFSITE_BACKUP", "1".to_string()),
    ];
    let f = front(data.path(), &up.base(), &env).await;
    let s = get(f.addr, ROOT, ADMIN).await.json();
    assert_eq!(s["ready"], true, "{s}");
    assert_eq!(s["destination"], format!("{} / b", up.addr));
    assert!(s["destinationFolder"].is_null() && s["google"].is_null());
    let r = send(f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "").await;
    assert_eq!(r.status, 200, "{}", r.text);
    let stored: Vec<String> = objects.lock().unwrap().keys().cloned().collect();
    assert!(
        stored.iter().all(|k| k.starts_with("/b/noevia/backup/")),
        "the prefix is collapsed and trimmed: {stored:?}"
    );
    assert!(
        stored.iter().any(|k| k.ends_with("/config"))
            && stored.iter().any(|k| k.contains("/snapshots/"))
            && stored.iter().filter(|k| k.contains("/data/")).count() >= 3
    );
    // Every request is signed (SigV4), over a payload hash, with the access key id and never the secret.
    for req in up.seen() {
        assert!(
            req.headers
                .contains("authorization: aws4-hmac-sha256 credential=akiasynthetic/"),
            "{} {}",
            req.method,
            req.path
        );
        assert!(
            req.headers.contains("x-amz-content-sha256:") && req.headers.contains("x-amz-date:")
        );
        assert!(
            !req.headers.contains("synthetic-s3-secret")
                && !req.path.contains("synthetic-s3-secret")
        );
    }
    // The listing came in pages of two, and a second run found every chunk already there (dedup by listing).
    assert!(
        up.seen()
            .iter()
            .any(|r| r.path.contains("continuation-token=")),
        "paged"
    );
    let puts = up.seen().iter().filter(|r| r.method == "PUT").count();
    let again = send(f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "")
        .await
        .json();
    // (The database and the status file changed between the runs; the notes and the key did not.)
    let new_puts = up.seen().iter().filter(|r| r.method == "PUT").count() - puts;
    assert!(
        new_puts < puts,
        "only a new manifest and what changed was sent: {new_puts} of {puts}; {again}"
    );
    assert_eq!(
        send(f.addr, "POST", &format!("{ROOT}/verify"), ADMIN, "")
            .await
            .status,
        200
    );
    // A destination that refuses writes fails the run with core's words and no secret.
    up.set(|r: &Seen| {
        if r.method == "GET" && r.path.contains("list-type") {
            reply("200 OK", "<ListBucketResult></ListBucketResult>")
        } else {
            reply("403 Forbidden", "")
        }
    });
    let r = send(f.addr, "POST", &format!("{ROOT}/run"), ADMIN, "").await;
    assert_eq!(r.status, 502);
    assert!(
        r.text.contains("The backup destination refused"),
        "{}",
        r.text
    );
    assert!(!r.text.contains("synthetic-s3-secret") && !r.text.contains("AKIASYNTHETIC"));
    // A plain-http remote endpoint is refused before any request leaves.
    let d2 = tempfile::tempdir().unwrap();
    seed(d2.path());
    let before = up.seen().len();
    let f2 = front(
        d2.path(),
        &up.base(),
        &[
            (
                "OFFSITE_BACKUP_S3_ENDPOINT",
                "http://s3.example.test".into(),
            ),
            ("OFFSITE_BACKUP_S3_BUCKET", "b".into()),
            ("OFFSITE_BACKUP_S3_ACCESS_KEY_ID", "a".into()),
            ("OFFSITE_BACKUP_S3_SECRET_ACCESS_KEY", "s".into()),
            ("OFFSITE_BACKUP_KEY_FILE", key.display().to_string()),
            ("NOEVIA_FEATURE_OFFSITE_BACKUP", "1".into()),
        ],
    )
    .await;
    let r = send(f2.addr, "POST", &format!("{ROOT}/run"), ADMIN, "").await;
    assert_eq!(
        (r.status, r.json()["error"].as_str().map(str::to_owned)),
        (409, Some("Offsite backup endpoints must use HTTPS.".into()))
    );
    assert_eq!(up.seen().len(), before);
}
