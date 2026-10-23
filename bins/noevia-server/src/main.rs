//! noevia-server binary: reads the environment (see config.rs), listens on UI_HOST:UI_PORT.

use noevia_server::{config::Config, serve, App};
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// How long in-flight requests (a streaming chat) get after SIGTERM before the process exits.
const DRAIN: Duration = Duration::from_secs(5);

/// A capability probe succeeds only after every line reaches its writer.
fn write_features(writer: &mut impl Write) -> io::Result<()> {
    for feature in noevia_server::FEATURES {
        writeln!(writer, "{feature}")?;
    }
    writer.flush()
}

fn feature_probe(writer: &mut impl Write) -> ExitCode {
    match write_features(writer) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

async fn terminated() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn legacy_claim_cli(args: impl Iterator<Item = OsString>) -> ExitCode {
    let mut args = args;
    let usage = || {
        eprintln!("usage: noevia-server --claim-legacy-root <absolute-data-dir> <user-uuid>");
        ExitCode::from(2)
    };
    let Some(root) = args.next().map(PathBuf::from) else {
        return usage();
    };
    let Some(user_id) = args.next().and_then(|value| value.into_string().ok()) else {
        return usage();
    };
    if args.next().is_some() || !root.is_absolute() {
        return usage();
    }
    let result = match server_projects::legacy_claim::claim_root(&root, &user_id) {
        Ok(true) => "CLAIMED",
        Ok(false) => "SKIPPED",
        Err(server_projects::legacy_claim::ClaimError::InvalidUser) => return usage(),
        Err(_) => {
            eprintln!("noevia-server: legacy claim refused");
            return ExitCode::FAILURE;
        }
    };
    let mut stdout = io::stdout().lock();
    match writeln!(stdout, "{result}").and_then(|()| stdout.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let _program = args.next();
    let command = args.next();
    // The owned outbound storage child (#1368): it sets its own limits before READY and must not
    // start the server's runtime first.
    #[cfg(feature = "rust-storage-browse")]
    if command.as_deref() == Some(OsStr::new(server_storage_reads::transport::CHILD_ARG)) {
        return server_storage_reads::transport::child_main();
    }
    // The filesystem-only claim helper runs before the async runtime and all server config/auth
    // startup. Its caller owns/revalidates the database transaction; this process never opens DB.
    if command.as_deref() == Some(OsStr::new("--claim-legacy-root")) {
        return legacy_claim_cli(args);
    }
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(server_main()),
        Err(e) => {
            eprintln!("noevia-server: runtime: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn server_main() -> ExitCode {
    // build/web-supervisor.sh asks before choosing this front (sbstndalton/noevia#1246).
    if std::env::args().nth(1).as_deref() == Some("--features") {
        return feature_probe(&mut io::stdout().lock());
    }
    let config = match Config::from_lookup(|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("noevia-server: {e}");
            return ExitCode::from(2);
        }
    };
    for name in &config.retired {
        eprintln!(
            "noevia-server: warning: {name} is retired (sbstndalton/noevia#1071) and ignored; its routes are always the front's"
        );
    }
    let listener = match tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "noevia-server: cannot listen on {}:{}: {e}",
                config.host, config.port
            );
            return ExitCode::from(1);
        }
    };
    if !config.dist.join("index.html").is_file() {
        eprintln!(
            "noevia-server: warning: {} has no index.html; the web client will not load",
            config.dist.display()
        );
    }
    println!(
        "noevia-server listening on http://{}:{} (legacy upstream {}, trust proxy {})",
        config.host,
        config.port,
        config.upstream.authority(),
        config.trust_proxy
    );
    let app = App::new(config);
    app.statics.warm();
    // Rust owns sign-in (#1071): a schema newer than this build stops the front (identity.rs).
    let layer = std::sync::Arc::clone(&app.identity);
    // The Argon2 decoy is built once; build it before the first request can time it.
    match tokio::task::spawn_blocking(move || {
        if !server_auth::password::warm() {
            eprintln!("noevia-server: warning: Argon2 decoy unavailable");
        }
        layer.status()
    })
    .await
    {
        Ok(noevia_server::identity::Status::Ready) => {
            println!("noevia-server: identity ready (read-only cowork.db)")
        }
        Ok(noevia_server::identity::Status::Unavailable(why)) => {
            println!("noevia-server: identity unavailable for now: {why}")
        }
        Ok(noevia_server::identity::Status::Refused(why)) => {
            eprintln!("noevia-server: identity is refused: {why}");
            return ExitCode::from(1);
        }
        Err(_) => eprintln!("noevia-server: warning: identity check did not run"),
    }
    let writes = std::sync::Arc::clone(&app.writes);
    match tokio::task::spawn_blocking(move || writes.writer().map(|_| ())).await {
        Ok(Ok(())) => println!("noevia-server: Rust owns sign-in and the account tables"),
        Ok(Err(e)) => println!("noevia-server: cowork.db writer not open yet: {e}"),
        Err(_) => eprintln!("noevia-server: warning: writer check did not run"),
    }
    // #1071: permanent; NOEVIA_RUST_PROJECTS is retired (a leftover value is reported above).
    println!("noevia-server: Rust owns the project routes, projects.json and free-chats.json");
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut stop_wait = stop_rx.clone();
    // sbstndalton/noevia#1264: Node's projects.json / free-chats.json writes, loopback only.
    let switch = app.config.rust_projects;
    if let (Some(cfg), Some(data_dir)) = (
        app.config.project_store.clone(),
        app.config.data_dir.clone(),
    ) {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", cfg.port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "noevia-server: cannot listen on 127.0.0.1:{} for the project store: {e}",
                    cfg.port
                );
                return ExitCode::from(1);
            }
        };
        println!("noevia-server: project store on 127.0.0.1:{}", cfg.port);
        let store = noevia_server::project_store::Store {
            data_dir,
            token: cfg.token.into(),
            switch,
            start_id: app.config.start_id.clone().unwrap_or_default().into(),
        };
        let mut rx = stop_rx.clone();
        tokio::spawn(noevia_server::project_store::serve(
            listener,
            store,
            async move {
                let _ = rx.wait_for(|s| *s).await;
            },
        ));
    }
    #[cfg(feature = "rust-storage-reads")]
    let storage_metadata = std::sync::Arc::clone(&app.storage_metadata);
    let server = serve::serve(listener, app, serve::Limits::default(), async move {
        let mut rx = stop_rx;
        let _ = rx.wait_for(|s| *s).await;
    });
    tokio::spawn(async move {
        terminated().await;
        let _ = stop_tx.send(true);
    });
    let drained = async {
        let _ = stop_wait.wait_for(|s| *s).await;
        tokio::time::sleep(DRAIN).await;
    };
    let exit = tokio::select! {
        () = server => ExitCode::SUCCESS,
        _ = drained => ExitCode::SUCCESS,
    };
    #[cfg(feature = "rust-storage-reads")]
    let _ = storage_metadata.shutdown().await;
    exit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct ProbeWriter {
        output: Vec<u8>,
        max_write: usize,
        fail_after: Option<usize>,
        flush_error: bool,
        flushed: bool,
    }

    impl Write for ProbeWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self
                .fail_after
                .is_some_and(|limit| self.output.len() >= limit)
            {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let remaining = self
                .fail_after
                .map_or(bytes.len(), |limit| limit.saturating_sub(self.output.len()));
            let count = bytes.len().min(self.max_write).min(remaining);
            self.output.extend(bytes.iter().take(count));
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushed = true;
            if self.flush_error {
                Err(io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            }
        }
    }

    fn expected() -> Vec<u8> {
        noevia_server::FEATURES
            .iter()
            .flat_map(|f| f.bytes().chain(std::iter::once(b'\n')))
            .collect()
    }

    #[test]
    fn complete_output_handles_partial_writes_and_flushes() {
        let mut writer = ProbeWriter {
            max_write: 1,
            ..ProbeWriter::default()
        };
        assert_eq!(feature_probe(&mut writer), ExitCode::SUCCESS);
        assert_eq!(writer.output, expected());
        assert!(writer.flushed);
    }

    #[test]
    fn broken_pipe_before_or_during_output_fails_without_flush() {
        for limit in [0, 3] {
            let mut writer = ProbeWriter {
                max_write: 2,
                fail_after: Some(limit),
                ..ProbeWriter::default()
            };
            assert_eq!(feature_probe(&mut writer), ExitCode::FAILURE);
            assert_eq!(writer.output.len(), limit);
            assert!(!writer.flushed);
        }
    }

    #[test]
    fn flush_failure_is_a_failed_probe_even_after_complete_output() {
        let mut writer = ProbeWriter {
            max_write: usize::MAX,
            flush_error: true,
            ..ProbeWriter::default()
        };
        assert_eq!(feature_probe(&mut writer), ExitCode::FAILURE);
        assert_eq!(writer.output, expected());
        assert!(writer.flushed);
    }

    #[test]
    fn zero_progress_writer_fails() {
        let mut writer = ProbeWriter::default();
        assert_eq!(feature_probe(&mut writer), ExitCode::FAILURE);
        assert!(!writer.flushed);
    }
}
