//! Private current Rust-router versus fixed Node-factory comparison. The reference
//! has no public headers or unauthorized/browser response; those are Rust source
//! contract assertions, never fabricated Node observations. No Node is invoked.
use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{HeaderMap, Request},
};
use serde_json::{json, Value};
use server_store::rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tower::ServiceExt;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const NOW: i64 = 1_800_000_000_000;
const ROOT: &str = "/private/tmp/m6-status-691-root-oracle-a";
const SCHEMA: &str = include_str!("../../../../crates/server-store/tests/fixtures/node-schema.sql");
const IDS: [&str; 7] = [
    "live-disconnected",
    "idle-upkeep",
    "unauthorized",
    "device-browser-only",
    "mixed-credential",
    "owner-isolated",
    "product-off-before-schema",
];
fn require(ok: bool, why: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(why.into())
    }
}
fn fixed_json(name: &str, sha: &str, cap: usize) -> Result<Value> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC)
                .bits() as i32,
        )
        .open(Path::new(ROOT).join(name))?;
    let meta = file.metadata()?;
    require(
        meta.is_file() && meta.nlink() == 1 && meta.len() <= cap as u64,
        "fixed reference regular singleton cap",
    )?;
    let mut bytes = Vec::new();
    file.take((cap + 1) as u64).read_to_end(&mut bytes)?;
    require(
        bytes.len() <= cap && format!("{:x}", Sha256::digest(&bytes)) == sha,
        "fixed reference complete hash",
    )?;
    Ok(serde_json::from_slice(&bytes)?)
}
fn keys(value: &Value, expected: &[&str]) -> Result<()> {
    let object = value.as_object().ok_or("reference object")?;
    require(
        object.len() == expected.len() && expected.iter().all(|k| object.contains_key(*k)),
        "exact reference object keys",
    )
}
fn sql_rows(value: &Value, key: &str, cap: usize) -> Result<()> {
    let rows = value.as_array().ok_or("reference SQL array")?;
    require(rows.len() <= cap, "SQL row cap")?;
    let mut last: Option<&str> = None;
    for row in rows {
        let time = if key == "id_hash" {
            "last_seen_at"
        } else {
            "last_used_at"
        };
        keys(row, &[key, time])?;
        let id = row
            .get(key)
            .and_then(Value::as_str)
            .ok_or("SQL identity string")?;
        require(
            !id.is_empty() && id.len() <= 64 && last.is_none_or(|previous| previous < id),
            "ordered unique SQL identities",
        )?;
        require(
            row.get(time).and_then(Value::as_i64).is_some(),
            "exact SQL integer timestamp",
        )?;
        last = Some(id);
    }
    Ok(())
}
fn reference() -> Result<Vec<Value>> {
    // Hash binding is finite accepted input identity, not a new ancestry/runtime proof.
    let _manifest = fixed_json(
        "qualification-manifest.json",
        "b4af8de5160df4146358d408a95acf30b910bc5de569544b372eb4631d465221",
        2 * 1024 * 1024,
    )?;
    let certificate = fixed_json(
        "capture-certificate.json",
        "245ac565799b233905fc316fc5ec90a48e5707dc03846e6105fec167e8f38d77",
        2 * 1024 * 1024,
    )?;
    let raw = fixed_json(
        "capture.stdout",
        "2a43c987ec6500361a859797527031be7be0dbe7332994a071ff587f322b739d",
        64 * 1024,
    )?;
    keys(&raw, &["node", "abi", "cases", "modules"])?;
    require(
        raw.get("node") == Some(&json!("v22.23.3")) && raw.get("abi") == Some(&json!("127")),
        "fixed Node dialect",
    )?;
    let rows = raw
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("reference cases")?;
    require(
        rows.len() == IDS.len() && certificate.get("cases") == Some(&Value::Array(rows.clone())),
        "seven certified current factory rows",
    )?;
    let modules = raw
        .get("modules")
        .and_then(Value::as_array)
        .ok_or("reference module inventory")?;
    require(
        modules.len() == 326
            && modules.iter().all(|m| {
                m.as_str()
                    .is_some_and(|s| s.len() <= 4096 && s.starts_with(&format!("{ROOT}/")))
            }),
        "finite one-origin loaded module inventory",
    )?;
    validate_rows(rows)?;
    Ok(rows.clone())
}
fn validate_rows(rows: &[Value]) -> Result<()> {
    require(rows.len() == IDS.len(), "seven reference rows")?;
    for (row, id) in rows.iter().zip(IDS) {
        keys(
            row,
            &[
                "id",
                "authenticated",
                "owner",
                "browser_only",
                "route",
                "sessions",
                "grants",
            ],
        )?;
        require(
            row.get("id").and_then(Value::as_str) == Some(id),
            "exact seven ordered ids",
        )?;
        let authenticated = !matches!(id, "idle-upkeep" | "unauthorized" | "mixed-credential");
        require(
            row.get("authenticated") == Some(&json!(authenticated))
                && row.get("owner")
                    == Some(&if authenticated {
                        json!("u")
                    } else {
                        Value::Null
                    })
                && row.get("browser_only") == Some(&json!(id == "device-browser-only")),
            "fixed factory auth outcomes",
        )?;
        sql_rows(
            row.get("sessions").ok_or("reference sessions")?,
            "id_hash",
            2,
        )?;
        sql_rows(row.get("grants").ok_or("reference grants")?, "id", 1)?;
        let route = row.get("route").ok_or("reference route")?;
        if matches!(
            id,
            "live-disconnected" | "owner-isolated" | "product-off-before-schema"
        ) {
            keys(route, &["status", "body"])?;
            require(
                route.get("status").and_then(Value::as_u64)
                    == Some(if id == "product-off-before-schema" {
                        404
                    } else {
                        200
                    })
                    && route
                        .get("body")
                        .and_then(Value::as_str)
                        .is_some_and(|b| b.len() <= 16 * 1024),
                "factory response bounds",
            )?;
        } else {
            require(route.is_null(), "missing public response stays absent")?;
        }
    }
    Ok(())
}
#[derive(Debug, PartialEq, Eq)]
struct Difference {
    case: String,
    field: &'static str,
}
fn compare(expected: &[Value], actual: &[Value]) -> Result<Vec<Difference>> {
    require(
        expected.len() == IDS.len() && actual.len() == IDS.len(),
        "comparison finite row count",
    )?;
    let mut differences = Vec::new();
    for ((left, right), id) in expected.iter().zip(actual).zip(IDS) {
        require(
            left.get("id").and_then(Value::as_str) == Some(id)
                && right.get("id").and_then(Value::as_str) == Some(id),
            "comparison ordered ids",
        )?;
        for value in [left, right] {
            sql_rows(
                value.get("sessions").ok_or("comparable sessions")?,
                "id_hash",
                2,
            )?;
            sql_rows(value.get("grants").ok_or("comparable grants")?, "id", 1)?;
        }
        for field in ["sessions", "grants", "route"] {
            let l = left.get(field).ok_or("expected comparable field")?;
            let r = right.get(field).ok_or("actual comparable field")?;
            if l != r {
                differences.push(Difference {
                    case: id.into(),
                    field: if field == "grants"
                        && l.as_array().zip(r.as_array()).is_some_and(|(a, b)| {
                            a.len() == 1
                                && b.len() == 1
                                && a.first().and_then(|v| v.get("id"))
                                    == b.first().and_then(|v| v.get("id"))
                        }) {
                        "grants.last_used_at"
                    } else {
                        field
                    },
                });
            }
        }
    }
    require(differences.len() <= 21, "comparison finite difference cap")?;
    Ok(differences)
}
#[test]
fn current_factory_comparator_rejects_grant_timestamp_delta() -> Result<()> {
    grant_negative(reference()?)
}
fn grant_negative(expected: Vec<Value>) -> Result<()> {
    let mut changed = expected.clone();
    let row = changed
        .iter_mut()
        .find(|row| row.get("id").and_then(Value::as_str) == Some("device-browser-only"))
        .ok_or("fixed negative case")?;
    let grant = row
        .get_mut("grants")
        .and_then(Value::as_array_mut)
        .and_then(|rows| rows.first_mut())
        .ok_or("fixed negative grant")?;
    let timestamp = grant
        .get_mut("last_used_at")
        .ok_or("fixed negative column")?;
    require(
        timestamp.as_i64() == Some(NOW),
        "accepted negative input timestamp",
    )?;
    *timestamp = json!(NOW + 1);
    let differences = compare(&expected, &changed)?;
    assert_eq!(
        differences,
        vec![Difference {
            case: "device-browser-only".into(),
            field: "grants.last_used_at"
        }],
        "M6_CURRENT_DIFF_ASSERT/semantic-grant-timestamp-negative"
    );
    Ok(())
}
fn setup(db: &Connection) -> Result<()> {
    db.busy_timeout(Duration::from_millis(200))?;
    db.execute_batch(SCHEMA)?;
    db.execute_batch("CREATE TABLE chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY,data_enc TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'connected',updated_at INTEGER NOT NULL);")?;
    let live = server_auth::js::digest("live");
    let idle = server_auth::js::digest("idle");
    db.execute_batch(&format!("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u','Alpha','alpha','Alpha','admin','x','w',1,1),('other','Other','other','Other','member','x','z',2,2); INSERT INTO sessions VALUES('{live}','u','c',1,{}, {},'ua','ip'),('{idle}','u','c',1,{}, {},'ua','ip');", NOW-1000, NOW+99999, NOW-604800001, NOW+99999))?;
    Ok(())
}
fn case_sql(db: &Connection, id: &str) -> Result<()> {
    match id {
        "live-disconnected" => db.execute_batch(&format!("INSERT INTO device_grants VALUES('g','u','Mac',1,{}, {},'ip','ua'); INSERT INTO device_tokens VALUES('{}','g','access',1,{},NULL,NULL); INSERT INTO settings VALUES('feature:chatgptOAuth','true');",NOW-60000,NOW+99999,server_auth::js::digest("nva_device"),NOW+99999))?,
        "owner-isolated" => db.execute_batch("INSERT INTO chatgpt_oauth_tokens VALUES('u','{\"email\":\"alice@example.invalid\",\"plan\":\"synthetic-alpha\"}','connected',1),('other','{\"email\":\"other@example.invalid\",\"plan\":\"synthetic-other\"}','connected',1);")?,
        "product-off-before-schema" => db.execute_batch("UPDATE settings SET value='false' WHERE key='feature:chatgptOAuth'; DROP TABLE chatgpt_oauth_tokens;")?,
        "idle-upkeep" | "unauthorized" | "device-browser-only" | "mixed-credential" => {},
        _ => return Err("fixed synthetic case only".into()),
    }
    Ok(())
}
fn observe_sql(db: &Connection, id: &str, route: Value) -> Result<Value> {
    let mut statement =
        db.prepare("SELECT id_hash,last_seen_at FROM sessions ORDER BY id_hash LIMIT 3")?;
    let sessions = statement
        .query_map([], |row| {
            Ok(json!({"id_hash":row.get::<_,String>(0)?,"last_seen_at":row.get::<_,i64>(1)?}))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut statement =
        db.prepare("SELECT id,last_used_at FROM device_grants ORDER BY id LIMIT 2")?;
    let grants = statement
        .query_map([], |row| {
            Ok(json!({"id":row.get::<_,String>(0)?,"last_used_at":row.get::<_,i64>(1)?}))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    require(
        sessions.len() <= 2 && grants.len() <= 1,
        "observed SQL finite row cap",
    )?;
    Ok(json!({"id":id,"route":route,"sessions":sessions,"grants":grants}))
}
fn request(id: &str) -> Result<Request<Body>> {
    let mut builder = Request::builder()
        .uri("/api/providers/chatgpt")
        .method("GET");
    match id {
        "live-disconnected" | "owner-isolated" | "product-off-before-schema" => {
            builder = builder.header("cookie", "cowork_session=live")
        }
        "idle-upkeep" => builder = builder.header("cookie", "cowork_session=idle"),
        "device-browser-only" => builder = builder.header("authorization", "Bearer nva_device"),
        "mixed-credential" => {
            builder = builder
                .header("authorization", "Bearer nva_device")
                .header("cookie", "cowork_session=live")
        }
        "unauthorized" => {}
        _ => return Err("fixed request case".into()),
    }
    let mut request = builder.body(Body::empty())?;
    request
        .extensions_mut()
        .insert(ConnectInfo(crate::serve::Conn {
            peer: "127.0.0.1:1".parse()?,
            local: Some("127.0.0.1:1".parse()?),
        }));
    request
        .extensions_mut()
        .insert(crate::serve::Arrival(Instant::now()));
    Ok(request)
}
fn assert_rust_headers(headers: &HeaderMap, unauthorized: bool, body_bytes: usize) -> Result<()> {
    require(
        headers.len() == if unauthorized { 9 } else { 8 },
        "exact Rust application header count",
    )?;
    let mut bytes = 0usize;
    for (name, value) in headers {
        bytes = bytes
            .checked_add(name.as_str().len() + value.as_bytes().len())
            .ok_or("header total overflow")?;
    }
    require(bytes <= 4096, "response header bytes cap")?;
    for (name, value) in [
        ("content-type", "application/json"),
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "same-origin"),
        ("x-frame-options", "DENY"),
        ("content-security-policy", crate::reply::CSP),
        ("x-noevia-api", "1"),
    ] {
        require(
            headers.get_all(name).iter().count() == 1
                && headers
                    .get(name)
                    .is_some_and(|v| v.as_bytes() == value.as_bytes()),
            "Rust source-contract header assertion",
        )?;
    }
    require(
        headers.get_all("content-length").iter().count() == 1
            && headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == body_bytes.to_string()),
        "actual router content-length matches complete bounded body",
    )?;
    require(
        headers.get("www-authenticate").map(|v| v.as_bytes())
            == if unauthorized {
                Some(b"Bearer realm=\"cowork\"".as_slice())
            } else {
                None
            },
        "Rust source-contract challenge assertion",
    )?;
    Ok(())
}
async fn wait_idle(owner: &crate::chatgpt_status_routes::StatusOwner) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while owner.active_jobs() != Some(0) {
        require(Instant::now() < deadline, "retained owner idle deadline")?;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_factory_seven_rows_through_rust_router() -> Result<()> {
    router_positive(reference()?).await
}
async fn router_positive(expected: Vec<Value>) -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("secrets.key"), [7; 32])?;
    let db = Connection::open(server_store::db_path(directory.path()))?;
    setup(&db)?;
    let config = crate::config::Config::from_lookup(|key| match key {
        "NOEVIA_RUST_AUTH" | "NOEVIA_RUST_CHATGPT_STATUS" => Some("1".into()),
        "UI_DATA_DIR" => directory.path().to_str().map(str::to_owned),
        "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:1".into()),
        "NOEVIA_FEATURE_NATIVE_CLIENT_AUTH" | "TRUST_PROXY" => Some("true".into()),
        _ => None,
    })?;
    // No CodeNet target is configured: retained resolver is Disabled, no DNS/helper.
    let mut app = crate::App::new(config);
    // Every fallible operation after App construction is inside this result so
    // failed admission/comparison still reaches actual controller shutdown.
    let result: Result<()> = async {
        Arc::get_mut(&mut app).ok_or("exclusive private App before publication")?.status_test_now = Some(NOW);
        let owner = app.chatgpt_status.as_ref().and_then(|o| o.as_ref().ok()).cloned().ok_or("actual retained status owner")?;
        let deadline = Instant::now()+Duration::from_secs(3);
        while !owner.ready() {
            require(Instant::now() < deadline, "retained owner startup deadline")?;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let router = crate::router(app.clone());
        let mut actual = Vec::new();
        let mut observations = Vec::new();
        for id in IDS {
            wait_idle(&owner).await?;
            case_sql(&db,id)?;
            let response = tokio::time::timeout(Duration::from_secs(3),router.clone().oneshot(request(id)?)).await??;
            let status = response.status().as_u16();
            let unauthorized = matches!(id,"idle-upkeep"|"unauthorized"|"mixed-credential");
            let headers = response.headers().clone();
            let body = String::from_utf8(tokio::time::timeout(Duration::from_secs(1),to_bytes(response.into_body(),16*1024)).await??.to_vec())?;
            assert_rust_headers(&headers, unauthorized, body.len())?;
            observations.push(json!({"id":id,"status":status,"body":body,"headers":headers.iter().map(|(name,value)| (name.as_str().to_owned(),String::from_utf8_lossy(value.as_bytes()).into_owned())).collect::<Vec<_>>() }));
            let route = if unauthorized {
                require(status == 401 && body == r#"{"error":"unauthorized"}"#, "Rust source-contract unauthorized response, absent Node public evidence")?;
                Value::Null
            } else if id == "device-browser-only" {
                require(status == 403 && body == r#"{"error":"This needs a signed-in browser session.","code":"browser_session_required"}"#, "Rust source-contract browser-only response, absent Node public evidence")?;
                Value::Null
            } else { json!({"status":status,"body":body}) };
            wait_idle(&owner).await?;
            actual.push(observe_sql(&db,id,route)?);
        }
        println!("M6_CURRENT_RUST_HTTP_OBSERVATIONS {}", serde_json::to_string(&observations)?);
        println!("M6_CURRENT_FACTORY_ROWS {}",serde_json::to_string(&actual)?);
        let differences = compare(&expected,&actual)?;
        if !differences.is_empty() { return Err(format!("M6_CURRENT_DIFF_ASSERT/seven-factory-rows {differences:?}").into()); }
        Ok(())
    }.await;
    if let Some(owner) = app.chatgpt_status.as_ref().and_then(|o| o.as_ref().ok()) {
        owner.request_shutdown();
    }
    let joined = tokio::task::spawn_blocking(crate::chatgpt_status_routes::shutdown_process_owners);
    // No timeout-drop of this actual controller JoinHandle: await real completion.
    let healthy = joined.await?;
    drop(app);
    db.close().map_err(|(_, e)| e)?;
    require(healthy, "actual retained owner teardown healthy")?;
    result
}

fn portable_field<'a>(value: &'a Value, key: &str) -> &'a Value {
    match value.get(key) {
        Some(value) => value,
        None => &Value::Null,
    }
}
// Only the two portable test entry points read this cwd-owned binding. Production
// code, clocks and historical Darwin reference remain independent of it.
fn portable_read(path: &Path, cap: usize) -> Result<Vec<u8>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC)
                .bits() as i32,
        )
        .open(path)?;
    let meta = file.metadata()?;
    require(
        meta.is_file()
            && meta.nlink() == 1
            && meta.mode() & 0o777 == 0o600
            && meta.len() <= cap as u64,
        "portable private singleton cap/mode",
    )?;
    let mut bytes = Vec::new();
    file.take((cap + 1) as u64).read_to_end(&mut bytes)?;
    require(bytes.len() <= cap, "portable complete read cap")?;
    Ok(bytes)
}
fn portable_json(
    root: &Path,
    name: &str,
    binding: &Value,
    field: &str,
    cap: usize,
) -> Result<Value> {
    let bytes = portable_read(&root.join(name), cap)?;
    require(
        binding.get(field).and_then(Value::as_str)
            == Some(format!("{:x}", Sha256::digest(&bytes)).as_str()),
        "portable reference complete hash",
    )?;
    Ok(serde_json::from_slice(&bytes)?)
}
fn linux_reference() -> Result<Vec<Value>> {
    let cwd = fs::canonicalize(std::env::current_dir()?)?;
    require(
        fs::symlink_metadata(&cwd)?.mode() & 0o777 == 0o700,
        "private qualification cwd",
    )?;
    let binding: Value = serde_json::from_slice(&portable_read(
        &cwd.join("m6-ci-reference-binding.json"),
        8192,
    )?)?;
    keys(
        &binding,
        &[
            "version",
            "root",
            "capture_sha256",
            "manifest_sha256",
            "certificate_sha256",
            "core_commit",
            "core_tree",
            "now",
            "input_sha256",
            "adapter_sha256",
            "schema_sha256",
            "ids",
            "runtime",
        ],
    )?;
    require(
        (*portable_field(&binding, "version")) == 1
            && (*portable_field(&binding, "core_commit"))
                == "010735c7d86d30269ab3f4a351b2135faea1c8e7"
            && (*portable_field(&binding, "core_tree"))
                == "345adfa12da083eb5888ec232dc2d86b9b0383a5"
            && (*portable_field(&binding, "now")) == NOW
            && (*portable_field(&binding, "ids")) == json!(IDS)
            && (*portable_field(&binding, "schema_sha256"))
                == format!("{:x}", Sha256::digest(SCHEMA.as_bytes())),
        "portable fixed baseline/clock/schema/ids",
    )?;
    let root = cwd.join("oracle");
    require(
        fs::canonicalize(&root)? == root
            && (*portable_field(&binding, "root")) == root.to_str().ok_or("portable root utf8")?,
        "canonical owned oracle root",
    )?;
    let manifest = portable_json(
        &root,
        "qualification-manifest.json",
        &binding,
        "manifest_sha256",
        2 * 1024 * 1024,
    )?;
    let certificate = portable_json(
        &root,
        "capture-certificate.json",
        &binding,
        "certificate_sha256",
        2 * 1024 * 1024,
    )?;
    let raw = portable_json(
        &root,
        "capture.stdout",
        &binding,
        "capture_sha256",
        1024 * 1024,
    )?;
    let input = portable_json(
        &root,
        "qualification-input.json",
        &binding,
        "input_sha256",
        8192,
    )?;
    require(
        (*portable_field(&manifest, "core_commit")) == (*portable_field(&binding, "core_commit"))
            && (*portable_field(&manifest, "core_tree"))
                == (*portable_field(&binding, "core_tree"))
            && (*portable_field(&manifest, "canonical_output"))
                == (*portable_field(&binding, "root"))
            && (*portable_field(&manifest, "runtime")) == (*portable_field(&binding, "runtime"))
            && *portable_field(portable_field(&manifest, "runtime"), "platform") == "linux"
            && *portable_field(portable_field(&manifest, "runtime"), "version") == "v22.23.3"
            && *portable_field(portable_field(&manifest, "runtime"), "abi") == "127"
            && (*portable_field(&manifest, "input_sha256"))
                == (*portable_field(&binding, "input_sha256"))
            && (*portable_field(&manifest, "adapter_sha256"))
                == (*portable_field(&binding, "adapter_sha256"))
            && (*portable_field(&input, "now")) == NOW
            && (*portable_field(&input, "schema_sql")) == SCHEMA
            && (*portable_field(&input, "schema_sha256"))
                == (*portable_field(&binding, "schema_sha256"))
            && (*portable_field(&certificate, "manifest")) == manifest
            && (*portable_field(&certificate, "stdout_sha256"))
                == (*portable_field(&binding, "capture_sha256"))
            && (*portable_field(&certificate, "stderr_sha256"))
                == format!("{:x}", Sha256::digest(b""))
            && (*portable_field(&certificate, "raw_exit_code")) == 0,
        "portable fresh capture provenance",
    )?;
    keys(&raw, &["node", "abi", "cases", "modules"])?;
    require(
        (*portable_field(&raw, "node")) == "v22.23.3"
            && (*portable_field(&raw, "abi")) == "127"
            && (*portable_field(&certificate, "cases")) == (*portable_field(&raw, "cases")),
        "portable raw dialect/cases",
    )?;
    let rows = (*portable_field(&raw, "cases"))
        .as_array()
        .ok_or("portable cases")?;
    validate_rows(rows)?;
    let modules = (*portable_field(&raw, "modules"))
        .as_array()
        .ok_or("portable modules")?;
    let files = (*portable_field(&manifest, "files"))
        .as_array()
        .ok_or("portable source files")?;
    let loaded = (*portable_field(&certificate, "loaded_files"))
        .as_array()
        .ok_or("portable loaded binding")?;
    require(
        !modules.is_empty()
            && modules.len() <= 512
            && loaded.len() == modules.len()
            && files.len() <= 5120,
        "portable finite inventories",
    )?;
    let mut unique = std::collections::BTreeSet::new();
    for (module, certified) in modules.iter().zip(loaded) {
        let name = module.as_str().ok_or("portable loaded path")?;
        require(
            name.len() <= 4096 && unique.insert(name),
            "portable unique loaded path",
        )?;
        let path = Path::new(name);
        require(
            fs::canonicalize(path)? == path,
            "portable canonical loaded file",
        )?;
        let relative = path
            .strip_prefix(&root)?
            .to_str()
            .ok_or("portable loaded utf8")?;
        let file = files
            .iter()
            .find(|f| (*portable_field(f, "path")) == relative)
            .ok_or("portable loaded source identity")?;
        require(
            file == certified && (*portable_field(file, "destination_mode")) == "100600",
            "portable certified loaded source",
        )?;
        let bytes = portable_read(path, 16 * 1024 * 1024)?;
        require(
            (*portable_field(file, "sha256")) == format!("{:x}", Sha256::digest(&bytes))
                && (*portable_field(file, "bytes")) == bytes.len(),
            "portable loaded bytes identity",
        )?;
    }
    Ok(rows.clone())
}
#[test]
fn linux_factory_comparator_rejects_grant_timestamp_delta() -> Result<()> {
    grant_negative(linux_reference()?)?;
    println!("M6_LINUX_NEGATIVE grants.last_used_at +1 rejected");
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn linux_factory_seven_rows_through_rust_router() -> Result<()> {
    router_positive(linux_reference()?).await
}
