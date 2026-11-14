#![cfg(feature = "rust-chatgpt-status")]
//! signin_drive's front halves over a real front and a seeded data directory (council p9-3): the
//! account re-check's query against core's own schema (an active, a disabled and a missing
//! account) and the engine over the front's broker. No real account, token or credential; nothing
//! in the shipped front calls src/signin_drive.rs yet.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::signin_drive::{
    account_check, front_engine, front_live, SigninPlan, NOT_ACTIVE,
};
use noevia_server::signin_host::SigninBinding;
use noevia_server::{config::Config, App};
use server_store::rusqlite::Connection;
use std::collections::HashMap;

const ACTIVE: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const DISABLED: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ZERO_DISABLED: &str = "cccccccc-cccc-cccc-cccc-cccccccccccc";
const MISSING: &str = "dddddddd-dddd-dddd-dddd-dddddddddddd";

/// core's schema with three QA accounts: active, disabled at a time, and `disabled_at = 0`
/// (core's `asUser` reads 0 as not disabled).
fn seed(dir: &std::path::Path) {
    std::fs::write(dir.join("secrets.key"), [7u8; 32]).unwrap();
    let db = Connection::open(dir.join("cowork.db")).unwrap();
    db.execute_batch(include_str!(
        "../../../crates/server-store/tests/fixtures/node-schema.sql"
    ))
    .unwrap();
    for (id, disabled) in [
        (ACTIVE, None),
        (DISABLED, Some(5_i64)),
        (ZERO_DISABLED, Some(0)),
    ] {
        db.execute(
            "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,disabled_at,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic','member','x',?1,?2,1,1)",
            server_store::rusqlite::params![id, disabled],
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("users").join(id)).unwrap();
    }
}

fn front(dir: &std::path::Path, sign_in: bool) -> std::sync::Arc<App> {
    let mut env = HashMap::from([
        ("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".to_string()),
        ("UI_DATA_DIR", dir.display().to_string()),
        ("PUBLIC_ORIGIN", "http://127.0.0.1:9".to_string()),
    ]);
    if sign_in {
        env.insert("NOEVIA_FEATURE_CHATGPT_OAUTH", "1".to_string());
    }
    App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap())
}

fn plan(owner: &str) -> SigninPlan {
    SigninPlan {
        owner: owner.to_owned(),
        conversation: Some("qa-chat-1".to_owned()),
        binding: SigninBinding {
            url: "http://x/v1/chat/completions".to_owned(),
            model: "m-large".to_owned(),
            expires_ms: 1.0e15,
        },
    }
}

#[test]
fn the_account_check_reads_cores_as_user() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let app = front(d.path(), true);
    let live = front_live(&app);
    assert_eq!(live(ACTIVE), Ok(true));
    assert_eq!(live(ZERO_DISABLED), Ok(true));
    assert_eq!(live(DISABLED), Ok(false));
    assert_eq!(live(MISSING), Ok(false));
    assert_eq!(account_check(front_live(&app), ACTIVE.to_owned())(), Ok(()));
    assert_eq!(
        account_check(front_live(&app), DISABLED.to_owned())(),
        Err(NOT_ACTIVE)
    );
    assert_eq!(
        account_check(front_live(&app), MISSING.to_owned())(),
        Err(NOT_ACTIVE)
    );
}

#[test]
fn an_account_disabled_mid_turn_fails_the_next_check() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let app = front(d.path(), true);
    let check = account_check(front_live(&app), ACTIVE.to_owned());
    assert_eq!(check(), Ok(()));
    let db = Connection::open(d.path().join("cowork.db")).unwrap();
    db.execute("UPDATE users SET disabled_at=9 WHERE id=?1", [ACTIVE])
        .unwrap();
    assert_eq!(check(), Err(NOT_ACTIVE));
}

#[test]
fn the_engine_is_the_plans_over_the_fronts_broker() {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    let app = front(d.path(), true);
    assert!(
        app.chatgpt_oauth.is_some(),
        "the front has a sign-in broker"
    );
    let engine = front_engine(&app, &plan(ACTIVE)).unwrap();
    assert_eq!(engine.user, ACTIVE);
    assert_eq!(engine.conversation.as_deref(), Some("qa-chat-1"));
}
