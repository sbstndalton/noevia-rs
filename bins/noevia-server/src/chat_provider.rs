//! A native chat turn on a provider row (council p6-1, the p3-3 CONTINUE): what chat_native.rs needs to run a
//! handed-off turn whose `prepared` frame names a provider other than the default one, with
//! `crates/chat-upstream`'s [`ProviderHost`] around the front's own host.
//!
//! Inert: core hands a provider-row turn off only to a front whose hand-off caps name [`CAPS_TOKEN`], and this
//! front names it only with [`SWITCH`] set to `1` (unset in every deployment) in a build with the provider store
//! (`rust-storage-reads`). Core main refuses every non-default row; the core half (noevia-core
//! council/p8-3-core-provider-rows, rust-chat-native.cjs `plainProviderRow`) lifts that for a plain direct keyed
//! row only (no machine, external row, sign-in provider or route target) and writes the frame facts [`plan`]
//! checks. Without the token chat_native.rs runs every frame as before.
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
//! - [`provider_client`]: an engine client that follows no redirect (Node's fetch with `redirect: 'error'`), so a 3xx
//!   reaches [`ProviderHost`] as an answer and is refused instead of followed.
//!
//! Not in this slice (core keeps those turns, [`chat_upstream::refusal`]): Inference machines and route targets.
//!
//! The sign-in provider's row (council p10-1, p9-3 CONTINUE 2) is planned here too, behind two locks: the front
//! names [`SIGNIN_CAPS_TOKEN`] only with [`SIGNIN_SWITCH`] set to `1` beside [`SWITCH`], **and** only once
//! [`EXTERNAL_EGRESS_CLEARED`] is true (it is false: an external row's tool calls still need the external-provider
//! rules shown to apply at the tool bridge, noevia#2731 item 5, and the sign-in row is an external row). With both,
//! [`plan`] resolves the row in Node's order (the sign-in checks: on, owner, connected), then
//! [`crate::signin_drive::plan`] binds it, and chat_native.rs runs it over [`crate::signin_host::SigninHost`]
//! through [`SigninTurn::run`]. Until then a sign-in frame is refused as before ([`NODE_REFUSES`] when the front did
//! not name the token, [`EXTERNAL`] in a build that clears the rules but before the front-side switch).

use std::path::Path;
use std::sync::Arc;

use chat_turn::host::Host;
use chat_turn::native::{Ended, Helpers, Process};
use chat_turn::prepared::Prepared;
use chat_upstream::cut::{self, Cut, NotCut};
use chat_upstream::host::Unsupported;
use chat_upstream::target::{Connection, Turn, User};
use js_json::JValue;
use server_auth::secrets::SecretKeys;

use crate::provider_files::CHATGPT_KIND;
use crate::App;

/// The hand-off capability that says this front runs provider-row turns (core rust-chat-native.cjs `plainProviderRow`).
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
pub const EXTERNAL: &str =
    "the row is an external provider (its tool calls need the external-provider rules at the tool bridge)";
pub const UNSUPPORTED: &str =
    "the row needs a transport this front does not run (machine or broker)";

/// The sign-in provider's hand-off capability (chat-upstream `refusal::SIGNIN_CAP`, signin_wire.rs
/// `CAPS_TOKEN`): core lifts the sign-in row only for a front that names it beside [`CAPS_TOKEN`].
pub const SIGNIN_CAPS_TOKEN: &str = "signin-transport-v1";
/// The front's switch for naming [`SIGNIN_CAPS_TOKEN`] (beside [`SWITCH`]): `1` names it; unset or anything else
/// does not. It takes effect only once [`EXTERNAL_EGRESS_CLEARED`] is true.
pub const SIGNIN_SWITCH: &str = "NOEVIA_RUST_CHAT_SIGNIN";
/// Whether a handed-off turn on an external row is shown to get chat.cjs's external-provider rules at the tool
/// bridge (`egressToolRefusal`; noevia#2731 item 5). False: no external row runs here, the sign-in row included,
/// whatever the switches say. Turning it true is its own reviewed change, with that check's tests.
pub const EXTERNAL_EGRESS_CLEARED: bool = false;

/// The FEATURES entry of a build that runs provider-row turns ([`CAPS_TOKEN`]; the provider store,
/// `rust-storage-reads`). The release assembly pairs it with core's half (release-assemble `chat_rows`): core must
/// name the `providers` cap and its `plainProviderRow`, else the front's [`SWITCH`] would change nothing.
pub const PROVIDERS_FEATURE: &str = "chat-providers-v1";
/// The FEATURES entry of a build that runs sign-in row turns ([`SIGNIN_CAPS_TOKEN`]). It is listed only once
/// [`EXTERNAL_EGRESS_CLEARED`] is true (tests::features): a front that can never name the token does not claim it,
/// so the release assembly pairs nothing for it before then. The change that clears the rules adds it.
pub const SIGNIN_FEATURE: &str = "chat-signin-v1";

/// Whether this front names [`SIGNIN_CAPS_TOKEN`]: never without the sign-in transport (`rust-chatgpt-status`),
/// [`offered`] and [`EXTERNAL_EGRESS_CLEARED`].
pub fn signin_offered() -> bool {
    cfg!(feature = "rust-chatgpt-status")
        && signin_offered_by(
            offered(),
            EXTERNAL_EGRESS_CLEARED,
            std::env::var(SIGNIN_SWITCH).ok().as_deref(),
        )
}

/// [`signin_offered`] for given facts: the provider caps offered, the rules cleared, the switch's value.
pub fn signin_offered_by(providers: bool, cleared: bool, value: Option<&str>) -> bool {
    providers && cleared && offered_by(value)
}

/// Whether this front offers [`CAPS_TOKEN`]: never in a build without the provider store (the
/// `rust-storage-reads` feature, which holds provider_test_routes.rs's row reader).
pub fn offered() -> bool {
    cfg!(feature = "rust-storage-reads") && offered_by(std::env::var(SWITCH).ok().as_deref())
}

/// [`offered`] for a given value of [`SWITCH`].
pub fn offered_by(value: Option<&str>) -> bool {
    value.map(str::trim) == Some("1")
}

/// The hand-off caps this front sends core: chat-turn's own, plus [`CAPS_TOKEN`] when `offer`.
pub fn caps(offer: bool) -> String {
    front_caps(offer, false)
}

/// [`caps`], plus [`SIGNIN_CAPS_TOKEN`] when `signin` too (never without `offer`: core lifts the sign-in row only
/// for a front naming both).
pub fn front_caps(offer: bool, signin: bool) -> String {
    match (offer, signin) {
        (true, true) => format!(
            "{},{CAPS_TOKEN},{SIGNIN_CAPS_TOKEN}",
            chat_turn::handoff::HANDOFF_CAPS
        ),
        (true, false) => format!("{},{CAPS_TOKEN}", chat_turn::handoff::HANDOFF_CAPS),
        (false, _) => chat_turn::handoff::HANDOFF_CAPS.to_string(),
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
    /// The sign-in row's facts ([`Signin::OFF`] for every other row).
    pub signin: Signin<'a>,
}

/// What [`plan`] reads for a sign-in row besides the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signin<'a> {
    /// The hand-off caps this front named for the turn ([`front_caps`]); the sign-in row is planned only when they
    /// name [`SIGNIN_CAPS_TOKEN`] (core lifted it for exactly that front).
    pub caps: &'a str,
    /// core `chatgptOAuth && chatgptEnabled()`: the front has a broker and the sign-in flag is on.
    pub enabled: bool,
    /// The broker's connection for the turn's account (core `chatgptOAuth.status(owner).state`).
    pub connection: Connection,
    /// [`EXTERNAL_EGRESS_CLEARED`] (a field so the tests run the cleared path; the front passes the constant).
    pub cleared: bool,
}

impl Signin<'static> {
    /// No sign-in facts: the front named no sign-in token.
    pub const OFF: Signin<'static> = Signin {
        caps: "",
        enabled: false,
        connection: Connection::Missing,
        cleared: EXTERNAL_EGRESS_CLEARED,
    };
}

/// The broker's state word (`Broker::connection`) as the target rules read it: `None` (the store unreadable) and
/// `disconnected` are a missing connection (core answers both 409).
pub fn connection_of(state: Option<&str>) -> Connection {
    match state {
        Some("connected") => Connection::Connected,
        Some("reconnect") => Connection::Reconnect,
        _ => Connection::Missing,
    }
}

/// The sign-in turn [`plan`] binds (crate::signin_drive's, in a build with the sign-in transport).
#[cfg(feature = "rust-chatgpt-status")]
pub use crate::signin_drive::SigninPlan;
/// A build without the sign-in transport never plans a sign-in turn (no value of this type exists).
#[cfg(not(feature = "rust-chatgpt-status"))]
#[derive(Debug, Clone, PartialEq)]
pub enum SigninPlan {}

/// Whether `caps` name [`SIGNIN_CAPS_TOKEN`] (signin_wire.rs `offered`'s reading; never without the transport).
fn names_signin(caps: &str) -> bool {
    #[cfg(feature = "rust-chatgpt-status")]
    {
        crate::signin_wire::offered(caps)
    }
    #[cfg(not(feature = "rust-chatgpt-status"))]
    {
        let _ = caps;
        false
    }
}

/// How chat_native.rs runs a frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// The front's host as it is: the default provider, or a front not offering [`CAPS_TOKEN`] (today's path).
    Plain,
    /// The provider host around the front's, bound by this cut (boxed: clippy large_enum_variant).
    Provider(Box<Cut>),
    /// The sign-in row's turn over the front's broker, bound by this plan (only for a front that named
    /// [`SIGNIN_CAPS_TOKEN`], with [`EXTERNAL_EGRESS_CLEARED`]).
    Signin(Box<SigninPlan>),
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
    let sign_in = row.get("kind").as_str() == Some(CHATGPT_KIND);
    // The sign-in row is this front's only when it named the sign-in token (core lifted it for that front only).
    let signin_turn = sign_in && names_signin(f.signin.caps);
    let turn = Turn {
        row,
        default_id: f.default_id,
        user: Some(user),
        member_origins: f.member_origins,
        machine: None,
        sign_in,
        // Without the sign-in token the transport is read as off: Node's own 409, it never reaches the broker.
        sign_in_enabled: signin_turn && f.signin.enabled,
        connection: if signin_turn {
            f.signin.connection
        } else {
            Connection::Missing
        },
        workspace_user: user.id.as_deref(),
        chat_id: p.chat_id.as_deref(),
        space_id: p.space_id.as_deref(),
    };
    if signin_turn {
        return plan_signin(p, f, &turn, user);
    }
    let c = match cut::cut(
        &turn,
        &p.model,
        p.chat_key.as_deref(),
        f.now_ms + BINDING_MS,
    ) {
        Ok(c) => c,
        Err(NotCut::Node(_)) => return Plan::Refuse(NODE_REFUSES),
        Err(NotCut::Throws) => return Plan::Refuse(NODE_THROWS),
        Err(NotCut::NoUser) => return Plan::Refuse(NO_ACCOUNT),
        Err(NotCut::Unsupported(_)) => return Plan::Refuse(UNSUPPORTED),
    };
    // After the cut (the sign-in row is Node's own refusal first). Core keeps an external row's turn (rust-chat-native.cjs refusals: `t.external`), and so does this front
    // (noevia#2731 item 5): its tool calls run through core's bridges, where the external-provider rules of
    // chat.cjs (`egressToolRefusal`) are not yet shown to apply to a handed-off turn.
    if external_row(row) {
        return Plan::Refuse(EXTERNAL);
    }
    if c.target.url != p.upstream_url {
        return Plan::Refuse(ADDRESS);
    }
    let (tokens, source) = chat_upstream::limit::hosted_limit(row);
    if p.runtime_limit.as_ref() != Some(&(tokens, source.to_string())) {
        return Plan::Refuse(LIMIT);
    }
    Plan::Provider(Box::new(c))
}

/// [`plan`] for a sign-in row of a front that named [`SIGNIN_CAPS_TOKEN`], in Node's order: the sign-in checks of
/// the target rules (on, owner, connected: Node's own 409/403 answers, [`NODE_REFUSES`]), the external-row hold
/// ([`EXTERNAL`] until the rules are cleared), the runtime limit, then [`crate::signin_drive::plan`]'s checks (the
/// offer, the owner, the address, the model, the expiry; the local-engine check ran before).
fn plan_signin(p: &Prepared, f: &Facts<'_>, turn: &Turn<'_>, user: &User) -> Plan {
    let target = match chat_upstream::target::resolve(turn) {
        None => return Plan::Refuse(NODE_THROWS),
        Some(Err(_)) => return Plan::Refuse(NODE_REFUSES),
        Some(Ok(t)) => t,
    };
    let chat_upstream::target::Fetch::Broker {
        owner,
        conversation,
    } = &target.fetch
    else {
        return Plan::Refuse(UNSUPPORTED);
    };
    if !f.signin.cleared {
        return Plan::Refuse(EXTERNAL);
    }
    let (tokens, source) = chat_upstream::limit::hosted_limit(turn.row);
    if p.runtime_limit.as_ref() != Some(&(tokens, source.to_string())) {
        return Plan::Refuse(LIMIT);
    }
    signin_plan(p, f, &target.url, owner, conversation.as_deref(), user)
}

#[cfg(feature = "rust-chatgpt-status")]
fn signin_plan(
    p: &Prepared,
    f: &Facts<'_>,
    url: &str,
    owner: &str,
    conversation: Option<&str>,
    user: &User,
) -> Plan {
    let cut = crate::signin_drive::SigninCut {
        owner,
        conversation,
        url,
    };
    match crate::signin_drive::plan(
        f.signin.caps,
        user.id.as_deref().unwrap_or_default(),
        &cut,
        p,
        f.now_ms + BINDING_MS,
    ) {
        Ok(s) => Plan::Signin(Box::new(s)),
        Err(why) => Plan::Refuse(why),
    }
}

/// Never reached: [`names_signin`] is false in this build.
#[cfg(not(feature = "rust-chatgpt-status"))]
fn signin_plan(
    _p: &Prepared,
    _f: &Facts<'_>,
    _url: &str,
    _owner: &str,
    _conversation: Option<&str>,
    _user: &User,
) -> Plan {
    Plan::Refuse(UNSUPPORTED)
}

/// core provider-egress.cjs `isExternalProvider(row)`, an unknown answer counted as external: a row the server
/// flagged external, a sign-in row, or a trial-terms host. The address is read without its user information, as
/// provider-egress.cjs `providerProjection` reads it.
pub fn external_row(row: &JValue) -> bool {
    let text = js_json::to_js_string(row.get("baseUrl")).unwrap_or_default();
    let text = match url::Url::parse(&text) {
        Ok(mut u) if !u.username().is_empty() || u.password().is_some() => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        _ => text,
    };
    let provider = provider_egress::Provider {
        kind: row.get("kind").as_str().map(|k| k.encode_utf16().collect()),
        external: matches!(row.get("external"), JValue::Bool(true)),
        base_url: text.encode_utf16().collect(),
        label: Vec::new(),
    };
    provider_egress::external_strict(Some(&provider))
}

/// How chat_native.rs runs a handed-off frame ([`setup`]).
pub enum Setup {
    /// The front's host as it is: the default row, or a front not offering [`CAPS_TOKEN`] (today's path).
    Plain,
    /// Not run: the reason is logged here, the turn ends with the native failure text before a byte goes out.
    Refused,
    /// A provider row's turn, cut and bound (boxed: clippy large_enum_variant).
    Provider(Box<ProviderTurn>),
    /// The sign-in row's turn, planned and bound, with its engine and account re-check.
    Signin(Box<SigninTurn>),
}

/// A sign-in row's turn as [`setup`] planned it: run it with [`SigninTurn::run`] around the front's host.
#[cfg(feature = "rust-chatgpt-status")]
pub struct SigninTurn {
    plan: SigninPlan,
    engine: crate::signin_turn::SigninEngine<crate::signin_wire::FrontAccounts>,
    check: Box<dyn Fn() -> Result<(), &'static str> + Send + Sync>,
}

/// A build without the sign-in transport never has a sign-in turn.
#[cfg(not(feature = "rust-chatgpt-status"))]
pub enum SigninTurn {}

#[cfg(feature = "rust-chatgpt-status")]
impl SigninTurn {
    /// The turn for `plan` over the front's broker for `user`: `Err` ([`crate::signin_drive::NO_BROKER`]) while the
    /// front has no broker or backend client.
    pub fn new(app: &Arc<App>, user: &str, plan: SigninPlan) -> Result<SigninTurn, &'static str> {
        let engine = crate::signin_drive::front_engine(app, &plan)?;
        let check = crate::signin_drive::account_check(
            crate::signin_drive::front_live(app),
            user.to_string(),
        );
        Ok(SigninTurn {
            plan,
            engine,
            check: Box::new(check),
        })
    }

    /// Runs the rest of the turn on `host` wrapped in the sign-in host. `Err`: the engine is not the plan's
    /// account and conversation (never for what [`SigninTurn::new`] builds; refused all the same).
    pub async fn run<H: Host>(
        self,
        host: &H,
        process: &Process,
        helpers: &Helpers<'_>,
        p: &Prepared,
    ) -> Result<Ended, &'static str> {
        let SigninTurn {
            plan,
            engine,
            check,
        } = self;
        crate::signin_drive::run(host, engine, plan, check, process, helpers, p).await
    }
}

#[cfg(not(feature = "rust-chatgpt-status"))]
impl SigninTurn {
    /// Never called (no value of this type exists).
    pub async fn run<H: Host>(
        self,
        _host: &H,
        _process: &Process,
        _helpers: &Helpers<'_>,
        _p: &Prepared,
    ) -> Result<Ended, &'static str> {
        match self {}
    }
}

/// A provider row's turn as [`setup`] cut it: run it with [`ProviderTurn::run`] on a front host built with
/// [`ProviderTurn::client`].
#[cfg(feature = "rust-storage-reads")]
pub struct ProviderTurn {
    cut: Cut,
    rows: FrontRows<AppAccounts>,
    client: reqwest::Client,
    default_id: String,
}

/// A build without the provider store never offers [`CAPS_TOKEN`], so it never has a provider row's turn.
#[cfg(not(feature = "rust-storage-reads"))]
pub enum ProviderTurn {}

#[cfg(feature = "rust-storage-reads")]
impl ProviderTurn {
    /// The engine client the front host of this turn must use (no proxy, no redirect followed).
    pub fn client(&self) -> reqwest::Client {
        self.client.clone()
    }

    /// Runs the rest of the turn on `host` wrapped in the provider host. `Err`: the provider host does not take
    /// this cut (never for what [`plan`] cuts; the caller refuses the turn all the same).
    pub async fn run<H: Host>(
        self,
        host: &H,
        process: &Process,
        helpers: &Helpers<'_>,
        p: &Prepared,
    ) -> Result<Ended, Unsupported> {
        let ProviderTurn {
            cut,
            rows,
            default_id,
            ..
        } = self;
        let wrapped = chat_upstream::host::ProviderHost::new(
            host,
            &rows,
            cut.target,
            cut.binding,
            &default_id,
        )?;
        Ok(chat_turn::native::run(&wrapped, process, helpers, p).await)
    }
}

#[cfg(not(feature = "rust-storage-reads"))]
impl ProviderTurn {
    /// Never called (no value of this type exists).
    pub fn client(&self) -> reqwest::Client {
        match *self {}
    }

    /// Never called (no value of this type exists).
    pub async fn run<H: Host>(
        self,
        _host: &H,
        _process: &Process,
        _helpers: &Helpers<'_>,
        _p: &Prepared,
    ) -> Result<Ended, Unsupported> {
        match self {}
    }
}

/// The plan for `user`'s handed-off frame `p` as of now (the account store and the provider files read on a
/// blocking thread). Without [`offered`], or on the default row, nothing is read and the frame runs as before.
#[cfg(feature = "rust-storage-reads")]
pub async fn setup(app: &Arc<App>, data_dir: &Path, user: &str, p: &Prepared) -> Setup {
    if !offered() {
        return Setup::Plain;
    }
    let default_id = crate::provider_test_routes::default_provider_id();
    if p.provider.id == default_id {
        return Setup::Plain;
    }
    let (app2, dir, who, id, def) = (
        Arc::clone(app),
        data_dir.to_path_buf(),
        user.to_string(),
        p.provider.id.clone(),
        default_id.clone(),
    );
    let read = tokio::task::spawn_blocking(move || {
        let accounts = AppAccounts(app2);
        let (account, row) = read_facts(&accounts, &dir, &who, &id, &def);
        let keys = row.is_some().then(|| load_keys(&dir)).flatten();
        (account, row, keys)
    })
    .await;
    let Ok((account, row, keys)) = read else {
        eprintln!("chat native: provider turn not run: its facts could not be read");
        return Setup::Refused;
    };
    let origins = origins_now();
    // The caps this front named core for the turn (handoff_headers), read the same way now.
    let caps = front_caps(true, signin_offered());
    let signin = signin_facts(app, user, row.as_ref(), &caps).await;
    #[allow(clippy::cast_precision_loss)]
    let now_ms = crate::identity::now_ms() as f64;
    let facts = Facts {
        offered: true,
        default_id: &default_id,
        user: account.as_ref(),
        row: row.as_ref(),
        member_origins: &origins,
        now_ms,
        signin,
    };
    match plan(p, &facts) {
        Plan::Plain => Setup::Plain,
        Plan::Refuse(why) => {
            eprintln!("chat native: provider turn not run: {why}");
            Setup::Refused
        }
        Plan::Provider(cut) => {
            let Some(client) = provider_client() else {
                eprintln!("chat native: provider turn not run: {NO_CLIENT}");
                return Setup::Refused;
            };
            let rows = FrontRows::new(
                AppAccounts(Arc::clone(app)),
                data_dir.to_path_buf(),
                user.to_string(),
                default_id.clone(),
                keys,
            );
            Setup::Provider(Box::new(ProviderTurn {
                cut: *cut,
                rows,
                client,
                default_id,
            }))
        }
        Plan::Signin(s) => signin_setup(app, user, *s),
    }
}

#[cfg(all(feature = "rust-storage-reads", feature = "rust-chatgpt-status"))]
fn signin_setup(app: &Arc<App>, user: &str, plan: SigninPlan) -> Setup {
    match SigninTurn::new(app, user, plan) {
        Ok(turn) => Setup::Signin(Box::new(turn)),
        Err(why) => {
            eprintln!("chat native: sign-in turn not run: {why}");
            Setup::Refused
        }
    }
}

#[cfg(all(feature = "rust-storage-reads", not(feature = "rust-chatgpt-status")))]
fn signin_setup(_app: &Arc<App>, _user: &str, plan: SigninPlan) -> Setup {
    match plan {}
}

/// The sign-in facts for `row`: [`Signin::OFF`]'s for every row that is not the sign-in kind, or when `caps` do not
/// name [`SIGNIN_CAPS_TOKEN`] (nothing is asked of the broker); else the broker's flag and the account's connection
/// as of now. A front without a broker reads as off (Node's 409).
#[cfg(feature = "rust-storage-reads")]
async fn signin_facts<'a>(
    app: &Arc<App>,
    user: &str,
    row: Option<&JValue>,
    caps: &'a str,
) -> Signin<'a> {
    let off = Signin {
        caps,
        ..Signin::OFF
    };
    let sign_in = row.is_some_and(|r| r.get("kind").as_str() == Some(CHATGPT_KIND));
    if !sign_in || !names_signin(caps) {
        return off;
    }
    broker_facts(app, user, off).await
}

#[cfg(all(feature = "rust-storage-reads", feature = "rust-chatgpt-status"))]
async fn broker_facts<'a>(app: &Arc<App>, user: &str, off: Signin<'a>) -> Signin<'a> {
    let Some(broker) = app.chatgpt_oauth.as_ref() else {
        return off;
    };
    Signin {
        enabled: broker.enabled(app).await,
        connection: connection_of(broker.connection(app, user).await),
        ..off
    }
}

#[cfg(all(feature = "rust-storage-reads", not(feature = "rust-chatgpt-status")))]
#[allow(clippy::unused_async)]
async fn broker_facts<'a>(_app: &Arc<App>, _user: &str, off: Signin<'a>) -> Signin<'a> {
    off
}

/// See the build with the provider store: this one never offers [`CAPS_TOKEN`], so every frame runs as before.
#[cfg(not(feature = "rust-storage-reads"))]
#[allow(clippy::unused_async)]
pub async fn setup(_app: &Arc<App>, _data_dir: &Path, _user: &str, _p: &Prepared) -> Setup {
    Setup::Plain
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

#[cfg(feature = "rust-storage-reads")]
/// The row with `id` as `user` sees it now (provider_test_routes.rs `find_row`: the shared file, then the
/// account's own rows; the env-derived default row never), without its key.
pub fn row_of(data_dir: &Path, user: &str, id: &str, default_id: &str) -> Option<JValue> {
    if id == default_id {
        return None;
    }
    crate::provider_test_routes::find_row(data_dir, user, id, None).map(|r| r.raw)
}

#[cfg(feature = "rust-storage-reads")]
/// [`chat_upstream::host::Rows`] over the front's provider store and account store, for one turn's account.
pub struct FrontRows<A> {
    accounts: A,
    data_dir: std::path::PathBuf,
    user: String,
    default_id: String,
    keys: Option<SecretKeys>,
}

#[cfg(feature = "rust-storage-reads")]
impl<A: Accounts> FrontRows<A> {
    /// `keys`: the secrets keys as loaded at the cut (`None`: no key file; every row's key then reads as none).
    pub fn new(
        accounts: A,
        data_dir: std::path::PathBuf,
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

#[cfg(feature = "rust-storage-reads")]
impl<A: Accounts> chat_upstream::host::Rows for FrontRows<A> {
    fn now(&self, binding: &chat_upstream::binding::Binding) -> chat_upstream::host::RowsNow {
        let user = self.accounts.active(&self.user);
        // The row only for an active account (an inactive one fails the re-check on the user anyway).
        let row = user.as_ref().and_then(|_| {
            row_of(
                &self.data_dir,
                &self.user,
                &binding.provider_id,
                &self.default_id,
            )
        });
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
        let row = crate::provider_test_routes::find_row(
            &self.data_dir,
            &self.user,
            provider_id,
            Some(keys),
        )?;
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

#[cfg(feature = "rust-storage-reads")]
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
/// redirect followed (a 3xx comes back as the answer, which [`ProviderHost`] refuses as Node's fetch does).
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

#[cfg(all(test, feature = "rust-storage-reads"))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use chat_upstream::binding::{recheck, Binding, Now, Recheck};
    use chat_upstream::host::Rows;
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
        format!(
            r#"{{"id":"qa-provider","label":"QA provider","baseUrl":"{base}","apiKey":"{key}"{extra}}}"#
        )
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

    const HOSTED: &str =
        r#","runtimeLimit":{"tokens":32768,"source":"Default for hosted providers"}"#;

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
            signin: Signin::OFF,
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
        assert_eq!(
            caps(true),
            format!("{},providers", chat_turn::handoff::HANDOFF_CAPS)
        );
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
        let Plan::Provider(c) = plan(
            &p,
            &facts(Some(&u), Some(&r), "https://qa-provider.example"),
        ) else {
            panic!("not cut")
        };
        assert_eq!(c.target.url, URL);
        assert_eq!(c.binding.user, USER);
        assert_eq!(c.binding.provider_id, "qa-provider");
        assert_eq!(c.binding.model, "qa-model");
        assert_eq!(c.binding.chat_key.as_deref(), Some("qa-chat"));
        assert!((c.binding.expires_ms - (1_000.0 + BINDING_MS)).abs() < f64::EPSILON);
        // A stated context is the limit the frame must carry.
        let r = row(&own_row(
            "",
            "https://qa-provider.example",
            r#","contextTokens":65536"#,
        ));
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
        let shared = row(&own_row(
            "",
            "https://qa-provider.example/v1",
            r#","shared":true"#,
        ));
        assert!(matches!(
            plan(&frame(HOSTED), &facts(Some(&u), Some(&shared), "")),
            Plan::Provider(_)
        ));
        assert_eq!(
            plan(
                &frame(HOSTED),
                &facts(None, Some(&r), "https://qa-provider.example")
            ),
            Plan::Refuse(NO_ACCOUNT)
        );
        assert_eq!(
            plan(
                &frame(HOSTED),
                &facts(Some(&u), None, "https://qa-provider.example")
            ),
            Plan::Refuse(ROW_MISSING)
        );
        let no_base = row(r#"{"id":"qa-provider","label":"QA provider","baseUrl":7}"#);
        assert_eq!(
            plan(
                &frame(HOSTED),
                &facts(Some(&u), Some(&no_base), "https://qa-provider.example")
            ),
            Plan::Refuse(NODE_THROWS)
        );
        // The sign-in provider: not this slice (read as turned off, Node's own 409).
        let sign_in = row(
            r#"{"id":"qa-provider","kind":"chatgpt-oauth","label":"QA","baseUrl":"https://qa-provider.example"}"#,
        );
        assert!(matches!(
            plan(
                &frame(HOSTED),
                &facts(Some(&u), Some(&sign_in), "https://qa-provider.example")
            ),
            Plan::Refuse(NODE_REFUSES)
        ));
    }

    #[test]
    fn an_external_row_is_refused_as_core_refuses_it() {
        let u = member();
        let origins = "https://qa-provider.example";
        // Flagged external by the server (a route target's row).
        let flagged = row(&own_row(
            "",
            "https://qa-provider.example/v1",
            r#","external":true"#,
        ));
        assert_eq!(
            plan(&frame(HOSTED), &facts(Some(&u), Some(&flagged), origins)),
            Plan::Refuse(EXTERNAL)
        );
        // Only `true` flags it (provider-egress.cjs reads `external === true`).
        for not_flagged in [
            r#","external":"yes""#,
            r#","external":1"#,
            r#","external":false"#,
        ] {
            let r = row(&own_row("", "https://qa-provider.example/v1", not_flagged));
            assert!(
                matches!(
                    plan(&frame(HOSTED), &facts(Some(&u), Some(&r), origins)),
                    Plan::Provider(_)
                ),
                "{not_flagged}"
            );
        }
        // A trial-terms host, also behind user information and in another case.
        for base in [
            "https://qa.nvidia.com/v1",
            "https://NVIDIA.com/v1",
            "https://qa:secret@qa.nvidia.com/v1",
        ] {
            let r = row(&own_row("", base, ""));
            assert!(external_row(&r), "{base}");
        }
        assert!(!external_row(&row(&own_row(
            "",
            "https://notnvidia.com/v1",
            ""
        ))));
        assert!(!external_row(&row(&own_row(
            "",
            "https://qa-provider.example/v1",
            ""
        ))));
        // An address the rules cannot be sure about counts as external.
        assert!(external_row(&row(&own_row(
            "",
            "https://xn--qa-provider.example/v1",
            ""
        ))));
        // The sign-in kind is external too (refused before, by the cut, as Node refuses it).
        assert!(external_row(&row(
            r#"{"id":"qa-provider","kind":"chatgpt-oauth","baseUrl":"https://qa-provider.example"}"#
        )));
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
            &own_row(
                &sealed(&k, "qa-key-1"),
                "https://qa-provider.example/v1",
                "",
            ),
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
        assert_eq!(
            rows.key("default"),
            None,
            "the default row's key is never read here"
        );

        // Key rotated: the next request reads the new one; the binding still holds (the key is not pinned).
        write_rows(
            dir.path(),
            false,
            &own_row(
                &sealed(&k, "qa-key-2"),
                "https://qa-provider.example/v1",
                "",
            ),
        );
        assert_eq!(rechecked(&rows, &c.binding), Recheck::Ok);
        assert_eq!(rows.key("qa-provider").as_deref(), Some("qa-key-2"));

        // Sealed under another key: unreadable, so no bearer (openProviderKey gives '').
        let other =
            SecretKeys::from_existing_bytes(zeroize::Zeroizing::new(vec![9u8; 32]), None).unwrap();
        write_rows(
            dir.path(),
            false,
            &own_row(
                &sealed(&other, "qa-key-3"),
                "https://qa-provider.example/v1",
                "",
            ),
        );
        assert_eq!(rows.key("qa-provider"), None);
        write_rows(
            dir.path(),
            false,
            &own_row("", "https://qa-provider.example/v1", ""),
        );
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
        write_rows(
            dir.path(),
            false,
            &own_row("", "https://qa-moved.example/v1", ""),
        );
        assert_ne!(rechecked(&rows, &c.binding), Recheck::Ok);
        // The row deleted.
        write_rows(dir.path(), false, "");
        assert_eq!(rechecked(&rows, &c.binding), Recheck::RowGone);
        // Back, then the account disabled.
        write_rows(
            dir.path(),
            false,
            &own_row("", "https://qa-provider.example/v1", ""),
        );
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
        write_rows(
            dir.path(),
            false,
            &own_row("", "https://qa-own.example", ""),
        );
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

    /// The sign-in row (council p10-1, p9-3 CONTINUE 2).
    #[cfg(feature = "rust-chatgpt-status")]
    mod signin {
        use super::*;

        /// A sign-in row (the sign-in kind; its address a QA host here, not the server constant).
        const SIGNIN_ROW: &str = r#"{"id":"qa-provider","kind":"chatgpt-oauth","label":"QA","baseUrl":"https://qa-provider.example/v1"}"#;

        fn signin<'a>(
            caps: &'a str,
            enabled: bool,
            connection: Connection,
            cleared: bool,
        ) -> Signin<'a> {
            Signin {
                caps,
                enabled,
                connection,
                cleared,
            }
        }

        fn with_signin<'a>(u: &'a User, r: &'a JValue, s: Signin<'a>) -> Facts<'a> {
            Facts {
                signin: s,
                ..facts(Some(u), Some(r), "")
            }
        }

        #[test]
        fn the_front_names_the_sign_in_token_only_beside_providers_and_never_before_the_rules_are_cleared(
        ) {
            let base = chat_turn::handoff::HANDOFF_CAPS;
            assert_eq!(front_caps(false, false), base);
            assert_eq!(front_caps(false, true), base, "never without providers");
            assert_eq!(front_caps(true, false), caps(true));
            assert_eq!(
                front_caps(true, true),
                format!("{base},providers,signin-transport-v1")
            );
            // One token, three names: chat-upstream's refusal lift, the sign-in transport's offer and this front's.
            assert_eq!(SIGNIN_CAPS_TOKEN, chat_upstream::refusal::SIGNIN_CAP);
            assert_eq!(SIGNIN_CAPS_TOKEN, crate::signin_wire::CAPS_TOKEN);
            assert_eq!(CAPS_TOKEN, chat_upstream::refusal::CAP);
            for (providers, cleared, value, on) in [
                (true, true, Some("1"), true),
                (true, true, Some(" 1 "), true),
                (true, true, Some("0"), false),
                (true, true, Some("true"), false),
                (true, true, None, false),
                (false, true, Some("1"), false),
                (true, false, Some("1"), false),
            ] {
                assert_eq!(
                    signin_offered_by(providers, cleared, value),
                    on,
                    "{providers} {cleared} {value:?}"
                );
            }
            // Today: the rules are not cleared, so whatever the switches say the front never names the token.
            const { assert!(!EXTERNAL_EGRESS_CLEARED) };
            assert!(!signin_offered());
            assert_eq!(Signin::OFF.caps, "");
            assert!(!Signin::OFF.cleared);
        }

        /// What core's refusal (as chat-upstream ports it) answers a sign-in row for this front's caps: lifted only
        /// for the caps naming both tokens, which is exactly when [`plan`] plans the sign-in row.
        #[test]
        fn core_lifts_the_sign_in_row_for_exactly_the_caps_plan_takes_it_for() {
            let f = chat_upstream::refusal::Facts {
                provider_id: Some("qa-provider".into()),
                default_provider_id: Some("default".into()),
                machine: false,
                // The sign-in row is an external row (provider-egress.cjs); core's signinRow lifts that for its kind.
                external: true,
                sign_in: true,
                route_target: false,
                authorized: false,
            };
            let u = member();
            let r = row(SIGNIN_ROW);
            for (offer, signin_on) in [(false, false), (false, true), (true, false), (true, true)] {
                let caps = front_caps(offer, signin_on);
                let lifted =
                    chat_upstream::refusal::with_caps(&f, &chat_upstream::refusal::caps(&caps))
                        .is_none();
                let planned = matches!(
                    plan(
                        &frame(HOSTED),
                        &with_signin(&u, &r, signin(&caps, true, Connection::Connected, true))
                    ),
                    Plan::Signin(_)
                );
                assert_eq!(lifted, offer && signin_on, "{caps}");
                assert_eq!(planned, lifted, "{caps}");
            }
        }

        #[test]
        fn a_cleared_sign_in_row_is_planned_over_the_broker_and_bound_to_the_frame() {
            let u = member();
            let r = row(SIGNIN_ROW);
            let caps = front_caps(true, true);
            let Plan::Signin(s) = plan(
                &frame(HOSTED),
                &with_signin(&u, &r, signin(&caps, true, Connection::Connected, true)),
            ) else {
                panic!("planned");
            };
            assert_eq!(s.owner, USER);
            assert_eq!(s.conversation.as_deref(), Some("qa-chat"), "the chat id");
            assert_eq!(s.binding.url, URL);
            assert_eq!(s.binding.model, "qa-model");
            assert!((s.binding.expires_ms - (1_000.0 + BINDING_MS)).abs() < f64::EPSILON);
            // Without a chat id the conversation is the space (`chatId || spaceId || null`), an empty chat id too.
            let mut spaced = frame(HOSTED);
            spaced.chat_id = Some(String::new());
            spaced.space_id = Some("qa-space".into());
            let Plan::Signin(s) = plan(
                &spaced,
                &with_signin(&u, &r, signin(&caps, true, Connection::Connected, true)),
            ) else {
                panic!("planned");
            };
            assert_eq!(s.conversation.as_deref(), Some("qa-space"));
        }

        #[test]
        fn without_the_token_or_before_the_rules_are_cleared_the_sign_in_row_is_refused_as_before()
        {
            let u = member();
            let r = row(SIGNIN_ROW);
            // The front named no sign-in token: Node's own 409 (the transport read as off), as before this slice,
            // whatever the broker says.
            for caps in [
                String::new(),
                front_caps(true, false),
                "signin-transport-v10".into(),
            ] {
                assert_eq!(
                    plan(
                        &frame(HOSTED),
                        &with_signin(&u, &r, signin(&caps, true, Connection::Connected, true))
                    ),
                    Plan::Refuse(NODE_REFUSES),
                    "{caps}"
                );
            }
            // With the token but the rules not cleared: the external-row hold.
            let caps = front_caps(true, true);
            assert_eq!(
                plan(
                    &frame(HOSTED),
                    &with_signin(&u, &r, signin(&caps, true, Connection::Connected, false))
                ),
                Plan::Refuse(EXTERNAL)
            );
            // Signin::OFF (every row but the sign-in kind gets it) refuses it too.
            assert_eq!(
                plan(&frame(HOSTED), &facts(Some(&u), Some(&r), "")),
                Plan::Refuse(NODE_REFUSES)
            );
        }

        #[test]
        fn the_sign_in_checks_answer_first_in_nodes_order() {
            let u = member();
            let r = row(SIGNIN_ROW);
            let caps = front_caps(true, true);
            // Node's checks (on, owner, connected) come before the external-row hold and the binding's checks.
            for cleared in [false, true] {
                for (enabled, connection) in [
                    (false, Connection::Connected),
                    (true, Connection::Reconnect),
                    (true, Connection::Missing),
                ] {
                    assert_eq!(
                        plan(
                            &frame(""),
                            &with_signin(&u, &r, signin(&caps, enabled, connection, cleared))
                        ),
                        Plan::Refuse(NODE_REFUSES),
                        "{enabled} {connection:?} {cleared}"
                    );
                }
                // A shared sign-in row, or a turn in another account's workspace, is not the owner's: Node's 403.
                let shared = row(
                    r#"{"id":"qa-provider","kind":"chatgpt-oauth","label":"QA","baseUrl":"https://qa-provider.example/v1","shared":true}"#,
                );
                assert_eq!(
                    plan(
                        &frame(HOSTED),
                        &with_signin(
                            &u,
                            &shared,
                            signin(&caps, true, Connection::Connected, cleared)
                        )
                    ),
                    Plan::Refuse(NODE_REFUSES)
                );
                let nameless = User {
                    id: None,
                    admin: false,
                };
                assert_eq!(
                    plan(
                        &frame(HOSTED),
                        &with_signin(
                            &nameless,
                            &r,
                            signin(&caps, true, Connection::Connected, cleared)
                        )
                    ),
                    Plan::Refuse(NODE_REFUSES)
                );
            }
            // A row without a text baseUrl: Node throws.
            let no_base =
                row(r#"{"id":"qa-provider","kind":"chatgpt-oauth","label":"QA","baseUrl":7}"#);
            assert_eq!(
                plan(
                    &frame(HOSTED),
                    &with_signin(
                        &u,
                        &no_base,
                        signin(&caps, true, Connection::Connected, true)
                    )
                ),
                Plan::Refuse(NODE_THROWS)
            );
        }

        #[test]
        fn a_cleared_sign_in_frame_that_disagrees_is_refused() {
            let u = member();
            let r = row(SIGNIN_ROW);
            let caps = front_caps(true, true);
            let f = with_signin(&u, &r, signin(&caps, true, Connection::Connected, true));
            assert_eq!(plan(&frame(""), &f), Plan::Refuse(LIMIT), "no runtimeLimit");
            for edit in [
                (|p: &mut Prepared| p.model_lease = true) as fn(&mut Prepared),
                |p| p.manager_enabled = true,
                |p| p.title.enabled = true,
            ] {
                let mut local = frame(HOSTED);
                edit(&mut local);
                assert_eq!(plan(&local, &f), Plan::Refuse(LOCAL_ENGINE));
            }
            let mut moved = frame(HOSTED);
            moved.upstream_url = "https://elsewhere.example/v1/chat/completions".into();
            assert_eq!(plan(&moved, &f), Plan::Refuse(crate::signin_drive::ADDRESS));
            let mut nameless = frame(HOSTED);
            nameless.model = String::new();
            assert_eq!(
                plan(&nameless, &f),
                Plan::Refuse(crate::signin_drive::NO_MODEL)
            );
            let late = Facts {
                now_ms: f64::INFINITY,
                ..f
            };
            assert_eq!(
                plan(&frame(HOSTED), &late),
                Plan::Refuse(crate::signin_drive::BAD_EXPIRY)
            );
            // A row with its own context setting: the frame must carry that limit, not the hosted default.
            let sized = row(
                r#"{"id":"qa-provider","kind":"chatgpt-oauth","label":"QA","baseUrl":"https://qa-provider.example/v1","contextTokens":65536}"#,
            );
            let fs = with_signin(&u, &sized, signin(&caps, true, Connection::Connected, true));
            assert_eq!(plan(&frame(HOSTED), &fs), Plan::Refuse(LIMIT));
            let (tokens, source) = chat_upstream::limit::hosted_limit(&sized);
            let mut fitted = frame(HOSTED);
            fitted.runtime_limit = Some((tokens, source.to_string()));
            assert!(matches!(plan(&fitted, &fs), Plan::Signin(_)));
        }

        #[test]
        fn the_brokers_state_words_read_as_the_target_rules_read_them() {
            assert_eq!(connection_of(Some("connected")), Connection::Connected);
            assert_eq!(connection_of(Some("reconnect")), Connection::Reconnect);
            assert_eq!(connection_of(Some("disconnected")), Connection::Missing);
            assert_eq!(
                connection_of(None),
                Connection::Missing,
                "an unreadable store"
            );
            assert_eq!(
                connection_of(Some("Connected")),
                Connection::Missing,
                "exact words"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod http_tests {
    use super::provider_client;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A one-answer-per-connection HTTP stub on loopback: counts the requests it got, answers `reply`.
    async fn stub(reply: String) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                let reply = reply.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    let mut got = Vec::new();
                    // Read the head and the declared body (the client sends a small JSON body).
                    loop {
                        let Ok(n) = s.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        got.extend_from_slice(buf.get(..n).unwrap_or_default());
                        let text = String::from_utf8_lossy(&got);
                        if let Some(end) = text.find("\r\n\r\n") {
                            let len = text
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if got.len() >= end + 4 + len {
                                break;
                            }
                        }
                    }
                    let _ = s.write_all(reply.as_bytes()).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    /// Node's fetch with `redirect: 'error'`: every redirect status comes back as the answer and its Location is never
    /// asked (chat-upstream's ProviderHost then ends the request with `fetch failed`); a 200 is read as sent.
    #[tokio::test]
    async fn the_provider_client_follows_no_redirect() {
        let (elsewhere, elsewhere_hits) =
            stub("HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok".into())
                .await;
        let client = provider_client().unwrap();
        for status in [301u16, 302, 303, 307, 308] {
            let (base, hits) = stub(format!(
                "HTTP/1.1 {status} Moved\r\nlocation: {elsewhere}/v1/chat/completions\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            ))
            .await;
            let answer = client
                .post(format!("{base}/v1/chat/completions"))
                .header("content-type", "application/json")
                .body(r#"{"model":"qa-model"}"#)
                .send()
                .await
                .unwrap();
            assert_eq!(answer.status().as_u16(), status);
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        }
        assert_eq!(
            elsewhere_hits.load(Ordering::SeqCst),
            0,
            "a redirect was followed"
        );
        let ok = client
            .post(format!("{elsewhere}/v1/chat/completions"))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status().as_u16(), 200);
        assert_eq!(ok.text().await.unwrap(), "ok");
    }
}

/// What this build claims in FEATURES (council p12-3): the release assembly reads exactly these entries.
#[cfg(test)]
mod features {
    use super::*;

    fn listed(name: &str) -> usize {
        crate::FEATURES.iter().filter(|&&f| f == name).count()
    }

    #[test]
    fn provider_rows_are_claimed_by_a_build_with_the_provider_store() {
        let want = usize::from(cfg!(feature = "rust-storage-reads"));
        assert_eq!(listed(PROVIDERS_FEATURE), want);
        // The literal the release assembly greps for in lib.rs is this constant.
        assert_eq!(PROVIDERS_FEATURE, "chat-providers-v1");
    }

    #[test]
    fn signin_rows_are_claimed_only_once_the_external_rules_are_cleared() {
        let can_name = EXTERNAL_EGRESS_CLEARED
            && cfg!(feature = "rust-storage-reads")
            && cfg!(feature = "rust-chatgpt-status");
        assert_eq!(listed(SIGNIN_FEATURE), usize::from(can_name));
        assert_eq!(SIGNIN_FEATURE, "chat-signin-v1");
    }

    #[test]
    fn a_build_that_lists_signin_rows_lists_provider_rows_too() {
        // Core lifts the sign-in row only for a front naming both caps (front_caps).
        assert!(listed(SIGNIN_FEATURE) == 0 || listed(PROVIDERS_FEATURE) == 1);
    }

    #[test]
    fn the_feature_entries_are_not_the_caps_tokens() {
        // FEATURES says what the build can do; the caps say what this start offers core per turn.
        for f in [PROVIDERS_FEATURE, SIGNIN_FEATURE] {
            assert!(!front_caps(true, true).split(',').any(|t| t == f), "{f}");
        }
        assert_ne!(PROVIDERS_FEATURE, CAPS_TOKEN);
        assert_ne!(SIGNIN_FEATURE, SIGNIN_CAPS_TOKEN);
    }

    #[test]
    fn no_build_offers_the_signin_token_while_the_rules_are_held() {
        // The constant is the lock; the switch alone never names the token. (The change that clears the
        // rules replaces this test with the cleared case.)
        if EXTERNAL_EGRESS_CLEARED {
            return;
        }
        for value in [None, Some("1"), Some(" 1 "), Some("0"), Some("true")] {
            assert!(!signin_offered_by(true, EXTERNAL_EGRESS_CLEARED, value));
        }
        assert!(!front_caps(
            true,
            signin_offered_by(true, EXTERNAL_EGRESS_CLEARED, Some("1"))
        )
        .split(',')
        .any(|t| t == SIGNIN_CAPS_TOKEN));
    }
}
