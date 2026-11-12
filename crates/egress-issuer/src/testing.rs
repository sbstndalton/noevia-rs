//! A fake proxy for tests of this crate and its users (feature `testing`): it reads the published key on
//! every request, as the real proxy does, verifies the request's MAC, signs its answers, and keeps one
//! session. As the real engine (`bins/egress-proxy/src/control/engine.rs`, `ready_precondition` and
//! `sync_key`) it accepts Initial only while it is not initialized under the published key, and a
//! changed key resets that and retires the session. Session expiry does not reset it.
//! [`FakeProxy::initialized_by_node`] starts already initialized under the published key, with a
//! session nobody in the test can name, as the live proxy is after the retired Node issuer's Ready.
//! Synthetic; never built into the front.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use crate::transport::{BoxFuture, Reply, Transport, TransportRequest};
use crate::{ControlError, Reason};
use std::sync::{Arc, Mutex};

use egress::control::{derive_control_key, FileKey, RequestMetadata, Route};
use serde_json::Value;
use std::path::PathBuf;

pub const BOOT: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
/// The session of [`FakeProxy::initialized_by_node`]: the retired Node issuer's, never known to a client.
pub const NODE_SESSION: &str = "40de40de40de40de40de40de40de40de";

#[derive(Default)]
pub struct ProxyState {
    pub session: Option<String>,
    pub sessions_issued: u32,
    /// The modes of every Ready it saw, in order ("initial" / "replace").
    pub ready_modes: Vec<String>,
    pub prior_sessions: Vec<Option<String>>,
    pub down: bool,
    pub requests: u32,
    /// A Ready succeeded under `fingerprint` (the engine's `initialized`).
    pub initialized: bool,
    /// The fingerprint of the published key the last request was read under (the engine's `sync_key`).
    pub fingerprint: Option<String>,
    /// How many times the published key changed under it.
    pub key_changes: u32,
    /// Not the engine: a key change resets nothing, so Initial stays refused (a proxy that never accepts).
    pub sticky: bool,
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
    /// A proxy the retired Node issuer already did its Ready on, under the key published now: Initial is
    /// refused until the key changes, and the live session is one no test client can name.
    pub fn initialized_by_node(key_file: PathBuf) -> Arc<Self> {
        let fingerprint = FileKey::from_hex(std::fs::read_to_string(&key_file).unwrap().trim())
            .unwrap()
            .fingerprint();
        Arc::new(Self {
            key_file,
            state: Mutex::new(ProxyState {
                session: Some(NODE_SESSION.to_string()),
                initialized: true,
                fingerprint: Some(fingerprint),
                ..ProxyState::default()
            }),
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
            if st.fingerprint.as_deref() != Some(fp.as_str()) {
                if st.fingerprint.is_some() {
                    st.key_changes += 1;
                    if !st.sticky {
                        st.session = None;
                        st.initialized = false;
                    }
                }
                st.fingerprint = Some(fp.clone());
            }
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
                        ("initial", None) => !st.initialized,
                        ("replace", Some(live)) => prior.as_deref() == Some(live.as_str()),
                        _ => false,
                    };
                    if !ok {
                        return Ok(reply(409, "\"state\":\"conflict\"".into()));
                    }
                    st.sessions_issued += 1;
                    st.initialized = true;
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
