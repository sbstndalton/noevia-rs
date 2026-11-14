use std::process::ExitCode;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;

fn main() -> ExitCode {
    // Image provenance: answered before the runtime starts, stdin is read or any state is touched.
    if std::env::args_os().nth(1).is_some_and(|a| a == "--version") {
        println!("{}", noevia_diary::version_line());
        return ExitCode::SUCCESS;
    }
    // R9 S2a: the container's main process (DIARY_MAIN=rust), with the Python app as its child.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if args.first().is_some_and(|a| a == "--main") {
        return match noevia_diary::front::parse_args(&args) {
            Ok(a) => serve_main(a),
            Err(e) => {
                eprintln!("noevia-diary: {e}");
                ExitCode::from(2)
            }
        };
    }
    serve_from_stdin()
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn serve_main(args: noevia_diary::front::Args) -> ExitCode {
    ExitCode::from(noevia_diary::front::run(args).await)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn serve_from_stdin() -> ExitCode {
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
    let app = Arc::new(noevia_diary::server::App::new(cfg));
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
