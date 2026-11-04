//! sbstndalton/noevia#2384, F2: `GET /api/reasoning-settings` and `PUT /api/routing-default` through the real front.
//! The reads are compared answer for answer with the real pre-move Core route (fixtures/reasoning-get-node.json, made by
//! reasoning-get-oracle.cjs); the `mode` is Node's, so the legacy upstream stands in for its facts bridge and answers with
//! what Node's route computed. The writes' differential over the real Core route and workspace store is crates/server-projects
//! `routing_default`; these tests are the HTTP plumbing: identity, CSRF, tenants, body caps, ordering. Synthetic data only.
#![allow(clippy::indexing_slicing, clippy::unwrap_used)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use support::*;

const FACTS: &str = "/internal/reasoning-read/facts";
const REASONING: &str = "/api/reasoning-settings";
const ROUTING: &str = "/api/routing-default";

async fn rig() -> (
    tempfile::TempDir,
    Front,
    Upstream,
    server_store::rusqlite::Connection,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let owner = upstream().await;
    owner.set(|_| reply("200 OK", r#"{"mode":"hint"}"#));
    let f = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    (dir, f, owner, db)
}

fn put_setting(db: &server_store::rusqlite::Connection, kind: &str, value: &Value) {
    db.execute(
        "DELETE FROM settings WHERE key='reasoning_effort_default'",
        [],
    )
    .unwrap();
    match kind {
        "missing" => {}
        // The schema makes the value NOT NULL, so a stored null cannot exist: the row is simply absent.
        "null" => {}
        _ => {
            db.execute(
                "INSERT INTO settings(key,value) VALUES('reasoning_effort_default',?1)",
                [value.as_str().unwrap()],
            )
            .unwrap();
        }
    }
}

fn write_projects(dir: &std::path::Path, user: &str, projects: &Value) {
    std::fs::write(
        dir.join("users").join(user).join("projects.json"),
        serde_json::to_string_pretty(&json!({ "projects": projects })).unwrap(),
    )
    .unwrap();
}

/// `url.searchParams.get('projectId')` over a query string, for the fixture's expected bridge call.
fn project_of(search: &str) -> Option<String> {
    let q = search.strip_prefix('?')?;
    url::form_urlencoded::parse(q.as_bytes())
        .find(|(k, _)| k == "projectId")
        .map(|(_, v)| v.into_owned())
}

/// The answer's text in the key order core's route built it.
fn ordered(body: &Value) -> String {
    format!(
        "{{\"default\":{},\"effort\":{},\"mode\":{},\"admin\":{}}}",
        body["default"], body["effort"], body["mode"], body["admin"]
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_read_answers_like_the_real_core_route_and_asks_node_only_for_the_mode() {
    let (dir, f, owner, db) = rig().await;
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/reasoning-get-node.json")).unwrap();
    write_projects(dir.path(), A, &fixture["projects"]);
    write_projects(dir.path(), B, &fixture["projects"]);
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 140);
    let mut asked = 0;
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let settings = &case["settings"];
        put_setting(
            &db,
            settings["kind"].as_str().unwrap_or("missing"),
            &settings["value"],
        );
        let expected = &case["reply"];
        let want_mode = expected["body"]["mode"].clone();
        let search = case["search"].as_str().unwrap();
        // What Node's route handed `modeFor` for the project: its provider id (or the default) and model.
        let project = project_of(search).and_then(|id| {
            fixture["projects"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == id.as_str())
                .cloned()
        });
        let truthy =
            |v: &Value| !(v.is_null() || v == &json!("") || v == &json!(0) || v == &json!(false));
        let raw = |key: &str, empty: Value| match &project {
            Some(p) if truthy(&p[key]) => p[key].clone(),
            _ => empty,
        };
        let effort = expected["body"]["effort"].clone();
        let before = owner.count(FACTS);
        let mode = want_mode.clone();
        owner.set(move |_| reply("200 OK", &json!({ "mode": mode }).to_string()));
        let cookie = if case["role"] == "admin" {
            ADMIN
        } else {
            MEMBER
        };
        let (status, _, body) = raw_text(
            f.addr,
            "GET",
            &format!("{REASONING}{search}"),
            cookie,
            "",
            b"",
        )
        .await;
        assert_eq!(
            status,
            expected["status"].as_u64().unwrap() as u16,
            "{name}"
        );
        assert_eq!(j(&body), expected["body"], "{name}");
        if status == 200 {
            assert_eq!(body, ordered(&expected["body"]), "{name}: key order");
        }
        let calls = owner
            .seen()
            .into_iter()
            .filter(|s| s.path == FACTS)
            .skip(before)
            .collect::<Vec<_>>();
        if status == 200 && effort != json!("default") {
            assert_eq!(calls.len(), 1, "{name}");
            let call = &calls[0];
            assert_eq!(call.method, "POST", "{name}");
            assert_eq!(
                call.header("x-noevia-store-token").as_deref(),
                Some(STORE_TOKEN),
                "{name}"
            );
            let ask = j(&call.body);
            let user = if case["role"] == "admin" { A } else { B };
            assert_eq!(
                ask,
                json!({
                    "user": user,
                    "provider": raw("provider", Value::Null),
                    "model": raw("model", Value::Null),
                    "effort": effort,
                }),
                "{name}"
            );
            // Node's own record of what it asked `modeFor`, resolved on its side.
            assert_eq!(case["asked"][0]["effort"], effort, "{name}");
            asked += 1;
        } else {
            assert!(
                calls.is_empty(),
                "{name}: no bridge call without a hint or real mode to look up"
            );
            if status == 200 {
                assert_eq!(want_mode, json!("off"), "{name}");
            }
        }
    }
    assert!(asked >= 40, "{asked}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_reads_only_their_own_projects() {
    let (dir, f, owner, _db) = rig().await;
    write_projects(
        dir.path(),
        A,
        &json!([{ "id": "mine-a", "reasoningEffort": "high", "modes": ["chat"] }]),
    );
    write_projects(
        dir.path(),
        B,
        &json!([{ "id": "mine-b", "reasoningEffort": "low", "modes": ["chat"] }]),
    );
    let (status, body) = get(f.addr, &format!("{REASONING}?projectId=mine-a"), MEMBER).await;
    assert_eq!((status, body), (404, json!({"error": "no such project"})));
    let (status, body) = get(f.addr, &format!("{REASONING}?projectId=mine-b"), MEMBER).await;
    assert_eq!((status, body["effort"].clone()), (200, json!("low")));
    assert_eq!(j(&owner.seen()[0].body)["user"], B);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_read_needs_a_session_and_other_methods_still_reach_node() {
    let (_dir, f, owner, _db) = rig().await;
    assert_eq!(get(f.addr, REASONING, "").await.0, 401);
    assert!(owner.seen().is_empty());
    owner.set(|_| reply("418 I'm a teapot", "{}"));
    for method in ["HEAD", "POST", "DELETE", "PATCH"] {
        assert_eq!(
            write(f.addr, method, REASONING, ADMIN, "").await.0,
            418,
            "{method}"
        );
    }
    assert_eq!(owner.count(FACTS), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bridge_that_cannot_answer_is_a_503_never_a_guessed_mode() {
    let (dir, f, owner, db) = rig().await;
    write_projects(
        dir.path(),
        B,
        &json!([{ "id": "p", "reasoningEffort": "high", "modes": ["chat"] }]),
    );
    for (status, body) in [
        (
            "500 Oops",
            r#"{"error":"Reasoning facts are unavailable."}"#,
        ),
        ("404 Not Found", r#"{"error":"Not found"}"#),
        ("200 OK", r#"{"mode":"maybe"}"#),
        ("200 OK", r#"{"mode":5}"#),
        ("200 OK", r#"{}"#),
        ("200 OK", "not json"),
    ] {
        owner.set(move |_| reply(status, body));
        let (code, answer) = get(f.addr, &format!("{REASONING}?projectId=p"), MEMBER).await;
        assert_eq!(
            (code, answer),
            (503, json!({"error": "Reasoning settings unavailable"})),
            "{status} {body}"
        );
    }
    // An effort of default never needs Node, so a bridge that is down does not matter then.
    owner.set(|_| reply("500 Oops", ""));
    let (code, answer) = get(f.addr, REASONING, MEMBER).await;
    assert_eq!((code, answer["mode"].clone()), (200, json!("off")));
    // A stored value this port will not echo (a blob, the one other class a TEXT column keeps) is refused, not guessed.
    db.execute(
        "DELETE FROM settings WHERE key='reasoning_effort_default'",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO settings(key,value) VALUES('reasoning_effort_default',X'6c6f77')",
        [],
    )
    .unwrap();
    let (code, answer) = get(f.addr, REASONING, MEMBER).await;
    assert_eq!(
        (code, answer),
        (503, json!({"error": "Reasoning settings unavailable"}))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_store_token_node_is_never_asked() {
    let dir = tempfile::tempdir().unwrap();
    let _db = seed(dir.path());
    let owner = upstream().await;
    owner.set(|_| reply("200 OK", r#"{"mode":"real"}"#));
    let f = front_env(
        dir.path(),
        true,
        &[("NOEVIA_LEGACY_UPSTREAM", owner.base())],
    )
    .await;
    write_projects(
        dir.path(),
        B,
        &json!([{ "id": "p", "reasoningEffort": "high", "modes": ["chat"] }]),
    );
    let (code, answer) = get(f.addr, &format!("{REASONING}?projectId=p"), MEMBER).await;
    assert_eq!(
        (code, answer),
        (503, json!({"error": "Reasoning settings unavailable"}))
    );
    assert_eq!(owner.count(FACTS), 0);
}

fn prefs(dir: &std::path::Path, user: &str) -> Option<String> {
    std::fs::read_to_string(dir.join("users").join(user).join("preferences.json")).ok()
}

fn projects_of(dir: &std::path::Path, user: &str) -> Value {
    let text = std::fs::read_to_string(dir.join("users").join(user).join("projects.json")).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()["projects"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_routing_default_put_saves_for_the_signed_in_account_only_and_the_native_get_reads_it()
{
    let (dir, f, owner, _db) = rig().await;
    owner.set(|_| reply("418 I'm a teapot", "{}"));
    let mk = |id: &str, routing: &str| json!({"id": id, "routing": routing, "updatedAt": 1, "modes": ["chat"]});
    write_projects(dir.path(), A, &json!([mk("a1", "auto")]));
    write_projects(
        dir.path(),
        B,
        &json!([mk("b1", "auto"), mk("b2", "manual")]),
    );
    assert_eq!(
        get(f.addr, ROUTING, MEMBER).await,
        (200, json!({"routing": "auto"}))
    );
    // Any signed-in role may save its own default (Node checked no role here); the default alone changes no project.
    let (status, body) = write(f.addr, "PUT", ROUTING, MEMBER, r#"{"routing":"manual"}"#).await;
    assert_eq!(
        (status, body),
        (200, json!({"routing": "manual", "updated": 0}))
    );
    assert_eq!(
        prefs(dir.path(), B).as_deref(),
        Some("{\n  \"defaultRouting\": \"manual\"\n}")
    );
    assert_eq!(
        prefs(dir.path(), A),
        None,
        "another account's preferences are untouched"
    );
    assert_eq!(
        get(f.addr, ROUTING, MEMBER).await,
        (200, json!({"routing": "manual"}))
    );
    assert_eq!(
        get(f.addr, ROUTING, ADMIN).await,
        (200, json!({"routing": "auto"}))
    );
    assert_eq!(
        projects_of(dir.path(), B),
        json!([mk("b1", "auto"), mk("b2", "manual")])
    );
    // On request every project of the signed-in account is switched and stamped; the other account's are not.
    let (status, body) = write(
        f.addr,
        "PUT",
        ROUTING,
        MEMBER,
        r#"{"routing":"manual","applyToExisting":true}"#,
    )
    .await;
    assert_eq!(
        (status, body),
        (200, json!({"routing": "manual", "updated": 1}))
    );
    let after = projects_of(dir.path(), B);
    assert_eq!(after[0]["routing"], "manual");
    assert_ne!(after[0]["updatedAt"], json!(1), "stamped");
    assert_eq!(
        after[1],
        mk("b2", "manual"),
        "an unchanged project is not rewritten"
    );
    assert_eq!(projects_of(dir.path(), A), json!([mk("a1", "auto")]));
    // Nothing that is not the answer reached Node.
    assert!(owner.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_routing_default_put_checks_identity_csrf_and_the_body_before_it_writes() {
    let (dir, f, owner, _db) = rig().await;
    let ok = r#"{"routing":"manual"}"#;
    assert_eq!(write(f.addr, "PUT", ROUTING, "", ok).await.0, 401);
    // No CSRF header, or a foreign origin: refused as every signed-in write is.
    assert_eq!(
        raw(f.addr, "PUT", ROUTING, MEMBER, "", ok.as_bytes())
            .await
            .0,
        403
    );
    let foreign = "Origin: http://untrusted.invalid\r\nX-CSRF-Token: csrf\r\n";
    assert_eq!(
        raw(f.addr, "PUT", ROUTING, MEMBER, foreign, ok.as_bytes())
            .await
            .0,
        403
    );
    for (body, status, error) in [
        ("", 400, "invalid JSON"),
        ("{", 400, "invalid JSON"),
        ("null", 400, "routing must be 'auto' or 'manual'"),
        (
            r#"{"routing":"fast"}"#,
            400,
            "routing must be 'auto' or 'manual'",
        ),
    ] {
        let (code, answer) = write(f.addr, "PUT", ROUTING, MEMBER, body).await;
        assert_eq!(
            (code, answer),
            (status, json!({ "error": error })),
            "{body}"
        );
    }
    // A body over the 1 MiB cap is the same 400 invalid JSON, and nothing is written.
    let big = format!(
        r#"{{"pad":"{}","routing":"manual"}}"#,
        "x".repeat(1024 * 1024 + 1)
    );
    let (code, answer) = write(f.addr, "PUT", ROUTING, MEMBER, &big).await;
    assert_eq!((code, answer), (400, json!({"error": "invalid JSON"})));
    assert_eq!(prefs(dir.path(), B), None);
    assert_eq!(prefs(dir.path(), A), None);
    // The public answers carry no-store, as Node's json() did.
    let (_, headers, _) = raw_text(
        f.addr,
        "PUT",
        ROUTING,
        MEMBER,
        &format!("Origin: http://{}\r\nX-CSRF-Token: csrf\r\n", f.addr),
        ok.as_bytes(),
    )
    .await;
    assert!(headers.contains("cache-control: no-store"), "{headers}");
    assert_eq!(
        prefs(dir.path(), B).as_deref(),
        Some("{\n  \"defaultRouting\": \"manual\"\n}")
    );
    // POST, DELETE and the rest are still Node's.
    owner.set(|_| reply("418 I'm a teapot", "{}"));
    for method in ["POST", "DELETE", "PATCH", "HEAD"] {
        assert_eq!(
            write(f.addr, method, ROUTING, MEMBER, ok).await.0,
            418,
            "{method}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_routing_default_puts_leave_one_whole_document() {
    let (dir, f, _owner, _db) = rig().await;
    std::fs::write(
        dir.path().join("users").join(B).join("preferences.json"),
        r#"{"theme":"dark"}"#,
    )
    .unwrap();
    let tasks = (0..8).map(|i| {
        let addr = f.addr;
        tokio::spawn(async move {
            let body = if i % 2 == 0 {
                r#"{"routing":"manual"}"#
            } else {
                r#"{"routing":"auto"}"#
            };
            write(addr, "PUT", ROUTING, MEMBER, body).await.0
        })
    });
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    let text = prefs(dir.path(), B).unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["theme"], "dark");
    assert!(["auto", "manual"].contains(&doc["defaultRouting"].as_str().unwrap()));
    let strays: Vec<_> = std::fs::read_dir(dir.path().join("users").join(B))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(strays.is_empty(), "{strays:?}");
}
