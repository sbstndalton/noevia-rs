//! The order a turn's events must keep, checked as they are pushed.
//!
//! The rule is what noevia-core's `chat.cjs` really sends, as recorded in
//! `tests/fixtures/node-turns.v1.json` (see `tests/record_turns.cjs`):
//! - the stream opens with `meta` (or `route_pending`, when the routing question opens it early);
//! - an `error` is not always the end: a failed round breaks the round loop, and the turn still sends
//!   the reply's closing events (a fallback or note `delta`, `telemetry` complete, `done`) after it;
//! - `telemetry` with phase `complete` is sent once, directly before `done`;
//! - after `done` only one `title` may follow (the title is generated after the reply completes);
//! - a stream that never sends `done` ends on an `error` (a mid-stream error frame, a failed
//!   preparation, a revoked skill), unless its client went away, when it just stops.

use serde_json::Value;

use crate::event::{Event, Phase};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderError {
    /// The first event was not `meta` or `route_pending` (the named kind).
    FirstEvent(String),
    /// An event other than one `title` arrived after `done` (the named kind).
    AfterDone(String),
    /// `title` arrived before `done`.
    TitleBeforeDone,
    /// `telemetry` complete was followed by something other than `done` (the named kind), or sent twice.
    AfterComplete(String),
    /// The turn was finished without a `done`, and its last event is not an `error`.
    Unterminated,
}

/// The order rule as a state machine over event kinds.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Order {
    started: bool,
    complete: bool,
    done: bool,
    title: bool,
    last_error: bool,
}

impl Order {
    pub fn new() -> Self {
        Self::default()
    }

    /// One event of `kind`; `complete` says whether it is `telemetry` with phase `complete`.
    pub fn push(&mut self, kind: &str, complete: bool) -> Result<(), OrderError> {
        if !self.started {
            if kind != "meta" && kind != "route_pending" {
                return Err(OrderError::FirstEvent(kind.to_string()));
            }
            self.started = true;
        }
        if self.done {
            if kind == "title" && !self.title {
                self.title = true;
                return Ok(());
            }
            return Err(OrderError::AfterDone(kind.to_string()));
        }
        if self.complete && kind != "done" {
            return Err(OrderError::AfterComplete(kind.to_string()));
        }
        match kind {
            "title" => return Err(OrderError::TitleBeforeDone),
            "done" => self.done = true,
            _ if complete => self.complete = true,
            _ => {}
        }
        self.last_error = kind == "error";
        Ok(())
    }

    /// The turn ended with its client still there.
    pub fn finish(&self) -> Result<(), OrderError> {
        if self.done || self.last_error {
            Ok(())
        } else if self.complete {
            Err(OrderError::AfterComplete("end of stream".to_string()))
        } else {
            Err(OrderError::Unterminated)
        }
    }

    pub fn has_ended(&self) -> bool {
        self.done
    }
}

/// Checks a whole recorded or built stream of event objects. `left`: the client went away, so the
/// stream may stop anywhere.
pub fn check_stream(events: &[Value], left: bool) -> Result<(), OrderError> {
    let mut order = Order::new();
    for event in events {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let complete =
            kind == "telemetry" && event.get("phase").and_then(Value::as_str) == Some("complete");
        order.push(kind, complete)?;
    }
    if left {
        Ok(())
    } else {
        order.finish()
    }
}

/// The events of one turn, in the order they were pushed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Turn {
    events: Vec<Event>,
    order: Order,
}

impl Turn {
    pub fn new() -> Self {
        Self::default()
    }

    /// A turn whose opening events were already sent elsewhere (a relay of one model round).
    pub fn resumed() -> Self {
        let mut turn = Self::default();
        turn.order.started = true;
        turn
    }

    /// Adds `event`, or refuses it when it breaks the order rule (nothing is added then).
    pub fn push(&mut self, event: Event) -> Result<(), OrderError> {
        let complete = matches!(
            event,
            Event::Telemetry {
                phase: Phase::Complete,
                ..
            }
        );
        self.order.push(event.kind(), complete)?;
        self.events.push(event);
        Ok(())
    }

    /// The turn ended with its client still there: it must have sent `done`, or ended on `error`.
    pub fn finish(&self) -> Result<(), OrderError> {
        self.order.finish()
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Whether `done` was sent.
    pub fn has_ended(&self) -> bool {
        self.order.has_ended()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::event::TimeToFirstToken;

    fn meta() -> Event {
        Event::Meta {
            model: "m".into(),
            chat_id: None,
            route: None,
        }
    }

    fn delta(text: &str) -> Event {
        Event::Delta {
            text: text.to_string(),
        }
    }

    fn complete() -> Event {
        Event::Telemetry {
            phase: Phase::Complete,
            model: "m".into(),
            time_to_first_token: TimeToFirstToken::Unknown,
        }
    }

    fn title(t: &str) -> Event {
        Event::Title {
            chat_id: "c".into(),
            title: t.into(),
        }
    }

    #[test]
    fn title_is_allowed_once_after_done() {
        let mut turn = Turn::new();
        turn.push(meta()).unwrap();
        turn.push(delta("a")).unwrap();
        turn.push(complete()).unwrap();
        turn.push(Event::Done { model: "m".into() }).unwrap();
        assert!(turn.push(title("t")).is_ok());
        assert_eq!(
            turn.push(title("u")),
            Err(OrderError::AfterDone("title".into()))
        );
        assert!(turn.finish().is_ok());
        assert_eq!(turn.events().len(), 5);
    }

    #[test]
    fn a_round_error_is_followed_by_the_closing_events() {
        let mut turn = Turn::new();
        turn.push(meta()).unwrap();
        turn.push(Event::Error {
            text: "down".into(),
            code: None,
        })
        .unwrap();
        assert!(turn.finish().is_ok(), "a stream may end on an error");
        turn.push(delta("note")).unwrap();
        assert_eq!(turn.finish(), Err(OrderError::Unterminated));
        turn.push(complete()).unwrap();
        assert_eq!(
            turn.push(delta("late")),
            Err(OrderError::AfterComplete("delta".into()))
        );
        turn.push(Event::Done { model: "m".into() }).unwrap();
        assert!(turn.finish().is_ok());
    }

    #[test]
    fn the_first_event_title_before_done_and_unterminated_are_refused() {
        let mut turn = Turn::new();
        assert_eq!(
            turn.push(delta("x")),
            Err(OrderError::FirstEvent("delta".into()))
        );
        turn.push(meta()).unwrap();
        assert_eq!(turn.push(title("t")), Err(OrderError::TitleBeforeDone));
        assert_eq!(turn.finish(), Err(OrderError::Unterminated));
        assert_eq!(turn.events().len(), 1);
    }
}
