//! A streamed binary read (sbstndalton/noevia#2191 round 8): core storage-client.cjs
//! `readBinaryFile`, answer for answer, as a series of bounded ranged GETs instead of one whole
//! reply. Every request is one attempt in a fresh owned child (the child's 256 MiB address-space
//! limit, the 64 MiB wire cap and the 25 MiB write cap are unchanged); a reply carries at most
//! [`crate::CHUNK_BYTES`] of body, so memory stays bounded in the child and in this process however
//! large the file is.
//!
//! - **Whole-file cap** ([`ReadOptions::cap`], noevia's 96 MiB): enforced on the size the first reply
//!   declares (the `Content-Range` total) before `before_body` runs and before a byte reaches the
//!   sink, and again on the bytes actually received. A larger file is core's 413
//!   `file exceeds the <cap> byte limit`.
//! - **`before_body(declared)`** is core's hook of the same name: it runs once, after the first
//!   reply's headers and before any of the body is used, with the declared size (`None` when the
//!   response does not say). The caller takes the large-document lock there; if it fails the error is
//!   passed on and nothing is read further.
//! - **Honest ranges.** A 206 must carry exactly the range asked for, never more than was asked, and
//!   the same declared total throughout; otherwise the read is refused mid-stream with what the sink
//!   already took left to the caller to drop. A server that ignores `Range` (a 200) is accepted only
//!   when the whole file fits one chunk; a larger file is refused (502), since it cannot be read in
//!   bounded pieces.
//! - **One file from end to end.** The first reply's strong `ETag` is carried as `If-Range` on every
//!   later chunk, and every later reply must carry the same ETag: a file replaced by one of the same
//!   size mid-read answers a 200 (or another ETag) and the read is refused (502 "the file changed in
//!   storage while it was being read"), never a file made of two. A reply with no ETag, or a weak one
//!   (`W/`, which `If-Range` cannot use), leaves only the size checks. A `416` after the first chunk
//!   of a file of declared size is the file having shrunk, the same 502; it is the end only when the
//!   first reply declared no size.
//! - Retry and deadlines are core's: one repeat after 250 ms of a pre-header transport failure per
//!   request, 404 is 404, any other status 502; the whole read ends at [`ReadOptions::timeout`]
//!   (each request at most 20 s).
use crate::protocol::{self, Connection, Kind, Transport, TransportFailure};
use crate::writes::{transport_error, StorageError};
use crate::CHUNK_BYTES;
use std::time::Duration;

/// The longest one chunk request may take (the GET attempt bound).
const ATTEMPT: Duration = Duration::from_secs(20);

pub struct ReadOptions<'a> {
    /// The whole-file cap in bytes.
    pub cap: u64,
    /// The whole read's deadline (core: 30 s, plus a second per MiB of the cap for a document).
    pub timeout: Duration,
    /// Runs once with the declared size before the body is used; an error ends the read.
    pub before_body: &'a mut dyn FnMut(Option<u64>) -> Result<(), StorageError>,
}

fn too_large(cap: u64) -> StorageError {
    StorageError::new(413, format!("file exceeds the {cap} byte limit"))
}

fn changed() -> StorageError {
    StorageError::new(502, "the file changed in storage while it was being read")
}

fn lying(what: &str) -> StorageError {
    StorageError::new(
        502,
        format!("storage sent a range that cannot be trusted ({what})"),
    )
}

/// One request with core's `withRetry`: a pre-header transport failure is repeated once after 250 ms.
fn fetch(
    c: &Connection,
    path: &str,
    first: u64,
    last: u64,
    if_range: Option<&str>,
    deadline: Duration,
    t: &mut impl Transport,
) -> Result<protocol::Chunk, StorageError> {
    for attempt in 0..2 {
        let remaining = deadline
            .checked_sub(t.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| transport_error(TransportFailure::Timeout))?;
        let request = protocol::chunk_request(
            c,
            path,
            first,
            last,
            if_range,
            &t.amz_date(),
            remaining.min(ATTEMPT),
        )
        .map_err(|f| StorageError::new(f.status, f.message))?;
        match t.attempt_chunk(request) {
            Ok(chunk) => return Ok(chunk),
            Err(e) if attempt == 0 && e.node_retries() => {
                if deadline.saturating_sub(t.elapsed()) <= Duration::from_millis(250) {
                    return Err(transport_error(TransportFailure::Timeout));
                }
                t.delay(Duration::from_millis(250))
                    .map_err(transport_error)?;
            }
            Err(e) => return Err(transport_error(e)),
        }
    }
    Err(transport_error(TransportFailure::Other))
}

/// storage-client.cjs `readBinaryFile(conn, path, { cap, timeoutMs, beforeBody })`, streamed: every
/// chunk goes to `sink` in order. Returns the number of bytes read.
pub fn read_binary(
    c: &Connection,
    raw: &str,
    opts: ReadOptions<'_>,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), StorageError>,
    t: &mut impl Transport,
) -> Result<u64, StorageError> {
    let ReadOptions {
        cap,
        timeout,
        before_body,
    } = opts;
    let path = storage_path::safe_relative_path(raw)
        .ok()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| StorageError::new(400, "invalid path"))?;
    if c.kind == Kind::Local {
        return Err(StorageError::new(400, "no browsable storage connected"));
    }
    let deadline = t
        .elapsed()
        .checked_add(timeout)
        .ok_or_else(|| transport_error(TransportFailure::Timeout))?;
    let step = CHUNK_BYTES as u64;
    let mut offset: u64 = 0;
    let mut declared: Option<Option<u64>> = None;
    // The first reply's ETag; `If-Range` carries it when it is strong.
    let mut first_etag: Option<String> = None;
    let mut if_range: Option<String> = None;
    loop {
        let last = match declared {
            Some(Some(total)) => offset.saturating_add(step).min(total).saturating_sub(1),
            _ => offset.saturating_add(step - 1),
        };
        let want = last - offset + 1;
        let chunk = fetch(c, &path, offset, last, if_range.as_deref(), deadline, t)?;
        match chunk.status {
            // Nothing to read at `offset`: an empty file (the first request), or the end of a file whose
            // size was not declared. After a declared size it is a file that shrank under the read.
            416 => {
                match declared {
                    None => {
                        refuse_if_over(cap, Some(0))?;
                        before_body(Some(0))?;
                    }
                    Some(Some(_)) => return Err(changed()),
                    Some(None) => {}
                }
                return Ok(offset);
            }
            200 | 206 => {}
            status => {
                return Err(StorageError::new(
                    if status == 404 { 404 } else { 502 },
                    format!("storage returned {status}"),
                )
                .upstream(status))
            }
        }
        let len = chunk.bytes.len() as u64;
        if chunk.status == 200 {
            // The server ignored `Range`: only a whole file that fits one chunk can be taken.
            if offset != 0 && if_range.is_some() {
                // Asked "only if it is still this file": a whole file back says it is another.
                return Err(changed());
            }
            if offset != 0 || chunk.capped {
                return Err(StorageError::new(
                    502,
                    "storage does not read byte ranges, so a file this large cannot be read",
                ));
            }
            refuse_if_over(cap, chunk.total.or(Some(len)))?;
            if chunk.total.is_some_and(|t| t != len) {
                return Err(lying("length"));
            }
            before_body(chunk.total)?;
            refuse_if_over(cap, Some(len))?;
            sink(&chunk.bytes)?;
            return Ok(len);
        }
        // A 206: the range must be the one asked for, and no more bytes than asked.
        if chunk.capped || len > want {
            return Err(lying("more bytes than asked"));
        }
        if len > 0 && (chunk.first != Some(offset) || chunk.last != Some(offset + len - 1)) {
            return Err(lying("another range"));
        }
        match declared {
            None => {
                // The declared size, checked before the body is used or any more is asked for.
                refuse_if_over(cap, chunk.total)?;
                before_body(chunk.total)?;
                declared = Some(chunk.total);
                if_range = chunk.etag.clone().filter(|e| !e.starts_with("W/"));
                first_etag = chunk.etag.clone();
            }
            Some(first) if first != chunk.total => return Err(changed()),
            Some(_) => {}
        }
        if first_etag.is_some() && chunk.etag != first_etag {
            return Err(changed());
        }
        if let Some(total) = chunk.total {
            if offset + len > total {
                return Err(lying("beyond the declared size"));
            }
        }
        // And again on what actually arrived.
        if offset + len > cap {
            return Err(too_large(cap));
        }
        let short = len < want;
        match chunk.total {
            Some(total) if short && offset + len < total => {
                return Err(StorageError::new(502, "terminated"))
            }
            None if len == 0 => return Ok(offset),
            _ => {}
        }
        sink(&chunk.bytes)?;
        offset += len;
        let finished = match chunk.total {
            Some(total) => offset >= total,
            None => short,
        };
        if finished {
            return Ok(offset);
        }
    }
}

fn refuse_if_over(cap: u64, declared: Option<u64>) -> Result<(), StorageError> {
    match declared {
        Some(n) if n > cap => Err(too_large(cap)),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use crate::protocol::{Chunk, Request, Response};
    use std::cell::RefCell;
    use zeroize::Zeroizing;

    /// How the synthetic server misbehaves.
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Honest,
        /// Answers 200 with the whole body (up to a chunk), ignoring `Range`.
        IgnoresRange,
        /// Declares a total that changes on the third request.
        TotalChanges,
        /// Sends more bytes than the range asked for on the third request.
        Overshoots,
        /// Says `*` for the total and goes on for ever.
        UnknownTotalEndless,
        /// Says `*` for the total, and the file ends (a 416 past its last byte).
        UnknownSizeEnds,
        /// Fails with a transport error on the nth request (counting from 1).
        FailsAt(usize),
        /// The file shrinks to 5 MiB after the nth request (its size is declared as 5 MiB from then on, and
        /// a range past it is a 416).
        ShrinksAfter(usize),
        /// The file is replaced by one of the same size from the nth request on: its ETag is "v2"; a
        /// server that honours `If-Range` answers the other ETag's ranges with the whole file (a 200).
        ReplacedAt(usize),
        /// The same replacement, by a server that ignores `If-Range` (it serves the 206 with the new ETag).
        ReplacedIgnoringIfRangeAt(usize),
    }

    struct Fake {
        file: Vec<u8>,
        mode: Mode,
        seen: RefCell<Vec<(u64, u64)>>,
        max_reply: std::cell::Cell<usize>,
        now: Duration,
        /// The ETag the first reply carries (`None`: the server sends none).
        etag: Option<&'static str>,
        /// The `If-Range` of each request.
        if_ranges: RefCell<Vec<Option<String>>>,
    }
    impl Fake {
        fn new(len: usize, mode: Mode) -> Self {
            Fake {
                file: (0..len).map(|i| (i % 251) as u8).collect(),
                mode,
                seen: RefCell::new(Vec::new()),
                max_reply: std::cell::Cell::new(0),
                now: Duration::ZERO,
                etag: Some("\"v1\""),
                if_ranges: RefCell::new(Vec::new()),
            }
        }
    }
    fn range_of(r: &Request) -> (u64, u64) {
        let h = r
            .headers
            .iter()
            .find(|(k, _)| k == "Range")
            .map(|(_, v)| v.as_str().to_owned())
            .expect("a Range header");
        let (a, b) = h.strip_prefix("bytes=").unwrap().split_once('-').unwrap();
        (a.parse().unwrap(), b.parse().unwrap())
    }
    impl Transport for Fake {
        fn elapsed(&self) -> Duration {
            self.now
        }
        fn amz_date(&self) -> String {
            "20260101T000000Z".into()
        }
        fn attempt(&mut self, _: Request) -> Result<Response, TransportFailure> {
            Err(TransportFailure::Other)
        }
        fn delay(&mut self, _: Duration) -> Result<(), TransportFailure> {
            Ok(())
        }
        fn attempt_chunk(&mut self, request: Request) -> Result<Chunk, TransportFailure> {
            assert_eq!(request.body_cap, crate::CHUNK_FRAME);
            let (first, last) = range_of(&request);
            assert!(
                last - first < CHUNK_BYTES as u64,
                "never more than a chunk asked"
            );
            let n = {
                let mut seen = self.seen.borrow_mut();
                seen.push((first, last));
                seen.len()
            };
            if self.mode == Mode::FailsAt(n) {
                return Err(TransportFailure::Body);
            }
            self.if_ranges.borrow_mut().push(
                request
                    .headers
                    .iter()
                    .find(|(k, _)| k == "If-Range")
                    .map(|(_, v)| v.as_str().to_owned()),
            );
            let replaced = matches!(self.mode, Mode::ReplacedAt(k) | Mode::ReplacedIgnoringIfRangeAt(k) if n >= k);
            let etag = if replaced {
                Some("\"v2\"".to_owned())
            } else {
                self.etag.map(str::to_owned)
            };
            let shrunk = matches!(self.mode, Mode::ShrinksAfter(k) if n > k);
            let total = if shrunk {
                5 * MIB as u64
            } else {
                self.file.len() as u64
            };
            // A server that honours `If-Range` serves the whole file when the validator is not its ETag.
            let validator = request
                .headers
                .iter()
                .find(|(k, _)| k == "If-Range")
                .map(|(_, v)| v.as_str().to_owned());
            if let (Some(v), Some(now)) = (&validator, &etag) {
                if v != now && matches!(self.mode, Mode::ReplacedAt(_)) && replaced {
                    let cut = self.file.len().min(CHUNK_BYTES);
                    return Ok(Chunk {
                        status: 200,
                        total: Some(self.file.len() as u64),
                        first: Some(0),
                        last: None,
                        bytes: self.file[..cut].to_vec(),
                        capped: self.file.len() > CHUNK_BYTES,
                        etag,
                    });
                }
            }
            if self.mode == Mode::IgnoresRange {
                let cut = self.file.len().min(CHUNK_BYTES);
                self.max_reply.set(cut);
                return Ok(Chunk {
                    status: 200,
                    total: Some(total),
                    first: Some(0),
                    last: None,
                    bytes: self.file[..cut].to_vec(),
                    capped: self.file.len() > CHUNK_BYTES,
                    etag,
                });
            }
            if first >= total && self.mode != Mode::UnknownTotalEndless {
                return Ok(Chunk {
                    status: 416,
                    total: None,
                    first: None,
                    last: None,
                    bytes: Vec::new(),
                    capped: false,
                    etag: None,
                });
            }
            let mut end = last.min(total.saturating_sub(1));
            if self.mode == Mode::Overshoots && n == 3 {
                end = (last + 10).min(total - 1);
            }
            let bytes = if self.mode == Mode::UnknownTotalEndless {
                vec![7u8; (last - first + 1) as usize]
            } else {
                self.file[first as usize..=end as usize].to_vec()
            };
            self.max_reply.set(self.max_reply.get().max(bytes.len()));
            let reported_total = match self.mode {
                Mode::UnknownTotalEndless | Mode::UnknownSizeEnds => None,
                Mode::TotalChanges if n == 3 => Some(total + 1),
                _ => Some(total),
            };
            let got = bytes.len() as u64;
            Ok(Chunk {
                status: 206,
                total: reported_total,
                first: Some(first),
                last: Some(first + got - 1),
                bytes,
                capped: self.mode == Mode::Overshoots && n == 3 && got > last - first + 1,
                etag,
            })
        }
    }

    fn conn() -> Connection {
        Connection {
            kind: Kind::WebDav,
            base_url: "http://storage.example/dav".into(),
            username: "u".into(),
            secret: Zeroizing::new(b"pw".to_vec()),
            corpus_root: String::new(),
            bucket: String::new(),
            region: String::new(),
        }
    }

    struct Run {
        result: Result<u64, StorageError>,
        got: Vec<u8>,
        sink_calls: usize,
        before: Vec<Option<u64>>,
    }
    fn run(fake: &mut Fake, cap: u64) -> Run {
        let mut got = Vec::new();
        let mut sink_calls = 0;
        let mut before = Vec::new();
        let result = {
            let mut hook = |d: Option<u64>| {
                before.push(d);
                Ok(())
            };
            let mut sink = |b: &[u8]| {
                sink_calls += 1;
                got.extend_from_slice(b);
                Ok(())
            };
            read_binary(
                &conn(),
                "Folder/big.pdf",
                ReadOptions {
                    cap,
                    timeout: Duration::from_secs(120),
                    before_body: &mut hook,
                },
                &mut sink,
                fake,
            )
        };
        Run {
            result,
            got,
            sink_calls,
            before,
        }
    }

    const MIB: usize = 1024 * 1024;

    #[test]
    fn a_large_file_is_read_in_ranged_chunks_of_at_most_the_cap() {
        let mut fake = Fake::new(20 * MIB + 5, Mode::Honest);
        let r = run(&mut fake, crate::FILE_CAP);
        assert_eq!(r.result, Ok((20 * MIB + 5) as u64));
        assert_eq!(r.got, fake.file);
        assert_eq!(r.before, vec![Some((20 * MIB + 5) as u64)]);
        let seen = fake.seen.borrow();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert_eq!(seen[0], (0, 8 * MIB as u64 - 1));
        assert_eq!(seen[2].0, 16 * MIB as u64);
        assert!(fake.max_reply.get() <= CHUNK_BYTES);
    }

    #[test]
    fn a_file_over_the_cap_is_refused_on_its_declared_size_before_the_body() {
        let mut fake = Fake::new(96 * MIB + 1, Mode::Honest);
        let r = run(&mut fake, crate::FILE_CAP);
        let e = r.result.unwrap_err();
        assert_eq!(e.status, 413);
        assert_eq!(
            e.message,
            format!("file exceeds the {} byte limit", crate::FILE_CAP)
        );
        assert_eq!(r.sink_calls, 0, "no byte reached the sink");
        assert!(
            r.before.is_empty(),
            "the lock is not taken for a file that is refused"
        );
        assert_eq!(fake.seen.borrow().len(), 1, "no second chunk was asked for");
        // Exactly the cap is read.
        let mut fake = Fake::new(96 * MIB, Mode::Honest);
        assert_eq!(
            run(&mut fake, crate::FILE_CAP).result,
            Ok((96 * MIB) as u64)
        );
    }

    #[test]
    fn a_declared_size_that_changes_mid_stream_is_refused() {
        let mut fake = Fake::new(40 * MIB, Mode::TotalChanges);
        let r = run(&mut fake, crate::FILE_CAP);
        assert_eq!(r.result.unwrap_err().status, 502);
        assert_eq!(r.sink_calls, 2, "the third chunk never reached the sink");
    }

    #[test]
    fn a_reply_longer_than_the_range_asked_is_refused_mid_stream() {
        let mut fake = Fake::new(40 * MIB, Mode::Overshoots);
        let r = run(&mut fake, crate::FILE_CAP);
        assert_eq!(r.result.unwrap_err().status, 502);
        assert_eq!(r.sink_calls, 2);
    }

    #[test]
    fn an_unknown_size_is_still_held_to_the_cap_on_the_bytes_received() {
        let mut fake = Fake::new(1, Mode::UnknownTotalEndless);
        let r = run(&mut fake, 20 * MIB as u64);
        assert_eq!(
            r.before,
            vec![None],
            "the hook is told the size is not known"
        );
        let e = r.result.unwrap_err();
        assert_eq!(e.status, 413);
        assert_eq!(
            r.got.len(),
            16 * MIB,
            "stopped when the next chunk would pass the cap"
        );
    }

    #[test]
    fn a_server_that_ignores_range_serves_only_a_file_that_fits_one_chunk() {
        let mut fake = Fake::new(3 * MIB, Mode::IgnoresRange);
        let r = run(&mut fake, crate::FILE_CAP);
        assert_eq!(r.result, Ok((3 * MIB) as u64));
        assert_eq!(r.got, fake.file);
        let mut fake = Fake::new(20 * MIB, Mode::IgnoresRange);
        let r = run(&mut fake, crate::FILE_CAP);
        assert_eq!(r.result.unwrap_err().status, 502);
        assert_eq!(r.sink_calls, 0);
    }

    #[test]
    fn a_transport_failure_mid_read_is_the_cores_answer_and_stops_the_read() {
        let mut fake = Fake::new(30 * MIB, Mode::FailsAt(3));
        let r = run(&mut fake, crate::FILE_CAP);
        let e = r.result.unwrap_err();
        assert_eq!(e.status, 502);
        assert_eq!(r.sink_calls, 2);
        // A pre-header failure is repeated once, as core's withRetry does.
        struct Flaky(Fake, bool);
        impl Transport for Flaky {
            fn elapsed(&self) -> Duration {
                self.0.elapsed()
            }
            fn amz_date(&self) -> String {
                self.0.amz_date()
            }
            fn attempt(&mut self, r: Request) -> Result<Response, TransportFailure> {
                self.0.attempt(r)
            }
            fn delay(&mut self, d: Duration) -> Result<(), TransportFailure> {
                self.0.delay(d)
            }
            fn attempt_chunk(&mut self, r: Request) -> Result<Chunk, TransportFailure> {
                if !self.1 {
                    self.1 = true;
                    return Err(TransportFailure::PreHeaderReset);
                }
                self.0.attempt_chunk(r)
            }
        }
        let mut flaky = Flaky(Fake::new(MIB, Mode::Honest), false);
        let mut got = Vec::new();
        let mut hook = |_: Option<u64>| Ok(());
        let mut sink = |b: &[u8]| {
            got.extend_from_slice(b);
            Ok(())
        };
        let n = read_binary(
            &conn(),
            "a.pdf",
            ReadOptions {
                cap: crate::FILE_CAP,
                timeout: Duration::from_secs(30),
                before_body: &mut hook,
            },
            &mut sink,
            &mut flaky,
        );
        assert_eq!(n, Ok(MIB as u64));
    }

    #[test]
    fn a_file_that_shrinks_mid_read_is_refused_not_staged_short() {
        // The second chunk read, the file is 5 MiB; the third range starts past it: 416. With a declared
        // size that is a changed file, never "the end".
        let mut fake = Fake::new(40 * MIB, Mode::ShrinksAfter(2));
        let r = run(&mut fake, crate::FILE_CAP);
        let e = r.result.unwrap_err();
        assert_eq!(e.status, 502);
        assert_eq!(
            e.message,
            "the file changed in storage while it was being read"
        );
        assert_eq!(r.sink_calls, 2);
    }

    #[test]
    fn a_416_is_the_end_only_for_a_file_whose_size_was_not_declared() {
        // Exactly one chunk, size `*`: the second request is the 416 that ends it.
        let mut fake = Fake::new(8 * MIB, Mode::UnknownSizeEnds);
        let r = run(&mut fake, crate::FILE_CAP);
        assert_eq!(r.result, Ok((8 * MIB) as u64));
        assert_eq!(r.got, fake.file);
        assert_eq!(r.before, vec![None]);
        assert_eq!(fake.seen.borrow().len(), 2);
    }

    #[test]
    fn later_chunks_carry_the_first_replys_etag_as_if_range() {
        let mut fake = Fake::new(20 * MIB, Mode::Honest);
        let r = run(&mut fake, crate::FILE_CAP);
        assert_eq!(r.result, Ok((20 * MIB) as u64));
        assert_eq!(
            *fake.if_ranges.borrow(),
            vec![None, Some("\"v1\"".to_owned()), Some("\"v1\"".to_owned())],
            "the first request asks for the file, the rest for that file only"
        );
        // No ETag, or a weak one (If-Range cannot use it): no If-Range, only the size checks.
        for etag in [None, Some("W/\"w\"")] {
            let mut fake = Fake::new(20 * MIB, Mode::Honest);
            fake.etag = etag;
            assert_eq!(
                run(&mut fake, crate::FILE_CAP).result,
                Ok((20 * MIB) as u64)
            );
            assert!(fake.if_ranges.borrow().iter().all(Option::is_none));
        }
    }

    #[test]
    fn a_file_replaced_by_one_of_the_same_size_is_never_mixed_into_the_read() {
        let changed = "the file changed in storage while it was being read";
        // A server that honours If-Range answers the old validator with the whole new file (a 200).
        let mut fake = Fake::new(40 * MIB, Mode::ReplacedAt(3));
        let r = run(&mut fake, crate::FILE_CAP);
        let e = r.result.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (502, changed));
        assert_eq!(r.sink_calls, 2, "the third chunk never reached the sink");
        // One that ignores If-Range still shows the new ETag on its 206.
        let mut fake = Fake::new(40 * MIB, Mode::ReplacedIgnoringIfRangeAt(3));
        let r = run(&mut fake, crate::FILE_CAP);
        let e = r.result.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (502, changed));
        assert_eq!(r.sink_calls, 2);
    }

    #[test]
    fn the_hook_can_refuse_and_missing_and_empty_files_answer_as_core_does() {
        let mut fake = Fake::new(10 * MIB, Mode::Honest);
        let mut hook = |_: Option<u64>| {
            Err(StorageError::new(
                503,
                "Large-document processing is busy; refresh to retry.",
            ))
        };
        let mut sink = |_: &[u8]| Ok(());
        let e = read_binary(
            &conn(),
            "a.pdf",
            ReadOptions {
                cap: crate::FILE_CAP,
                timeout: Duration::from_secs(30),
                before_body: &mut hook,
            },
            &mut sink,
            &mut fake,
        )
        .unwrap_err();
        assert_eq!(e.status, 503);
        // An empty file: 416 on the first range is zero bytes.
        let mut fake = Fake::new(0, Mode::Honest);
        let r = run(&mut fake, crate::FILE_CAP);
        assert_eq!(r.result, Ok(0));
        assert_eq!(r.before, vec![Some(0)]);
        // A path storage-path refuses.
        let mut fake = Fake::new(1, Mode::Honest);
        let mut hook = |_: Option<u64>| Ok(());
        let mut sink = |_: &[u8]| Ok(());
        let e = read_binary(
            &conn(),
            "../x.pdf",
            ReadOptions {
                cap: 1,
                timeout: Duration::from_secs(1),
                before_body: &mut hook,
            },
            &mut sink,
            &mut fake,
        )
        .unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (400, "invalid path"));
    }
}
