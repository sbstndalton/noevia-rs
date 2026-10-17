#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use proptest::prelude::*;

#[test]
fn normpath_matches_posixpath() {
    // Expected values are CPython 3.12 `posixpath.normpath`.
    for (input, expected) in [
        ("", "."),
        ("/", "/"),
        ("//", "//"),
        ("///", "/"),
        ("//a//b/", "//a/b"),
        ("/a/./b/../c", "/a/c"),
        ("/../a", "/a"),
        ("a/../..", ".."),
        ("../../a", "../../a"),
        ("./", "."),
        ("/a/b/..", "/a"),
    ] {
        assert_eq!(normpath(input), expected, "{input:?}");
    }
}

#[test]
fn unescape_is_code_points_left_to_right() {
    assert_eq!(unescape("a\\040b"), "a b");
    assert_eq!(unescape("\\777"), "\u{1ff}");
    assert_eq!(unescape("\\303\\244"), "\u{c3}\u{a4}");
    assert_eq!(unescape("\\8\\12\\1234"), "\\8\\12S4");
    assert_eq!(unescape("\\\\134"), "\\\\");
}

#[test]
fn declared_names_strip_python_whitespace_and_cap() {
    let names = declared_names("\u{2003}d\u{2003},\u{1f}e\u{1c},, ,x").unwrap();
    assert_eq!(names.into_iter().collect::<Vec<_>>(), ["d", "e", "x"]);
    let at_cap = (0..NAME_CAP)
        .map(|i| format!("n{i}"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(declared_names(&at_cap).is_some());
    assert!(declared_names(&format!("{at_cap},extra")).is_none());
}

#[test]
fn normal_root_is_pathlib_normal_form() {
    for ok in ["/", "/models", "/a/b c/d"] {
        assert!(normal_root(ok), "{ok}");
    }
    for bad in [
        "", "models", "/models/", "//models", "/a/./b", "/a/../b", "/a//b", "/a\0",
    ] {
        assert!(!normal_root(bad), "{bad:?}");
    }
}

#[test]
fn root_slash_joins_without_doubling() {
    assert_eq!(join("/", OsStr::new("x")), OsString::from("/x"));
    assert_eq!(join("/m", OsStr::new("x")), OsString::from("/m/x"));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // Hostile mount tables never panic, and every point is in normal form.
    #[test]
    fn hostile_mount_tables_are_total(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        for point in mount_points(Some(&bytes)) {
            prop_assert_eq!(normpath(&point), point.clone());
        }
    }

    // Hostile declared values never panic; names are stripped, non-empty and comma-free.
    #[test]
    fn hostile_declared_values_are_total(value in "\\PC{0,512}") {
        if let Some(names) = declared_names(&value) {
            for name in names {
                prop_assert!(!name.is_empty());
                prop_assert!(!name.contains(','));
                prop_assert_eq!(name.trim_matches(py_space), name.as_str());
            }
        }
    }

    // Any declared value over an empty folder: only the folder itself, or real declared folders.
    #[test]
    fn answers_only_list_existing_plain_folders(value in "[a-c./ ,]{0,24}") {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(root.join("a")).unwrap();
        let root = root.to_str().unwrap().to_owned();
        let Outcome::Json(body) = targets(&root, &value, Path::new("/nonexistent-synthetic")) else {
            return Err(TestCaseError::fail("failed"));
        };
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<_> = json["targets"].as_array().unwrap().iter()
            .map(|t| t["id"].as_str().unwrap().to_owned()).collect();
        prop_assert_eq!(&ids[0], "");
        for id in &ids[1..] {
            prop_assert_eq!(id, "a");
        }
    }
}

/// noevia#2006: a request dropped while the targets are read (the client went away) keeps its
/// admission permit until the read has returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_request_keeps_its_permit_until_the_read_ends() {
    use crate::test_support::{all_permits_back, dropped_in_blocking_work, test_state};
    let dir = tempfile::tempdir().unwrap();
    let mut state = test_state(crate::LEGACY_BASE);
    state.download_targets = Some(std::sync::Arc::new(DownloadTargets::new(
        "/models",
        "",
        &dir.path().join("mountinfo"),
    )));
    let state = std::sync::Arc::new(state);
    let request = axum::http::Request::get("/api/v1/download-targets")
        .body(axum::body::Body::empty())
        .unwrap();
    let open = dropped_in_blocking_work(&state, request).await;
    open.send(()).unwrap();
    all_permits_back(&state).await;
}
