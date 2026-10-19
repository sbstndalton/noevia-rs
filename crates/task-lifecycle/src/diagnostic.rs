//! Additive opcode 6. Policy remains shared with the original five operations; only the
//! diagnostic projection is new. References name raw host values, never stringify them.
use super::*;

pub(super) struct Diagnostic {
    pub(super) code: Throw,
    site: &'static str,
    event: Option<usize>,
    current: Option<State>,
    target: Option<State>,
}
impl Diagnostic {
    fn new(code: Throw, site: &'static str) -> Self {
        Self {
            code,
            site,
            event: None,
            current: None,
            target: None,
        }
    }
    fn movement(code: Throw, from: State, to: State) -> Self {
        Self {
            current: Some(from),
            target: Some(to),
            ..Self::new(code, "move")
        }
    }
    fn json(&self) -> String {
        let mut fields = vec![format!(r#""site":"{}""#, self.site)];
        if let Some(index) = self.event {
            fields.push(format!(r#""event":{index}"#));
        }
        if let Some(state) = self.current {
            fields.push(format!(r#""current":"{}""#, state.name()));
        }
        if let Some(state) = self.target {
            fields.push(format!(r#""target":"{}""#, state.name()));
        }
        format!(
            r#"{{"throws":"{}","diagnostic":{{{}}}}}"#,
            self.code.code(),
            fields.join(",")
        )
    }
}
enum Fault {
    Policy(Diagnostic),
    Refused(Refusal),
}
impl From<Diagnostic> for Fault {
    fn from(error: Diagnostic) -> Self {
        Self::Policy(error)
    }
}
enum Step {
    Unchanged,
    State(State),
}

fn checked(value: Option<&Value>, site: &'static str) -> Result<State, Diagnostic> {
    State::from_value(value).ok_or_else(|| Diagnostic::new(Throw::UnknownState, site))
}
fn moved(from: Option<&Value>, to: State) -> Result<Step, Fault> {
    let from = checked(from, "from")?;
    advance(from, to)
        .map(Step::State)
        .map_err(|code| Diagnostic::movement(code, from, to).into())
}
fn stage(from: Option<&Value>, data: Option<&Value>, same: Option<bool>) -> Result<Step, Fault> {
    let to = checked(field(data, "to"), "event_to")?;
    let state = State::from_value(from);
    // For the raw-value single-step operation, the host supplies only strict identity. It
    // cannot choose a state or legal move. Normal folds compare their known state directly.
    let matches = match state {
        Some(state) => field(data, "from")
            .and_then(Value::as_str)
            .is_some_and(|value| is(value, state.name())),
        None => same.unwrap_or(false),
    };
    if !matches {
        return Err(Diagnostic {
            current: state,
            ..Diagnostic::new(Throw::Stale, "stale")
        }
        .into());
    }
    if to == State::Reviewing && state != Some(to) && !report_hash_ok(field(data, "reportHash")) {
        return Err(Diagnostic {
            current: state,
            target: Some(to),
            ..Diagnostic::new(Throw::ReportHash, "move")
        }
        .into());
    }
    if to == State::Merged && state != Some(State::Merged) && state != Some(State::Reviewing) {
        return Err(Diagnostic {
            current: state,
            target: Some(to),
            ..Diagnostic::new(Throw::MergeFromReviewing, "move")
        }
        .into());
    }
    let from = checked(from, "from")?;
    assert_stage_move(from, to)
        .map(Step::State)
        .map_err(|code| Diagnostic::movement(code, from, to).into())
}
fn step(
    from: Option<&Value>,
    event: &Value,
    authoritative: bool,
    same: Option<bool>,
) -> Result<Step, Fault> {
    let ty = match event {
        Value::Obj(_) => match event.get("type") {
            Some(Value::Str(value)) => value.as_slice(),
            _ => return Ok(Step::Unchanged),
        },
        Value::Deep => return Err(Fault::Refused(Refusal::Input)),
        _ => return Ok(Step::Unchanged),
    };
    let data = match event.get("data") {
        Some(value @ Value::Obj(_)) => Some(value),
        Some(Value::Deep) => return Err(Fault::Refused(Refusal::Input)),
        _ => None,
    };
    if is(ty, "task.stage") {
        return stage(from, data, same);
    }
    if is(ty, "task.revision") {
        return Ok(Step::Unchanged);
    }
    if authoritative {
        if is_failure(ty) && State::from_value(from) != Some(State::Merged) {
            return moved(from, State::Blocked);
        }
        return Ok(Step::Unchanged);
    }
    if is(ty, "job.started") {
        moved(from, State::Implementing)
    } else if is(ty, "progress") {
        match field(data, "stage").and_then(Value::as_str) {
            Some(value) if is(value, "implementing") => moved(from, State::Implementing),
            Some(value) if is(value, "verifying") => moved(from, State::Verifying),
            _ => Ok(Step::Unchanged),
        }
    } else if is(ty, "job.completed") {
        moved(from, State::Verifying)
    } else if is_failure(ty) {
        moved(from, State::Blocked)
    } else {
        Ok(Step::Unchanged)
    }
}
pub(super) fn fold(
    events: &Value,
    from: &Value,
    authoritative: bool,
) -> Result<Result<State, Diagnostic>, Refusal> {
    let list = events_of(events)?;
    let mut state = match checked(Some(from), "fromState") {
        Ok(state) => state,
        Err(error) => return Ok(Err(error)),
    };
    for (index, event) in list.iter().enumerate() {
        let value = Value::Str(state.name().encode_utf16().collect());
        match step(Some(&value), event, authoritative, None) {
            Ok(Step::Unchanged) => {}
            Ok(Step::State(next)) => state = next,
            Err(Fault::Policy(mut error)) => {
                error.event = Some(index);
                return Ok(Err(error));
            }
            Err(Fault::Refused(error)) => return Err(error),
        }
    }
    Ok(Ok(state))
}
fn result(value: Result<State, Diagnostic>) -> String {
    match value {
        Ok(state) => outcome_json(Ok(state)),
        Err(error) => error.json(),
    }
}
fn pair_result(op: i64, from: &Value, to: &Value) -> String {
    if op == 3 {
        if let Err(code) = stage_move_values(from, to) {
            if code != Throw::UnknownState {
                return Diagnostic::new(code, "pair").json();
            }
        }
    }
    let pair =
        checked(Some(from), "from").and_then(|from| checked(Some(to), "to").map(|to| (from, to)));
    match pair {
        Err(error) => error.json(),
        Ok((from, to)) if op == 1 => format!(r#"{{"allowed":{}}}"#, can_transition(from, to)),
        Ok((from, to)) => {
            let next = if op == 3 {
                assert_stage_move(from, to)
            } else {
                transition(from, to)
            };
            result(next.map_err(|code| Diagnostic::new(code, "pair")))
        }
    }
}
fn metadata() -> String {
    let names = STATES
        .iter()
        .map(|state| format!(r#""{}""#, state.name()))
        .collect::<Vec<_>>();
    let transitions = STATES
        .iter()
        .filter(|state| **state != State::Merged)
        .chain(std::iter::once(&State::Merged))
        .map(|state| {
            let next = state
                .successors()
                .iter()
                .map(|s| format!(r#""{}""#, s.name()))
                .collect::<Vec<_>>();
            format!(r#""{}":[{}]"#, state.name(), next.join(","))
        })
        .collect::<Vec<_>>();
    format!(
        r#"{{"states":[{}],"initial":"{}","transitions":{{{}}},"authorityTypes":[{}]}}"#,
        names.join(","),
        INITIAL_STATE.name(),
        transitions.join(","),
        AUTHORITY_TYPES
            .iter()
            .map(|name| format!(r#""{name}""#))
            .collect::<Vec<_>>()
            .join(",")
    )
}
pub(super) fn call(args: &[Value]) -> Result<String, Refusal> {
    // The tag is a JSON number; no host-supplied policy data is consulted.
    let Some(Value::Num(number)) = args.first() else {
        return Err(Refusal::Input);
    };
    let tag = (0_u8..=6)
        .find(|n| f64::from(*n) == *number)
        .ok_or(Refusal::Input)?;
    match (tag, args) {
        (0, [_]) => Ok(metadata()),
        (1, [_, from, to]) => Ok(pair_result(1, from, to)),
        (2, [_, from, to]) => Ok(pair_result(2, from, to)),
        (3, [_, from, to]) => Ok(pair_result(3, from, to)),
        (4, [_, events, from, Value::Bool(authoritative)]) => {
            fold(events, from, *authoritative).map(result)
        }
        (5, [_, events]) => {
            let authoritative = is_authoritative(events_of(events)?);
            let from = Value::Str(INITIAL_STATE.name().encode_utf16().collect());
            fold(events, &from, authoritative).map(result)
        }
        (6, [_, from, event, Value::Bool(authoritative), Value::Bool(same)]) => {
            match step(Some(from), event, *authoritative, Some(*same)) {
                Ok(Step::Unchanged) => Ok(r#"{"unchanged":true}"#.into()),
                Ok(Step::State(state)) => Ok(outcome_json(Ok(state))),
                Err(Fault::Policy(error)) => Ok(error.json()),
                Err(Fault::Refused(error)) => Err(error),
            }
        }
        _ => Err(Refusal::Input),
    }
}
