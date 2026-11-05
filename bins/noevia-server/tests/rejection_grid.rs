//! Residual methods retain their current owners. The oracle is the unmodified Core entrypoint
//! at contracts/http/front-core.ref, not a reconstruction of its mounts. This is no retirement claim.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use noevia_server::{
    config::Config,
    routes::ROUTES,
    serve::{serve, Limits},
    App,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::net::TcpListener;

const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "HEAD"];
const ORIGIN: &str = "http://127.0.0.1:18021";
const EXPECTED_ROUTES: usize = 79;
const EXPECTED_METHODS: usize = 460;
const EXPECTED_PATH_METHODS: usize = 1058;
const EXPECTED_CASES: usize = 10580; // Path normalization variants × five session/CSRF states × two feature states
const HEADERS: [&str; 12] = [
    "content-type",
    "cache-control",
    "www-authenticate",
    "allow",
    "x-noevia-api",
    "x-content-type-options",
    "referrer-policy",
    "x-frame-options",
    "content-security-policy",
    "strict-transport-security",
    "permissions-policy",
    "vary",
];

fn family(p: &str) -> bool {
    [
        "/api/stats",
        "/.well-known/webauthn",
        "/api/auth/",
        "/api/admin/",
        "/api/profile",
        "/api/account/",
        "/api/setup/",
        "/api/instance",
        "/api/connectors",
        "/api/integrations/",
        "/api/mcp-",
        "/api/models/",
        "/api/model-manager/",
        "/api/providers",
        "/api/auto-roles",
        "/api/routing-",
        "/api/reasoning-",
        "/api/sampling-",
    ]
    .iter()
    .any(|prefix| p.starts_with(prefix))
}
fn sample(p: &str) -> String {
    p.replace("{*?}", "")
        .replace("{*}", "synthetic-id")
        .replace("{id}", "synthetic-id")
}
fn variants(template: &str) -> Vec<String> {
    let base = sample(template);
    let mut paths = vec![base.clone(), format!("{base}/")];
    if template.contains("{*?}") {
        paths.extend([
            format!("{base}/synthetic-id"),
            format!("{base}/synthetic-id/"),
        ]);
    } else if template.contains('{') {
        paths.extend([
            base.replace("synthetic-id", "bad%2Fid"),
            base.replace("synthetic-id", "synthetic-id/extra"),
        ]);
    }
    paths
}
// Each row holds one manifest route/method/path and five Node statuses for each feature state.
// The fixture was recorded from the pinned real Core with a live-session probe before every
// authenticated outcome. It is independent of the sessions exercised by this replay.
type StatusKey = (String, String, String);
type StatusRow = (String, String, String, [u16; 5], [u16; 5]);
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SavedNodeStatuses {
    core_ref: String,
    cases: usize,
    states: [String; 5],
    features: [String; 2],
    outcomes: Vec<StatusRow>,
}

struct Oracle(Child);
impl Drop for Oracle {
    fn drop(&mut self) {
        // EOF reaches the launcher's cleanup hook; killing the launcher immediately could orphan Core.
        drop(self.0.stdin.take());
        for _ in 0..250 {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[derive(Debug, PartialEq, serde::Serialize)]
struct Answer {
    status: u16,
    headers: BTreeMap<String, Vec<String>>,
    body: Vec<u8>,
}
async fn answer(
    client: &reqwest::Client,
    port: u16,
    method: &str,
    path: &str,
    account: Option<&Value>,
    bad_csrf: bool,
) -> Answer {
    let mut req = client
        .request(
            method.parse::<reqwest::Method>().unwrap(),
            format!("http://127.0.0.1:{port}{path}"),
        )
        .header("origin", ORIGIN)
        .header("content-type", "application/json")
        .body("{}");
    if let Some(account) = account {
        req = req
            .header("cookie", account["cookie"].as_str().unwrap())
            .header(
                "x-csrf-token",
                if bad_csrf {
                    "synthetic-invalid"
                } else {
                    account["csrf"].as_str().unwrap()
                },
            );
    }
    let res = req
        .send()
        .await
        .unwrap_or_else(|e| panic!("{method} {path}: {e}"));
    let status = res.status().as_u16();
    let mut headers = BTreeMap::new();
    for name in HEADERS {
        let values: Vec<_> = res
            .headers()
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        headers.insert(name.into(), values);
    }
    // reqwest removes transport chunk framing only; no text/JSON parsing or normalization.
    let body = res.bytes().await.unwrap().to_vec();
    Answer {
        status,
        headers,
        body,
    }
}
async fn front(data: &Path, upstream: u16, flag: &str) -> u16 {
    let env = BTreeMap::from([
        (
            "NOEVIA_LEGACY_UPSTREAM",
            format!("http://127.0.0.1:{upstream}"),
        ),
        ("UI_DATA_DIR", data.display().to_string()),
        ("PUBLIC_ORIGIN", ORIGIN.to_owned()),
        ("NOEVIA_FEATURE_CHATGPT_OAUTH", flag.to_owned()),
    ]);
    let app = App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve(
        listener,
        Arc::clone(&app),
        Limits::default(),
        std::future::pending(),
    ));
    port
}

#[tokio::test]
async fn manifest_rejection_grid_matches_pinned_core() {
    let Ok(core) = std::env::var("NOEVIA_CORE_CHECKOUT") else {
        assert_ne!(
            std::env::var("NOEVIA_REJECTION_ORACLE_REQUIRED").as_deref(),
            Ok("1"),
            "required pinned Core oracle missing"
        );
        eprintln!("SKIPPED live rejection grid: NOEVIA_CORE_CHECKOUT is unset");
        return;
    };
    let routes: Vec<_> = ROUTES
        .iter()
        .filter(|r| !r.methods.is_empty() && family(r.path))
        .collect();
    assert_eq!(
        routes.len(),
        EXPECTED_ROUTES,
        "restricted family entries changed; audit coverage before updating count"
    );
    let methods: Vec<_> = routes
        .iter()
        .flat_map(|r| {
            METHODS
                .iter()
                .filter(|m| !r.methods.contains(m))
                .map(move |m| (r.path, *m, sample(r.path)))
        })
        .collect();
    assert_eq!(
        methods.len(),
        EXPECTED_METHODS,
        "every nonlisted method must be covered"
    );
    let wire_cases: Vec<_> = methods
        .iter()
        .flat_map(|(template, method, _)| {
            variants(template)
                .into_iter()
                .map(move |path| (*template, *method, path))
        })
        .collect();
    assert_eq!(
        wire_cases.len(),
        EXPECTED_PATH_METHODS,
        "all trailing-slash and malformed-id variants are required"
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();
    let mut compared = 0;
    let mut differences = Vec::new();
    let mut status_counts = BTreeMap::new();
    let mut session_statuses: BTreeMap<&str, BTreeMap<u16, usize>> = BTreeMap::new();
    let states = [
        "admin",
        "member",
        "signed-out",
        "bad-CSRF-admin",
        "bad-CSRF-member",
    ];
    let fixture: SavedNodeStatuses =
        serde_json::from_str(include_str!("fixtures/rejection-status-node.json")).unwrap();
    assert_eq!(
        fixture.core_ref,
        include_str!("../../../contracts/http/front-core.ref").trim()
    );
    assert_eq!(fixture.cases, EXPECTED_CASES);
    assert_eq!(fixture.states, states.map(str::to_owned));
    assert_eq!(fixture.features, ["1".to_owned(), "0".to_owned()]);
    assert_eq!(fixture.outcomes.len(), EXPECTED_PATH_METHODS);
    let mut saved_node: BTreeMap<StatusKey, [[u16; 5]; 2]> = BTreeMap::new();
    for (route, method, path, on, off) in fixture.outcomes {
        assert!(
            saved_node
                .insert((route, method, path), [on, off])
                .is_none(),
            "duplicate saved Node case"
        );
    }
    let generated_keys: BTreeSet<_> = wire_cases
        .iter()
        .map(|(route, method, path)| (route.to_string(), method.to_string(), path.clone()))
        .collect();
    assert_eq!(generated_keys.len(), EXPECTED_PATH_METHODS);
    assert_eq!(
        saved_node.keys().cloned().collect::<BTreeSet<_>>(),
        generated_keys,
        "saved Node outcomes must cover exactly the generated cases"
    );
    let mut status_outcomes = Vec::new();
    for (feature_index, flag) in ["1", "0"].into_iter().enumerate() {
        let data = tempfile::tempdir().unwrap();
        let ready_file = data.path().join("oracle-ready.json");
        let log = std::fs::File::create(data.path().join("oracle.log")).unwrap();
        let mut oracle = Oracle(
            Command::new("timeout")
                .args(["--signal=TERM", "--kill-after=10s", "600s", "node"])
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/rejection-oracle.cjs"
                ))
                .arg(&core)
                .arg(data.path())
                .arg(&ready_file)
                .arg(flag)
                .arg(include_str!("../../../contracts/http/front-core.ref").trim())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(100);
        while !ready_file.exists() {
            assert!(
                oracle.0.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "pinned Core startup failed: {}",
                std::fs::read_to_string(data.path().join("oracle.log")).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let ready: Value = serde_json::from_slice(&std::fs::read(&ready_file).unwrap()).unwrap();
        let port = u16::try_from(ready["port"].as_u64().unwrap()).unwrap();
        let native = front(data.path(), port, flag).await;
        let accounts = ready["accounts"].as_array().unwrap();
        // On every entry: admin, member, signed-out, bad-CSRF admin and bad-CSRF member.
        for (template, method, path) in &wire_cases {
            let saved_statuses = saved_node
                .get(&(template.to_string(), method.to_string(), path.clone()))
                .unwrap();
            for (state, &state_name) in states.iter().enumerate() {
                let account = match state {
                    0 | 3 => Some(&accounts[0]),
                    1 | 4 => Some(&accounts[1]),
                    _ => None,
                };
                let expected = answer(&client, port, method, path, account, state >= 3).await;
                let actual = answer(&client, native, method, path, account, state >= 3).await;
                *status_counts.entry(expected.status).or_insert(0usize) += 1;
                *session_statuses
                    .entry(state_name)
                    .or_default()
                    .entry(expected.status)
                    .or_default() += 1;
                let saved_status = saved_statuses[feature_index][state];
                // Individual assertions prevent a shared session failure from making a live
                // Node/native pair trivially equal. Keep Node's 403/404/405 distinctions.
                assert_eq!(expected.status, saved_status, "saved Node status guard (Node): {template} | {method} {path} | {state_name} | feature={flag}");
                assert_eq!(actual.status, saved_status, "saved Node status guard (native): {template} | {method} {path} | {state_name} | feature={flag}");
                status_outcomes.push(json!([
                    template,
                    method,
                    path,
                    state_name,
                    flag,
                    saved_status,
                    expected.status,
                    actual.status
                ]));
                compared += 1;
                let equal = actual.status == expected.status
                    && actual.headers == expected.headers
                    && (*method == "HEAD" || actual.body == expected.body);
                if !equal {
                    differences.push(json!({"template":template,"method":method,"path":path,"state":state,"feature":flag,"node":expected,"native":actual}));
                }
            }
        }
        drop(oracle);
    }
    assert_eq!(compared, EXPECTED_CASES, "silent coverage shrink");
    eprintln!("asserted rejection case count: {compared}; restricted entries: {}; method pairs: {}; Node statuses: {status_counts:?}; differences: {}", routes.len(), methods.len(), differences.len());
    if let Ok(report) = std::env::var("NOEVIA_REJECTION_REPORT") {
        std::fs::write(report, serde_json::to_vec_pretty(&json!({"cases":compared,"entries":routes.len(),"methods":methods.len(),"pathMethods":wire_cases.len(),"coreRef":include_str!("../../../contracts/http/front-core.ref").trim(),"headers":HEADERS,"states":states,"statusCounts":status_counts,"sessionStatusCounts":session_statuses,"statusOutcomes":status_outcomes,"differences":differences})).unwrap()).unwrap();
    }
    assert!(
        differences.is_empty(),
        "{} native/Core differences; first: {:?}",
        differences.len(),
        differences.first()
    );
}
