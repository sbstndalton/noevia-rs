//! The issuer's own contract (beyond the oracle): files, restarts, rotation with held grants, and a
//! proxy that cannot be reached. Synthetic keys and a fake proxy that reads the published key on
//! every request, as the real one does.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod support;
use egress::control::{derive_epoch_keys, FileKey};
use egress_issuer::files::{key_file_state, load_state, IssuerFiles, KeyKeeper, KeyState, Opened};
use egress_issuer::wire::{hex, unhex};
use egress_issuer::{Client, ClientConfig, ControlError, Reason, SystemClock};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use support::{FakeProxy, ProxyTransport, BOOT};

struct Rig {
    _dir: tempfile::TempDir,
    key_file: PathBuf,
    state_file: PathBuf,
    files: IssuerFiles,
    uid: u32,
    gid: u32,
}

fn rig() -> Option<Rig> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    if gid == 0 {
        eprintln!("skipped: the 0440 key needs a non-root group");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let keydir = root.join("key");
    std::fs::create_dir(&keydir).unwrap();
    std::os::unix::fs::chown(&keydir, Some(uid), Some(gid)).unwrap();
    std::fs::set_permissions(&keydir, std::fs::Permissions::from_mode(0o750)).unwrap();
    let key_file = keydir.join("current.key");
    let state_file = root.join("issuer.json");
    let files = IssuerFiles {
        key_file: key_file.clone(),
        uid,
        gid,
        state_file: state_file.clone(),
    };
    Some(Rig {
        _dir: dir,
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
fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().mode() & 0o7777
}
fn client(r: &Rig, keeper: KeyKeeper, proxy: &Arc<FakeProxy>, auto: bool) -> Client {
    let key = keeper.key();
    let persisted = keeper.persisted_epoch();
    let mut config = ClientConfig::new("egress-rs-code:8040", 6 * 3600 * 1000, 7_200_000);
    config.auto_renew = auto;
    let _ = r;
    Client::new(
        config,
        key,
        Some(keeper),
        persisted,
        Arc::new(SystemClock::new()),
        Arc::new(ProxyTransport(Arc::clone(proxy))),
        Arc::new(|_, _| {}),
    )
    .unwrap()
}

/// The proxy's own verification of a token, as the real proxy derives its grant key from the published key.
fn proxy_accepts(proxy: &FakeProxy, session: &str, token: &str) -> bool {
    let file = proxy.key();
    let keys = derive_epoch_keys(
        &file,
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

#[tokio::test]
async fn the_published_key_keeps_its_contract_and_the_private_state_is_0600() {
    let Some(r) = rig() else { return };
    let (keeper, how) = open(&r).unwrap();
    assert_eq!(how, Opened::Fresh);
    // Published for the proxy exactly as before: regular file, one link, owner/group, 0440, 64 hex + LF.
    let meta = std::fs::metadata(&r.key_file).unwrap();
    assert!(meta.is_file());
    assert_eq!(
        (meta.nlink(), meta.uid(), meta.gid(), meta.mode() & 0o7777),
        (1, r.uid, r.gid, 0o440)
    );
    let text = std::fs::read_to_string(&r.key_file).unwrap();
    assert_eq!(text.len(), 65);
    assert!(
        text.ends_with('\n')
            && text[..64]
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    );
    assert_eq!(text.trim(), hex(&keeper.key()));
    // New private state: 0600, and the key's only other copy.
    assert_eq!(mode(&r.state_file), 0o600);
    // No temporary names are left behind.
    for dir in [r.key_file.parent().unwrap(), r.state_file.parent().unwrap()] {
        assert!(std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
    }
    // The key never appears in Debug output of anything the issuer hands out.
    let key_hex = hex(&keeper.key());
    assert!(!format!("{keeper:?} {:?}", r.files).contains(&key_hex));
}

#[tokio::test]
async fn a_restart_keeps_the_persisted_key_and_never_rotates_silently() {
    let Some(r) = rig() else { return };
    let (first, how) = open(&r).unwrap();
    assert_eq!(how, Opened::Fresh);
    let key = first.key();
    let published = std::fs::read(&r.key_file).unwrap();
    let inode = std::fs::metadata(&r.key_file).unwrap().ino();
    drop(first);
    for _ in 0..3 {
        let (again, how) = open(&r).unwrap();
        assert_eq!(how, Opened::Kept);
        assert_eq!(again.key(), key);
        assert_eq!(
            std::fs::read(&r.key_file).unwrap(),
            published,
            "the published file is untouched by a restart"
        );
        assert_eq!(
            std::fs::metadata(&r.key_file).unwrap().ino(),
            inode,
            "not even rewritten"
        );
    }
    assert_eq!(mode(&r.state_file), 0o600);
}

#[tokio::test]
async fn a_lost_published_key_is_restored_unchanged_and_a_foreign_one_is_never_touched() {
    let Some(r) = rig() else { return };
    let (first, _) = open(&r).unwrap();
    let key = first.key();
    drop(first);
    std::fs::remove_file(&r.key_file).unwrap();
    let (restored, how) = open(&r).unwrap();
    assert_eq!(how, Opened::Restored);
    assert_eq!(restored.key(), key, "no loss: the same key comes back");
    assert_eq!(mode(&r.key_file), 0o440);
    assert_eq!(
        std::fs::read_to_string(&r.key_file).unwrap().trim(),
        hex(&key)
    );
    drop(restored);
    // Another web's key: superseded, and nothing is ever written over it.
    let foreign = format!("{}\n", "ab".repeat(32));
    std::fs::remove_file(&r.key_file).unwrap();
    std::fs::write(&r.key_file, &foreign).unwrap();
    std::fs::set_permissions(&r.key_file, std::fs::Permissions::from_mode(0o440)).unwrap();
    let (mut other, how) = open(&r).unwrap();
    assert_eq!(how, Opened::Superseded);
    assert_eq!(other.key(), key);
    assert_eq!(other.restore(), "superseded");
    assert!(other.rekey().is_none(), "a superseded keeper never re-keys");
    assert_eq!(std::fs::read_to_string(&r.key_file).unwrap(), foreign);
}

#[tokio::test]
async fn private_state_that_is_not_exactly_ours_fails_closed() {
    let Some(r) = rig() else { return };
    let (first, _) = open(&r).unwrap();
    drop(first);
    // Loose permissions: refused, never "no state" (which would mint a new key).
    std::fs::set_permissions(&r.state_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::Publication);
    assert!(load_state(&r.state_file).is_err());
    std::fs::set_permissions(&r.state_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    // Corrupt: refused.
    std::fs::write(&r.state_file, b"{\"version\":1,\"key\":\"zz\"}").unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::Publication);
    // A symlink is not followed.
    std::fs::remove_file(&r.state_file).unwrap();
    let target = r.state_file.with_extension("target");
    std::fs::write(&target, b"x").unwrap();
    std::os::unix::fs::symlink(&target, &r.state_file).unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::Publication);
    // Nothing above changed the published key.
    assert_eq!(mode(&r.key_file), 0o440);
}

#[tokio::test]
async fn an_unsafe_key_directory_or_file_is_refused_and_never_repaired() {
    let Some(r) = rig() else { return };
    let keydir = r.key_file.parent().unwrap().to_path_buf();
    for unsafe_mode in [0o770, 0o757, 0o700, 0o740] {
        std::fs::set_permissions(&keydir, std::fs::Permissions::from_mode(unsafe_mode)).unwrap();
        assert_eq!(
            open(&r).unwrap_err().0,
            Reason::Publication,
            "{unsafe_mode:o}"
        );
        assert_eq!(mode(&keydir), unsafe_mode, "the directory is not repaired");
        assert!(!r.key_file.exists(), "nothing is published into it");
        let _ = std::fs::remove_file(&r.state_file);
    }
    std::fs::set_permissions(&keydir, std::fs::Permissions::from_mode(0o750)).unwrap();
    // A key file with the wrong mode, an extra link, or a symlink stays as it is.
    std::fs::write(&r.key_file, "x\n").unwrap();
    std::fs::set_permissions(&r.key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::Publication);
    assert_eq!(mode(&r.key_file), 0o600);
    assert_eq!(std::fs::read_to_string(&r.key_file).unwrap(), "x\n");
    let _ = std::fs::remove_file(&r.state_file);
    std::fs::remove_file(&r.key_file).unwrap();
    std::fs::write(keydir.join("other"), "x").unwrap();
    std::os::unix::fs::symlink(keydir.join("other"), &r.key_file).unwrap();
    assert_eq!(open(&r).unwrap_err().0, Reason::Publication);
    assert!(std::fs::symlink_metadata(&r.key_file)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[tokio::test]
async fn an_interrupted_publication_or_rekey_is_finished_with_the_same_key() {
    let Some(r) = rig() else { return };
    // First publication that crashed before the rename: the candidate was persisted first.
    let candidate = [0x42u8; 32];
    egress_issuer::files::save_state(
        &r.state_file,
        &egress_issuer::files::PrivateState {
            key: None,
            next: Some(candidate),
            epoch: None,
        },
    )
    .unwrap();
    let (keeper, how) = open(&r).unwrap();
    assert_eq!((how, keeper.key()), (Opened::Fresh, candidate));
    assert_eq!(
        std::fs::read_to_string(&r.key_file).unwrap().trim(),
        hex(&candidate)
    );
    drop(keeper);
    // A re-key that crashed after the rename: the published file holds `next`, which becomes the key.
    let next = [0x43u8; 32];
    let state = egress_issuer::files::PrivateState {
        key: Some(candidate),
        next: Some(next),
        epoch: Some((BOOT.into(), "cd".repeat(16))),
    };
    egress_issuer::files::save_state(&r.state_file, &state).unwrap();
    std::fs::remove_file(&r.key_file).unwrap();
    std::fs::write(&r.key_file, format!("{}\n", hex(&next))).unwrap();
    std::fs::set_permissions(&r.key_file, std::fs::Permissions::from_mode(0o440)).unwrap();
    let (promoted, how) = open(&r).unwrap();
    assert_eq!((how, promoted.key()), (Opened::Kept, next));
    assert!(
        promoted.persisted_epoch().is_none(),
        "a new key carries no session"
    );
    drop(promoted);
    // A re-key that crashed before the rename: the old key is still the published one and stays the key.
    let state = egress_issuer::files::PrivateState {
        key: Some(next),
        next: Some([0x44u8; 32]),
        epoch: None,
    };
    egress_issuer::files::save_state(&r.state_file, &state).unwrap();
    let (kept, how) = open(&r).unwrap();
    assert_eq!((how, kept.key()), (Opened::Kept, next));
    assert_eq!(
        std::fs::read_to_string(&r.key_file).unwrap().trim(),
        hex(&next)
    );
}

#[tokio::test]
async fn a_restart_replaces_its_own_session_and_old_grants_stop_verifying() {
    let Some(r) = rig() else { return };
    let (keeper, _) = open(&r).unwrap();
    let proxy = FakeProxy::new(r.key_file.clone());
    let c1 = client(&r, keeper, &proxy, false);
    c1.ready().await.unwrap();
    let before = proxy.session().unwrap();
    let held = c1
        .grant("task:1", &["example.com".to_string()])
        .await
        .unwrap();
    assert!(
        proxy_accepts(&proxy, &before, &held.token),
        "the proxy accepts what the issuer signs"
    );
    let published = std::fs::read(&r.key_file).unwrap();
    drop(c1);
    // The web restarts: same key, and the persisted session is Replaced, not waited out.
    let (keeper, how) = open(&r).unwrap();
    assert_eq!(how, Opened::Kept);
    assert_eq!(keeper.persisted_epoch().map(|e| e.1), Some(before.clone()));
    let c2 = client(&r, keeper, &proxy, false);
    c2.ready().await.unwrap();
    let after = proxy.session().unwrap();
    assert_ne!(after, before);
    {
        let st = proxy.state.lock().unwrap();
        assert_eq!(st.ready_modes, ["initial", "replace"]);
        assert_eq!(
            st.prior_sessions.last().unwrap().as_deref(),
            Some(before.as_str())
        );
    }
    assert!(
        !proxy_accepts(&proxy, &after, &held.token),
        "a grant of the replaced session verifies nowhere"
    );
    let fresh = c2
        .grant("task:1", &["example.com".to_string()])
        .await
        .unwrap();
    assert!(proxy_accepts(&proxy, &after, &fresh.token));
    assert_eq!(
        std::fs::read(&r.key_file).unwrap(),
        published,
        "no rotation happened"
    );
    // The old incarnation's handle is unknown to the new one: refused, never silently re-acquired.
    assert_eq!(
        c2.revoke(&held.id, None).await.unwrap_err().0,
        Reason::Handle
    );
    let _ = c2.close().await;
}

#[tokio::test]
async fn rotation_while_a_task_holds_a_grant_leaves_the_new_key_working() {
    let Some(r) = rig() else { return };
    let (mut keeper, _) = open(&r).unwrap();
    let proxy = FakeProxy::new(r.key_file.clone());
    let old_key = keeper.key();
    let rekeyed = keeper.rekey().expect("own key can be rotated");
    assert_ne!(rekeyed, old_key);
    assert_eq!(
        std::fs::read_to_string(&r.key_file).unwrap().trim(),
        hex(&rekeyed)
    );
    assert_eq!(mode(&r.key_file), 0o440);
    assert_eq!(key_file_state(&r.key_file, &rekeyed), KeyState::Own);
    assert_eq!(key_file_state(&r.key_file, &old_key), KeyState::Foreign);
    assert!(keeper.persisted_epoch().is_none());
    // The persisted key is the new one, for the next restart.
    drop(keeper);
    let (again, how) = open(&r).unwrap();
    assert_eq!((how, again.key()), (Opened::Kept, rekeyed));
    let c = client(&r, again, &proxy, false);
    c.ready().await.unwrap();
    let h = c
        .grant("task:2", &["api.example.com".to_string()])
        .await
        .unwrap();
    assert!(
        proxy_accepts(&proxy, &proxy.session().unwrap(), &h.token),
        "grants after a rotation verify under the new key"
    );
    let old = derive_epoch_keys(
        &FileKey::from_bytes(old_key),
        &unhex::<16>(BOOT).unwrap(),
        &unhex::<16>(&proxy.session().unwrap()).unwrap(),
    )
    .unwrap();
    assert!(
        egress::verify(&old.grant, h.token.as_bytes(), 1_800_000_000_000).is_err(),
        "and not under the retired one"
    );
}

#[tokio::test]
async fn an_unreachable_proxy_refuses_every_grant_and_says_so() {
    let Some(r) = rig() else { return };
    let (keeper, _) = open(&r).unwrap();
    let proxy = FakeProxy::new(r.key_file.clone());
    proxy.state.lock().unwrap().down = true;
    let c = client(&r, keeper, &proxy, false);
    assert!(!c.available());
    assert!(c.ready().await.is_err());
    assert_eq!(
        c.grant("task:3", &["example.com".to_string()])
            .await
            .unwrap_err()
            .to_string(),
        "native egress is unavailable"
    );
    assert!(!c.available());
    // Inside the backoff a caller is refused at once and the proxy is not hammered.
    let seen = proxy.state.lock().unwrap().requests;
    assert!(c
        .grant("task:3", &["example.com".to_string()])
        .await
        .is_err());
    assert_eq!(proxy.state.lock().unwrap().requests, seen);
}

#[tokio::test]
async fn neither_errors_nor_debug_output_carry_key_material_or_tokens() {
    let Some(r) = rig() else { return };
    let (keeper, _) = open(&r).unwrap();
    let key_hex = hex(&keeper.key());
    let proxy = FakeProxy::new(r.key_file.clone());
    let c = client(&r, keeper, &proxy, false);
    let h = c
        .grant("task:4", &["example.com".to_string()])
        .await
        .unwrap();
    let shown = format!(
        "{c:?} {h:?} {:?} {:?}",
        c.grant("bad task", &[]).await,
        c.revoke("nope", None).await
    );
    assert!(!shown.contains(&key_hex) && !shown.contains(&h.token) && !shown.contains("ngr1."));
    let _ = c.close().await;
}
