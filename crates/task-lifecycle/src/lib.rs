//! noevia-core's `server/task-lifecycle.cjs` in Rust, exported from `dav-parse.wasm`
//! (TASK_LIFECYCLE_IMPL). The lifecycle is the coarse "where is this task in its life" state of a
//! Code task (#511/#512, #701): a guarded state table plus a fold over the jobs.cjs journal. The
//! journal I/O, the lifecycle capability token, revision checks and the completeness report stay
//! in the JS; this crate answers:
//!
//! - [`can_transition`] / [`transition`]: the guarded table (`TRANSITIONS`; staying put is always
//!   allowed; `merged` is terminal).
//! - [`assert_stage_move`]: the pipeline's stricter move (`merged` only from `reviewing`).
//! - [`fold`]: `foldEvents(events, fromState, { authoritative })`, one `step()` per event:
//!   - `task.stage` (either mode): `data.to` must be a state, `data.from` the current state, entering
//!     `reviewing` needs `data.reportHash` to be a string matching `/^[0-9a-f]{64}$/` (#1125), then
//!     [`assert_stage_move`]; `task.revision` is a no-op;
//!   - authoritative: `job.failed` / `job.cancelled` / `job.interrupted` force `blocked` (except out
//!     of `merged`); everything else is a no-op;
//!   - otherwise: `job.started` → `implementing`, `progress` with stage `implementing`/`verifying`
//!     → that stage, `job.completed` → `verifying`, the three failure types → `blocked` (each a
//!     guarded move), anything else (approvals included) a no-op.
//! - [`derive`]: `deriveLifecycle(events)`: authoritative when any event's type is `task.stage` or
//!   `task.revision` ([`is_authoritative`]).
//!
//! Where the JS throws a `TaskLifecycleError` the port answers with a [`Throw`] code; that is an
//! answer, not a refusal.
//!
//! # Stricter than the JS
//!
//! A request that is not the documented shape, or over [`MAX_INPUT_BYTES`], is refused. (Before
//! noevia#1125 the JS ran a non-string `reportHash` through `String()` and the port refused those;
//! both now answer `report_hash`.) The host sends only the events and fields the fold reads
//! (noevia#1126), but any full journal folds the same. The host
//! (noevia-core TASK_LIFECYCLE_IMPL) refuses the transition whenever the port refuses or disagrees.
//!
//! No Unicode tables, number formatting or locale data are involved: types, states and stages are
//! compared as exact UTF-16 code units, so no answer depends on the Node/ICU version.
//!
//! Linear time (each event reads at most four members once), memory linear in the input, no
//! panics.

#![forbid(unsafe_code)]

use prompt_framing::json::{self, Value};

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024 + 1;
/// JSON nesting kept: the request array (0), the events (1), an event (2), its data (3) and the
/// data's members (4, scalars only; a container there is [`Value::Deep`]).
const MAX_DEPTH: usize = 4;

/// A lifecycle state (`STATES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    Planned,
    Implementing,
    Verifying,
    Reviewing,
    ChangesRequested,
    Merged,
    Blocked,
}

/// Every state, in `STATES` order.
pub const STATES: [State; 7] = [
    State::Planned,
    State::Implementing,
    State::Verifying,
    State::Reviewing,
    State::ChangesRequested,
    State::Merged,
    State::Blocked,
];

/// `INITIAL_STATE`.
pub const INITIAL_STATE: State = State::Planned;

impl State {
    /// The JS name.
    pub fn name(self) -> &'static str {
        match self {
            State::Planned => "planned",
            State::Implementing => "implementing",
            State::Verifying => "verifying",
            State::Reviewing => "reviewing",
            State::ChangesRequested => "changes_requested",
            State::Merged => "merged",
            State::Blocked => "blocked",
        }
    }

    /// The state named exactly `s` (UTF-16 code units).
    pub fn from_units(s: &[u16]) -> Option<State> {
        STATES.into_iter().find(|st| is(s, st.name()))
    }

    /// `STATE_SET.has(v)`: a string naming a state.
    fn from_value(v: Option<&Value>) -> Option<State> {
        v.and_then(Value::as_str).and_then(State::from_units)
    }

    /// `TRANSITIONS[self]`.
    pub fn successors(self) -> &'static [State] {
        use State::*;
        match self {
            Planned => &[Implementing, Blocked],
            Implementing => &[Verifying, Reviewing, Merged, Blocked],
            Verifying => &[Reviewing, Implementing, ChangesRequested, Merged, Blocked],
            Reviewing => &[Implementing, ChangesRequested, Merged, Blocked],
            ChangesRequested => &[Implementing, Reviewing, Merged, Blocked],
            Blocked => &[Implementing, Planned],
            Merged => &[],
        }
    }
}

/// What the JS throws (`TaskLifecycleError`), by its `code`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Throw {
    /// `Unknown task-lifecycle state`.
    UnknownState,
    /// `Illegal task-lifecycle transition`.
    Illegal,
    /// `Stale task-lifecycle stage`: the recorded `from` is not the current state.
    Stale,
    /// Entering `reviewing` without a completeness report hash.
    ReportHash,
    /// `A task is merged only from reviewing`.
    MergeFromReviewing,
}

impl Throw {
    /// The code the host compares (the JS error's `code`).
    pub fn code(self) -> &'static str {
        match self {
            Throw::UnknownState => "unknown_state",
            Throw::Illegal => "illegal",
            Throw::Stale => "stale",
            Throw::ReportHash => "report_hash",
            Throw::MergeFromReviewing => "merge_from_reviewing",
        }
    }
}

/// Why the port will not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Input,
    TooLarge,
}

impl Refusal {
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
        }
    }
}

/// A fold either ends in a state, or stops where the JS throws.
pub type Outcome = Result<State, Throw>;

enum Stop {
    Throws(Throw),
    Refused(Refusal),
}

impl From<Throw> for Stop {
    fn from(t: Throw) -> Self {
        Stop::Throws(t)
    }
}

fn is(s: &[u16], ascii: &str) -> bool {
    s.iter().copied().eq(ascii.encode_utf16())
}

/// `canTransition(from, to)`.
pub fn can_transition(from: State, to: State) -> bool {
    from == to || from.successors().contains(&to)
}

/// `transition(from, to)`.
pub fn transition(from: State, to: State) -> Outcome {
    if can_transition(from, to) {
        Ok(to)
    } else {
        Err(Throw::Illegal)
    }
}

/// `assertStageMove(from, to)`.
pub fn assert_stage_move(from: State, to: State) -> Outcome {
    if to == State::Merged && from != State::Merged && from != State::Reviewing {
        return Err(Throw::MergeFromReviewing);
    }
    transition(from, to)
}

fn advance(from: State, to: State) -> Outcome {
    if from == to {
        Ok(from)
    } else {
        transition(from, to)
    }
}

/// `/^[0-9a-f]{64}$/` (no `m` flag: `$` is the end of the text).
fn is_hex64(s: &[u16]) -> bool {
    s.len() == 64 && s.iter().all(|&c| matches!(c, 0x30..=0x39 | 0x61..=0x66))
}

/// `typeof v === 'string' && REPORT_HASH.test(v)` (#1125).
fn report_hash_ok(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str).is_some_and(is_hex64)
}

/// A member of `event.data || {}`: only an object has the members read here (a string, number or
/// array's prototype has none of `to`, `from`, `stage`, `reportHash`).
fn field<'a>(data: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    data.and_then(|d| d.get(key))
}

fn stage_step(state: State, data: Option<&Value>) -> Result<State, Stop> {
    let to = State::from_value(field(data, "to")).ok_or(Throw::UnknownState)?;
    let from_matches = field(data, "from")
        .and_then(Value::as_str)
        .is_some_and(|s| is(s, state.name()));
    if !from_matches {
        return Err(Throw::Stale.into());
    }
    if to == State::Reviewing && to != state && !report_hash_ok(field(data, "reportHash")) {
        return Err(Throw::ReportHash.into());
    }
    Ok(assert_stage_move(state, to)?)
}

fn is_failure(ty: &[u16]) -> bool {
    is(ty, "job.failed") || is(ty, "job.cancelled") || is(ty, "job.interrupted")
}

/// `step(state, event, { authoritative })`.
fn step(state: State, event: &Value, authoritative: bool) -> Result<State, Stop> {
    // `!event || typeof event.type !== 'string'`: only an object can carry a string `type`.
    let ty = match event {
        Value::Obj(_) => match event.get("type") {
            Some(Value::Str(t)) => t.as_slice(),
            _ => return Ok(state),
        },
        Value::Deep => return Err(Stop::Refused(Refusal::Input)),
        _ => return Ok(state),
    };
    let data = match event.get("data") {
        Some(d @ Value::Obj(_)) => Some(d),
        Some(Value::Deep) => return Err(Stop::Refused(Refusal::Input)),
        _ => None,
    };
    if is(ty, "task.stage") {
        return stage_step(state, data);
    }
    if is(ty, "task.revision") {
        return Ok(state);
    }
    if authoritative {
        if is_failure(ty) && state != State::Merged {
            return Ok(advance(state, State::Blocked)?);
        }
        return Ok(state);
    }
    let next = if is(ty, "job.started") {
        advance(state, State::Implementing)
    } else if is(ty, "progress") {
        match field(data, "stage").and_then(Value::as_str) {
            Some(s) if is(s, "implementing") => advance(state, State::Implementing),
            Some(s) if is(s, "verifying") => advance(state, State::Verifying),
            _ => Ok(state),
        }
    } else if is(ty, "job.completed") {
        advance(state, State::Verifying)
    } else if is_failure(ty) {
        advance(state, State::Blocked)
    } else {
        Ok(state)
    };
    Ok(next?)
}

fn events_of(events: &Value) -> Result<&[Value], Refusal> {
    match events {
        Value::Null => Ok(&[]),
        Value::Arr(list) => Ok(list),
        _ => Err(Refusal::Input),
    }
}

/// `isAuthoritative(events)`.
pub fn is_authoritative(events: &[Value]) -> bool {
    events.iter().any(|e| {
        matches!(e, Value::Obj(_))
            && e.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| is(t, "task.stage") || is(t, "task.revision"))
    })
}

/// `foldEvents(events, fromState, { authoritative })`: `events` is an array or `null`; `from` any
/// value (a non-state throws `unknown_state`, as `assertKnownState` does).
pub fn fold(events: &Value, from: &Value, authoritative: bool) -> Result<Outcome, Refusal> {
    let list = events_of(events)?;
    let Some(mut state) = State::from_value(Some(from)) else {
        return Ok(Err(Throw::UnknownState));
    };
    for event in list {
        match step(state, event, authoritative) {
            Ok(next) => state = next,
            Err(Stop::Throws(t)) => return Ok(Err(t)),
            Err(Stop::Refused(r)) => return Err(r),
        }
    }
    Ok(Ok(state))
}

/// `deriveLifecycle(events)`.
pub fn derive(events: &Value) -> Result<Outcome, Refusal> {
    let authoritative = is_authoritative(events_of(events)?);
    fold(
        events,
        &Value::Str(INITIAL_STATE.name().encode_utf16().collect()),
        authoritative,
    )
}

fn pair(a: &Value, b: &Value) -> Result<(State, State), Throw> {
    let from = State::from_value(Some(a)).ok_or(Throw::UnknownState)?;
    let to = State::from_value(Some(b)).ok_or(Throw::UnknownState)?;
    Ok((from, to))
}

/// `assertStageMove(from, to)` over raw values: the JS tests `to === 'merged'` and `from` against
/// `'merged'`/`'reviewing'` before either is checked to be a state.
fn stage_move_values(a: &Value, b: &Value) -> Outcome {
    let named = |v: &Value, s: State| v.as_str().is_some_and(|u| is(u, s.name()));
    if named(b, State::Merged) && !named(a, State::Merged) && !named(a, State::Reviewing) {
        return Err(Throw::MergeFromReviewing);
    }
    pair(a, b).and_then(|(f, t)| assert_stage_move(f, t))
}

fn outcome_json(o: Outcome) -> String {
    match o {
        Ok(s) => format!(r#"{{"state":"{}"}}"#, s.name()),
        Err(t) => format!(r#"{{"throws":"{}"}}"#, t.code()),
    }
}

fn run(op: u8, args: &[Value]) -> Result<String, Refusal> {
    match (op, args) {
        (1, [a, b]) => Ok(match pair(a, b) {
            Ok((from, to)) => format!(r#"{{"allowed":{}}}"#, can_transition(from, to)),
            Err(t) => outcome_json(Err(t)),
        }),
        (2, [a, b]) => Ok(outcome_json(pair(a, b).and_then(|(f, t)| transition(f, t)))),
        (3, [a, b]) => Ok(outcome_json(stage_move_values(a, b))),
        (4, [events, from, Value::Bool(authoritative)]) => {
            fold(events, from, *authoritative).map(outcome_json)
        }
        (5, [events]) => derive(events).map(outcome_json),
        _ => Err(Refusal::Input),
    }
}

/// The wasm call: `u8(op)` and UTF-8 JSON.
///
/// - op 1 `[from, to]` canTransition: `{"allowed":bool}`;
/// - op 2 `[from, to]` transition and op 3 `[from, to]` assertStageMove: `{"state":"…"}`;
/// - op 4 `[events|null, from, authoritative]` foldEvents and op 5 `[events|null]` deriveLifecycle:
///   `{"state":"…"}`;
///
/// each may instead answer `{"throws":"code"}` (what the JS throws). Status 0 for those, 1 with
/// `{"error":"input"|"too_large"}` for a refusal.
pub fn call(input: &[u8]) -> (u32, String) {
    if input.len() > MAX_INPUT_BYTES {
        return (1, Refusal::TooLarge.json().to_owned());
    }
    let Some((&op, body)) = input.split_first() else {
        return (1, Refusal::Input.json().to_owned());
    };
    let Some(Value::Arr(args)) = json::parse_utf8(body, MAX_DEPTH) else {
        return (1, Refusal::Input.json().to_owned());
    };
    match run(op, &args) {
        Ok(s) => (0, s),
        Err(r) => (1, r.json().to_owned()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn req(op: u8, json: &str) -> (u32, String) {
        let mut v = vec![op];
        v.extend(json.as_bytes());
        call(&v)
    }

    #[test]
    fn table() {
        assert!(can_transition(State::Planned, State::Implementing));
        assert!(!can_transition(State::Planned, State::Verifying));
        assert!(can_transition(State::Merged, State::Merged));
        for s in STATES {
            assert!(!can_transition(State::Merged, s) || s == State::Merged);
        }
        assert_eq!(
            assert_stage_move(State::Verifying, State::Merged),
            Err(Throw::MergeFromReviewing)
        );
        assert_eq!(
            assert_stage_move(State::Reviewing, State::Merged),
            Ok(State::Merged)
        );
    }

    #[test]
    fn wire() {
        assert_eq!(req(1, r#"["planned","blocked"]"#).1, r#"{"allowed":true}"#);
        assert_eq!(
            req(1, r#"["planned",1]"#).1,
            r#"{"throws":"unknown_state"}"#
        );
        assert_eq!(
            req(2, r#"["merged","planned"]"#).1,
            r#"{"throws":"illegal"}"#
        );
        assert_eq!(req(5, "[null]").1, r#"{"state":"planned"}"#);
        assert_eq!(
            req(5, r#"[[{"type":"job.started"},{"type":"job.completed"}]]"#).1,
            r#"{"state":"verifying"}"#
        );
        let hash = "a".repeat(64);
        let stage = format!(
            r#"[[{{"type":"task.stage","data":{{"from":"planned","to":"implementing"}}}},{{"type":"task.stage","data":{{"from":"implementing","to":"reviewing","reportHash":["{hash}"]}}}}]]"#
        );
        assert_eq!(
            req(5, &stage),
            (0, r#"{"throws":"report_hash"}"#.to_owned())
        );
        assert_eq!(req(4, "[[],\"planned\",1]").0, 1);
        assert_eq!(req(9, "[]").0, 1);
        assert_eq!(call(&[]).0, 1);
        assert_eq!(req(4, "{}").0, 1);
    }
}
