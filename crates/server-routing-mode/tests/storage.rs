#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use server_routing_mode::{self as rm, storage::Root};
use std::{
    os::unix::fs::{symlink, PermissionsExt},
    sync::atomic::AtomicBool,
};
const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
fn cap() -> server_store::RustRoutingMode {
    server_store::RustRoutingMode::from_env_value(
        Some("1"),
        server_store::RustAuth::from_env_value(Some("1")).unwrap(),
    )
    .unwrap()
}
#[test]
fn tenant_boundaries_regular_files_atomic_bytes_and_cancellation() {
    let d = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let c = AtomicBool::new(false);
    let root = Root::open(d.path()).unwrap();
    assert_eq!(root.read(A, &c).unwrap(), rm::normalize(None));
    assert!(!d.path().join("users").exists());
    for id in ["../escape", "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa/", ""] {
        assert!(root.write(id, &rm::normalize(None), cap(), &c).is_err());
    }
    symlink(external.path(), d.path().join("users")).unwrap();
    assert!(root.write(A, &rm::normalize(None), cap(), &c).is_err());
    assert_eq!(std::fs::read_dir(external.path()).unwrap().count(), 0);
    std::fs::remove_file(d.path().join("users")).unwrap();
    let setting = rm::validate(
        &rm::input::Json::parse(br#"{"mode":"hybrid","cloud":{"providerId":"p","smart":"m"}}"#)
            .unwrap(),
        &rm::all_modes(),
    )
    .unwrap();
    root.write(A, &setting, cap(), &c).unwrap();
    let tenant = d.path().join("users").join(A);
    let file = tenant.join(rm::FILE);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), setting.bytes());
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(root.read(A, &c).unwrap(), setting);
    let cancel = AtomicBool::new(true);
    assert!(root.write(A, &rm::normalize(None), cap(), &cancel).is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), setting.bytes());
    std::fs::write(tenant.join("routing-mode.json.tmp"), "preserve preexisting").unwrap();
    assert!(root.write(A, &rm::normalize(None), cap(), &c).is_err());
    assert_eq!(
        std::fs::read_to_string(tenant.join("routing-mode.json.tmp")).unwrap(),
        "preserve preexisting"
    );
    std::fs::remove_file(tenant.join("routing-mode.json.tmp")).unwrap();
    std::fs::remove_file(&file).unwrap();
    let private = external.path().join("private");
    std::fs::write(&private, "unchanged").unwrap();
    symlink(&private, &file).unwrap();
    assert!(root.read(A, &c).is_err());
    assert!(root.write(A, &setting, cap(), &c).is_err());
    assert_eq!(std::fs::read_to_string(&private).unwrap(), "unchanged");
    std::fs::remove_file(&file).unwrap();
    std::fs::hard_link(&private, &file).unwrap();
    assert!(root.read(A, &c).is_err());
    assert!(root.write(A, &setting, cap(), &c).is_err());
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert!(root.read(A, &c).is_err());
    assert!(root.write(A, &setting, cap(), &c).is_err());
    std::fs::remove_dir(&file).unwrap();
    std::fs::write(&file, "{").unwrap();
    assert_eq!(root.read(A, &c).unwrap(), rm::normalize(None));
    std::fs::write(&file, vec![b' '; rm::BODY_LIMIT + 1]).unwrap();
    assert!(root.read(A, &c).is_err());
}
#[test]
fn original_root_descriptor_survives_ancestor_replacement_and_lock_budget() {
    let parent = tempfile::Builder::new()
        .prefix("rm-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = parent.path().join("root");
    std::fs::create_dir(&base).unwrap();
    let root = Root::open(&base).unwrap();
    let c = AtomicBool::new(false);
    std::fs::rename(&base, parent.path().join("original")).unwrap();
    let external = tempfile::tempdir().unwrap();
    symlink(external.path(), &base).unwrap();
    root.write(A, &rm::normalize(None), cap(), &c).unwrap();
    assert_eq!(std::fs::read_dir(external.path()).unwrap().count(), 0);
    let tenant = parent.path().join("original/users").join(A);
    let fd = std::fs::File::open(tenant.join("routing-mode.lock")).unwrap();
    rustix::fs::flock(&fd, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let start = std::time::Instant::now();
    assert!(matches!(
        root.write(A, &rm::normalize(None), cap(), &c),
        Err(rm::storage::Fault::Busy)
    ));
    assert!(start.elapsed() >= std::time::Duration::from_millis(100));
    drop(fd);
    std::fs::remove_file(tenant.join(rm::FILE)).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(tenant.join(rm::FILE)).unwrap();
    assert!(root.read(A, &c).is_err());
    assert!(root.write(A, &rm::normalize(None), cap(), &c).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_fifo_refusal_is_nonblocking() {
    let d = tempfile::tempdir().unwrap();
    let c = AtomicBool::new(false);
    let root = Root::open(d.path()).unwrap();
    let tenant = d.path().join("users").join(A);
    std::fs::create_dir_all(&tenant).unwrap();
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        tenant.join(rm::FILE),
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .unwrap();
    assert!(root.read(A, &c).is_err());
    assert!(root.write(A, &rm::normalize(None), cap(), &c).is_err());
}
