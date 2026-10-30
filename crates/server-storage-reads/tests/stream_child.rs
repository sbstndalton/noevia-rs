//! The streamed binary read through the real owned child (`private-request-child`, the same body as
//! `noevia-server --storage-request-child`) against a synthetic ranged WebDAV file server
//! (sbstndalton/noevia#2191 round 8). Linux only: the child sets its limits before READY.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use server_storage_reads::{
    protocol::{Chunk, Connection, Kind, Request, Response, Transport, TransportFailure},
    stream::{read_binary, ReadOptions},
    transport::ChildTransport,
    writes::StorageError,
    CHUNK_BYTES, CHUNK_FRAME, FILE_CAP,
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use zeroize::Zeroizing;

const MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum Lab {
    Honest,
    /// Ignores `Range` and sends the whole file with a 200.
    IgnoresRange,
    /// Sends 16 MiB more than the range asked for from the third request on.
    Overshoots,
    /// The declared total changes (+1) from the third request on.
    TotalChanges,
    /// The file is replaced by one of the same size from the third request on (a new ETag); a request
    /// whose `If-Range` is not the new ETag is answered with the whole file, as RFC 9110 says.
    Replaced,
    /// A slow link: the body of the first chunk stalls for 11 s half way (a link of a few Mbit/s).
    Slow,
}

struct Served {
    requests: AtomicUsize,
    bytes: AtomicU64,
    ranged: AtomicUsize,
    /// The `If-Range` header of each request, in arrival order.
    if_ranges: std::sync::Mutex<Vec<(usize, Option<String>)>>,
}

fn byte_at(i: u64) -> u8 {
    (i % 251) as u8
}

/// A server for one file of `size` bytes at `/dav/big.pdf`.
fn server(size: u64, lab: Lab) -> (u16, Arc<Served>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(Served {
        requests: AtomicUsize::new(0),
        bytes: AtomicU64::new(0),
        ranged: AtomicUsize::new(0),
        if_ranges: std::sync::Mutex::new(Vec::new()),
    });
    let state = Arc::clone(&served);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&raw).into_owned();
                let n = state.requests.fetch_add(1, Ordering::SeqCst) + 1;
                let range = head
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("Range: bytes=")
                            .or_else(|| l.strip_prefix("range: bytes="))
                    })
                    .and_then(|r| r.trim().split_once('-'))
                    .map(|(a, b)| (a.parse::<u64>().unwrap(), b.parse::<u64>().unwrap()));
                if range.is_some() {
                    state.ranged.fetch_add(1, Ordering::SeqCst);
                }
                let if_range = head.lines().find_map(|l| {
                    l.strip_prefix("If-Range: ")
                        .or_else(|| l.strip_prefix("if-range: "))
                        .map(|v| v.trim().to_owned())
                });
                state.if_ranges.lock().unwrap().push((n, if_range.clone()));
                let etag = if lab == Lab::Replaced && n >= 3 {
                    "\"v2\""
                } else {
                    "\"v1\""
                };
                let whole = lab == Lab::Replaced && if_range.as_deref().is_some_and(|v| v != etag);
                let (status, first, last, total) = match (lab, range) {
                    _ if whole => (200, 0, size.saturating_sub(1), size),
                    (Lab::IgnoresRange, _) | (_, None) => (200, 0, size.saturating_sub(1), size),
                    (_, Some((a, b))) => {
                        let mut b = b.min(size - 1);
                        if lab == Lab::Overshoots && n >= 3 {
                            b = (b + 16 * MIB).min(size - 1);
                        }
                        let total = if lab == Lab::TotalChanges && n >= 3 {
                            size + 1
                        } else {
                            size
                        };
                        (206, a, b, total)
                    }
                };
                let len = last - first + 1;
                let mut head = format!(
                    "HTTP/1.1 {status} Synthetic\r\nContent-Length: {len}\r\nETag: {etag}\r\nConnection: close\r\n"
                );
                if status == 206 {
                    head.push_str(&format!("Content-Range: bytes {first}-{last}/{total}\r\n"));
                }
                head.push_str("\r\n");
                if stream.write_all(head.as_bytes()).is_err() {
                    return;
                }
                let mut at = first;
                let mut block = vec![0u8; 256 * 1024];
                let mut stalled = lab != Lab::Slow || n != 1;
                while at <= last {
                    if !stalled && at - first >= (last - first).div_ceil(2) {
                        stalled = true;
                        let _ = stream.flush();
                        std::thread::sleep(Duration::from_secs(11));
                    }
                    let take = ((last - at + 1) as usize).min(block.len());
                    for (k, slot) in block[..take].iter_mut().enumerate() {
                        *slot = byte_at(at + k as u64);
                    }
                    if stream.write_all(&block[..take]).is_err() {
                        return;
                    }
                    state.bytes.fetch_add(take as u64, Ordering::SeqCst);
                    at += take as u64;
                }
            });
        }
    });
    (port, served)
}

fn conn(port: u16) -> Connection {
    Connection {
        kind: Kind::WebDav,
        base_url: format!("http://127.0.0.1:{port}/dav/"),
        username: "u".into(),
        secret: Zeroizing::new(b"pw".to_vec()),
        corpus_root: String::new(),
        bucket: String::new(),
        region: String::new(),
    }
}

fn child() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_private-request-child"))
}

/// Records every reply's size and the largest frame the child was allowed to send.
struct Watch<'a> {
    inner: ChildTransport<'a>,
    replies: usize,
    largest: usize,
    caps: Vec<usize>,
}
impl Transport for Watch<'_> {
    fn elapsed(&self) -> Duration {
        self.inner.elapsed()
    }
    fn amz_date(&self) -> String {
        self.inner.amz_date()
    }
    fn delay(&mut self, d: Duration) -> Result<(), TransportFailure> {
        self.inner.delay(d)
    }
    fn attempt(&mut self, r: Request) -> Result<Response, TransportFailure> {
        self.inner.attempt(r)
    }
    fn attempt_chunk(&mut self, r: Request) -> Result<Chunk, TransportFailure> {
        self.caps.push(r.body_cap);
        let reply = self.inner.attempt_chunk(r);
        if let Ok(chunk) = &reply {
            self.replies += 1;
            self.largest = self.largest.max(chunk.bytes.len());
        }
        reply
    }
}

fn read(
    port: u16,
    cap: u64,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), StorageError>,
    watch: &mut Watch<'_>,
) -> (Result<u64, StorageError>, Vec<Option<u64>>) {
    let mut before = Vec::new();
    let mut hook = |d: Option<u64>| {
        before.push(d);
        Ok(())
    };
    let r = read_binary(
        &conn(port),
        "big.pdf",
        ReadOptions {
            cap,
            timeout: Duration::from_secs(120),
            before_body: &mut hook,
        },
        sink,
        watch,
    );
    (r, before)
}

fn watch(cancelled: &AtomicBool) -> Watch<'_> {
    Watch {
        inner: ChildTransport::new(child(), &[], cancelled),
        replies: 0,
        largest: 0,
        caps: Vec::new(),
    }
}

static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Linux only, and one test at a time: the memory test reads this process's peak.
fn linux_only() -> bool {
    if !cfg!(target_os = "linux") {
        eprintln!("skipped: the owned child runs only on Linux");
        return false;
    }
    true
}
fn serial() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner())
}

#[test]
fn a_file_larger_than_one_frame_is_read_in_ranged_chunks_through_the_child() {
    if !linux_only() {
        return;
    }
    let _one = serial();
    let size = 30 * MIB + 17;
    let (port, served) = server(size, Lab::Honest);
    let cancelled = AtomicBool::new(false);
    let mut w = watch(&cancelled);
    let mut at = 0u64;
    let mut sink = |b: &[u8]| {
        for (k, v) in b.iter().enumerate() {
            assert_eq!(*v, byte_at(at + k as u64), "byte {}", at + k as u64);
        }
        at += b.len() as u64;
        Ok(())
    };
    let (r, before) = read(port, FILE_CAP, &mut sink, &mut w);
    assert_eq!(r, Ok(size));
    assert_eq!(at, size);
    assert_eq!(before, vec![Some(size)]);
    assert_eq!(served.requests.load(Ordering::SeqCst), 4);
    assert_eq!(
        served.ranged.load(Ordering::SeqCst),
        4,
        "every request was ranged"
    );
    assert!(
        w.largest <= CHUNK_BYTES,
        "a frame carried {} bytes",
        w.largest
    );
    assert!(w.caps.iter().all(|c| *c == CHUNK_FRAME));
}

#[test]
fn a_file_one_byte_over_the_cap_is_refused_before_its_body_is_read() {
    if !linux_only() {
        return;
    }
    let _one = serial();
    let (port, served) = server(FILE_CAP + 1, Lab::Honest);
    let cancelled = AtomicBool::new(false);
    let mut w = watch(&cancelled);
    let mut taken = 0usize;
    let mut sink = |b: &[u8]| {
        taken += b.len();
        Ok(())
    };
    let (r, before) = read(port, FILE_CAP, &mut sink, &mut w);
    let e = r.unwrap_err();
    assert_eq!(e.status, 413);
    assert_eq!(taken, 0, "the sink took nothing");
    assert!(before.is_empty(), "no lock is taken for a refused file");
    assert_eq!(served.requests.load(Ordering::SeqCst), 1);
    assert!(
        served.bytes.load(Ordering::SeqCst) <= CHUNK_BYTES as u64,
        "storage was asked for one chunk only"
    );
}

#[test]
fn a_lying_range_is_refused_mid_stream() {
    if !linux_only() {
        return;
    }
    let _one = serial();
    for lab in [Lab::Overshoots, Lab::TotalChanges] {
        let (port, _) = server(40 * MIB, lab);
        let cancelled = AtomicBool::new(false);
        let mut w = watch(&cancelled);
        let mut taken = 0u64;
        let mut sink = |b: &[u8]| {
            taken += b.len() as u64;
            Ok(())
        };
        let (r, _) = read(port, FILE_CAP, &mut sink, &mut w);
        let e = r.unwrap_err();
        assert_eq!(e.status, 502, "{}", e.message);
        assert_eq!(taken, 2 * CHUNK_BYTES as u64, "stopped at the third chunk");
    }
}

#[test]
fn later_chunks_ask_only_for_the_same_file_and_a_replaced_file_is_refused() {
    if !linux_only() {
        return;
    }
    let _one = serial();
    // Honest: the first request has no validator, every later one carries the first reply's ETag.
    let (port, served) = server(20 * MIB, Lab::Honest);
    let cancelled = AtomicBool::new(false);
    let mut w = watch(&cancelled);
    assert_eq!(
        read(port, FILE_CAP, &mut |_| Ok(()), &mut w).0,
        Ok(20 * MIB)
    );
    assert_eq!(
        *served.if_ranges.lock().unwrap(),
        vec![
            (1, None),
            (2, Some("\"v1\"".to_owned())),
            (3, Some("\"v1\"".to_owned()))
        ]
    );
    // Replaced by a file of the same size at the third request: the whole new file comes back as a
    // 200 and the read ends, with what was staged left to the caller to drop.
    let (port, _) = server(40 * MIB, Lab::Replaced);
    let mut w = watch(&cancelled);
    let mut taken = 0u64;
    let mut sink = |b: &[u8]| {
        taken += b.len() as u64;
        Ok(())
    };
    let (r, _) = read(port, FILE_CAP, &mut sink, &mut w);
    let e = r.unwrap_err();
    assert_eq!(
        (e.status, e.message.as_str()),
        (502, "the file changed in storage while it was being read")
    );
    assert_eq!(
        taken,
        2 * CHUNK_BYTES as u64,
        "nothing of the new file was taken"
    );
}

/// The body of a chunk has the whole attempt's time (20 s), not the 10 s every small reply gets: a slow link
/// that needs 12 s for 8 MiB reads the file (core's whole-read deadline is 30 s plus a second a MiB).
#[test]
fn a_slow_link_gets_the_attempts_time_for_a_chunks_body() {
    if !linux_only() {
        return;
    }
    let _one = serial();
    let (port, _) = server(3 * MIB, Lab::Slow);
    let cancelled = AtomicBool::new(false);
    let mut w = watch(&cancelled);
    let mut got = 0u64;
    let mut sink = |b: &[u8]| {
        got += b.len() as u64;
        Ok(())
    };
    let (r, _) = read(port, FILE_CAP, &mut sink, &mut w);
    assert_eq!(
        r,
        Ok(3 * MIB),
        "a stall of 11 s inside one chunk is within the attempt"
    );
    assert_eq!(got, 3 * MIB);
}

#[test]
fn a_server_without_ranges_serves_a_small_file_and_refuses_a_large_one() {
    if !linux_only() {
        return;
    }
    let _one = serial();
    let (port, _) = server(2 * MIB, Lab::IgnoresRange);
    let cancelled = AtomicBool::new(false);
    let mut w = watch(&cancelled);
    let mut got = 0u64;
    let mut sink = |b: &[u8]| {
        got += b.len() as u64;
        Ok(())
    };
    assert_eq!(read(port, FILE_CAP, &mut sink, &mut w).0, Ok(2 * MIB));
    let (port, _) = server(20 * MIB, Lab::IgnoresRange);
    let mut w = watch(&cancelled);
    let (r, _) = read(port, FILE_CAP, &mut |_| Ok(()), &mut w);
    assert_eq!(r.unwrap_err().status, 502);
}

/// Peak resident memory of this process while a 90 MiB file is read through the child: the frames
/// are at most one chunk, so the whole file is never held (no sink keeps anything).
#[test]
fn peak_memory_stays_bounded_by_the_chunk_not_the_file() {
    if !linux_only() {
        return;
    }
    let _one = serial();
    fn hwm_kib() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap()
    }
    let size = 90 * MIB;
    let (port, _) = server(size, Lab::Honest);
    let cancelled = AtomicBool::new(false);
    let mut w = watch(&cancelled);
    let before = hwm_kib();
    let mut total = 0u64;
    let mut sink = |b: &[u8]| {
        total += b.len() as u64;
        Ok(())
    };
    let (r, _) = read(port, FILE_CAP, &mut sink, &mut w);
    assert_eq!(r, Ok(size));
    let grown = hwm_kib().saturating_sub(before) / 1024;
    assert!(w.largest <= CHUNK_BYTES);
    assert!(w.replies >= 12);
    // One chunk is 8 MiB; the pump's buffer and the copy handed out are about two of them. The
    // whole file (90 MiB) would show here.
    assert!(
        grown < 48,
        "peak grew by {grown} MiB reading {} MiB",
        size / MIB
    );
}
