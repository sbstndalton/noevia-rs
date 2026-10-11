//! Reuse the reviewed M6 Rust process seam from this isolated c665 worktree.
//! No shared tooling is modified. No blocking pipe joins or unbounded captures.
#[allow(dead_code)]
#[path = "../../../../tools/qualification/process.rs"]
pub mod process;
use std::{io::Read, process::Command, time::Duration};

pub fn run(command: Command, input: Vec<u8>) -> Result<process::Output, String> {
    process::bounded(command, input, Duration::from_secs(30))
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
