//! The access-token cache and the refresh-once-on-401 rule (`session`, `authorised`), without I/O:
//! the caller asks the broker and the backend, this decides what comes next.
//!
//! Cache rule (core #2190 round 5): a session is kept at most [`SESSION_CACHE_MS`], never inside the
//! broker's refresh margin ([`REFRESH_MARGIN_MS`] before expiry), only while the broker's change
//! signal reads the same, and never when the signal could not be read. At most
//! [`SESSION_CACHE_MAX`] users are kept; the oldest insert goes first.

use crate::request::{Refusal, Session};
use crate::texts::Texts;
use std::collections::VecDeque;

pub const REFRESH_MARGIN_MS: f64 = 5.0 * 60.0 * 1000.0;
pub const SESSION_CACHE_MS: f64 = 60.0 * 1000.0;
pub const SESSION_CACHE_MAX: usize = 1000;

#[derive(Debug, Clone)]
struct Entry {
    user: String,
    session: Session,
    until: f64,
    signal: String,
}

/// `sessions` in core: user → (session, until, signal), in insertion order.
#[derive(Debug, Default, Clone)]
pub struct SessionCache {
    entries: VecDeque<Entry>,
}

impl SessionCache {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The cached session when the caller has no rejected token, the signal is readable and equal,
    /// and it is still in time. Every other path drops the user's entry (core deletes before asking).
    pub fn lookup(
        &mut self,
        user: &str,
        signal: Option<&str>,
        rejected: bool,
        now: f64,
    ) -> Option<Session> {
        if !rejected {
            if let Some(mark) = signal {
                if let Some(hit) = self.entries.iter().find(|e| e.user == user) {
                    if hit.signal == mark && hit.until > now {
                        return Some(hit.session.clone());
                    }
                }
            }
        }
        self.forget(user);
        None
    }

    /// Keeps the broker's fresh session (core: after a successful ask).
    pub fn store(&mut self, user: &str, session: &Session, signal: Option<&str>, now: f64) {
        let margin_end = match session.expires_at {
            Some(at) if at.is_finite() && at > 0.0 => at - REFRESH_MARGIN_MS,
            _ => f64::INFINITY,
        };
        let until = (now + SESSION_CACHE_MS).min(margin_end);
        let Some(mark) = signal else { return };
        if until <= now {
            return;
        }
        // Map#set on an existing key keeps its place; lookup always deleted it first, so this appends.
        match self.entries.iter_mut().find(|e| e.user == user) {
            Some(e) => {
                e.session = session.clone();
                e.until = until;
                e.signal = mark.to_string();
            }
            None => self.entries.push_back(Entry {
                user: user.to_string(),
                session: session.clone(),
                until,
                signal: mark.to_string(),
            }),
        }
        while self.entries.len() > SESSION_CACHE_MAX {
            self.entries.pop_front();
        }
    }

    pub fn forget(&mut self, user: &str) {
        self.entries.retain(|e| e.user != user);
    }
}

/// The broker's answer to a session ask.
#[derive(Debug, Clone, PartialEq)]
pub enum BrokerAnswer {
    Ok(Session),
    /// `{ ok: false, error, status, code }` (fields as sent; any may be missing).
    Refused {
        error: Option<String>,
        status: Option<f64>,
        code: Option<String>,
    },
    /// The ask itself failed (channel down).
    Unreachable,
}

/// `brokerError(r)` / `authError('transient')` as the answer the chat loop reads.
pub fn broker_refusal(texts: &Texts, answer: &BrokerAnswer) -> Option<Refusal> {
    match answer {
        BrokerAnswer::Ok(_) => None,
        BrokerAnswer::Unreachable => Some(Refusal {
            status: 503,
            message: texts.transient.clone(),
            code: Some("transient".into()),
        }),
        BrokerAnswer::Refused {
            error,
            status,
            code,
        } => {
            let message = error
                .as_deref()
                .map_or_else(|| texts.transient.clone(), |e| js_json::js_slice(e, 300));
            let status = status
                .filter(|s| s.fract() == 0.0 && (400.0..600.0).contains(s))
                .map_or(503, |s| {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let s = s as u16;
                    s
                });
            Some(Refusal {
                status,
                message,
                code: code.clone(),
            })
        }
    }
}

/// One step of `authorised(userId, send)`. Debug is written by hand: `AskBroker` holds the token
/// the backend refused, which never appears in any output.
#[derive(Clone, PartialEq)]
pub enum Step {
    /// Ask the broker for a session (with the rejected access token on the second ask).
    AskBroker { rejected_token: Option<String> },
    /// Send the request with this session.
    Send(Session),
    /// Tell the broker this sign-in failed (the broker's reject op with this generation), then answer `Refuse`.
    Reject {
        generation: js_json::JValue,
        then: Refusal,
    },
    /// Pass the backend's answer on (any status but the handled 401s).
    Pass,
    /// Answer with this error (the `x-noevia-provider-message` error response).
    Refuse(Refusal),
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Step::AskBroker { rejected_token } => f
                .debug_struct("AskBroker")
                .field(
                    "rejected_token",
                    &rejected_token.as_ref().map(|_| "<redacted>"),
                )
                .finish(),
            Step::Send(s) => f.debug_tuple("Send").field(s).finish(),
            Step::Reject { generation, then } => f
                .debug_struct("Reject")
                .field("generation", generation)
                .field("then", then)
                .finish(),
            Step::Pass => f.write_str("Pass"),
            Step::Refuse(r) => f.debug_tuple("Refuse").field(r).finish(),
        }
    }
}

/// The state of one authorised call.
#[derive(Debug, Clone, PartialEq)]
pub struct Authorise {
    attempt: u8,
    last: Option<Session>,
}

impl Default for Authorise {
    fn default() -> Self {
        Self::new()
    }
}

impl Authorise {
    pub fn new() -> Self {
        Authorise {
            attempt: 0,
            last: None,
        }
    }

    /// The first step: always ask (the cache is consulted by the caller through [`SessionCache`]).
    pub fn start(&self) -> Step {
        Step::AskBroker {
            rejected_token: None,
        }
    }

    /// The broker answered (or the cache did).
    pub fn on_session(&mut self, texts: &Texts, answer: BrokerAnswer) -> Step {
        match answer {
            BrokerAnswer::Ok(s) => {
                self.last = Some(s.clone());
                Step::Send(s)
            }
            other => {
                let r = broker_refusal(texts, &other)
                    .unwrap_or_else(|| Refusal::new(401, texts.transient.clone()));
                Step::Refuse(r)
            }
        }
    }

    /// The backend answered with `status`. The caller discards a 401's body before the next step.
    pub fn on_status(&mut self, texts: &Texts, status: u16) -> Step {
        if status != 401 {
            return Step::Pass;
        }
        self.attempt += 1;
        match (self.attempt, self.last.take()) {
            (1, Some(s)) => Step::AskBroker {
                rejected_token: Some(s.access_token),
            },
            (_, Some(s)) => Step::Reject {
                generation: s.generation,
                then: Refusal {
                    status: 401,
                    message: texts.reconnect.clone(),
                    code: Some("reconnect".into()),
                },
            },
            (_, None) => Step::Refuse(Refusal {
                status: 401,
                message: texts.reconnect.clone(),
                code: Some("reconnect".into()),
            }),
        }
    }
}
