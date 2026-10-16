#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use proptest::prelude::*;
fn synthetic(root: &Path) -> Files {
    Files {
        models: access::Root::synthetic(root).unwrap(),
        ini_root: access::Root::synthetic(root).unwrap(),
        ini_name: "models.ini".into(),
    }
}
#[test]
fn complete_actual_python_responses() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/sections.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let temp = tempfile::tempdir().unwrap();
        if let Some(text) = case["ini"].as_str() {
            std::fs::write(temp.path().join("models.ini"), text).unwrap();
        }
        for item in case["files"].as_array().unwrap() {
            let file = temp.path().join(item["name"].as_str().unwrap());
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            let bytes: Vec<u8> = item["bytes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u8)
                .collect();
            std::fs::write(&file, bytes).unwrap();
            let handle = std::fs::File::options().write(true).open(file).unwrap();
            if let Some(size) = item["size"].as_u64() {
                handle.set_len(size).unwrap();
            }
            if item["name"]
                .as_str()
                .unwrap()
                .starts_with("models.ini.bak-")
            {
                handle
                    .set_times(
                        std::fs::FileTimes::new().set_modified(
                            std::time::UNIX_EPOCH + Duration::from_secs(1_200_000_000),
                        ),
                    )
                    .unwrap();
            }
        }
        let mut before = Vec::new();
        for item in case["files"].as_array().unwrap() {
            let file =
                std::fs::File::open(temp.path().join(item["name"].as_str().unwrap())).unwrap();
            let stat = file.metadata().unwrap();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(file, 4096), &mut bytes).unwrap();
            before.push((stat.len(), stat.modified().unwrap(), bytes));
        }
        #[cfg(target_os = "linux")]
        let reader = Files::new(temp.path(), &temp.path().join("models.ini")).unwrap();
        #[cfg(not(target_os = "linux"))]
        let reader = synthetic(temp.path());
        let answer = reader.response(case["name"].as_str(), case["defaults"].as_bool().unwrap());
        if let Some(expected) = case["expected"].get("response") {
            let result: Value =
                serde_json::from_slice(&answer.unwrap_or_else(|e| panic!("{} {e:?}", case["id"])))
                    .unwrap();
            assert_eq!(&result, expected, "{}", case["id"]);
        } else {
            assert!(answer.is_err(), "{}", case["id"]);
        }
        for (i, item) in case["files"].as_array().unwrap().iter().enumerate() {
            let file =
                std::fs::File::open(temp.path().join(item["name"].as_str().unwrap())).unwrap();
            let stat = file.metadata().unwrap();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(file, 4096), &mut bytes).unwrap();
            assert_eq!((stat.len(), stat.modified().unwrap(), bytes), before[i]);
        }
        assert!(!temp.path().join("state.sqlite").exists());
    }
}

struct NeverDocker;
impl crate::DockerRead for NeverDocker {
    fn containers(&self) -> super::super::Job<'_, Vec<crate::Container>> {
        Box::pin(async { panic!("sections must not discover Docker") })
    }
    fn logs<'a>(&'a self, _: &'a str, _: usize) -> super::super::Job<'a, Vec<u8>> {
        Box::pin(async { panic!("sections must not read logs") })
    }
}
fn state(reader: Arc<dyn Read>) -> Arc<crate::StateData> {
    Arc::new(crate::StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis: None,
        #[cfg(feature = "native-prompts")]
        prompts: None,
        #[cfg(feature = "native-settings")]
        settings: None,
        #[cfg(feature = "native-model-updates")]
        model_updates: None,
        #[cfg(feature = "native-model-detail")]
        model_detail: None,
        sections: Some(Arc::new(Sections::new(reader))),
        embedding: None,
        host: None,
        token: "fixture-token".into(),
        configured: Vec::new(),
        docker: Arc::new(NeverDocker),
        redactor: crate::Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(4),
        legacy: crate::legacy_client().unwrap(),
        legacy_base: crate::LEGACY_BASE.to_owned(),
        idle_unload: None,
        #[cfg(feature = "native-download-targets")]
        download_targets: None,
    })
}
#[tokio::test]
async fn actual_framework_query_redirect_method_and_auth_contract() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let fixture: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/sections.json")).unwrap();
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("models.ini"), "[tiny]\nparallel=1\n").unwrap();
    let bytes: Vec<u8> = fixture["cases"][0]["files"][0]["bytes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_u64().unwrap() as u8)
        .collect();
    std::fs::write(temp.path().join("tiny.gguf"), bytes).unwrap();
    let app = crate::router(state(Arc::new(synthetic(temp.path()))));
    for case in fixture["http"].as_array().unwrap() {
        let request = axum::http::Request::builder()
            .method(case["method"].as_str().unwrap())
            .uri(case["path"].as_str().unwrap())
            .header("host", "testserver")
            .header("x-model-loader-token", "fixture-token")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status().as_u16() as u64,
            case["expected"]["status"].as_u64().unwrap(),
            "{}",
            case["path"]
        );
        assert_eq!(
            response
                .headers()
                .get("location")
                .map(|h| h.to_str().unwrap()),
            case["expected"]["location"].as_str(),
            "{}",
            case["path"]
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        assert_eq!(body, case["expected"]["body"], "{}", case["path"]);
    }
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/v1/sections/tiny?defaults=bad")
                .header("x-model-loader-token", "wrong")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

struct Blocking {
    count: std::sync::atomic::AtomicUsize,
    started: tokio::sync::Notify,
    release: std::sync::Mutex<bool>,
    condition: std::sync::Condvar,
}
impl Read for Blocking {
    fn response(&self, _: Option<&str>, _: bool) -> Result<Vec<u8>, Error> {
        if self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == 2 {
            self.started.notify_one();
        }
        let mut released = self.release.lock().unwrap();
        while !*released {
            released = self.condition.wait(released).unwrap();
        }
        Ok(b"{}".to_vec())
    }
}
#[tokio::test]
async fn cancelled_reads_keep_admission_until_blocking_operation_completes() {
    let reader = Arc::new(Blocking {
        count: std::sync::atomic::AtomicUsize::new(0),
        started: tokio::sync::Notify::new(),
        release: std::sync::Mutex::new(false),
        condition: std::sync::Condvar::new(),
    });
    let sections = Arc::new(Sections::new(reader.clone()));
    let first = tokio::spawn({
        let s = sections.clone();
        async move { s.response(None, false).await }
    });
    let second = tokio::spawn({
        let s = sections.clone();
        async move { s.response(None, false).await }
    });
    let started = tokio::time::timeout(Duration::from_secs(1), reader.started.notified()).await;
    first.abort();
    second.abort();
    let third = sections.response(None, false).await;
    *reader.release.lock().unwrap() = true;
    reader.condition.notify_all();
    assert!(started.is_ok());
    assert!(third.is_err());
    assert_eq!(reader.count.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn timed_out_reads_keep_both_slots_until_completion() {
    let reader = Arc::new(Blocking {
        count: std::sync::atomic::AtomicUsize::new(0),
        started: tokio::sync::Notify::new(),
        release: std::sync::Mutex::new(false),
        condition: std::sync::Condvar::new(),
    });
    let sections = Arc::new(Sections::new(reader.clone()));
    let first = tokio::spawn({
        let s = sections.clone();
        async move { s.response(None, false).await }
    });
    let second = tokio::spawn({
        let s = sections.clone();
        async move { s.response(None, false).await }
    });
    let (a, b) = tokio::join!(first, second);
    let third = sections.response(None, false).await;
    *reader.release.lock().unwrap() = true;
    reader.condition.notify_all();
    assert!(a.unwrap().is_err());
    assert!(b.unwrap().is_err());
    assert!(third.is_err());
    assert_eq!(reader.count.load(std::sync::atomic::Ordering::SeqCst), 2);
}
#[test]
fn cap_checks_and_cli_number_regression() {
    let mut budget = Budget::default();
    let ini = ini::Ini::parse("[DEFAULT]\nparallel=1\n[tiny]\nmain-gpu=0\n", &mut budget).unwrap();
    let values = ini.items("tiny", &mut budget).unwrap().unwrap();
    assert_eq!(
        ini::cli("tiny", &values, kind).unwrap(),
        "--alias tiny --model /models/tiny.gguf --parallel 1 --main-gpu 0"
    );
    let oversized = format!("[tiny]\nx={}\n", "x".repeat(ini::MAX_VALUE));
    assert!(ini::Ini::parse(&oversized, &mut Budget::default()).is_err());
    let mut budget = Budget::default();
    for _ in 0..16384 {
        budget.entry().unwrap();
    }
    assert!(budget.entry().is_err());
    let mut out = Capped {
        bytes: vec![0; RESPONSE_CAP],
    };
    assert!(out.write(b"x").is_err());
}

#[test]
fn enumeration_and_default_expansion_caps_precede_growth() {
    let temp = tempfile::tempdir().unwrap();
    for index in 0..10 {
        std::fs::write(temp.path().join(format!(".ignored-{index}")), b"").unwrap();
    }
    let root = access::Root::synthetic(temp.path()).unwrap();
    let mut budget = Budget {
        entries: 16380,
        ..Budget::default()
    };
    assert!(root.entries(".", &mut budget).is_err());
    assert_eq!(budget.entries, 16385);
    let mut text = "[DEFAULT]\n".to_owned();
    for key in 0..256 {
        text.push_str(&format!("key-{key}=value\n"));
    }
    for section in 0..257 {
        text.push_str(&format!("[section-{section}]\n"));
    }
    assert!(ini::Ini::parse(&text, &mut Budget::default()).is_err());
    let mut text = String::new();
    for section in 0..=ini::MAX_SECTIONS {
        text.push_str(&format!("[s-{section}]\n"));
    }
    assert!(ini::Ini::parse(&text, &mut Budget::default()).is_err());
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("models.ini"), "[tiny]\n").unwrap();
    let reader = synthetic(temp.path());
    let mut budget = Budget {
        output: RESPONSE_CAP,
        ..Budget::default()
    };
    assert!(budget.charge(1).is_err());
    assert!(reader.response(Some("bad/name"), false).is_err());
}
proptest! {
    #[test]
    fn inherited_values_and_raw_revision(value in "[a-zA-Z0-9 ]{0,100}") {
        let temp=tempfile::tempdir().unwrap();let text=format!("[DEFAULT]\nunknown={value}\n[tiny]\nparallel=1\n");std::fs::write(temp.path().join("models.ini"),&text).unwrap();
        let response:Value=serde_json::from_slice(&synthetic(temp.path()).response(None,false).unwrap()).unwrap();
        prop_assert_eq!(response["revision"].as_str().unwrap(),format!("{:x}",Sha256::digest(text.as_bytes())));
        prop_assert_eq!(response["raw"].as_str().unwrap(),text);
        prop_assert_eq!(response["sections"][0]["items"][0][1].as_str().unwrap(),value.trim());
    }
}
#[cfg(not(target_os = "linux"))]
#[test]
fn unsupported_production_adapter_refuses() {
    let temp = tempfile::tempdir().unwrap();
    assert!(Files::new(temp.path(), &temp.path().join("models.ini")).is_err());
}
#[cfg(target_os = "linux")]
#[test]
fn kernel_containment_cache_symlink_and_special_files() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("cache/snapshots/rev")).unwrap();
    std::fs::create_dir_all(temp.path().join("cache/blobs")).unwrap();
    std::fs::write(temp.path().join("cache/blobs/header"), b"GGUF").unwrap();
    std::fs::write(outside.path().join("private"), b"OUTSIDE").unwrap();
    symlink(
        "../../blobs/header",
        temp.path().join("cache/snapshots/rev/model.gguf"),
    )
    .unwrap();
    symlink(
        outside.path().join("private"),
        temp.path().join("escape.gguf"),
    )
    .unwrap();
    let root = access::Root::new(temp.path()).unwrap();
    assert!(root
        .file(
            "cache/snapshots/rev/model.gguf",
            false,
            &mut Budget::default()
        )
        .unwrap()
        .is_some());
    assert!(root
        .file("escape.gguf", false, &mut Budget::default())
        .is_err());
    assert!(root
        .file("../private", false, &mut Budget::default())
        .is_err());
    rustix::fs::mknodat(
        rustix::fs::CWD,
        temp.path().join("fifo.gguf"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
    assert!(root
        .file("fifo.gguf", false, &mut Budget::default())
        .is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn kernel_symlink_replacement_never_opens_outside_bytes() {
    use std::io::Read;
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("inside"), b"INSIDE").unwrap();
    std::fs::write(outside.path().join("private"), b"OUTSIDE").unwrap();
    symlink("inside", temp.path().join("active.gguf")).unwrap();
    let root = access::Root::new(temp.path()).unwrap();
    let path = temp.path().to_owned();
    let external = outside.path().join("private");
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let worker_barrier = barrier.clone();
    let worker = std::thread::spawn(move || {
        worker_barrier.wait();
        for index in 0..200 {
            let next = path.join("next-link");
            symlink(
                if index % 2 == 0 {
                    Path::new("inside")
                } else {
                    external.as_path()
                },
                &next,
            )
            .unwrap();
            std::fs::rename(next, path.join("active.gguf")).unwrap();
        }
    });
    barrier.wait();
    for _ in 0..200 {
        if let Ok(Some(mut file)) = root.file("active.gguf", false, &mut Budget::default()) {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"INSIDE");
        }
    }
    worker.join().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn replacement_response_keeps_raw_byte_revision_coherent() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("models.ini"), "[a]\nx=1\n").unwrap();
    let files = Files::new(temp.path(), &temp.path().join("models.ini")).unwrap();
    let path = temp.path().to_owned();
    let worker = std::thread::spawn(move || {
        for index in 0..100 {
            std::fs::write(
                path.join("next-ini"),
                if index % 2 == 0 {
                    "[a]\nx=1\n"
                } else {
                    "[b]\ny=2\n"
                },
            )
            .unwrap();
            std::fs::rename(path.join("next-ini"), path.join("models.ini")).unwrap();
        }
    });
    for _ in 0..100 {
        if let Ok(bytes) = files.response(None, false) {
            let response: Value = serde_json::from_slice(&bytes).unwrap();
            let raw = response["raw"].as_str().unwrap();
            assert!(raw == "[a]\nx=1\n" || raw == "[b]\ny=2\n");
            assert_eq!(
                response["revision"].as_str().unwrap(),
                format!("{:x}", Sha256::digest(raw.as_bytes()))
            );
        }
    }
    worker.join().unwrap();
}
