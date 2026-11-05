//! Native approval registry contracts and concurrency regressions; synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
#[path = "support/approval_front.rs"]
mod fixture;
use fixture::*;
use serde_json::{json, Value};
use server_store::rusqlite::Connection;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

#[tokio::test]
async fn identity_and_body_policy_precede_pending_state_and_route_decisions_preserve_remember() {
    let f = front().await;
    assert_eq!(f.open("ap-a", "write", json!({})).await.0, 200);
    let post = r#"{"decision":"approve"}"#;
    assert_eq!(
        f.call("/api/tool-approvals/ap-a", post, false, false)
            .await
            .0,
        401
    );
    assert_eq!(
        f.call("/api/tool-approvals/ap-a", post, true, false)
            .await
            .0,
        403
    );
    for body in ["null", "[]", "1", "true", "{}{"] {
        assert_eq!(f.decide("ap-a", body).await.0, 400, "{body}");
    }
    assert_eq!(f.decide("%ZZ", post).await.0, 400);
    assert_eq!(
        f.decide("ap-a", r#"{"decision":"cloud"}"#).await,
        (
            400,
            json!({"error":"decision must be 'approve', 'deny' or 'approve_all'"})
        ),
        "a routing choice on a write card"
    );
    // None of that consumed the question.
    assert_eq!(f.decide("ap-a", post).await, (200, json!({"ok":true})));
    assert_eq!(f.wait("ap-a").await, (200, json!({"outcome":"approve"})));
    assert_eq!(f.decide("ap-a", post).await.0, 404, "single use");
    // A routing question (#778): its own choices, remember strictly boolean.
    assert_eq!(
        f.open("route-1", "route", json!({"chat":"qa-one"})).await.0,
        200
    );
    assert_eq!(
        f.decide("route-1", post).await,
        (400, json!({"error":"decision must be 'cloud' or 'local'"}))
    );
    assert_eq!(
        f.decide("route-1", r#"{"decision":"local","remember":true}"#)
            .await,
        (200, json!({"ok":true}))
    );
    assert_eq!(
        f.wait("route-1").await,
        (200, json!({"outcome":"local","remember":true}))
    );
    assert_eq!(f.open("route-2", "route", json!({})).await.0, 200);
    assert_eq!(
        f.decide("route-2", r#"{"decision":["cloud"],"remember":1}"#)
            .await
            .0,
        200
    );
    assert_eq!(
        f.wait("route-2").await,
        (200, json!({"outcome":"cloud","remember":false}))
    );
}

#[tokio::test]
async fn missing_other_user_and_disabled_account_never_return_success() {
    let f = front().await;
    assert_eq!(
        f.decide("nobody", r#"{"decision":"bogus"}"#).await,
        (404, json!({"error":"no such pending approval"}))
    );
    // Another account's question: invisible, even to the coercion (owner check comes first).
    let open = json!({"user":STRANGER,"id":"theirs","kind":"write"});
    assert_eq!(f.op("approvals/open", open).await.0, 200);
    assert_eq!(f.decide("theirs", r#"{"decision":"approve"}"#).await.0, 404);
    assert_eq!(
        f.decide("theirs", r#"{"decision":{"toString":null}}"#)
            .await
            .0,
        404
    );
    assert_eq!(
        f.op("approvals/cancel", json!({"user":f.user,"id":"theirs"}))
            .await,
        (200, json!({"cancelled":false}))
    );
    assert_eq!(
        f.op("approvals/wait", json!({"user":f.user,"id":"theirs"}))
            .await
            .0,
        404
    );
    // The owner's own: JavaScript coercion errors survive, after the lookup.
    assert_eq!(f.open("mine", "write", json!({})).await.0, 200);
    assert_eq!(
        f.decide("mine", r#"{"decision":{"toString":null}}"#)
            .await
            .0,
        500
    );
    // A disabled account answers nothing and consumes nothing.
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute("UPDATE users SET disabled_at=1 WHERE id=?1", [&f.user])
        .unwrap();
    assert_eq!(f.decide("mine", r#"{"decision":"approve"}"#).await.0, 401);
    // The question is still open (disabling ended the session, so ask the store directly).
    assert_eq!(
        f.op("approvals/cancel", json!({"user":f.user,"id":"mine"}))
            .await,
        (200, json!({"cancelled":true}))
    );
}

#[tokio::test]
async fn a_held_account_lock_is_a_generic_503_and_consumes_nothing() {
    let f = front().await;
    assert_eq!(f.open("busy", "write", json!({})).await.0, 200);
    let c = Connection::open(server_store::db_path(&f.data)).unwrap();
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (status, body) = f.decide("busy", r#"{"decision":"approve"}"#).await;
    c.execute_batch("ROLLBACK").unwrap();
    assert_eq!(status, 503, "{body}");
    assert!(!body.to_string().contains("SQLITE"));
    assert_eq!(f.decide("busy", r#"{"decision":"approve"}"#).await.0, 200);
}

/// #679: the person left while the card was open. Node drops its wait; the question is withdrawn,
/// so a later answer finds nothing, is never recorded as declined and grants nothing.
#[tokio::test]
async fn a_dropped_wait_stops_the_question_and_a_late_answer_finds_nothing() {
    let f = front().await;
    let b = f.binding("qa-one", "space:free").await;
    let extra = json!({"chat":"qa-one","scope":"space:free","binding":b});
    assert_eq!(f.open("ap-left", "write", extra).await.0, 200);
    let mut wait = TcpStream::connect(f.store).await.unwrap();
    let body = json!({"user":f.user,"id":"ap-left"}).to_string();
    wait.write_all(format!("POST /v1/approvals/wait HTTP/1.1\r\nhost: 127.0.0.1\r\nx-noevia-store-token: {STORE_TOKEN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(wait);
    let mut gone = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // `kind` is read without consuming: a 404 on a choice the card never takes.
        if f.decide("ap-left", r#"{"decision":"cloud"}"#).await.0 == 404 {
            gone = true;
            break;
        }
    }
    assert!(gone, "withdrawn within 1 s of the dropped wait");
    assert_eq!(
        f.decide("ap-left", r#"{"decision":"approve_all"}"#).await.0,
        404
    );
    assert!(!f.granted("qa-one", "space:free", &b).await);
}

#[tokio::test]
async fn approve_all_grants_its_chat_scope_and_binding_and_a_move_or_delete_revokes_it() {
    let f = front().await;
    let b = f.binding("qa-one", "space:free").await;
    assert!(!f.granted("qa-one", "space:free", &b).await);
    let extra = json!({"chat":"qa-one","scope":"space:free","binding":b});
    assert_eq!(f.open("ap-all", "write", extra.clone()).await.0, 200);
    assert_eq!(
        f.decide("ap-all", r#"{"decision":"approve_all"}"#).await.0,
        200
    );
    assert_eq!(f.wait("ap-all").await, (200, json!({"outcome":"approve"})));
    assert!(f.granted("qa-one", "space:free", &b).await);
    assert!(!f.granted("qa-one", "project:qa-p", &b).await, "#917 scope");
    assert!(!f.granted("qa-one", "space:free", &Value::Null).await);
    let two = f.binding("qa-two", "space:free").await;
    assert!(
        !f.granted("qa-two", "space:free", &two).await,
        "one chat only"
    );
    // The store's move (Node's turn or the public route): the generation moves, the grant goes.
    let (status, _) = f
        .op(
            "chat/move",
            json!({"user":f.user,"chat":"qa-one","projectId":"qa-p"}),
        )
        .await;
    assert_eq!(status, 200);
    assert!(!f.granted("qa-one", "space:free", &b).await);
    let moved = f.binding("qa-one", "project:qa-p").await;
    assert!(!f.granted("qa-one", "project:qa-p", &moved).await);
    // revokeChatGrant.
    let extra = json!({"chat":"qa-two","scope":"space:free","binding":two});
    assert_eq!(f.open("ap-two", "write", extra.clone()).await.0, 200);
    assert_eq!(
        f.decide("ap-two", r#"{"decision":"approve_all"}"#).await.0,
        200
    );
    assert!(f.granted("qa-two", "space:free", &two).await);
    assert_eq!(
        f.op("approvals/revoke", json!({"user":f.user,"chat":"qa-two"}))
            .await,
        (200, json!({"revoked":true}))
    );
    assert!(!f.granted("qa-two", "space:free", &two).await);
    // The public delete drops it too.
    assert_eq!(f.open("ap-three", "write", extra).await.0, 200);
    assert_eq!(
        f.decide("ap-three", r#"{"decision":"approve_all"}"#)
            .await
            .0,
        200
    );
    assert!(f.granted("qa-two", "space:free", &two).await);
    let head = format!(
        "DELETE /api/freechats/qa-two HTTP/1.1\r\nhost: front.test\r\norigin: {ORIGIN}\r\ncookie: {}\r\nx-csrf-token: {}\r\n",
        f.jar, f.csrf
    );
    let (status, body) = http(f.addr, &head, "").await;
    assert_eq!(status, 200, "{body}");
    assert!(!f.granted("qa-two", "space:free", &two).await);
}

#[tokio::test]
async fn store_ops_need_the_secret_and_well_formed_questions() {
    let f = front().await;
    let head = "POST /v1/approvals/open HTTP/1.1\r\nhost: 127.0.0.1\r\n";
    let body = json!({"user":f.user,"id":"x","kind":"write"}).to_string();
    assert_eq!(http(f.store, head, &body).await.0, 401);
    let bad = format!("{head}x-noevia-store-token: not-the-secret-0123456789abcdef\r\n");
    assert_eq!(http(f.store, &bad, &body).await.0, 401);
    assert_eq!(f.open("x", "other", json!({})).await.0, 400);
    assert_eq!(f.open("", "write", json!({})).await.0, 400);
    assert_eq!(f.open("x", "write", json!({"chat":1})).await.0, 400);
    assert_eq!(
        f.op("approvals/open", json!({"id":"x","kind":"write"}))
            .await
            .0,
        400
    );
    assert_eq!(f.open("x", "write", json!({})).await.0, 200);
    assert_eq!(
        f.open("x", "route", json!({})).await,
        (
            409,
            json!({"error":"approval already open","code":"APPROVAL_EXISTS"})
        )
    );
    assert_eq!(f.wait("never-opened").await.0, 404);
    assert_eq!(
        f.op("approvals/cancel", json!({"user":f.user,"id":"x"}))
            .await,
        (200, json!({"cancelled":true}))
    );
    assert_eq!(f.decide("x", r#"{"decision":"approve"}"#).await.0, 404);
}

/// Re-review 1ed3a14: a granted:true answer already produced authorises nothing; the write's redeem,
/// ordered at the front against every revocation, does. A public move after the answer refuses it.
#[tokio::test]
async fn a_front_move_after_the_grant_answer_refuses_its_redeem() {
    let f = front().await;
    let (b, t) = f.ticket("ap-1", "qa-one").await;
    assert_eq!(
        f.public("POST", "/api/chats/qa-one/move", r#"{"projectId":"qa-p"}"#)
            .await,
        200
    );
    assert!(!f.redeem("qa-one", &b, &t).await);
    // Control: with nothing in between, a ticket redeems once.
    let (b, t) = f.ticket("ap-2", "qa-two").await;
    assert!(f.redeem("qa-two", &b, &t).await);
    assert!(!f.redeem("qa-two", &b, &t).await, "a replay is refused");
}

#[tokio::test]
async fn a_front_delete_after_the_grant_answer_refuses_its_redeem() {
    let f = front().await;
    let (b, t) = f.ticket("ap-1", "qa-two").await;
    assert_eq!(f.public("DELETE", "/api/freechats/qa-two", "").await, 200);
    assert!(!f.redeem("qa-two", &b, &t).await);
}

/// A move that fails (a 400, nothing changed, the live context the same) still revokes, and is
/// ordered against the redeem like any other revocation.
#[tokio::test]
async fn a_failed_move_revokes_and_refuses_the_redeem() {
    let f = front().await;
    let (b, t) = f.ticket("ap-1", "qa-one").await;
    assert_eq!(f.public("POST", "/api/chats/qa-one/move", "{}").await, 400);
    assert_eq!(
        f.binding("qa-one", "space:free").await,
        b,
        "nothing changed"
    );
    assert!(!f.redeem("qa-one", &b, &t).await);
    // Node's own store move, failing too, the same.
    let (b, t) = f.ticket("ap-2", "qa-two").await;
    let (status, _) = f
        .op("chat/move", json!({"user":f.user,"chat":"qa-two"}))
        .await;
    assert_eq!(status, 400);
    assert!(!f.redeem("qa-two", &b, &t).await);
    // And Node's store delete.
    let (b, t) = f.ticket("ap-3", "qa-one").await;
    let (status, _) = f
        .op(
            "chat/delete",
            json!({"user":f.user,"chat":"qa-one","kind":"free"}),
        )
        .await;
    assert_eq!(status, 200);
    assert!(!f.redeem("qa-one", &b, &t).await);
}

#[tokio::test]
async fn redeem_needs_a_ticket() {
    let f = front().await;
    let (b, _) = f.ticket("ap-1", "qa-one").await;
    let (status, _) = f
        .op(
            "approvals/redeem",
            json!({"user":f.user,"chat":"qa-one","scope":"space:free","binding":b,"protocol":PROTOCOL}),
        )
        .await;
    assert_eq!(status, 400);
}

/// Re-review 689d17a P1-2: a grant check that does not name the redeem protocol (a Core from before
/// tickets, which would read `{granted:true, ticket}` as plain approval) is answered "no grant".
#[tokio::test]
async fn a_grant_check_without_the_redeem_protocol_gets_no_grant() {
    let f = front().await;
    let (b, _) = f.ticket("ap-1", "qa-one").await;
    for protocol in [
        Value::Null,
        json!("rust-approval-registry"),
        json!("registry-redeem-v2"),
        json!(1),
    ] {
        let mut body = json!({"user":f.user,"chat":"qa-one","scope":"space:free","binding":b});
        if !protocol.is_null() {
            body["protocol"] = protocol;
        }
        let (status, answer) = f.op("approvals/granted", body).await;
        assert_eq!(status, 200);
        assert_eq!(answer["granted"], json!(false), "{answer}");
        assert!(answer.get("ticket").is_none(), "{answer}");
        assert_eq!(answer["code"], json!("APPROVAL_PROTOCOL"));
    }
    assert!(
        f.granted("qa-one", "space:free", &b).await,
        "the grant stands"
    );
}

/// Revocation success requires Node's epoch acknowledgement, in either redeem/revoke order.
async fn check_revoke_order(redeem_first: bool) {
    use std::sync::atomic::Ordering::SeqCst;
    {
        let f = front().await;
        let (b, ticket) = f.ticket("ap-order", "qa-one").await;
        if redeem_first {
            assert!(f.redeem("qa-one", &b, &ticket).await);
        }
        f.node.block.store(true, SeqCst);
        let revoking = f.op("approvals/revoke", json!({"user":f.user,"chat":"qa-one"}));
        tokio::pin!(revoking);
        tokio::select! { _ = &mut revoking => panic!("revoke returned before Node acknowledgement"), () = f.node.entered.notified() => {} }
        assert!(
            !f.redeem("qa-one", &b, &ticket).await,
            "registry revocation already recorded"
        );
        f.node.release.notify_one();
        assert_eq!(revoking.await.0, 200);
    }
}
#[tokio::test]
async fn redeem_before_revoke_still_requires_node_acknowledgement() {
    check_revoke_order(true).await;
}
#[tokio::test]
async fn revoke_before_redeem_requires_node_acknowledgement_and_refuses_ticket() {
    check_revoke_order(false).await;
}
#[tokio::test]
async fn unreachable_node_never_reports_success_and_retry_can_confirm() {
    use std::sync::atomic::Ordering::SeqCst;
    let f = front_with_node(false).await;
    let (b, ticket) = f.ticket("ap-down", "qa-one").await;
    assert_eq!(
        f.op("approvals/revoke", json!({"user":f.user,"chat":"qa-one"}))
            .await
            .0,
        503
    );
    assert!(
        !f.redeem("qa-one", &b, &ticket).await,
        "revocation is recorded despite failed ack"
    );
    for (method, path) in [
        ("POST", "/api/chats/qa-one/move"),
        ("DELETE", "/api/freechats/qa-one"),
        ("DELETE", "/api/projects/qa-p"),
    ] {
        assert_eq!(f.public(method, path, "{}").await, 503, "{path}");
    }
    assert_eq!(
        f.op(
            "chat/delete",
            json!({"user":f.user,"chat":"qa-one","kind":"free"})
        )
        .await
        .0,
        503
    );
    let f = front().await;
    f.node.refuse.store(true, SeqCst);
    assert_eq!(
        f.op("approvals/revoke", json!({"user":f.user,"chat":"qa-one"}))
            .await
            .0,
        503
    );
    f.node.refuse.store(false, SeqCst);
    assert_eq!(
        f.op("approvals/revoke", json!({"user":f.user,"chat":"qa-one"}))
            .await
            .0,
        200
    );
}

#[tokio::test]
async fn missing_epoch_acknowledgement_times_out_fail_closed() {
    use std::sync::atomic::Ordering::SeqCst;
    let f = front().await;
    f.node.block.store(true, SeqCst);
    let started = std::time::Instant::now();
    let (status, _) = f
        .op("approvals/revoke", json!({"user":f.user,"chat":"qa-one"}))
        .await;
    assert_eq!(status, 503);
    assert!(started.elapsed() < Duration::from_secs(4));
}

// Direct store mutation represents a changed live context discovered by the next check, without
// an explicit HTTP revoke path that would already have notified the executor.
fn change_live_context(f: &Front) {
    server_projects::chat_move::move_chat(
        &f.data,
        &f.user,
        "qa-one",
        server_projects::chat_move::Holder::Project("qa-p".into()),
        None,
        START,
    )
    .unwrap();
}
async fn context_check_payload(f: &Front, op: &str, b: &Value) -> Value {
    let mut body =
        json!({"user":f.user,"chat":"qa-one","scope":"space:free","binding":b,"protocol":PROTOCOL});
    if op == "redeem" {
        let (status, grant) = f.op("approvals/granted", body.clone()).await;
        assert_eq!(status, 200);
        body["ticket"] = grant["ticket"].clone();
        assert!(body["ticket"].is_string());
    }
    body
}
async fn check_context_invalidation_with_paused_executor(op: &str) {
    use std::sync::atomic::Ordering::SeqCst;
    let f = front().await;
    let (b, ticket_a) = f.ticket("ap-context", "qa-one").await;
    let payload = context_check_payload(&f, op, &b).await;
    assert!(f.redeem("qa-one", &b, &ticket_a).await);
    let captured = f.node.epoch.load(SeqCst);
    // Ticket A is already redeemed and parked inside credential setup. Only the final outbound
    // epoch check remains; the earlier registry redeem is not repeated by this executor.
    let credentials = Arc::new(tokio::sync::Semaphore::new(0));
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let parked = Arc::new(tokio::sync::Notify::new());
    let running = {
        let parked = Arc::clone(&parked);
        let (credentials, node, writes) = (
            Arc::clone(&credentials),
            Arc::clone(&f.node),
            Arc::clone(&writes),
        );
        tokio::spawn(async move {
            parked.notify_one();
            let _credential = credentials.acquire().await.unwrap();
            if node.epoch.load(SeqCst) == captured {
                writes.fetch_add(1, SeqCst); // synthetic transport's outbound write call
            }
        })
    };
    parked.notified().await;
    change_live_context(&f);
    let (status, answer) = f.op(&format!("approvals/{op}"), payload).await;
    assert_eq!(status, 200);
    assert_eq!(
        answer[if op == "granted" {
            "granted"
        } else {
            "redeemed"
        }],
        json!(false)
    );
    credentials.add_permits(1);
    running.await.unwrap();
    assert_eq!(
        writes.load(SeqCst),
        0,
        "context-invalidating success must fence ticket A's outbound write"
    );
    assert_eq!(
        f.node.calls.load(SeqCst),
        1,
        "context invalidation requires the private acknowledgement"
    );
}
#[tokio::test]
async fn context_invalidating_grant_check_fences_an_executor_paused_in_credentials() {
    check_context_invalidation_with_paused_executor("granted").await;
}
#[tokio::test]
async fn context_invalidating_redeem_fences_an_executor_paused_in_credentials() {
    check_context_invalidation_with_paused_executor("redeem").await;
}
#[tokio::test]
async fn context_invalidation_cannot_complete_before_the_epoch_acknowledgement() {
    use std::sync::atomic::Ordering::SeqCst;
    let f = front().await;
    let (b, _) = f.ticket("ap-context-wait", "qa-one").await;
    let payload = context_check_payload(&f, "granted", &b).await;
    change_live_context(&f);
    f.node.block.store(true, SeqCst);
    let checking = f.op("approvals/granted", payload);
    tokio::pin!(checking);
    tokio::select! {
        answer = &mut checking => panic!("context invalidation completed without Node acknowledgement: {answer:?}"),
        () = f.node.entered.notified() => {}
    }
    assert_eq!(f.node.epoch.load(SeqCst), 0);
    f.node.release.notify_one();
    assert_eq!(checking.await, (200, json!({"granted":false})));
    assert_eq!(f.node.epoch.load(SeqCst), 1);
}
#[tokio::test]
async fn failed_context_ack_remains_pending_after_the_grant_and_ticket_are_gone() {
    use std::sync::atomic::Ordering::SeqCst;
    for op in ["granted", "redeem"] {
        let f = front().await;
        let (b, _) = f.ticket("ap-context-retry", "qa-one").await;
        let payload = context_check_payload(&f, op, &b).await;
        change_live_context(&f);
        f.node.refuse.store(true, SeqCst);
        for _ in 0..2 {
            assert_eq!(
                f.op(&format!("approvals/{op}"), payload.clone()).await.0,
                503
            );
        }
        f.node.refuse.store(false, SeqCst);
        assert_eq!(
            f.op(&format!("approvals/{op}"), payload.clone()).await.0,
            200
        );
        assert_eq!(f.node.calls.load(SeqCst), 3);
        assert_eq!(f.op(&format!("approvals/{op}"), payload).await.0, 200);
        assert_eq!(
            f.node.calls.load(SeqCst),
            3,
            "confirmed invalidation is retired"
        );
    }
}
#[tokio::test]
async fn context_invalidation_with_an_unreachable_executor_never_reports_success() {
    for op in ["granted", "redeem"] {
        let f = front_with_node(false).await;
        let (b, _) = f.ticket("ap-context-down", "qa-one").await;
        let payload = context_check_payload(&f, op, &b).await;
        change_live_context(&f);
        assert_eq!(
            f.op(&format!("approvals/{op}"), payload.clone()).await.0,
            503
        );
        assert_eq!(f.op(&format!("approvals/{op}"), payload).await.0, 503);
    }
}

async fn explicit_fence_retires_abandoned_invalidation(project: bool) {
    use std::sync::atomic::Ordering::SeqCst;
    let f = front().await;
    let (b, _) = f.ticket("ap-abandoned", "qa-one").await;
    let payload = context_check_payload(&f, "granted", &b).await;
    change_live_context(&f);
    f.node.refuse.store(true, SeqCst);
    assert_eq!(f.op("approvals/granted", payload.clone()).await.0, 503);
    f.node.refuse.store(false, SeqCst);
    if project {
        assert_eq!(f.public("DELETE", "/api/projects/qa-p", "").await, 200);
    } else {
        assert_eq!(
            f.op("approvals/revoke", json!({"user":f.user,"chat":"qa-one"}))
                .await
                .0,
            200
        );
    }
    let confirmed = f.node.calls.load(SeqCst);
    f.node.refuse.store(true, SeqCst);
    assert_eq!(
        f.op("approvals/granted", payload).await.0,
        200,
        "an acknowledged fence must retire the abandoned debt"
    );
    assert_eq!(
        f.node.calls.load(SeqCst),
        confirmed,
        "retired markers must not be retried"
    );
}
#[tokio::test]
async fn explicit_chat_revoke_retires_abandoned_context_marker() {
    explicit_fence_retires_abandoned_invalidation(false).await;
}
#[tokio::test]
async fn explicit_project_revoke_retires_abandoned_context_marker() {
    explicit_fence_retires_abandoned_invalidation(true).await;
}
#[tokio::test]
async fn another_chats_covering_fence_retires_older_context_markers() {
    use std::sync::atomic::Ordering::SeqCst;
    let f = front().await;
    let (b, _) = f.ticket("ap-older-marker", "qa-one").await;
    let old = context_check_payload(&f, "granted", &b).await;
    change_live_context(&f);
    f.node.refuse.store(true, SeqCst);
    assert_eq!(f.op("approvals/granted", old.clone()).await.0, 503);
    let (b2, _) = f.ticket("ap-newer-marker", "qa-two").await;
    server_projects::chat_move::move_chat(
        &f.data,
        &f.user,
        "qa-two",
        server_projects::chat_move::Holder::Project("qa-p".into()),
        None,
        START,
    )
    .unwrap();
    f.node.refuse.store(false, SeqCst);
    assert_eq!(f.op("approvals/granted", json!({"user":f.user,"chat":"qa-two","scope":"space:free","binding":b2,"protocol":PROTOCOL})).await.0, 200);
    let covered = f.node.calls.load(SeqCst);
    f.node.refuse.store(true, SeqCst);
    assert_eq!(f.op("approvals/granted", old).await.0, 200);
    assert_eq!(f.node.calls.load(SeqCst), covered);
}
#[tokio::test]
async fn outstanding_context_marker_cap_blocks_grants_and_redeems_until_a_fence_drains_it() {
    use std::sync::atomic::Ordering::SeqCst;
    // The hard contract is deliberately independent of a new production-only test API, so this
    // exact test can run against the old production head as a behavioural negative control.
    const LIMIT: usize = 64;
    let f = front().await;
    let chats: Vec<_> = (0..=LIMIT)
        .map(|i| json!({"id":format!("cap-{i}"),"title":"synthetic"}))
        .collect();
    std::fs::write(
        f.data.join("users").join(&f.user).join("free-chats.json"),
        serde_json::to_vec(&chats).unwrap(),
    )
    .unwrap();
    let chat = format!("cap-{LIMIT}");
    let (b, ticket) = f.ticket("ap-probe", &chat).await;
    f.node.refuse.store(true, SeqCst);
    for i in 0..LIMIT {
        let chat = format!("cap-{i}");
        let (b, _) = f.ticket(&format!("ap-cap-{i}"), &chat).await;
        server_projects::chat_move::move_chat(
            &f.data,
            &f.user,
            &chat,
            server_projects::chat_move::Holder::Project("qa-p".into()),
            None,
            START,
        )
        .unwrap();
        assert_eq!(f.op("approvals/granted", json!({"user":f.user,"chat":chat,"scope":"space:free","binding":b,"protocol":PROTOCOL})).await.0, 503);
    }
    let probe =
        json!({"user":f.user,"chat":chat,"scope":"space:free","binding":b,"protocol":PROTOCOL});
    assert_eq!(
        f.op("approvals/granted", probe.clone()).await.0,
        503,
        "new grant admission must fail closed at the cap"
    );
    let mut redeem = probe.clone();
    redeem["ticket"] = json!(ticket);
    assert_eq!(
        f.op("approvals/redeem", redeem).await.0,
        503,
        "redeem admission must fail closed at the cap"
    );
    f.node.refuse.store(false, SeqCst);
    assert_eq!(
        f.op(
            "approvals/revoke",
            json!({"user":f.user,"chat":"abandoned"})
        )
        .await
        .0,
        200
    );
    let (status, answer) = f.op("approvals/granted", probe).await;
    assert_eq!(status, 200);
    assert_eq!(answer["granted"], json!(true));
    assert!(
        f.redeem(&chat, &b, &ticket).await,
        "capacity rejection must not consume a previously issued ticket"
    );
}
