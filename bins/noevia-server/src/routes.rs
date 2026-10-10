//! Route ownership, from contracts/http/routes.toml (compiled in by build.rs). One owner per
//! route: a request whose route is owner = "rust" (for one of its methods) is answered natively,
//! everything else goes to Node through the legacy proxy.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Rust,
    Node,
}

#[derive(Debug)]
pub struct Route {
    pub path: &'static str,
    pub owner: Owner,
    /// Methods the owner answers; empty means all. Other methods go to Node.
    pub methods: &'static [&'static str],
    /// The non-/api fallback: matches any non-/api path no other entry names.
    pub catch_all: bool,
}

include!(concat!(env!("OUT_DIR"), "/routes_gen.rs"));

/// The native handlers. Every owner = "rust" route must map to one ([`native_for`]); the tests
/// fail otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Native {
    /// GET /api/ready (core routes/health.cjs createReadyRoutes).
    Ready,
    /// The web bundle with SPA fallback (core static-files.cjs + spa-routes.cjs).
    Static,
}

/// The native handler a rust-owned route is implemented by.
pub fn native_for(route: &Route) -> Option<Native> {
    match (route.path, route.catch_all) {
        ("/api/ready", false) => Some(Native::Ready),
        (_, true) => Some(Native::Static),
        _ => None,
    }
}

fn is_api(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

/// Segments of a route pattern; `{name}` matches one non-empty segment, `{*?}` (an optional
/// suffix or query on the last segment) is dropped.
fn pattern_matches(pattern: &str, path: &str) -> Option<usize> {
    let pattern = pattern.replace("{*?}", "");
    let trimmed = |s: &str| -> String {
        if s.len() > 1 {
            s.strip_suffix('/').unwrap_or(s).to_string()
        } else {
            s.to_string()
        }
    };
    let pat = trimmed(&pattern);
    let p = trimmed(path);
    let ps: Vec<&str> = pat.split('/').collect();
    let xs: Vec<&str> = p.split('/').collect();
    if ps.len() != xs.len() {
        return None;
    }
    let mut literal = 0;
    for (a, b) in ps.iter().zip(xs.iter()) {
        if a.starts_with('{') && a.ends_with('}') {
            if b.is_empty() {
                return None;
            }
        } else if a == b {
            literal += 1;
        } else {
            return None;
        }
    }
    Some(literal)
}

/// The route a request path belongs to: the most specific listed pattern, else (for non-/api
/// paths) the catch-all. `None` is an unlisted /api path, which Node owns.
pub fn route_for(path: &str) -> Option<&'static Route> {
    let best = ROUTES
        .iter()
        .filter(|r| !r.catch_all)
        .filter_map(|r| pattern_matches(r.path, path).map(|score| (score, r)))
        .max_by_key(|(score, _)| *score)
        .map(|(_, r)| r);
    if best.is_some() || is_api(path) {
        return best;
    }
    ROUTES.iter().find(|r| r.catch_all)
}

/// What answers `method path`: a native handler, or `None` for the legacy proxy.
pub fn dispatch(method: &str, path: &str) -> Option<Native> {
    let route = route_for(path)?;
    if route.owner != Owner::Rust {
        return None;
    }
    if !route.methods.is_empty() && !route.methods.contains(&method) {
        return None;
    }
    native_for(route)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rust_route_has_a_native_handler() {
        for r in ROUTES.iter().filter(|r| r.owner == Owner::Rust) {
            assert!(
                native_for(r).is_some(),
                "{} is owner = rust but not implemented",
                r.path
            );
        }
        assert!(ROUTES.iter().any(|r| r.catch_all && r.owner == Owner::Rust));
    }

    #[test]
    fn node_routes_have_no_native_handler() {
        for r in ROUTES.iter().filter(|r| r.owner == Owner::Node) {
            assert_eq!(dispatch("GET", r.path), None, "{}", r.path);
        }
    }

    #[test]
    fn dispatch_decisions() {
        assert_eq!(dispatch("GET", "/api/ready"), Some(Native::Ready));
        assert_eq!(dispatch("HEAD", "/api/ready"), None);
        assert_eq!(dispatch("POST", "/api/ready"), None);
        assert_eq!(dispatch("GET", "/api/health"), None);
        assert_eq!(dispatch("GET", "/api/not-listed/at/all"), None);
        assert_eq!(dispatch("GET", "/api"), None);
        assert_eq!(dispatch("GET", "/"), Some(Native::Static));
        assert_eq!(dispatch("HEAD", "/assets/a.js"), Some(Native::Static));
        assert_eq!(dispatch("GET", "/c/abc"), Some(Native::Static));
        assert_eq!(dispatch("POST", "/c/abc"), None);
        assert_eq!(dispatch("GET", "/about"), None);
        assert_eq!(dispatch("GET", "/privacy"), None);
        assert_eq!(dispatch("GET", "/device"), None);
        assert_eq!(dispatch("GET", "/device/"), None);
        assert_eq!(dispatch("GET", "/.well-known/webauthn"), None);
        assert_eq!(dispatch("GET", "/api/projects/p1/chats"), None);
    }

    #[test]
    fn most_specific_pattern_wins() {
        assert_eq!(pattern_matches("/api/x/{id}", "/api/x/1"), Some(3));
        assert_eq!(pattern_matches("/api/x/new", "/api/x/new"), Some(4));
        assert_eq!(pattern_matches("/api/x/{id}", "/api/x/"), None);
        assert_eq!(pattern_matches("/api/m{*?}", "/api/m"), Some(3));
    }
}
