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
/// `--ensure-schema`: core's start-up steps on `UI_DATA_DIR/cowork.db` (boot-schema), with core's two
/// first-run lines when a setup code is made, then the store's own schema guards on the result.
fn ensure_schema_cli(get: impl Fn(&str) -> Option<String>, err: &mut impl Write) -> ExitCode {
    use rand_core::{OsRng, RngCore};
    let Some(dir) = get("UI_DATA_DIR").filter(|d| !d.is_empty()).map(PathBuf::from) else {
        let _ = writeln!(err, "noevia-server: UI_DATA_DIR is not set");
        return ExitCode::FAILURE;
    };
    let ctx = boot_schema::Context::from_lookup(&get);
    match boot_schema::ensure(&dir, &ctx, &mut |buf| OsRng.fill_bytes(buf)) {
        Ok(booted) => {
            if let Some(code) = booted.setup_code {
                let _ = writeln!(err, "FIRST-RUN SETUP CODE: {}", code.as_str());
                let _ = writeln!(
                    err,
                    "Setup code file: {} (deleted after setup)",
                    dir.join("first-run-setup-code").display()
                );
            }
        }
        Err(e) => {
            let _ = writeln!(err, "noevia-server: {e}");
            return ExitCode::FAILURE;
        }
    }
    match server_store::Store::open(&dir) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "noevia-server: cowork.db: {e}");
            ExitCode::FAILURE
        }
    }
}

/// sbstndalton/noevia#2582: one invitation from the operator's shell (server_account::operator).
/// Command line only; stdout carries the token and nothing else.
fn operator_invitation_cli(
    args: impl Iterator<Item = OsString>,
    data_dir: Option<PathBuf>,
    now: i64,
    out: &mut impl Write,
) -> ExitCode {
    use server_account::operator::{self, Refusal, Role};
    let mut args = args;
    let usage = || {
        eprintln!("usage: noevia-server --operator-invitation <member|admin>");
        ExitCode::from(2)
    };
    let Some(role) = args.next().and_then(|v| v.into_string().ok()) else {
        return usage();
    };
    let Some(role) = Role::parse(&role) else {
        return usage();
    };
    if args.next().is_some() {
        return usage();
    }
    let Some(dir) = data_dir else {
        eprintln!("noevia-server: operator invitation refused: UI_DATA_DIR is not set");
        return ExitCode::FAILURE;
    };
    let writer = match server_store::Writer::open(&dir, server_store::RustAuth::owned()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("noevia-server: operator invitation refused: {e}");
            return ExitCode::FAILURE;
        }
    };
    let invitation = match operator::create(&writer, role, now) {
        Ok(i) => i,
        Err(Refusal::EmptyStore) => {
            eprintln!("noevia-server: operator invitation refused: no account exists yet; use first-run setup");
            return ExitCode::FAILURE;
        }
        Err(Refusal::NoActiveAdministrator) => {
            eprintln!("noevia-server: operator invitation refused: no active administrator");
            return ExitCode::FAILURE;
        }
        Err(Refusal::Store(e)) => {
            eprintln!("noevia-server: operator invitation refused: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "noevia-server: one {} invitation, valid for 24 hours and once; accept it on the sign-in page",
        role.as_str()
    );
    match writeln!(out, "{}", invitation.token).and_then(|()| out.flush()) {
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
    // Before the runtime and any listener: no network surface (sbstndalton/noevia#2582).
    if command.as_deref() == Some(OsStr::new("--operator-invitation")) {
        let data_dir = std::env::var_os("UI_DATA_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        return operator_invitation_cli(args, data_dir, now, &mut io::stdout().lock());
    }
    // Track R12: core's boot of cowork.db (crates/boot-schema), for the start-up once Node no longer runs
    // first (the removal release calls it from the web supervisor). Before the runtime and any listener.
    if command.as_deref() == Some(OsStr::new("--ensure-schema")) {
        return ensure_schema_cli(|k| std::env::var(k).ok(), &mut io::stderr().lock());
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
    noevia_server::extraction::spawn(&app);
    // sbstndalton/noevia#2153: every chat change journal entry carries this start.
    server_projects::journal::set_start_id(app.config.start_id.as_deref().unwrap_or(""));
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
    // sbstndalton/noevia#2191 round 9 (review L5): an account folder with no account row is named in the log
    // (count and ids), never removed: a database restored from an older copy would make live data look orphaned.
    noevia_server::admin_user_routes::report_orphan_folders(&app).await;
    // Complete the source-text sweep before the project store or public requests can run.
    noevia_server::source_startup::run(&app).await;

    // sbstndalton/noevia#2190 round 5: the token broker removes the rows of deleted accounts.
    #[cfg(feature = "rust-chatgpt-status")]
    if let Some(broker) = app.chatgpt_oauth.as_ref() {
        broker.sweep_once(&app).await;
    }
    // sbstndalton/noevia#2202: the MCP credential broker removes the credentials of deleted accounts
    // and servers and publishes who is ready (Node re-reads the file's signal).
    #[cfg(feature = "rust-storage-reads")]
    if let Some(mcp) = app.mcp.as_ref() {
        mcp.sweep_once(&app).await;
    }
    // sbstndalton/noevia row T: MCP tool discovery is this process's; the first listing warms the
    // catalogue so the first chat does not pay for it (a server still starting is retried by the cadence).
    #[cfg(feature = "rust-storage-reads")]
    noevia_server::mcp_discovery_backend::warm(&app);
    // sbstndalton/noevia#2190 round 7: Google Drive's state file is brought up to date with the disk,
    // and the off-site backup's nightly schedule starts.
    if let Some(drive) = app.drive.as_ref() {
        let drive = std::sync::Arc::clone(drive);
        let _ = tokio::task::spawn_blocking(move || drive.publish_startup()).await;
    }
    if let Some(drive) = app.drive.as_ref() {
        // The orphan sweep (accounts deleted while this process was down) and the state file's check.
        drive.spawn_background(&app);
    }
    if let Some(offsite) = app.offsite.as_ref() {
        offsite.spawn_scheduler(&app);
    }
    // sbstndalton/noevia#2210: the front is the evidence store's only writer; Node's producers leave
    // their records in the inbox, which is drained here.
    app.config.maint.spawn_inbox();
    // #2303: a calibration interrupted by a restart restores the preset it was testing (Core's boot
    // `modelManager.calibration?.recover()`, which is off now that the worker is native).
    // The recovery takes the launch slot before the routes serve, so no launch races it.
    let _ = app.config.spawn_calibration_recovery();
    // #2516: only with AUTOTUNE_RUNNER_IMPL=rust (Core's boot recovery is then off).
    if let Some(autotune) = app.config.autotune.as_ref() {
        autotune.spawn_boot();
    }
    {
        let maint = std::sync::Arc::clone(&app.config.maint);
        tokio::spawn(async move {
            if let Some(warning) = maint.missing_token_warning().await {
                eprintln!("{warning}");
            }
        });
    }
    // #1071: permanent; NOEVIA_RUST_PROJECTS is retired (a leftover value is reported above).
    println!("noevia-server: Rust owns the project routes, projects.json and free-chats.json");
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    // #697: the memory watchdog (Core's inference-budget-watch.cjs, which Core no longer starts:
    // rust-budget-watch). Its first reading is one interval from now; it stops with the server.
    let budget_watch = noevia_server::model_maint::budget_watch::spawn(
        &app.config.maint,
        &|k: &str| std::env::var(k).ok(),
        stop_rx.clone(),
    );
    let mut stop_wait = stop_rx.clone();
    let backup_stop = stop_tx.clone();
    let diary_backup = noevia_server::diary_backup::spawn(&app, stop_rx.clone());
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
        // Row V: the egress issuer publishes the proxy key and holds the session from the front's
        // own start, whether or not Node is up; Node only asks it for grants.
        if let Some(issuer) = app.code_egress.as_ref() {
            issuer.start();
        }
        let mut rx = stop_rx.clone();
        tokio::spawn(noevia_server::project_store::serve_with_context(
            listener,
            store,
            app.feature_context.clone(),
            Some(std::sync::Arc::clone(&app)),
            async move {
                let _ = rx.wait_for(|s| *s).await;
            },
        ));
    }
    #[cfg(feature = "rust-storage-reads")]
    let storage_metadata = std::sync::Arc::clone(&app.storage_metadata);
    let embedding_background =
        match noevia_server::embedding_background::start(&app, |key| std::env::var(key).ok()) {
            Ok(worker) => worker,
            Err(error) => {
                eprintln!("noevia-server: optional embedding background idle: {error}");
                None
            }
        };
    // `rust-chat-mirror-v1`: the change-journal consumer and the mirror and brain schedules.
    if let Some(mirror) = noevia_server::chat_mirror::ChatMirror::start(
        std::sync::Arc::clone(&app.config.research),
        app.config.data_dir.clone(),
    ) {
        mirror.install_nudge_hook();
        let _ = app.chat_mirror.set(mirror);
    } else {
        eprintln!("noevia-server: chat mirror schedule idle (no data directory)");
    }
    let dav = match noevia_server::dav::start(&app, stop_rx.clone()).await {
        Ok(worker) => worker,
        Err(error) => {
            eprintln!("noevia-server: {error}");
            return ExitCode::from(1);
        }
    };
    let server = serve::serve(listener, app, serve::Limits::default(), async move {
        let mut rx = stop_rx;
        let _ = rx.wait_for(|s| *s).await;
    });
    let worker_stop = stop_tx.clone();
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
    let _ = worker_stop.send(true);
    if let Some(mut watch) = budget_watch {
        if tokio::time::timeout(Duration::from_secs(2), &mut watch)
            .await
            .is_err()
        {
            watch.abort();
            let _ = watch.await;
        }
    }
    if let Some(worker) = dav {
        worker.abort();
        let _ = worker.await;
    }
    if let Some(worker) = embedding_background {
        worker.shutdown().await;
    }
    #[cfg(feature = "rust-storage-reads")]
    let _ = storage_metadata.shutdown().await;
    let _ = backup_stop.send(true);
    let _ = diary_backup.await;
    exit
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ensure_schema_makes_a_database_the_store_opens() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("ui-data");
        let env = |k: &str| (k == "UI_DATA_DIR").then(|| data.display().to_string());
        let mut err = Vec::new();
        assert_eq!(ensure_schema_cli(env, &mut err), ExitCode::SUCCESS);
        let said = String::from_utf8(err).unwrap();
        assert!(said.starts_with("FIRST-RUN SETUP CODE: "), "{said}");
        assert!(data.join("first-run-setup-code").exists());
        let mut err = Vec::new();
        assert_eq!(ensure_schema_cli(env, &mut err), ExitCode::SUCCESS);
        assert!(err.is_empty());
        assert_eq!(ensure_schema_cli(|_| None, &mut Vec::new()), ExitCode::FAILURE);
    }


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

    const NODE_SCHEMA: &str =
        include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");

    /// A Node-shaped cowork.db; `admin` adds one synthetic administrator.
    fn store(admin: bool) -> (tempfile::TempDir, server_store::rusqlite::Connection) {
        let dir = tempfile::tempdir().unwrap();
        let node =
            server_store::rusqlite::Connection::open(server_store::db_path(dir.path())).unwrap();
        node.execute_batch(NODE_SCHEMA).unwrap();
        if admin {
            node.execute_batch(
                "INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('qa-admin','QA-Admin','qa-admin','QA Admin','admin','x','w1',1,1)",
            )
            .unwrap();
        }
        (dir, node)
    }

    fn run(args: &[&str], dir: Option<PathBuf>) -> (ExitCode, String) {
        let mut out = Vec::new();
        let code = operator_invitation_cli(
            args.iter().map(OsString::from),
            dir,
            1_800_000_000_000,
            &mut out,
        );
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn operator_invitation_prints_only_the_token_once() {
        let (dir, node) = store(true);
        let (code, out) = run(&["admin"], Some(dir.path().to_path_buf()));
        assert_eq!(code, ExitCode::SUCCESS);
        let token = out.strip_suffix('\n').unwrap();
        assert_eq!(out.matches(token).count(), 1);
        assert_eq!(out.lines().count(), 1);
        let (hash, role): (String, String) = node
            .query_row("SELECT token_hash, role FROM invitations", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(hash, server_auth::js::digest(token));
        assert_eq!(role, "admin");
    }

    #[test]
    fn operator_invitation_refusals_print_nothing_and_write_nothing() {
        let (empty, node) = store(false);
        let (dir, with_admin) = store(true);
        let cases: [(&[&str], Option<PathBuf>, ExitCode); 6] = [
            (
                &["admin"],
                Some(empty.path().to_path_buf()),
                ExitCode::FAILURE,
            ),
            (
                &["owner"],
                Some(dir.path().to_path_buf()),
                ExitCode::from(2),
            ),
            (
                &["Admin"],
                Some(dir.path().to_path_buf()),
                ExitCode::from(2),
            ),
            (&[], Some(dir.path().to_path_buf()), ExitCode::from(2)),
            (
                &["member", "member"],
                Some(dir.path().to_path_buf()),
                ExitCode::from(2),
            ),
            (&["member"], None, ExitCode::FAILURE),
        ];
        for (args, data_dir, want) in cases {
            let (code, out) = run(args, data_dir);
            assert_eq!(code, want, "{args:?}");
            assert!(out.is_empty(), "{args:?}");
        }
        for db in [&node, &with_admin] {
            let n: i64 = db
                .query_row(
                    "SELECT (SELECT count(*) FROM invitations)+(SELECT count(*) FROM audit_events)",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0);
        }
    }

    #[test]
    fn zero_progress_writer_fails() {
        let mut writer = ProbeWriter::default();
        assert_eq!(feature_probe(&mut writer), ExitCode::FAILURE);
        assert!(!writer.flushed);
    }
}
