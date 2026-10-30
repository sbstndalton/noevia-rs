//! core service-endpoint.cjs, the rule for an operator's own services, here for a chat role on another
//! machine (#1755, #1845): http or https, no login, and an address on this network, namely loopback,
//! RFC 1918, Tailscale CGNAT 100.64.0.0/10 or IPv6 ULA. Link-local is refused (no caller opts in), and so
//! are public, unspecified, multicast, reserved and documentation addresses, the IPv6 forms that carry
//! another address, and the cloud metadata addresses. A name passes only if every address it resolves
//! to passes: the check runs in the socket's own lookup, so the address connected to is the one checked
//! (DNS rebinding). A redirect is never followed.
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// A name or address outside this network. The error text is searched for it in a reqwest error chain.
#[derive(Debug)]
pub struct NotOnThisNetwork;
impl std::fmt::Display for NotOnThisNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the address is not on this network")
    }
}
impl std::error::Error for NotOnThisNetwork {}

/// Whether reqwest's error came from [`NotOnThisNetwork`].
pub fn refused_in_chain(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut at: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = at {
        if err.is::<NotOnThisNetwork>() {
            return true;
        }
        at = err.source();
    }
    false
}

fn v4_on_network(ip: Ipv4Addr) -> bool {
    let [a, b, c, d] = ip.octets();
    if ip == Ipv4Addr::new(100, 100, 100, 200) || ip == Ipv4Addr::new(169, 254, 169, 254) {
        return false;
    }
    if (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a == 0
        || a >= 224
        || (a == 255 && b == 255 && c == 255 && d == 255)
    {
        return false;
    }
    a == 127
        || a == 10
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 100 && (64..=127).contains(&b))
}

fn v6_on_network(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_on_network(v4);
    }
    let s = ip.segments();
    if ip == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254) {
        return false;
    }
    ip == Ipv6Addr::LOCALHOST || s.first().is_some_and(|h| h & 0xfe00 == 0xfc00)
}

/// The rule for one address.
pub fn on_this_network(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_on_network(v4),
        IpAddr::V6(v6) => v6_on_network(v6),
    }
}

/// The synchronous half: scheme, no login, and an address literal's own check. (An address literal never
/// reaches the socket's lookup.) A name is judged at connect time.
pub fn url_allowed(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return false;
    }
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => on_this_network(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => on_this_network(IpAddr::V6(ip)),
        Some(url::Host::Domain(_)) => true,
        None => false,
    }
}

struct OnlyThisNetwork;

impl Resolve for OnlyThisNetwork {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = format!("{}:0", name.as_str());
            let found = tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host(host))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
            let all: Vec<SocketAddr> = found.collect();
            if all.is_empty() || !all.iter().all(|a| on_this_network(a.ip())) {
                return Err(Box::new(NotOnThisNetwork) as Box<dyn std::error::Error + Send + Sync>);
            }
            let addrs: Addrs = Box::new(all.into_iter());
            Ok(addrs)
        })
    }
}

/// The client for a model on another machine: every name resolved through the rule, no redirect, no
/// proxy. Pair it with [`url_allowed`] for address literals.
pub fn this_network_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(std::sync::Arc::new(OnlyThisNetwork))
        .build()
        .map_err(|e| format!("machine client: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_address_rule_as_core_has_it() {
        for ok in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.1",
            "192.168.1.20",
            "100.64.0.1",
            "100.127.255.1",
            "::1",
            "fd12::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(on_this_network(ok.parse().unwrap()), "{ok}");
        }
        for no in [
            "8.8.8.8",
            "0.0.0.0",
            "169.254.169.254",
            "169.254.1.1",
            "100.100.100.200",
            "100.128.0.1",
            "172.32.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "::",
            "fe80::1",
            "2001:4860:4860::8888",
            "fd00:ec2::254",
            "64:ff9b::808:808",
            "2001::1",
            "::ffff:8.8.8.8",
            "::808:808",
        ] {
            assert!(!on_this_network(no.parse().unwrap()), "{no}");
        }
    }

    #[test]
    fn the_url_rule() {
        assert!(url_allowed("http://192.168.1.20:8080/v1/chat/completions"));
        assert!(url_allowed("https://box.tailnet.ts.net:8080/v1"));
        assert!(!url_allowed("http://8.8.8.8:8080/v1"));
        assert!(!url_allowed("http://[fe80::1]:8080/v1"));
        assert!(!url_allowed("ftp://192.168.1.20/v1"));
        assert!(!url_allowed("http://u:p@192.168.1.20/v1"));
        assert!(!url_allowed("not a url"));
    }

    #[tokio::test]
    async fn a_name_resolving_outside_the_network_is_refused_at_the_socket() {
        // `localhost` passes; the refusal is exercised through the resolver's own check above, and a
        // public literal never gets this far (url_allowed).
        let client = this_network_client().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        // Reaches the listener by name, so the resolver ran and let it through.
        let got = client
            .get(format!("http://localhost:{port}/"))
            .timeout(Duration::from_millis(200))
            .send()
            .await;
        assert!(got.is_err_and(|e| !refused_in_chain(&e)));
        // A name that cannot be on this network: the documentation-only name has no answer here, and
        // an unresolved name is refused too, without a connection.
        let got = client
            .get("http://noevia-test.invalid:1/")
            .timeout(Duration::from_secs(8))
            .send()
            .await;
        assert!(got.is_err());
    }
}
