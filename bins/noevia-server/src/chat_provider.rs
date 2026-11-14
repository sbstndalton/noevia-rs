//! A native chat turn on a provider row (council p6-1, the p3-3 CONTINUE): what chat_native.rs needs to run a
//! handed-off turn whose `prepared` frame names a provider other than the default one, with
//! `crates/chat-upstream`'s [`ProviderHost`] around the front's own host.
//!
//! Inert: core hands a provider-row turn off only to a front whose hand-off caps name [`CAPS_TOKEN`], and this
//! front names it only with [`SWITCH`] set to `1` (unset in every deployment; no core understands the token yet).
//! Without it chat_native.rs runs every frame as before.
//!
//! What a provider-row turn gets here, as chat.cjs does for a non-default row:
//! - [`plan`] at the cut: the row looked up as the signed-in user now (the merged view, exact id), the target
//!   resolved and bound by [`chat_upstream::cut::cut`] in Node's order; the frame must agree with it (the
//!   bound address is the frame's `upstreamUrl`; no lease, no model manager, no title; the runtime limit is
//!   [`chat_upstream::limit::hosted_limit`] of the row). Anything else ends the turn with the native failure
//!   text before a byte goes out (fail closed; core already checked the same row, so a disagreement means the
//!   row changed in between or core and this front differ).
//! - [`FrontRows`] per request: the account and the row as they are now (the binding's re-check), and the row's
//!   key unsealed now (a rotated key is picked up; an unreadable one sends no bearer, as `openProviderKey`).
//! - [`provider_client`]: an engine client that follows no redirect (undici `redirect: 'error'`), so a 3xx
//!   reaches [`ProviderHost`] as an answer and is refused instead of followed.
//!
//! Not in this slice (core keeps those turns, [`chat_upstream::refusal`]): Inference machines, the sign-in
//! provider's broker and route targets.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chat_turn::prepared::Prepared;
use chat_upstream::binding::Binding;
use chat_upstream::cut::{self, Cut, NotCut};
use chat_upstream::host::{Rows, RowsNow};
use chat_upstream::target::{Connection, Turn, User};
use js_json::JValue;
use server_auth::secrets::SecretKeys;

use crate::provider_files::CHATGPT_KIND;
use crate::App;

/// The hand-off capability that says this front runs provider-row turns (core rust-chat-native.cjs, to come).
pub const CAPS_TOKEN: &str = "providers";
/// The front's switch for offering [`CAPS_TOKEN`]: `1` offers it; unset or anything else does not.
pub const SWITCH: &str = "NOEVIA_RUST_CHAT_PROVIDERS";
/// How long a provider-row turn's binding holds after the cut. Node binds nothing; a native turn's longest
/// sensible life (rounds, tool runs of up to 15 minutes each, the fallback and the summary) is well inside it.
pub const BINDING_MS: f64 = 2.0 * 60.0 * 60.0 * 1000.0;

/// Why a provider-row frame is not run (logged by chat_native.rs; the client sees the native failure text).
pub const ROW_MISSING: &str = "the frame's provider is not one of this account's rows";
pub const NO_ACCOUNT: &str = "the turn's account is not an active account";
pub const LOCAL_ENGINE: &str =
    "the frame asks for the local engine's lease, model manager or title on a provider row";
pub const ADDRESS: &str = "the frame's upstreamUrl is not the row's address";
pub const LIMIT: &str = "the frame's runtimeLimit is not the row's hosted limit";
pub const NODE_REFUSES: &str = "Node refuses this row's turn (sign-in or member origin)";
pub const NODE_THROWS: &str = "the row has no text baseUrl";
pub const UNSUPPORTED: &str = "the row needs a transport this front does not run (machine or broker)";

/// Whether this front offers [`CAPS_TOKEN`].
pub fn offered() -> bool {
    offered_by(std::env::var(SWITCH).ok().as_deref())
}

/// [`offered`] for a given value of [`SWITCH`].
pub fn offered_by(value: Option<&str>) -> bool {
    value.map(str::trim) == Some("1")
}

/// The hand-off caps this front sends core: chat-turn's own, plus [`CAPS_TOKEN`] when `offer`.
pub fn caps(offer: bool) -> String {
    if offer {
        format!("{},{CAPS_TOKEN}", chat_turn::handoff::HANDOFF_CAPS)
    } else {
        chat_turn::handoff::HANDOFF_CAPS.to_string()
    }
}

/// core ssrf.cjs `MEMBER_OUTBOUND_ORIGINS`, read per call ("" when unset).
fn member_origins() -> String {
    std::env::var("MEMBER_OUTBOUND_ORIGINS").unwrap_or_default()
}

/// What [`plan`] reads besides the frame.
pub struct Facts<'a> {
    /// [`offered`], as of the hand-off request.
    pub offered: bool,
    /// core `DEFAULT_PROVIDER_ID`.
    pub default_id: &'a str,
    /// The turn's account as the account store says now (`None`: removed or disabled).
    pub user: Option<&'a User>,
    /// The row with the frame's provider id, looked up as this account now (merged view, exact id).
    pub row: Option<&'a JValue>,
    pub member_origins: &'a str,
    pub now_ms: f64,
}

/// How chat_native.rs runs a frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// The front's host as it is: the default provider, or a front not offering [`CAPS_TOKEN`] (today's path).
    Plain,
    /// The provider host around the front's, bound by this cut.
    Provider(Cut),
    /// Nothing is sent; the turn ends with the native failure text. The reason is for the log.
    Refuse(&'static str),
}

/// The plan for frame `p` (see the module docs).
pub fn plan(p: &Prepared, f: &Facts<'_>) -> Plan {
    if !f.offered || p.provider.id == f.default_id {
        return Plan::Plain;
    }
    let Some(user) = f.user else {
        return Plan::Refuse(NO_ACCOUNT);
    };
    let Some(row) = f.row else {
        return Plan::Refuse(ROW_MISSING);
    };
    if p.manager_enabled || p.model_lease || p.title.enabled {
        return Plan::Refuse(LOCAL_ENGINE);
    }
    let turn = Turn {
        row,
        default_id: f.default_id,
        user: Some(user),
        member_origins: f.member_origins,
        machine: None,
        sign_in: row.get("kind").as_str() == Some(CHATGPT_KIND),
        // The sign-in provider's transport is not this slice's: read as off, it never reaches the broker.
        sign_in_enabled: false,
        connection: Connection::Missing,
        workspace_user: user.id.as_deref(),
        chat_id: p.chat_id.as_deref(),
        space_id: p.space_id.as_deref(),
    };
    let c = match cut::cut(&turn, &p.model, p.chat_key.as_deref(), f.now_ms + BINDING_MS) {
        Ok(c) => c,
        Err(NotCut::Node(_)) => return Plan::Refuse(NODE_REFUSES),
        Err(NotCut::Throws) => return Plan::Refuse(NODE_THROWS),
        Err(NotCut::NoUser) => return Plan::Refuse(NO_ACCOUNT),
        Err(NotCut::Unsupported(_)) => return Plan::Refuse(UNSUPPORTED),
    };
    if c.target.url != p.upstream_url {
        return Plan::Refuse(ADDRESS);
    }
    let (tokens, source) = chat_upstream::limit::hosted_limit(row);
    if p.runtime_limit.as_ref() != Some(&(tokens, source.to_string())) {
        return Plan::Refuse(LIMIT);
    }
    Plan::Provider(c)
}

/// The account store as the re-check reads it.
pub trait Accounts: Send + Sync {
    /// The account `id` if it is active (not removed, not disabled), with whether it is an administrator;
    /// `None` also when the store cannot be read (fail closed).
    fn active(&self, id: &str) -> Option<User>;
}

/// The front's account store: core's `asUser` (a `users` row that is not disabled) and its role.
pub struct AppAccounts(pub Arc<App>);

impl Accounts for AppAccounts {
    fn active(&self, id: &str) -> Option<User> {
        let writer = self.0.writes.writer().ok()?;
        let role = writer
            .read(|r| {
                r.row(
                    "SELECT role FROM users WHERE id=?1 AND (disabled_at IS NULL OR disabled_at=0)",
                    [id],
                    |row| row.get::<_, String>(0),
                )
            })
            .ok()??;
        Some(User {
            id: Some(id.to_string()),
            admin: role == "admin",
        })
    }
}

/// The row with `id` as `user` sees it now (provider_test_routes.rs `find_row`: the shared file, then the
/// account's own rows; the env-derived default row never), without its key.
pub fn row_of(data_dir: &Path, user: &str, id: &str, default_id: &str) -> Option<JValue> {
    if id == default_id {
        return None;
    }
    crate::provider_test_routes::find_row(data_dir, user, id, None).map(|r| r.raw)
}

/// [`Rows`] over the front's provider store and account store, for one turn's account.
pub struct FrontRows<A> {
    accounts: A,
    data_dir: PathBuf,
    user: String,
    default_id: String,
    keys: Option<SecretKeys>,
}

impl<A: Accounts> FrontRows<A> {
    /// `keys`: the secrets keys as loaded at the cut (`None`: no key file; every row's key then reads as none).
    pub fn new(
        accounts: A,
        data_dir: PathBuf,
        user: String,
        default_id: String,
        keys: Option<SecretKeys>,
    ) -> Self {
        FrontRows {
            accounts,
            data_dir,
            user,
            default_id,
            keys,
        }
    }
}

impl<A: Accounts> Rows for FrontRows<A> {
    fn now(&self, binding: &Binding) -> RowsNow {
        let user = self.accounts.active(&self.user);
        // The row only for an active account (an inactive one fails the re-check on the user anyway).
        let row = user
            .as_ref()
            .and_then(|_| row_of(&self.data_dir, &self.user, &binding.provider_id, &self.default_id));
        // No account: the re-check reads no user, inactive (UserChanged).
        cut::rows_now(
            user.as_ref(),
            user.is_some(),
            row.as_ref(),
            &self.default_id,
            &member_origins(),
            binding,
        )
    }

    fn key(&self, provider_id: &str) -> Option<String> {
        if provider_id == self.default_id {
            return None;
        }
        let keys = self.keys.as_ref()?;
        let row =
            crate::provider_test_routes::find_row(&self.data_dir, &self.user, provider_id, Some(keys))?;
        Some(row.key.as_str().to_string()).filter(|k| !k.is_empty())
    }
}

/// The secrets keys as provider_test_routes.rs loads them (`SECRETS_KEY_PREVIOUS` for the previous key).
pub fn load_keys(data_dir: &Path) -> Option<SecretKeys> {
    let previous = std::env::var("SECRETS_KEY_PREVIOUS")
        .ok()
        .map(zeroize::Zeroizing::new);
    server_auth::secrets::load_existing(data_dir, previous.as_deref().map(String::as_str)).ok()
}

/// The facts of [`plan`] for `user`'s turn on `provider_id`, read now (blocking: the account store and the
/// provider files).
pub fn read_facts<A: Accounts>(
    accounts: &A,
    data_dir: &Path,
    user: &str,
    provider_id: &str,
    default_id: &str,
) -> (Option<User>, Option<JValue>) {
    let account = accounts.active(user);
    let row = account
        .as_ref()
        .and_then(|_| row_of(data_dir, user, provider_id, default_id));
    (account, row)
}

/// One client for every provider-row turn's calls: no proxy from the environment (as Node's fetch) and no
/// redirect followed (a 3xx comes back as the answer, which [`ProviderHost`] refuses as undici does).
/// `None` when it cannot be built: the turn is refused, never sent with a client that follows redirects.
///
/// [`ProviderHost`]: chat_upstream::host::ProviderHost
pub fn provider_client() -> Option<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()
        })
        .clone()
}

/// Why a provider-row turn has no client (see [`provider_client`]).
pub const NO_CLIENT: &str = "the redirect-refusing engine client could not be built";

/// The member origins for [`Facts::member_origins`] as of now.
pub fn origins_now() -> String {
    member_origins()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_upstream::binding::{recheck, Now, Recheck};
    use std::sync::Mutex;

    const USER: &str = "00000000-0000-4000-8000-0000000000aa";
    const URL: &str = "https://qa-provider.example/v1/chat/completions";

    /// An account store from a script: the account's state as of each read.
    struct Script(Mutex<Option<User>>);

    impl Script {
        fn member() -> Script {
            Script(Mutex::new(Some(User {
                id: Some(USER.into()),
                admin: false,
            })))
        }
        fn set(&self, u: Option<User>) {
            *self.0.lock().unwrap() = u;
        }
    }

    impl Accounts for &Script {
        fn active(&self, _id: &str) -> Option<User> {
            self.0.lock().unwrap().clone()
        }
    }

    fn keys() -> SecretKeys {
        SecretKeys::from_existing_bytes(zeroize::Zeroizing::new(vec![7u8; 32]), None).unwrap()
    }

    fn sealed(k: &SecretKeys, plain: &str) -> String {
        k.seal(plain, None, &[1u8; 12]).unwrap()
    }

    fn write_rows(dir: &Path, shared: bool, rows: &str) {
        let file = if shared {
            dir.join("shared-providers.json")
        } else {
            let d = dir.join("users").join(USER);
            std::fs::create_dir_all(&d).unwrap();
            d.join("providers.json")
        };
        std::fs::write(file, format!(r#"{{"providers":[{rows}]}}"#)).unwrap();
    }

    fn own_row(key: &str, base: &str, extra: &str) -> String {
        format!(r#"{{"id":"qa-provider","label":"QA provider","baseUrl":"{base}","apiKey":"{key}"{extra}}}"#)
    }

    fn frame(extra: &str) -> Prepared {
        let f = js_json::parse(&format!(
            r#"{{"type":"prepared","v":1,"startedAt":1,"model":"qa-model","effort":"default",
"provider":{{"id":"qa-provider","label":"QA provider","localEngine":false,"capabilities":null}},
"upstreamUrl":"{URL}","limitScope":"qa","messages":[],"tools":[],"routes":[],
"writes":[],"public":[],"automaticOnlyWrites":[],"managerEnabled":false,"modelLease":false,
"chat":{{"id":"qa-chat","key":"qa-chat","grantScope":"chat","grantHolder":null,"recentWrites":[],
"title":{{"chatId":"qa-chat","firstMessage":"QA","enabled":false}}}},
"turn":{{"message":"QA","streamIdleMs":0}}{extra}}}"#
        ))
        .unwrap();
        Prepared::read(&f).unwrap_or_else(|e| panic!("{e:?}"))
    }

    const HOSTED: &str = r#","runtimeLimit":{"tokens":32768,"source":"Default for hosted providers"}"#;

    fn row(json: &str) -> JValue {
        js_json::parse(json).unwrap()
    }

    fn facts<'a>(user: Option<&'a User>, row: Option<&'a JValue>, origins: &'a str) -> Facts<'a> {
        Facts {
            offered: true,
            default_id: "default",
            user,
            row,
            member_origins: origins,
            now_ms: 1_000.0,
        }
    }

    fn member() -> User {
        User {
            id: Some(USER.into()),
            admin: false,
        }
    }

    #[test]
    fn the_switch_is_one_and_the_caps_name_providers_only_then() {
        for (v, on) in [
            (None, false),
            (Some(""), false),
            (Some("0"), false),
            (Some("true"), false),
            (Some("yes"), false),
            (Some("1"), true),
            (Some(" 1 "), true),
        ] {
            assert_eq!(offered_by(v), on, "{v:?}");
        }
        assert_eq!(caps(false), chat_turn::handoff::HANDOFF_CAPS);
        assert_eq!(caps(true), format!("{},providers", chat_turn::handoff::HANDOFF_CAPS));
        assert!(!caps(false).split(',').any(|t| t == CAPS_TOKEN));
    }

    #[test]
    fn without_the_offer_or_on_the_default_row_the_turn_runs_as_before() {
        let u = member();
        let r = row(&own_row("", "https://qa-provider.example/v1", ""));
        let mut f = facts(Some(&u), Some(&r), "https://qa-provider.example");
        f.offered = false;
        // Not offered: today's path, whatever the frame says (core never hands such a turn off without it).
        assert_eq!(plan(&frame(HOSTED), &f), Plan::Plain);
        assert_eq!(plan(&frame(""), &f), Plan::Plain);
        f.offered = true;
        let mut local = frame("");
        local.provider.id = "default".into();
        assert_eq!(plan(&local, &f), Plan::Plain);
    }

    #[test]
    fn a_member_own_row_is_cut_and_bound_to_the_frame() {
        let u = member();
        let r = row(&own_row("", "https://qa-provider.example/v1/", ""));
        let p = frame(HOSTED);
        let Plan::Provider(c) = plan(&p, &facts(Some(&u), Some(&r), "https://qa-provider.example"))
        else {
            panic!("not cut")
        };
        assert_eq!(c.target.url, URL);
        assert_eq!(c.binding.user, USER);
        assert_eq!(c.binding.provider_id, "qa-provider");
        assert_eq!(c.binding.model, "qa-model");
        assert_eq!(c.binding.chat_key.as_deref(), Some("qa-chat"));
        assert!((c.binding.expires_ms - (1_000.0 + BINDING_MS)).abs() < f64::EPSILON);
        // A stated context is the limit the frame must carry.
        let r = row(&own_row("", "https://qa-provider.example", r#","contextTokens":65536"#));
        let f = facts(Some(&u), Some(&r), "https://qa-provider.example");
        assert_eq!(plan(&p, &f), Plan::Refuse(LIMIT));
        let stated = frame(r#","runtimeLimit":{"tokens":65536,"source":"Provider setting"}"#);
        assert!(matches!(plan(&stated, &f), Plan::Provider(_)));
    }

    #[test]
    fn a_frame_that_disagrees_with_the_row_is_refused() {
        let u = member();
        let r = row(&own_row("", "https://qa-provider.example/v1", ""));
        let f = facts(Some(&u), Some(&r), "https://qa-provider.example");
        assert_eq!(plan(&frame(""), &f), Plan::Refuse(LIMIT), "no runtimeLimit");
        let mut leased = frame(HOSTED);
        leased.model_lease = true;
        assert_eq!(plan(&leased, &f), Plan::Refuse(LOCAL_ENGINE));
        let mut titled = frame(HOSTED);
        titled.title.enabled = true;
        assert_eq!(plan(&titled, &f), Plan::Refuse(LOCAL_ENGINE));
        let mut managed = frame(HOSTED);
        managed.manager_enabled = true;
        assert_eq!(plan(&managed, &f), Plan::Refuse(LOCAL_ENGINE));
        let mut moved = frame(HOSTED);
        moved.upstream_url = "https://elsewhere.example/v1/chat/completions".into();
        assert_eq!(plan(&moved, &f), Plan::Refuse(ADDRESS));
        // The row with a machine address the frame was moved to is not the frame's: refused the same way.
        let r2 = row(&own_row("", "https://qa-provider.example/other", ""));
        let f2 = facts(Some(&u), Some(&r2), "https://qa-provider.example");
        assert_eq!(plan(&frame(HOSTED), &f2), Plan::Refuse(ADDRESS));
    }

    #[test]
    fn node_refusals_and_missing_facts_are_refused() {
        let u = member();
        let r = row(&own_row("", "https://qa-provider.example/v1", ""));
        // A member's own row whose origin is not approved: Node answers 400.
        assert_eq!(
            plan(&frame(HOSTED), &facts(Some(&u), Some(&r), "")),
            Plan::Refuse(NODE_REFUSES)
        );
        // An administrator's turn is not held to the member list.
        let admin = User {
            id: Some(USER.into()),
            admin: true,
        };
        assert!(matches!(
            plan(&frame(HOSTED), &facts(Some(&admin), Some(&r), "")),
            Plan::Provider(_)
        ));
        // A shared row is not the member's own: no origin check.
        let shared = row(&own_row("", "https://qa-provider.example/v1", r#","shared":true"#));
        assert!(matches!(
            plan(&frame(HOSTED), &facts(Some(&u), Some(&shared), "")),
            Plan::Provider(_)
        ));
        assert_eq!(
            plan(&frame(HOSTED), &facts(None, Some(&r), "https://qa-provider.example")),
            Plan::Refuse(NO_ACCOUNT)
        );
        assert_eq!(
            plan(&frame(HOSTED), &facts(Some(&u), None, "https://qa-provider.example")),
            Plan::Refuse(ROW_MISSING)
        );
        let no_base = row(r#"{"id":"qa-provider","label":"QA provider","baseUrl":7}"#);
        assert_eq!(
            plan(&frame(HOSTED), &facts(Some(&u), Some(&no_base), "https://qa-provider.example")),
            Plan::Refuse(NODE_THROWS)
        );
        // The sign-in provider: not this slice (read as turned off, Node's own 409).
        let sign_in = row(
            r#"{"id":"qa-provider","kind":"chatgpt-oauth","label":"QA","baseUrl":"https://qa-provider.example"}"#,
        );
        assert!(matches!(
            plan(&frame(HOSTED), &facts(Some(&u), Some(&sign_in), "https://qa-provider.example")),
            Plan::Refuse(NODE_REFUSES | UNSUPPORTED)
        ));
    }

    fn rechecked(rows: &FrontRows<&Script>, b: &Binding) -> Recheck {
        let n = rows.now(b);
        recheck(
            b,
            &Now {
                user: &n.user,
                user_active: n.user_active,
                row: n.row.as_ref(),
                member_own: n.member_own,
                origin_approved: n.origin_approved,
            },
        )
    }

    /// The re-check over the real provider files: passes as cut; a moved address, a deleted row, a disabled
    /// account each fail it; the key is unsealed per request (rotation picked up; unreadable or empty: none).
    #[test]
    fn front_rows_read_the_files_as_they_are_now() {
        let dir = tempfile::tempdir().unwrap();
        let k = keys();
        write_rows(
            dir.path(),
            false,
            &own_row(&sealed(&k, "qa-key-1"), "https://qa-provider.example/v1", ""),
        );
        // An administrator: the member origin list (read from the environment per request) does not apply.
        let admin = User {
            id: Some(USER.into()),
            admin: true,
        };
        let script = Script(Mutex::new(Some(admin.clone())));
        let found = row_of(dir.path(), USER, "qa-provider", "default").expect("own row");
        let Plan::Provider(c) = plan(&frame(HOSTED), &facts(Some(&admin), Some(&found), "")) else {
            panic!("not cut")
        };
        let rows = FrontRows::new(
            &script,
            dir.path().to_path_buf(),
            USER.into(),
            "default".into(),
            Some(keys()),
        );
        assert_eq!(rechecked(&rows, &c.binding), Recheck::Ok);
        assert_eq!(rows.key("qa-provider").as_deref(), Some("qa-key-1"));
        assert_eq!(rows.key("default"), None, "the default row's key is never read here");

        // Key rotated: the next request reads the new one; the binding still holds (the key is not pinned).
        write_rows(
            dir.path(),
            false,
            &own_row(&sealed(&k, "qa-key-2"), "https://qa-provider.example/v1", ""),
        );
        assert_eq!(rechecked(&rows, &c.binding), Recheck::Ok);
        assert_eq!(rows.key("qa-provider").as_deref(), Some("qa-key-2"));

        // Sealed under another key: unreadable, so no bearer (openProviderKey gives '').
        let other = SecretKeys::from_existing_bytes(zeroize::Zeroizing::new(vec![9u8; 32]), None).unwrap();
        write_rows(
            dir.path(),
            false,
            &own_row(&sealed(&other, "qa-key-3"), "https://qa-provider.example/v1", ""),
        );
        assert_eq!(rows.key("qa-provider"), None);
        write_rows(dir.path(), false, &own_row("", "https://qa-provider.example/v1", ""));
        assert_eq!(rows.key("qa-provider"), None);
        let no_keys = FrontRows::new(
            &script,
            dir.path().to_path_buf(),
            USER.into(),
            "default".into(),
            None,
        );
        assert_eq!(no_keys.key("qa-provider"), None);

        // The address moved: the settings changed during this reply.
        write_rows(dir.path(), false, &own_row("", "https://qa-moved.example/v1", ""));
        assert_ne!(rechecked(&rows, &c.binding), Recheck::Ok);
        // The row deleted.
        write_rows(dir.path(), false, "");
        assert_eq!(rechecked(&rows, &c.binding), Recheck::RowGone);
        // Back, then the account disabled.
        write_rows(dir.path(), false, &own_row("", "https://qa-provider.example/v1", ""));
        assert_eq!(rechecked(&rows, &c.binding), Recheck::Ok);
        script.set(None);
        assert_eq!(rechecked(&rows, &c.binding), Recheck::UserChanged);
    }

    /// A shared row is found in the shared file and reads as shared; the account's own row with the same id
    /// does not shadow it (provider_test_routes.rs `find_row`'s order), and the default id is never a row here.
    #[test]
    fn shared_rows_and_the_default_id() {
        let dir = tempfile::tempdir().unwrap();
        write_rows(
            dir.path(),
            true,
            r#"{"id":"qa-provider","label":"QA shared","baseUrl":"https://qa-shared.example"}"#,
        );
        write_rows(dir.path(), false, &own_row("", "https://qa-own.example", ""));
        let r = row_of(dir.path(), USER, "qa-provider", "default").unwrap();
        assert_eq!(r.get("baseUrl").as_str(), Some("https://qa-shared.example"));
        assert!(r.get("shared").truthy());
        assert_eq!(row_of(dir.path(), USER, "default", "default"), None);
        assert_eq!(row_of(dir.path(), USER, "qa-missing", "default"), None);
        let script = Script::member();
        let (account, row) = read_facts(&&script, dir.path(), USER, "qa-provider", "default");
        assert_eq!(account, Some(member()));
        assert!(row.is_some());
        script.set(None);
        assert_eq!(
            read_facts(&&script, dir.path(), USER, "qa-provider", "default"),
            (None, None),
            "no row is read for an inactive account"
        );
    }

    #[test]
    fn the_provider_client_is_built() {
        assert!(provider_client().is_some());
    }
}
