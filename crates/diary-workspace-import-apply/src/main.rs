use diary_workspace_import_apply::{apply, read_request};
use std::io::{self, Write};
fn main() {
    let result = if std::env::args().skip(1).collect::<Vec<_>>() == ["apply-v1"] {
        read_request(io::stdin().lock()).and_then(|(request, body)| apply(&request, &body))
    } else {
        Err(diary_workspace_import_apply::Error::Frame)
    };
    let (output, code) = match result {
        Ok(applied) => match serde_json::to_vec(&applied) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                (bytes, 0)
            }
            Err(_) => (b"unavailable: response\n".to_vec(), 3),
        },
        Err(error) => (
            format!(
                "{}: {}\n",
                if error.unavailable() {
                    "unavailable"
                } else {
                    "reject"
                },
                error.code()
            )
            .into_bytes(),
            if error.unavailable() { 3 } else { 2 },
        ),
    };
    let mut out = io::stdout().lock();
    let code = if out.write_all(&output).and_then(|()| out.flush()).is_ok() {
        code
    } else {
        3
    };
    std::process::exit(code);
}
