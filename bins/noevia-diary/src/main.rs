use std::process::ExitCode;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut line = String::new();
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    if stdin.read_line(&mut line).await.is_err() {
        eprintln!("noevia-diary: no start-up config on stdin");
        return ExitCode::from(2);
    }
    let cfg = match noevia_diary::config::parse(&line) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("noevia-diary: {e}");
            return ExitCode::from(2);
        }
    };
    let port = cfg.port;
    let app = Arc::new(noevia_diary::server::App::new(cfg, started_at));
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("noevia-diary: cannot listen on 127.0.0.1:{port}: {e}");
            return ExitCode::from(3);
        }
    };
    eprintln!("noevia-diary: listening on 127.0.0.1:{port}");
    match noevia_diary::serve(listener, app).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("noevia-diary: server error: {e}");
            ExitCode::from(1)
        }
    }
}
