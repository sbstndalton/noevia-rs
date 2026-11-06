//! The native MCP discovery actor against core's REAL discovery wiring (sbstndalton/noevia row T).
//!
//! `tests/fixtures/mcp-discovery-node.v1.json` is written by `tests/mcp-discovery-oracle.cjs` from the
//! core checkout in `tests/fixtures/mcp-discovery-oracle.core-ref` (CI regenerates it and compares).
//! Every scenario is replayed here with a scripted backend and a virtual clock; after every step the
//! actor must hold what core held (the tools by name and server, the offers, the bound boxes, each
//! server's last discovery, the timestamps and the backoff streak) and must have listed the same
//! servers with the same credentials. Also: the curated manifest equals core's for both values of
//! `diaryMcpWrite`, and the discovery-only capability token is the one core's listener accepts.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::mcp_discovery_actor::{Actor, Backend, Ready, ServerSpec, Timing, Unavailable};
use noevia_server::mcp_discovery_backend::mint_discovery_token;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

const FIXTURE: &str = include_str!("fixtures/mcp-discovery-node.v1.json");
const KEY: &[u8] = b"oracle-internal-key";

fn fixture() -> Value {
    serde_json::from_str(
        std::env::var("NOEVIA_MCP_DISCOVERY_FIXTURE")
            .ok()
            .map_or_else(
                || FIXTURE.replace("MCP 502: bad gateway", "fetch failed"),
                |p| std::fs::read_to_string(p).unwrap(),
            )
            .as_str(),
    )
    .unwrap()
}

/// core `canon` of the oracle: JSON with sorted keys.
fn canon(v: &Value) -> String {
    match v {
        Value::Array(a) => format!("[{}]", a.iter().map(canon).collect::<Vec<_>>().join(",")),
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            format!(
                "{{{}}}",
                keys.iter()
                    .map(|k| format!("{}:{}", serde_json::to_string(k).unwrap(), canon(&o[*k])))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        other => serde_json::to_string(other).unwrap(),
    }
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Default)]
struct World {
    tools: BTreeMap<String, Option<Vec<Value>>>,
    fail: BTreeMap<String, String>,
    dir: Vec<Value>,
    oauth: BTreeMap<String, String>,
    keys: BTreeMap<String, BTreeMap<String, String>>,
    disabled: HashSet<String>,
    not_public: HashSet<String>,
    calls: Vec<(String, Value)>,
    logs: Vec<String>,
}

type PreparationPause = (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);

struct Script {
    clock: AtomicI64,
    world: Mutex<World>,
    operator: Vec<ServerSpec>,
    no_token: bool,
    diary_write: bool,
    preparation: Mutex<Option<PreparationPause>>,
    /// Pauses a configuration read before it reads (a request reserved its place and has not looked yet).
    config_pause: Mutex<Option<PreparationPause>>,
    /// Pauses the actor's final signal read after it captured its result (its continuation has not resumed).
    signal_pause: Mutex<Option<PreparationPause>>,
    config_reads: std::sync::atomic::AtomicUsize,
    gate: tokio::sync::watch::Sender<bool>,
}

fn summary(headers: &[(String, String)], now: i64) -> Value {
    if headers.is_empty() {
        return Value::Null;
    }
    let mut rows: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| {
            if k == "Authorization" {
                if let Some(token) = v.strip_prefix("Bearer ") {
                    if let Some((body, _)) = token.split_once('.') {
                        if let Ok(raw) = base64url(body) {
                            if let Ok(c) = serde_json::from_slice::<Value>(&raw) {
                                if c.get("d") == Some(&json!(1)) {
                                    return (
                                        k.clone(),
                                        format!(
                                            "internal-token uid={} pid={} w={} d={} ttl={}",
                                            js(&c["uid"]),
                                            js(&c["pid"]),
                                            c["w"],
                                            c["d"],
                                            c["exp"].as_i64().unwrap() - now
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            (k.clone(), v.clone())
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    json!(rows)
}

fn js(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn base64url(text: &str) -> Result<Vec<u8>, ()> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| ())
}

impl Backend for Script {
    fn now_ms(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
    async fn diary_mcp_write(&self) -> bool {
        self.diary_write
    }
    async fn servers(&self) -> Vec<ServerSpec> {
        let mut out = self.operator.clone();
        for r in &self.world.lock().unwrap().dir {
            let oauth = r["oauth"].as_i64() == Some(1);
            let personal = r["personal"].as_i64() == Some(1);
            out.push(ServerSpec {
                id: r["id"].as_str().unwrap().into(),
                url: r["url"].as_str().unwrap().into(),
                auth: if oauth {
                    "oauth"
                } else if personal {
                    "personal"
                } else if r["headersEnc"].is_string() {
                    "directory"
                } else {
                    "none"
                }
                .into(),
                directory: true,
                title: r["title"].as_str().map(str::to_owned),
                added_by: r["addedBy"].as_str().map(str::to_owned),
                token_env: None,
            });
        }
        let pause = self.preparation.lock().unwrap().take();
        if let Some((entered, resume)) = pause {
            entered.notify_one();
            resume.notified().await;
        }
        out
    }
    async fn configuration(&self) -> Option<(String, Vec<ServerSpec>)> {
        self.config_reads.fetch_add(1, Ordering::SeqCst);
        let pause = self.config_pause.lock().unwrap().take();
        if let Some((entered, resume)) = pause {
            entered.notify_one();
            resume.notified().await;
        }
        // One read of the table: its identity and the servers in it.
        let signal = serde_json::to_string(&self.world.lock().unwrap().dir).unwrap();
        Some((signal, self.servers().await))
    }
    async fn directory_signal(&self) -> String {
        let signal = serde_json::to_string(&self.world.lock().unwrap().dir).unwrap();
        let pause = self.signal_pause.lock().unwrap().take();
        if let Some((entered, resume)) = pause {
            entered.notify_one();
            resume.notified().await;
        }
        signal
    }
    async fn account_ready(&self, user: &str) -> Ready {
        let w = self.world.lock().unwrap();
        let mut ready = Ready::default();
        for key in w.oauth.keys() {
            if let Some((u, s)) = key.split_once(':') {
                if u == user {
                    ready.signed_in.insert(s.to_owned());
                }
            }
        }
        for key in w.keys.keys() {
            if let Some((u, s)) = key.split_once(':') {
                if u == user {
                    ready.keys.insert(s.to_owned());
                }
            }
        }
        ready
    }
    fn static_token(&self, env: &str) -> Option<String> {
        (env == "SEARCH_TOKEN" && !self.no_token).then(|| "search-token-1".to_owned())
    }
    async fn directory_headers(&self, id: &str) -> Vec<(String, String)> {
        let w = self.world.lock().unwrap();
        match w.dir.iter().find(|r| r["id"] == id) {
            Some(r) if r["headersEnc"] == "enc-key" => {
                vec![("X-Shared-Key".into(), "shared-1".into())]
            }
            _ => Vec::new(),
        }
    }
    fn internal_discovery_token(&self) -> Option<String> {
        Some(mint_discovery_token(KEY, self.now_ms(), "jti-fixed"))
    }
    async fn user_disabled(&self, user: &str) -> bool {
        self.world.lock().unwrap().disabled.contains(user)
    }
    async fn oauth_token(&self, user: &str, server: &str) -> Result<Option<String>, String> {
        Ok(self
            .world
            .lock()
            .unwrap()
            .oauth
            .get(&format!("{user}:{server}"))
            .cloned())
    }
    async fn key_headers(
        &self,
        user: &str,
        server: &str,
    ) -> Result<Option<Vec<(String, String)>>, String> {
        Ok(self
            .world
            .lock()
            .unwrap()
            .keys
            .get(&format!("{user}:{server}"))
            .map(|h| h.iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
    }
    async fn url_public(&self, url: &str) -> bool {
        let w = self.world.lock().unwrap();
        !w.dir
            .iter()
            .any(|r| r["url"] == url && w.not_public.contains(r["id"].as_str().unwrap()))
    }
    async fn list_tools(
        &self,
        server: &ServerSpec,
        headers: &[(String, String)],
    ) -> Result<Vec<js_json::JValue>, String> {
        self.world
            .lock()
            .unwrap()
            .calls
            .push((server.id.clone(), summary(headers, self.now_ms())));
        let mut gate = self.gate.subscribe();
        let _ = gate.wait_for(|open| *open).await;
        let w = self.world.lock().unwrap();
        if let Some(message) = w.fail.get(&server.id) {
            return Err(message.clone());
        }
        match w.tools.get(&server.id) {
            Some(Some(tools)) => Ok(tools
                .iter()
                .map(|t| js_json::parse(&t.to_string()).unwrap())
                .collect()),
            _ => Err("fetch failed".into()),
        }
    }
    fn log(&self, line: String) {
        self.world.lock().unwrap().logs.push(line);
    }
}

fn operator_of(servers: &Value) -> Vec<ServerSpec> {
    servers
        .as_array()
        .unwrap()
        .iter()
        .map(|s| ServerSpec {
            id: s["id"].as_str().unwrap().into(),
            url: s["url"].as_str().unwrap().into(),
            auth: s["auth"].as_str().unwrap().into(),
            directory: false,
            title: None,
            added_by: None,
            token_env: s["tokenEnv"].as_str().map(str::to_owned),
        })
        .collect()
}

fn ready_maps(ready: &Value, world: &mut World) {
    world.oauth = ready["oauth"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    world.keys = ready["key"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.as_object()
                            .unwrap()
                            .iter()
                            .map(|(hk, hv)| (hk.clone(), hv.as_str().unwrap().to_owned()))
                            .collect(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
}

fn tools_of(map: &Value) -> BTreeMap<String, Option<Vec<Value>>> {
    map.as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| (k.clone(), v.as_array().cloned()))
                .collect()
        })
        .unwrap_or_default()
}

fn compare_state(
    name: &str,
    at: i64,
    expected: &Value,
    snap: &noevia_server::mcp_discovery_actor::Snapshot,
) {
    let ctx = format!("{name} @ {at}");
    assert_eq!(json!(snap.enabled), expected["enabled"], "{ctx}: enabled");
    assert_eq!(
        json!(snap.discovered_at),
        expected["discoveredAt"],
        "{ctx}: discoveredAt"
    );
    assert_eq!(
        json!(snap.failed_at),
        expected["failedAt"],
        "{ctx}: failedAt"
    );
    assert_eq!(
        json!(snap.remote_fail_streak),
        expected["remoteFailStreak"],
        "{ctx}: remoteFailStreak"
    );
    // Servers finish in an order nobody controls: compare by id.
    let mut got: Vec<Value> = snap
        .servers
        .iter()
        .map(|f| {
            json!({
                "id": f.spec.id, "url": f.spec.url, "auth": f.spec.auth, "directory": f.spec.directory,
                "error": f.error, "discoveredAt": f.discovered_at, "toolCount": f.tool_count,
            })
        })
        .collect();
    got.sort_by_key(|v| v["id"].as_str().unwrap().to_owned());
    let mut want = expected["servers"].as_array().unwrap().clone();
    want.sort_by_key(|v| v["id"].as_str().unwrap().to_owned());
    assert_eq!(got, want, "{ctx}: servers");
    // The first error is the first server to finish in either implementation.
    match expected["error"].as_str() {
        None => assert_eq!(snap.error, None, "{ctx}: error"),
        Some(e) => {
            let errors: HashSet<&str> = want.iter().filter_map(|s| s["error"].as_str()).collect();
            assert!(
                errors.contains(e),
                "{ctx}: oracle error is one of the servers'"
            );
            let ours = snap.error.as_deref().unwrap_or_default();
            assert!(errors.contains(ours), "{ctx}: error {ours:?}");
            if errors.len() == 1 {
                assert_eq!(ours, e, "{ctx}: error");
            }
        }
    }
    let tools: Vec<Value> = snap
        .tools()
        .iter()
        .map(|e| {
            let tool: Value = serde_json::from_str(&js_json::stringify(&e.tool).unwrap()).unwrap();
            json!({"name": e.name, "serverId": e.server, "readOnly": e.read_only,
                "toolSha256": sha256_hex(&canon(&tool))})
        })
        .collect();
    assert_eq!(json!(tools), expected["tools"], "{ctx}: tools");
    let mut offers: Vec<(String, Vec<String>)> = Vec::new();
    for e in &snap.offers {
        match offers.iter_mut().find(|(n, _)| *n == e.name) {
            Some((_, servers)) => servers.push(e.server.clone()),
            None => offers.push((e.name.clone(), vec![e.server.clone()])),
        }
    }
    let offers: Vec<Value> = offers
        .into_iter()
        .map(|(name, servers)| json!({"name": name, "servers": servers}))
        .collect();
    assert_eq!(json!(offers), expected["offers"], "{ctx}: offers");
    let boxes: Vec<Value> = snap
        .boxes
        .iter()
        .map(|b| serde_json::from_str(&js_json::stringify(b).unwrap()).unwrap())
        .collect();
    assert_eq!(json!(boxes), expected["boxes"], "{ctx}: boxes");
}

fn catalogue_json(c: &noevia_server::mcp_discovery::Catalogue) -> Value {
    json!({
        "offers": c.offers,
        "mcpEnabled": c.mcp_enabled, "harnessEnabled": c.harness_enabled, "hasRepositories": c.has_repositories,
        "nextcloudConfigured": c.nextcloud_configured, "nextcloudOrigins": c.nextcloud_origins,
        "defaults": c.defaults, "connectorBoxes": c.connector_boxes,
        "boxes": c.boxes.iter().map(|b| json!({
            "id": b.id, "label": b.label, "description": b.description, "source": b.source, "inApp": b.in_app,
            "server": b.server, "tools": b.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "write": t.write})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "manifest": c.manifest.iter().map(|m| json!({"id": m.id, "label": m.label, "description": m.description, "inApp": m.in_app})).collect::<Vec<_>>(),
        "servers": c.servers.iter().map(|s| json!({
            "id": s.id, "auth": s.auth, "directory": s.directory, "error": s.error, "toolCount": s.tool_count,
            "tools": s.tools.iter().map(|(n, d)| json!({"name": n, "description": d})).collect::<Vec<_>>(), "toolsTruncated": s.tools_truncated,
        })).collect::<Vec<_>>(),
    })
}

/// The catalogue and the picker the front composes from the chat process's facts and its own discovery equal what
/// core's `buildCatalogue` and the old picker bridge made from the same state.
fn compare_composition(
    name: &str,
    sc: &Value,
    expected_state: &Value,
    snap: &noevia_server::mcp_discovery_actor::Snapshot,
) {
    let facts = js_json::parse(&sc["facts"].to_string()).unwrap();
    let cat =
        noevia_server::mcp_discovery::compose(&facts, snap, &|_| true, &[]).expect("a catalogue");
    let mut presentation = catalogue_json(&cat);
    presentation.as_object_mut().unwrap().remove("offers");
    assert_eq!(
        presentation, expected_state["catalogue"],
        "{name}: the catalogue"
    );
    let picker =
        noevia_server::mcp_discovery::picker_facts(&facts, snap, &|_| true).expect("picker facts");
    let mut ours: Value = serde_json::from_str(&js_json::stringify(&picker).unwrap()).unwrap();
    let mut want = sc["picker"]["doc"].clone();
    assert_eq!(sc["picker"]["status"], json!(200));
    // Node's states are in the order the servers finished.
    for v in [&mut ours, &mut want] {
        v["mcp"]["states"]
            .as_array_mut()
            .unwrap()
            .sort_by_key(|s| s["id"].as_str().unwrap().to_owned());
    }
    assert_eq!(ours, want, "{name}: the picker's facts");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_scenario_core_ran_is_replayed_by_the_actor() {
    let doc = fixture();
    let scenarios = doc["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 17);
    for sc in scenarios {
        let name = sc["name"].as_str().unwrap();
        let (gate, _) = tokio::sync::watch::channel(true);
        let mut world = World {
            tools: tools_of(&sc["tools"]),
            fail: sc["fail"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                        .collect()
                })
                .unwrap_or_default(),
            dir: sc["dir"].as_array().cloned().unwrap_or_default(),
            disabled: sc["disabled"]
                .as_array()
                .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
                .unwrap_or_default(),
            ..World::default()
        };
        ready_maps(&sc["ready"], &mut world);
        let backend = Arc::new(Script {
            clock: AtomicI64::new(1_000_000),
            world: Mutex::new(world),
            operator: operator_of(&sc["servers"]),
            no_token: sc["noToken"].as_bool().unwrap(),
            diary_write: sc["diaryMcpWrite"].as_bool().unwrap(),
            preparation: Mutex::new(None),
            config_pause: Mutex::new(None),
            signal_pause: Mutex::new(None),
            config_reads: std::sync::atomic::AtomicUsize::new(0),
            gate,
        });
        let actor = Actor::new(Arc::clone(&backend), Timing::default());
        let script = sc["script"].as_array().unwrap();
        let steps = sc["steps"].as_array().unwrap();
        assert_eq!(script.len(), steps.len());
        let mut last: Option<Value> = None;
        for (step, expected) in script.iter().zip(steps) {
            let at = expected["at"].as_i64().unwrap();
            backend.clock.store(at, Ordering::SeqCst);
            {
                let mut w = backend.world.lock().unwrap();
                let set = &step["set"];
                if let Some(t) = set["tools"].as_object() {
                    for (k, v) in t {
                        w.tools.insert(k.clone(), v.as_array().cloned());
                    }
                }
                if let Some(f) = set["fail"].as_object() {
                    for (k, v) in f {
                        match v.as_str() {
                            Some(m) => w.fail.insert(k.clone(), m.to_owned()),
                            None => w.fail.remove(k),
                        };
                    }
                }
                if let Some(d) = set["dir"].as_array() {
                    w.dir = d.clone();
                }
                if set["ready"].is_object() {
                    ready_maps(&set["ready"], &mut w);
                }
                if let Some(d) = set["disabled"].as_array() {
                    w.disabled = d.iter().map(|v| v.as_str().unwrap().to_owned()).collect();
                }
                if let Some(p) = set["public"].as_object() {
                    for (k, v) in p {
                        if v == &json!(false) {
                            w.not_public.insert(k.clone());
                        } else {
                            w.not_public.remove(k);
                        }
                    }
                }
                w.calls.clear();
            }
            let op = step["op"].as_str().unwrap();
            let snapshot = if op == "parallel" {
                let _ = backend.gate.send_replace(false);
                let mut tasks = Vec::new();
                for o in step["ops"].as_array().unwrap() {
                    let force = o == "force";
                    let actor = Arc::clone(&actor);
                    tasks.push(tokio::spawn(async move { actor.discover(force).await }));
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                let before = backend.world.lock().unwrap().calls.len();
                assert_eq!(
                    json!({"listingsBeforeRelease": before}),
                    expected["parallel"],
                    "{name} @ {at}: listings in flight"
                );
                let _ = backend.gate.send_replace(true);
                let mut last_ok = None;
                for t in tasks {
                    last_ok = Some(t.await.unwrap().expect("a parallel call answers"));
                }
                last_ok.unwrap()
            } else {
                actor
                    .discover(op == "force")
                    .await
                    .expect("discovery answers")
            };
            let mut calls: Vec<Value> = backend
                .world
                .lock()
                .unwrap()
                .calls
                .iter()
                .map(|(s, h)| json!({"server": s, "headers": h}))
                .collect();
            calls.sort_by(|a, b| {
                (a["server"].as_str().unwrap(), a["headers"].to_string())
                    .cmp(&(b["server"].as_str().unwrap(), b["headers"].to_string()))
            });
            let want_calls: Vec<Value> = expected["calls"].as_array().unwrap().clone();
            assert_eq!(
                json!(calls),
                json!(want_calls),
                "{name} @ {at}: which servers were listed, and with which credential"
            );
            let want_state = if expected["state"]["same"] == json!(true) {
                last.clone().expect("the first step has a state")
            } else {
                expected["state"].clone()
            };
            compare_state(name, at, &want_state, &snapshot);
            if std::ptr::eq(step, script.last().unwrap()) {
                compare_composition(name, sc, &want_state, &snapshot);
            }
            last = Some(want_state);
        }
    }
}

#[test]
fn the_curated_manifest_is_cores_for_both_values_of_the_construction_flag() {
    let doc = fixture();
    for (key, flag) in [("off", false), ("on", true)] {
        let ours: Vec<Value> = noevia_server::mcp_manifest::manifest(flag)
            .iter()
            .map(|b| serde_json::from_str(&js_json::stringify(b).unwrap()).unwrap())
            .collect();
        assert_eq!(json!(ours), doc["manifests"][key], "diaryMcpWrite {key}");
    }
}

#[test]
fn the_discovery_token_is_what_cores_listener_verifies() {
    // core mcp-internal.cjs verifyToken: signature over the base64url payload, version, expiry; a
    // discovery token carries d:1 and no user or project and may never call a tool.
    use base64::Engine as _;
    let token = mint_discovery_token(KEY, 5_000, "11111111-1111-4111-8111-111111111111");
    let (body, sig) = token.split_once('.').unwrap();
    let want = {
        // HMAC-SHA256 through the standard construction, independently of the module under test.
        use sha2::{Digest, Sha256};
        let mut block = [0u8; 64];
        block[..KEY.len()].copy_from_slice(KEY);
        let mut inner = Sha256::new();
        inner.update(block.map(|b| b ^ 0x36));
        inner.update(body.as_bytes());
        let inner = inner.finalize();
        let mut outer = Sha256::new();
        outer.update(block.map(|b| b ^ 0x5c));
        outer.update(inner);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(outer.finalize())
    };
    assert_eq!(sig, want);
    let claims: Value = serde_json::from_slice(&base64url(body).unwrap()).unwrap();
    assert_eq!(
        claims,
        json!({"v": 1, "uid": null, "pid": null, "cid": null, "jti": "11111111-1111-4111-8111-111111111111",
            "exp": 125_000, "w": 0, "d": 1})
    );
}

#[tokio::test]
async fn a_caller_that_goes_away_neither_cancels_nor_duplicates_the_run() {
    let doc = fixture();
    let sc = &doc["scenarios"][0];
    let (gate, _) = tokio::sync::watch::channel(false);
    let backend = Arc::new(Script {
        clock: AtomicI64::new(1_000_000),
        world: Mutex::new(World {
            tools: tools_of(&sc["tools"]),
            ..World::default()
        }),
        operator: operator_of(&sc["servers"]),
        no_token: false,
        diary_write: false,
        preparation: Mutex::new(None),
        config_pause: Mutex::new(None),
        signal_pause: Mutex::new(None),
        config_reads: std::sync::atomic::AtomicUsize::new(0),
        gate,
    });
    let actor = Actor::new(Arc::clone(&backend), Timing::default());
    let first = tokio::spawn({
        let actor = Arc::clone(&actor);
        async move { actor.discover(false).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    // The caller is gone at the await; the run (three listings in flight) goes on.
    first.abort();
    let second = tokio::spawn({
        let actor = Arc::clone(&actor);
        async move { actor.discover(false).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    let _ = backend.gate.send_replace(true);
    let snapshot = second
        .await
        .unwrap()
        .expect("the run finished for the second caller");
    assert_eq!(
        backend.world.lock().unwrap().calls.len(),
        3,
        "one run, not two"
    );
    assert!(!snapshot.boxes.is_empty());
    // And it is published: the next call is served from it.
    backend.world.lock().unwrap().calls.clear();
    actor.discover(false).await.unwrap();
    assert!(backend.world.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn a_server_whose_listing_panics_is_a_failed_server_not_a_missing_run() {
    struct Panicking(Script);
    impl Backend for Panicking {
        fn now_ms(&self) -> i64 {
            self.0.now_ms()
        }
        async fn diary_mcp_write(&self) -> bool {
            false
        }
        async fn servers(&self) -> Vec<ServerSpec> {
            self.0.servers().await
        }
        async fn directory_signal(&self) -> String {
            String::new()
        }
        async fn account_ready(&self, _: &str) -> Ready {
            Ready::default()
        }
        fn static_token(&self, _: &str) -> Option<String> {
            None
        }
        async fn directory_headers(&self, _: &str) -> Vec<(String, String)> {
            Vec::new()
        }
        fn internal_discovery_token(&self) -> Option<String> {
            None
        }
        async fn user_disabled(&self, _: &str) -> bool {
            false
        }
        async fn oauth_token(&self, _: &str, _: &str) -> Result<Option<String>, String> {
            Ok(None)
        }
        async fn key_headers(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<Vec<(String, String)>>, String> {
            Ok(None)
        }
        async fn url_public(&self, _: &str) -> bool {
            true
        }
        async fn list_tools(
            &self,
            server: &ServerSpec,
            headers: &[(String, String)],
        ) -> Result<Vec<js_json::JValue>, String> {
            if server.id == "nextcloud" {
                panic!("a listing that dies");
            }
            self.0.list_tools(server, headers).await
        }
        fn log(&self, _: String) {}
    }
    let doc = fixture();
    let sc = &doc["scenarios"][0];
    let (gate, _) = tokio::sync::watch::channel(true);
    let backend = Arc::new(Panicking(Script {
        clock: AtomicI64::new(1_000_000),
        world: Mutex::new(World {
            tools: tools_of(&sc["tools"]),
            ..World::default()
        }),
        operator: operator_of(&sc["servers"]),
        no_token: false,
        diary_write: false,
        preparation: Mutex::new(None),
        config_pause: Mutex::new(None),
        signal_pause: Mutex::new(None),
        config_reads: std::sync::atomic::AtomicUsize::new(0),
        gate,
    }));
    let actor = Actor::new(backend, Timing::default());
    let snapshot = actor.discover(false).await.unwrap();
    let nextcloud = snapshot.server("nextcloud").unwrap();
    assert!(nextcloud.error.is_some());
    assert!(
        snapshot.server("search").unwrap().error.is_none(),
        "one server failing removes no other's tools"
    );
    let _ = Unavailable;
}

fn directory_backend(open: bool) -> Arc<Script> {
    let (gate, _) = tokio::sync::watch::channel(open);
    Arc::new(Script {
        clock: AtomicI64::new(1_000_000),
        world: Mutex::new(World {
            dir: vec![
                json!({"id":"removed", "url":"https://example.invalid/mcp", "title":"Synthetic"}),
            ],
            tools: BTreeMap::from([(
                "removed".into(),
                Some(vec![
                    json!({"name":"one", "description":"one", "inputSchema":{"type":"object"}}),
                ]),
            )]),
            ..World::default()
        }),
        operator: vec![],
        no_token: false,
        diary_write: false,
        preparation: Mutex::new(None),
        config_pause: Mutex::new(None),
        signal_pause: Mutex::new(None),
        config_reads: std::sync::atomic::AtomicUsize::new(0),
        gate,
    })
}
#[tokio::test]
async fn removed_last_directory_server_fences_blocked_listing_and_waiters() {
    let backend = directory_backend(false);
    let actor = Actor::new(backend.clone(), Timing::default());
    let old = tokio::spawn({
        let actor = actor.clone();
        async move { actor.discover(false).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while backend.world.lock().unwrap().calls.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    backend.world.lock().unwrap().dir.clear();
    let empty = actor.discover(false).await.unwrap();
    assert!(empty.offers.is_empty());
    backend.gate.send_replace(true);
    let late = old.await.unwrap();
    assert!(
        late.is_err() || late.unwrap().offers.is_empty(),
        "removed tools escaped to original waiter"
    );
    assert!(
        actor.current().offers.is_empty(),
        "late run overwrote authoritative empty snapshot"
    );
}
#[tokio::test]
async fn reflected_remote_credentials_never_reach_snapshot_picker_connectors_or_logs() {
    for (auth, reflected) in [
        ("oauth", "MCP 401: Bearer synthetic-reflected-token"),
        ("oauth", "MCP error -1: synthetic-reflected-token"),
        (
            "personal",
            "MCP 401: X-Custom-Key synthetic-reflected-token",
        ),
    ] {
        let backend = directory_backend(true);
        {
            let mut w = backend.world.lock().unwrap();
            w.dir[0]["addedBy"] = json!("admin");
            w.dir[0][if auth == "oauth" { "oauth" } else { "personal" }] = json!(1);
            w.oauth
                .insert("admin:removed".into(), "synthetic-reflected-token".into());
            w.keys.insert(
                "admin:removed".into(),
                BTreeMap::from([("X-Custom-Key".into(), "synthetic-reflected-token".into())]),
            );
            w.fail.insert("removed".into(), reflected.into());
        }
        let actor = Actor::new(backend.clone(), Timing::default());
        let snapshot = actor.discover(true).await.unwrap();
        let facts = js_json::parse(&fixture()["scenarios"][0]["facts"].to_string()).unwrap();
        let picker =
            noevia_server::mcp_discovery::picker_facts(&facts, &snapshot, &|_| true).unwrap();
        let catalogue =
            noevia_server::mcp_discovery::compose(&facts, &snapshot, &|_| true, &[]).unwrap();
        let output = format!(
            "{} {} {} {:?}",
            js_json::stringify(&snapshot.to_json()).unwrap(),
            js_json::stringify(&picker).unwrap(),
            catalogue_json(&catalogue),
            backend.world.lock().unwrap().logs
        );
        assert!(
            !output.contains("synthetic-reflected-token"),
            "remote credential reflected to public sink"
        );
        assert!(snapshot.error.is_some());
    }
}

#[tokio::test]
async fn front_restart_scopes_equal_generations_to_distinct_incarnations() {
    let first = Actor::new(directory_backend(true), Timing::default())
        .discover(false)
        .await
        .unwrap();
    let second = Actor::new(directory_backend(true), Timing::default())
        .discover(false)
        .await
        .unwrap();
    assert_eq!(first.generation, second.generation);
    let a = first.to_json();
    let b = second.to_json();
    assert_ne!(a.get("incarnation"), b.get("incarnation"));
    assert!(a.get("incarnation").as_str().is_some_and(|s| !s.is_empty()));
}

/// A request that reserved its place first but has not read the table yet, and a later request: the later one
/// must not read (and be accepted on) an older table than the earlier one publishes. The reads are serialized with
/// the acceptance of what they saw, so observation order is acceptance order.
#[tokio::test]
async fn a_later_starter_cannot_be_accepted_on_an_older_table_than_an_earlier_one_publishes() {
    let backend = directory_backend(true);
    let actor = Actor::new(backend.clone(), Timing::default());
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    *backend.config_pause.lock().unwrap() = Some((entered.clone(), resume.clone()));
    // A reserves its place and pauses before its configuration read executes.
    let a = tokio::spawn({
        let actor = actor.clone();
        async move { actor.discover(false).await }
    });
    entered.notified().await;
    // B starts second and arms the pause at the end of its own final signal read.
    let b_entered = Arc::new(tokio::sync::Notify::new());
    let b_resume = Arc::new(tokio::sync::Notify::new());
    *backend.signal_pause.lock().unwrap() = Some((b_entered.clone(), b_resume.clone()));
    let b = tokio::spawn({
        let actor = actor.clone();
        async move { actor.discover(false).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        backend.config_reads.load(Ordering::SeqCst),
        1,
        "B read the table while A, which reserved first, had not: its observation could be older than A's"
    );
    // The last directory server is removed; A resumes and sees the new, empty table.
    backend.world.lock().unwrap().dir.clear();
    resume.notify_one();
    // B's final signal read is paused after it captured its result (the table is empty by now: it read after A).
    tokio::time::timeout(std::time::Duration::from_secs(2), b_entered.notified())
        .await
        .unwrap();
    b_resume.notify_one();
    let a_out = a.await.unwrap().unwrap();
    let b_out = b.await.unwrap().unwrap();
    assert!(a_out.offers.is_empty() && b_out.offers.is_empty());
    assert!(
        b_out.generation >= a_out.generation,
        "a later observation never carries an older generation"
    );
    assert!(actor.current().offers.is_empty());
    assert!(
        backend.world.lock().unwrap().calls.is_empty(),
        "nothing was listed for the removed server"
    );
    assert_eq!(backend.config_reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn removal_invalidates_visible_catalogue_during_blocked_replacement() {
    let backend = directory_backend(true);
    {
        let mut w = backend.world.lock().unwrap();
        let mut remaining = w.dir[0].clone();
        remaining["id"] = json!("remaining");
        w.dir.push(remaining);
        let tools = w.tools["removed"].clone();
        w.tools.insert("remaining".into(), tools);
    }
    let actor = Actor::new(backend.clone(), Timing::default());
    assert_eq!(actor.discover(false).await.unwrap().offers.len(), 2);
    backend.gate.send_replace(false);
    {
        let mut w = backend.world.lock().unwrap();
        w.dir.retain(|r| r["id"] != "removed");
        w.calls.clear();
    }
    let refresh = tokio::spawn({
        let actor = actor.clone();
        async move { actor.discover(true).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while backend.world.lock().unwrap().calls.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // This is precisely the timeout fallback used by discovered().
    let fallback_safe = actor.current().offers.is_empty();
    let ordinary =
        tokio::time::timeout(std::time::Duration::from_millis(50), actor.discover(false)).await;
    backend.gate.send_replace(true);
    let replaced = refresh.await.unwrap().unwrap();
    assert!(
        fallback_safe,
        "timeout fallback exposed superseded catalogue"
    );
    assert!(
        ordinary.is_err() || ordinary.unwrap().is_err(),
        "ordinary caller reused pre-change tools"
    );
    assert!(replaced.offers.iter().all(|e| e.server == "remaining"));
}

#[path = "fixtures/l-catalogue-parser.rs"]
mod l_catalogue;

#[test]
fn all_native_offers_survive_l_catalogue_contract() {
    use noevia_server::mcp_discovery_actor::{Entry, Snapshot};
    for ids in [
        vec!["search"],
        vec!["search", "second"],
        vec!["search", "dir-x"],
    ] {
        let snap = Snapshot {
            enabled: true,
            offers: ["tavily_search", "tavily_extract", "outside_presentation"]
                .iter()
                .flat_map(|name| {
                    ids.iter().chain(ids.iter()).map(move |id| Entry {
                        name: (*name).into(),
                        server: (*id).into(),
                        tool: js_json::JValue::Null,
                        read_only: None,
                    })
                })
                .collect(),
            ..Snapshot::default()
        };
        let facts =
            js_json::parse(r#"{"builtin":[],"harnessEnabled":false,"hasRepositories":false}"#)
                .unwrap();
        let cat = noevia_server::mcp_discovery::compose(&facts, &snap, &|_| false, &[]).unwrap();
        let published = catalogue_json(&cat);
        let fixtures: Vec<Value> =
            serde_json::from_str(include_str!("fixtures/t-l-catalogue.v1.json")).unwrap();
        let expected = fixtures
            .iter()
            .find(|v| v["offers"]["tavily_search"] == json!(ids))
            .unwrap();
        assert_eq!(&published, expected, "T publishes the cross-branch fixture");
        let parsed =
            l_catalogue::parse(&published).expect("L accepts T's published catalogue fixture");
        for name in ["tavily_search", "tavily_extract", "outside_presentation"] {
            assert_eq!(
                parsed.offers.get(name),
                Some(&ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>())
            );
        }
        // L's operatorServerFor: ignore directory collisions, require exactly one operator.
        let operators: Vec<_> = parsed.offers["tavily_search"]
            .iter()
            .filter(|id| id.as_str() != "dir-x")
            .collect();
        assert_eq!(operators.len() == 1, !ids.contains(&"second"));
    }
}
