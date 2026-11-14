//! The sign-in turn's plan, account re-check and runner (no network here): every refusal of
//! [`plan`] in its order, the re-check's answers, and a runner that refuses a foreign engine
//! before the inner host is touched. A whole turn through [`run`] over a loopback backend:
//! signin_wire_tests.rs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use super::*;
use crate::chatgpt_oauth_routes::{transport, Settings};
use crate::signin_host::tests::FakeInner;
use js_json::JValue;
use signin_transport::session::BrokerAnswer;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

const URL: &str = "http://qa-signin.invalid/v1/chat/completions";
const USER: &str = "qa-user";
const CAPS: &str = "skills,signin-transport-v1";

/// A prepared frame for the sign-in row; `edit` changes its text before it is read.
fn frame(edit: impl Fn(String) -> String) -> Prepared {
    let text = edit(
        r#"{"type":"prepared","v":1,"startedAt":1000,"model":"m-large","role":null,"auto":false,"effort":"default","thinkingOff":false,"sampling":{},
"provider":{"id":"qa-signin","label":"QA sign-in","localEngine":false,"capabilities":null},
"upstreamUrl":"http://qa-signin.invalid/v1/chat/completions","limitScope":"qa-scope",
"messages":[{"role":"system","content":"QA system prompt."},{"role":"user","content":"QA hello"}],
"tools":[],"routes":[],"writes":[],"public":[],"automaticOnlyWrites":[],"editTools":[],"routeNote":"",
"templateGate":false,"managerEnabled":false,"modelLease":false,
"chat":{"id":"qa-chat-1","contextId":"qa-chat-1","key":"qa-chat-1","grantScope":"project:qa-project","grantHolder":{"kind":"project","id":"qa-project"},"recentWrites":[],
"title":{"chatId":"qa-chat-1","listId":"qa-project","firstMessage":"QA hello","firstReply":true,"enabled":false}},
"turn":{"message":"QA hello","spaceId":null,"projectId":"qa-project","declineEndsReply":true,"record":true,"streamIdleMs":120000,"compactOnly":false,"sources":{"reply":[],"injected":[]}},
"skills":{"loaded":[],"enabled":[],"revokedInHistory":[],"imageWarning":"","coverageWarning":""}}"#
            .to_owned(),
    );
    Prepared::read(&js_json::parse(&text).unwrap()).unwrap()
}

fn same(s: String) -> String {
    s
}

fn cut() -> SigninCut<'static> {
    SigninCut {
        owner: USER,
        conversation: Some("qa-chat-1"),
        url: URL,
    }
}

#[test]
fn a_matching_cut_plans_the_bound_turn() {
    let p = frame(same);
    let got = plan(CAPS, USER, &cut(), &p, 5_000.0).unwrap();
    assert_eq!(
        got,
        SigninPlan {
            owner: USER.into(),
            conversation: Some("qa-chat-1".into()),
            binding: SigninBinding {
                url: URL.into(),
                model: "m-large".into(),
                expires_ms: 5_000.0,
            },
        }
    );
    // No conversation (a turn without chat or space) stays none.
    let mut c = cut();
    c.conversation = None;
    assert_eq!(
        plan(CAPS, USER, &c, &p, 5_000.0).unwrap().conversation,
        None
    );
}

#[test]
fn without_the_offer_nothing_is_planned() {
    let p = frame(same);
    for caps in [
        "",
        "skills",
        "providers",
        "signin-transport-v10",
        "Signin-transport-v1",
        "signin-transport",
        "providers signin-transport-v1",
    ] {
        assert_eq!(
            plan(caps, USER, &cut(), &p, 5_000.0),
            Err(NOT_OFFERED),
            "{caps:?}"
        );
    }
    // The offer is an exact token anywhere in the list.
    for caps in [
        "signin-transport-v1",
        " signin-transport-v1 ",
        "providers, signin-transport-v1",
        ",,signin-transport-v1,",
    ] {
        assert!(plan(caps, USER, &cut(), &p, 5_000.0).is_ok(), "{caps:?}");
    }
}

#[test]
fn another_owner_is_refused() {
    let p = frame(same);
    for (user, owner) in [
        ("qa-user", "qa-other"),
        ("qa-other", "qa-user"),
        ("", ""),
        ("qa-user", ""),
        ("qa-user", "QA-USER"),
        ("qa-user", "qa-user "),
    ] {
        let mut c = cut();
        c.owner = owner;
        assert_eq!(
            plan(CAPS, user, &c, &p, 5_000.0),
            Err(FOREIGN),
            "{user:?} {owner:?}"
        );
    }
}

#[test]
fn a_frame_asking_for_the_local_engine_is_refused() {
    let edits: [fn(String) -> String; 3] = [
        |s| s.replace(r#""managerEnabled":false"#, r#""managerEnabled":true"#),
        |s| s.replace(r#""modelLease":false"#, r#""modelLease":true"#),
        |s| {
            s.replace(
                r#""firstReply":true,"enabled":false"#,
                r#""firstReply":true,"enabled":true"#,
            )
        },
    ];
    for edit in edits {
        let p = frame(edit);
        assert_eq!(plan(CAPS, USER, &cut(), &p, 5_000.0), Err(LOCAL_ENGINE));
    }
}

#[test]
fn the_address_model_and_expiry_are_checked_in_that_order() {
    let p = frame(same);
    for url in [
        "",
        "http://qa-signin.invalid/v1/chat/completions/",
        "http://qa-other.invalid/v1/chat/completions",
    ] {
        let mut c = cut();
        c.url = url;
        assert_eq!(plan(CAPS, USER, &c, &p, 5_000.0), Err(ADDRESS), "{url:?}");
    }

    let nameless = frame(|s| s.replace(r#""model":"m-large""#, r#""model":"""#));
    assert_eq!(plan(CAPS, USER, &cut(), &nameless, 5_000.0), Err(NO_MODEL));
    // The address is checked before the model.
    let mut c = cut();
    c.url = "http://qa-other.invalid/v1/chat/completions";
    assert_eq!(plan(CAPS, USER, &c, &nameless, 5_000.0), Err(ADDRESS));

    for expires in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            plan(CAPS, USER, &cut(), &p, expires),
            Err(BAD_EXPIRY),
            "{expires}"
        );
    }
    // The offer comes first of all, the owner second.
    let mut c = cut();
    c.owner = "qa-other";
    assert_eq!(plan("", USER, &c, &nameless, f64::NAN), Err(NOT_OFFERED));
    assert_eq!(plan(CAPS, USER, &c, &nameless, f64::NAN), Err(FOREIGN));
}

#[test]
fn the_account_check_asks_for_the_turns_user_every_time() {
    let asked = Mutex::new(Vec::new());
    let answer = Mutex::new(Ok(true));
    let check = account_check(
        |u: &str| {
            asked.lock().unwrap().push(u.to_owned());
            *answer.lock().unwrap()
        },
        USER.to_owned(),
    );
    assert_eq!(check(), Ok(()));
    *answer.lock().unwrap() = Ok(false);
    assert_eq!(check(), Err(NOT_ACTIVE));
    *answer.lock().unwrap() = Err(());
    assert_eq!(check(), Err(NOT_ACTIVE));
    *answer.lock().unwrap() = Ok(true);
    assert_eq!(check(), Ok(()));
    assert_eq!(*asked.lock().unwrap(), vec![USER; 4]);
}

/// A broker that must never be reached by these tests.
#[derive(Clone, Default)]
struct NoBroker {
    asked: Arc<AtomicBool>,
}

impl Accounts for NoBroker {
    fn signal(&self) -> Option<String> {
        None
    }
    async fn session(&self, _user: &str, _rejected: Option<&str>) -> BrokerAnswer {
        self.asked.store(true, Ordering::SeqCst);
        BrokerAnswer::Unreachable
    }
    async fn reject(&self, _user: &str, _generation: &str) {
        self.asked.store(true, Ordering::SeqCst);
    }
    async fn catalogue(&self, _user: &str) -> Option<JValue> {
        self.asked.store(true, Ordering::SeqCst);
        None
    }
}

fn engine(user: &str, conversation: Option<&str>, broker: NoBroker) -> SigninEngine<NoBroker> {
    let settings = Settings::from_lookup(&|_: &str| None, None);
    let mut t = transport(&settings);
    // Never reached.
    t.base_url = "http://127.0.0.1:9/api".into();
    SigninEngine {
        accounts: broker,
        client: crate::signin_wire::client().unwrap(),
        transport: t,
        user: user.into(),
        conversation: conversation.map(str::to_owned),
    }
}

/// The turn's helpers as chat_native.rs passes them (a macro: the closures live in the caller's
/// `let`).
macro_rules! helpers {
    () => {
        Helpers {
            clean_search_args: &|n, a| chat_turn::helpers::clean_search_args(n, a).into_owned(),
            announces_tool_action: &chat_turn::helpers::announces_tool_action,
            announced_note: chat_turn::helpers::ANNOUNCED_NOTE,
            reduce_tool_result: &|t, cap| chat_turn::reduce::reduce_tool_result(t, cap).text,
            source_links: &chat_turn::helpers::source_links,
            fallback_text: &|s| chat_turn::helpers::fallback_text(s),
            final_answer_note: chat_turn::helpers::FINAL_ANSWER_NOTE,
            tool_result_cap: 8000,
        }
    };
}

#[tokio::test]
async fn a_foreign_engine_is_refused_before_the_inner_host_is_touched() {
    let p = frame(same);
    let planned = plan(CAPS, USER, &cut(), &p, 5_000.0).unwrap();
    let process = Process::default();
    let helpers = helpers!();
    for (user, conversation) in [
        ("qa-other", Some("qa-chat-1")),
        (USER, Some("qa-chat-2")),
        (USER, None),
    ] {
        let inner = FakeInner::new(1_000.0);
        let broker = NoBroker::default();
        let checked = AtomicUsize::new(0);
        let got = run(
            &inner,
            engine(user, conversation, broker.clone()),
            planned.clone(),
            || {
                checked.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            &process,
            &helpers,
            &p,
        )
        .await;
        assert_eq!(got, Err(FOREIGN), "{user:?} {conversation:?}");
        assert!(inner.calls().is_empty(), "{:?}", inner.calls());
        assert!(inner.events.lock().unwrap().is_empty());
        assert_eq!(checked.load(Ordering::SeqCst), 0);
        assert!(!broker.asked.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn an_inactive_account_ends_the_turn_before_the_broker_is_asked() {
    let p = frame(same);
    let planned = plan(CAPS, USER, &cut(), &p, 5_000.0).unwrap();
    let process = Process::default();
    let helpers = helpers!();
    let inner = FakeInner::new(1_000.0);
    let broker = NoBroker::default();
    let asked = AtomicUsize::new(0);
    let check = account_check(
        |u: &str| {
            assert_eq!(u, USER);
            asked.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        },
        USER.to_owned(),
    );
    let ended = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run(
            &inner,
            engine(USER, Some("qa-chat-1"), broker.clone()),
            planned,
            check,
            &process,
            &helpers,
            &p,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(ended, Ended::Left);
    // chat_turn's failed-request event names the re-check's text.
    let all = inner.events.lock().unwrap().join("\n");
    assert!(all.contains(NOT_ACTIVE), "{all}");
    assert!(asked.load(Ordering::SeqCst) >= 1);
    assert!(!broker.asked.load(Ordering::SeqCst));
    // The inner host's engine was never asked either.
    for c in ["request", "lease", "health", "load", "read", "text"] {
        assert!(
            !inner.calls().iter().any(|x| x == c),
            "{c} reached the inner host"
        );
    }
}

#[tokio::test]
async fn an_expired_binding_ends_the_turn_before_anything_is_asked() {
    let p = frame(same);
    // The inner host's clock (2_000) is past the binding's end (1_500).
    let planned = plan(CAPS, USER, &cut(), &p, 1_500.0).unwrap();
    let process = Process::default();
    let helpers = helpers!();
    let inner = FakeInner::new(2_000.0);
    let broker = NoBroker::default();
    let checked = AtomicUsize::new(0);
    let ended = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run(
            &inner,
            engine(USER, Some("qa-chat-1"), broker.clone()),
            planned,
            || {
                checked.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            &process,
            &helpers,
            &p,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(ended, Ended::Left);
    let all = inner.events.lock().unwrap().join("\n");
    assert!(all.contains(crate::signin_host::EXPIRED), "{all}");
    // Expiry is checked before the account re-check (SigninHost::admit's order).
    assert_eq!(checked.load(Ordering::SeqCst), 0);
    assert!(!broker.asked.load(Ordering::SeqCst));
}

#[test]
fn the_account_read_runs_in_place_outside_a_runtime() {
    assert_eq!(off_the_reactor(|| 7), 7);
}

#[tokio::test(flavor = "current_thread")]
async fn on_a_current_thread_runtime_the_account_read_runs_in_place() {
    // block_in_place panics on a current-thread runtime; the read must not.
    assert_eq!(off_the_reactor(|| 8), 8);
}

/// noevia#2754 item 5: on the front's multi-thread runtime the blocking account read gives its
/// worker away, so the runtime's other tasks go on while it waits. With one worker and an in-place
/// read the spawned task could never run and the wait below would time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn on_a_multi_thread_runtime_the_account_read_lets_other_tasks_run() {
    let ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    let other = tokio::spawn(async move { flag.store(true, Ordering::SeqCst) });
    let seen = off_the_reactor(|| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ran.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        ran.load(Ordering::SeqCst)
    });
    assert!(seen, "the other task ran while the read blocked");
    tokio::time::timeout(std::time::Duration::from_secs(5), other)
        .await
        .unwrap()
        .unwrap();
}
