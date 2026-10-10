//! Refuse requests that arrive over the internal code network (sbstndalton/noevia#853, #1246):
//! the front's port of core server/code-net-guard.cjs, with the decisions taken by the
//! code-net-guard crate (the same Rust the Node guard confirms itself with).
//!
//! `COWORK_CODE_NET_ADDR` names web's OWN address on the code network (IP literals and/or host
//! names, comma or space separated; the override sets it to `egress`). A request whose
//! connection arrived on one of those local addresses is answered 403 with no body before any
//! dispatch. Unset or empty: no check.
//!
//! As in the JS:
//! - a malformed entry stops startup; so does a spec the crate refuses as ambiguous;
//! - host names are resolved at startup (5 s per lookup), the first pass is awaited by the first
//!   requests so none slips through, and unresolved names are retried with backoff (5 s doubling
//!   to 5 min). Until a name resolves the guard fails open for it (refusing everything would take
//!   the UI down) and says so in the log each time;
//! - a fault while reading lookup answers refuses every request from then on; a fault on one
//!   request refuses that request.

use crate::reply;
use axum::body::Body;
use axum::http::Response;
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_secs(5);
const MAX_RETRY: Duration = Duration::from_secs(5 * 60);

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `COWORK_CODE_NET_ADDR`, parsed as core parseCodeNetSpec (via the crate).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeNetSpec {
    pub literals: Vec<String>,
    pub hosts: Vec<String>,
}

impl CodeNetSpec {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match code_net_guard::parse_spec(&utf16(raw)) {
            Ok(code_net_guard::Spec::Ok { literals, hosts }) => Ok(CodeNetSpec { literals, hosts }),
            Ok(code_net_guard::Spec::Malformed(entry)) => Err(format!(
                "COWORK_CODE_NET_ADDR should list IP addresses or host names, not {entry:?}"
            )),
            Err(_) => Err(
                "COWORK_CODE_NET_ADDR is ambiguous (non-ASCII text, a %zone literal or an unusual host name); list plain IP addresses or host names"
                    .into(),
            ),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.literals.is_empty() || !self.hosts.is_empty()
    }
}

/// How a host name is looked up (injectable for tests).
pub type Lookup = Arc<
    dyn Fn(
            String,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<String>, String>> + Send>,
        > + Send
        + Sync,
>;

pub fn system_lookup() -> Lookup {
    Arc::new(|host: String| {
        Box::pin(async move {
            tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map(|it| it.map(|a| a.ip().to_string()).collect())
                .map_err(|e| e.to_string())
        })
    })
}

pub struct CodeNetGuard {
    enabled: bool,
    addresses: Mutex<BTreeSet<String>>,
    broken: AtomicBool,
    ready: watch::Receiver<bool>,
    refused: AtomicU64,
    last_logged: Mutex<Option<std::time::Instant>>,
}

impl CodeNetGuard {
    /// A guard for `spec`. Host names are resolved on the current tokio runtime; without one
    /// (never the case in the binary) a spec with host names refuses everything.
    pub fn new(spec: &CodeNetSpec, lookup: Lookup) -> Arc<Self> {
        let (ready_tx, ready_rx) = watch::channel(spec.hosts.is_empty());
        let guard = Arc::new(CodeNetGuard {
            enabled: spec.enabled(),
            addresses: Mutex::new(spec.literals.iter().cloned().collect()),
            broken: AtomicBool::new(false),
            ready: ready_rx,
            refused: AtomicU64::new(0),
            last_logged: Mutex::new(None),
        });
        if !spec.hosts.is_empty() {
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => {
                    rt.spawn(resolve_loop(
                        Arc::clone(&guard),
                        spec.hosts.clone(),
                        lookup,
                        ready_tx,
                    ));
                }
                Err(_) => {
                    guard.broken.store(true, Ordering::SeqCst);
                    let _ = ready_tx.send(true);
                }
            }
        } else if guard.enabled {
            log_guarding(&guard);
        }
        guard
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether a connection that arrived on `local` came in over the code network.
    pub async fn refuses(&self, local: SocketAddr) -> bool {
        if !self.enabled {
            return false;
        }
        let mut ready = self.ready.clone();
        if ready.wait_for(|r| *r).await.is_err() {
            return true;
        }
        if self.broken.load(Ordering::SeqCst) {
            return true;
        }
        let addresses: Vec<Vec<u16>> = match self.addresses.lock() {
            Ok(a) => a.iter().map(|s| utf16(s)).collect(),
            Err(_) => return true,
        };
        let refs: Vec<&[u16]> = addresses.iter().map(Vec::as_slice).collect();
        let local = utf16(&local.ip().to_string());
        // A fault on a request refuses that request.
        code_net_guard::refuses(&refs, Some(&local)).unwrap_or(true)
    }

    /// The 403 with no body (core deny()), logged at most once a minute.
    pub fn deny(&self) -> Response<Body> {
        let n = self.refused.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut last) = self.last_logged.lock() {
            let now = std::time::Instant::now();
            if last.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(60)) {
                *last = Some(now);
                eprintln!(r#"{{"event":"codenet.refused","listener":"ui","refused":{n}}}"#);
            }
        }
        reply::code_net_refused()
    }
}

fn log_guarding(guard: &CodeNetGuard) {
    if let Ok(a) = guard.addresses.lock() {
        let list = serde_json::to_string(&a.iter().collect::<Vec<_>>()).unwrap_or_default();
        eprintln!(r#"{{"event":"codenet.guarding","addresses":{list}}}"#);
    }
}

async fn resolve_loop(
    guard: Arc<CodeNetGuard>,
    mut unresolved: Vec<String>,
    lookup: Lookup,
    ready: watch::Sender<bool>,
) {
    let mut wait = RETRY;
    loop {
        let mut still = Vec::new();
        for host in unresolved {
            let answer = tokio::time::timeout(RESOLVE_TIMEOUT, lookup(host.clone()))
                .await
                .unwrap_or_else(|_| Err("lookup timed out".into()));
            let found = answer.map(|answers| {
                let raw: Vec<Vec<u16>> = answers.iter().map(|a| utf16(a)).collect();
                let refs: Vec<Option<&[u16]>> = raw.iter().map(|a| Some(a.as_slice())).collect();
                match code_net_guard::resolved_addresses(&refs) {
                    Ok(found) => found,
                    Err(_) => {
                        // The guard's view of what to refuse is now incomplete: fail closed.
                        guard.broken.store(true, Ordering::SeqCst);
                        eprintln!(r#"{{"event":"codenet.fault","where":"resolve"}}"#);
                        Vec::new()
                    }
                }
            });
            match found {
                Ok(found) if !found.is_empty() => {
                    if let Ok(mut a) = guard.addresses.lock() {
                        a.extend(found);
                    } else {
                        guard.broken.store(true, Ordering::SeqCst);
                    }
                }
                Ok(_) if guard.broken.load(Ordering::SeqCst) => {}
                other => {
                    let reason = other.err().unwrap_or_else(|| "no address".into());
                    let reason: String = reason.chars().take(200).collect();
                    eprintln!(
                        "{}",
                        serde_json::json!({"event": "codenet.resolve_failed", "host": host, "reason": reason})
                    );
                    still.push(host);
                }
            }
        }
        // The first pass is what the first requests wait for.
        let _ = ready.send(true);
        if still.is_empty() {
            log_guarding(&guard);
            return;
        }
        unresolved = still;
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(MAX_RETRY);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fixed(answers: Result<Vec<&'static str>, &'static str>) -> Lookup {
        Arc::new(move |_| {
            let a = answers
                .clone()
                .map(|v| v.into_iter().map(String::from).collect())
                .map_err(String::from);
            Box::pin(async move { a })
        })
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn spec_parses_like_core() {
        assert_eq!(CodeNetSpec::parse("").unwrap(), CodeNetSpec::default());
        let s = CodeNetSpec::parse(" 172.30.0.2, Egress ::ffff:10.0.0.1").unwrap();
        assert_eq!(s.literals, vec!["172.30.0.2", "10.0.0.1"]);
        assert_eq!(s.hosts, vec!["egress"]);
        assert!(CodeNetSpec::parse("not_a host").is_err());
        assert!(CodeNetSpec::parse("fe80::1%eth0").is_err());
    }

    #[tokio::test]
    async fn literals_refuse_their_address_only() {
        let g = CodeNetGuard::new(&CodeNetSpec::parse("127.0.0.2").unwrap(), fixed(Ok(vec![])));
        assert!(g.refuses(sa("127.0.0.2:8021")).await);
        assert!(g.refuses(sa("[::ffff:127.0.0.2]:8021")).await);
        assert!(!g.refuses(sa("127.0.0.1:8021")).await);
        let off = CodeNetGuard::new(&CodeNetSpec::default(), fixed(Ok(vec![])));
        assert!(!off.enabled());
        assert!(!off.refuses(sa("127.0.0.2:8021")).await);
    }

    #[tokio::test]
    async fn host_names_resolve_before_the_first_answer() {
        let g = CodeNetGuard::new(
            &CodeNetSpec::parse("egress").unwrap(),
            fixed(Ok(vec!["172.30.0.2"])),
        );
        assert!(g.refuses(sa("172.30.0.2:8021")).await);
        assert!(!g.refuses(sa("127.0.0.1:8021")).await);
    }

    #[tokio::test]
    async fn unresolved_names_fail_open_and_bad_answers_fail_closed() {
        let g = CodeNetGuard::new(&CodeNetSpec::parse("egress").unwrap(), fixed(Err("nx")));
        assert!(!g.refuses(sa("172.30.0.2:8021")).await);
        let g = CodeNetGuard::new(
            &CodeNetSpec::parse("egress").unwrap(),
            fixed(Ok(vec!["17\u{e9}.0.0.1"])),
        );
        assert!(g.refuses(sa("127.0.0.1:8021")).await);
    }
}
