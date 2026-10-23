//! Route ownership, from contracts/http/routes.toml (compiled in by build.rs). One owner per
//! route: a request whose route is owner = "rust" (for one of its methods) is answered natively,
//! everything else goes to Node through the legacy proxy. Native ownership is permanent
//! (#1071); stale deployment switches are rejected by the build schema.

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

/// The deployment switches that are on in this process. Sign-in, the account, the front read
/// routes, the M4 project routes and the export have none (sbstndalton/noevia#1071): they are
/// always Rust's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Switches {}

impl Switches {
    pub fn on(&self, _name: &str) -> bool {
        false
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
    /// Sign-in and the account (M3, permanent since #1071): crates/server-account.
    Account,
    /// Three live, read-only address GETs; Node retains the POST writers.
    WebAddress,
    /// The project routes ported so far (M4, permanent since #1071): crates/server-projects
    /// `assets` (a project's images), `chats` (its chat list) and `free_chats` (the free-chat list).
    Projects,
    Usage,
    SamplingSettings,
    ReasoningSettings,
    RoutingMode,
    StorageMetadata,
    /// GET /api/integrations/storage/files[/path], POST /api/integrations/storage/file (M4, #1368,
    /// NOEVIA_RUST_STORAGE_BROWSE): the outbound list and text read.
    StorageBrowse,
    /// GET /api/routing-default (M6, NOEVIA_RUST_ROUTING_DEFAULT): crates/server-routing-default.
    RoutingDefault,
    /// GET /api/stats (M6, NOEVIA_RUST_STATS): crates/server-stats.
    Stats,
    /// GET /api/export/conversations (M4, NOEVIA_RUST_EXPORT): crates/server-export.
    Export,
    /// GET /api/providers/chatgpt (M6, NOEVIA_RUST_CHATGPT_STATUS): crates/server-chatgpt-status.
    ChatgptStatus,
}

/// The route [`Native::RoutingDefault`] implements (GET only; routes.toml).
const ROUTING_DEFAULT: &str = "/api/routing-default";

/// The route [`Native::Stats`] implements (GET only; routes.toml), answered only in a build with
/// the cargo feature `rust-stats` (listed by `--features`, so the supervisor confirms it to Node).
const STATS: &str = "/api/stats";

/// The route [`Native::Export`] implements (GET only; routes.toml), answered only in a build with
/// the cargo feature `rust-export` (listed by `--features`, so the supervisor confirms it to Node).
const EXPORT: &str = "/api/export/conversations";

/// The route [`Native::ChatgptStatus`] implements (GET only; routes.toml), answered only in a build
/// with the cargo feature `rust-chatgpt-status` (listed by `--features`, so the supervisor confirms
/// it to Node). Every sibling (`/device`, `/device/poll`, `/device/cancel`, `/models`) is Node's.
const CHATGPT_STATUS: &str = "/api/providers/chatgpt";

/// The routes [`Native::Projects`] implements.
const PROJECT_ROUTES: &[&str] = &[
    "/api/projects/{id}/assets",
    "/api/projects/{id}/assets/{assetId}",
    PROJECT_CHATS,
    FREE_CHATS,
    CHAT_CONTEXT_WINDOW,
    CHAT_HISTORY,
    FREE_CHAT,
    PROJECT_CHAT,
];

/// Settings → Data → Delete old chats, its preview (GET only; routes.toml): a read of the chat
/// lists, answered by crates/server-projects `retention` (sbstndalton/noevia#1071). `PUT`, which
/// deletes chats, is Node's.
const ACCOUNT_RETENTION: &str = "/api/account/retention";

/// A chat's last context meter (GET only; routes.toml): a read, always answered here
/// (sbstndalton/noevia#1071); Node refuses it.
const CHAT_CONTEXT_WINDOW: &str = "/api/chats/{chatId}/context-window";

/// A chat's transcript and interrupted partial reply (GET, slice 2) and its save (POST, slice 5;
/// routes.toml), always answered here (sbstndalton/noevia#1071); Node refuses both.
const CHAT_HISTORY: &str = "/api/chats/{chatId}/history";

/// One chat's delete (DELETE only; routes.toml), the whole cascade one Rust store operation
/// (crates/server-projects `chat_delete`, sbstndalton/noevia#1071 slice 6); Node refuses both.
const FREE_CHAT: &str = "/api/freechats/{chatId}";
const PROJECT_CHAT: &str = "/api/projects/{projectId}/chats/{chatId}";

/// A project's chat list: the second M4 slice, answered here only in a build with the cargo
/// feature `rust-project-chats` (listed by `--features`). Node no longer answers it
/// (sbstndalton/noevia#1071), so config.rs refuses to start a build without it.
const PROJECT_CHATS: &str = "/api/projects/{projectId}/chats";

/// The free-chat list: the third M4 slice, answered here only in a build with the cargo feature
/// `rust-free-chats` (listed by `--features`; config.rs refuses to start a build without it).
/// `DELETE /api/freechats/{chatId}` is [`FREE_CHAT`].
const FREE_CHATS: &str = "/api/freechats";

/// GET/POST chat context, permanently native (#1071), requiring the compiled
/// `rust-project-writes` capability. This does not port other project writers.
const PROJECT_WRITES: &[&str] = &[CHAT_CONTEXT];
const CHAT_CONTEXT: &str = "/api/chats/{chatId}/context";

/// The route [`Native::StorageMetadata`] implements (GET only; routes.toml), answered only in a
/// build with the cargo feature `rust-storage-reads` under NOEVIA_RUST_STORAGE_READS (#1368). The
/// list (`/files`) and text read (`/file`) routes stay Node's in every build until a bounded
/// transport adapter is qualified.
const STORAGE_METADATA: &str = "/api/integrations/storage";

/// The routes [`Native::StorageBrowse`] implements, in a build with the cargo feature
/// `rust-storage-browse` under NOEVIA_RUST_STORAGE_BROWSE (#1368). Node's handler matches
/// `^/api/integrations/storage/files(?:/(.*))?$` for GET and `.../file` exactly for POST.
const STORAGE_FILES: &str = "/api/integrations/storage/files";
const STORAGE_FILES_ROUTE: &str = "/api/integrations/storage/files{*?}";
const STORAGE_FILE: &str = "/api/integrations/storage/file";

/// The storage browse decision, before the generic table: the list route takes any suffix, which
/// the route patterns cannot express. `None` when `path` is not a browse path.
fn storage_browse(method: &str, path: &str) -> Option<Option<Native>> {
    let files = path == STORAGE_FILES
        || path
            .strip_prefix(STORAGE_FILES)
            .is_some_and(|rest| rest.starts_with('/'));
    let file = path == STORAGE_FILE;
    if !files && !file {
        return None;
    }
    let owned = cfg!(feature = "rust-storage-browse")
        && ((files && method == "GET") || (file && method == "POST"));
    Some(owned.then_some(Native::StorageBrowse))
}

/// Whether this build answers `route` itself (when its switch, if any, is on).
fn compiled(route: &Route) -> bool {
    (route.path != PROJECT_CHATS || cfg!(feature = "rust-project-chats"))
        && (route.path != FREE_CHATS || cfg!(feature = "rust-free-chats"))
        && (!PROJECT_WRITES.contains(&route.path) || cfg!(feature = "rust-project-writes"))
        && (route.path != ROUTING_DEFAULT || cfg!(feature = "rust-routing-default"))
        && (route.path != STATS || cfg!(feature = "rust-stats"))
        && (route.path != STORAGE_METADATA || cfg!(feature = "rust-storage-reads"))
        && (![STORAGE_FILE, STORAGE_FILES_ROUTE].contains(&route.path)
            || cfg!(feature = "rust-storage-browse"))
        && (route.path != EXPORT || cfg!(feature = "rust-export"))
        && (route.path != CHATGPT_STATUS || cfg!(feature = "rust-chatgpt-status"))
}

/// The native handler a rust-owned route is implemented by. The front read routes and the export
/// are matched on their path first: like the account and project routes they have no switch
/// (#1071).
pub fn native_for(route: &Route) -> Option<Native> {
    if route.catch_all {
        return (route.switch.is_none()).then_some(Native::Static);
    }
    let auth = route.switch.is_none();
    match route.path {
        STORAGE_FILE | STORAGE_FILES_ROUTE if auth => return Some(Native::StorageBrowse),
        STORAGE_METADATA if auth => return Some(Native::StorageMetadata),
        "/api/routing-mode" | "/api/routing-mode/allowed" if auth => {
            return Some(Native::RoutingMode)
        }
        ROUTING_DEFAULT if auth => return Some(Native::RoutingDefault),
        STATS if auth => return Some(Native::Stats),
        CHATGPT_STATUS if auth => return Some(Native::ChatgptStatus),
        "/api/reasoning-settings" if auth => return Some(Native::ReasoningSettings),
        "/api/sampling-settings" if auth => return Some(Native::SamplingSettings),
        "/api/usage" | "/api/usage/aggregate" if auth => return Some(Native::Usage),
        EXPORT if auth => return Some(Native::Export),
        "/api/instance"
        | "/.well-known/webauthn"
        | "/api/admin/web-address"
        | "/api/admin/web-address/remove"
            if auth =>
        {
            return Some(Native::WebAddress)
        }
        // Under /api/account/ but not the account crate's: it reads the chat lists.
        ACCOUNT_RETENTION if auth => return Some(Native::Projects),
        _ => {}
    }
    match (route.path, route.switch) {
        ("/api/ready", None) => Some(Native::Ready),
        (p, None) if is_account_path(p) => Some(Native::Account),
        (p, None) if PROJECT_ROUTES.contains(&p) => Some(Native::Projects),
        (p, None) if PROJECT_WRITES.contains(&p) => Some(Native::Projects),
        _ => None,
    }
}

/// The unswitched API routes crates/server-account answers (sign-in, setup, profile, account files
/// and admin). An unswitched Rust route outside these prefixes must be mapped explicitly above, so a
/// new one never falls through to the account handler (sbstndalton/noevia#1932).
const ACCOUNT_PREFIXES: &[&str] = &[
    "/api/auth/",
    "/api/account/",
    "/api/admin/",
    "/api/setup/",
    "/api/profile",
];

fn is_account_path(path: &str) -> bool {
    ACCOUNT_PREFIXES.iter().any(|p| {
        path.strip_prefix(p)
            .is_some_and(|rest| p.ends_with('/') || rest.is_empty() || rest.starts_with('/'))
    })
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

/// What answers `method path` with every deployment switch off (sign-in, the account, the front
/// read routes, the project routes and the export are Rust's regardless): a native handler, or `None` for the legacy proxy.
pub fn dispatch(method: &str, path: &str) -> Option<Native> {
    dispatch_under(method, path, Switches::default())
}

/// [`dispatch`] under the deployment switches that are on.
pub fn dispatch_under(method: &str, path: &str, switches: Switches) -> Option<Native> {
    if let Some(decision) = storage_browse(method, path) {
        return decision;
    }
    let route = route_for(path)?;
    if route.path == STORAGE_METADATA && (path != STORAGE_METADATA || method != "GET") {
        return None;
    }
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

    /// sbstndalton/noevia#1932: every unswitched Rust-owned /api/ route is either an account route
    /// (served by the account handler) or mapped to its own handler, never the account fallback.
    #[test]
    fn unswitched_rust_api_routes_are_account_or_explicitly_mapped() {
        let mut account = 0;
        for r in ROUTES.iter().filter(|r| {
            r.owner == Owner::Rust
                && r.switch.is_none()
                && !r.catch_all
                && r.path.starts_with("/api/")
        }) {
            match native_for(r) {
                Some(Native::Account) => {
                    assert!(
                        is_account_path(r.path),
                        "{} falls through to Native::Account",
                        r.path
                    );
                    account += 1;
                }
                // The retention preview reads the chat lists, so the project handler answers it.
                Some(Native::Projects) if r.path == ACCOUNT_RETENTION => {}
                Some(Native::WebAddress)
                    if r.path == "/api/admin/web-address"
                        || r.path == "/api/admin/web-address/remove" => {}
                Some(_) => assert!(!is_account_path(r.path), "{} is an account path", r.path),
                other @ None => assert!(
                    other.is_some(),
                    "{} is owner = rust with no switch but has no handler",
                    r.path
                ),
            }
        }
        assert!(account > 0);
        assert!(!is_account_path("/api/profiles"));
        assert!(!is_account_path("/api/accounting"));
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
        let off = Switches::default();
        let on = Switches::default();
        // Retired (#1071): the names mean nothing, sign-in and the projects are Rust's regardless.
        for s in [off, on] {
            assert!(!s.on("NOEVIA_RUST_PROJECTS"));
            assert!(!s.on("NOEVIA_RUST_AUTH"));
            assert!(!s.on("NOEVIA_SOMETHING_ELSE"));
        }
        assert!(!off.on("NOEVIA_RUST_PROJECT_WRITES"));
        assert!(!on.on("NOEVIA_RUST_PROJECT_WRITES"));
    }

    // #1071: NOEVIA_RUST_PROJECTS is retired; the project routes are Rust's with no switch.
    #[test]
    fn project_images_are_always_rusts_for_their_methods() {
        let both = Switches::default();
        for (m, p) in [
            ("POST", "/api/projects/p1/assets"),
            ("GET", "/api/projects/p1/assets/img-1"),
            ("DELETE", "/api/projects/p1/assets/img-1"),
        ] {
            assert_eq!(dispatch(m, p), Some(Native::Projects), "{m} {p}");
            assert_eq!(
                dispatch_under(m, p, Switches::default()),
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
    fn a_projects_chat_list_is_always_rusts_for_get_and_post() {
        let both = Switches::default();
        // Only a build with the chat-list slice answers it (#1409); config.rs refuses to start one
        // without it, since Node no longer answers it (#1071).
        let answered = cfg!(feature = "rust-project-chats").then_some(Native::Projects);
        assert_eq!(
            crate::FEATURES.contains(&"rust-project-chats"),
            answered.is_some()
        );
        for m in ["GET", "POST"] {
            let p = "/api/projects/p1/chats";
            assert_eq!(dispatch(m, p), answered, "{m}");
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
            ("PATCH", "/api/projects/p1/chats/c1"),
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
        for path in [
            "/api/instance",
            "/.well-known/webauthn",
            "/api/admin/web-address",
        ] {
            assert_eq!(dispatch("GET", path), Some(Native::WebAddress), "{path}");
            for method in ["HEAD", "PUT", "DELETE", "OPTIONS"] {
                assert_eq!(dispatch(method, path), None, "{method} {path}");
            }
        }
        assert_eq!(dispatch("POST", "/api/instance"), None);
        assert_eq!(dispatch("POST", "/.well-known/webauthn"), None);
        for path in ["/api/admin/web-address", "/api/admin/web-address/remove"] {
            assert_eq!(dispatch("POST", path), Some(Native::WebAddress), "{path}");
            assert_eq!(dispatch("HEAD", path), None);
            assert_eq!(
                dispatch("GET", path),
                (path == "/api/admin/web-address").then_some(Native::WebAddress)
            );
        }
        assert_eq!(
            dispatch("GET", "/api/projects/p1/chats"),
            cfg!(feature = "rust-project-chats").then_some(Native::Projects)
        );
    }

    // #1071: sign-in and the account are always Rust's for their listed methods, with no switch;
    // a retired NOEVIA_RUST_AUTH value is never consulted, and the methods Rust does not list
    // (and account deletion) stay Node's.
    #[test]
    fn account_routes_are_unconditional() {
        for r in ROUTES.iter() {
            assert_ne!(r.switch, Some("NOEVIA_RUST_AUTH"), "{}", r.path);
        }
        for (m, p) in [
            ("GET", "/api/setup/status"),
            ("POST", "/api/setup/complete"),
            ("POST", "/api/auth/login/password"),
            ("POST", "/api/auth/login/passkey/options"),
            ("POST", "/api/auth/logout"),
            ("GET", "/api/auth/session"),
            ("DELETE", "/api/auth/sessions/s1"),
            ("PATCH", "/api/auth/passkeys/k1"),
            ("POST", "/api/auth/device/token"),
            ("GET", "/api/auth/devices"),
            ("POST", "/api/auth/invitations/accept"),
            ("POST", "/api/auth/recovery/complete"),
            ("GET", "/api/profile"),
            ("PATCH", "/api/profile"),
            ("POST", "/api/profile/app-passwords"),
            (
                "DELETE",
                "/api/profile/app-passwords/0123456789abcdef0123456789abcdef",
            ),
            ("PUT", "/api/profile/appearance"),
            ("PUT", "/api/profile/features"),
            ("POST", "/api/profile/onboarding"),
            ("GET", "/api/profile/sharing"),
            ("PUT", "/api/profile/sharing"),
            ("GET", "/api/account/memory"),
            ("PUT", "/api/account/preferences"),
            ("GET", "/api/admin/users"),
            ("POST", "/api/admin/invitations"),
            ("PUT", "/api/admin/users/u1/disabled"),
            ("POST", "/api/admin/users/u1/recovery"),
        ] {
            assert_eq!(dispatch(m, p), Some(Native::Account), "{m} {p}");
        }
        for (m, p) in [
            ("DELETE", "/api/admin/users/u1"),
            ("HEAD", "/api/admin/web-address"),
            ("POST", "/api/profile/sharing"),
            ("PUT", "/api/account/retention"),
            ("HEAD", "/api/auth/session"),
            ("GET", "/api/auth/login/password"),
            ("GET", "/api/profile/features"),
        ] {
            assert_eq!(dispatch(m, p), None, "{m} {p}");
        }
    }

    #[test]
    fn most_specific_pattern_wins() {
        assert_eq!(pattern_matches("/api/x/{id}", "/api/x/1"), Some(3));
        assert_eq!(pattern_matches("/api/x/new", "/api/x/new"), Some(4));
        assert_eq!(pattern_matches("/api/x/{id}", "/api/x/"), None);
        assert_eq!(pattern_matches("/api/m{*?}", "/api/m"), Some(3));
    }
    #[test]
    fn usage_owns_all_methods_always() {
        let on = Switches::default();
        for p in ["/api/usage", "/api/usage/aggregate"] {
            for m in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
                assert_eq!(dispatch_under(m, p, on), Some(Native::Usage));
            }
        }
    }
    #[test]
    fn sampling_owns_all_methods_always() {
        let on = Switches::default();
        for m in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
            assert_eq!(
                dispatch_under(m, "/api/sampling-settings", on),
                Some(Native::SamplingSettings)
            );
        }
    }
    #[test]
    fn reasoning_owns_only_put_always() {
        let on = Switches::default();
        for m in ["GET", "HEAD", "POST", "PATCH", "DELETE", "OPTIONS"] {
            assert_eq!(dispatch_under(m, "/api/reasoning-settings", on), None);
        }
        assert_eq!(
            dispatch_under("PUT", "/api/reasoning-settings", on),
            Some(Native::ReasoningSettings)
        );
    }

    // #1071: only these two context methods are permanently native.
    #[test]
    fn context_routes_are_permanently_rusts_without_broadening_other_writes() {
        let projects = Switches::default();
        let on = Switches::default();
        let answered = cfg!(feature = "rust-project-writes").then_some(Native::Projects);
        assert_eq!(
            crate::FEATURES.contains(&"rust-project-writes"),
            answered.is_some()
        );
        for m in ["GET", "POST"] {
            let p = "/api/chats/c1/context";
            assert_eq!(dispatch(m, p), answered, "{m}");
            assert_eq!(dispatch_under(m, p, projects), answered, "{m}: permanent");
            assert_eq!(dispatch_under(m, p, on), answered, "{m}: on = Rust");
        }
        for (m, p) in [
            ("PUT", "/api/chats/c1/context"),
            ("DELETE", "/api/chats/c1/context"),
            ("PUT", "/api/chats/c1/history"),
            ("POST", "/api/chats/c1/move"),
            ("GET", "/api/diary/context"),
        ] {
            assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
        }
        assert!(!on.on("NOEVIA_RUST_PROJECT_WRITES"));
        assert!(!projects.on("NOEVIA_RUST_PROJECT_WRITES"));
    }

    #[test]
    fn the_free_chat_list_is_always_rusts_for_get_and_post() {
        let both = Switches::default();
        // Only a build with the free-chat slice answers it; config.rs refuses to start one without
        // it (#1071).
        let answered = cfg!(feature = "rust-free-chats").then_some(Native::Projects);
        assert_eq!(
            crate::FEATURES.contains(&"rust-free-chats"),
            answered.is_some()
        );
        for m in ["GET", "POST"] {
            assert_eq!(dispatch(m, "/api/freechats"), answered, "{m}");
        }
        // Independent of the project chat-list slice.
        assert_eq!(
            dispatch_under("GET", "/api/projects/p1/chats", both),
            cfg!(feature = "rust-project-chats").then_some(Native::Projects)
        );
        // Node keeps every other method and the paths around it.
        for (m, p) in [
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

    /// sbstndalton/noevia#1071 slice 6: one chat's delete is the front's, DELETE only.
    #[test]
    fn a_chats_delete_is_rusts_for_delete_only() {
        let off = Switches::default();
        for p in ["/api/freechats/c1", "/api/projects/p1/chats/c1"] {
            assert_eq!(
                dispatch_under("DELETE", p, off),
                Some(Native::Projects),
                "{p}"
            );
            for m in ["GET", "POST", "PUT", "PATCH", "HEAD"] {
                assert_eq!(dispatch_under(m, p, off), None, "{m} {p}");
            }
        }
    }

    #[test]
    fn a_chats_context_window_is_always_rusts_for_get_only() {
        let off = Switches::default();
        assert_eq!(
            dispatch_under("GET", "/api/chats/c1/context-window", off),
            Some(Native::Projects)
        );
        // Node keeps every other method and the chat routes around it (#1071: a chat's move is
        // still Node's).
        for (m, p) in [
            ("POST", "/api/chats/c1/context-window"),
            ("DELETE", "/api/chats/c1/context-window"),
            ("POST", "/api/chats/c1/move"),
        ] {
            assert_eq!(dispatch_under(m, p, off), None, "{m} {p}");
        }
    }

    #[test]
    fn a_chats_history_is_rusts_for_get_and_post() {
        let off = Switches::default();
        // The read (slice 2) and the save with its revision compare-and-swap (slice 5, #1071).
        for m in ["GET", "POST"] {
            assert_eq!(
                dispatch_under(m, "/api/chats/c1/history", off),
                Some(Native::Projects),
                "{m}"
            );
        }
        // Every other method stays Node's.
        for (m, p) in [
            ("PUT", "/api/chats/c1/history"),
            ("DELETE", "/api/chats/c1/history"),
            ("POST", "/api/chats/c1/move"),
        ] {
            assert_eq!(dispatch_under(m, p, off), None, "{m} {p}");
        }
    }

    #[test]
    fn the_retention_preview_is_rusts_for_get_only() {
        let off = Switches::default();
        assert_eq!(
            dispatch_under("GET", "/api/account/retention", off),
            Some(Native::Projects)
        );
        for m in ["PUT", "POST", "DELETE"] {
            assert_eq!(
                dispatch_under(m, "/api/account/retention", off),
                None,
                "{m}"
            );
        }
    }

    #[test]
    fn stats_owns_only_get_always_in_a_build_with_the_slice() {
        let on = Switches::default();
        let answered = cfg!(feature = "rust-stats").then_some(Native::Stats);
        assert_eq!(crate::FEATURES.contains(&"rust-stats"), answered.is_some());
        assert_eq!(dispatch_under("GET", "/api/stats", on), answered);
        // Node keeps every other method (it answers them all) and the paths around it.
        for (m, p) in [
            ("HEAD", "/api/stats"),
            ("POST", "/api/stats"),
            ("PUT", "/api/stats"),
            ("DELETE", "/api/stats"),
            ("OPTIONS", "/api/stats"),
            ("GET", "/api/stats/x"),
            ("GET", "/api/statsx"),
            ("GET", "/api/models"),
            ("POST", "/api/auto-roles"),
        ] {
            assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
        }
        // The retired switch name means nothing (#1071).
        assert!(!on.on("NOEVIA_RUST_STATS"));
    }

    #[test]
    fn export_owns_only_get_always_in_a_build_with_the_slice() {
        let on = Switches::default();
        let answered = cfg!(feature = "rust-export").then_some(Native::Export);
        assert_eq!(crate::FEATURES.contains(&"rust-export"), answered.is_some());
        assert_eq!(dispatch("GET", "/api/export/conversations"), answered);
        // Node keeps every other method (its 405) and the paths around it. (A trailing slash
        // matches the routes.toml pattern; export_routes.rs forwards it, Node matching exactly.)
        for (m, p) in [
            ("HEAD", "/api/export/conversations"),
            ("POST", "/api/export/conversations"),
            ("PUT", "/api/export/conversations"),
            ("DELETE", "/api/export/conversations"),
            ("OPTIONS", "/api/export/conversations"),
            ("GET", "/api/export/conversationsx"),
            ("GET", "/api/export"),
            ("GET", "/api/diary/workspace-export"),
        ] {
            assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
        }
        assert!(!on.on("NOEVIA_RUST_EXPORT"));
        assert!(!on.on("NOEVIA_RUST_PROJECTS"));
    }

    #[test]
    fn chatgpt_status_owns_only_the_exact_get_always() {
        let on = Switches::default();
        let answered = cfg!(feature = "rust-chatgpt-status").then_some(Native::ChatgptStatus);
        assert_eq!(
            crate::FEATURES.contains(&"rust-chatgpt-status"),
            answered.is_some()
        );
        assert_eq!(
            dispatch_under("GET", "/api/providers/chatgpt", on),
            answered
        );
        // Node keeps every other method (its 405), every sibling and the provider list.
        for (m, p) in [
            ("HEAD", "/api/providers/chatgpt"),
            ("POST", "/api/providers/chatgpt"),
            ("PUT", "/api/providers/chatgpt"),
            ("DELETE", "/api/providers/chatgpt"),
            ("OPTIONS", "/api/providers/chatgpt"),
            ("GET", "/api/providers/chatgpt/models"),
            ("POST", "/api/providers/chatgpt/device"),
            ("POST", "/api/providers/chatgpt/device/poll"),
            ("POST", "/api/providers/chatgpt/device/cancel"),
            ("GET", "/api/providers/chatgptx"),
            ("GET", "/api/providers"),
            ("GET", "/api/providers/chatgpt-oauth/models"),
        ] {
            assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
        }
        assert!(!on.on("NOEVIA_RUST_CHATGPT_STATUS"));
    }

    // #1071: the front read routes are always Rust's, with no switch at all; no route names a
    // retired switch.
    #[test]
    fn front_read_routes_are_unconditional() {
        let auth = Switches::default();
        for r in ROUTES.iter() {
            if let Some(sw) = r.switch {
                assert!(
                    !crate::config::RETIRED_SWITCHES.contains(&sw),
                    "{} names retired {sw}",
                    r.path
                );
            }
        }
        for (m, p, native) in [
            ("GET", "/api/usage", Native::Usage),
            ("GET", "/api/usage/aggregate", Native::Usage),
            ("GET", "/api/sampling-settings", Native::SamplingSettings),
            ("PUT", "/api/reasoning-settings", Native::ReasoningSettings),
            ("GET", "/api/routing-mode", Native::RoutingMode),
            ("GET", "/api/routing-mode/allowed", Native::RoutingMode),
            ("GET", "/api/routing-default", Native::RoutingDefault),
            ("GET", "/api/stats", Native::Stats),
            ("GET", "/api/providers/chatgpt", Native::ChatgptStatus),
            ("GET", "/api/integrations/storage", Native::StorageMetadata),
            (
                "GET",
                "/api/integrations/storage/files",
                Native::StorageBrowse,
            ),
            (
                "GET",
                "/api/integrations/storage/files/a/b",
                Native::StorageBrowse,
            ),
            (
                "POST",
                "/api/integrations/storage/file",
                Native::StorageBrowse,
            ),
        ] {
            // The default build carries every slice (Cargo.toml default features).
            if crate::config::FRONT_ROUTE_FEATURES
                .iter()
                .all(|(_, on)| *on)
            {
                assert_eq!(dispatch_under(m, p, auth), Some(native), "{m} {p}");
            }
            if crate::config::FRONT_ROUTE_FEATURES
                .iter()
                .all(|(_, on)| *on)
            {
                assert_eq!(dispatch(m, p), Some(native), "{m} {p}: no switch at all");
            }
        }
    }
}
