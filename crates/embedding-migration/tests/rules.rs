//! Rules the recorded scenarios do not reach on their own: JavaScript coercions, the saved bytes,
//! the publication flock and the address rules.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use embedding_migration::config::{flock, lock_path, state_text, EmbeddingConfig, Options};
use embedding_migration::js;
use embedding_migration::migration::{cosine, number};
use embedding_migration::served::{check_served, looks_like_path, slice_units};
use embedding_migration::target::{
    normalize_base_url, normalize_target, same_target, Profiles, State, Target,
};
use serde_json::{json, Value};

fn profiles() -> Profiles {
    Profiles {
        default: "qa-plain".into(),
        known: vec!["qa-plain".into(), "qa-framed".into()],
    }
}

#[test]
fn number_reads_strings_as_javascript_does() {
    for (text, want) in [
        ("", 0.0),
        ("  ", 0.0),
        (" 0.25 ", 0.25),
        ("0x10", 16.0),
        ("0b11", 3.0),
        ("0o7", 7.0),
        ("1e2", 100.0),
        (".5", 0.5),
        ("-2", -2.0),
        ("+3", 3.0),
        ("\u{feff}4", 4.0),
    ] {
        assert_eq!(js::number(text), want, "{text:?}");
    }
    assert_eq!(js::number("Infinity"), f64::INFINITY);
    for junk in [
        "abc", "inf", "nan", "infinity", "1e", "0x", "0xg", "1 2", "\u{85}1",
    ] {
        assert!(js::number(junk).is_nan(), "{junk:?}");
    }
    assert_eq!(js::round(2.5), 3.0);
    assert_eq!(js::round(-2.5), -2.0);
    assert_eq!(js::round(-0.4), 0.0);
}

#[test]
fn trim_is_javascripts_not_rusts() {
    assert_eq!(js::trim("\u{feff} a \u{3000}"), "a");
    assert_eq!(js::trim("\u{85}a"), "\u{85}a");
}

#[test]
fn integral_numbers_print_without_a_fraction() {
    assert_eq!(number(4.0).to_string(), "4");
    assert_eq!(number(0.5).to_string(), "0.5");
    assert_eq!(number(f64::NAN), Value::Null);
}

#[test]
fn the_saved_bytes_are_json_stringify_with_two_spaces() {
    let s = State {
        active: Target {
            base_url: None,
            model: "qa-a".into(),
            profile: "qa-plain".into(),
        },
        next: Some(Target {
            base_url: Some("http://10.0.0.5:8080".into()),
            model: "qa-\"b\"".into(),
            profile: "qa-framed".into(),
        }),
        previous: None,
    };
    let want = "{\n  \"version\": 1,\n  \"active\": {\n    \"baseUrl\": null,\n    \"model\": \"qa-a\",\n    \"profile\": \"qa-plain\"\n  },\n  \"next\": {\n    \"baseUrl\": \"http://10.0.0.5:8080\",\n    \"model\": \"qa-\\\"b\\\"\",\n    \"profile\": \"qa-framed\"\n  },\n  \"previous\": null\n}";
    assert_eq!(state_text(&s), want);
    let back: Value = serde_json::from_str(&state_text(&s)).unwrap();
    assert_eq!(back["next"]["model"], "qa-\"b\"");
}

#[test]
fn addresses_keep_their_spelling_without_trailing_slashes() {
    assert_eq!(
        normalize_base_url(Some(&json!(" http://10.0.0.5:8080/v1// ")))
            .unwrap()
            .as_deref(),
        Some("http://10.0.0.5:8080/v1")
    );
    assert_eq!(normalize_base_url(Some(&json!(""))).unwrap(), None);
    assert_eq!(normalize_base_url(None).unwrap(), None);
    assert_eq!(
        normalize_base_url(Some(&json!("http://10.0.0.5/?#")))
            .unwrap()
            .as_deref(),
        Some("http://10.0.0.5/?#")
    );
    for bad in [
        "ftp://10.0.0.5",
        "http://u@10.0.0.5",
        "http://10.0.0.5/?a",
        "http://10.0.0.5/#a",
        "10.0.0.5:80x",
        "file:///x",
    ] {
        let e = normalize_base_url(Some(&json!(bad))).unwrap_err();
        assert_eq!(
            (e.status, e.code),
            (Some(400), Some("invalidTarget")),
            "{bad}"
        );
    }
    // A non-scalar address is the string JavaScript builds, refused as a URL.
    assert!(normalize_base_url(Some(&json!({"a": 1}))).is_err());
}

#[test]
fn targets_need_a_known_profile_and_a_plain_model() {
    let p = profiles();
    let t = normalize_target(
        Some(&json!({"model": " qa-a ", "profile": " qa-framed "})),
        &p,
    )
    .unwrap();
    assert_eq!(
        (t.model.as_str(), t.profile.as_str(), t.base_url),
        ("qa-a", "qa-framed", None)
    );
    let e =
        normalize_target(Some(&json!({"model": "qa-a", "profile": "qa-other"})), &p).unwrap_err();
    assert_eq!(
        e.message,
        "Unknown embedding profile \"qa-other\" (known: qa-plain, qa-framed)."
    );
    assert!(normalize_target(Some(&json!({"model": "a\u{1f}b"})), &p).is_err());
    assert!(normalize_target(Some(&json!([])), &p).is_err());
    assert_eq!(
        normalize_target(Some(&json!({"model": 1.5})), &p)
            .unwrap()
            .model,
        "1.5"
    );
    let a = Target {
        base_url: Some(String::new()),
        model: "m".into(),
        profile: "p".into(),
    };
    let b = Target {
        base_url: None,
        ..a.clone()
    };
    assert!(same_target(Some(&a), Some(&b)));
}

#[test]
fn served_checks_match_names_not_files() {
    assert!(check_served(Some(&[json!("QA-A:latest")]), " qa-a ").is_ok());
    let e = check_served(Some(&[json!("/srv/x.gguf")]), "qa-a").unwrap_err();
    assert!(e.message.contains("--alias qa-a"));
    assert_eq!(
        check_served(None, "qa-a").unwrap_err().code,
        Some("endpointUnverified")
    );
    assert!(
        looks_like_path("d:/x") && looks_like_path("w.SAFETENSORS") && !looks_like_path("qa-a")
    );
    assert_eq!(slice_units("\u{1F600}\u{1F600}", 3), "\u{1F600}");
}

#[test]
fn cosine_of_mismatched_or_zero_vectors_is_nan() {
    assert!(cosine(&[1.0], &[1.0, 2.0]).is_nan());
    assert!(cosine(&[], &[]).is_nan());
    assert!(cosine(&[0.0, 0.0], &[1.0, 0.0]).is_nan());
    assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-12);
}

#[test]
fn a_change_needs_the_publication_lock_and_waits_for_it() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("embedding-config.json");
    let env = |k: &str| (k == "EMBEDDING_MODEL").then(|| "qa-a".to_owned());
    let (mut config, _) = EmbeddingConfig::load(Options {
        env: &env,
        state_file: Some(file.clone()),
        fallback_model: "default",
        profiles: profiles(),
        native_owner: true,
    });
    let t = Target {
        base_url: None,
        model: "qa-b".into(),
        profile: "qa-plain".into(),
    };
    // Outside the lock: refused, nothing written.
    let e = config.set_next(t.clone()).unwrap_err();
    assert_eq!(e.status, None);
    assert!(!file.exists());
    // Held by another writer: the change waits, then gives up.
    let held = flock(&file).unwrap();
    let started = std::time::Instant::now();
    let e = config.with_lock(|c| c.set_next(t.clone())).unwrap_err();
    assert_eq!(e.message, "Embedding config lock timed out");
    assert!(started.elapsed() >= std::time::Duration::from_secs(4));
    drop(held);
    config
        .with_lock(|c| c.set_next(t.clone()))
        .unwrap()
        .unwrap();
    assert!(lock_path(&file).exists());
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["next"]["model"], "qa-b");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // The saved file wins over the environment on the next start.
    let (again, logs) = EmbeddingConfig::load(Options {
        env: &env,
        state_file: Some(file.clone()),
        fallback_model: "default",
        profiles: profiles(),
        native_owner: true,
    });
    assert!(logs.is_empty());
    assert_eq!(again.get().source, "saved");
    assert_eq!(again.next().unwrap().model, "qa-b");
}

#[cfg(unix)]
#[test]
fn an_unreadable_state_file_is_damaged_not_absent() {
    let tmp = tempfile::tempdir().unwrap();
    // A directory where the file should be: reading it fails with something other than NotFound.
    let file = tmp.path().join("embedding-config.json");
    std::fs::create_dir(&file).unwrap();
    let env = |k: &str| (k == "EMBEDDING_MODEL").then(|| "qa-a".to_owned());
    let (config, logs) = EmbeddingConfig::load(Options {
        env: &env,
        state_file: Some(file),
        fallback_model: "default",
        profiles: profiles(),
        native_owner: true,
    });
    assert!(config.damaged.is_some());
    assert_eq!(config.active().model, "unavailable");
    assert!(logs[0].ends_with("Embeddings are off until then."));
}
