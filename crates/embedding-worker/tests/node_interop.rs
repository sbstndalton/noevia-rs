#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
mod support;
use embedding_worker::{fill::Store, Control, Index};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::Duration,
};
use support::*;
fn extension() -> PathBuf {
    static BUILT: OnceLock<tempfile::TempDir> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let tmp = tempfile::tempdir().unwrap();
            let cargo = std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cargo"));
            let source = std::fs::read_dir(cargo.join("registry/src"))
                .unwrap()
                .map(Result::unwrap)
                .map(|d| d.path().join("sqlite-vec-0.1.9"))
                .find(|p| p.join("sqlite-vec.c").is_file())
                .unwrap();
            let mut command = Command::new("cc");
            command
                .args(["-shared", "-fPIC", "-O2"])
                .arg(source.join("sqlite-vec.c"))
                .arg("-I")
                .arg(&source)
                .arg("-lm")
                .arg("-o")
                .arg(tmp.path().join("vec0.so"));
            let output = process::bounded(command, vec![], Duration::from_secs(120)).unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            tmp
        })
        .path()
        .join("vec0.so")
}
fn node(db: &Path, mode: &str, ready: &Path, done: &Path) -> Command {
    let mut c = Command::new("node");
    c.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/node-reader-writer.cjs"))
        .arg(db)
        .arg(extension())
        .arg(mode)
        .arg(ready)
        .arg(done);
    c
}
fn publish(store: &embedding_worker::sqlite::SqlStore, index: &Index) {
    let rows = store
        .missing(index, &target("next"), 4, &Default::default())
        .unwrap()
        .unwrap();
    if !rows.is_empty() {
        store
            .publish(
                index,
                &target("next"),
                &rows,
                &vec![vec![1.0, 0.0]; rows.len()],
                0,
                &Control::default(),
            )
            .unwrap();
    }
}
#[test]
fn node_knn_reader_uses_actual_join_sql_over_rust_published_vectors() {
    let (dir, store) = seed();
    let index = Index {
        user: None,
        project: "p".into(),
    };
    publish(&store, &index);
    let out = process::bounded(
        node(
            &store.index_path(&index).unwrap(),
            "read",
            dir.path(),
            dir.path(),
        ),
        vec![],
        Duration::from_secs(15),
    )
    .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    // Python uses the same vec0 SQL/blob ABI as the Diary process, no Python engine.
    let mut python = Command::new("python3");
    python.arg("-c").arg("import sqlite3,sys; c=sqlite3.connect(sys.argv[1]);c.enable_load_extension(True);c.load_extension(sys.argv[2]);r=c.execute('select chunk_id from vec_items where embedding match ? and k=1',(__import__('struct').pack('<ff',1,0),)).fetchone();assert r==(1,),r").arg(store.index_path(&index).unwrap()).arg(extension());
    let out = process::bounded(python, vec![], Duration::from_secs(15)).unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
#[test]
fn actual_node_embed_rows_and_native_publication_share_the_same_live_index() {
    let (dir, store) = seed();
    let index = Index {
        user: None,
        project: "p".into(),
    };
    let active = store
        .missing(&index, &target("active"), 4, &Default::default())
        .unwrap()
        .unwrap();
    store
        .publish(
            &index,
            &target("active"),
            &active,
            &[vec![1.0, 0.0]],
            0,
            &Control::default(),
        )
        .unwrap();
    let ready = dir.path().join("ready");
    let done = dir.path().join("done");
    let cmd = node(&store.index_path(&index).unwrap(), "write", &ready, &done);
    let child =
        std::thread::spawn(move || process::bounded(cmd, vec![], Duration::from_secs(20)).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ready.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(ready.exists());
    for _ in 0..40 {
        let rows = vec![embedding_worker::fill::Row {
            id: 1,
            hash: "hash".into(),
            text: vec![],
        }];
        store
            .publish(
                &index,
                &target("next"),
                &rows,
                &[vec![1.0, 0.0]],
                0,
                &Control::default(),
            )
            .unwrap();
    }
    let out = child.join().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(done.exists());
    assert_eq!(store.remaining(&index, &target("next")).unwrap(), 0);
}
