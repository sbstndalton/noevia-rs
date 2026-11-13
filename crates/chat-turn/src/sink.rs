//! The order a turn's events must keep, checked as they are pushed.
//!
//! A reply is a run of non-terminal events followed by exactly one terminal (`done` or `error`).
//! Node also sends `title` after `done` (chat.cjs 2470-2490); that is the only event allowed after
//! a terminal, and only after `done`, never after `error`.

use crate::event::Event;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderError {
    /// An event arrived after the reply had ended (the named kind).
    AfterTerminal(&'static str),
    /// `title` arrived before `done`.
    TitleBeforeDone,
    /// The turn was finished without a `done` or `error`.
    MissingTerminal,
}

/// The events of one turn, in the order they were pushed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Turn {
    events: Vec<Event>,
    ended: Option<Ended>,
    title_seen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Done,
    Error,
}

impl Turn {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an event if the order allows it.
    pub fn push(&mut self, event: Event) -> Result<(), OrderError> {
        match (&event, self.ended) {
            (Event::Title { .. }, Some(Ended::Done)) if !self.title_seen => self.title_seen = true,
            (Event::Title { .. }, Some(_)) => return Err(OrderError::AfterTerminal("title")),
            (Event::Title { .. }, None) => return Err(OrderError::TitleBeforeDone),
            (_, Some(_)) => return Err(OrderError::AfterTerminal(event.kind())),
            (Event::Done { .. }, None) => self.ended = Some(Ended::Done),
            (Event::Error { .. }, None) => self.ended = Some(Ended::Error),
            (_, None) => {}
        }
        self.events.push(event);
        Ok(())
    }

    /// Checks the turn ended with a terminal event.
    pub fn finish(&self) -> Result<(), OrderError> {
        if self.ended.is_some() {
            Ok(())
        } else {
            Err(OrderError::MissingTerminal)
        }
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// True once `done` or `error` has been pushed.
    pub fn has_ended(&self) -> bool {
        self.ended.is_some()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn delta(text: &str) -> Event {
        Event::Delta {
            text: text.to_string(),
        }
    }

    #[test]
    fn title_is_allowed_once_after_done() {
        let mut turn = Turn::new();
        turn.push(delta("a")).unwrap();
        turn.push(Event::Done { model: "m".into() }).unwrap();
        assert!(turn
            .push(Event::Title {
                chat_id: "c".into(),
                title: "t".into()
            })
            .is_ok());
        assert_eq!(
            turn.push(Event::Title {
                chat_id: "c".into(),
                title: "u".into()
            }),
            Err(OrderError::AfterTerminal("title"))
        );
        assert!(turn.finish().is_ok());
    }

    #[test]
    fn nothing_follows_error_but_title_is_refused() {
        let mut turn = Turn::new();
        turn.push(Event::Error {
            text: "down".into(),
            code: None,
        })
        .unwrap();
        assert_eq!(
            turn.push(delta("late")),
            Err(OrderError::AfterTerminal("delta"))
        );
        assert_eq!(
            turn.push(Event::Title {
                chat_id: "c".into(),
                title: "t".into()
            }),
            Err(OrderError::AfterTerminal("title"))
        );
        assert!(turn.finish().is_ok());
    }

    #[test]
    fn title_before_done_and_missing_terminal_are_refused() {
        let mut turn = Turn::new();
        assert_eq!(
            turn.push(Event::Title {
                chat_id: "c".into(),
                title: "t".into()
            }),
            Err(OrderError::TitleBeforeDone)
        );
        assert_eq!(turn.finish(), Err(OrderError::MissingTerminal));
        assert_eq!(turn.events().len(), 0);
    }
}
