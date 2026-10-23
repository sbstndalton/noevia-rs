use diary_workspace_export::{export, OUTPUT_END, OUTPUT_MAGIC};
use sha2::{Digest, Sha256};
use std::io::{self, Write};

fn main() {
    let code = if std::env::args()
        .collect::<Vec<_>>()
        .get(1)
        .map(String::as_str)
        != Some("export")
        || std::env::args().count() != 2
    {
        let _ = io::stderr().write_all(b"usage: diary-workspace-export export\n");
        1
    } else {
        match export(io::stdin().lock()) {
            Ok(bytes) => {
                let digest = Sha256::digest(&bytes);
                let mut out = io::stdout().lock();
                if out
                    .write_all(OUTPUT_MAGIC)
                    .and_then(|()| out.write_all(&(bytes.len() as u64).to_be_bytes()))
                    .and_then(|()| out.write_all(&digest))
                    .and_then(|()| out.write_all(&bytes))
                    .and_then(|()| out.write_all(OUTPUT_END))
                    .is_ok()
                {
                    0
                } else {
                    1
                }
            }
            Err(error) => {
                let reply = format!("reject: {}\n", error.code());
                if io::stdout().lock().write_all(reply.as_bytes()).is_ok() {
                    2
                } else {
                    1
                }
            }
        }
    };
    std::process::exit(code);
}
