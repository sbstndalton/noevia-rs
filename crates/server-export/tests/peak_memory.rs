//! #1824: the peak resident memory of the largest export the route admits, measured in its own
//! process (this test binary holds only this test). Synthetic data only.
//!
//! The tenant: three chats whose histories are near the 16 MiB file cap (one ~9 MiB message
//! each, written once to the Markdown and once to conversations.json: ~54 MiB of payload), and one
//! history at the value cap (~490,000 small values, the largest tree a file may build). Together
//! just under Node's 64 MiB payload limit, so the export succeeds.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::panic
)]

use server_export::route::{export, Outcome, MAX_FILE_VALUES};

/// This process's peak resident set (Linux `VmHWM`); `None` elsewhere (on macOS, measure the
/// test binary with `/usr/bin/time -l` instead).
fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

#[test]
fn the_largest_admitted_export_stays_under_150_mib() {
    let root = tempfile::tempdir().unwrap();
    let user = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
    let dir = root.path().join("users").join(user);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("free-chats.json"),
        r#"[{"id":"a","title":"A"},{"id":"b","title":"B"},{"id":"c","title":"C"},{"id":"v","title":"V"}]"#,
    )
    .unwrap();
    let content = "abcdefgh".repeat(1_180_000); // ~9 MiB
    for id in ["a", "b", "c"] {
        std::fs::write(
            dir.join(format!("history-{id}.json")),
            format!(r#"{{"history":[{{"role":"user","content":"{content}"}}]}}"#),
        )
        .unwrap();
    }
    drop(content);
    let values = vec!["1"; MAX_FILE_VALUES - 20].join(",");
    std::fs::write(
        dir.join("history-v.json"),
        format!(r#"{{"history":[{{"role":"tool","content":"t","x":[{values}]}}]}}"#),
    )
    .unwrap();
    drop(values);
    let before = peak_rss_bytes();
    let out = export(root.path(), user, 1_800_000_000_000);
    let Outcome::Zip { len, .. } = &out else {
        panic!("{out:?}");
    };
    let len = *len;
    let peak = peak_rss_bytes();
    drop(out);
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    assert!(
        len > 60 * 1024 * 1024,
        "the export is near the limit: {len}"
    );
    let (Some(peak), Some(before)) = (peak, before) else {
        println!(
            "export of {:.1} MiB: peak RSS not measured on this OS",
            mib(len as u64)
        );
        return;
    };
    println!(
        "export of {:.1} MiB: peak RSS {:.1} MiB (before the export {:.1} MiB)",
        mib(len as u64),
        mib(peak),
        mib(before)
    );
    assert!(peak < 150 * 1024 * 1024, "peak RSS {:.1} MiB", mib(peak));
}
