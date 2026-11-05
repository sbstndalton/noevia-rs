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

pub use egress_issuer::testing::{FakeProxy, ProxyTransport, BOOT};
