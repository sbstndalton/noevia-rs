//! Reuse the reviewed M6 Rust process seam from this isolated c665 worktree.
//! No shared tooling is modified. No blocking pipe joins or unbounded captures.
#[allow(dead_code)]
#[path = "../../../../tools/qualification/process.rs"]
pub mod process;
use std::{io::Read, process::Command, time::Duration};

pub fn run(command: Command, input: Vec<u8>) -> Result<process::Output, String> {
    run_within(command, input, Duration::from_secs(30))
}
/// The same seam with an explicit deadline, so failure-path controls stay short (#1371).
pub fn run_within(
    command: Command,
    input: Vec<u8>,
    deadline: Duration,
) -> Result<process::Output, String> {
    process::bounded(command, input, deadline)
}
/// The pinned core checkout for the actual-source oracles. They need a private synthetic-only
/// checkout and Node, so they run only when `M4_CORE_ORACLE` names one (as
/// NOEVIA_ROUTING_ORACLE_CORE does for routing-mode); ordinary CI skips them.
pub fn core_oracle() -> Option<String> {
    std::env::var("M4_CORE_ORACLE")
        .ok()
        .filter(|v| !v.is_empty())
}
/// Record a skipped actual-source oracle so CI can count and show it (noevia#1827): one line on
/// stderr, and one appended to `M4_ORACLE_SKIP_LOG` when the job names that file.
pub fn oracle_skipped(test: &str) {
    eprintln!("M4 oracle skipped: {test} (set M4_CORE_ORACLE to run it)");
    if let Some(log) = std::env::var_os("M4_ORACLE_SKIP_LOG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            let _ = writeln!(f, "{test}");
        }
    }
}
/// Open an oracle source without following a final symlink and without blocking on a FIFO, and
/// accept only a regular file with a single link (#1371), so a substituted path is refused.
pub fn open_regular(path: &str) -> Result<std::fs::File, String> {
    use rustix::fs::{FileType, Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| "oracle source unavailable")?;
    let stat = rustix::fs::fstat(&fd).map_err(|_| "oracle source stat failed")?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err("oracle source is not a single-link regular file".into());
    }
    Ok(std::fs::File::from(fd))
}
pub fn checked_source(root: &str, file: &str) -> Result<(), String> {
    let mut command = Command::new("git");
    command.args([
        "-C",
        root,
        "show",
        &format!("010735c7d86d30269ab3f4a351b2135faea1c8e7:server/{file}"),
    ]);
    let source = run(command, Vec::new())?;
    let file = open_regular(&format!("{root}/server/{file}"))?;
    let mut actual = Vec::new();
    file.take((process::INPUT + 1) as u64)
        .read_to_end(&mut actual)
        .map_err(|_| "oracle source read failed")?;
    if actual.len() > process::INPUT || !source.status.success() || source.stdout != actual {
        return Err("oracle source drift or size refusal".into());
    }
    Ok(())
}
