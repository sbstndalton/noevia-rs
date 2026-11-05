//! Shared test support: the deterministic byte stream and clock the oracle fixtures were made with,
//! and a transport that replays recorded exchanges.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use egress_issuer::transport::{BoxFuture, Reply, Transport, TransportRequest};
use egress_issuer::{Clock, ControlError, Reason};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// draw k (0-based) of n bytes = sha256("noevia-egress-fixture" || k as u64 LE)[0..n].
pub struct FixtureClock {
    pub wall: Mutex<u64>,
    pub mono: Mutex<f64>,
    pub drawn: Mutex<u64>,
}

impl FixtureClock {
    pub fn new(wall: u64, mono: f64) -> Arc<Self> {
        Arc::new(Self {
            wall: Mutex::new(wall),
            mono: Mutex::new(mono),
            drawn: Mutex::new(0),
        })
    }
    pub fn draw(&self, buf: &mut [u8]) {
        let mut k = self.drawn.lock().unwrap();
        let digest = Sha256::new()
            .chain_update(b"noevia-egress-fixture")
            .chain_update(k.to_le_bytes())
            .finalize();
        *k += 1;
        buf.copy_from_slice(&digest[..buf.len()]);
    }
}

impl Clock for FixtureClock {
    fn wall_ms(&self) -> u64 {
        *self.wall.lock().unwrap()
    }
    fn mono_ms(&self) -> f64 {
        *self.mono.lock().unwrap()
    }
    fn fill(&self, buf: &mut [u8]) {
        self.draw(buf);
    }
    fn sleep(&self, _ms: u64) -> BoxFuture<()> {
        Box::pin(tokio::task::yield_now())
    }
}

pub type RecordedReply = (u16, Vec<(String, String)>, String);

/// One recorded exchange: what the client must send and what the proxy answered.
#[derive(Clone, Debug)]
pub struct Exchange {
    pub route: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub response: Result<RecordedReply, ()>,
}

#[derive(Default)]
pub struct Script {
    pub queue: Mutex<VecDeque<Exchange>>,
    pub mismatches: Mutex<Vec<String>>,
}

impl Script {
    pub fn load(&self, exchanges: Vec<Exchange>) {
        *self.queue.lock().unwrap() = exchanges.into();
    }
    pub fn drained(&self) -> bool {
        self.queue.lock().unwrap().is_empty()
    }
}

pub struct ScriptedTransport(pub Arc<Script>);

impl Transport for ScriptedTransport {
    fn post(&self, request: TransportRequest) -> BoxFuture<Result<Reply, ControlError>> {
        let script = Arc::clone(&self.0);
        Box::pin(async move {
            let expected = script.queue.lock().unwrap().pop_front();
            let Some(expected) = expected else {
                script
                    .mismatches
                    .lock()
                    .unwrap()
                    .push(format!("unexpected request to {}", request.route));
                return Err(ControlError(Reason::Transport));
            };
            let body = String::from_utf8_lossy(&request.body).to_string();
            if expected.route != request.route
                || expected.headers != request.headers
                || expected.body != body
            {
                script.mismatches.lock().unwrap().push(format!(
                    "request differs from the oracle's: route {} vs {}\n  headers {:?}\n  vs      {:?}\n  body {}\n  vs   {}",
                    request.route, expected.route, request.headers, expected.headers, body, expected.body
                ));
                return Err(ControlError(Reason::Transport));
            }
            match expected.response {
                Err(()) => Err(ControlError(Reason::Transport)),
                Ok((status, raw_headers, body)) => Ok(Reply {
                    status,
                    raw_headers,
                    body: body.into_bytes(),
                }),
            }
        })
    }
}

// ---- a live fake proxy: reads the published key on every request, as the real one does ----

use egress::control::{derive_control_key, FileKey, RequestMetadata, Route};
use serde_json::Value;
use std::path::PathBuf;

pub const BOOT: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";

#[derive(Default)]
pub struct ProxyState {
    pub session: Option<String>,
    pub sessions_issued: u32,
    /// The modes of every Ready it saw, in order ("initial" / "replace").
    pub ready_modes: Vec<String>,
    pub prior_sessions: Vec<Option<String>>,
    pub down: bool,
    pub requests: u32,
}

pub struct FakeProxy {
    pub key_file: PathBuf,
    pub state: Mutex<ProxyState>,
}

impl FakeProxy {
    pub fn new(key_file: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            key_file,
            state: Mutex::new(ProxyState::default()),
        })
    }
    pub fn key(&self) -> FileKey {
        FileKey::from_hex(std::fs::read_to_string(&self.key_file).unwrap().trim()).unwrap()
    }
    pub fn session(&self) -> Option<String> {
        self.state.lock().unwrap().session.clone()
    }
}

pub struct ProxyTransport(pub Arc<FakeProxy>);

fn header<'a>(request: &'a TransportRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

impl Transport for ProxyTransport {
    fn post(&self, request: TransportRequest) -> BoxFuture<Result<Reply, ControlError>> {
        let proxy = Arc::clone(&self.0);
        Box::pin(async move {
            let mut st = proxy.state.lock().unwrap();
            st.requests += 1;
            if st.down {
                return Err(ControlError(Reason::Transport));
            }
            let file = proxy.key();
            let control = derive_control_key(&file).unwrap();
            let route = Route::parse(request.route).unwrap();
            let request_id = header(&request, "x-noevia-request-id").unwrap();
            let boot = header(&request, "x-noevia-boot-id");
            let meta = RequestMetadata {
                request_id,
                deadline_ms: header(&request, "x-noevia-deadline-ms")
                    .unwrap()
                    .parse()
                    .unwrap(),
                boot_id: boot,
                key_fingerprint: header(&request, "x-noevia-key-fingerprint").unwrap(),
            };
            let tag = header(&request, "x-noevia-control-mac")
                .unwrap()
                .to_string();
            control
                .verify_request(route, meta, &request.body, &tag)
                .map_err(|_| ControlError(Reason::Authentication))?;
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let fp = file.fingerprint();
            let reply = |status: u16, fields: String| -> Reply {
                let json = format!("{{\"version\":1,\"requestId\":\"{request_id}\",{fields}}}");
                let mac = control
                    .sign_response(&tag, status, json.as_bytes())
                    .unwrap();
                Reply {
                    status,
                    raw_headers: vec![
                        ("content-type".into(), "application/json".into()),
                        ("content-length".into(), json.len().to_string()),
                        ("cache-control".into(), "no-store".into()),
                        ("connection".into(), "close".into()),
                        ("x-noevia-control-mac".into(), mac),
                    ],
                    body: json.into_bytes(),
                }
            };
            let proof = |state: &str, session: &str, sequence: u64, challenge: Option<&str>| {
                let extra = challenge.map_or(String::new(), |c| format!(",\"challenge\":\"{c}\""));
                format!(
                    "\"state\":\"{state}\",\"capability\":\"{}\",\"bootId\":\"{BOOT}\",\"sessionId\":\"{session}\",\"keyFingerprint\":\"{fp}\",\"sequence\":{sequence},\"leaseMs\":15000{extra}",
                    egress::control::CAPABILITY
                )
            };
            match route {
                Route::Ready => {
                    let mode = body["mode"].as_str().unwrap().to_string();
                    let prior = body["priorSession"].as_str().map(str::to_string);
                    st.ready_modes.push(mode.clone());
                    st.prior_sessions.push(prior.clone());
                    let ok = match (mode.as_str(), &st.session) {
                        ("initial", None) => true,
                        ("replace", Some(live)) => prior.as_deref() == Some(live.as_str()),
                        _ => false,
                    };
                    if !ok {
                        return Ok(reply(409, "\"state\":\"conflict\"".into()));
                    }
                    st.sessions_issued += 1;
                    let session =
                        format!("{:032x}", 0x5e55_1011u128 + u128::from(st.sessions_issued));
                    st.session = Some(session.clone());
                    Ok(reply(
                        200,
                        proof("ready", &session, 0, body["challenge"].as_str()),
                    ))
                }
                Route::Renew => {
                    let session = body["sessionId"].as_str().unwrap();
                    Ok(reply(
                        200,
                        proof("renewed", session, body["sequence"].as_u64().unwrap(), None),
                    ))
                }
                Route::Revoke | Route::Activity | Route::Forget => {
                    if st.session.as_deref() != body["sessionId"].as_str() {
                        return Ok(reply(409, "\"state\":\"unknown-session\"".into()));
                    }
                    let (task, nonce, session) = (
                        body["taskId"].as_str().unwrap(),
                        body["leaseNonce"].as_str().unwrap(),
                        body["sessionId"].as_str().unwrap(),
                    );
                    let scope = format!("\"bootId\":\"{BOOT}\",\"sessionId\":\"{session}\",\"taskId\":\"{task}\",\"leaseNonce\":\"{nonce}\"");
                    if route == Route::Forget {
                        let snapshot = body["snapshotId"].as_str().unwrap();
                        return Ok(reply(
                            200,
                            format!(
                                "\"state\":\"forgotten\",{scope},\"snapshotId\":\"{snapshot}\""
                            ),
                        ));
                    }
                    let revoke = route == Route::Revoke;
                    let state = if revoke {
                        "revoke-confirmed"
                    } else {
                        "activity"
                    };
                    let snapshot = format!(
                        "{{\"id\":\"{}\",\"state\":\"complete\",\"final\":{revoke},\"summary\":{{\"hosts\":[],\"allowed\":0,\"refused\":0}},\"partial\":false,\"overflow\":false}}",
                        "ee".repeat(16)
                    );
                    Ok(reply(200, format!("\"state\":\"{state}\",{scope},\"barrierInstalled\":{revoke},\"drained\":{revoke},\"snapshot\":{snapshot}")))
                }
            }
        })
    }
}
