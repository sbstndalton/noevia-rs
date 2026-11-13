//! The stream guard for a tool call's arguments while the model is still writing them: the
//! `stream-guard` crate's validator, fed the text as it arrives, reporting the first violation as
//! soon as it can be decided (what `runGuardedStream` in noevia-core's stream-guard.cjs does
//! around the same validator).
//!
//! The validator reads UTF-16 code units, as the JS does, so [`ArgGuard::feed`] converts each
//! chunk with `encode_utf16`. A surrogate pair split across two chunks is handled by the validator
//! itself. The abort and the one correction retry stay with the caller.

use stream_guard::{
    Error, Reason, Schema, State, Validator, Violation, DEFAULT_MAX_BYTES, DEFAULT_MAX_DEPTH,
};

/// What the guard reports: the violation's message, path and rule. Message and path are the
/// JS's, as UTF-16 code units; use [`Failure::message_text`] for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: Vec<u16>,
    pub path: Vec<u16>,
    pub reason: Reason,
}

impl Failure {
    fn from_violation(v: &Violation) -> Self {
        Failure {
            message: v.message.clone(),
            path: v.path.clone(),
            reason: v.reason,
        }
    }

    pub fn message_text(&self) -> String {
        String::from_utf16_lossy(&self.message)
    }
}

/// Guards one tool call's argument text against its schema.
pub struct ArgGuard<'s> {
    validator: Validator<'s>,
    failure: Option<Failure>,
}

impl<'s> ArgGuard<'s> {
    /// A guard with the defaults the JS uses (`maxDepth`, `maxBytes`).
    pub fn new(schema: &'s Schema) -> Result<Self, Error> {
        Self::with_limits(schema, DEFAULT_MAX_DEPTH, DEFAULT_MAX_BYTES)
    }

    pub fn with_limits(schema: &'s Schema, max_depth: i64, max_bytes: i64) -> Result<Self, Error> {
        let state = State::new(max_depth, max_bytes)?;
        Ok(ArgGuard {
            validator: Validator::new(schema, state),
            failure: None,
        })
    }

    /// Feeds one chunk of argument text. Returns the first failure, once there is one; later
    /// chunks change nothing.
    pub fn feed(&mut self, chunk: &str) -> Option<&Failure> {
        if self.failure.is_none() {
            let units: Vec<u16> = chunk.encode_utf16().collect();
            if let Some(v) = self.validator.feed(&units) {
                self.failure = Some(Failure::from_violation(v));
            }
        }
        self.failure.as_ref()
    }

    /// Ends the text. A complete, valid document reports no failure.
    pub fn end(&mut self) -> Option<&Failure> {
        if self.failure.is_none() {
            if let Some(v) = self.validator.end() {
                self.failure = Some(Failure::from_violation(v));
            }
        }
        self.failure.as_ref()
    }

    pub fn failure(&self) -> Option<&Failure> {
        self.failure.as_ref()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use stream_guard::Reason;

    const SCHEMA: &[u8] = br#"{"type":"object","properties":{"query":{"type":"string","maxLength":8}},"required":["query"],"additionalProperties":false}"#;

    fn schema() -> Schema {
        Schema::parse(SCHEMA).unwrap_or_else(|_| panic!("the fixture schema must compile"))
    }

    #[test]
    fn a_valid_document_fed_in_pieces_has_no_failure() {
        let s = schema();
        let mut g = ArgGuard::new(&s).unwrap_or_else(|_| panic!("defaults are valid"));
        assert!(g.feed("{\"query\":\"ab").is_none());
        assert!(g.feed("c\"}").is_none());
        assert!(g.end().is_none());
    }

    #[test]
    fn an_unknown_key_is_reported_at_the_key() {
        let s = schema();
        let mut g = ArgGuard::new(&s).unwrap_or_else(|_| panic!("defaults are valid"));
        let failure = g.feed("{\"other\":1}").cloned();
        assert_eq!(failure.map(|f| f.reason), Some(Reason::UnknownProperty));
    }

    #[test]
    fn a_string_over_max_length_is_reported() {
        let s = schema();
        let mut g = ArgGuard::new(&s).unwrap_or_else(|_| panic!("defaults are valid"));
        let failure = g.feed("{\"query\":\"123456789\"}").cloned();
        assert_eq!(failure.map(|f| f.reason), Some(Reason::MaxLength));
    }

    #[test]
    fn a_truncated_document_fails_at_end_and_the_first_failure_sticks() {
        let s = schema();
        let mut g = ArgGuard::new(&s).unwrap_or_else(|_| panic!("defaults are valid"));
        assert!(g.feed("{\"query\":\"a\"").is_none());
        assert!(g.end().is_some());
        let first = g.failure().cloned();
        assert!(g.feed("garbage").cloned() == first);
    }
}
