//! Row V fix round 3: the one-time, ownership-checked takeover of the key the retired Node issuer left
//! published. Node kept no private issuer state (the key is HMAC(master, random nonce), the nonce was
//! never stored and the session lived in memory), so the published key file is the only thing to take
//! over. `fixtures/node-published-key.json` was written by the real Node publisher (Node 22, a synthetic
//! temporary directory, noevia-core `tests/reference/code-egress-legacy-publication.cjs`); the optional
//! test at the end imports a directory the real Node code wrote during the run.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod support;
use egress_issuer::files::{
    key_file_state, load_state, read_legacy_key, IssuerFiles, KeyKeeper, KeyState, Opened,
};
use egress_issuer::testing::FakeProxy;
use egress_issuer::wire::{hex, unhex};
use egress_issuer::{Client, ClientConfig, ControlError, Reason, SystemClock};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use support::ProxyTransport;

const FIXTURE: &str = include_str!("fixtures/node-published-key.json");

struct Node {
    key: [u8; 32],
    file_bytes: Vec<u8>,
}

fn node_fixture() -> Node {
    let doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let unhex_bytes = |s: &str| -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    };
    // What Node's publisher recorded about its own file: the contract the takeover relies on.
    assert_eq!(doc["fileMode"], "440");
    assert_eq!(doc["dirMode"], "750");
    assert_eq!(doc["nlink"], 1);
    Node {
        key: unhex::<32>(doc["keyHex"].as_str().unwrap()).unwrap(),
        file_bytes: unhex_bytes(doc["fileHex"].as_str().unwrap()),
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    keydir: PathBuf,
    key_file: PathBuf,
    state_file: PathBuf,
    files: IssuerFiles,
    uid: u32,
    gid: u32,
}

/// The deploy layout: a 0750 key directory of the web uid/gid with the private state in a directory of
/// its own inside it (created by the issuer).
fn rig() -> Option<Rig> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    if gid == 0 {
        eprintln!("skipped: the 0440 key needs a non-root group");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let keydir = root.join("egress-control");
    std::fs::create_dir(&keydir).unwrap();
    std::os::unix::fs::chown(&keydir, Some(uid), Some(gid)).unwrap();
    std::fs::set_permissions(&keydir, std::fs::Permissions::from_mode(0o750)).unwrap();
    let key_file = keydir.join("current.key");
    let state_file = keydir.join("issuer").join("code-egress-issuer.json");
    let files = IssuerFiles {
        key_file: key_file.clone(),
        uid,
        gid,
        state_file: state_file.clone(),
    };
    Some(Rig {
        _dir: dir,
        keydir,
        key_file,
        state_file,
        files,
        uid,
        gid,
    })
}

fn master() -> egress_issuer::files::Master {
    Arc::new(|| Ok([7u8; 32]))
}
fn os_rng() -> egress_issuer::files::Rng {
    Arc::new(|b: &mut [u8]| {
        use rand_core::{OsRng, RngCore};
        OsRng.fill_bytes(b);
    })
}
fn open(r: &Rig) -> Result<(KeyKeeper, Opened), ControlError> {
    KeyKeeper::open(r.files.clone(), master(), os_rng())
}
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().mode() & 0o7777
}

/// Lays down the bytes Node wrote, with the metadata Node gave them (uid/gid of the web, 0440).
fn lay(r: &Rig, bytes: &[u8]) {
    let _ = std::fs::remove_file(&r.key_file);
    std::fs::write(&r.key_file, bytes).unwrap();
    std::os::unix::fs::chown(&r.key_file, Some(r.uid), Some(r.gid)).unwrap();
    std::fs::set_permissions(&r.key_file, std::fs::Permissions::from_mode(0o440)).unwrap();
}

/// Everything about the published file Node could notice changing.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    bytes: Vec<u8>,
    ino: u64,
    mode: u32,
    nlink: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    uid: u32,
    gid: u32,
    keydir_names: Vec<String>,
    keydir_mode: u32,
}
fn snapshot(r: &Rig) -> Snapshot {
    let m = std::fs::symlink_metadata(&r.key_file).unwrap();
    let mut names: Vec<String> = std::fs::read_dir(&r.keydir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Snapshot {
        bytes: std::fs::read(&r.key_file).unwrap(),
        ino: m.ino(),
        mode: m.mode() & 0o7777,
        nlink: m.nlink(),
        mtime: (m.mtime(), m.mtime_nsec()),
        ctime: (m.ctime(), m.ctime_nsec()),
        uid: m.uid(),
        gid: m.gid(),
        keydir_names: names,
        keydir_mode: mode(&r.keydir),
    }
}

#[test]
fn the_node_published_key_is_taken_over_unchanged_and_node_s_files_are_not_touched() {
    let Some(r) = rig() else { return };
    let node = node_fixture();
    lay(&r, &node.file_bytes);
    let before = snapshot(&r);
    assert_eq!(before.keydir_names, ["current.key"]);

    let (keeper, how) = open(&r).unwrap();
    assert_eq!(how, Opened::Adopted);
    assert_eq!(
        keeper.key(),
        node.key,
        "the same key is served: no rotation"
    );
    // Node's file is exactly what it was: bytes, inode, mode, owner, times (not even rewritten).
    let after = snapshot(&r);
    assert_eq!(after.bytes, before.bytes);
    assert_eq!(
        (after.ino, after.mode, after.nlink, after.uid, after.gid),
        (before.ino, 0o440, 1, r.uid, r.gid)
    );
    assert_eq!((after.mtime, after.ctime), (before.mtime, before.ctime));
    assert_eq!(after.keydir_mode, 0o750);
    // The only addition is the issuer's own 0700 directory with its 0600 state, holding that key.
    assert_eq!(after.keydir_names, ["current.key", "issuer"]);
    assert_eq!(mode(r.state_file.parent().unwrap()), 0o700);
    assert_eq!(mode(&r.state_file), 0o600);
    let state = load_state(&r.state_file).unwrap().unwrap();
    assert_eq!(state.key, Some(node.key));
    assert_eq!((state.next, state.epoch), (None, None));
    assert_eq!(key_file_state(&r.key_file, &node.key), KeyState::Own);
    assert!(std::fs::read_dir(r.state_file.parent().unwrap())
        .unwrap()
        .flatten()
        .all(|e| e.file_name() == "code-egress-issuer.json"));
}

#[test]
fn a_second_start_is_idempotent_and_survives_losing_the_private_state() {
    let Some(r) = rig() else { return };
    let node = node_fixture();
    lay(&r, &node.file_bytes);
    let (first, how) = open(&r).unwrap();
    assert_eq!((how, first.key()), (Opened::Adopted, node.key));
    drop(first);
    let snap = snapshot(&r);
    let state_bytes = std::fs::read(&r.state_file).unwrap();
    for _ in 0..3 {
        let (again, how) = open(&r).unwrap();
        assert_eq!(how, Opened::Kept, "a restart, not another import");
        assert_eq!(again.key(), node.key);
        assert_eq!(
            snapshot(&r),
            snap,
            "Node's file is untouched by every start"
        );
        assert_eq!(std::fs::read(&r.state_file).unwrap(), state_bytes);
    }
    // The private state is lost (a new volume) while the published key stays: taken over again, with
    // the same key; never a rotation and never a refusal.
    std::fs::remove_file(&r.state_file).unwrap();
    let (back, how) = open(&r).unwrap();
    assert_eq!((how, back.key()), (Opened::Adopted, node.key));
    assert_eq!(snapshot(&r).bytes, snap.bytes);
}

#[test]
fn an_interrupted_adoption_leaves_no_state_and_the_next_start_adopts() {
    let Some(r) = rig() else { return };
    let node = node_fixture();
    lay(&r, &node.file_bytes);
    // The state directory exists but the process died before the state was written.
    egress_issuer::files::prepare_state_dir(&r.state_file).unwrap();
    assert!(!r.state_file.exists());
    let (keeper, how) = open(&r).unwrap();
    assert_eq!((how, keeper.key()), (Opened::Adopted, node.key));
}

#[tokio::test]
async fn a_takeover_serves_the_same_key_to_the_proxy_and_a_later_rotation_is_node_compatible() {
    let Some(r) = rig() else { return };
    let node = node_fixture();
    lay(&r, &node.file_bytes);
    let (keeper, how) = open(&r).unwrap();
    assert_eq!(how, Opened::Adopted);
    let proxy = FakeProxy::new(r.key_file.clone());
    let persisted = keeper.persisted_epoch();
    let mut config = ClientConfig::new("egress-rs-code:8040", 6 * 3600 * 1000, 7_200_000);
    config.auto_renew = false;
    let client = Client::new(
        config,
        keeper.key(),
        Some(keeper),
        persisted,
        Arc::new(SystemClock::new()),
        Arc::new(ProxyTransport(Arc::clone(&proxy))),
        Arc::new(|_, _| {}),
    )
    .unwrap();
    // A proxy that never saw Node's Ready (restarted since) reads the key Node published, signs with
    // it, and the issuer is Ready on it without a rotation. A proxy Node already readied under that key
    // refuses Initial and gets one immediate rotation instead: tests/adopted_proxy.rs.
    client.ready().await.unwrap();
    assert!(proxy.session().is_some());
    assert_eq!(hex(proxy.key().as_bytes()), hex(&node.key));
    let _ = std::fs::read(&r.key_file).unwrap();
}

/// Each way a key file can fail the takeover checks is refused by its own named reason, nothing is
/// written anywhere (no state, no new key), and Node's file and directory are exactly as they were.
#[test]
fn a_published_key_that_fails_a_check_is_refused_by_name_and_nothing_changes() {
    let Some(r) = rig() else { return };
    let node = node_fixture();
    let text = String::from_utf8(node.file_bytes.clone()).unwrap();
    let check = |r: &Rig, reason: Reason| {
        let before = snapshot(r);
        let err = open(r).unwrap_err();
        assert_eq!(err.0, reason, "{}", reason.as_str());
        assert!(
            reason.as_str().starts_with("legacy_key_"),
            "the log line names the check"
        );
        assert_eq!(read_legacy_key(&r.files).unwrap_err().0, reason);
        assert_eq!(
            snapshot(r).bytes,
            before.bytes,
            "the published key is never overwritten"
        );
        assert_eq!(
            (snapshot(r).ino, snapshot(r).mode),
            (before.ino, before.mode)
        );
        assert!(!r.state_file.exists(), "no state for a refused takeover");
        assert!(
            !r.keydir.join("issuer").exists(),
            "nothing is created for it"
        );
    };
    // content
    for bad in [
        text.to_uppercase(),                       // upper-case hex
        text.trim_end().to_string(),               // no LF
        text.replace('\n', "\r\n"),                // CRLF
        format!("{text}{text}"),                   // more than one line
        text[..text.len() - 2].to_string() + "\n", // 63 digits
        text.replace(&text[..1], "g"),             // not hex
        String::new(),                             // empty
    ] {
        lay(&r, bad.as_bytes());
        check(&r, Reason::LegacyKeyContent);
    }
    // mode of the file: anything but exactly 0440
    for m in [0o400, 0o444, 0o640, 0o660, 0o600, 0o441] {
        lay(&r, text.as_bytes());
        std::fs::set_permissions(&r.key_file, std::fs::Permissions::from_mode(m)).unwrap();
        check(&r, Reason::LegacyKeyMode);
    }
    // an extra link
    lay(&r, text.as_bytes());
    let alias = r.keydir.join("alias");
    std::fs::hard_link(&r.key_file, &alias).unwrap();
    check(&r, Reason::LegacyKeyLinks);
    std::fs::remove_file(&alias).unwrap();
    // not a regular file: a symlink, a FIFO (never opened blocking), a directory
    let target = r.keydir.parent().unwrap().join("elsewhere.key");
    std::fs::write(&target, text.as_bytes()).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o440)).unwrap();
    std::fs::remove_file(&r.key_file).unwrap();
    std::os::unix::fs::symlink(&target, &r.key_file).unwrap();
    let before = std::fs::read_link(&r.key_file).unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::LegacyKeyFile);
    assert_eq!(std::fs::read_link(&r.key_file).unwrap(), before);
    std::fs::remove_file(&r.key_file).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &r.key_file,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o440),
        0,
    )
    .unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::LegacyKeyFile);
    std::fs::remove_file(&r.key_file).unwrap();
    std::fs::create_dir(&r.key_file).unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::LegacyKeyFile);
    std::fs::remove_dir(&r.key_file).unwrap();
    assert!(!r.state_file.exists());
    // the directory: group/other-writable, or without group read/search (not a 0750-class directory)
    lay(&r, text.as_bytes());
    for m in [0o770, 0o757, 0o700, 0o777, 0o730] {
        std::fs::set_permissions(&r.keydir, std::fs::Permissions::from_mode(m)).unwrap();
        let err = open(&r).unwrap_err();
        assert_eq!(err.0, Reason::LegacyKeyDirMode, "{m:o}");
        assert_eq!(mode(&r.keydir), m, "the directory mode is never repaired");
        assert!(!r.keydir.join("issuer").exists());
    }
    std::fs::set_permissions(&r.keydir, std::fs::Permissions::from_mode(0o750)).unwrap();
    // the directory reached through a link
    let link = r.keydir.parent().unwrap().join("via-link");
    std::os::unix::fs::symlink(&r.keydir, &link).unwrap();
    let via = IssuerFiles {
        key_file: link.join("current.key"),
        state_file: r.keydir.parent().unwrap().join("state").join("s.json"),
        ..r.files.clone()
    };
    assert_eq!(
        read_legacy_key(&via).unwrap_err().0,
        Reason::LegacyKeyDirectory
    );
    // the wrong owner: a different configured group (the directory does not belong to it), a different
    // configured uid (this process is not the web user), a relative or non-canonical path
    let other_group = IssuerFiles {
        gid: r.gid + 1,
        ..r.files.clone()
    };
    assert_eq!(
        read_legacy_key(&other_group).unwrap_err().0,
        Reason::LegacyKeyDirOwner
    );
    let other_user = IssuerFiles {
        uid: r.uid + 1,
        ..r.files.clone()
    };
    assert_eq!(
        read_legacy_key(&other_user).unwrap_err().0,
        Reason::LegacyKeyUser
    );
    let relative = IssuerFiles {
        key_file: PathBuf::from("current.key"),
        ..r.files.clone()
    };
    assert_eq!(
        read_legacy_key(&relative).unwrap_err().0,
        Reason::LegacyKeyPath
    );
    // after all of that Node's file is still the only thing there
    assert_eq!(std::fs::read(&r.key_file).unwrap(), text.as_bytes());
    assert_eq!(
        key_file_state(&r.key_file, &node.key),
        KeyState::Own,
        "a valid file adopted once the layout is right again"
    );
    assert_eq!(open(&r).unwrap().1, Opened::Adopted);
}

/// A file owned by another group of the same user: only possible when the user has one.
#[test]
fn a_key_file_of_another_group_is_refused_as_the_wrong_owner() {
    let Some(r) = rig() else { return };
    let groups = rustix::process::getgroups().unwrap_or_default();
    let Some(other) = groups.iter().map(|g| g.as_raw()).find(|g| *g != r.gid) else {
        eprintln!("skipped: the user has no second group");
        return;
    };
    lay(&r, &node_fixture().file_bytes);
    std::os::unix::fs::chown(&r.key_file, Some(r.uid), Some(other)).unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::LegacyKeyOwner);
    assert_eq!(
        std::fs::metadata(&r.key_file).unwrap().gid(),
        other,
        "ownership is never repaired"
    );
    assert!(!r.state_file.exists());
}

#[test]
fn with_no_state_and_no_published_key_a_first_start_publishes_as_before() {
    let Some(r) = rig() else { return };
    let (keeper, how) = open(&r).unwrap();
    assert_eq!(how, Opened::Fresh);
    assert_eq!(mode(&r.key_file), 0o440);
    assert_eq!(
        std::fs::read_to_string(&r.key_file).unwrap().trim(),
        hex(&keeper.key())
    );
    assert_eq!(read_legacy_key(&r.files).unwrap(), Some(keeper.key()));
}

#[test]
fn a_published_key_that_matches_neither_the_state_nor_a_candidate_is_never_overwritten() {
    let Some(r) = rig() else { return };
    let node = node_fixture();
    lay(&r, &node.file_bytes);
    let (keeper, _) = open(&r).unwrap();
    drop(keeper);
    // Somebody publishes another key after the takeover: superseded, nothing is written over it.
    let other = format!("{}\n", "ab".repeat(32));
    lay(&r, other.as_bytes());
    let snap = snapshot(&r);
    let (mut again, how) = open(&r).unwrap();
    assert_eq!(how, Opened::Superseded);
    assert_eq!(again.key(), node.key);
    assert_eq!(again.restore(), "superseded");
    assert!(again.rekey().is_none());
    assert_eq!(snapshot(&r), snap);
    // A pending candidate of ours that is not what is published: refused, and the key is untouched.
    drop(again);
    std::fs::remove_file(&r.state_file).unwrap();
    egress_issuer::files::save_state(
        &r.state_file,
        &egress_issuer::files::PrivateState {
            key: None,
            next: Some([0x42u8; 32]),
            epoch: None,
        },
    )
    .unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::Publication);
    assert_eq!(snapshot(&r), snap);
}

#[test]
fn the_state_directory_is_created_only_inside_a_directory_of_the_issuers_own() {
    let Some(r) = rig() else { return };
    // The key directory is writable by others (not a trusted holder): nothing is created inside it.
    std::fs::set_permissions(&r.keydir, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(egress_issuer::files::prepare_state_dir(&r.state_file).is_err());
    assert!(!r.keydir.join("issuer").exists());
    // An absent holder is not created (no mkdir -p).
    let deep = r.keydir.join("a").join("b").join("state.json");
    assert!(egress_issuer::files::prepare_state_dir(&deep).is_err());
    assert!(!r.keydir.join("a").exists());
    std::fs::set_permissions(&r.keydir, std::fs::Permissions::from_mode(0o750)).unwrap();
    egress_issuer::files::prepare_state_dir(&r.state_file).unwrap();
    assert_eq!(mode(r.state_file.parent().unwrap()), 0o700);
    // Idempotent; an existing directory is validated by the state code, not by this.
    egress_issuer::files::prepare_state_dir(&r.state_file).unwrap();
}

/// Optional, run by the evidence script: `EGRESS_NODE_DIR` is a directory the real Node code published
/// into during this run (`node code-egress-legacy-publication.cjs --dir <dir>` with its json printed to
/// `EGRESS_NODE_DOC`). The takeover must serve exactly that key and leave the directory as Node left it.
#[test]
fn a_directory_published_by_the_real_node_code_during_this_run_is_taken_over() {
    let (Some(dir), Some(doc)) = (
        std::env::var_os("EGRESS_NODE_DIR"),
        std::env::var_os("EGRESS_NODE_DOC"),
    ) else {
        eprintln!("skipped: EGRESS_NODE_DIR / EGRESS_NODE_DOC not set");
        return;
    };
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(doc).unwrap()).unwrap();
    let want = unhex::<32>(doc["keyHex"].as_str().unwrap()).unwrap();
    let keydir = PathBuf::from(dir);
    let key_file = keydir.join("current.key");
    let files = IssuerFiles {
        key_file: key_file.clone(),
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        state_file: keydir.join("issuer").join("code-egress-issuer.json"),
    };
    let before = (
        std::fs::read(&key_file).unwrap(),
        std::fs::metadata(&key_file).unwrap().ino(),
    );
    let (keeper, how) = KeyKeeper::open(files.clone(), master(), os_rng()).unwrap();
    assert_eq!((how, keeper.key()), (Opened::Adopted, want));
    drop(keeper);
    let (again, how) = KeyKeeper::open(files, master(), os_rng()).unwrap();
    assert_eq!((how, again.key()), (Opened::Kept, want));
    assert_eq!(
        (
            std::fs::read(&key_file).unwrap(),
            std::fs::metadata(&key_file).unwrap().ino()
        ),
        before
    );
}

// ---- the private-state directory is held to its rule when it already exists ----

/// An `issuer/` that was pre-created, restored from a backup, or loosened later is validated exactly as
/// strictly as one the issuer creates: a real directory, the issuer's own, mode 0700 with no group or
/// other bit at all (a process with the proxy's supplementary group must not be able to enter it).
/// Anything else is refused by name, never repaired, and nothing in it or beside it changes.
#[test]
fn an_existing_issuer_directory_is_held_to_0700_and_never_repaired() {
    let Some(r) = rig() else { return };
    let node = node_fixture();
    lay(&r, &node.file_bytes);
    let issuer = r.keydir.join("issuer");
    std::fs::create_dir(&issuer).unwrap();
    for m in [
        0o710, 0o750, 0o701, 0o755, 0o770, 0o777, 0o500, 0o600, 0o2700, 0o1700,
    ] {
        std::fs::set_permissions(&issuer, std::fs::Permissions::from_mode(m)).unwrap();
        let before = snapshot(&r);
        let err = open(&r).unwrap_err();
        assert_eq!(err.0, Reason::StateDirMode, "{m:o}");
        assert!(
            err.0.as_str().starts_with("state_directory_"),
            "named in the log"
        );
        assert_eq!(mode(&issuer), m, "the mode is never repaired ({m:o})");
        assert!(
            !r.state_file.exists(),
            "no state written for a refused directory ({m:o})"
        );
        assert_eq!(snapshot(&r).bytes, before.bytes);
        assert_eq!(
            (snapshot(&r).ino, snapshot(&r).ctime),
            (before.ino, before.ctime),
            "Node's key file is untouched"
        );
        assert!(egress_issuer::files::load_state(&r.state_file).is_err());
    }
    // Exactly 0700 is accepted and the key is taken over.
    std::fs::set_permissions(&issuer, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (keeper, how) = open(&r).unwrap();
    assert_eq!((how, keeper.key()), (Opened::Adopted, node.key));
    assert_eq!(mode(&issuer), 0o700);
}

#[test]
fn an_issuer_directory_loosened_after_start_is_refused_on_the_next_write_and_start() {
    let Some(r) = rig() else { return };
    lay(&r, &node_fixture().file_bytes);
    let (mut keeper, how) = open(&r).unwrap();
    assert_eq!(how, Opened::Adopted);
    let issuer = r.keydir.join("issuer");
    let state_before = std::fs::read(&r.state_file).unwrap();
    for m in [0o750, 0o755, 0o710, 0o701] {
        std::fs::set_permissions(&issuer, std::fs::Permissions::from_mode(m)).unwrap();
        keeper.save_epoch(Some((support::BOOT.to_string(), "ab".repeat(16))));
        assert!(
            keeper.persisted_epoch().is_none(),
            "a refused save is not remembered ({m:o})"
        );
        assert_eq!(open(&r).unwrap_err().0, Reason::StateDirMode, "{m:o}");
        assert_eq!(mode(&issuer), m, "not repaired");
        assert_eq!(std::fs::read(&r.state_file).unwrap(), state_before);
    }
    std::fs::set_permissions(&issuer, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(open(&r).unwrap().1, Opened::Kept);
}

#[test]
fn an_issuer_directory_that_is_a_link_or_not_a_directory_is_refused() {
    let Some(r) = rig() else { return };
    lay(&r, &node_fixture().file_bytes);
    let issuer = r.keydir.join("issuer");
    // A symlink to a perfectly good 0700 directory: never followed.
    let real = r.keydir.parent().unwrap().join("real-state");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&real, &issuer).unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::StateDirNotDirectory);
    assert!(std::fs::symlink_metadata(&issuer)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(
        std::fs::read_dir(&real).unwrap().next().is_none(),
        "nothing written through it"
    );
    // A dangling link and a regular file in its place: refused as well, left as they are.
    std::fs::remove_file(&issuer).unwrap();
    std::os::unix::fs::symlink(r.keydir.join("missing"), &issuer).unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::StateDirNotDirectory);
    std::fs::remove_file(&issuer).unwrap();
    std::fs::write(&issuer, b"x").unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::StateDirNotDirectory);
    assert_eq!(std::fs::read(&issuer).unwrap(), b"x");
    assert_eq!(read_legacy_key(&r.files).unwrap(), Some(node_fixture().key));
}

/// A state directory of another uid (here `/tmp`, root's, or any directory this user does not own):
/// refused by name before anything is read or written.
#[test]
fn an_issuer_directory_owned_by_another_uid_is_refused() {
    let Some(r) = rig() else { return };
    if r.uid == 0 {
        eprintln!("skipped: root owns /tmp");
        return;
    }
    lay(&r, &node_fixture().file_bytes);
    let snap = snapshot(&r);
    for foreign in [
        PathBuf::from("/tmp/egress-issuer-r3.json"),
        PathBuf::from("/usr/egress-issuer-r3.json"),
    ] {
        let files = IssuerFiles {
            state_file: foreign.clone(),
            ..r.files.clone()
        };
        let err = KeyKeeper::open(files, master(), os_rng()).unwrap_err();
        assert_eq!(err.0, Reason::StateDirOwner, "{foreign:?}");
        assert!(!foreign.exists());
    }
    assert_eq!(
        snapshot(&r),
        snap,
        "Node's files and directory are unchanged"
    );
}
