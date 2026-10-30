//! `GET /api/toolboxes/permitted`, `GET /api/connectors` and `PUT /api/connectors/nextcloud/policy`
//! (sbstndalton/noevia#2202 round 2) through the real front. Node's published discovery is a file the
//! test writes (core mcp-discovery-snapshot.cjs makes the real one; the live Node test in
//! mcp_credentials.rs runs that code); everything per account is the front's own state, changed
//! between two requests to show that nothing is cached. Synthetic accounts and boxes only.
#![cfg(feature = "rust-storage-reads")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use support::*;

const PERMITTED: &str = "/api/toolboxes/permitted";

fn tool(name: &str, write: bool) -> Value {
    json!({"name": name, "description": format!("{name} (synthetic)"), "write": write})
}

fn bx(id: &str, source: &str, server: Option<&str>, tools: Vec<Value>) -> Value {
    json!({"id": id, "label": id, "description": format!("{id} box"), "source": source, "inApp": source != "mcp" || id.starts_with("nextcloud"), "server": server, "tools": tools})
}

fn catalogue() -> Value {
    json!({
        "harnessEnabled": false, "hasRepositories": true,
        "nextcloudConfigured": true, "nextcloudOrigins": ["https://cloud.example.test"],
        "boxes": [
            bx("core", "builtin", None, vec![tool("get_current_time", false), tool("read_project_file", false)]),
            bx("nextcloud-notes", "mcp", Some("nextcloud"), vec![tool("nc_notes_search_notes", false), tool("nc_notes_create_note", true)]),
            bx("dir-x", "mcp", Some("dir-x"), vec![tool("x_read", false)]),
            bx("gdrive", "builtin", None, vec![tool("drive_search_files", false), tool("drive_update_file", true)]),
            bx("diary", "mcp", Some("sso-mcp"), vec![tool("diary_read_month", false)]),
        ],
        "manifest": [{"id": "nextcloud-calendar", "label": "Calendar", "description": "Cal", "inApp": true}],
    })
}

fn project(dir: &std::path::Path, user: &str, project: Value) {
    let d = dir.join("users").join(user);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("projects.json"),
        json!({"projects": [project]}).to_string(),
    )
    .unwrap();
}

async fn rig() -> (tempfile::TempDir, server_store::rusqlite::Connection, Front) {
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    let f = front(d.path()).await;
    write_snapshot(d.path(), catalogue());
    project(
        d.path(),
        A,
        json!({"id": "p1", "toolsMode": "manual", "toolboxes": ["core", "nextcloud-notes", "dir-x", "diary"]}),
    );
    (d, db, f)
}

fn boxes(body: &Value) -> std::collections::HashMap<String, Value> {
    body["boxes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| (b["id"].as_str().unwrap().to_owned(), b.clone()))
        .collect()
}

fn perm(b: &Value, tool: &str) -> String {
    b["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == tool)
        .unwrap()["permission"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_view_is_computed_by_the_front_from_the_catalogue_and_the_account() {
    let (_d, db, f) = rig().await;
    let a = f.addr;
    add_server(&db, "dir-x", "http://127.0.0.1:9/mcp", true, false, "[]");
    let (st, body) = get(a, &format!("{PERMITTED}?projectId=p1&mode=chat"), ADMIN).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(
        (body["mode"].clone(), body["projectId"].clone()),
        (json!("chat"), json!("p1"))
    );
    assert!(body["computedAt"].as_i64().unwrap() > 1_700_000_000_000);
    let order: Vec<&str> = body["boxes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            "core",
            "nextcloud-notes",
            "dir-x",
            "gdrive",
            "diary",
            "nextcloud-calendar",
            "code"
        ],
        "offered boxes, the configured but undiscovered, then the coding harness"
    );
    let b = boxes(&body);
    // A box shows what Node's catalogue says of it, in core's key order.
    let (_, _, text) = raw_text(
        a,
        "GET",
        &format!("{PERMITTED}?projectId=p1&mode=chat"),
        ADMIN,
        "",
        b"",
    )
    .await;
    assert!(
        text.contains(r#"{"id":"core","label":"core","description":"core box","source":"builtin","inApp":true,"state":"available","reason":null,"reasonCode":null,"active":true,"tools":[{"name":"get_current_time","description":"get_current_time (synthetic)","write":false,"permission":"allowed","reason":null,"reasonCode":null}"#),
        "{text}"
    );
    assert_eq!(
        (b["core"]["state"].clone(), b["core"]["active"].clone()),
        (json!("available"), json!(true))
    );
    assert_eq!(perm(&b["core"], "get_current_time"), "allowed");
    assert_eq!(
        b["core"]["tools"][0],
        json!({"name": "get_current_time", "description": "get_current_time (synthetic)", "write": false, "permission": "allowed", "reason": null, "reasonCode": null})
    );
    // Reads run, writes ask; selected by the project means active.
    assert_eq!(
        perm(&b["nextcloud-notes"], "nc_notes_search_notes"),
        "allowed"
    );
    assert_eq!(
        perm(&b["nextcloud-notes"], "nc_notes_create_note"),
        "needs-approval"
    );
    assert_eq!(b["nextcloud-notes"]["active"], true);
    // A sign-in service the account has not signed in to; a connector that is not connected; the Diary add-on off; a configured box with no server.
    assert_eq!(
        (
            b["dir-x"]["state"].clone(),
            b["dir-x"]["reasonCode"].clone()
        ),
        (json!("unavailable"), json!("signIn"))
    );
    assert_eq!(
        b["dir-x"]["reason"],
        "Sign in to this service in Settings → Connected apps first."
    );
    assert_eq!(perm(&b["dir-x"], "x_read"), "unavailable");
    assert_eq!(b["dir-x"]["tools"][0]["reasonCode"], "signIn");
    assert_eq!(b["gdrive"]["reasonCode"], "connect");
    assert_eq!(b["diary"]["reasonCode"], "diaryOff");
    assert_eq!(
        (
            b["nextcloud-calendar"]["reasonCode"].clone(),
            b["nextcloud-calendar"]["tools"].clone()
        ),
        (json!("notConnected"), json!([]))
    );
    assert_eq!(b["nextcloud-calendar"]["inApp"], true);
    // The coding harness: Cowork only, then admin only, then switched on.
    assert_eq!(b["code"]["reasonCode"], "codeNeedsCowork");
    assert_eq!(b["code"]["tools"][1]["write"], true);
    let (_, cowork) = get(a, &format!("{PERMITTED}?projectId=p1&mode=cowork"), ADMIN).await;
    assert_eq!(
        boxes(&cowork)["code"]["reasonCode"],
        "codeOff",
        "an administrator in Cowork, harness off"
    );
    let (_, member) = get(a, &format!("{PERMITTED}?mode=cowork"), MEMBER).await;
    assert_eq!(boxes(&member)["code"]["reasonCode"], "codeAdminOnly");
    // No project: the operator default is the selection; the account has no project to run a task in.
    let (_, free) = get(a, PERMITTED, ADMIN).await;
    assert_eq!(free["projectId"], Value::Null);
    assert_eq!(boxes(&free)["core"]["active"], true);
    assert_eq!(
        boxes(&free)["nextcloud-notes"]["active"],
        false,
        "available, not selected: offered for a turn, not on by default"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_the_front_writes_is_stale_for_a_moment() {
    let (d, db, f) = rig().await;
    let a = f.addr;
    add_server(&db, "dir-x", "http://127.0.0.1:9/mcp", true, false, "[]");
    let view = || async {
        boxes(
            &get(a, &format!("{PERMITTED}?projectId=p1&mode=chat"), ADMIN)
                .await
                .1,
        )
    };
    let first = view().await;
    assert_eq!(
        perm(&first["nextcloud-notes"], "nc_notes_search_notes"),
        "allowed"
    );
    assert_eq!(first["dir-x"]["state"], "unavailable");
    // A tool permission (the front is its only writer): the very next request shows it.
    db.execute(
        "INSERT INTO tool_policies VALUES(?1,'nc_notes_search_notes','block',1)",
        [A],
    )
    .unwrap();
    let blocked = view().await;
    let t = blocked["nextcloud-notes"]["tools"][0].clone();
    assert_eq!(
        (
            t["permission"].clone(),
            t["reasonCode"].clone(),
            t["reason"].clone()
        ),
        (
            json!("unavailable"),
            json!("blocked"),
            json!("Blocked in your tool permissions.")
        )
    );
    db.execute("UPDATE tool_policies SET mode='ask' WHERE user_id=?1", [A])
        .unwrap();
    assert_eq!(
        perm(&view().await["nextcloud-notes"], "nc_notes_search_notes"),
        "needs-approval",
        "a read set to Ask asks"
    );
    // Another account's permission is not this account's.
    db.execute(
        "INSERT INTO tool_policies VALUES(?1,'get_current_time','block',1)",
        [B],
    )
    .unwrap();
    assert_eq!(perm(&view().await["core"], "get_current_time"), "allowed");
    // The project's toolbox selection (the front writes projects.json): the next request shows it.
    project(
        d.path(),
        A,
        json!({"id": "p1", "toolsMode": "manual", "toolboxes": ["nextcloud-notes"]}),
    );
    let narrowed = view().await;
    assert_eq!(
        (
            narrowed["core"]["active"].clone(),
            narrowed["nextcloud-notes"]["active"].clone()
        ),
        (json!(false), json!(true))
    );
    // The account signs in to the sign-in service (the front's broker writes the token): available at once.
    store_tokens(
        &db,
        A,
        "dir-x",
        "tok",
        Some("ref"),
        Some(now() + 3_600_000),
        1,
    );
    let signed = view().await;
    assert_eq!(
        (
            signed["dir-x"]["state"].clone(),
            signed["dir-x"]["reasonCode"].clone()
        ),
        (json!("available"), Value::Null)
    );
    assert_eq!(perm(&signed["dir-x"], "x_read"), "allowed");
    db.execute("DELETE FROM mcp_oauth_tokens", []).unwrap();
    assert_eq!(
        view().await["dir-x"]["state"],
        "unavailable",
        "and gone the moment the sign-in is"
    );
    // A per-account key server is ready by the key.
    db.execute(
        "UPDATE directory_mcp_servers SET oauth=0, personal=1 WHERE id='dir-x'",
        [],
    )
    .unwrap();
    assert_eq!(view().await["dir-x"]["state"], "unavailable");
    db.execute(
        "INSERT INTO directory_mcp_user_keys VALUES(?1,'dir-x',?2,1)",
        (A, seal(r#"{"X-Api-Key":"k"}"#, Some(A), 3)),
    )
    .unwrap();
    assert_eq!(view().await["dir-x"]["state"], "available");
    // The Diary add-on preference.
    assert_eq!(view().await["diary"]["reasonCode"], "diaryOff");
    db.execute(
        "INSERT INTO user_features(user_id,diary_enabled,onboarded,updated_at) VALUES(?1,1,1,1)",
        [A],
    )
    .unwrap();
    assert_eq!(view().await["diary"]["state"], "available");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_catalogue_that_is_not_provably_current_is_not_ready_and_the_rest_is_refused_as_core_refused_it(
) {
    let (d, _db, f) = rig().await;
    let a = f.addr;
    // Tenant boundary: another account's project is a 404, not an empty catalogue.
    let (st, body) = get(a, &format!("{PERMITTED}?projectId=p1"), MEMBER).await;
    assert_eq!(
        (st, body["error"].as_str()),
        (404, Some("project not found"))
    );
    let (st, body) = get(a, &format!("{PERMITTED}?projectId=nope"), ADMIN).await;
    assert_eq!(
        (st, body["error"].as_str()),
        (404, Some("project not found"))
    );
    let (st, body) = get(a, &format!("{PERMITTED}?mode=agent"), ADMIN).await;
    assert_eq!(
        (st, body["error"].as_str()),
        (400, Some("mode must be \"chat\" or \"cowork\""))
    );
    let (st, _) = get(a, PERMITTED, "").await;
    assert_eq!(st, 401);
    let (st, body) = write(a, "POST", PERMITTED, ADMIN, "{}").await;
    assert_eq!(
        (st, body["error"].as_str()),
        (405, Some("method not allowed"))
    );
    // An empty projectId is no project; an empty mode is chat.
    let (st, body) = get(a, &format!("{PERMITTED}?projectId=&mode="), ADMIN).await;
    assert_eq!(
        (st, body["mode"].clone(), body["projectId"].clone()),
        (200, json!("chat"), Value::Null)
    );
    // The front asked Node to publish.
    assert!(d.path().join("snapshot-demand").exists());
    // A file that is missing, corrupt, of another kind or older than two intervals is no catalogue: 503, try again.
    for (why, text) in [
        ("old", std::fs::read_to_string(d.path().join("mcp-discovery.json")).unwrap().replacen("\"writtenAt\":", "\"writtenAt\":1,\"was\":", 1)),
        ("corrupt", "{not json".to_owned()),
        ("kind", std::fs::read_to_string(d.path().join("mcp-discovery.json")).unwrap().replacen("mcp-discovery", "rag-status", 1)),
        ("shape", json!({"version": 1, "kind": "mcp-discovery", "writtenAt": noevia_server::identity::now_ms(), "everyMs": 3000, "boxes": "none"}).to_string()),
    ] {
        std::fs::write(d.path().join("mcp-discovery.json"), text).unwrap();
        let (st, h, b) = raw_text(a, "GET", PERMITTED, ADMIN, "", b"").await;
        assert_eq!(st, 503, "{why}: {b}");
        assert!(h.contains("retry-after: 2"), "{why}: {h}");
        assert!(b.contains("being prepared"), "{why}: {b}");
    }
    std::fs::remove_file(d.path().join("mcp-discovery.json")).unwrap();
    let (st, _) = get(a, PERMITTED, ADMIN).await;
    assert_eq!(st, 503);
}

fn storage(
    db: &server_store::rusqlite::Connection,
    user: &str,
    kind: &str,
    base: &str,
    name: &str,
    secret: &str,
) {
    db.execute(
        "INSERT OR REPLACE INTO storage_connections(user_id,kind,base_url,username,secret,corpus_root,updated_at) VALUES(?1,?2,?3,?4,?5,'',1)",
        (user, kind, base, name, secret),
    )
    .unwrap();
}

const CONNECTORS: &str = "/api/connectors";
const POLICY: &str = "/api/connectors/nextcloud/policy";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_list_joins_drive_with_nextcloud_from_the_storage_connection_the_origins_and_the_permissions(
) {
    let (d, db, f) = rig().await;
    let a = f.addr;
    let nc = |body: &Value| body["connectors"][1].clone();
    let (st, body) = get(a, CONNECTORS, MEMBER).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["connectors"][0]["id"], "gdrive");
    assert_eq!(body["connectors"][0]["state"], "not-configured");
    // No storage connection: its tools have nothing to use.
    let n = nc(&body);
    assert_eq!(
        (
            n["id"].clone(),
            n["name"].clone(),
            n["configured"].clone(),
            n["state"].clone()
        ),
        (
            json!("nextcloud"),
            json!("Nextcloud"),
            json!(true),
            json!("disconnected")
        )
    );
    assert_eq!(n["message"], "Connect Nextcloud under Settings → Diary & storage; its tools then use that same connection.");
    assert_eq!(
        (n["account"].clone(), n["baseUrl"].clone()),
        (Value::Null, Value::Null)
    );
    assert_eq!(
        n["boxes"],
        json!([{"id": "nextcloud-notes", "label": "nextcloud-notes", "toolCount": 2}])
    );
    assert_eq!(
        n["tools"][0],
        json!({"name": "nc_notes_search_notes", "label": "nc_notes_search_notes", "write": false, "mode": "allow"})
    );
    assert_eq!(n["tools"][1]["mode"], "ask", "a write asks by default");
    // A Nextcloud connection with a credential that opens, at an origin the operator declared.
    storage(
        &db,
        B,
        "nextcloud",
        "https://cloud.example.test/remote.php/dav",
        "bea",
        &seal("app-password", Some(B), 5),
    );
    let (_, body) = get(a, CONNECTORS, MEMBER).await;
    let n = nc(&body);
    assert_eq!(
        (
            n["state"].clone(),
            n["account"].clone(),
            n["baseUrl"].clone(),
            n["message"].clone()
        ),
        (
            json!("connected"),
            json!("bea"),
            json!("https://cloud.example.test/remote.php/dav"),
            json!("")
        )
    );
    assert!(!body.to_string().contains("app-password"));
    // The same, but at an origin the operator did not declare: the credential would not be forwarded.
    storage(
        &db,
        B,
        "webdav",
        "https://other.example.test/dav",
        "bea",
        &seal("app-password", Some(B), 6),
    );
    let n = nc(&get(a, CONNECTORS, MEMBER).await.1);
    assert_eq!(
        (n["state"].clone(), n["account"].clone()),
        (json!("error"), json!("bea"))
    );
    assert!(n["message"]
        .as_str()
        .unwrap()
        .contains("MCP_NEXTCLOUD_ORIGINS"));
    // A credential no key opens, a local connection and a connection without a user name are not connected.
    storage(
        &db,
        B,
        "nextcloud",
        "https://cloud.example.test/dav",
        "bea",
        "enc:v2:AAAA-not-a-sealed-value",
    );
    assert_eq!(
        nc(&get(a, CONNECTORS, MEMBER).await.1)["state"],
        "disconnected"
    );
    storage(&db, B, "local", "", "", "");
    assert_eq!(
        nc(&get(a, CONNECTORS, MEMBER).await.1)["state"],
        "disconnected"
    );
    // Another account's connection is not this one's.
    storage(
        &db,
        A,
        "nextcloud",
        "https://cloud.example.test/dav",
        "ann",
        &seal("pw", Some(A), 7),
    );
    assert_eq!(
        nc(&get(a, CONNECTORS, MEMBER).await.1)["state"],
        "disconnected"
    );
    assert_eq!(nc(&get(a, CONNECTORS, ADMIN).await.1)["state"], "connected");
    // A permission the front wrote shows at once.
    db.execute(
        "INSERT INTO tool_policies VALUES(?1,'nc_notes_search_notes','block',1)",
        [B],
    )
    .unwrap();
    assert_eq!(
        nc(&get(a, CONNECTORS, MEMBER).await.1)["tools"][0]["mode"],
        "block"
    );
    assert_eq!(
        nc(&get(a, CONNECTORS, ADMIN).await.1)["tools"][0]["mode"],
        "allow"
    );
    // No Nextcloud MCP server configured on this deployment.
    write_snapshot(
        d.path(),
        json!({"nextcloudConfigured": false, "boxes": catalogue()["boxes"].clone()}),
    );
    let n = nc(&get(a, CONNECTORS, MEMBER).await.1);
    assert_eq!(
        (
            n["configured"].clone(),
            n["state"].clone(),
            n["boxes"].clone()
        ),
        (json!(false), json!("not-configured"), json!([]))
    );
    // Node's catalogue is not ready: the page still lists Drive and says why Nextcloud is missing.
    std::fs::remove_file(d.path().join("mcp-discovery.json")).unwrap();
    let (st, body) = get(a, CONNECTORS, MEMBER).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["connectors"][0]["id"], "gdrive");
    assert_eq!(
        (nc(&body)["state"].clone(), nc(&body)["tools"].clone()),
        (json!("error"), json!([]))
    );
    assert!(nc(&body)["message"]
        .as_str()
        .unwrap()
        .contains("being prepared"));
    // The route is for the signed-in; its other methods are not its own.
    let (st, _) = get(a, CONNECTORS, "").await;
    assert_eq!(st, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nextcloud_permissions_are_written_by_the_front_for_the_tools_node_offers_and_nothing_else()
{
    let (d, db, f) = rig().await;
    let a = f.addr;
    let put = |cookie: &'static str, body: &str| {
        let body = body.to_owned();
        async move { write(a, "PUT", POLICY, cookie, &body).await }
    };
    // Block a read and a write; a tool nobody offered is dropped, not stored.
    let (st, body) = put(MEMBER, r#"{"tools":["nc_notes_search_notes","nc_notes_create_note","drive_search_files","not_offered"],"mode":"block"}"#).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["id"], "nextcloud");
    assert_eq!(body["tools"][0]["mode"], "block");
    assert_eq!(body["tools"][1]["mode"], "block");
    let stored: Vec<(String, String, String)> = db
        .prepare("SELECT user_id, tool, mode FROM tool_policies ORDER BY tool")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        stored,
        [
            (B.to_owned(), "nc_notes_create_note".into(), "block".into()),
            (B.to_owned(), "nc_notes_search_notes".into(), "block".into())
        ]
    );
    assert_eq!(audits(&db, "tool.policy").len(), 1);
    // Allow is refused for a write (the write flags come from Node's catalogue); a read may be allowed.
    let (st, body) = put(
        MEMBER,
        r#"{"tools":["nc_notes_create_note"],"mode":"allow"}"#,
    )
    .await;
    assert_eq!(st, 400, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("write"),
        "{body}"
    );
    let (st, _) = put(
        MEMBER,
        r#"{"tools":["nc_notes_search_notes"],"mode":"allow"}"#,
    )
    .await;
    assert_eq!(st, 200);
    let mode: String = db
        .query_row(
            "SELECT mode FROM tool_policies WHERE user_id=?1 AND tool='nc_notes_search_notes'",
            [B],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(mode, "allow");
    // A mode that is not one, no tools, and a body that is not an object.
    for (body, st) in [
        (r#"{"tools":["nc_notes_search_notes"],"mode":"bogus"}"#, 400),
        (r#"{"tools":["not_offered"],"mode":"ask"}"#, 400),
        (r#"{"mode":"ask"}"#, 400),
        ("[1]", 400),
    ] {
        let (got, out) = put(MEMBER, body).await;
        assert_eq!(got, st, "{body}: {out}");
    }
    // The permission is the account's own: the administrator's is untouched.
    let admins: i64 = db
        .query_row(
            "SELECT count(*) FROM tool_policies WHERE user_id=?1",
            [A],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(admins, 0);
    // Other methods of the path are not this route's; the session gate comes first.
    let (st, _) = write(a, "PUT", POLICY, "", "{}").await;
    assert_eq!(st, 401);
    let (st, _) = raw(
        a,
        "PUT",
        POLICY,
        MEMBER,
        "",
        br#"{"tools":["nc_notes_search_notes"],"mode":"ask"}"#,
    )
    .await;
    assert_eq!(st, 403, "no CSRF header, no write");
    // Node's catalogue not ready: the tool names cannot be checked, so nothing is written.
    std::fs::remove_file(d.path().join("mcp-discovery.json")).unwrap();
    let before: i64 = db
        .query_row("SELECT count(*) FROM tool_policies", [], |r| r.get(0))
        .unwrap();
    let (st, body) = put(
        MEMBER,
        r#"{"tools":["nc_notes_search_notes"],"mode":"block"}"#,
    )
    .await;
    assert_eq!(st, 503, "{body}");
    let after: i64 = db
        .query_row("SELECT count(*) FROM tool_policies", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn a_device_token_may_read_the_list_but_not_change_a_permission() {
    // The front's gate (crates/server-auth): connector writes need a signed-in browser session, as core's device-auth did.
    assert!(server_auth::identity::browser_only_path(POLICY, "PUT"));
    assert!(!server_auth::identity::browser_only_path(CONNECTORS, "GET"));
    assert!(!server_auth::identity::browser_only_path(PERMITTED, "GET"));
}
