//! The takeover against a proxy the retired Node issuer already did its Ready on. The live proxy
//! (bins/egress-proxy engine.rs) accepts Initial only while it is not initialized under the published
//! key; only a key change resets that, session expiry does not. Node's Ready ran under the very key the
//! front adopts, so the front's first Initial is refused (signed 409) and it has no session of its own
//! to Replace. It must rotate once, at once (no 25 s conflict wait), and be ready. A restart on its own
//! persisted epoch must not rotate. Synthetic key directory and fixture only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod support;
use egress::control::{derive_epoch_keys, FileKey};
use egress_issuer::client::Log;
use egress_issuer::files::{load_state, IssuerFiles, KeyKeeper, Opened};
use egress_issuer::testing::{FakeProxy, NODE_SESSION};
use egress_issuer::wire::{hex, unhex};
use egress_issuer::{Client, ClientConfig, ControlError, Reason, SystemClock};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{ProxyTransport, BOOT};

const FIXTURE: &str = include_str!("fixtures/node-published-key.json");
/// Far inside the 25 s (SESSION_MS + RPC_MS) wait the conflict path would have taken.
const PROMPT: Duration = Duration::from_secs(5);

struct Rig {
    _dir: tempfile::TempDir,
    keydir: PathBuf,
    key_file: PathBuf,
    state_file: PathBuf,
    files: IssuerFiles,
    uid: u32,
    gid: u32,
}

/// The deploy layout: a 0750 key directory of the web uid/gid, the private state in `issuer/` inside it.
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

/// Lays down the bytes the real Node publisher wrote, with the metadata it gave them.
fn lay_node_key(r: &Rig) -> [u8; 32] {
    let doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let key = unhex::<32>(doc["keyHex"].as_str().unwrap()).unwrap();
    std::fs::write(&r.key_file, format!("{}\n", hex(&key))).unwrap();
    std::os::unix::fs::chown(&r.key_file, Some(r.uid), Some(r.gid)).unwrap();
    std::fs::set_permissions(&r.key_file, std::fs::Permissions::from_mode(0o440)).unwrap();
    key
}

fn open(r: &Rig) -> Result<(KeyKeeper, Opened), ControlError> {
    KeyKeeper::open(
        r.files.clone(),
        Arc::new(|| Ok([7u8; 32])),
        Arc::new(|b: &mut [u8]| {
            use rand_core::{OsRng, RngCore};
            OsRng.fill_bytes(b);
        }),
    )
}

type Events = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

fn client(keeper: KeyKeeper, proxy: &Arc<FakeProxy>) -> (Client, Events) {
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let log: Log = Arc::new(move |event, fields| {
        sink.lock().unwrap().push((
            event.to_string(),
            fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        ));
    });
    let mut config = ClientConfig::new("egress-rs-code:8040", 6 * 3600 * 1000, 7_200_000);
    config.auto_renew = false;
    let persisted = keeper.persisted_epoch();
    let c = Client::new(
        config,
        keeper.key(),
        Some(keeper),
        persisted,
        Arc::new(SystemClock::new()),
        Arc::new(ProxyTransport(Arc::clone(proxy))),
        log,
    )
    .unwrap();
    (c, events)
}

fn count(events: &Events, name: &str) -> usize {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|(e, _)| e == name)
        .count()
}

fn published(r: &Rig) -> [u8; 32] {
    unhex::<32>(std::fs::read_to_string(&r.key_file).unwrap().trim()).unwrap()
}

fn names(dir: &Path) -> Vec<String> {
    let mut n: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    n.sort();
    n
}

/// The proxy's own check of a token, from the key it reads now.
fn proxy_accepts(proxy: &FakeProxy, session: &str, token: &str) -> bool {
    let keys = derive_epoch_keys(
        &proxy.key(),
        &unhex::<16>(BOOT).unwrap(),
        &unhex::<16>(session).unwrap(),
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    egress::verify(&keys.grant, token.as_bytes(), now).is_ok()
}

/// Brings up the takeover against a Node-initialized proxy and checks it rotated exactly once, at once.
async fn take_over(r: &Rig, node: [u8; 32]) -> (Arc<FakeProxy>, Client, Events, [u8; 32]) {
    let ino_before = std::fs::metadata(&r.key_file).unwrap().ino();
    let (keeper, how) = open(r).unwrap();
    assert_eq!((how, keeper.key()), (Opened::Adopted, node));
    assert_eq!(published(r), node, "adoption itself writes only the state");
    let proxy = FakeProxy::initialized_by_node(r.key_file.clone());
    let (c, events) = client(keeper, &proxy);

    let started = Instant::now();
    tokio::time::timeout(PROMPT, c.ready())
        .await
        .expect("ready without the conflict wait")
        .expect("ready on the first attempt");
    assert!(started.elapsed() < PROMPT);
    assert!(c.available());

    // Exactly one rotation, no conflict, no outage.
    assert_eq!(count(&events, "egress.control_rekeyed"), 1);
    assert!(events.lock().unwrap().iter().any(|(e, f)| {
        e == "egress.control_rekeyed"
            && f == &[("cause".to_string(), "unnamed_session".to_string())]
    }));
    assert_eq!(count(&events, "egress.control_conflict"), 0);
    assert_eq!(count(&events, "egress.control_rekey_refused"), 0);
    assert_eq!(count(&events, "egress.control_unavailable"), 0);
    assert_eq!(count(&events, "egress.control_ready"), 1);
    let session = {
        let st = proxy.state.lock().unwrap();
        assert_eq!(st.ready_modes, ["initial", "initial"]);
        assert_eq!(st.prior_sessions, [None, None]);
        assert_eq!((st.key_changes, st.sessions_issued), (1, 1));
        st.session.clone().unwrap()
    };
    assert_ne!(
        session, NODE_SESSION,
        "Node's session was retired by the new key"
    );

    // The new key keeps the published contract, so Node can still overwrite it on a rollback.
    let rotated = published(r);
    assert_ne!(rotated, node);
    let m = std::fs::symlink_metadata(&r.key_file).unwrap();
    assert!(m.is_file());
    assert_eq!(
        (m.mode() & 0o7777, m.nlink(), m.uid(), m.gid()),
        (0o440, 1, r.uid, r.gid)
    );
    assert_ne!(
        m.ino(),
        ino_before,
        "replaced by rename, not rewritten in place"
    );
    assert_eq!(
        names(&r.keydir),
        ["current.key", "issuer"],
        "no temporary left"
    );
    assert_eq!(std::fs::metadata(&r.keydir).unwrap().mode() & 0o7777, 0o750);
    assert_eq!(
        std::fs::metadata(r.state_file.parent().unwrap())
            .unwrap()
            .mode()
            & 0o7777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&r.state_file).unwrap().mode() & 0o7777,
        0o600
    );
    // The state names the rotated key and this session, for a Replace after a restart.
    let state = load_state(&r.state_file).unwrap().unwrap();
    assert_eq!((state.key, state.next), (Some(rotated), None));
    assert_eq!(state.epoch, Some((BOOT.to_string(), session.clone())));
    // Grants verify under the new key only.
    let h = c
        .grant("task:adopt", &["example.com".to_string()])
        .await
        .unwrap();
    assert!(proxy_accepts(&proxy, &session, &h.token));
    let old = derive_epoch_keys(
        &FileKey::from_bytes(node),
        &unhex::<16>(BOOT).unwrap(),
        &unhex::<16>(&session).unwrap(),
    )
    .unwrap();
    assert!(egress::verify(&old.grant, h.token.as_bytes(), 1_800_000_000_000).is_err());
    (proxy, c, events, rotated)
}

#[tokio::test]
async fn a_takeover_from_a_proxy_node_already_readied_rotates_once_at_once_and_is_ready() {
    let Some(r) = rig() else { return };
    let node = lay_node_key(&r);
    let (_proxy, c, _events, _rotated) = take_over(&r, node).await;
    let _ = c.close().await;
}

#[tokio::test]
async fn a_restart_on_its_own_epoch_after_the_takeover_replaces_and_never_rotates() {
    let Some(r) = rig() else { return };
    let node = lay_node_key(&r);
    let (proxy, c, _events, rotated) = take_over(&r, node).await;
    let before = proxy.session().unwrap();
    let ino = std::fs::metadata(&r.key_file).unwrap().ino();
    drop(c);

    let (keeper, how) = open(&r).unwrap();
    assert_eq!((how, keeper.key()), (Opened::Kept, rotated));
    assert_eq!(keeper.persisted_epoch().map(|e| e.1), Some(before.clone()));
    let (c2, events) = client(keeper, &proxy);
    tokio::time::timeout(PROMPT, c2.ready())
        .await
        .expect("prompt")
        .expect("ready");
    assert_eq!(count(&events, "egress.control_rekeyed"), 0, "no rotation");
    assert_eq!(count(&events, "egress.control_conflict"), 0);
    assert_eq!(count(&events, "egress.control_ready"), 1);
    {
        let st = proxy.state.lock().unwrap();
        assert_eq!(st.ready_modes, ["initial", "initial", "replace"]);
        assert_eq!(
            st.prior_sessions.last().unwrap().as_deref(),
            Some(before.as_str())
        );
        assert_eq!(st.key_changes, 1);
    }
    assert_eq!(published(&r), rotated);
    assert_eq!(std::fs::metadata(&r.key_file).unwrap().ino(), ino);
    let _ = c2.close().await;
}

/// A proxy that still refuses Initial after the new key (it never resets) does not get a second
/// immediate rotation: the next refusal takes the conflict wait, as before.
#[tokio::test]
async fn the_immediate_rotation_happens_once_and_a_persisting_refusal_waits() {
    let Some(r) = rig() else { return };
    let node = lay_node_key(&r);
    let (keeper, how) = open(&r).unwrap();
    assert_eq!(how, Opened::Adopted);
    let proxy = FakeProxy::initialized_by_node(r.key_file.clone());
    proxy.state.lock().unwrap().sticky = true;
    let (c, events) = client(keeper, &proxy);

    // The Initial after the new key is refused too (a 409 is never taken as a proof).
    assert!(tokio::time::timeout(PROMPT, c.ready())
        .await
        .unwrap()
        .is_err());
    assert!(!c.available());
    let rotated = published(&r);
    assert_ne!(rotated, node);
    assert_eq!(count(&events, "egress.control_rekeyed"), 1);
    // Past the first backoff (1 s) the next attempt is refused and starts the wait; no new key.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        tokio::time::timeout(PROMPT, c.ready())
            .await
            .unwrap()
            .unwrap_err()
            .0,
        Reason::Conflict
    );
    assert_eq!(count(&events, "egress.control_rekeyed"), 1);
    assert_eq!(count(&events, "egress.control_conflict"), 1);
    assert_eq!(count(&events, "egress.control_ready"), 0);
    assert_eq!(published(&r), rotated);
    assert_eq!(
        proxy.state.lock().unwrap().ready_modes,
        ["initial", "initial", "initial"]
    );
    let _ = c.close().await;
}
