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
pub fn checked_source(root: &str, file: &str) -> Result<(), String> {
    let mut command = Command::new("git");
    command.args([
        "-C",
        root,
        "show",
        &format!("010735c7d86d30269ab3f4a351b2135faea1c8e7:server/{file}"),
    ]);
    let source = run(command, Vec::new())?;
    let file = std::fs::File::open(format!("{root}/server/{file}"))
        .map_err(|_| "oracle source unavailable")?;
    let mut actual = Vec::new();
    file.take((process::INPUT + 1) as u64)
        .read_to_end(&mut actual)
        .map_err(|_| "oracle source read failed")?;
    if actual.len() > process::INPUT || !source.status.success() || source.stdout != actual {
        return Err("oracle source drift or size refusal".into());
    }
    Ok(())
}
