//! #1824 / #1830: the peak resident memory of the largest exports the route admits, measured in
//! its own process (this test binary holds only this test). Synthetic data only.
//!
//! Worst case: a full history file (the 12 MiB cap minus a little) is read last, while the payload made
//! so far is held; its text, the parser's rewritten copy, its tree and its Markdown and JSON then
//! exist together. Every scenario stays under Node's 64 MiB payload limit, so it is exported:
//!
//! - `held`: ~48 MiB of payload first (three ~8 MiB messages), then a full history whose
//!   ~5.5 MiB message is exported and whose ~6.5 MiB of reasoning is read and dropped;
//! - `content`: ~38 MiB of payload first, then a full history that is all message (~24 MiB of
//!   payload from it alone);
//!
//! With a 16 MiB history cap the `content` case peaked at ~168 MiB (macOS), over the 150 MiB
//! budget, so the cap is 12 MiB (#1830).
//! - `values`: ~54 MiB of payload first, then a history at the value cap (the largest tree).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::panic
)]

use server_export::route::{export, Outcome, HISTORY_READ_CAP, MAX_FILE_VALUES};

const MIB: usize = 1024 * 1024;
const USER: &str = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";

/// This process's peak resident set (Linux `VmHWM`); `None` elsewhere (on macOS, measure the
/// test binary with `/usr/bin/time -l` instead).
fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// A history file of one user message (`content` bytes) and, optionally, reasoning.
fn history(content: usize, reasoning: usize) -> String {
    let mut s = String::with_capacity(content + reasoning + 128);
    s.push_str(r#"{"history":[{"role":"user","content":""#);
    s.push_str(&"abcdefgh".repeat(content / 8));
    s.push('"');
    if reasoning > 0 {
        s.push_str(r#","reasoning":""#);
        s.push_str(&"r".repeat(reasoning));
        s.push('"');
    }
    s.push_str("}]}");
    s
}

/// A history file to write: one message (`content`, `reasoning` bytes), or one at the value cap.
enum Spec {
    Message(usize, usize),
    Values,
}

fn text(spec: &Spec) -> String {
    match spec {
        Spec::Message(content, reasoning) => history(*content, *reasoning),
        Spec::Values => format!(
            r#"{{"history":[{{"role":"tool","content":"t","x":[{}]}}]}}"#,
            vec!["1"; MAX_FILE_VALUES - 20].join(",")
        ),
    }
}

/// Lays out `files` (chat id, history; exported in this order), each made, written and dropped in
/// turn so the test's own data is not counted, and exports them.
fn run(name: &str, files: Vec<(&str, Spec)>) -> usize {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("users").join(USER);
    std::fs::create_dir_all(&dir).unwrap();
    let list: Vec<String> = files
        .iter()
        .map(|(id, _)| format!(r#"{{"id":"{id}","title":"{id}"}}"#))
        .collect();
    std::fs::write(dir.join("free-chats.json"), format!("[{}]", list.join(","))).unwrap();
    for (id, spec) in files {
        let text = text(&spec);
        assert!(
            text.len() as u64 <= HISTORY_READ_CAP,
            "{name}: {id} over the cap"
        );
        std::fs::write(dir.join(format!("history-{id}.json")), text).unwrap();
    }
    let out = export(root.path(), USER, 1_800_000_000_000);
    let Outcome::Zip { len, .. } = &out else {
        panic!("{name}: {out:?}");
    };
    println!("{name}: export of {:.1} MiB", *len as f64 / MIB as f64);
    *len
}

/// One scenario, in this process.
fn scenario(name: &str) -> usize {
    let full = HISTORY_READ_CAP as usize - 64 * 1024; // the cap minus a little
    match name {
        "held" => run(
            "held",
            vec![
                ("a", Spec::Message(8 * MIB, 0)),
                ("b", Spec::Message(8 * MIB, 0)),
                ("c", Spec::Message(8 * MIB, 0)),
                (
                    "last",
                    Spec::Message(11 * MIB / 2, full - 11 * MIB / 2 - 200),
                ),
            ],
        ),
        "content" => run(
            "content",
            vec![
                ("a", Spec::Message(19 * MIB / 2, 0)),
                ("b", Spec::Message(19 * MIB / 2, 0)),
                ("last", Spec::Message(full - 100, 0)),
            ],
        ),
        "values" => run(
            "values",
            vec![
                ("a", Spec::Message(9 * MIB, 0)),
                ("b", Spec::Message(9 * MIB, 0)),
                ("c", Spec::Message(9 * MIB, 0)),
                ("last", Spec::Values),
            ],
        ),
        other => panic!("unknown scenario {other}"),
    }
}

const SCENARIOS: [&str; 3] = ["held", "content", "values"];

/// Each scenario runs in a fresh process (this test binary, re-entered with
/// `NOEVIA_PEAK_SCENARIO`), so one export's peak is measured alone, as the route runs exports
/// one at a time.
#[test]
fn the_largest_admitted_exports_stay_under_150_mib() {
    if let Ok(name) = std::env::var("NOEVIA_PEAK_SCENARIO") {
        let len = scenario(&name);
        assert!(
            len > 58 * MIB,
            "{name}: the export is near the 64 MiB limit: {len}"
        );
        if let Some(peak) = peak_rss_bytes() {
            println!("PEAK {name} {peak}");
        }
        return;
    }
    let exe = std::env::current_exe().unwrap();
    for name in SCENARIOS {
        let out = std::process::Command::new(&exe)
            .args([
                "--exact",
                "the_largest_admitted_exports_stay_under_150_mib",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("NOEVIA_PEAK_SCENARIO", name)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{name}: {text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let peak = text
            .lines()
            .find_map(|l| l.strip_prefix(&format!("PEAK {name} ")))
            .and_then(|n| n.trim().parse::<u64>().ok());
        let Some(peak) = peak else {
            println!("{name}: peak RSS not measured on this OS");
            continue;
        };
        println!("{name}: peak RSS {:.1} MiB", peak as f64 / MIB as f64);
        assert!(peak < 150 * MIB as u64, "{name}: peak RSS {peak}");
    }
}
