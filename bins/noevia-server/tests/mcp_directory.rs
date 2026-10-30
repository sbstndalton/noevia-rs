//! The admin MCP directory (sbstndalton/noevia#2202 round 2) through the real front and the real
//! internal store listener. The MCP server, its registry and its OAuth service are one synthetic http
//! server on 127.0.0.1 (NOEVIA_QA_ALLOW_LOOPBACK_MCP=1, NOEVIA_QA_MCP_REGISTRY); the legacy upstream is
//! dead, so a request that fell through to Node would fail. No real server, account, token or key.
#![cfg(feature = "rust-storage-reads")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
#[path = "mcp_support/mod.rs"]
mod support;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;
use tokio::io::AsyncWriteExt;

const SECRET: &str = "SECRET-KEY-ONE";

/// A front with a synthetic registry at `up` and a database seeded with an administrator and a member.
async fn rig(
    registry: &str,
    key_header: bool,
    tools: &str,
) -> (
    tempfile::TempDir,
    server_store::rusqlite::Connection,
    Upstream,
    Arc<Synth>,
    Front,
) {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let up = upstream().await;
    let s = synth(&up, tools);
    *s.registry.lock().unwrap() = Some(registry_of(&up.base(), "io.test/weather", key_header));
    let _ = registry;
    let f = front_env(d.path(), true, &[("NOEVIA_QA_MCP_REGISTRY", up.base())]).await;
    (d, db, up, s, f)
}

const LIST: &str = "/api/admin/mcp-directory";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_routes_keep_the_sessions_gate_and_are_for_administrators() {
    let (_d, db, _up, _s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    for (method, path) in [
        ("GET", LIST.to_owned()),
        ("POST", LIST.to_owned()),
        ("POST", format!("{LIST}/custom")),
        ("POST", format!("{LIST}/custom/preview")),
        ("PUT", format!("{LIST}/dir-x/keys")),
        ("PUT", format!("{LIST}/dir-x/oauth-client")),
        ("DELETE", format!("{LIST}/dir-x")),
    ] {
        // No session: the gate's own answer, before anything of the directory.
        let (st, _) = raw(a, method, &path, "", "", b"{}").await;
        assert_eq!(st, 401, "{method} {path}");
        // A member is not an administrator (the CSRF header is valid, so this is the role check).
        let (st, body) = write(a, method, &path, MEMBER, "{}").await;
        assert_eq!(
            (st, body["error"].as_str()),
            (403, Some("Administrator required")),
            "{method} {path}"
        );
        // A write without the CSRF header never reaches the directory.
        if method != "GET" {
            let (st, _) = raw(a, method, &path, ADMIN, "", b"{}").await;
            assert_eq!(st, 403, "{method} {path}: csrf");
        }
    }
    // An administrator gets core's 405 for a method that is not one of the operations.
    for (method, path) in [
        ("PUT", LIST.to_owned()),
        ("GET", format!("{LIST}/custom")),
        ("PATCH", format!("{LIST}/dir-x")),
        ("DELETE", format!("{LIST}/Dir-X")),
        ("POST", format!("{LIST}/dir-x")),
        ("GET", format!("{LIST}/dir-x/keys")),
    ] {
        let (st, body) = write(a, method, &path, ADMIN, "{}").await;
        assert_eq!(
            (st, body["error"].as_str()),
            (405, Some("method not allowed")),
            "{method} {path}"
        );
    }
    // A body that is not a JSON object is the client's mistake (core readObject), on every operation that reads one.
    for (method, path) in [
        ("POST", LIST.to_owned()),
        ("POST", format!("{LIST}/custom")),
        ("PUT", format!("{LIST}/dir-x/keys")),
    ] {
        let (st, body) = write(a, method, &path, ADMIN, "[1]").await;
        assert_eq!(
            (st, body["error"].as_str()),
            (400, Some("invalid JSON")),
            "{method} {path}"
        );
    }
    assert_eq!(dir_count(&db), 0, "none of that wrote anything");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registry_server_is_listed_by_the_front_itself_then_added_and_every_refusal_is_core_s_wording(
) {
    let (d, db, up, s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    let (st, body) = write(
        a,
        "POST",
        LIST,
        ADMIN,
        r#"{"registryName":"io.test/weather"}"#,
    )
    .await;
    assert_eq!(st, 201, "{body}");
    assert_eq!(body["server"]["id"], "dir-io-test-weather");
    assert_eq!(body["server"]["toolCount"], 1);
    assert_eq!(body["server"]["keyHeaders"], j("[]"));
    // The listing of the server that was just added carries the tools this request listed itself (Node's
    // discovery has not seen the server yet), and no key, no ciphertext.
    let listed = &body["servers"][0];
    assert_eq!(listed["toolCount"], 1);
    assert_eq!(listed["tools"][0]["name"], "echo");
    assert_eq!(listed["tools"][0]["description"], "Echo");
    assert_eq!(
        (listed["toolsTruncated"].clone(), listed["error"].clone()),
        (j("false"), serde_json::Value::Null)
    );
    assert!(!body.to_string().contains("headersEnc"));
    let row = dir_row(&db, "dir-io-test-weather").unwrap();
    assert_eq!(
        (row.0.as_str(), row.1.as_str(), row.2, row.3, row.4),
        ("weather", format!("{}/mcp", up.base()).as_str(), None, 0, 0)
    );
    assert_eq!(audits(&db, "mcp.directory.add").len(), 1);
    assert!(
        s.lists.load(Ordering::SeqCst) >= 1,
        "the front asked the server for its tools"
    );
    // The server answered an `initialize` and a `tools/list`, and the session was closed.
    assert!(up
        .seen()
        .iter()
        .any(|r| r.method == "DELETE" && r.path == "/mcp"));

    // The same server again, and a name the registry does not know, and a registry that is down.
    let (st, body) = write(
        a,
        "POST",
        LIST,
        ADMIN,
        r#"{"registryName":"io.test/weather"}"#,
    )
    .await;
    assert_eq!(
        (st, body["error"].as_str()),
        (409, Some("That server is already added."))
    );
    let (st, body) = write(
        a,
        "POST",
        LIST,
        ADMIN,
        r#"{"registryName":"io.test/nothing"}"#,
    )
    .await;
    assert_eq!(
        (st, body["error"].as_str()),
        (404, Some("That server is not in the MCP registry."))
    );
    let (st, body) = write(a, "POST", LIST, ADMIN, r#"{}"#).await;
    assert_eq!(
        (st, body["error"].as_str()),
        (404, Some("That server is not in the MCP registry."))
    );
    *s.registry.lock().unwrap() = None;
    let (st, body) = write(
        a,
        "POST",
        LIST,
        ADMIN,
        r#"{"registryName":"io.test/weather"}"#,
    )
    .await;
    assert_eq!(
        (st, body["error"].as_str()),
        (
            502,
            Some("The MCP registry could not be reached right now.")
        )
    );
    assert_eq!(dir_count(&db), 1);

    // The listing (GET) reads Node's published discovery: with none, a server shows no tools and no error.
    let (st, body) = get(a, LIST, ADMIN).await;
    assert_eq!(st, 200);
    assert_eq!(body["servers"][0]["toolCount"], serde_json::Value::Null);
    assert_eq!(body["servers"][0]["tools"], j("[]"));
    let _ = d;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_with_no_usable_tool_or_one_that_does_not_answer_is_not_added() {
    let (_d, db, up, s, f) = rig("", false, "[]").await;
    let a = f.addr;
    let add = |name: &'static str| async move {
        write(
            a,
            "POST",
            LIST,
            ADMIN,
            &format!(r#"{{"registryName":"{name}"}}"#),
        )
        .await
    };
    // No tools; tools noevia cannot use (a name with a space, a schema that is not an object).
    let (st, body) = add("io.test/weather").await;
    assert_eq!(
        (st, body["error"].as_str()),
        (
            422,
            Some("The server answered but offers no tools noevia can use.")
        )
    );
    *s.tools.lock().unwrap() = r#"[{"name":"has space","inputSchema":{"type":"object"}},{"name":"flat","inputSchema":{"type":"string"}}]"#.into();
    let (st, body) = add("io.test/weather").await;
    assert_eq!(
        (st, body["error"].as_str()),
        (
            422,
            Some("The server answered but offers no tools noevia can use.")
        )
    );
    // A server that answers 500: the failure is quoted, cut and attributed.
    up.set(|r: &Seen| {
        if r.path.starts_with("/v0/servers") {
            return reply(
                "200 OK",
                &registry_of(
                    &format!("http://{}", r.header("host").unwrap()),
                    "io.test/weather",
                    false,
                ),
            );
        }
        reply("500 Oops", "broken")
    });
    let (st, body) = add("io.test/weather").await;
    assert_eq!(st, 422);
    let why = body["error"].as_str().unwrap();
    assert!(
        why.starts_with("The server did not answer as an MCP server: MCP 500: broken"),
        "{why}"
    );
    assert_eq!(dir_count(&db), 0, "nothing was kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_is_proven_then_sealed_for_the_server_or_for_the_adding_administrator() {
    let (d, db, up, s, f) = rig("", true, ECHO).await;
    let a = f.addr;
    *s.key.lock().unwrap() = Some(SECRET.into());
    // A missing key, and a wrong one, are refused before anything is kept.
    let (st, body) = write(
        a,
        "POST",
        LIST,
        ADMIN,
        r#"{"registryName":"io.test/weather"}"#,
    )
    .await;
    assert_eq!(
        (st, body["error"].as_str()),
        (400, Some("Enter X-Api-Key."))
    );
    let (st, body) = write(
        a,
        "POST",
        LIST,
        ADMIN,
        r#"{"registryName":"io.test/weather","headers":{"X-Api-Key":"wrong"}}"#,
    )
    .await;
    assert_eq!(st, 422);
    assert!(
        body["error"].as_str().unwrap().starts_with(
            "The server did not accept that key, or did not answer as an MCP server: MCP 401"
        ),
        "{body}"
    );
    assert_eq!(dir_count(&db), 0);
    // The right key, shared (the default): sealed unbound, as core sealed it, and never in an answer.
    let (st, body) = write(
        a,
        "POST",
        LIST,
        ADMIN,
        &format!(r#"{{"registryName":"io.test/weather","headers":{{"X-Api-Key":"{SECRET}"}}}}"#),
    )
    .await;
    assert_eq!(st, 201, "{body}");
    assert!(
        !body.to_string().contains(SECRET),
        "a key is never returned"
    );
    assert_eq!(body["server"]["keyHeaders"], j(r#"["X-Api-Key"]"#));
    assert_eq!(body["server"]["personal"], false);
    let id = "dir-io-test-weather";
    let (_, _, enc, _, personal) = dir_row(&db, id).unwrap();
    let enc = enc.unwrap();
    assert!(enc.starts_with("enc:v1:") && !enc.contains(SECRET));
    assert_eq!(
        open(&enc, None),
        j(&format!(
            r#"{{"X-Api-Key":"{}"}}"#,
            SECRET.replace("{k}", "")
        ))
    );
    assert_eq!(personal, 0);
    // The declared header is kept (the key form needs no registry): the template, the description.
    let declared: String = db
        .query_row(
            "SELECT declared_json FROM directory_mcp_servers WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        j(&declared),
        j(
            r#"[{"name":"X-Api-Key","required":true,"secret":true,"description":"Your key","template":"{k}"}]"#
        )
    );
    assert!(audits(&db, "mcp.directory.add").len() == 1);
    let detail: String = db
        .query_row(
            "SELECT detail FROM audit_events WHERE action='mcp.directory.add'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !detail.contains(SECRET) && detail.contains("X-Api-Key"),
        "the audit names the header, never the key: {detail}"
    );

    // The same registry server, each person bringing their own key: the adding administrator keeps theirs.
    db.execute("DELETE FROM directory_mcp_servers", []).unwrap();
    let (st, body) = write(a, "POST", LIST, ADMIN, &format!(r#"{{"registryName":"io.test/weather","headers":{{"X-Api-Key":"{SECRET}"}},"keyMode":"personal"}}"#)).await;
    assert_eq!(st, 201, "{body}");
    assert_eq!(body["server"]["personal"], true);
    let (_, _, enc, _, personal) = dir_row(&db, id).unwrap();
    assert_eq!(
        (enc, personal),
        (None, 1),
        "a personal server keeps no shared key"
    );
    assert_eq!(
        open(&key_row(&db, A, id).unwrap(), Some(A)),
        j(&format!(r#"{{"X-Api-Key":"{SECRET}"}}"#))
    );
    assert!(key_row(&db, B, id).is_none());
    assert!(up
        .seen()
        .iter()
        .any(|r| r.header("x-api-key") == Some(SECRET.to_ascii_lowercase())));
    let _ = d;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_asks_for_a_sign_in_is_added_for_oauth_and_the_administrator_is_sent_to_sign_in(
) {
    let (_d, db, up, s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    *s.oauth.lock().unwrap() = true;
    // Serve the authorization service beside the MCP server.
    let base = up.base();
    let counts = Arc::new(Counts::default());
    let inner = Arc::clone(&s);
    let c2 = Arc::clone(&counts);
    let b2 = base.clone();
    up.set(move |r: &Seen| {
        let path = r.path.split('?').next().unwrap_or("");
        if path == "/v0/servers" {
            return reply("200 OK", &registry_of(&b2, "io.test/weather", false));
        }
        let _ = &inner;
        oauth_reply(r, &b2, &c2).unwrap_or_else(|| reply("404 Not Found", ""))
    });
    let (st, body) = write(
        a,
        "POST",
        LIST,
        ADMIN,
        r#"{"registryName":"io.test/weather"}"#,
    )
    .await;
    assert_eq!(st, 202, "{body}");
    let sign_in = url::Url::parse(body["signIn"].as_str().unwrap()).unwrap();
    assert_eq!(sign_in.path(), "/authorize");
    assert_eq!(body["server"]["oauth"], true);
    assert_eq!(body["servers"][0]["oauth"], true);
    assert_eq!(
        body["servers"][0]["oauthClient"]["manual"], false,
        "the front registered noevia at the service"
    );
    assert!(body["servers"][0]["redirectUri"]
        .as_str()
        .unwrap()
        .ends_with("/api/mcp-oauth/callback"));
    assert_eq!(counts.register.load(Ordering::SeqCst), 1);
    let (_, _, enc, oauth, personal) = dir_row(&db, "dir-io-test-weather").unwrap();
    assert_eq!((enc, oauth, personal), (None, 1, 0));
    // The preview of a custom address says so, and the connect needs that exact preview.
    let url = format!("{base}/mcp");
    let form = format!(r#"{{"title":"Custom","url":"{url}"}}"#);
    let (st, pre) = write(a, "POST", &format!("{LIST}/custom/preview"), ADMIN, &form).await;
    assert_eq!(st, 200, "{pre}");
    assert_eq!(
        (pre["requiresSignIn"].clone(), pre["toolCount"].clone()),
        (j("true"), serde_json::Value::Null)
    );
    let token = pre["previewToken"].as_str().unwrap().to_owned();
    let (st, body) = write(a, "POST", &format!("{LIST}/custom"), ADMIN, &form).await;
    assert_eq!((st, body["error"].as_str()), (409, Some("The server or access details changed since preview. Preview again before connecting.")));
    let (st, body) = write(
        a,
        "POST",
        &format!("{LIST}/custom"),
        ADMIN,
        &format!(r#"{{"title":"Custom","url":"{url}","previewToken":"{token}"}}"#),
    )
    .await;
    assert_eq!(st, 202, "{body}");
    assert!(body["signIn"].as_str().unwrap().contains("/authorize"));
    assert_eq!(
        dir_count(&db),
        2,
        "the registry server and the custom address, each added for sign-in"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_custom_server_is_previewed_then_connected_only_as_previewed() {
    let (_d, db, up, s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    let url = format!("{}/mcp", up.base());
    let form = format!(r#"{{"title":"My server","url":"{url}"}}"#);
    let preview = |body: String| async move {
        write(a, "POST", &format!("{LIST}/custom/preview"), ADMIN, &body).await
    };
    let (st, pre) = preview(form.clone()).await;
    assert_eq!(st, 200, "{pre}");
    assert_eq!(
        (
            pre["requiresSignIn"].clone(),
            pre["toolCount"].clone(),
            pre["toolsTruncated"].clone()
        ),
        (j("false"), j("1"), j("false"))
    );
    assert_eq!(pre["tools"], j(r#"[{"name":"echo","description":"Echo"}]"#));
    assert_eq!(dir_count(&db), 0, "a preview saves nothing");
    let token = pre["previewToken"].as_str().unwrap().to_owned();
    assert!(regex_like(&token));
    let with = |t: &str, title: &str| {
        format!(r#"{{"title":"{title}","url":"{url}","previewToken":"{t}"}}"#)
    };
    // No token, another form, a token of nothing: not connected.
    for body in [
        form.clone(),
        with(&token, "Other name"),
        with("1234567890123.00", "My server"),
        with("", "My server"),
    ] {
        let (st, out) = write(a, "POST", &format!("{LIST}/custom"), ADMIN, &body).await;
        assert_eq!((st, out["error"].as_str()), (409, Some("The server or its tools changed since preview. Preview again before connecting.")), "{body}");
    }
    assert_eq!(dir_count(&db), 0);
    // The tools changed between the preview and the connect: not connected either.
    *s.tools.lock().unwrap() = r#"[{"name":"echo","description":"Echo","inputSchema":{"type":"object"}},{"name":"extra","inputSchema":{"type":"object"}}]"#.into();
    let (st, out) = write(
        a,
        "POST",
        &format!("{LIST}/custom"),
        ADMIN,
        &with(&token, "My server"),
    )
    .await;
    assert_eq!(st, 409, "{out}");
    assert_eq!(dir_count(&db), 0);
    // Previewed and connected as it is: kept, with the address as its registry name.
    *s.tools.lock().unwrap() = ECHO.into();
    let (_, pre) = preview(form.clone()).await;
    let token = pre["previewToken"].as_str().unwrap().to_owned();
    let (st, out) = write(
        a,
        "POST",
        &format!("{LIST}/custom"),
        ADMIN,
        &with(&token, "My server"),
    )
    .await;
    assert_eq!(st, 201, "{out}");
    let id = out["server"]["id"].as_str().unwrap().to_owned();
    let registry: String = db
        .query_row(
            "SELECT registry_name FROM directory_mcp_servers WHERE id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(registry, format!("url:{url}"));
    // The same token is good for the same form again (it vouches for a form, not for one use), but the server is now added.
    let (st, out) = write(
        a,
        "POST",
        &format!("{LIST}/custom"),
        ADMIN,
        &with(&token, "My server"),
    )
    .await;
    assert_eq!(
        (st, out["error"].as_str()),
        (409, Some("That server is already added."))
    );
    // Input checks, in core's order and words.
    for (body, st, msg) in [
        (
            r#"{"url":"https://x.example/mcp"}"#.to_owned(),
            400,
            "Give the server a name.".to_owned(),
        ),
        (
            r#"{"title":"t","url":"http://x.example/mcp"}"#.to_owned(),
            400,
            "The address must be an https URL (no placeholders).".to_owned(),
        ),
        (
            r#"{"title":"t","url":"https://{tenant}.example/mcp"}"#.to_owned(),
            400,
            "The address must be an https URL (no placeholders).".to_owned(),
        ),
        (
            r#"{"title":"t","url":"https://10.0.0.5/mcp"}"#.to_owned(),
            422,
            "That address is not a public host.".to_owned(),
        ),
        (
            format!(r#"{{"title":"t","url":"{url}","headerName":"Host","headerValue":"x"}}"#),
            400,
            "Host cannot be used as a sign-in header.".to_owned(),
        ),
        (
            format!(r#"{{"title":"t","url":"{url}","headerName":"Bad Name","headerValue":"x"}}"#),
            400,
            "Bad Name cannot be used as a sign-in header.".to_owned(),
        ),
        (
            format!(r#"{{"title":"t","url":"{url}","headerName":"X-Api-Key","headerValue":""}}"#),
            400,
            "Enter X-Api-Key.".to_owned(),
        ),
    ] {
        let (got, out) = write(a, "POST", &format!("{LIST}/custom/preview"), ADMIN, &body).await;
        assert_eq!(
            (got, out["error"].as_str()),
            (st, Some(msg.as_str())),
            "{body}"
        );
    }
}

fn regex_like(token: &str) -> bool {
    let Some((exp, sig)) = token.split_once('.') else {
        return false;
    };
    exp.len() == 13
        && exp.bytes().all(|b| b.is_ascii_digit())
        && sig.len() == 64
        && sig.bytes().all(|b| b.is_ascii_hexdigit())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_custom_add_whose_tools_change_while_connecting_is_rolled_back_with_the_key_it_stored() {
    let (_d, db, up, s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    let url = format!("{}/mcp", up.base());
    *s.key.lock().unwrap() = Some(SECRET.into());
    let form = format!(
        r#"{{"title":"Keyed","url":"{url}","headerName":"X-Api-Key","headerValue":"{SECRET}","keyMode":"personal"}}"#
    );
    let (st, pre) = write(a, "POST", &format!("{LIST}/custom/preview"), ADMIN, &form).await;
    assert_eq!(st, 200, "{pre}");
    let token = pre["previewToken"].as_str().unwrap().to_owned();
    // The preview listed once; the connect lists twice (the check, then the re-listing with the stored key).
    // The server changes its tools after the second.
    let lists_now = s.lists.load(Ordering::SeqCst);
    let other = r#"[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}},{"name":"sneaky","inputSchema":{"type":"object"}}]"#;
    *s.swap_after.lock().unwrap() = Some((lists_now + 1, other.to_owned()));
    let body = format!(
        r#"{{"title":"Keyed","url":"{url}","headerName":"X-Api-Key","headerValue":"{SECRET}","keyMode":"personal","previewToken":"{token}"}}"#
    );
    let (st, out) = write(a, "POST", &format!("{LIST}/custom"), ADMIN, &body).await;
    assert_eq!(
        (st, out["error"].as_str()),
        (
            409,
            Some("The server’s tools changed while connecting. It was not kept; preview again.")
        ),
        "{out}"
    );
    assert_eq!(dir_count(&db), 0, "the server is gone");
    let leftover: i64 = db
        .query_row("SELECT count(*) FROM directory_mcp_user_keys", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        leftover, 0,
        "the administrator's key goes with the server it was for (the M3 fix)"
    );
    // Adding the same address again inherits nothing.
    *s.swap_after.lock().unwrap() = None;
    *s.tools.lock().unwrap() = ECHO.into();
    let (_, pre) = write(a, "POST", &format!("{LIST}/custom/preview"), ADMIN, &form).await;
    let token = pre["previewToken"].as_str().unwrap().to_owned();
    let body = format!(
        r#"{{"title":"Keyed","url":"{url}","headerName":"X-Api-Key","headerValue":"{SECRET}","keyMode":"personal","previewToken":"{token}"}}"#
    );
    let (st, out) = write(a, "POST", &format!("{LIST}/custom"), ADMIN, &body).await;
    assert_eq!(st, 201, "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_key_is_replaced_only_after_the_server_accepts_the_new_one_and_a_custom_server_keeps_its_declaration(
) {
    let (_d, db, up, s, f) = rig("", true, ECHO).await;
    let a = f.addr;
    *s.key.lock().unwrap() = Some("KEY-1".into());
    let (st, out) = write(
        a,
        "POST",
        LIST,
        ADMIN,
        r#"{"registryName":"io.test/weather","headers":{"X-Api-Key":"KEY-1"}}"#,
    )
    .await;
    assert_eq!(st, 201, "{out}");
    let id = "dir-io-test-weather";
    let before = dir_row(&db, id).unwrap().2.unwrap();
    let put = |id: &str, body: &str| {
        let (path, body) = (format!("{LIST}/{id}/keys"), body.to_owned());
        async move { write(a, "PUT", &path, ADMIN, &body).await }
    };
    // The server will accept only KEY-2: the new key is refused, and the old one stays.
    *s.key.lock().unwrap() = Some("KEY-2".into());
    let (st, out) = put(id, r#"{"headers":{"X-Api-Key":"KEY-3"}}"#).await;
    assert_eq!(st, 422, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .starts_with("The server did not accept that key: MCP 401"),
        "{out}"
    );
    assert_eq!(
        dir_row(&db, id).unwrap().2.unwrap(),
        before,
        "a key the server refused changes nothing"
    );
    let (st, out) = put(id, r#"{"headers":{}}"#).await;
    assert_eq!((st, out["error"].as_str()), (400, Some("Enter X-Api-Key.")));
    let (st, out) = put(id, r#"{"headers":{"X-Api-Key":"KEY-2"}}"#).await;
    assert_eq!(st, 200, "{out}");
    assert!(!out.to_string().contains("KEY-2"));
    let after = dir_row(&db, id).unwrap().2.unwrap();
    assert_ne!(after, before);
    assert_eq!(open(&after, None), j(r#"{"X-Api-Key":"KEY-2"}"#));
    assert_eq!(audits(&db, "mcp.directory.keys").len(), 1);
    let (st, out) = put("dir-nothing", r#"{"headers":{"X-Api-Key":"x"}}"#).await;
    assert_eq!((st, out["error"].as_str()), (404, Some("No such server.")));

    // A custom server declares its own header: re-keying it keeps the header (core asked the registry about
    // `url:...` names, found nothing and so cleared the key).
    let url = format!("{}/mcp", up.base());
    let form = format!(
        r#"{{"title":"Custom","url":"{url}","headerName":"X-Api-Key","headerValue":"KEY-2"}}"#
    );
    let (_, pre) = write(a, "POST", &format!("{LIST}/custom/preview"), ADMIN, &form).await;
    let token = pre["previewToken"].as_str().unwrap().to_owned();
    // (the registry server above has the same address, so remove it first)
    db.execute("DELETE FROM directory_mcp_servers", []).unwrap();
    let body = format!(
        r#"{{"title":"Custom","url":"{url}","headerName":"X-Api-Key","headerValue":"KEY-2","previewToken":"{token}"}}"#
    );
    let (st, out) = write(a, "POST", &format!("{LIST}/custom"), ADMIN, &body).await;
    assert_eq!(st, 201, "{out}");
    let cid = out["server"]["id"].as_str().unwrap().to_owned();
    *s.key.lock().unwrap() = Some("KEY-9".into());
    let (st, out) = put(&cid, r#"{"headers":{"X-Api-Key":"KEY-9"}}"#).await;
    assert_eq!(st, 200, "{out}");
    let enc = dir_row(&db, &cid)
        .unwrap()
        .2
        .expect("the key is still there");
    assert_eq!(open(&enc, None), j(r#"{"X-Api-Key":"KEY-9"}"#));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hand_registered_app_is_stored_and_the_administrator_is_sent_to_sign_in() {
    let (_d, db, up, s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    let base = up.base();
    let counts = Arc::new(Counts::default());
    let (b2, c2, inner) = (base.clone(), Arc::clone(&counts), Arc::clone(&s));
    up.set(move |r: &Seen| {
        let _ = &inner;
        oauth_reply(r, &b2, &c2).unwrap_or_else(|| reply("404 Not Found", ""))
    });
    add_server(&db, "dir-oauth", &format!("{base}/mcp"), true, false, "[]");
    add_server(&db, "dir-plain", &format!("{base}/mcp"), false, false, "[]");
    let put = |id: &str, body: &str| {
        let (path, body) = (format!("{LIST}/{id}/oauth-client"), body.to_owned());
        async move { write(a, "PUT", &path, ADMIN, &body).await }
    };
    let (st, out) = put("dir-plain", r#"{"clientId":"c"}"#).await;
    assert_eq!(
        (st, out["error"].as_str()),
        (404, Some("No such sign-in server."))
    );
    let (st, out) = put("dir-oauth", r#"{"clientSecret":"s"}"#).await;
    assert_eq!(st, 400, "{out}");
    assert!(out["error"]
        .as_str()
        .unwrap()
        .starts_with("Enter the client ID"));
    let (st, out) = put(
        "dir-oauth",
        r#"{"clientId":"hand-registered","clientSecret":"SECRET-APP"}"#,
    )
    .await;
    assert_eq!(st, 200, "{out}");
    assert!(out["signIn"].as_str().unwrap().contains("/authorize"));
    assert!(
        !out.to_string().contains("SECRET-APP"),
        "the secret is never shown"
    );
    let listed = out["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "dir-oauth")
        .unwrap();
    assert_eq!(listed["oauthClient"]["manual"], true);
    assert_eq!(listed["oauthClient"]["clientId"], "hand-registered");
    assert_eq!(listed["oauthClient"]["hasSecret"], true);
    assert!(!audits(&db, "mcp.oauth.client").is_empty());
    let stored: String = db
        .query_row(
            "SELECT data_enc FROM mcp_oauth_clients WHERE server_id='dir-oauth'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(stored.starts_with("enc:v1:") && !stored.contains("SECRET-APP"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_a_server_takes_every_accounts_sign_in_app_and_key_with_it() {
    let (_d, db, _up, _s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    for id in ["dir-a", "dir-b"] {
        add_server(&db, id, "http://127.0.0.1:9/mcp", true, true, DECLARED);
        for u in [A, B] {
            store_tokens(&db, u, id, "tok", Some("ref"), Some(now() + 3_600_000), 1);
            db.execute(
                "INSERT INTO directory_mcp_user_keys VALUES(?1,?2,?3,1)",
                (u, id, seal(r#"{"X-Api-Key":"k"}"#, Some(u), 2)),
            )
            .unwrap();
        }
        store_client(
            &db,
            id,
            "http://127.0.0.1:9",
            "http://x/api/mcp-oauth/callback",
            "",
        );
    }
    let (st, out) = write(a, "DELETE", &format!("{LIST}/dir-a"), ADMIN, "").await;
    assert_eq!(st, 200, "{out}");
    assert_eq!(out["servers"].as_array().unwrap().len(), 1);
    assert!(dir_row(&db, "dir-a").is_none());
    for u in [A, B] {
        assert!(token_row(&db, u, "dir-a").is_none() && key_row(&db, u, "dir-a").is_none());
        assert!(
            token_row(&db, u, "dir-b").is_some() && key_row(&db, u, "dir-b").is_some(),
            "the other server is untouched"
        );
    }
    assert!(client_row(&db, "dir-a").is_none() && client_row(&db, "dir-b").is_some());
    assert_eq!(audits(&db, "mcp.directory.remove").len(), 1);
    // Gone already: core's answer was a 404 after the (idempotent) forget.
    let (st, out) = write(a, "DELETE", &format!("{LIST}/dir-a"), ADMIN, "").await;
    assert_eq!((st, out["error"].as_str()), (404, Some("No such server.")));
    // The readiness file Node reads follows.
    let state = state_file(_d.path());
    assert!(
        state["oauth"][A].get("dir-a").is_none() && state["oauth"][A].get("dir-b").is_some(),
        "{state}"
    );
}

fn client_row(db: &server_store::rusqlite::Connection, server: &str) -> Option<String> {
    db.query_row(
        "SELECT data_enc FROM mcp_oauth_clients WHERE server_id=?1",
        [server],
        |r| r.get(0),
    )
    .ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_shows_what_nodes_published_discovery_found_and_only_while_it_is_current() {
    let (d, db, up, _s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    add_server(
        &db,
        "dir-found",
        &format!("{}/mcp", up.base()),
        false,
        false,
        "[]",
    );
    add_server(
        &db,
        "dir-down",
        &format!("{}/mcp", up.base()),
        true,
        false,
        "[]",
    );
    add_server(
        &db,
        "dir-unseen",
        &format!("{}/mcp", up.base()),
        false,
        false,
        "[]",
    );
    let many: Vec<serde_json::Value> = (0..40)
        .map(|i| serde_json::json!({"name": format!("t{i}"), "description": "d"}))
        .collect();
    write_snapshot(
        d.path(),
        serde_json::json!({"servers": [
            {"id": "dir-found", "auth": "none", "directory": true, "error": null, "toolCount": 55, "tools": many, "toolsTruncated": true},
            {"id": "dir-down", "auth": "oauth", "directory": true, "error": "waiting for the administrator who added it to sign in", "toolCount": 0, "tools": [], "toolsTruncated": false},
        ]}),
    );
    let (st, body) = get(a, LIST, ADMIN).await;
    assert_eq!(st, 200, "{body}");
    let by = |id: &str| {
        body["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(
        (
            by("dir-found")["toolCount"].clone(),
            by("dir-found")["tools"].as_array().unwrap().len(),
            by("dir-found")["toolsTruncated"].clone()
        ),
        (j("55"), 40, j("true"))
    );
    assert_eq!(
        by("dir-down")["error"],
        "waiting for the administrator who added it to sign in"
    );
    assert_eq!(by("dir-down")["oauth"], true);
    assert_eq!(by("dir-down")["oauthClient"], serde_json::Value::Null);
    assert!(by("dir-down")["redirectUri"]
        .as_str()
        .unwrap()
        .ends_with("/api/mcp-oauth/callback"));
    assert!(
        by("dir-found").get("redirectUri").is_none(),
        "only a sign-in server carries an app and a return address"
    );
    assert_eq!(
        (
            by("dir-unseen")["toolCount"].clone(),
            by("dir-unseen")["error"].clone()
        ),
        (serde_json::Value::Null, serde_json::Value::Null),
        "not discovered yet"
    );
    // The front asked Node to publish (the demand marker), and the listing never serves a file it cannot prove current.
    assert!(d.path().join("snapshot-demand").exists());
    let mut stale = std::fs::read_to_string(d.path().join("mcp-discovery.json")).unwrap();
    stale = stale.replacen("\"writtenAt\":", "\"writtenAt\":1,\"x\":", 1);
    std::fs::write(d.path().join("mcp-discovery.json"), stale).unwrap();
    let (_, body) = get(a, LIST, ADMIN).await;
    assert_eq!(
        body["servers"][0]["toolCount"],
        serde_json::Value::Null,
        "an old file is no discovery at all"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_directory_is_bounded_and_two_adds_of_one_server_make_one() {
    let (_d, db, up, _s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    // Two administrators add the same server at once: one wins, the other is told it is already added.
    let one = || {
        write(
            a,
            "POST",
            LIST,
            ADMIN,
            r#"{"registryName":"io.test/weather"}"#,
        )
    };
    let (x, y) = tokio::join!(one(), one());
    let mut codes = [x.0, y.0];
    codes.sort_unstable();
    assert_eq!(codes, [201, 409], "{x:?} {y:?}");
    assert_eq!(dir_count(&db), 1);
    // At most 20.
    for i in 1..20 {
        add_server(
            &db,
            &format!("dir-fill-{i}"),
            "http://127.0.0.1:9/mcp",
            false,
            false,
            "[]",
        );
    }
    assert_eq!(dir_count(&db), 20);
    let url = format!("{}/mcp", up.base());
    let form = format!(r#"{{"title":"Twenty one","url":"{url}"}}"#);
    let (_, pre) = write(a, "POST", &format!("{LIST}/custom/preview"), ADMIN, &form).await;
    let token = pre["previewToken"].as_str().unwrap().to_owned();
    let (st, out) = write(
        a,
        "POST",
        &format!("{LIST}/custom"),
        ADMIN,
        &format!(r#"{{"title":"Twenty one","url":"{url}","previewToken":"{token}"}}"#),
    )
    .await;
    assert_eq!(
        (st, out["error"].as_str()),
        (409, Some("At most 20 directory servers can be added.")),
        "{out}"
    );
    assert_eq!(dir_count(&db), 20);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_add_the_client_abandons_is_still_finished_and_nothing_is_stranded() {
    let (_d, db, _up, s, f) = rig("", false, ECHO).await;
    *s.list_delay.lock().unwrap() = Duration::from_millis(800);
    // Send the request and hang up before the server has listed its tools.
    let body = br#"{"registryName":"io.test/weather"}"#;
    let mut sock = tokio::net::TcpStream::connect(f.addr).await.unwrap();
    let head = format!(
        "POST {LIST} HTTP/1.1\r\nHost: {}\r\nCookie: {ADMIN}\r\nOrigin: http://{}\r\nX-CSRF-Token: csrf\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        f.addr, f.addr, body.len()
    );
    sock.write_all(head.as_bytes()).await.unwrap();
    sock.write_all(body).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(sock);
    // The task the handler spawned finishes the add.
    let mut done = false;
    for _ in 0..100 {
        if dir_count(&db) == 1 {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(done, "the abandoned add was finished");
    assert_eq!(audits(&db, "mcp.directory.add").len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_directory_names_no_private_address_and_no_redirect_is_followed() {
    let (_d, db, up, s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    // A server that redirects: never followed.
    up.set(|r: &Seen| {
        if r.path.starts_with("/v0/servers") {
            return reply(
                "200 OK",
                &registry_of(
                    &format!("http://{}", r.header("host").unwrap()),
                    "io.test/weather",
                    false,
                ),
            );
        }
        reply_with(
            "302 Found",
            "location: http://127.0.0.1:1/elsewhere\r\n",
            "",
        )
    });
    let (st, out) = write(
        a,
        "POST",
        LIST,
        ADMIN,
        r#"{"registryName":"io.test/weather"}"#,
    )
    .await;
    assert_eq!(st, 422, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("did not answer as an MCP server: fetch failed"),
        "{out}"
    );
    assert_eq!(dir_count(&db), 0);
    // Without the QA switch a loopback stand-in is not a public address at all.
    let d2 = tempfile::tempdir().unwrap();
    let db2 = seed(d2.path());
    let up2 = upstream().await;
    let s2 = synth(&up2, ECHO);
    *s2.registry.lock().unwrap() = Some(registry_of(&up2.base(), "io.test/weather", false));
    let f2 = front_env(d2.path(), false, &[("NOEVIA_QA_MCP_REGISTRY", up2.base())]).await;
    let (st, out) = write(
        f2.addr,
        "POST",
        LIST,
        ADMIN,
        r#"{"registryName":"io.test/weather"}"#,
    )
    .await;
    assert_eq!(st, 422, "{out}");
    assert_eq!(dir_count(&db2), 0);
    assert_eq!(
        s2.lists.load(Ordering::SeqCst),
        0,
        "nothing was sent to a loopback address"
    );
    let _ = s;
}

// ---------------------------------------------------------------------------------------------
// Node's real chat-time MCP client over the directory the front writes (needs a paired noevia-core
// checkout: NOEVIA_MCP_CORE, the contracts/http/front-core.ref generation).

fn paired_core() -> Option<std::path::PathBuf> {
    match std::env::var("NOEVIA_MCP_CORE") {
        Ok(path) if !path.is_empty() => Some(path.into()),
        _ => {
            assert!(
                std::env::var("NOEVIA_MCP_CORE_REQUIRED").is_err(),
                "NOEVIA_MCP_CORE_REQUIRED is set but NOEVIA_MCP_CORE is not"
            );
            eprintln!("SKIPPED node_lists_the_tools_of_a_server_the_front_added: NOEVIA_MCP_CORE (a noevia-core checkout at contracts/http/front-core.ref) is not set");
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_lists_the_tools_of_a_server_the_front_added_through_its_reread_directory() {
    use std::io::{BufRead, BufReader};
    let Some(core) = paired_core() else {
        return;
    };
    let (d, db, up, s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    *s.key.lock().unwrap() = Some(SECRET.into());
    let url = format!("{}/mcp", up.base());
    // One real `node` process over the shared database, with the paired Core's real directory re-reader, MCP
    // wiring and MCP client. It prints a line per stage; the test moves it on by creating a file.
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp_node_live.cjs");
    let mut child = std::process::Command::new("node")
        .arg(script)
        .arg("directory")
        .env("NOEVIA_MCP_CORE", &core)
        .env("STORE_PORT", f.store.port().to_string())
        .env("STORE_TOKEN", STORE_TOKEN)
        .env("DATA_DIR", d.path())
        .env("MCP_URL", &url)
        .env("USER_ID", A)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let next = |rx: &std::sync::mpsc::Receiver<String>| {
        rx.recv_timeout(Duration::from_secs(60)).map(|l| j(&l))
    };

    // 1. Nothing in the directory: Node has no directory server and no tool.
    let rx = Arc::new(Mutex::new(rx));
    let r2 = Arc::clone(&rx);
    let before = tokio::task::spawn_blocking(move || next(&r2.lock().unwrap()))
        .await
        .unwrap()
        .expect("the before stage");
    assert_eq!(
        (
            before["stage"].clone(),
            before["servers"].clone(),
            before["tools"].clone()
        ),
        (j(r#""before""#), j("[]"), j("[]")),
        "{before}"
    );

    // 2. The front adds a custom server with a shared key (the real route: preview, then connect).
    let form = format!(
        r#"{{"title":"Live directory","url":"{url}","headerName":"X-Api-Key","headerValue":"{SECRET}"}}"#
    );
    let (st, pre) = write(a, "POST", &format!("{LIST}/custom/preview"), ADMIN, &form).await;
    assert_eq!(st, 200, "{pre}");
    let token = pre["previewToken"].as_str().unwrap();
    let body = format!(
        r#"{{"title":"Live directory","url":"{url}","headerName":"X-Api-Key","headerValue":"{SECRET}","previewToken":"{token}"}}"#
    );
    let (st, out) = write(a, "POST", &format!("{LIST}/custom"), ADMIN, &body).await;
    assert_eq!(st, 201, "{out}");
    let id = out["server"]["id"].as_str().unwrap().to_owned();
    std::fs::write(d.path().join("go-added"), "").unwrap();
    // 3. Node, asked for its tools in the ordinary way, has re-read the table: the server, its box, its tool, and a
    //    call that carries the shared key the front sealed (Node opened it with its own copy of the secret).
    let r3 = Arc::clone(&rx);
    let added = tokio::task::spawn_blocking(move || next(&r3.lock().unwrap()))
        .await
        .unwrap()
        .expect("the added stage");
    assert_eq!(added["stage"], "added", "{added}");
    assert_eq!(added["servers"], j(&format!(r#"["{id}"]"#)), "{added}");
    assert_eq!(
        added["auth"],
        j(r#"["directory"]"#),
        "the shared key makes it a directory-key server: {added}"
    );
    assert_eq!(added["tools"], j(r#"["echo"]"#), "{added}");
    assert_eq!(added["boxes"], j(&format!(r#"["{id}"]"#)), "{added}");
    assert_eq!(added["errors"], j("[null]"), "{added}");
    assert_eq!(added["text"], "echo:hi", "{added}");
    assert!(up
        .seen()
        .iter()
        .filter(|r| r.body.contains("tools/call"))
        .all(|r| r.header("x-api-key") == Some(SECRET.to_ascii_lowercase())));

    // 4. The front removes it: Node drops the server and its tool without a restart.
    let (st, out) = write(a, "DELETE", &format!("{LIST}/{id}"), ADMIN, "").await;
    assert_eq!(st, 200, "{out}");
    assert_eq!(dir_count(&db), 0);
    std::fs::write(d.path().join("go-removed"), "").unwrap();
    let r4 = Arc::clone(&rx);
    let removed = tokio::task::spawn_blocking(move || next(&r4.lock().unwrap()))
        .await
        .unwrap()
        .expect("the removed stage");
    assert_eq!(
        (
            removed["servers"].clone(),
            removed["tools"].clone(),
            removed["boxes"].clone()
        ),
        (j("[]"), j("[]"), j("[]")),
        "{removed}"
    );
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_follows_pages_and_reads_event_streams_and_cuts_a_long_list() {
    let many: Vec<String> = (0..45)
        .map(|i| {
            format!(
                r#"{{"name":"t{i}","description":"{}","inputSchema":{{"type":"object"}}}}"#,
                "d".repeat(300)
            )
        })
        .collect();
    let (_d, db, up, s, f) = rig("", false, &format!("[{}]", many.join(","))).await;
    let a = f.addr;
    *s.paged.lock().unwrap() = true;
    *s.sse.lock().unwrap() = true;
    let url = format!("{}/mcp", up.base());
    let form = format!(r#"{{"title":"Paged","url":"{url}"}}"#);
    let (st, pre) = write(a, "POST", &format!("{LIST}/custom/preview"), ADMIN, &form).await;
    assert_eq!(st, 200, "{pre}");
    // 15 pages of three tools, each an event stream with a keep-alive comment and an unrelated event before the reply.
    assert_eq!(pre["toolCount"], 45, "{pre}");
    assert_eq!(
        pre["tools"].as_array().unwrap().len(),
        40,
        "the first 40 are shown"
    );
    assert_eq!(pre["toolsTruncated"], true);
    assert_eq!(pre["tools"][0]["name"], "t0");
    assert_eq!(
        pre["tools"][0]["description"].as_str().unwrap().len(),
        240,
        "a description is cut at 240"
    );
    assert!(s.lists.load(Ordering::SeqCst) >= 15, "one request per page");
    let token = pre["previewToken"].as_str().unwrap();
    let (st, out) = write(
        a,
        "POST",
        &format!("{LIST}/custom"),
        ADMIN,
        &format!(r#"{{"title":"Paged","url":"{url}","previewToken":"{token}"}}"#),
    )
    .await;
    assert_eq!(st, 201, "{out}");
    assert_eq!(out["servers"][0]["toolCount"], 45);
    assert_eq!(out["servers"][0]["toolsTruncated"], true);
    assert_eq!(dir_count(&db), 1);
    // A server that pages forever stops at twenty pages and keeps what it gathered (core mcp.cjs MCP_LIST_TOOLS_MAX_PAGES).
    let hundred: Vec<String> = (0..100)
        .map(|i| format!(r#"{{"name":"u{i}","inputSchema":{{"type":"object"}}}}"#))
        .collect();
    *s.tools.lock().unwrap() = format!("[{}]", hundred.join(","));
    let (_, pre) = write(
        a,
        "POST",
        &format!("{LIST}/custom/preview"),
        ADMIN,
        &format!(r#"{{"title":"Endless","url":"{url}"}}"#),
    )
    .await;
    assert_eq!(pre["toolCount"], 60, "{pre}");
}

// ---------------------------------------------------------------------------------------------
// The review of round 2 (M3, L2): a rollback says what it could not undo, and forgets only what is its own.

/// The preview and the body of a connect for a personal-key custom server (the administrator's own key is kept).
async fn personal_form(a: std::net::SocketAddr, url: &str) -> String {
    let form = format!(
        r#"{{"title":"Keyed","url":"{url}","headerName":"X-Api-Key","headerValue":"{SECRET}","keyMode":"personal"}}"#
    );
    let (st, pre) = write(a, "POST", &format!("{LIST}/custom/preview"), ADMIN, &form).await;
    assert_eq!(st, 200, "{pre}");
    let token = pre["previewToken"].as_str().unwrap();
    format!(
        r#"{{"title":"Keyed","url":"{url}","headerName":"X-Api-Key","headerValue":"{SECRET}","keyMode":"personal","previewToken":"{token}"}}"#
    )
}

/// The tools change after the connect's check, so the re-listing shows others and the add is rolled back.
fn change_after_the_check(s: &Synth) {
    let lists_now = s.lists.load(Ordering::SeqCst);
    let other = r#"[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}},{"name":"sneaky","inputSchema":{"type":"object"}}]"#;
    *s.swap_after.lock().unwrap() = Some((lists_now + 1, other.to_owned()));
}

fn keys_of(db: &server_store::rusqlite::Connection, server: &str) -> Vec<String> {
    let mut stmt = db
        .prepare("SELECT user_id FROM directory_mcp_user_keys WHERE server_id=?1 ORDER BY user_id")
        .unwrap();
    stmt.query_map([server], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rollback_that_cannot_remove_the_row_is_loud_and_the_server_can_be_removed_by_hand() {
    let (_d, db, up, s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    let url = format!("{}/mcp", up.base());
    *s.key.lock().unwrap() = Some(SECRET.into());
    let body = personal_form(a, &url).await;
    change_after_the_check(&s);
    // The database refuses to delete a directory row (as a locked or read-only file would).
    db.execute_batch(
        "CREATE TRIGGER refuse_delete BEFORE DELETE ON directory_mcp_servers BEGIN SELECT RAISE(ABORT, 'locked'); END;",
    )
    .unwrap();
    let (st, out) = write(a, "POST", &format!("{LIST}/custom"), ADMIN, &body).await;
    let id: String = db
        .query_row("SELECT id FROM directory_mcp_servers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(st, 500, "not a quiet 409: {out}");
    let message = out["error"].as_str().unwrap();
    assert!(
        message.contains("by hand") && message.contains(&id),
        "it says which server is left and what to do: {message}"
    );
    assert_eq!(dir_count(&db), 1, "the server is still listed");
    assert_eq!(keys_of(&db, &id), [A], "and so is the key it holds");
    // By hand: once the database lets go, removing it from the list clears the server and its key.
    db.execute_batch("DROP TRIGGER refuse_delete").unwrap();
    let (st, out) = write(a, "DELETE", &format!("{LIST}/{id}"), ADMIN, "").await;
    assert_eq!(st, 200, "{out}");
    assert_eq!(dir_count(&db), 0);
    assert!(keys_of(&db, &id).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rollback_that_cannot_clear_the_keys_says_so_and_adding_and_removing_again_clears_them() {
    let (_d, db, up, s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    let url = format!("{}/mcp", up.base());
    *s.key.lock().unwrap() = Some(SECRET.into());
    let body = personal_form(a, &url).await;
    change_after_the_check(&s);
    db.execute_batch(
        "CREATE TRIGGER refuse_key_delete BEFORE DELETE ON directory_mcp_user_keys BEGIN SELECT RAISE(ABORT, 'locked'); END;",
    )
    .unwrap();
    let (st, out) = write(a, "POST", &format!("{LIST}/custom"), ADMIN, &body).await;
    assert_eq!(st, 500, "{out}");
    let message = out["error"].as_str().unwrap();
    assert_eq!(dir_count(&db), 0, "the row did go");
    let id: String = db
        .query_row("SELECT server_id FROM directory_mcp_user_keys", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(
        message.contains("could not be cleared") && message.contains(&id),
        "{message}"
    );
    assert_eq!(keys_of(&db, &id), [A], "the key is what is left");
    // As the message says: add it again, remove it, and nothing is left.
    db.execute_batch("DROP TRIGGER refuse_key_delete").unwrap();
    *s.swap_after.lock().unwrap() = None;
    *s.tools.lock().unwrap() = ECHO.into();
    let body = personal_form(a, &url).await;
    let (st, out) = write(a, "POST", &format!("{LIST}/custom"), ADMIN, &body).await;
    assert_eq!(st, 201, "{out}");
    let (st, out) = write(a, "DELETE", &format!("{LIST}/{id}"), ADMIN, "").await;
    assert_eq!(st, 200, "{out}");
    assert!(keys_of(&db, &id).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rollback_that_finds_the_server_replaced_forgets_only_the_adding_administrators_key() {
    let (_d, db, up, s, f) = rig("", false, ECHO).await;
    let a = f.addr;
    let url = format!("{}/mcp", up.base());
    *s.key.lock().unwrap() = Some(SECRET.into());
    let body = personal_form(a, &url).await;
    change_after_the_check(&s);
    let lists_before = s.lists.load(Ordering::SeqCst);
    // Slow listings give the test the moment between the add and its rollback.
    *s.list_delay.lock().unwrap() = Duration::from_millis(1500);
    let request = tokio::spawn({
        let body = body.clone();
        async move { write(a, "POST", &format!("{LIST}/custom"), ADMIN, &body).await }
    });
    // The re-listing has started: the row is in, and the administrator's key is stored.
    for _ in 0..200 {
        if s.lists.load(Ordering::SeqCst) >= lists_before + 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(s.lists.load(Ordering::SeqCst) >= lists_before + 2);
    let (id, added_at): (String, i64) = db
        .query_row("SELECT id, added_at FROM directory_mcp_servers", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    // Another administrator removed that server and added one of the same name; a member has a key for it.
    db.execute(
        "UPDATE directory_mcp_servers SET added_at=?1 WHERE id=?2",
        (added_at + 1, &id),
    )
    .unwrap();
    db.execute(
        "INSERT INTO directory_mcp_user_keys(user_id,server_id,headers_enc,updated_at) VALUES(?1,?2,'enc:v2:x',1)",
        (B, &id),
    )
    .unwrap();
    assert_eq!(keys_of(&db, &id), [A, B]);
    let (st, out) = request.await.unwrap();
    assert_eq!(st, 409, "{out}");
    assert_eq!(
        dir_count(&db),
        1,
        "the other administrator's server is not removed"
    );
    assert_eq!(
        keys_of(&db, &id),
        [B],
        "only the key this request stored is forgotten, not everyone's"
    );
}

#[test]
fn the_snapshot_node_writes_for_hostile_tools_is_one_the_front_reads() {
    // The review of round 2, H1: a description cut through an emoji left a lone surrogate, which this reader's
    // JSON parser refuses, and twenty thousand tools passed the 4 MiB the reader accepts: either made the
    // snapshot unreadable for every account. Node's real publisher writes both cases; the front's reader reads it.
    let Some(core) = paired_core() else {
        return;
    };
    let d = tempfile::tempdir().unwrap();
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp_snapshot_live.cjs");
    let out = std::process::Command::new("node")
        .arg(script)
        .env("NOEVIA_MCP_CORE", &core)
        .env("DATA_DIR", d.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "node: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let file = d.path().join("mcp-discovery.json");
    let size = std::fs::metadata(&file).unwrap().len();
    assert!(size < 4 * 1024 * 1024, "{size} bytes");
    let snap = noevia_server::snapshot::read(
        &file,
        noevia_server::mcp_discovery::SNAPSHOT_KIND,
        noevia_server::identity::now_ms(),
    )
    .expect("the front reads what Node wrote");
    let cat = noevia_server::mcp_discovery::parse(&snap.body).expect("and it is a catalogue");
    let b = &cat.boxes[0];
    assert!(
        !b.tools.is_empty() && b.tools.len() <= 500,
        "{}",
        b.tools.len()
    );
    assert_eq!(
        b.tools[0].description.chars().count(),
        240,
        "the cut keeps 239 letters and the replacement for the half of the pair"
    );
    assert!(b.tools[0].description.ends_with('\u{FFFD}'));
    assert_eq!(cat.servers[0].tool_count, Some(20000));
}
