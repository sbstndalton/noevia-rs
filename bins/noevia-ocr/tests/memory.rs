//! sbstndalton/noevia#1867 (#697: DaServer has no swap): a 90 MiB ranged /extract is streamed to the
//! temp file, so the worker's own peak stays near its idle size and the engines' peak is one rendered
//! page. Linux with the real engines only (CI's native-OCR job); elsewhere it says why it skips.
#![cfg(feature = "native-ocr")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn high_water_kib(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap()
}

#[test]
fn peak_memory_of_a_90_mib_ranged_ocr_is_bounded() {
    let (Ok(reference), Ok("1")) = (
        std::env::var("NOEVIA_OCR_REFERENCE"),
        std::env::var("NOEVIA_OCR_REAL_ENGINES").as_deref(),
    ) else {
        eprintln!(
            "SKIP 90 MiB memory bound: CI sets NOEVIA_OCR_REFERENCE and NOEVIA_OCR_REAL_ENGINES=1"
        );
        return;
    };
    if !std::path::Path::new("/proc/self/status").exists() {
        eprintln!("SKIP 90 MiB memory bound: peak RSS is read from /proc (Linux)");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let pdf = root.path().join("ninety.pdf");
    let status = std::process::Command::new("python3")
        .args([
            "-c",
            "import synthetic_pdfs,sys; synthetic_pdfs.write_ballast_pdf(sys.argv[1], 90 * 1024 * 1024)",
        ])
        .arg(&pdf)
        .current_dir(&reference)
        .status()
        .unwrap();
    assert!(status.success());
    let size = std::fs::metadata(&pdf).unwrap().len();
    assert!(size > 90 * 1024 * 1024);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-ocr"))
        .env("NOEVIA_OCR_LISTEN", format!("127.0.0.1:{port}"))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "native OCR did not listen");
        std::thread::sleep(Duration::from_millis(20));
    }
    let idle = high_water_kib(child.id());
    let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(300)))
        .unwrap();
    write!(
        socket,
        "POST /extract HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {size}\r\nX-OCR-Pages: [3]\r\n\r\n"
    )
    .unwrap();
    let mut file = std::fs::File::open(&pdf).unwrap();
    std::io::copy(&mut file, &mut socket).unwrap();
    let mut reply = Vec::new();
    socket.take(1024 * 1024).read_to_end(&mut reply).unwrap();
    let worker = high_water_kib(child.id());
    child.kill().unwrap();
    child.wait().unwrap();
    // Every process this test waited for (the generator, the worker, and the engines the worker reaped).
    let tree = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_CHILDREN)
        .unwrap()
        .max_rss();
    let text = String::from_utf8_lossy(&reply);
    assert!(text.contains("REF-2042"), "{text}");
    let report = format!("OCR (Rust) 90 MiB ranged /extract: worker idle {idle} KiB, worker peak {worker} KiB, engines+worker peak {tree} KiB");
    eprintln!("{report}");
    if let Ok(summary) = std::env::var("GITHUB_STEP_SUMMARY") {
        if let Ok(mut out) = std::fs::OpenOptions::new().append(true).open(summary) {
            let _ = writeln!(out, "{report}");
        }
    }
    assert!(
        worker - idle < 16 * 1024,
        "the body must not be held in memory"
    );
    assert!(tree < 512 * 1024, "one rendered page, not the file");
}
