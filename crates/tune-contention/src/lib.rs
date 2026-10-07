//! Auto-tune and another client of the llama.cpp router (sbstndalton/noevia#1062).
//!
//! Auto-tune measures one model at a time on a router that keeps at most one chat model resident
//! (`models_max 1`). noevia's own chat waits behind its maintenance gate while a tune runs, but a
//! client that talks to the router directly does not: a request it sends for another model makes
//! the router evict the model being tuned and load its own. Before this crate a tune treated that
//! as fatal ("Another client loaded a model during tuning", or "Timed out waiting for router
//! unload" when the other model kept coming back).
//!
//! [`decide`] is the one decision auto-tune asks for each time it looks at the router's rows while
//! another client may be using it: **proceed, wait, unload the other model, or give up.** It is
//! stateless; the caller keeps the wait's start time and hands back the `fingerprint` and `since`
//! of the previous reply, so the same rows at the same times always give the same answer.
//!
//! 1. **Foreign models.** Every row whose id is not the model being tuned and whose status is not
//!    `unloaded` or `failed` is foreign. `loaded` and `sleeping` are resident; `loading` is
//!    loading; any other status is unrecognised and treated as activity, never unloaded. None
//!    foreign is [`Action::Proceed`] (`clear`), whatever the time.
//! 2. **Give up.** Once `now - startedAt` reaches `maxWaitMs` the reply is [`Action::GiveUp`]
//!    (`timed_out`): the tune stops as interrupted and can be resumed.
//! 3. **Activity waits.** A foreign model that is loading (`loading`), in an unrecognised state
//!    (`other`) or resident with requests in flight (`busy > 0`, `busy`) is [`Action::Wait`]. The
//!    caller reads `busy` from that model's `llamacpp:requests_processing`; `null` means it could
//!    not be read.
//! 4. **Quiet unloads.** When every foreign model is resident and idle, the reply is
//!    [`Action::Unload`] (`idle`) once the foreign set has looked exactly the same for `quietMs`;
//!    if any idle model's `busy` was unknown it must look the same for twice that
//!    (`idle_unknown`). Until then it is [`Action::Wait`] (`settling`). The unload is the router's
//!    own `/models/unload` that auto-tune already sends before every step: this crate never asks
//!    for a model with requests in flight, or one still loading, to be stopped.
//!
//! "Looked the same" is the `fingerprint`: the foreign ids in byte order with their state
//! (`loading`, `other`, `busy`, `idle`, `unknown`) as a JSON array of pairs. `since` is the
//! previous reply's `since` when its fingerprint is identical (clamped to `now`), and `now`
//! otherwise.
//!
//! Input is untrusted JSON (see [`decide_json`]); anything malformed, with unknown keys, duplicate
//! row ids or over a cap is refused with a fixed error code. Nothing here panics.

use serde_json::{json, Map, Value};

/// Requests longer than this are refused ([`DecideError::TooLarge`]).
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
/// At most this many router rows.
pub const MAX_ROWS: usize = 256;
/// A model id at most this many UTF-8 bytes (1 or more).
pub const MAX_ID_BYTES: usize = 256;
/// A status at most this many UTF-8 bytes.
pub const MAX_STATUS_BYTES: usize = 64;
/// The previous fingerprint at most this many UTF-8 bytes.
pub const MAX_FINGERPRINT_BYTES: usize = 48 * 1024;
/// `maxWaitMs` at most one day.
pub const MAX_WAIT_MS: u64 = 86_400_000;
/// `quietMs` at most one hour.
pub const MAX_QUIET_MS: u64 = 3_600_000;
/// Times are milliseconds no larger than JavaScript's largest safe integer.
pub const MAX_TIME: u64 = 9_007_199_254_740_991;

/// What the tune does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Proceed,
    Wait,
    Unload,
    GiveUp,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Proceed => "proceed",
            Action::Wait => "wait",
            Action::Unload => "unload",
            Action::GiveUp => "give_up",
        }
    }
}

/// Why, in a fixed vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Clear,
    TimedOut,
    Loading,
    Other,
    Busy,
    Settling,
    Idle,
    IdleUnknown,
}

impl Reason {
    pub fn name(self) -> &'static str {
        match self {
            Reason::Clear => "clear",
            Reason::TimedOut => "timed_out",
            Reason::Loading => "loading",
            Reason::Other => "other",
            Reason::Busy => "busy",
            Reason::Settling => "settling",
            Reason::Idle => "idle",
            Reason::IdleUnknown => "idle_unknown",
        }
    }
}

/// A foreign model's state, as the fingerprint records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Loading,
    Other,
    Busy,
    Idle,
    Unknown,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Loading => "loading",
            State::Other => "other",
            State::Busy => "busy",
            State::Idle => "idle",
            State::Unknown => "unknown",
        }
    }
}

/// One router row: its id, `status.value` and, for a resident model, requests in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub status: String,
    pub busy: Option<u64>,
}

/// The previous reply's fingerprint and since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prev {
    pub fingerprint: String,
    pub since: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub tuning: String,
    pub rows: Vec<Row>,
    pub prev: Option<Prev>,
    pub started_at: u64,
    pub now: u64,
    pub max_wait_ms: u64,
    pub quiet_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub action: Action,
    pub reason: Reason,
    /// Foreign ids in byte order.
    pub foreign: Vec<String>,
    /// What to unload: every foreign id on [`Action::Unload`], otherwise empty.
    pub unload: Vec<String>,
    pub fingerprint: String,
    pub since: u64,
    pub waited_ms: u64,
}

/// A refused request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecideError {
    Input,
    TooLarge,
}

impl DecideError {
    pub fn code(self) -> &'static str {
        match self {
            DecideError::Input => "input",
            DecideError::TooLarge => "too_large",
        }
    }
}

fn state_of(row: &Row) -> Option<State> {
    match row.status.as_str() {
        "unloaded" | "failed" => None,
        "loading" => Some(State::Loading),
        "loaded" | "sleeping" => Some(match row.busy {
            Some(0) => State::Idle,
            Some(_) => State::Busy,
            None => State::Unknown,
        }),
        _ => Some(State::Other),
    }
}

/// The decision for one look at the router; see the crate docs.
pub fn decide(r: &Request) -> Decision {
    let mut foreign: Vec<(&str, State)> = r
        .rows
        .iter()
        .filter(|row| row.id != r.tuning)
        .filter_map(|row| state_of(row).map(|s| (row.id.as_str(), s)))
        .collect();
    foreign.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let waited_ms = r.now.saturating_sub(r.started_at);
    let ids: Vec<String> = foreign.iter().map(|(id, _)| (*id).to_owned()).collect();
    if foreign.is_empty() {
        return Decision {
            action: Action::Proceed,
            reason: Reason::Clear,
            foreign: ids,
            unload: Vec::new(),
            fingerprint: String::new(),
            since: r.now,
            waited_ms,
        };
    }
    let fingerprint = Value::Array(
        foreign
            .iter()
            .map(|(id, s)| json!([id, s.name()]))
            .collect(),
    )
    .to_string();
    let since = match &r.prev {
        Some(p) if p.fingerprint == fingerprint => p.since.min(r.now),
        _ => r.now,
    };
    let has = |want: State| foreign.iter().any(|(_, s)| *s == want);
    let (action, reason) = if waited_ms >= r.max_wait_ms {
        (Action::GiveUp, Reason::TimedOut)
    } else if has(State::Loading) {
        (Action::Wait, Reason::Loading)
    } else if has(State::Other) {
        (Action::Wait, Reason::Other)
    } else if has(State::Busy) {
        (Action::Wait, Reason::Busy)
    } else {
        let unknown = has(State::Unknown);
        let needed = if unknown {
            r.quiet_ms.saturating_mul(2)
        } else {
            r.quiet_ms
        };
        if r.now.saturating_sub(since) >= needed {
            (
                Action::Unload,
                if unknown {
                    Reason::IdleUnknown
                } else {
                    Reason::Idle
                },
            )
        } else {
            (Action::Wait, Reason::Settling)
        }
    };
    Decision {
        unload: if action == Action::Unload {
            ids.clone()
        } else {
            Vec::new()
        },
        action,
        reason,
        foreign: ids,
        fingerprint,
        since,
        waited_ms,
    }
}

fn text(v: &Value, min: usize, max: usize) -> Result<String, DecideError> {
    match v.as_str() {
        Some(s) if s.len() >= min && s.len() <= max => Ok(s.to_owned()),
        _ => Err(DecideError::Input),
    }
}

fn time(v: Option<&Value>, max: u64) -> Result<u64, DecideError> {
    match v.and_then(Value::as_u64) {
        Some(n) if n <= max => Ok(n),
        _ => Err(DecideError::Input),
    }
}

fn exact_keys(o: &Map<String, Value>, keys: &[&str]) -> Result<(), DecideError> {
    if o.len() == keys.len() && keys.iter().all(|k| o.contains_key(*k)) {
        Ok(())
    } else {
        Err(DecideError::Input)
    }
}

/// Parse and validate a request; see [`decide_json`].
pub fn parse(input: &str) -> Result<Request, DecideError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(DecideError::TooLarge);
    }
    let v: Value = serde_json::from_str(input).map_err(|_| DecideError::Input)?;
    let o = v.as_object().ok_or(DecideError::Input)?;
    exact_keys(
        o,
        &[
            "tuning",
            "rows",
            "prev",
            "startedAt",
            "now",
            "maxWaitMs",
            "quietMs",
        ],
    )?;
    let tuning = text(o.get("tuning").ok_or(DecideError::Input)?, 1, MAX_ID_BYTES)?;
    let list = o
        .get("rows")
        .and_then(Value::as_array)
        .ok_or(DecideError::Input)?;
    if list.len() > MAX_ROWS {
        return Err(DecideError::Input);
    }
    let mut rows = Vec::with_capacity(list.len());
    for item in list {
        let ro = item.as_object().ok_or(DecideError::Input)?;
        exact_keys(ro, &["id", "status", "busy"])?;
        let id = text(ro.get("id").ok_or(DecideError::Input)?, 1, MAX_ID_BYTES)?;
        let status = text(
            ro.get("status").ok_or(DecideError::Input)?,
            0,
            MAX_STATUS_BYTES,
        )?;
        let busy = match ro.get("busy") {
            Some(Value::Null) => None,
            other => Some(time(other, u64::from(u32::MAX))?),
        };
        if rows.iter().any(|r: &Row| r.id == id) {
            return Err(DecideError::Input);
        }
        rows.push(Row { id, status, busy });
    }
    let prev = match o.get("prev") {
        Some(Value::Null) => None,
        Some(Value::Object(p)) => {
            exact_keys(p, &["fingerprint", "since"])?;
            Some(Prev {
                fingerprint: text(
                    p.get("fingerprint").ok_or(DecideError::Input)?,
                    0,
                    MAX_FINGERPRINT_BYTES,
                )?,
                since: time(p.get("since"), MAX_TIME)?,
            })
        }
        _ => return Err(DecideError::Input),
    };
    Ok(Request {
        tuning,
        rows,
        prev,
        started_at: time(o.get("startedAt"), MAX_TIME)?,
        now: time(o.get("now"), MAX_TIME)?,
        max_wait_ms: time(o.get("maxWaitMs"), MAX_WAIT_MS)?,
        quiet_ms: time(o.get("quietMs"), MAX_QUIET_MS)?,
    })
}

/// The reply as JSON (keys in byte order).
pub fn to_json(d: &Decision) -> String {
    json!({
        "action": d.action.name(),
        "reason": d.reason.name(),
        "foreign": d.foreign,
        "unload": d.unload,
        "fingerprint": d.fingerprint,
        "since": d.since,
        "waitedMs": d.waited_ms,
    })
    .to_string()
}

/// The JSON entry point dav-parse.wasm's `tune_contention` calls: status 0 with the decision, or
/// status 1 with `{"error":"input"|"too_large"}`.
///
/// Input: `{"tuning":"…","rows":[{"id":"…","status":"…","busy":n|null}],"prev":{"fingerprint":"…",
/// "since":n}|null,"startedAt":n,"now":n,"maxWaitMs":n,"quietMs":n}`.
/// Reply: `{"action":"proceed"|"wait"|"unload"|"give_up","reason":"…","foreign":[…],"unload":[…],
/// "fingerprint":"…","since":n,"waitedMs":n}`.
pub fn decide_json(input: &str) -> (u32, String) {
    match parse(input) {
        Ok(r) => (0, to_json(&decide(&r))),
        Err(e) => (1, format!("{{\"error\":\"{}\"}}", e.code())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn row(id: &str, status: &str, busy: Option<u64>) -> Row {
        Row {
            id: id.to_owned(),
            status: status.to_owned(),
            busy,
        }
    }

    fn req(rows: Vec<Row>, prev: Option<Prev>, now: u64) -> Request {
        Request {
            tuning: "tuned".to_owned(),
            rows,
            prev,
            started_at: 1_000,
            now,
            max_wait_ms: 600_000,
            quiet_ms: 30_000,
        }
    }

    #[test]
    fn nothing_foreign_proceeds() {
        let d = decide(&req(
            vec![row("tuned", "loaded", Some(1)), row("x", "unloaded", None)],
            None,
            5_000,
        ));
        assert_eq!((d.action, d.reason), (Action::Proceed, Reason::Clear));
        assert!(d.foreign.is_empty() && d.fingerprint.is_empty());
        assert_eq!(d.since, 5_000);
    }

    #[test]
    fn clear_beats_timeout() {
        let d = decide(&req(vec![], None, 10_000_000));
        assert_eq!(d.action, Action::Proceed);
    }

    #[test]
    fn activity_waits_in_order() {
        let d = decide(&req(
            vec![
                row("b", "loaded", Some(2)),
                row("a", "loading", None),
                row("c", "unloading", None),
            ],
            None,
            2_000,
        ));
        assert_eq!((d.action, d.reason), (Action::Wait, Reason::Loading));
        assert_eq!(d.foreign, ["a", "b", "c"]);
        assert_eq!(
            d.fingerprint,
            r#"[["a","loading"],["b","busy"],["c","other"]]"#
        );
        let d = decide(&req(
            vec![row("b", "loaded", Some(2)), row("c", "weird", None)],
            None,
            2_000,
        ));
        assert_eq!(d.reason, Reason::Other);
        let d = decide(&req(vec![row("b", "sleeping", Some(1))], None, 2_000));
        assert_eq!(d.reason, Reason::Busy);
        assert!(d.unload.is_empty());
    }

    #[test]
    fn idle_unloads_only_after_quiet() {
        let first = decide(&req(vec![row("q", "loaded", Some(0))], None, 2_000));
        assert_eq!(
            (first.action, first.reason),
            (Action::Wait, Reason::Settling)
        );
        assert_eq!(first.since, 2_000);
        let prev = Prev {
            fingerprint: first.fingerprint.clone(),
            since: first.since,
        };
        let d = decide(&req(
            vec![row("q", "loaded", Some(0))],
            Some(prev.clone()),
            31_999,
        ));
        assert_eq!(d.action, Action::Wait);
        let d = decide(&req(vec![row("q", "loaded", Some(0))], Some(prev), 32_000));
        assert_eq!((d.action, d.reason), (Action::Unload, Reason::Idle));
        assert_eq!(d.unload, ["q"]);
    }

    #[test]
    fn unknown_busy_needs_twice_the_quiet() {
        let prev = Prev {
            fingerprint: r#"[["q","unknown"]]"#.to_owned(),
            since: 2_000,
        };
        let d = decide(&req(
            vec![row("q", "loaded", None)],
            Some(prev.clone()),
            61_999,
        ));
        assert_eq!(d.reason, Reason::Settling);
        let d = decide(&req(vec![row("q", "loaded", None)], Some(prev), 62_000));
        assert_eq!((d.action, d.reason), (Action::Unload, Reason::IdleUnknown));
    }

    #[test]
    fn a_change_restarts_the_quiet() {
        let prev = Prev {
            fingerprint: r#"[["q","busy"]]"#.to_owned(),
            since: 2_000,
        };
        let d = decide(&req(vec![row("q", "loaded", Some(0))], Some(prev), 90_000));
        assert_eq!((d.action, d.since), (Action::Wait, 90_000));
    }

    #[test]
    fn since_is_clamped_to_now() {
        let prev = Prev {
            fingerprint: r#"[["q","idle"]]"#.to_owned(),
            since: 9_000_000,
        };
        let d = decide(&req(vec![row("q", "loaded", Some(0))], Some(prev), 5_000));
        assert_eq!((d.since, d.action), (5_000, Action::Wait));
    }

    #[test]
    fn gives_up_at_the_limit_even_when_idle() {
        let d = decide(&req(vec![row("q", "loaded", Some(3))], None, 601_000));
        assert_eq!((d.action, d.reason), (Action::GiveUp, Reason::TimedOut));
        assert!(d.unload.is_empty());
        assert_eq!(d.waited_ms, 600_000);
        let d = decide(&req(vec![row("q", "loaded", Some(3))], None, 600_999));
        assert_eq!(d.action, Action::Wait);
    }

    #[test]
    fn json_round_trip_and_refusals() {
        let (s, r) = decide_json(
            r#"{"tuning":"t","rows":[{"id":"q","status":"loaded","busy":0}],"prev":null,"startedAt":0,"now":0,"maxWaitMs":10,"quietMs":0}"#,
        );
        assert_eq!(s, 0);
        assert_eq!(
            r,
            r#"{"action":"unload","fingerprint":"[[\"q\",\"idle\"]]","foreign":["q"],"reason":"idle","since":0,"unload":["q"],"waitedMs":0}"#
        );
        for bad in [
            "{",
            "[]",
            r#"{"tuning":"t","rows":[],"prev":null,"startedAt":0,"now":0,"maxWaitMs":10}"#,
            r#"{"tuning":"t","rows":[],"prev":null,"startedAt":0,"now":0,"maxWaitMs":10,"quietMs":0,"x":1}"#,
            r#"{"tuning":"","rows":[],"prev":null,"startedAt":0,"now":0,"maxWaitMs":10,"quietMs":0}"#,
            r#"{"tuning":"t","rows":[{"id":"a","status":"loaded","busy":-1}],"prev":null,"startedAt":0,"now":0,"maxWaitMs":10,"quietMs":0}"#,
            r#"{"tuning":"t","rows":[{"id":"a","status":"loaded","busy":0},{"id":"a","status":"loaded","busy":0}],"prev":null,"startedAt":0,"now":0,"maxWaitMs":10,"quietMs":0}"#,
            r#"{"tuning":"t","rows":[],"prev":null,"startedAt":0,"now":0,"maxWaitMs":86400001,"quietMs":0}"#,
            r#"{"tuning":"t","rows":[],"prev":{"fingerprint":"x"},"startedAt":0,"now":0,"maxWaitMs":10,"quietMs":0}"#,
            r#"{"tuning":"t","rows":[],"prev":null,"startedAt":0.5,"now":0,"maxWaitMs":10,"quietMs":0}"#,
        ] {
            assert_eq!(
                decide_json(bad),
                (1, r#"{"error":"input"}"#.to_owned()),
                "{bad}"
            );
        }
        let big = " ".repeat(MAX_INPUT_BYTES + 1);
        assert_eq!(
            decide_json(&big),
            (1, r#"{"error":"too_large"}"#.to_owned())
        );
    }
}
