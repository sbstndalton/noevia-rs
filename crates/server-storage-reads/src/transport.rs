//! The production [`Transport`]: every attempt runs in a fresh owned child process (noevia#1368).
//!
//! The child is a fixed executable mode (`noevia-server --storage-request-child`, or the crate's
//! `private-request-child` binary), never a user command. Before READY it drops dumpability and
//! sets core/address-space/CPU limits (Linux only; elsewhere it refuses); the parent then sends
//! one request over a bounded pipe and reads one capped reply. The parent owns the child's
//! process group: deadline, cancellation and every error path kill and reap it.
//!
//! Bounds per attempt: the reply is at most the body cap + 1 plus 18 bytes of framing (4 MiB + 19
//! for a listing, 800 000 + 19 for a file), decoded once into text of at most the cap. Redirects,
//! proxies, cookies and implicit retries are refused by the child; the one pre-header retry
//! belongs to [`crate::protocol::execute`].
use crate::{
    admission::Admission,
    isolated,
    protocol::{decoded_fixture, Request, Response, Transport, TransportFailure},
    response_ipc::Reply,
};
use std::{
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The fixed child mode argument of `noevia-server`.
pub const CHILD_ARG: &str = "--storage-request-child";

pub struct ChildTransport<'a> {
    program: PathBuf,
    args: &'static [&'static str],
    start: Instant,
    cancelled: &'a AtomicBool,
    owner: String,
}

impl<'a> ChildTransport<'a> {
    /// `program` + `args` must name the fixed child mode. `owner` is the validated identity.
    pub fn new(
        program: PathBuf,
        args: &'static [&'static str],
        owner: &str,
        cancelled: &'a AtomicBool,
    ) -> Self {
        Self {
            program,
            args,
            start: Instant::now(),
            cancelled,
            owner: owner.to_owned(),
        }
    }
}

impl Transport for ChildTransport<'_> {
    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
    fn amz_date(&self) -> String {
        amz_date(SystemTime::now())
    }
    fn delay(&mut self, delay: Duration) -> Result<(), TransportFailure> {
        let end = Instant::now() + delay;
        while Instant::now() < end {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(TransportFailure::Other);
            }
            std::thread::sleep(Duration::from_millis(10).min(end - Instant::now()));
        }
        Ok(())
    }
    fn attempt(&mut self, request: Request) -> Result<Response, TransportFailure> {
        let cap = request.body_cap;
        let budget = request.attempt_timeout;
        // One attempt at a time per transport; the per-request admission is the caller's.
        let admission = Admission::default();
        let permit = admission
            .try_acquire(&self.owner)
            .map_err(|_| TransportFailure::Other)?;
        let mut command = Command::new(&self.program);
        command.args(self.args);
        let reply = isolated::attempt_dns(command, request, budget, self.cancelled, permit)
            .map_err(|error| match error {
                isolated::Error::Deadline => TransportFailure::Timeout,
                _ => TransportFailure::Other,
            })?;
        match reply.view() {
            Ok(Reply::Http {
                status, payload, ..
            }) => Ok(Response {
                status,
                body: decoded_fixture(payload, cap).map_err(|_| TransportFailure::Body)?,
            }),
            Ok(Reply::Failure {
                phase: crate::response_ipc::Phase::Body,
                failure: TransportFailure::Timeout,
            }) => Err(TransportFailure::Body),
            Ok(Reply::Failure { failure, .. }) => Err(failure),
            Err(_) => Err(TransportFailure::Other),
        }
    }
}

/// SigV4 `x-amz-date` (`YYYYMMDDTHHMMSSZ`) for a wall-clock time, as Node's
/// `new Date().toISOString()` with separators removed.
pub fn amz_date(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// The child process body: limits before READY, one request, one framed reply.
pub fn child_main() -> std::process::ExitCode {
    fn run() -> Result<(), ()> {
        isolated::linux_setup().map_err(|_| ())?;
        let runtime = crate::child_http::runtime().map_err(|_| ())?;
        let policy = crate::child_http::ProductionTls::new().map_err(|_| ())?;
        let request = crate::child_request::receive(
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
        )
        .map_err(|_| ())?;
        let cap = request.body_cap;
        let attempt = crate::child_http::prepare_owned_dns(request, &policy).map_err(|_| ())?;
        let frame = runtime
            .block_on(crate::child_http::execute_owned_dns(attempt))
            .map_err(|_| ())?;
        crate::child_request::network_reply(&mut std::io::stdout().lock(), &frame, cap)
            .map_err(|_| ())
    }
    if run().is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn amz_date_matches_known_instants() {
        for (secs, expected) in [
            (0, "19700101T000000Z"),
            (951_782_400, "20000229T000000Z"),
            (1_700_000_000, "20231114T221320Z"),
            (4_102_444_799, "20991231T235959Z"),
        ] {
            assert_eq!(
                amz_date(UNIX_EPOCH + Duration::from_secs(secs)),
                expected,
                "{secs}"
            );
        }
    }
}
