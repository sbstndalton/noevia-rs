use diary_workspace_transaction::{execute, read_request, write_reply};
use std::io::{self, Write};
fn main() {
    let result = read_request(io::stdin().lock()).and_then(|request| execute(&request));
    match result {
        Ok(reply) => {
            if write_reply(io::stdout().lock(), &reply).is_err() {
                // The transaction may already be committed. The caller must use
                // its original stable identity to resolve the uncertain result.
                let _ = io::stderr().write_all(b"workspace transaction reply unavailable\n");
                std::process::exit(3);
            }
        }
        Err(_) => {
            let _ = io::stderr().write_all(b"workspace transaction refused\n");
            std::process::exit(2);
        }
    }
}
