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
    /// The Diary routes that are only a proxy and a policy (sbstndalton/noevia#2185):
    /// crates/server-diary. The others stay Node's (contracts/http/README.md).
    Diary,
    /// GET /api/models/capabilities and GET /api/models/hardware (sbstndalton/noevia#2187), and GET
    /// /api/models/estimate and GET /api/models/preset/suggest (#2189): model_routes.rs and
    /// model_sizing.rs. The other model routes stay Node's.
    Models,
    /// GET|HEAD /about and /privacy, every method of /device (sbstndalton/noevia#2190):
    /// public_pages.rs.
    PublicPages,
    /// GET /api/plugins/directory and its 405s (sbstndalton/noevia#2190):
    /// plugin_directory_routes.rs.
    PluginDirectory,
    /// Every method of /api/admin/decision-settings and /test (sbstndalton/noevia#2190):
    /// decision_settings_routes.rs.
    DecisionSettings,
    /// POST /api/integrations/storage/{folder,test,nextcloud/start,poll,cancel}
    /// (sbstndalton/noevia#2190): storage_integration_routes.rs.
    StorageIntegrations,
    /// POST /api/providers/test (sbstndalton/noevia#2190 round 4): provider_test_routes.rs.
    ProviderTest,
    /// POST /api/providers/chatgpt/{device,device/poll,device/cancel}, GET
    /// /api/providers/chatgpt/models (sbstndalton/noevia#2190 round 5): chatgpt_oauth_routes.rs.
    ChatgptOAuth,
    /// /api/providers/{providerId}/models{*?} (sbstndalton/noevia#2190 round 5):
    /// provider_models_routes.rs.
    ProviderModels,
    /// Every method of the eight /api/admin/offsite-backup paths (sbstndalton/noevia#2190 round 7):
    /// offsite_routes.rs.
    Offsite,
    /// DELETE /api/admin/users/{id} (sbstndalton/noevia#2190, #2191 round 9): admin_user_routes.rs.
    AdminUsers,
    /// POST /api/connectors/gdrive/{connect,disconnect}, PUT .../{policy,backup-copy}
    /// (sbstndalton/noevia#2190 round 7): connector_routes.rs.
    Connectors,
    /// GET|POST /api/providers, PUT|DELETE /api/providers/{id}, DELETE /api/providers/chatgpt
    /// (sbstndalton/noevia#2190 round 6): provider_routes.rs.
    Providers,
    /// GET /api/health (sbstndalton/noevia#2190 close-out, `rust-health`): health_routes.rs.
    Health,
    /// GET /api/auto-roles (sbstndalton/noevia#2190 close-out, `rust-auto-roles`):
    /// auto_roles_routes.rs. The PUT stays Node's.
    AutoRoles,
    /// /api/mcp-keys/* and /api/mcp-oauth/* (sbstndalton/noevia#2202): mcp_routes.rs.
    McpCredentials,
    /// Every method of /api/admin/mcp-directory* (sbstndalton/noevia#2202 round 2):
    /// mcp_directory_routes.rs.
    McpDirectory,
    /// GET /api/toolboxes/permitted (sbstndalton/noevia#2202 round 2): toolbox_routes.rs.
    ToolboxesPermitted,
}

/// The routes [`Native::McpDirectory`] implements (every method; routes.toml).
const MCP_DIRECTORY: &[&str] = &[
    "/api/admin/mcp-directory",
    "/api/admin/mcp-directory/custom",
    "/api/admin/mcp-directory/custom/preview",
    "/api/admin/mcp-directory/{*}",
    "/api/admin/mcp-directory/{*}/keys",
    "/api/admin/mcp-directory/{id}/oauth-client",
];

/// The route [`Native::ToolboxesPermitted`] implements (routes.toml gives Rust every method; the
/// handler answers 405 to all but GET).
const TOOLBOXES_PERMITTED: &str = "/api/toolboxes/permitted";

/// The route [`Native::ProviderTest`] implements (POST only; routes.toml).
const PROVIDER_TEST: &str = "/api/providers/test";

/// The routes [`Native::Providers`] implements (sbstndalton/noevia#2190 round 6): the list (GET,
/// POST) and one row (PUT, DELETE). `DELETE /api/providers/chatgpt` and `PUT|DELETE
/// /api/providers/test` are the same handler's, picked by method in [`dispatch_under`].
pub(crate) const PROVIDERS_LIST: &str = "/api/providers";
pub(crate) const PROVIDER_ONE: &str = "/api/providers/{id}";

/// The routes [`Native::McpCredentials`] implements (sbstndalton/noevia#2202; routes.toml gives
/// Rust exactly the methods mcp_routes.rs answers).
const MCP_CREDENTIALS: &[&str] = &[
    "/api/mcp-keys/servers",
    "/api/mcp-keys/{*}",
    "/api/mcp-oauth/servers",
    "/api/mcp-oauth/{id}",
    "/api/mcp-oauth/{id}/connect",
    "/api/mcp-oauth/callback",
];

/// The route [`Native::ProviderModels`] implements (every method; routes.toml).
const PROVIDER_MODELS: &str = "/api/providers/{providerId}/models{*?}";

/// The routes [`Native::ChatgptOAuth`] implements (the three POSTs and the models GET; routes.toml).
const CHATGPT_OAUTH: &[&str] = &[
    "/api/providers/chatgpt/device",
    "/api/providers/chatgpt/device/poll",
    "/api/providers/chatgpt/device/cancel",
    "/api/providers/chatgpt/models",
];

/// The routes [`Native::Diary`] implements (diary_routes.rs). `/api/diary/history`, which core's
/// handler shares with `today`, is not a route the web client names and stays Node's, as do `files`,
/// `file`, `workspace-trash`, `external-sources` and `entries/edit`, and
/// `POST /api/diary/local-exchange` (contracts/http/README.md says why). Paths marked `methods` in
/// routes.toml answer only those; the rest answer every method and give their own 405 after the
/// add-on gate, as core does.
pub(crate) const DIARY_ROUTES: &[&str] = &[
    "/api/diary/source",
    "/api/diary/today",
    "/api/diary/storage-import",
    "/api/diary/workspace-export",
    "/api/diary/workspace-import",
    // sbstndalton/noevia#2206:
    "/api/diary",
    "/api/diary-connector",
    "/api/diary/context",
    "/api/diary/exchanges",
    "/api/diary/storage-status",
    "/api/profile/diary-connectors",
    "/api/profile/diary-connectors/{*}",
];

/// The route [`Native::RoutingDefault`] implements (GET only; routes.toml).
const ROUTING_DEFAULT: &str = "/api/routing-default";

/// An account's deletion (DELETE only; routes.toml), the front's under `rust-admin-users`
/// (sbstndalton/noevia#2190, #2191 round 9); Node refuses it (core rust-admin-users.cjs).
pub(crate) const ADMIN_USER: &str = "/api/admin/users/{id}";

/// The route [`Native::Stats`] implements (GET only; routes.toml), answered only in a build with
/// the cargo feature `rust-stats` (listed by `--features`, so the supervisor confirms it to Node).
const STATS: &str = "/api/stats";

/// The route [`Native::Export`] implements (GET only; routes.toml), answered only in a build with
/// the cargo feature `rust-export` (listed by `--features`, so the supervisor confirms it to Node).
const EXPORT: &str = "/api/export/conversations";

/// The route [`Native::ChatgptStatus`] implements (GET only; routes.toml), answered only in a build
/// with the cargo feature `rust-chatgpt-status` (listed by `--features`, so the supervisor confirms
/// it to Node). Its siblings `/device`, `/device/poll`, `/device/cancel` (POST) and `/models` (GET) are
/// [`Native::ChatgptOAuth`]'s since sbstndalton/noevia#2190 round 5; DELETE stays Node's.
const CHATGPT_STATUS: &str = "/api/providers/chatgpt";

/// The routes [`Native::Projects`] implements.
const PROJECT_ROUTES: &[&str] = &[
    "/api/projects/{id}/assets",
    "/api/projects/{id}/assets/{assetId}",
    PROJECT_CHATS,
    FREE_CHATS,
    CHAT_CONTEXT_WINDOW,
    CHAT_HISTORY,
    CHAT_MOVE,
    FREE_CHAT,
    PROJECT_CHAT,
    PROJECT_SOURCE_TEXT,
    PROJECT_DOCUMENT_ORIGINAL,
    PROJECT_DOCUMENT_PAGES,
    PROJECT_ORIGINAL,
    PROJECT_CREATE,
    PROJECT_ONE,
    PROJECT_FILES,
    PROJECT_DOCUMENTS,
    PROJECT_UPLOAD,
    PROJECT_SYNC,
    PROJECT_FOLDER_IMPORT,
    PROJECT_SOURCE_JOB,
    PROJECT_CONFIG,
    SKILLS,
    SKILL_MANIFESTS,
    SKILL_CONTENT,
    SKILL_INSTALL,
    RESEARCH,
    RESEARCH_PLAN,
    RESEARCH_JOB,
    RESEARCH_CANCEL,
    RESEARCH_SAVE,
    server_projects::import::ROUTE,
    WORKSPACE,
];

/// Deep research (every method; routes.toml), the front's under `rust-research` (sbstndalton/noevia#2218);
/// Node refuses them (core rust-research.cjs).
pub(crate) const RESEARCH: &str = "/api/projects/{projectId}/research";
pub(crate) const RESEARCH_PLAN: &str = "/api/projects/{projectId}/research/plan";
pub(crate) const RESEARCH_JOB: &str = "/api/projects/{projectId}/research/{job}";
pub(crate) const RESEARCH_CANCEL: &str = "/api/projects/{projectId}/research/{job}/cancel";
pub(crate) const RESEARCH_SAVE: &str = "/api/projects/{projectId}/research/{job}/save";

/// The projects and free chats (GET only; routes.toml), the front's under `rust-workspace`
/// (sbstndalton/noevia#2191 close-out); Node refuses the GET (core rust-workspace.cjs
/// `RUST_WORKSPACE_OWNED`). It runs the delete-old-chats sweep and the orphan-context cleanup too.
pub(crate) const WORKSPACE: &str = "/api/workspace";

/// A project upload, folder sync and folder-import plan (POST only; routes.toml), the front's under
/// `rust-project-source-work` (sbstndalton/noevia#2191 round 8), and a source job's status (GET only),
/// the front's under `rust-source-jobs` (round 6). Node refuses all of them (core
/// rust-project-source-work.cjs, rust-source-jobs.cjs).
pub(crate) const PROJECT_UPLOAD: &str = "/api/projects/{id}/upload";
pub(crate) const PROJECT_SYNC: &str = "/api/projects/{id}/sources/sync";
pub(crate) const PROJECT_FOLDER_IMPORT: &str = "/api/projects/{id}/folder-import/plan";
pub(crate) const PROJECT_SOURCE_JOB: &str = "/api/projects/{id}/source-jobs/{jobId}";

/// A project document's upload (POST only; routes.toml), the front's under `rust-project-documents`
/// (sbstndalton/noevia#2191 round 5), on the Rust extraction pipeline; Node refuses it
/// (core rust-project-documents.cjs).
pub(crate) const PROJECT_DOCUMENTS: &str = "/api/projects/{id}/documents";

/// A project source file's delete from the user's storage (DELETE only; routes.toml), the front's
/// under `rust-project-files` (sbstndalton/noevia#2191 round 3); Node refuses it
/// (core rust-project-files.cjs).
pub(crate) const PROJECT_FILES: &str = "/api/projects/{id}/files";

/// A project's configuration (POST only), its instruction skills (GET, PUT; the manifests and a
/// manifest's content, GET) and a published skill's install (POST only; routes.toml), the front's
/// under `rust-project-config` (sbstndalton/noevia#2191 round 9); Node refuses them (core
/// rust-project-config.cjs).
pub(crate) const PROJECT_CONFIG: &str = "/api/projects/{projectId}/config";
pub(crate) const SKILLS: &str = "/api/projects/{projectId}/instruction-skills";
pub(crate) const SKILL_MANIFESTS: &str = "/api/projects/{projectId}/instruction-skills/manifests";
pub(crate) const SKILL_CONTENT: &str =
    "/api/projects/{projectId}/instruction-skills/manifests/{skillId}/content";
pub(crate) const SKILL_INSTALL: &str = "/api/projects/{project}/skills/install";

/// A project's create (POST only) and delete (DELETE only; routes.toml), the front's under
/// `rust-project-lifecycle` (sbstndalton/noevia#2191); Node refuses them
/// (core rust-project-lifecycle.cjs).
pub(crate) const PROJECT_CREATE: &str = "/api/projects";
pub(crate) const PROJECT_ONE: &str = "/api/projects/{projectId}";

/// A project source's stored text and stored original bytes (GET only; routes.toml), the front's
/// under `rust-project-reads` (sbstndalton/noevia#2191); Node refuses those GETs
/// (core rust-project-reads.cjs). `{*}/original` is `uploads/original`; any other segment there is
/// sent on to Node by project_routes::route.
pub(crate) const PROJECT_SOURCE_TEXT: &str = "/api/projects/{id}/sources/text";
pub(crate) const PROJECT_DOCUMENT_ORIGINAL: &str = "/api/projects/{id}/documents/original";
/// A stored document's page text (GET only; routes.toml), the front's under
/// `rust-project-document-pages` (sbstndalton/noevia#2191 round 4); Node refuses the GET (core
/// rust-project-document-pages.cjs).
pub(crate) const PROJECT_DOCUMENT_PAGES: &str = "/api/projects/{id}/documents/pages";
pub(crate) const PROJECT_ORIGINAL: &str = "/api/projects/{id}/{*}/original";

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
const CHAT_MOVE: &str = "/api/chats/{chatId}/move";

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
/// The storage routes storage_integration_routes.rs answers (POST only, routes.toml).
const STORAGE_INTEGRATIONS: &[&str] = &[
    "/api/integrations/storage/folder",
    "/api/integrations/storage/test",
    "/api/integrations/storage/nextcloud/start",
    "/api/integrations/storage/nextcloud/poll",
    "/api/integrations/storage/nextcloud/cancel",
];
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
        && (!STORAGE_INTEGRATIONS.contains(&route.path) || cfg!(feature = "rust-storage-reads"))
        && (route.path != PROVIDER_TEST || cfg!(feature = "rust-storage-reads"))
        && (route.path != EXPORT || cfg!(feature = "rust-export"))
        && (route.path != CHATGPT_STATUS || cfg!(feature = "rust-chatgpt-status"))
        && (!CHATGPT_OAUTH.contains(&route.path) || cfg!(feature = "rust-chatgpt-status"))
        && (route.path != PROVIDER_MODELS || cfg!(feature = "rust-storage-reads"))
        && (![PROVIDERS_LIST, PROVIDER_ONE].contains(&route.path)
            || cfg!(feature = "rust-storage-reads"))
        && (!MCP_CREDENTIALS.contains(&route.path) || cfg!(feature = "rust-storage-reads"))
        && (!MCP_DIRECTORY.contains(&route.path) || cfg!(feature = "rust-storage-reads"))
        && (route.path != TOOLBOXES_PERMITTED || cfg!(feature = "rust-storage-reads"))
        && (![
            crate::connector_routes::LIST,
            crate::connector_routes::NEXTCLOUD_POLICY,
        ]
        .contains(&route.path)
            || cfg!(feature = "rust-storage-reads"))
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
        ADMIN_USER if auth => return Some(Native::AdminUsers),
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
        p if auth && DIARY_ROUTES.contains(&p) => return Some(Native::Diary),
        p if auth
            && (crate::model_routes::ROUTES.contains(&p)
                || crate::model_maint::ROUTES.contains(&p)) =>
        {
            return Some(Native::Models)
        }
        p if auth && crate::public_pages::ROUTES.contains(&p) => return Some(Native::PublicPages),
        crate::plugin_directory_routes::DIRECTORY if auth => return Some(Native::PluginDirectory),
        p if auth && crate::decision_settings_routes::ROUTES.contains(&p) => {
            return Some(Native::DecisionSettings)
        }
        p if auth && STORAGE_INTEGRATIONS.contains(&p) => return Some(Native::StorageIntegrations),
        crate::health_routes::PATH if auth => return Some(Native::Health),
        crate::auto_roles_routes::PATH if auth => return Some(Native::AutoRoles),
        PROVIDER_TEST if auth => return Some(Native::ProviderTest),
        p if auth && CHATGPT_OAUTH.contains(&p) => return Some(Native::ChatgptOAuth),
        PROVIDER_MODELS if auth => return Some(Native::ProviderModels),
        p if auth && crate::offsite_routes::ROUTES.contains(&p) => return Some(Native::Offsite),
        p if auth && crate::connector_routes::ROUTES.contains(&p) => {
            return Some(Native::Connectors)
        }
        PROVIDERS_LIST | PROVIDER_ONE if auth => return Some(Native::Providers),
        p if auth && MCP_CREDENTIALS.contains(&p) => return Some(Native::McpCredentials),
        p if auth && MCP_DIRECTORY.contains(&p) => return Some(Native::McpDirectory),
        TOOLBOXES_PERMITTED if auth => return Some(Native::ToolboxesPermitted),
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
    path == "/api/chat-framing/preferences"
        || path == server_account::vault_mirror::PATH
        || server_account::features::owns(path)
        || ACCOUNT_PREFIXES.iter().any(|p| {
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
    // The original feature suffix is a namespace, including empty/encoded or
    // slash-containing suffixes; only the public root rejects a trailing slash.
    if server_account::features::owns(path) {
        let pattern = if path.starts_with("/api/admin/features/") {
            "/api/admin/features/{name}"
        } else {
            path
        };
        return ROUTES.iter().find(|r| r.path == pattern);
    }
    if path == "/api/features/" {
        return None;
    }

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
    // This owner is exact-path only; the shared matcher tolerates trailing slashes.
    if (route.path == "/api/chat-framing/preferences"
        || route.path == server_account::vault_mirror::PATH)
        && path != route.path
    {
        return None;
    }
    if route.path == STORAGE_METADATA
        && (path != STORAGE_METADATA || !["GET", "PUT"].contains(&method))
    {
        return None;
    }
    if route.owner_under(switches) != Owner::Rust {
        return None;
    }
    if !route.methods.is_empty() && !route.methods.contains(&method) {
        return None;
    }
    // sbstndalton/noevia#2190 round 6: the provider registry's writes share these paths with the
    // status GET and the probe POST (a `test` or `chatgpt` segment is also a provider id).
    if matches!(
        (route.path, method),
        (CHATGPT_STATUS, "DELETE") | (PROVIDER_TEST, "PUT" | "DELETE")
    ) {
        return cfg!(feature = "rust-storage-reads").then_some(Native::Providers);
    }
    if !compiled(route) {
        return None;
    }
    // sbstndalton/noevia#2190 round 4: saving the connection is storage_integration_routes.rs;
    // reading it is the metadata worker's.
    if route.path == STORAGE_METADATA && method == "PUT" {
        return Some(Native::StorageIntegrations);
    }
    native_for(route)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_feature_namespace_is_native_for_every_method_and_preserves_siblings() {
        for path in [
            "/api/features",
            "/api/admin/features",
            "/api/admin/features/",
            "/api/admin/features/previews",
            "/api/admin/features/%",
            "/api/admin/features/a/b",
        ] {
            for method in ["GET", "PUT", "POST", "DELETE", "HEAD", "OPTIONS", "PATCH"] {
                assert_eq!(
                    dispatch(method, path),
                    Some(Native::Account),
                    "{method} {path}"
                );
            }
        }
        for path in ["/api/features/", "/api/features-x", "/api/admin/features-x"] {
            for method in ["GET", "PUT", "POST", "DELETE"] {
                assert_eq!(dispatch(method, path), None, "{method} {path}");
            }
        }
    }
    #[test]
    fn framing_preferences_are_native_for_all_methods_without_claiming_siblings() {
        for method in ["GET", "PUT", "POST", "DELETE", "HEAD", "OPTIONS"] {
            assert_eq!(
                dispatch(method, "/api/chat-framing/preferences"),
                Some(Native::Account)
            );
        }
        for path in [
            "/api/chat-framing/suggest",
            "/api/chat-framing/preferences/",
            "/api/chat-framing/preferences/x",
        ] {
            assert!(!is_account_path(path));
            for method in ["GET", "PUT", "POST", "DELETE", "HEAD", "OPTIONS"] {
                assert_eq!(dispatch(method, path), None, "{method} {path}");
            }
        }
    }

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
                Some(Native::DecisionSettings)
                    if crate::decision_settings_routes::ROUTES.contains(&r.path) => {}
                Some(Native::StorageIntegrations) if STORAGE_INTEGRATIONS.contains(&r.path) => {}
                Some(Native::ProviderTest) if r.path == PROVIDER_TEST => {}
                Some(Native::ChatgptOAuth) if CHATGPT_OAUTH.contains(&r.path) => {}
                Some(Native::ProviderModels) if r.path == PROVIDER_MODELS => {}
                Some(Native::Offsite) if crate::offsite_routes::ROUTES.contains(&r.path) => {}
                Some(Native::AdminUsers) if r.path == ADMIN_USER => {}
                Some(Native::Connectors) if crate::connector_routes::ROUTES.contains(&r.path) => {}
                // `/api/profile/diary-connectors` is under the account prefix and is not the account's.
                Some(Native::Diary) if DIARY_ROUTES.contains(&r.path) => {}
                Some(Native::Providers) if r.path == PROVIDERS_LIST || r.path == PROVIDER_ONE => {}
                Some(Native::McpDirectory) if MCP_DIRECTORY.contains(&r.path) => {}
                Some(Native::ToolboxesPermitted) if r.path == TOOLBOXES_PERMITTED => {}
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
            ("GET", "/api/projects/p1/upload"),
            // DELETE /api/projects/p1 is the front's since #2191 round 2 (rust-project-lifecycle).
            ("GET", "/api/projects/p1"),
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
            // The project's own create and delete are the front's since #2191 round 2.
            ("GET", "/api/projects"),
            // The chat-bound project routes stay Node's (browser, code, research).
            ("POST", "/api/projects/p1/browser"),
            ("PUT", "/api/projects/p1"),
        ] {
            assert_eq!(dispatch_under(m, p, both), None, "{m} {p}");
        }
    }

    #[test]
    fn dispatch_decisions() {
        assert_eq!(dispatch("GET", "/api/ready"), Some(Native::Ready));
        assert_eq!(dispatch("HEAD", "/api/ready"), None);
        assert_eq!(dispatch("POST", "/api/ready"), None);
        // sbstndalton/noevia#2190 close-out: GET only is the front's; the other methods stay Node's.
        assert_eq!(dispatch("GET", "/api/health"), Some(Native::Health));
        assert_eq!(dispatch("POST", "/api/health"), None);
        assert_eq!(dispatch("GET", "/api/auto-roles"), Some(Native::AutoRoles));
        for m in ["PUT", "POST", "DELETE", "HEAD"] {
            assert_eq!(dispatch(m, "/api/auto-roles"), None, "{m}");
        }
        assert_eq!(dispatch("GET", "/api/workspace"), Some(Native::Projects));
        for m in ["POST", "PUT", "DELETE", "HEAD"] {
            assert_eq!(dispatch(m, "/api/workspace"), None, "{m}");
        }
        assert_eq!(dispatch("GET", "/api/not-listed/at/all"), None);
        assert_eq!(dispatch("GET", "/api"), None);
        assert_eq!(dispatch("GET", "/"), Some(Native::Static));
        assert_eq!(dispatch("HEAD", "/assets/a.js"), Some(Native::Static));
        assert_eq!(dispatch("GET", "/c/abc"), Some(Native::Static));
        assert_eq!(dispatch("POST", "/c/abc"), None);
        // sbstndalton/noevia#2190: the public pages (GET/HEAD) and the device page are the front's.
        assert_eq!(dispatch("GET", "/about"), Some(Native::PublicPages));
        assert_eq!(dispatch("HEAD", "/privacy"), Some(Native::PublicPages));
        assert_eq!(dispatch("POST", "/about"), None);
        assert_eq!(dispatch("GET", "/device"), Some(Native::PublicPages));
        assert_eq!(dispatch("POST", "/device/"), Some(Native::PublicPages));
        assert_eq!(dispatch("GET", "/device/x"), Some(Native::Static));
        for m in ["GET", "POST", "PUT", "DELETE", "HEAD"] {
            assert_eq!(
                dispatch(m, "/api/plugins/directory"),
                Some(Native::PluginDirectory),
                "{m}"
            );
        }
        assert_eq!(dispatch("GET", "/api/plugins/directoryx"), None);
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
        // An account's deletion is the front's since #2190/#2191 round 9 (its own handler, not the account crate's).
        assert_eq!(
            dispatch("DELETE", "/api/admin/users/u1"),
            Some(Native::AdminUsers)
        );
        for m in ["GET", "PUT", "PATCH", "POST"] {
            assert_eq!(dispatch(m, "/api/admin/users/u1"), None, "{m}");
        }
        // Advertised only when the MCP and ChatGPT brokers its cleanup asks are compiled in (review L6).
        assert_eq!(
            crate::FEATURES.contains(&"rust-admin-users"),
            cfg!(all(
                feature = "rust-storage-reads",
                feature = "rust-chatgpt-status"
            ))
        );
        for (m, p) in [
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
            ("GET", "/api/chats/c1/move"),
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
            ("GET", "/api/chats/c1/move"),
            ("POST", "/api/workspace"),
        ] {
            assert_eq!(dispatch_under(m, p, both), None, "{m} {p}");
        }
    }

    #[test]
    fn chat_move_is_rusts_for_post_only() {
        assert_eq!(
            dispatch("POST", "/api/chats/c1/move"),
            Some(Native::Projects)
        );
        for method in ["GET", "HEAD", "PUT", "PATCH", "DELETE"] {
            assert_eq!(dispatch(method, "/api/chats/c1/move"), None);
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
            ("GET", "/api/chats/c1/move"),
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
            ("GET", "/api/chats/c1/move"),
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
        ] {
            assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
        }
        assert!(!on.on("NOEVIA_RUST_EXPORT"));
        assert!(!on.on("NOEVIA_RUST_PROJECTS"));
    }

    /// sbstndalton/noevia#2185 and #2206: the Diary routes the front answers, with the methods
    /// routes.toml gives each; everything else Diary stays Node's.
    #[test]
    fn diary_routes_are_rusts_and_the_rest_stays_nodes() {
        let on = Switches::default();
        const EVERY: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];
        // Every method: the proxies, `/api/diary` itself, the storage status and the connector
        // endpoint (which answers its own 405 before authentication).
        for p in [
            "/api/diary/source",
            "/api/diary/today",
            "/api/diary/storage-import",
            "/api/diary/workspace-export",
            "/api/diary/workspace-import",
            "/api/diary",
            "/api/diary-connector",
            "/api/diary/storage-status",
        ] {
            assert!(DIARY_ROUTES.contains(&p), "{p}");
            for m in EVERY {
                assert_eq!(dispatch_under(m, p, on), Some(Native::Diary), "{m} {p}");
            }
        }
        // The methods core mounted and no more: another method is Node's (its unknown-path answer).
        for (p, methods) in [
            ("/api/diary/context", &["GET", "POST"][..]),
            ("/api/diary/exchanges", &["GET", "POST"][..]),
            ("/api/profile/diary-connectors", &["GET", "POST"][..]),
            (
                "/api/profile/diary-connectors/0123456789abcdef0123456789abcdef",
                &["DELETE"][..],
            ),
        ] {
            for m in EVERY {
                let want = methods.contains(&m).then_some(Native::Diary);
                assert_eq!(dispatch_under(m, p, on), want, "{m} {p}");
            }
        }
        assert!(crate::FEATURES.contains(&"rust-diary"));
        assert!(crate::FEATURES.contains(&"rust-diary-routes"));
        // Node keeps the local exchange (the model throttle chat shares and the inference gate, both
        // Node memory), and what the web client never names is not in the route table at all.
        for p in [
            "/api/diary/local-exchange",
            "/api/diary/history",
            "/api/diary/files",
            "/api/diary/file",
            "/api/diary/workspace-trash",
            "/api/diary/entries/edit",
            "/api/diary/external-sources",
            "/api/diary/sourcex",
            "/api/diary/source/x",
        ] {
            for m in ["GET", "POST", "PUT", "DELETE"] {
                assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
            }
        }
    }

    /// sbstndalton/noevia#2187: the two configuration-only model reads are the front's, GET only, and
    /// (#2189) the two sizing reads; every other method and every other model route stays Node's.
    #[test]
    fn model_reads_are_rusts_for_get_only_and_the_other_model_routes_stay_nodes() {
        let on = Switches::default();
        for p in crate::model_routes::ROUTES {
            assert_eq!(
                dispatch_under("GET", p, on),
                Some(Native::Models),
                "GET {p}"
            );
            for m in ["HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
                assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
            }
        }
        assert!(crate::FEATURES.contains(&"rust-model-reads"));
        assert!(crate::FEATURES.contains(&"rust-model-sizing"));
        assert!(crate::FEATURES.contains(&"rust-model-maintenance"));
        for p in [
            "/api/model-manager",
            "/api/model-manager/models",
            "/api/models/autotune",
            "/api/models/autotune/cancel",
            "/api/models/autotune/clear",
            "/api/models/autotune/resume",
            "/api/models/autotune/settings",
            "/api/models/autotune/untuned",
            "/api/models/calibration",
            "/api/models/calibration/cancel",
            "/api/models/delete",
            "/api/models/evidence/recheck",
            "/api/models/inference-budget",
            "/api/models/installed",
            "/api/models/load",
            "/api/models/machines",
            "/api/models/preset",
            "/api/models/downloads",
            "/api/models/capabilitiesx",
            "/api/models/hardware/x",
            "/api/models/estimate/x",
            "/api/models/preset/suggestx",
            "/api/models/preset/x",
        ] {
            for m in ["GET", "POST", "PUT", "DELETE"] {
                assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
            }
        }
        // Exactly the two model routes are the front's in the contract, GET only.
        let rust: Vec<_> = ROUTES
            .iter()
            .filter(|r| {
                (r.path.starts_with("/api/models") || r.path.starts_with("/api/model-manager"))
                    && r.owner == Owner::Rust
            })
            .map(|r| (r.path, r.methods))
            .collect();
        assert_eq!(
            rust,
            vec![
                (crate::model_routes::CAPABILITIES, &["GET"][..]),
                (crate::model_sizing::ESTIMATE, &["GET"][..]),
                (crate::model_maint::EVIDENCE, &["GET"][..]),
                (crate::model_maint::EVIDENCE_IMPORT, &["POST"][..]),
                (crate::model_routes::HARDWARE, &["GET"][..]),
                (crate::model_sizing::SUGGEST, &["GET"][..]),
                (crate::model_maint::RELOAD, &["POST"][..]),
            ]
        );
        // #2210: the maintenance routes are the front's for their one method only; every other
        // method stays Node's.
        for p in crate::model_maint::ROUTES {
            let own = if *p == crate::model_maint::EVIDENCE {
                "GET"
            } else {
                "POST"
            };
            assert_eq!(
                dispatch_under(own, p, on),
                Some(Native::Models),
                "{own} {p}"
            );
            for m in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
                if m != own {
                    assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
                }
            }
        }
    }

    /// sbstndalton/noevia#2191: a project source's three stored reads are the front's, GET only;
    /// other methods, the page reader and the source writes stay Node's.
    #[test]
    fn project_source_reads_are_rusts_for_get_only() {
        let on = Switches::default();
        for p in [
            "/api/projects/p1/sources/text",
            "/api/projects/p1/documents/original",
            "/api/projects/p1/documents/pages",
            "/api/projects/p1/uploads/original",
        ] {
            assert_eq!(
                dispatch_under("GET", p, on),
                Some(Native::Projects),
                "GET {p}"
            );
            for m in ["POST", "PUT", "PATCH", "DELETE"] {
                assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
            }
        }
        for p in [
            "/api/projects/p1/upload",
            "/api/projects/p1/sources/sync",
            "/api/projects/p1/config",
        ] {
            for m in ["GET", "DELETE"] {
                assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
            }
        }
        assert_eq!(dispatch_under("POST", "/api/projects/p1/browser", on), None);
        // `{*}/original` is matched by the table; project_routes::route sends any segment but
        // `uploads` on to Node.
        assert_eq!(
            crate::project_routes::route("GET", "/api/projects/p1/other/original"),
            None
        );
        assert!(crate::FEATURES.contains(&"rust-project-reads"));
    }

    /// sbstndalton/noevia#2191 round 9: a project's configuration, its instruction skills and a
    /// published skill's install are the front's for their methods only; the browser, code and research
    /// routes beside them are chat-bound and stay Node's.
    #[test]
    fn project_config_and_skills_are_rusts_for_their_methods_only() {
        let on = Switches::default();
        for (m, p) in [
            ("POST", "/api/projects/p1/config"),
            ("GET", "/api/projects/p1/instruction-skills"),
            ("PUT", "/api/projects/p1/instruction-skills"),
            ("GET", "/api/projects/p1/instruction-skills/manifests"),
            (
                "GET",
                "/api/projects/p1/instruction-skills/manifests/skill_x/content",
            ),
            ("POST", "/api/projects/p1/skills/install"),
        ] {
            assert_eq!(dispatch_under(m, p, on), Some(Native::Projects), "{m} {p}");
        }
        for (m, p) in [
            ("GET", "/api/projects/p1/config"),
            ("PUT", "/api/projects/p1/config"),
            ("POST", "/api/projects/p1/instruction-skills"),
            ("DELETE", "/api/projects/p1/instruction-skills"),
            ("POST", "/api/projects/p1/instruction-skills/manifests"),
            (
                "PUT",
                "/api/projects/p1/instruction-skills/manifests/skill_x/content",
            ),
            ("GET", "/api/projects/p1/skills/install"),
            ("POST", "/api/projects/p1/browser"),
            ("POST", "/api/projects/p1/code"),
            ("GET", "/api/code/active"),
        ] {
            assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
        }
        use crate::project_routes::{route, Route};
        assert_eq!(
            route("POST", "/api/projects/p1/config"),
            Some(Route::Config {
                project: "p1".into()
            })
        );
        assert_eq!(
            route(
                "GET",
                "/api/projects/p1/instruction-skills/manifests/skill_x/content"
            ),
            Some(Route::SkillContent {
                project: "p1".into(),
                skill: "skill_x".into()
            })
        );
        // Node's matchers took one segment for each parameter.
        for (m, p) in [
            ("POST", "/api/projects//config"),
            ("POST", "/api/projects/a/b/config"),
            (
                "GET",
                "/api/projects/p1/instruction-skills/manifests//content",
            ),
            (
                "GET",
                "/api/projects/p1/instruction-skills/manifests/a/b/content",
            ),
            ("POST", "/api/projects//skills/install"),
        ] {
            assert_eq!(route(m, p), None, "{m} {p}");
        }
        assert!(crate::FEATURES.contains(&"rust-project-config"));
    }

    /// sbstndalton/noevia#2191 round 2: a project's create (POST) and delete (DELETE) are the
    /// front's; every other method on those paths, and /config, stay Node's.
    #[test]
    fn project_create_and_delete_are_rusts_for_their_method_only() {
        let on = Switches::default();
        assert_eq!(
            dispatch_under("POST", "/api/projects", on),
            Some(Native::Projects)
        );
        assert_eq!(
            dispatch_under("DELETE", "/api/projects/p1", on),
            Some(Native::Projects)
        );
        for m in ["GET", "PUT", "PATCH", "DELETE"] {
            assert_eq!(dispatch_under(m, "/api/projects", on), None, "{m}");
        }
        for m in ["GET", "PUT", "PATCH", "POST"] {
            assert_eq!(dispatch_under(m, "/api/projects/p1", on), None, "{m}");
        }
        assert_eq!(dispatch_under("POST", "/api/projects/p1/browser", on), None);
        assert_eq!(
            crate::project_routes::route("DELETE", "/api/projects/"),
            None
        );
        assert!(crate::FEATURES.contains(&"rust-project-lifecycle"));
    }

    #[test]
    fn a_document_upload_is_native_for_post_only() {
        let on = Switches::default();
        assert_eq!(
            dispatch_under("POST", "/api/projects/p1/documents", on),
            Some(Native::Projects)
        );
        for m in ["GET", "PUT", "PATCH", "DELETE"] {
            assert_eq!(
                dispatch_under(m, "/api/projects/p1/documents", on),
                None,
                "{m}"
            );
        }
        assert!(matches!(
            crate::project_routes::route("POST", "/api/projects/p1/documents"),
            Some(crate::project_routes::Route::Documents { .. })
        ));
        assert_eq!(
            crate::project_routes::route("POST", "/api/projects//documents"),
            None
        );
        assert!(crate::FEATURES.contains(&"rust-project-documents"));
        assert!(crate::FEATURES.contains(&"rust-document-extraction"));
        // Round 7: the large-document lock is the host's, shared with core.
        assert!(crate::FEATURES.contains(&"rust-large-document-host-lock"));
    }

    /// sbstndalton/noevia#2191 round 6: the source jobs are the front's: the upload's and sync's POST
    /// (project_routes sends the synchronous form, without `background=1`, on to Node) and the poll.
    #[test]
    fn the_source_jobs_are_native_for_their_methods() {
        use crate::project_routes::{route, Route};
        let on = Switches::default();
        for p in [
            "/api/projects/p1/upload",
            "/api/projects/p1/sources/sync",
            "/api/projects/p1/folder-import/plan",
        ] {
            assert_eq!(dispatch_under("POST", p, on), Some(Native::Projects), "{p}");
            for m in ["GET", "PUT", "PATCH", "DELETE"] {
                assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
            }
        }
        let poll = "/api/projects/p1/source-jobs/j1";
        assert_eq!(dispatch_under("GET", poll, on), Some(Native::Projects));
        for m in ["POST", "PUT", "DELETE"] {
            assert_eq!(dispatch_under(m, poll, on), None, "{m}");
        }
        assert_eq!(
            route("POST", "/api/projects/p1/upload"),
            Some(Route::SourceUpload {
                project: "p1".into()
            })
        );
        assert_eq!(
            route("POST", "/api/projects/p%201/sources/sync"),
            Some(Route::SourceSync {
                project: "p%201".into()
            })
        );
        assert_eq!(
            route("GET", poll),
            Some(Route::SourceJob {
                project: "p1".into(),
                job: "j1".into()
            })
        );
        for (m, p) in [
            ("POST", "/api/projects//upload"),
            ("GET", "/api/projects/p1/source-jobs/"),
            ("GET", "/api/projects/p1/upload"),
        ] {
            assert_eq!(route(m, p), None, "{m} {p}");
        }
        assert!(crate::FEATURES.contains(&"rust-source-jobs"));
        assert!(crate::FEATURES.contains(&"rust-project-source-work"));
        assert!(!crate::FEATURES.contains(&"noevia-source-jobs-store/1"));
        assert_eq!(
            route("POST", "/api/projects/p1/folder-import/plan"),
            Some(Route::FolderImportPlan {
                project: "p1".into()
            })
        );
    }

    #[test]
    fn a_source_file_delete_is_native_and_nothing_else_of_its_path() {
        let on = Switches::default();
        assert_eq!(
            dispatch_under("DELETE", "/api/projects/p1/files", on),
            Some(Native::Projects)
        );
        for m in ["GET", "PUT", "POST", "PATCH"] {
            assert_eq!(dispatch_under(m, "/api/projects/p1/files", on), None, "{m}");
        }
        assert!(matches!(
            crate::project_routes::route("DELETE", "/api/projects/p1/files"),
            Some(crate::project_routes::Route::DeleteFile { .. })
        ));
        assert_eq!(
            crate::project_routes::route("GET", "/api/projects/p1/files"),
            None
        );
        assert_eq!(
            crate::project_routes::route("DELETE", "/api/projects//files"),
            None
        );
        assert!(crate::FEATURES.contains(&"rust-project-files"));
        assert!(crate::FEATURES.contains(&"rust-project-document-pages"));
        // Round 4: the conversation import, POST only (Node keeps its 405).
        assert_eq!(
            dispatch_under("POST", "/api/import/conversations", on),
            Some(Native::Projects)
        );
        for m in ["GET", "PUT", "DELETE"] {
            assert_eq!(
                crate::project_routes::route(m, "/api/import/conversations"),
                None
            );
        }
        assert_eq!(
            crate::project_routes::route("POST", "/api/import/conversations/"),
            None
        );
        assert!(crate::FEATURES.contains(&"rust-conversation-import"));
        assert_eq!(
            crate::project_routes::route("GET", "/api/projects//documents/pages"),
            None
        );
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
        // The device sign-in and the model list are the broker's (sbstndalton/noevia#2190 round 5).
        let broker = cfg!(feature = "rust-chatgpt-status").then_some(Native::ChatgptOAuth);
        for (m, p) in [
            ("GET", "/api/providers/chatgpt/models"),
            ("POST", "/api/providers/chatgpt/device"),
            ("POST", "/api/providers/chatgpt/device/poll"),
            ("POST", "/api/providers/chatgpt/device/cancel"),
        ] {
            assert_eq!(dispatch_under(m, p, on), broker, "{m} {p}");
        }
        // The disconnect is the provider registry's since round 6; Node keeps every other method
        // (its 404/405), and the siblings' other methods.
        assert_eq!(
            dispatch_under("DELETE", "/api/providers/chatgpt", on),
            cfg!(feature = "rust-storage-reads").then_some(Native::Providers)
        );
        for (m, p) in [
            ("HEAD", "/api/providers/chatgpt"),
            ("POST", "/api/providers/chatgpt"),
            ("PUT", "/api/providers/chatgpt"),
            ("OPTIONS", "/api/providers/chatgpt"),
            ("POST", "/api/providers/chatgpt/models"),
            ("GET", "/api/providers/chatgpt/device"),
            ("DELETE", "/api/providers/chatgpt/device/poll"),
            ("GET", "/api/providers/chatgptx"),
        ] {
            assert_eq!(dispatch_under(m, p, on), None, "{m} {p}");
        }
        // A provider row's model list, the ChatGPT row's included (round 5, provider_models_routes.rs).
        assert_eq!(
            dispatch_under("GET", "/api/providers/chatgpt-oauth/models", on),
            cfg!(feature = "rust-storage-reads").then_some(Native::ProviderModels)
        );
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
            ("POST", "/api/providers/test", Native::ProviderTest),
            // sbstndalton/noevia#2190 round 5: the ChatGPT token broker.
            (
                "POST",
                "/api/providers/chatgpt/device",
                Native::ChatgptOAuth,
            ),
            (
                "POST",
                "/api/providers/chatgpt/device/poll",
                Native::ChatgptOAuth,
            ),
            (
                "POST",
                "/api/providers/chatgpt/device/cancel",
                Native::ChatgptOAuth,
            ),
            ("GET", "/api/providers/chatgpt/models", Native::ChatgptOAuth),
            // sbstndalton/noevia#2190 round 6: the provider registry.
            ("GET", "/api/providers", Native::Providers),
            ("POST", "/api/providers", Native::Providers),
            ("PUT", "/api/providers/mine", Native::Providers),
            ("DELETE", "/api/providers/mine", Native::Providers),
            ("DELETE", "/api/providers/chatgpt", Native::Providers),
            ("PUT", "/api/providers/test", Native::Providers),
            ("DELETE", "/api/providers/test", Native::Providers),
            ("GET", "/api/providers/mine/models", Native::ProviderModels),
            ("POST", "/api/providers/mine/models", Native::ProviderModels),
            // sbstndalton/noevia#2190 round 4: the save.
            (
                "PUT",
                "/api/integrations/storage",
                Native::StorageIntegrations,
            ),
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
