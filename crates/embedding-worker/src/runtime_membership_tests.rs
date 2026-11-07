#![allow(clippy::unwrap_used)]
use super::*;
#[path = "../tests/support/mod.rs"]
mod support;
use server_projects::outbox::{self, Job};
use std::sync::atomic::AtomicUsize;
use support::*;
const USER: &str = "11111111-1111-4111-8111-111111111111";
const OTHER: &str = "22222222-2222-4222-8222-222222222222";
#[derive(Clone, Copy)]
enum Missing {
    File,
    Project,
    Account,
    Tenant,
}
async fn refuses_notification(missing: Missing) {
    let (_tmp, mut store) = support::seed();
    store.seed.next = None;
    let auth = rusqlite::Connection::open(store.root.join("cowork.db")).unwrap();
    for user in [USER, OTHER] {
        auth.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,'Synthetic','member','synthetic',?1,0,0)",[user]).unwrap();
        let dir = store.root.join("users").join(user);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("projects.json"),
            r#"{"projects":[{"id":"p","files":[{"name":"a.md","content":"synthetic source"}]}]}"#,
        )
        .unwrap();
        outbox::enqueue(&dir, &[], 0).unwrap();
    }
    let dir = store.root.join("users").join(USER);
    let local = if matches!(missing, Missing::Tenant) {
        outbox::enqueue(
            &dir,
            &[Job {
                kind: "index",
                project: "p",
                file: Some("a.md"),
                payload: None,
            }],
            1,
        )
        .unwrap();
        Some(outbox::claim_index(&dir, false, 2).unwrap().unwrap())
    } else {
        None
    };
    let owner = if matches!(missing, Missing::Tenant) {
        OTHER
    } else {
        USER
    };
    let job_dir = store.root.join("users").join(owner);
    outbox::enqueue(
        &job_dir,
        &[Job {
            kind: "index",
            project: "p",
            file: Some("a.md"),
            payload: None,
        }],
        1,
    )
    .unwrap();
    let row = outbox::claim_index(&job_dir, false, 2).unwrap().unwrap();
    if let Some(local) = local {
        assert_eq!(local.key, row.key);
        assert_eq!(local.generation, row.generation);
    }
    match missing {
        Missing::File => std::fs::write(
            dir.join("projects.json"),
            r#"{"projects":[{"id":"p","files":[]}]}"#,
        )
        .unwrap(),
        Missing::Project => {
            std::fs::write(dir.join("projects.json"), r#"{"projects":[]}"#).unwrap()
        }
        Missing::Account => {
            auth.execute("DELETE FROM users WHERE id=?", [USER])
                .unwrap();
        }
        // Both tenants contain p/a.md. Both have equal running keys/generations; this claim originated in OTHER.
        Missing::Tenant => {}
    }
    let notices = Arc::new(AtomicUsize::new(0));
    let seen = notices.clone();
    let stub = server(move |path, _| {
        if path == "/internal/embedding-worker-owner/source-changed" {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        default_reply(path)
    })
    .await;
    let host = NativeHost {
        store,
        http: Http::new("", None, &stub.base, None)
            .unwrap()
            .with_ownership(&stub.base, "synthetic-token", START),
        valid: Arc::new(AtomicBool::new(true)),
        off: Arc::new(AtomicBool::new(false)),
        service_base: None,
        core: stub.base.clone(),
        token: zeroize::Zeroizing::new("synthetic-token".into()),
        start: START.into(),
    };
    let (_stop, mut rx) = watch::channel(false);
    let attempt = host.run_rag(USER, &row, &mut rx).await;
    assert!(
        attempt.result.is_ok(),
        "non-member text work remains a no-op"
    );
    assert_eq!(
        notices.load(Ordering::SeqCst),
        0,
        "non-member source must never notify media"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_file_does_not_notify_media() {
    refuses_notification(Missing::File).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removed_project_does_not_notify_media() {
    refuses_notification(Missing::Project).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inactive_account_does_not_notify_media() {
    refuses_notification(Missing::Account).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_tenant_claim_does_not_notify_media_even_with_matching_names() {
    refuses_notification(Missing::Tenant).await;
}
