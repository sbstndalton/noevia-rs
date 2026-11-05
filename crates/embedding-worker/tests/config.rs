#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use embedding_worker::{
    config::{self, Change},
    runtime,
};
use serde_json::{json, Value};

#[test]
fn node_oracle_transitions_and_errors() -> Result<(), &'static str> {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/embedding-route-oracle.json")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let seed =
        runtime::seed(|key| (key == "EMBEDDING_MODEL").then(|| "fixture-active".into())).unwrap();
    for step in oracle["states"].as_array().unwrap() {
        let before = runtime::load_config(dir.path(), &seed).unwrap();
        let change = match step["action"].as_str().unwrap() {
            "setNext" => Change::SetNext(embedding_worker::Target {
                model: step["input"]["model"].as_str().unwrap().into(),
                profile: seed.active.profile.clone(),
                base_url: step["input"]["baseUrl"].as_str().map(String::from),
            }),
            "switchToNext" => Change::Switch,
            "rollback" => Change::Rollback,
            "finish" => Change::Finish,
            "clearNext" => Change::ClearNext,
            _ => return Err("unknown fixture step"),
        };
        match config::publish(dir.path(), &seed, &before, change) {
            Ok(result) => assert_eq!(config::state_json(&result.config), step["state"]),
            Err(error) => assert_eq!(
                json!({"status":error.status,"code":error.code,"message":error.message}),
                step["error"]
            ),
        }
    }
    Ok(())
}
#[test]
fn stale_checks_cannot_overwrite_new_target_or_drop_its_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let seed = runtime::seed(|_| None).unwrap();
    let mut next = seed.active.clone();
    next.model = "fixture-next".into();
    let published =
        config::publish(dir.path(), &seed, &seed, Change::SetNext(next.clone())).unwrap();
    assert!(published.retired.is_empty());
    let bytes = std::fs::read(dir.path().join("embedding-config.json")).unwrap();
    let error = config::publish(dir.path(), &seed, &seed, Change::ClearNext)
        .err()
        .unwrap();
    assert_eq!(error.code, "nextChanged");
    assert_eq!(
        std::fs::read(dir.path().join("embedding-config.json")).unwrap(),
        bytes
    );
    let cleared = config::publish(dir.path(), &seed, &published.config, Change::ClearNext).unwrap();
    assert_eq!(cleared.retired, vec![next]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join("embedding-config.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
#[test]
fn damaged_file_is_never_replaced_by_seed() {
    let dir = tempfile::tempdir().unwrap();
    let seed = runtime::seed(|_| None).unwrap();
    std::fs::write(dir.path().join("embedding-config.json"), "{").unwrap();
    assert_eq!(
        config::publish(dir.path(), &seed, &seed, Change::ClearNext)
            .err()
            .unwrap()
            .code,
        "badState"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("embedding-config.json")).unwrap(),
        "{"
    );
}

#[test]
fn competing_checked_writers_publish_only_one_generation() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::sync::Arc::new(dir.path().to_path_buf());
    let seed = runtime::seed(|_| None).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let jobs: Vec<_> = ["fixture-one", "fixture-two"]
        .into_iter()
        .map(|model| {
            let root = root.clone();
            let seed = seed.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut target = seed.active.clone();
                target.model = model.into();
                barrier.wait();
                config::publish(&root, &seed, &seed, Change::SetNext(target))
            })
        })
        .collect();
    let results: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| r.as_ref().err().is_some_and(|e| e.code == "nextChanged"))
            .count(),
        1
    );
    assert!(runtime::load_config(&root, &seed).unwrap().next.is_some());
    assert!(!std::fs::read_dir(&*root).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
}
