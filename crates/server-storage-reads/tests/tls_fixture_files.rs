#[path = "../src/bin/support/tls_file.rs"]
mod files;
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{symlink, DirBuilderExt, OpenOptionsExt},
};
#[test]
fn owned_fixture_file_cap_and_descriptor_refusals() {
    let root = std::env::temp_dir().join(format!("m4-tls-files-{}", std::process::id()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .unwrap_or_else(|_| unreachable!());
    let exact = root.join("exact");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&exact)
        .unwrap_or_else(|_| unreachable!());
    file.set_len(files::FILE_CAP as u64)
        .unwrap_or_else(|_| unreachable!());
    assert_eq!(
        files::read(&exact).unwrap_or_else(|_| unreachable!()).len(),
        files::FILE_CAP
    );
    file.set_len(files::FILE_CAP as u64 + 1)
        .unwrap_or_else(|_| unreachable!());
    assert!(files::read(&exact).is_err());
    let link = root.join("link");
    symlink(&exact, &link).unwrap_or_else(|_| unreachable!());
    assert!(files::read(&link).is_err());
    assert!(files::read(&root).is_err());
    file.set_len(1).unwrap_or_else(|_| unreachable!());
    fs::hard_link(&exact, root.join("second")).unwrap_or_else(|_| unreachable!());
    assert!(files::read(&exact).is_err());
}
