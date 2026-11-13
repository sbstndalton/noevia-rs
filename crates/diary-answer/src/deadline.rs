//! #1505: a deadline for one Diary exchange (port of `diary/agent/deadline.py`).
//!
//! The Python runs a model call on a helper thread and abandons it when the budget runs out (the
//! thread cannot be killed; #127 closes its connection). Here a call is a future: giving up drops
//! it, which closes its connection, and nothing it would have emitted afterwards is seen.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

/// Seconds: one exchange's companion answer and the decisions before its save.
pub const EXCHANGE_DEADLINE_S: f64 = 300.0;
/// Seconds with no sign of life from the companion before its answer is given up.
pub const FIRST_TOKEN_DEADLINE_S: f64 = 150.0;
/// Seconds for any single auxiliary call.
pub const AUX_STEP_DEADLINE_S: f64 = 60.0;

/// Why a call was given up (Python `ExchangeTimeout` and its messages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    BeforeCall,
    NoFirstToken,
    NotFinished,
}

impl Timeout {
    pub fn message(self) -> &'static str {
        match self {
            Self::BeforeCall => "the exchange deadline passed before the companion was called",
            Self::NoFirstToken => "no first token from the companion before its deadline",
            Self::NotFinished => "the companion did not finish before its deadline",
        }
    }
}

/// What a bounded call reports as provider progress (Python `alive()` and an emitted event).
#[derive(Clone)]
pub struct Progress(Arc<watch::Sender<bool>>);

impl Progress {
    pub fn alive(&self) {
        self.0.send_replace(true);
    }
}

fn seconds(s: f64) -> Duration {
    Duration::try_from_secs_f64(s.max(0.0)).unwrap_or(Duration::MAX)
}

/// One exchange's budget (Python `ExchangeBudget`).
pub struct Budget {
    end: Instant,
    first_token: Duration,
}

impl Budget {
    pub fn new(total_s: Option<f64>, first_token_s: Option<f64>) -> Self {
        let now = Instant::now();
        Self {
            end: now
                .checked_add(seconds(total_s.unwrap_or(EXCHANGE_DEADLINE_S)))
                .unwrap_or(now),
            first_token: seconds(first_token_s.unwrap_or(FIRST_TOKEN_DEADLINE_S)),
        }
    }

    pub fn remaining(&self) -> Duration {
        self.end.saturating_duration_since(Instant::now())
    }

    /// Runs `call(progress)` within the budget (Python `ExchangeBudget.run`). With `first_token`
    /// the call must show progress within the first-token deadline. `cap_s` limits this one call.
    pub async fn run<T, F, Fut>(
        &self,
        first_token: bool,
        cap_s: Option<f64>,
        call: F,
    ) -> Result<T, Timeout>
    where
        F: FnOnce(Progress) -> Fut,
        Fut: Future<Output = T>,
    {
        let total = match cap_s {
            Some(cap) => self.remaining().min(seconds(cap)),
            None => self.remaining(),
        };
        if total.is_zero() {
            return Err(Timeout::BeforeCall);
        }
        let (tx, mut rx) = watch::channel(false);
        let began = Instant::now();
        // This side keeps a sender, so the channel stays open whatever the call does with its copy.
        let progress = Progress(Arc::new(tx));
        let fut = call(progress.clone());
        tokio::pin!(fut);
        if first_token {
            let wait = self.first_token.min(total);
            let first = tokio::time::timeout(wait, async {
                tokio::select! {
                    biased;
                    value = &mut fut => Some(value),
                    _ = rx.wait_for(|started| *started) => None,
                }
            })
            .await;
            match first {
                Err(_) => return Err(Timeout::NoFirstToken),
                Ok(Some(value)) => return Ok(value),
                Ok(None) => {}
            }
        }
        let left = total.saturating_sub(began.elapsed());
        let done = tokio::time::timeout(left, fut)
            .await
            .map_err(|_| Timeout::NotFinished);
        drop(progress);
        done
    }
}
