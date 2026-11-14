//! Council p8-3 (noevia#2731 item 4): `chat_provider::setup`, what chat_native.rs `drive` runs for a handed-off
//! frame, with the front's switch on, over a real `App` (the Node schema's `users` table read by the front's
//! writer) and real provider files. Its own test binary: it sets the switch and `MEMBER_OUTBOUND_ORIGINS` for the
//! whole process, so no other test sees them.
//!
//! Synthetic data only; no address is contacted.
#![cfg(feature = "rust-storage-reads")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use chat_turn::prepared::Prepared;
use noevia_server::chat_provider::{self, Setup};
use noevia_server::config::Config;
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const MEMBER: &str = "00000000-0000-4000-8000-0000000000bb";
const ORIGIN: &str = "http://127.0.0.1:18021";
const START: &str = "0123456789abcdef0123456789abcdef";
const URL: &str = "https://qa-provider.example/v1/chat/completions";
const APPROVED: &str = "https://qa-provider.example, https://qa.nvidia.com";
const HOSTED: &str = r#","runtimeLimit":{"tokens":32768,"source":"Default for hosted providers"}"#;

fn app(data: &Path) -> Arc<noevia_server::App> {
    let c = Connection::open(server_store::db_path(data)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute(
        "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,disabled_at,created_at,updated_at) VALUES(?1,'qa-member','qa-member','QA member','member','x','qa-webauthn-1',NULL,1,1)",
        [MEMBER],
    )
    .unwrap();
    let mut env: HashMap<&str, String> = HashMap::new();
    // Never contacted: setup reads only the account store and the provider files.
    env.insert("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into());
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    env.insert("NOEVIA_START_ID", START.into());
    noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap())
}

fn write_rows(data: &Path, rows: &str) {
    let d = data.join("users").join(MEMBER);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("providers.json"),
        format!(r#"{{"providers":[{rows}]}}"#),
    )
    .unwrap();
}

fn own_row(id: &str, base: &str, extra: &str) -> String {
    format!(r#"{{"id":"{id}","label":"QA provider","baseUrl":"{base}","apiKey":""{extra}}}"#)
}

fn frame(provider: &str, url: &str, extra: &str) -> Prepared {
    let f = js_json::parse(&format!(
        r#"{{"type":"prepared","v":1,"startedAt":1,"model":"qa-model","effort":"default",
"provider":{{"id":"{provider}","label":"QA provider","localEngine":false,"capabilities":null}},
"upstreamUrl":"{url}","limitScope":"qa","messages":[],"tools":[],"routes":[],
"writes":[],"public":[],"automaticOnlyWrites":[],"managerEnabled":false,"modelLease":false,
"chat":{{"id":"qa-chat","key":"qa-chat","grantScope":"chat","grantHolder":null,"recentWrites":[],
"title":{{"chatId":"qa-chat","firstMessage":"QA","enabled":false}}}},
"turn":{{"message":"QA","streamIdleMs":0}}{extra}}}"#
    ))
    .unwrap();
    Prepared::read(&f).unwrap_or_else(|e| panic!("{e:?}"))
}

fn plain(s: &Setup) -> bool {
    matches!(s, Setup::Plain)
}

fn refused(s: &Setup) -> bool {
    matches!(s, Setup::Refused)
}

fn provider(s: &Setup) -> bool {
    matches!(s, Setup::Provider(_))
}

/// One test, in order: the process environment is shared by everything in it.
#[tokio::test(flavor = "multi_thread")]
async fn setup_with_the_switch_on_reads_the_account_and_the_row_now() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let app = app(&data);
    std::env::remove_var("DEFAULT_PROVIDER_ID");
    // Both origins approved, so the trial-terms row passes the cut and meets the external rule.
    std::env::set_var("MEMBER_OUTBOUND_ORIGINS", APPROVED);
    write_rows(
        &data,
        &[
            own_row("qa-provider", "https://qa-provider.example/v1", ""),
            own_row(
                "qa-flagged",
                "https://qa-provider.example/v1",
                r#","external":true"#,
            ),
            own_row("qa-trial", "https://qa.nvidia.com/v1", ""),
        ]
        .join(","),
    );
    let hosted = frame("qa-provider", URL, HOSTED);

    // Switch unset: today's path for every frame, whatever it names.
    std::env::remove_var(chat_provider::SWITCH);
    assert!(!chat_provider::offered());
    assert!(plain(&chat_provider::setup(&app, &data, MEMBER, &hosted).await));
    // Anything but `1` is off.
    std::env::set_var(chat_provider::SWITCH, "true");
    assert!(plain(&chat_provider::setup(&app, &data, MEMBER, &hosted).await));

    std::env::set_var(chat_provider::SWITCH, "1");
    assert!(chat_provider::offered());
    // The default row: as before.
    let local = frame("default", "http://127.0.0.1:9/v1/chat/completions", "");
    assert!(plain(&chat_provider::setup(&app, &data, MEMBER, &local).await));
    // The member's own row, its origin approved, the frame agreeing: cut.
    let s = chat_provider::setup(&app, &data, MEMBER, &hosted).await;
    assert!(provider(&s));
    // A frame that disagrees with the row: refused before anything is sent.
    let moved = frame(
        "qa-provider",
        "https://elsewhere.example/v1/chat/completions",
        HOSTED,
    );
    assert!(refused(&chat_provider::setup(&app, &data, MEMBER, &moved).await));
    assert!(refused(
        &chat_provider::setup(&app, &data, MEMBER, &frame("qa-provider", URL, "")).await
    ));
    // External rows stay core's.
    let flagged = frame("qa-flagged", URL, HOSTED);
    assert!(refused(&chat_provider::setup(&app, &data, MEMBER, &flagged).await));
    let trial = frame("qa-trial", "https://qa.nvidia.com/v1/chat/completions", HOSTED);
    assert!(refused(&chat_provider::setup(&app, &data, MEMBER, &trial).await));
    // A row this account does not have, and an account the store does not know.
    let other = frame("qa-other", URL, HOSTED);
    assert!(refused(&chat_provider::setup(&app, &data, MEMBER, &other).await));
    assert!(refused(
        &chat_provider::setup(&app, &data, "00000000-0000-4000-8000-0000000000cc", &hosted).await
    ));
    // The member's origin no longer approved: Node's own refusal.
    std::env::set_var("MEMBER_OUTBOUND_ORIGINS", "");
    assert!(refused(&chat_provider::setup(&app, &data, MEMBER, &hosted).await));
    std::env::set_var("MEMBER_OUTBOUND_ORIGINS", APPROVED);
    assert!(provider(&chat_provider::setup(&app, &data, MEMBER, &hosted).await));
    // The account disabled since: read now, refused.
    let c = Connection::open(server_store::db_path(&data)).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    c.execute("UPDATE users SET disabled_at=5 WHERE id=?1", [MEMBER])
        .unwrap();
    assert!(refused(&chat_provider::setup(&app, &data, MEMBER, &hosted).await));
    c.execute("UPDATE users SET disabled_at=NULL WHERE id=?1", [MEMBER])
        .unwrap();
    assert!(provider(&chat_provider::setup(&app, &data, MEMBER, &hosted).await));
    // The row deleted since: refused.
    write_rows(&data, "");
    assert!(refused(&chat_provider::setup(&app, &data, MEMBER, &hosted).await));
    std::env::remove_var(chat_provider::SWITCH);
}
