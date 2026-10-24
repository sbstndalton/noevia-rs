//! Native `GET /api/providers/chatgpt` (M6, sbstndalton/noevia#1373): the signed-in account's
//! "Sign in with ChatGPT" connection status, as core routes/providers.cjs `handleChatGpt` and
//! chatgpt-oauth.cjs `status()` answer it. Read-only: one read transaction on the read-only store
//! ([`server_store::Store::read`]), no DDL, no migration and no write of any kind. The caller has
//! already run the request gate; `owner` is the gate's `identity.user.id`, the only row ever
//! selected, and the only associated data the envelope is opened with.
//!
//! Order, as Node and as the owner's 2026-10-12 decisions on #1373 require:
//! 1. The `chatgptOAuth` product flag (`NOEVIA_FEATURE_CHATGPT_OAUTH`, else the typed
//!    `feature:chatgptOAuth` settings row, else off). Off: Node's 404, before any OAuth row is
//!    read or any key is touched.
//! 2. The owner's `chatgpt_oauth_tokens` row: none is `disconnected`; a state other than
//!    `connected` is `reconnect` (its ciphertext is neither fetched nor opened).
//! 3. A connected row is opened with the keys snapshotted at startup (current, then previous) and
//!    parsed as `JSON.parse` would; an envelope no key opens, text that is not JSON and JSON that
//!    is not an object are `reconnect`, as Node's `read()` turns them into `data = null`.
//! 4. The answer: `state`, then for `connected` `account: { email: maskEmail(email), plan }`, then
//!    `providerId` and `external`, serialized as `JSON.stringify` does (UTF-16 slicing, lone
//!    surrogates as `\udxxx`).
//!
//! `plan` is projected ([`public_plan`]) exactly as core's `publicPlan` does since core#135: the
//! stored `chatgpt_plan_type` identifier only, otherwise `null`. Nothing else stored beside the
//! tokens (an object `plan` with credential-looking keys, the tokens themselves) can be echoed.
//!
//! Every byte is bounded before it is copied: the row's lengths are read in SQL and the ciphertext
//! is fetched only when it fits [`DATA_ENC_BYTES`]; the row's aggregate budget ([`ROW_BYTES`]) is
//! checked against what remains, with checked arithmetic, before it is spent (#1377). What Node
//! would answer but this port refuses (a missing or older table, a non-text or oversized row, a
//! duplicate owner row, unreadable keys, JSON this parser does not follow) is a sanitized 502 with
//! Node's own fallback text ([`FAILED_MESSAGE`]); nothing about the row or keys is in it.

use js_json::{wtf16, JValue};
use server_auth::secrets::SecretKeys;
use server_store::{rusqlite::types::ValueRef, Reader, Store};
use zeroize::Zeroizing;

/// The one path and method this crate answers (GET; every other method and sibling is Node's).
pub const ROUTE: &str = "/api/providers/chatgpt";
/// chatgpt-oauth.cjs `PROVIDER_ID`.
pub const PROVIDER_ID: &str = "chatgpt-oauth";
/// The settings row core features.cjs `settingsStore` reads for the flag.
pub const FEATURE_KEY: &str = server_features::spec(server_features::Feature::ChatgptOAuth).setting;
/// routes/providers.cjs: the flag-off answer.
pub const OFF_MESSAGE: &str = "Sign in with ChatGPT is turned off on this server.";
/// routes/providers.cjs: the answer to an error without a status (the sanitized 502).
pub const FAILED_MESSAGE: &str = "ChatGPT sign-in failed.";

/// Largest owner id (UTF-8 bytes) selected; ids are UUIDs.
pub const OWNER_BYTES: usize = 4_096;
/// Largest stored ciphertext fetched. A connected row is the token JSON (access, refresh and id
/// tokens, a few KiB) in an envelope.
pub const DATA_ENC_BYTES: usize = 65_536;
/// The row's aggregate budget: owner id plus ciphertext.
pub const ROW_BYTES: usize = 69_632;
/// Largest opened plaintext parsed.
pub const DECRYPTED_BYTES: usize = 65_536;
/// core chatgpt-oauth.cjs `maskEmail`: `.slice(0, 120)`.
const EMAIL_UNITS: usize = 120;

/// Why the status cannot be answered as Node would. Every one is the sanitized 502.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The store cannot be read (missing settings table, schema change, I/O).
    Store,
    /// `chatgpt_oauth_tokens` is missing or older than this port (no DDL or migration is run).
    Schema,
    /// A row Node never writes: non-text owner or ciphertext, invalid UTF-8, or two owner rows.
    Row,
    /// Over a byte budget.
    Cap,
    /// The secret keys could not be loaded, so a connected row cannot be opened.
    Keys,
    /// JSON `JSON.parse` would accept that js-json does not follow (see js-json `wtf16`).
    Unsupported,
}

/// The status code and JSON body Node would send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    pub body: String,
}

/// The connection state Node's `status()` reports.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Disconnected,
    Reconnect,
    /// `email` and `plan` as JS strings (lone surrogates mapped as in js-json `wtf16`), or `null`.
    Connected {
        email: Option<String>,
        plan: Option<String>,
    },
}

/// What the read transaction saw. The ciphertext leaves it only for a connected row.
enum Stored {
    Off,
    Missing,
    NotConnected,
    Connected(Zeroizing<String>),
}

fn schema(r: &Reader<'_>) -> Result<bool, server_store::StoreError> {
    let table: i64 = r
        .row(
            "SELECT count(*) FROM sqlite_master WHERE name='chatgpt_oauth_tokens' AND type='table'",
            [],
            |row| row.get(0),
        )?
        .unwrap_or(0);
    if table != 1 {
        return Ok(false);
    }
    let columns: i64 = r
        .row(
            "SELECT count(*) FROM pragma_table_info('chatgpt_oauth_tokens') WHERE name IN ('user_id','data_enc','state','updated_at')",
            [],
            |row| row.get(0),
        )?
        .unwrap_or(0);
    let pk: i64 = r
        .row(
            "SELECT count(*) FROM pragma_table_info('chatgpt_oauth_tokens') WHERE pk>0",
            [],
            |row| row.get(0),
        )?
        .unwrap_or(0);
    let owner_pk: i64 = r
        .row(
            "SELECT count(*) FROM pragma_table_info('chatgpt_oauth_tokens') WHERE name='user_id' AND pk=1",
            [],
            |row| row.get(0),
        )?
        .unwrap_or(0);
    Ok(columns == 4 && pk == 1 && owner_pk == 1)
}

/// features.cjs: the env value wins; else a typed `'true'` settings row; else off.
fn flag_on(r: &Reader<'_>, env: Option<bool>) -> Result<bool, server_store::StoreError> {
    if let Some(value) = env {
        return Ok(server_features::evaluate(
            server_features::Feature::ChatgptOAuth,
            Some(value),
            None,
            true,
        )
        .enabled);
    }
    let stored = r
        .row(
            "SELECT CASE WHEN typeof(value)='text' AND value IN ('true','false') THEN value ELSE NULL END FROM settings WHERE key=?1",
            [FEATURE_KEY],
            |row| row.get::<_, Option<String>>(0),
        )?
        .flatten();
    Ok(server_features::evaluate(
        server_features::Feature::ChatgptOAuth,
        env,
        stored.as_deref(),
        true,
    )
    .enabled)
}

/// Spends `len` from `remaining`, refusing before anything is spent when it does not fit (#1377).
fn spend(remaining: &mut usize, len: usize) -> Result<(), Fault> {
    *remaining = remaining.checked_sub(len).ok_or(Fault::Cap)?;
    Ok(())
}

fn length(value: ValueRef<'_>) -> Result<Option<usize>, Fault> {
    match value {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(n) => usize::try_from(n).map(Some).map_err(|_| Fault::Row),
        _ => Err(Fault::Row),
    }
}

/// One owner row's bounded projection, mapped inside the read transaction.
struct RowView {
    owner_text: bool,
    owner_matches: bool,
    owner_len: Option<usize>,
    connected: bool,
    data_text: bool,
    data_len: Option<usize>,
    data: Option<Zeroizing<String>>,
}

fn map_row(row: &server_store::rusqlite::Row<'_>) -> Result<RowView, Fault> {
    let get = |i: usize| row.get_ref(i).map_err(|_| Fault::Row);
    let flag = |v: ValueRef<'_>| matches!(v, ValueRef::Integer(1));
    let is_text = |v: ValueRef<'_>| matches!(v, ValueRef::Text(b"text"));
    let data = match get(6)? {
        ValueRef::Null => None,
        ValueRef::Text(bytes) => {
            if bytes.len() > DATA_ENC_BYTES {
                return Err(Fault::Cap);
            }
            let text = std::str::from_utf8(bytes).map_err(|_| Fault::Row)?;
            let mut out = Zeroizing::new(String::new());
            out.try_reserve_exact(text.len()).map_err(|_| Fault::Cap)?;
            out.push_str(text);
            Some(out)
        }
        _ => return Err(Fault::Row),
    };
    Ok(RowView {
        owner_text: is_text(get(0)?),
        owner_len: length(get(1)?)?,
        owner_matches: flag(get(2)?),
        connected: flag(get(3)?),
        data_text: is_text(get(4)?),
        data_len: length(get(5)?)?,
        data,
    })
}

fn snapshot(store: &Store, owner: &str, feature_env: Option<bool>) -> Result<Stored, Fault> {
    if owner.is_empty() || owner.len() > OWNER_BYTES {
        return Err(Fault::Cap);
    }
    let cap = i64::try_from(DATA_ENC_BYTES).map_err(|_| Fault::Cap)?;
    store
        .read(|r| {
            if !flag_on(r, feature_env)? {
                return Ok(Ok(Stored::Off));
            }
            if !schema(r)? {
                return Ok(Err(Fault::Schema));
            }
            // Lengths first; the ciphertext is materialized only for a connected row within cap.
            let mut rows = r.rows(
                "SELECT typeof(user_id), length(CAST(user_id AS BLOB)), user_id = ?1, \
                   (typeof(state) = 'text' AND state = 'connected'), \
                   typeof(data_enc), length(CAST(data_enc AS BLOB)), \
                   CASE WHEN typeof(state) = 'text' AND state = 'connected' \
                     AND typeof(data_enc) = 'text' AND length(CAST(data_enc AS BLOB)) <= ?2 \
                   THEN data_enc END \
                 FROM chatgpt_oauth_tokens WHERE user_id = ?1 LIMIT 2",
                server_store::rusqlite::params![owner, cap],
                |row| Ok(map_row(row)),
            )?;
            if rows.len() > 1 {
                return Ok(Err(Fault::Row));
            }
            let Some(view) = rows.pop() else {
                return Ok(Ok(Stored::Missing));
            };
            Ok(view.and_then(|v| classify(v, owner)))
        })
        .map_err(|_| Fault::Store)?
}

fn classify(v: RowView, owner: &str) -> Result<Stored, Fault> {
    if !v.owner_text || !v.owner_matches || v.owner_len != Some(owner.len()) {
        return Err(Fault::Row);
    }
    let mut remaining = ROW_BYTES;
    spend(&mut remaining, owner.len())?;
    if !v.connected {
        // Node: `row.state !== 'connected'` is reconnect whatever the ciphertext holds.
        return Ok(Stored::NotConnected);
    }
    if !v.data_text {
        return Err(Fault::Row);
    }
    let len = v.data_len.ok_or(Fault::Row)?;
    if len > DATA_ENC_BYTES {
        return Err(Fault::Cap);
    }
    spend(&mut remaining, len)?;
    let data = v.data.ok_or(Fault::Row)?;
    if data.len() != len {
        return Err(Fault::Row);
    }
    Ok(Stored::Connected(data))
}

/// core chatgpt-oauth.cjs `publicPlan`: the stored `chatgpt_plan_type` identifier when it matches
/// `^[A-Za-z0-9_.-]{1,40}$`, else `null`.
pub fn public_plan(plan: &JValue) -> Option<String> {
    let s = plan.as_str()?;
    let ok = (1..=40).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    ok.then(|| s.to_owned())
}

/// The UTF-16 units of a js-json `wtf16` string (a mapped lone surrogate is its one unit).
fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for c in s.chars() {
        if let Some(u) = wtf16::lone_unit(c) {
            out.push(u);
        } else {
            let mut buf = [0u16; 2];
            out.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    out
}

/// The `wtf16` string for UTF-16 units (an unpaired surrogate is mapped, never replaced).
fn from_units(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .filter_map(|r| match r {
            Ok(c) => Some(c),
            Err(e) => wtf16::lone_char(e.unpaired_surrogate()),
        })
        .collect()
}

/// core chatgpt-oauth.cjs `maskEmail`, on UTF-16 units: `null` without an `@`; else the first
/// unit of the part before the last `@`, `…@`, the domain, all cut to 120 units.
pub fn mask_email(email: &JValue) -> Option<String> {
    let u = units(email.as_str()?);
    let at = u.iter().rposition(|&c| c == u16::from(b'@'))?;
    let mut out: Vec<u16> = u.get(..at.min(1)).unwrap_or_default().to_vec();
    out.push(0x2026);
    out.extend_from_slice(u.get(at..).unwrap_or_default());
    out.truncate(EMAIL_UNITS);
    Some(from_units(&out))
}

/// The stored row's state, after opening and parsing a connected row.
fn state_of(
    stored: Stored,
    owner: &str,
    keys: &dyn Fn() -> Option<std::sync::Arc<SecretKeys>>,
) -> Result<Option<State>, Fault> {
    let data = match stored {
        Stored::Off => return Ok(None),
        Stored::Missing => return Ok(Some(State::Disconnected)),
        Stored::NotConnected => return Ok(Some(State::Reconnect)),
        Stored::Connected(data) => data,
    };
    let keys = keys().ok_or(Fault::Keys)?;
    // Node: a value no key opens is `data = null`, so reconnect.
    let Ok((plain, _used)) = keys.open(&data, Some(owner)) else {
        return Ok(Some(State::Reconnect));
    };
    drop(data);
    if plain.len() > DECRYPTED_BYTES {
        return Err(Fault::Cap);
    }
    let parsed = match wtf16::parse(&plain) {
        Ok(v) => v,
        Err(wtf16::Refusal::Invalid) => return Ok(Some(State::Reconnect)),
        Err(wtf16::Refusal::Unsupported) => return Err(Fault::Unsupported),
    };
    drop(plain);
    // chatgpt-oauth.cjs `record`: a plain object, never null or an array.
    if !matches!(parsed, JValue::Obj(_)) {
        return Ok(Some(State::Reconnect));
    }
    Ok(Some(State::Connected {
        email: mask_email(parsed.get("email")),
        plan: public_plan(parsed.get("plan")),
    }))
}

fn error_body(message: &str) -> String {
    wtf16::stringify(&JValue::obj([("error", JValue::Str(message.into()))])).unwrap_or_default()
}

/// The 502 every [`Fault`] is answered with.
pub fn failed() -> Answer {
    Answer {
        status: 502,
        body: error_body(FAILED_MESSAGE),
    }
}

/// The 200 body for `state`, byte for byte as Node sends it.
pub fn render(state: &State) -> String {
    let opt = |v: &Option<String>| v.clone().map_or(JValue::Null, JValue::Str);
    let mut items: Vec<(String, JValue)> = Vec::new();
    match state {
        State::Disconnected => items.push(("state".into(), JValue::Str("disconnected".into()))),
        State::Reconnect => items.push(("state".into(), JValue::Str("reconnect".into()))),
        State::Connected { email, plan } => {
            items.push(("state".into(), JValue::Str("connected".into())));
            items.push((
                "account".into(),
                JValue::obj([("email", opt(email)), ("plan", opt(plan))]),
            ));
        }
    }
    items.push(("providerId".into(), JValue::Str(PROVIDER_ID.into())));
    items.push(("external".into(), JValue::Bool(true)));
    wtf16::stringify(&JValue::Obj(items)).unwrap_or_default()
}

/// The status for the gate's `owner`. `feature_env` is `NOEVIA_FEATURE_CHATGPT_OAUTH` as parsed at
/// startup; `keys` is called only for a connected row (never while the flag is off).
pub fn answer(
    store: &Store,
    owner: &str,
    feature_env: Option<bool>,
    keys: &dyn Fn() -> Option<std::sync::Arc<SecretKeys>>,
) -> Answer {
    match snapshot(store, owner, feature_env).and_then(|s| state_of(s, owner, keys)) {
        Ok(None) => Answer {
            status: 404,
            body: error_body(OFF_MESSAGE),
        },
        Ok(Some(state)) => Answer {
            status: 200,
            body: render(&state),
        },
        Err(_) => failed(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn spend_checks_the_remaining_budget_before_spending() {
        let mut r = 10usize;
        assert!(spend(&mut r, 10).is_ok());
        assert_eq!(r, 0);
        assert_eq!(spend(&mut r, 1), Err(Fault::Cap));
        assert_eq!(r, 0, "a refused spend leaves the budget alone");
        let mut r = ROW_BYTES;
        assert_eq!(spend(&mut r, usize::MAX), Err(Fault::Cap));
        assert_eq!(r, ROW_BYTES);
        assert!(spend(&mut r, OWNER_BYTES).is_ok());
        assert!(spend(&mut r, DATA_ENC_BYTES).is_ok());
        assert_eq!(spend(&mut r, 1), Err(Fault::Cap));
    }

    #[test]
    fn mask_email_counts_utf16_units() {
        let s = |v: &str| mask_email(&JValue::Str(v.into()));
        assert_eq!(s("ab@c"), Some("a…@c".into()));
        assert_eq!(s("@c"), Some("…@c".into()));
        assert_eq!(s("abc"), None);
        assert_eq!(mask_email(&JValue::Null), None);
        let lone = s("\u{1F600}@c").unwrap();
        assert_eq!(
            wtf16::stringify(&JValue::Str(lone)).unwrap(),
            "\"\\ud83d…@c\""
        );
    }

    #[test]
    fn public_plan_is_an_identifier_or_null() {
        assert_eq!(
            public_plan(&JValue::Str("plus".into())),
            Some("plus".into())
        );
        assert_eq!(public_plan(&JValue::Str(String::new())), None);
        assert_eq!(
            public_plan(&JValue::obj([("access_token", JValue::Str("x".into()))])),
            None
        );
        assert_eq!(public_plan(&JValue::Str("a".repeat(41))), None);
    }
}
