//! Route ownership, from contracts/http/routes.toml (compiled in by build.rs). One owner per
//! route: a request whose route is owner = "rust" (for one of its methods) is answered natively,
//! everything else goes to Node through the legacy proxy. A route with a `switch` is Rust's only
//! while that deployment switch is on (M3: NOEVIA_RUST_AUTH, M4: NOEVIA_RUST_PROJECTS, see
//! [`Switches`]); off, it is Node's, so merging a flip changes nothing until the deployment sets
//! the switch.

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
    /// The deployment switch `owner = "rust"` depends on, if any.
    pub switch: Option<&'static str>,
}

/// The deployment switches that are on in this process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Switches {
    /// NOEVIA_RUST_AUTH=1: Rust owns sign-in and the account (M3).
    pub rust_auth: bool,
    /// NOEVIA_RUST_PROJECTS=1 (only with rust_auth, config.rs): Rust owns the project routes
    /// ported so far (M4).
    pub rust_projects: bool,
    pub rust_usage: bool,
    pub rust_sampling_settings: bool,
    pub rust_reasoning_settings: bool,
    pub rust_routing_mode: bool,
    /// NOEVIA_RUST_ROUTING_DEFAULT=1 (M6, only with rust_auth, config.rs): GET /api/routing-default.
    pub rust_routing_default: bool,
}

impl Switches {
    pub fn on(&self, name: &str) -> bool {
        match name {
            "NOEVIA_RUST_AUTH" => self.rust_auth,
            "NOEVIA_RUST_PROJECTS" => self.rust_projects,
            "NOEVIA_RUST_USAGE" => self.rust_usage,
            "NOEVIA_RUST_SAMPLING_SETTINGS" => self.rust_sampling_settings,
            "NOEVIA_RUST_REASONING_SETTINGS" => self.rust_reasoning_settings,
            "NOEVIA_RUST_ROUTING_MODE" => self.rust_routing_mode,
            "NOEVIA_RUST_ROUTING_DEFAULT" => self.rust_routing_default,
            _ => false,
        }
    }
}

impl Route {
    /// The owner in effect under `switches`.
    pub fn owner_under(&self, switches: Switches) -> Owner {
        match (self.owner, self.switch) {
            (Owner::Rust, Some(sw)) if !switches.on(sw) => Owner::Node,
            (owner, _) => owner,
        }
    }
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
    /// Sign-in and the account (M3, NOEVIA_RUST_AUTH): crates/server-account.
    Account,
    /// The project routes ported so far (M4, NOEVIA_RUST_PROJECTS): crates/server-projects
    /// `assets` (a project's images), `chats` (its chat list) and `free_chats` (the free-chat list).
    Projects,
    Usage,
    SamplingSettings,
    ReasoningSettings,
    RoutingMode,
    /// GET /api/routing-default (M6, NOEVIA_RUST_ROUTING_DEFAULT): crates/server-routing-default.
    RoutingDefault,
}

/// The route [`Native::RoutingDefault`] implements (GET only; routes.toml).
const ROUTING_DEFAULT: &str = "/api/routing-default";

/// The routes [`Native::Projects`] implements.
const PROJECT_ROUTES: &[&str] = &[
    "/api/projects/{id}/assets",
    "/api/projects/{id}/assets/{assetId}",
    PROJECT_CHATS,
    FREE_CHATS,
];

/// A project's chat list: the second M4 slice. It is answered here only in a build with the cargo
/// feature `rust-project-chats`, which `--features` then lists, so the supervisor can confirm the
/// slice to Node on its own (core rust-projects.cjs chatsEnabledFrom, sbstndalton/noevia#1409).
/// Without it the route stays Node's under NOEVIA_RUST_PROJECTS too, as in a front that answers
/// only the images.
const PROJECT_CHATS: &str = "/api/projects/{projectId}/chats";

/// The free-chat list: the third M4 slice, answered here only in a build with the cargo feature
/// `rust-free-chats` (listed by `--features`; the supervisor confirms it to Node on its own, core
/// rust-projects.cjs freeChatsEnabledFrom). Without it the route stays Node's under the switch.
/// `DELETE /api/freechats/{chatId}` is Node's in every build (routes.toml).
const FREE_CHATS: &str = "/api/freechats";

/// Whether this build answers `route` itself when its switch is on.
fn compiled(route: &Route) -> bool {
    (route.path != PROJECT_CHATS || cfg!(feature = "rust-project-chats"))
        && (route.path != FREE_CHATS || cfg!(feature = "rust-free-chats"))
        && (route.path != ROUTING_DEFAULT || cfg!(feature = "rust-routing-default"))
}

/// The native handler a rust-owned route is implemented by.
pub fn native_for(route: &Route) -> Option<Native> {
    match (route.path, route.catch_all, route.switch) {
        (
            "/api/routing-mode" | "/api/routing-mode/allowed",
            false,
            Some("NOEVIA_RUST_ROUTING_MODE"),
        ) => Some(Native::RoutingMode),
        ("/api/ready", false, None) => Some(Native::Ready),
        (_, true, None) => Some(Native::Static),
        (_, false, Some("NOEVIA_RUST_AUTH")) if route.path.starts_with("/api/") => {
            Some(Native::Account)
        }
        (p, false, Some("NOEVIA_RUST_PROJECTS")) if PROJECT_ROUTES.contains(&p) => {
            Some(Native::Projects)
        }
        (ROUTING_DEFAULT, false, Some("NOEVIA_RUST_ROUTING_DEFAULT")) => {
            Some(Native::RoutingDefault)
        }
        ("/api/reasoning-settings", false, Some("NOEVIA_RUST_REASONING_SETTINGS")) => {
            Some(Native::ReasoningSettings)
        }
        ("/api/sampling-settings", false, Some("NOEVIA_RUST_SAMPLING_SETTINGS")) => {
            Some(Native::SamplingSettings)
        }
        ("/api/usage" | "/api/usage/aggregate", false, Some("NOEVIA_RUST_USAGE")) => {
            Some(Native::Usage)
        }
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

/// What answers `method path` with every switch off: a native handler, or `None` for the
/// legacy proxy.
pub fn dispatch(method: &str, path: &str) -> Option<Native> {
    dispatch_under(method, path, Switches::default())
}

/// [`dispatch`] under the deployment switches that are on.
pub fn dispatch_under(method: &str, path: &str, switches: Switches) -> Option<Native> {
    let route = route_for(path)?;
    if route.owner_under(switches) != Owner::Rust {
        return None;
    }
    if !route.methods.is_empty() && !route.methods.contains(&method) {
        return None;
    }
    if !compiled(route) {
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
        // A switched route is Node's while its switch is off.
        for r in ROUTES.iter().filter(|r| r.switch.is_some()) {
            assert_eq!(
                r.owner_under(Switches::default()),
                Owner::Node,
                "{}",
                r.path
            );
            for m in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
                assert_eq!(dispatch(m, r.path), None, "{m} {}", r.path);
            }
        }
    }

    #[test]
    fn switches_by_name() {
        let on = Switches {
            rust_auth: true,
            rust_projects: false,
            rust_usage: false,
            rust_sampling_settings: false,
            rust_reasoning_settings: false,
            rust_routing_mode: false,
            rust_routing_default: false,
        };
        assert!(on.on("NOEVIA_RUST_AUTH"));
        assert!(!on.on("NOEVIA_RUST_PROJECTS"));
        assert!(!Switches::default().on("NOEVIA_RUST_AUTH"));
        assert!(!on.on("NOEVIA_SOMETHING_ELSE"));
        let both = Switches {
            rust_auth: true,
            rust_projects: true,
            rust_usage: false,
            rust_sampling_settings: false,
            rust_reasoning_settings: false,
            rust_routing_mode: false,
            rust_routing_default: false,
        };
        assert!(both.on("NOEVIA_RUST_PROJECTS"));
    }

    #[test]
    fn project_images_are_rusts_only_under_their_switch_and_methods() {
        let auth = Switches {
            rust_auth: true,
            rust_projects: false,
            rust_usage: false,
            rust_sampling_settings: false,
            rust_reasoning_settings: false,
            rust_routing_mode: false,
            rust_routing_default: false,
        };
        let both = Switches {
            rust_auth: true,
            rust_projects: true,
            rust_usage: false,
            rust_sampling_settings: false,
            rust_reasoning_settings: false,
            rust_routing_mode: false,
            rust_routing_default: false,
        };
        for (m, p) in [
            ("POST", "/api/projects/p1/assets"),
            ("GET", "/api/projects/p1/assets/img-1"),
            ("DELETE", "/api/projects/p1/assets/img-1"),
        ] {
            assert_eq!(dispatch(m, p), None, "{m} {p}");
            assert_eq!(dispatch_under(m, p, auth), None, "{m} {p}");
            assert_eq!(
                dispatch_under(m, p, both),
                Some(Native::Projects),
                "{m} {p}"
            );
        }
        for (m, p) in [
            ("GET", "/api/projects/p1/assets"),
            ("PUT", "/api/projects/p1/assets/img-1"),
            ("HEAD", "/api/projects/p1/assets/img-1"),
            ("POST", "/api/projects/p1/assets/img-1"),
            ("POST", "/api/projects/p1/upload"),
            ("DELETE", "/api/projects/p1"),
        ] {
            assert_eq!(dispatch_under(m, p, both), None, "{m} {p}");
        }
    }

    #[test]
    fn a_projects_chat_list_is_rusts_only_under_the_switch_for_get_and_post() {
        let auth = Switches {
            rust_auth: true,
            ..Default::default()
        };
        let both = Switches {
            rust_auth: true,
            rust_projects: true,
            ..Default::default()
        };
        // Only a build with the chat-list slice answers it (#1409); without, it stays Node's.
        let answered = cfg!(feature = "rust-project-chats").then_some(Native::Projects);
        assert_eq!(
            crate::FEATURES.contains(&"rust-project-chats"),
            answered.is_some()
        );
        for m in ["GET", "POST"] {
            let p = "/api/projects/p1/chats";
            assert_eq!(dispatch(m, p), None, "{m}");
            assert_eq!(dispatch_under(m, p, auth), None, "{m}");
            assert_eq!(dispatch_under(m, p, both), answered, "{m}");
        }
        // The images do not depend on it.
        assert_eq!(
            dispatch_under("POST", "/api/projects/p1/assets", both),
            Some(Native::Projects)
        );
        // Node keeps every other method, the chat routes below the list, and the project itself.
        for (m, p) in [
            ("PUT", "/api/projects/p1/chats"),
            ("DELETE", "/api/projects/p1/chats"),
            ("HEAD", "/api/projects/p1/chats"),
            ("GET", "/api/projects/p1/chats/c1"),
            ("DELETE", "/api/projects/p1/chats/c1"),
            ("POST", "/api/projects"),
            ("POST", "/api/projects/p1/config"),
            ("DELETE", "/api/projects/p1"),
        ] {
            assert_eq!(dispatch_under(m, p, both), None, "{m} {p}");
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
    #[test]
    fn usage_owns_all_methods_only_when_switched_on() {
        let on = Switches {
            rust_auth: true,
            rust_usage: true,
            rust_sampling_settings: false,
            rust_reasoning_settings: false,
            rust_routing_mode: false,
            rust_routing_default: false,
            ..Default::default()
        };
        for p in ["/api/usage", "/api/usage/aggregate"] {
            for m in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
                assert_eq!(dispatch(m, p), None);
                assert_eq!(dispatch_under(m, p, on), Some(Native::Usage));
            }
        }
    }
    #[test]
    fn sampling_owns_all_methods_only_when_switched_on() {
        let on = Switches {
            rust_auth: true,
            rust_sampling_settings: true,
            ..Default::default()
        };
        for m in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
            assert_eq!(dispatch(m, "/api/sampling-settings"), None);
            assert_eq!(
                dispatch_under(m, "/api/sampling-settings", on),
                Some(Native::SamplingSettings)
            );
        }
    }
    #[test]
    fn reasoning_owns_only_put_with_dedicated_selector() {
        let on = Switches {
            rust_auth: true,
            rust_reasoning_settings: true,
            ..Default::default()
        };
        for m in ["GET", "HEAD", "POST", "PATCH", "DELETE", "OPTIONS"] {
            assert_eq!(dispatch_under(m, "/api/reasoning-settings", on), None);
        }
        assert_eq!(dispatch("PUT", "/api/reasoning-settings"), None);
        assert_eq!(
            dispatch_under(
                "PUT",
                "/api/reasoning-settings",
                Switches {
                    rust_auth: true,
                    ..Default::default()
                }
            ),
            None
        );
        assert_eq!(
            dispatch_under("PUT", "/api/reasoning-settings", on),
            Some(Native::ReasoningSettings)
        );
    }

    #[test]
    fn the_free_chat_list_is_rusts_only_under_the_switch_for_get_and_post() {
        let auth = Switches {
            rust_auth: true,
            ..Default::default()
        };
        let both = Switches {
            rust_auth: true,
            rust_projects: true,
            ..Default::default()
        };
        // Only a build with the free-chat slice answers it; without, it stays Node's.
        let answered = cfg!(feature = "rust-free-chats").then_some(Native::Projects);
        assert_eq!(
            crate::FEATURES.contains(&"rust-free-chats"),
            answered.is_some()
        );
        for m in ["GET", "POST"] {
            assert_eq!(dispatch(m, "/api/freechats"), None, "{m}");
            assert_eq!(dispatch_under(m, "/api/freechats", auth), None, "{m}");
            assert_eq!(dispatch_under(m, "/api/freechats", both), answered, "{m}");
        }
        // Independent of the project chat-list slice.
        assert_eq!(
            dispatch_under("GET", "/api/projects/p1/chats", both),
            cfg!(feature = "rust-project-chats").then_some(Native::Projects)
        );
        // Node keeps a free chat's delete (transcript, attachments project), every other method,
        // and the paths around it.
        for (m, p) in [
            ("DELETE", "/api/freechats/c1"),
            ("GET", "/api/freechats/c1"),
            ("POST", "/api/freechats/c1"),
            ("PUT", "/api/freechats"),
            ("DELETE", "/api/freechats"),
            ("HEAD", "/api/freechats"),
            ("POST", "/api/chats/c1/move"),
            ("GET", "/api/workspace"),
        ] {
            assert_eq!(dispatch_under(m, p, both), None, "{m} {p}");
        }
    }
}
