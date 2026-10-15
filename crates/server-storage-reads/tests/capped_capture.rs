#[path = "../examples/support/capped_capture.rs"]
mod capture;
use std::{
    fs::{self, File},
    os::unix::fs::symlink,
};
#[test]
fn descriptor_type_cap_growth_controls() {
    let root = std::env::temp_dir().join(format!("m4-capture-controls-{}", std::process::id()));
    fs::create_dir(&root).unwrap_or_else(|_| unreachable!());
    let path = root.join("regular");
    let file = File::create(&path).unwrap_or_else(|_| unreachable!());
    file.set_len(capture::CAP as u64)
        .unwrap_or_else(|_| unreachable!());
    let bytes = capture::read(&path).unwrap_or_else(|_| unreachable!());
    assert_eq!(bytes.len(), capture::CAP);
    file.set_len(capture::CAP as u64 + 1)
        .unwrap_or_else(|_| unreachable!());
    assert!(matches!(capture::read(&path), Err(capture::Error::Cap)));
    file.set_len(0).unwrap_or_else(|_| unreachable!());
    let mut opened = capture::open(&path).unwrap_or_else(|_| unreachable!());
    file.set_len(capture::CAP as u64 + 1)
        .unwrap_or_else(|_| unreachable!());
    assert!(matches!(
        capture::consume(&mut opened),
        Err(capture::Error::Cap)
    ));
    let link = root.join("symlink");
    symlink(&path, &link).unwrap_or_else(|_| unreachable!());
    assert!(matches!(capture::read(&link), Err(capture::Error::Open)));
    assert!(matches!(capture::read(&root), Err(capture::Error::Type)));
    let hard = root.join("hard");
    fs::hard_link(&path, &hard).unwrap_or_else(|_| unreachable!());
    assert!(matches!(capture::read(&path), Err(capture::Error::Type)));
    assert!(matches!(
        capture::read(std::path::Path::new("relative")),
        Err(capture::Error::Path)
    ));
    println!("owned sparse cap/growth/symlink/directory/hardlink/path controls passed");
}
#[test]
#[cfg(target_os = "linux")]
fn nonblocking_fifo_refused_before_read() {
    let root = std::env::temp_dir().join(format!("m4-capture-fifo-{}", std::process::id()));
    fs::create_dir(&root).unwrap_or_else(|_| unreachable!());
    let fifo = root.join("fifo");
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap_or_else(|_| unreachable!());
    assert!(matches!(capture::read(&fifo), Err(capture::Error::Type)));
}
