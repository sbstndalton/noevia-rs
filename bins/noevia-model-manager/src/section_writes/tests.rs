#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::cfg::*;
use super::*;
use proptest::prelude::*;

#[test]
fn splitlines_matches_python() {
    assert_eq!(
        splitlines_keep("a\r\nb\rc\x0bd\u{2028}e"),
        ["a\r\n", "b\r", "c\x0b", "d\u{2028}", "e"]
    );
    assert_eq!(splitlines("a\r\nb\n"), ["a", "b"]);
    assert!(splitlines_keep("").is_empty());
}

#[test]
fn section_names_reject_a_trailing_newline() {
    // noevia#1934: Python's `$` accepted one trailing newline, which wrote an unparseable file.
    assert!(!valid_section_name("foo\n"));
    assert!(!valid_section_name("\n"));
    assert!(!valid_section_name("foo\n\n"));
    assert!(!valid_section_name(""));
    assert!(valid_section_name("foo-1.2_b+c"));
}

#[test]
fn headers_and_duplicates() {
    assert_eq!(header_match("  [a] ; c"), Some((3, 4)));
    assert_eq!(header_match("[a] junk"), None);
    assert_eq!(header_match("[]"), None);
    assert_eq!(duplicate_sections("[a]\n[b]\n[a]\n[a]\n[b]\n"), ["a", "b"]);
}

#[test]
fn configparser_reads() {
    let p = parse("v = 1\n[a]\nk = v\n  more\n\n  after blank\n# c\nk2: x = y\n[DEFAULT]\nd = 1\n")
        .unwrap();
    assert_eq!(p.preamble, "v = 1\n");
    assert_eq!(
        p.section("a").unwrap().get("k"),
        Some("v\nmore\n\nafter blank")
    );
    assert_eq!(p.section("a").unwrap().get("k2"), Some("x = y"));
    assert_eq!(p.items("a").unwrap().0[0], ("d".to_owned(), "1".to_owned()));
    assert_eq!(
        parse("[a]\nnot an option\n").unwrap_err(),
        ParseError::Invalid
    );
    assert_eq!(parse("[a]\n= v\n").unwrap_err(), ParseError::Invalid);
    assert_eq!(parse("[\nk = v\n").unwrap_err(), ParseError::Invalid);
    assert!(matches!(parse("[a]\n[a]\n"), Err(ParseError::Duplicate(_))));
}

#[test]
fn cache_ram_rules() {
    assert_eq!(clamp_cache_ram(" 007 ", 100), "7");
    assert_eq!(clamp_cache_ram("-1", 100), "100");
    assert_eq!(clamp_cache_ram("101", 100), "100");
    assert_eq!(clamp_cache_ram("lots", 100), "lots");
    assert_eq!(
        clamp_cache_ram("99999999999999999999999999999999999999999", 5),
        "5"
    );
}

#[test]
fn route_shapes() {
    use crate::{section_write_route, SectionWrite};
    use axum::http::Method;
    assert_eq!(
        section_write_route(&Method::PUT, "/api/v1/models-ini"),
        Some(SectionWrite::ModelsIni)
    );
    assert_eq!(
        section_write_route(&Method::PUT, "/api/v1/sections/a%20b"),
        Some(SectionWrite::Save("a b".into()))
    );
    assert_eq!(
        section_write_route(&Method::DELETE, "/api/v1/sections/%zz"),
        Some(SectionWrite::Delete("%zz".into()))
    );
    assert_eq!(
        section_write_route(&Method::POST, "/api/v1/sections/a/rename"),
        Some(SectionWrite::Rename("a".into()))
    );
    for (m, p) in [
        (Method::GET, "/api/v1/sections/a"),
        (Method::PUT, "/api/v1/sections/"),
        (Method::PUT, "/api/v1/sections/a/"),
        (Method::PUT, "/api/v1/sections/a%2Fb"),
        (Method::POST, "/api/v1/sections/a/other"),
        (Method::POST, "/api/v1/models-ini"),
        (Method::PUT, "/api/v1/models-ini/"),
    ] {
        assert_eq!(section_write_route(&m, p), None, "{m} {p}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // Hostile models.ini text never panics the reader or the renderers, and whatever the
    // preserving renderer returns reads back to exactly the parser it was given.
    #[test]
    fn hostile_ini_is_total(text in "[\\[\\]a-c=:;# \\t\\r\\n\\x0b\u{2028}]{0,120}", key in "[a-c]{1,3}") {
        if let Ok(mut cp) = parse(&text) {
            let _ = render_full(&cp);
            if let Some((name, _)) = cp.sections.first().cloned() {
                if let Some(s) = cp.section_mut(&name) {
                    s.set(&key, "v");
                }
                if let Some(out) = render_preserving(&text, &cp) {
                    let back = parse(&out).unwrap();
                    prop_assert!(back.same_sections(&cp.sections));
                }
                let _ = rename_header(&text, &cp, &name, "renamed");
            }
        }
    }
}

#[test]
fn system_routing_name_rule_matches_python() {
    // `re.match(r"^<role>(?:[_.-]|$)", part, re.I)` for the system routing role.
    let role = SYSTEM_ROUTING_ROLE;
    let upper = role.to_uppercase();
    let short = &role[..role.len() - 1];
    for (part, want) in [
        (role.to_owned(), true),
        (upper.clone(), true),
        (format!("{upper}_x"), true),
        (format!("{role}.gguf"), true),
        (format!("{role}-"), true),
        (format!("{role}\n"), true),
        (format!("{role}\nnext"), false),
        (format!("{role}b"), false),
        (short.to_owned(), false),
        (format!("x{role}"), false),
        (String::new(), false),
        (format!("{short}\u{e4}"), false),
        (format!("{role} \u{e4}"), false),
    ] {
        assert_eq!(system_routing_name(&part), want, "{part:?}");
    }
}

fn migration_dir(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    std::fs::write(&ini, text).unwrap();
    (dir, ini)
}

fn files(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (
                e.file_name().into_string().unwrap(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

/// noevia#2010: the startup migration waits for the writers' lock and keeps it across the write,
/// so a writer queued behind it only ever sees the migrated file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_migration_holds_the_write_lock_across_the_write() {
    let (dir, ini) = migration_dir("[chat]\nmodel = /models/synthetic.gguf\n");
    let writes = Arc::new(SectionWrites::new(&ini, (1024, 2048)));
    let held = writes.lock().await;
    let task = tokio::spawn(writes.clone().migrate_cache_ram_at_startup());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!task.is_finished(), "migration ran without the lock");
    assert_eq!(
        std::fs::read_to_string(&ini).unwrap(),
        "[chat]\nmodel = /models/synthetic.gguf\n"
    );
    // tokio's mutex is fair: the migration is first in line, this writer second.
    let next = tokio::spawn({
        let writes = writes.clone();
        let ini = ini.clone();
        async move {
            let _guard = writes.lock().await;
            std::fs::read_to_string(&ini).unwrap()
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    drop(held);
    assert_eq!(task.await.unwrap().unwrap(), ["chat"]);
    assert_eq!(
        next.await.unwrap(),
        "[chat]\nmodel = /models/synthetic.gguf\ncache-ram = 1024\n"
    );
    assert!(!write_lock_held(&writes).await);
    drop(dir);
}

#[tokio::test]
async fn startup_migration_is_idempotent_and_writes_nothing_when_nothing_changes() {
    let (dir, ini) = migration_dir(&format!(
        "[chat]\n\n[my-embed]\n\n[{SYSTEM_ROUTING_ROLE}_router]\n"
    ));
    let writes = Arc::new(SectionWrites::new(&ini, (512, 2048)));
    assert_eq!(
        writes.clone().migrate_cache_ram_at_startup().await.unwrap(),
        ["chat", "my-embed"]
    );
    let after = files(dir.path());
    assert_eq!(after.len(), 2, "models.ini and one rotating backup");
    assert!(writes
        .clone()
        .migrate_cache_ram_at_startup()
        .await
        .unwrap()
        .is_empty());
    assert_eq!(files(dir.path()), after, "the second run wrote nothing");
}

#[tokio::test]
async fn startup_migration_failures_write_nothing() {
    for text in ["[a]\n[a]\n", "[a]\ngarbage line\n"] {
        let (dir, ini) = migration_dir(text);
        let writes = Arc::new(SectionWrites::new(&ini, (1024, 2048)));
        let before = files(dir.path());
        assert!(
            writes.migrate_cache_ram_at_startup().await.is_err(),
            "{text:?}"
        );
        assert_eq!(files(dir.path()), before, "{text:?}");
    }
    let dir = tempfile::tempdir().unwrap();
    let writes = Arc::new(SectionWrites::new(
        &dir.path().join("models.ini"),
        (1024, 2048),
    ));
    assert!(writes
        .migrate_cache_ram_at_startup()
        .await
        .unwrap()
        .is_empty());
    assert!(
        files(dir.path()).is_empty(),
        "a missing file is not created"
    );
}

/// A child that answers its startup-ready advertisement (so the native writers know its startup is over).
async fn healthy_child() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/api/v1/internal/startup-ready",
        axum::routing::get(|| async { r#"{"startup_ready":1}"# }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, server)
}

/// True when the writers' lock is taken (the next write would wait).
async fn write_lock_held(writes: &SectionWrites) -> bool {
    tokio::time::timeout(std::time::Duration::from_millis(100), writes.lock())
        .await
        .is_err()
}

/// noevia#2006: a section delete dropped while it writes (the client went away) keeps the
/// request's admission permit, as it keeps the write lock, until the write has returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_write_keeps_its_permit_and_lock_until_the_write_ends() {
    use crate::test_support::{all_permits_back, dropped_in_blocking_work, test_state};
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    std::fs::write(&ini, "[synthetic]\nmodel = /models/synthetic.gguf\n").unwrap();
    let (base, server) = healthy_child().await;
    let writes = Arc::new(SectionWrites::new(&ini, (0, 0)));
    let mut state = test_state(&base);
    state.section_writes = Some(writes.clone());
    let state = Arc::new(state);
    let request = axum::http::Request::delete("/api/v1/sections/synthetic?baseRevision=synthetic")
        .body(axum::body::Body::empty())
        .unwrap();
    let open = dropped_in_blocking_work(&state, request).await;
    assert!(
        write_lock_held(&writes).await,
        "write lock released mid-write"
    );
    open.send(()).unwrap();
    all_permits_back(&state).await;
    assert!(!write_lock_held(&writes).await);
    server.abort();
}
