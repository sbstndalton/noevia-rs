//! Isolated candidate entry point. Not selected by release packaging or production flags.
use backend_runtime::{
    api::Api,
    chat::{OpenAiModel, Runtime},
    state::StateStore,
    transport::Endpoint,
};
use std::{path::PathBuf, sync::Arc};
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("native backend candidate refused startup or failed; no legacy fallback was started");
            std::process::ExitCode::FAILURE
        }
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let action = args.next().ok_or("action required")?;
    let path = PathBuf::from(args.next().ok_or("explicit state path required")?);
    if args.next().is_some() || !path.is_absolute() {
        return Err("invalid arguments".into());
    }
    if action == "--init-experimental-state" {
        StateStore::create(&path)?;
        return Ok(());
    }
    if action != "--experimental-state"
        && action != "--existing-projects"
        && action != "--existing-front"
    {
        return Err("invalid action".into());
    }
    let data = PathBuf::from(std::env::var_os("UI_DATA_DIR").ok_or("UI_DATA_DIR required")?);
    if !data.is_absolute() {
        return Err("absolute data directory required".into());
    }
    let endpoint = std::env::var("NATIVE_CANDIDATE_MODEL_ENDPOINT")?;
    let parsed = url::Url::parse(&endpoint)?;
    let origin = parsed.origin().ascii_serialization();
    let model = OpenAiModel::configured_default(
        Endpoint::new(
            &endpoint,
            &[origin],
            std::env::var("INFERENCE_API_KEY")
                .ok()
                .as_deref()
                .filter(|s| !s.is_empty() && *s != "local"),
        )?,
        std::env::var("INFERENCE_MODEL")?,
        std::env::var("DEFAULT_PROVIDER_ID").unwrap_or_else(|_| "default".into()),
        std::env::var("DEFAULT_PROVIDER_LABEL").unwrap_or_else(|_| "Local".into()),
    )?;
    let runtime = if action == "--existing-projects" || action == "--existing-front" {
        #[cfg(target_os = "linux")]
        {
            let start = std::env::var("NOEVIA_START_ID")?;
            server_projects::journal::set_start_id(&start);
            let store = Arc::new(
                backend_runtime::project_conversations::ProjectConversations::open(
                    &data, &start, &path,
                )?,
            );
            Arc::new(Runtime::new(
                store,
                Arc::new(model),
                backend_runtime::tools::core_tools(),
            )?)
        }
        #[cfg(not(target_os = "linux"))]
        {
            return Err("existing project authority requires Linux".into());
        }
    } else {
        let store = Arc::new(StateStore::open(&path)?);
        store.recover_interrupted()?;
        Arc::new(Runtime::new(
            store,
            Arc::new(model),
            backend_runtime::tools::core_tools(),
        )?)
    };
    let diary = diary_registry()?;
    let app = Api::with_diary(&data, runtime, |key| std::env::var(key).ok(), diary)?;
    #[cfg(target_os = "linux")]
    if action == "--existing-front" {
        let config = noevia_server::config::Config::from_lookup(|key| std::env::var(key).ok())?;
        let sharing = configured_dav(&app, &data, config.port)?;
        let front = backend_runtime::frontend::assemble(config, Arc::clone(&app))?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(async move {
            // Candidate remains loopback-only. Production networking is not selected here.
            let listener =
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, front.config.port))
                    .await?;
            let dav_listener = if let Some((port, service)) = sharing {
                let listener =
                    tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
                Some((
                    listener,
                    backend_runtime::frontend::dav_router(&front, service),
                ))
            } else {
                None
            };
            let replay_task = app.start_replay_worker();
            let dav_task = tokio::spawn(async move {
                if let Some((listener, router)) = dav_listener {
                    noevia_server::serve::serve_router(
                        listener,
                        router,
                        noevia_server::serve::Limits {
                            header_read: std::time::Duration::from_secs(15),
                            keep_alive_idle: std::time::Duration::from_secs(60),
                            max_connections: 64,
                        },
                        std::future::pending(),
                    )
                    .await;
                }
            });
            noevia_server::serve::serve(
                listener,
                front,
                noevia_server::serve::Limits::default(),
                std::future::pending(),
            )
            .await;
            replay_task.abort();
            let _ = replay_task.await;
            dav_task.abort();
            let _ = dav_task.await;
            Ok::<(), std::io::Error>(())
        })?;
        return Ok(());
    }
    // Loopback-only candidate: front code-network/TLS/static routing has not been integrated.
    let port = std::env::var("NATIVE_CANDIDATE_PORT")
        .ok()
        .map(|v| v.parse::<u16>())
        .transpose()?
        .unwrap_or(18021);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        axum::serve(
            listener,
            app.router()
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    })?;
    Ok(())
}

fn diary_registry(
) -> Result<Option<Arc<backend_runtime::diary_http::DiaryHttp>>, Box<dyn std::error::Error>> {
    let Some(path) = std::env::var_os("NATIVE_CANDIDATE_DIARY_REGISTRY") else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() || std::fs::metadata(&path)?.len() > 64 * 1024 {
        return Err("invalid registry".into());
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Row {
        corpus: PathBuf,
        journal: PathBuf,
    }
    let rows: std::collections::HashMap<String, Row> =
        serde_json::from_slice(&std::fs::read(path)?)?;
    if rows.len() > 128 {
        return Err("registry capacity".into());
    }
    let mut tenants = std::collections::HashMap::new();
    for (user, row) in rows {
        if !row.corpus.is_absolute() || !row.journal.is_absolute() || !row.journal.is_file() {
            return Err("invalid tenant paths".into());
        }
        if row.corpus.canonicalize()? == row.journal.canonicalize()? {
            return Err("corpus and journal must be distinct databases".into());
        }
        let corpus = backend_runtime::diary::Corpus::open(&row.corpus, &user, 3.0)?;
        let journal = Arc::new(backend_runtime::journal::Journal::open(&row.journal)?);
        tenants.insert(
            user,
            backend_runtime::diary_http::Tenant { corpus, journal },
        );
    }
    let mut diary = backend_runtime::diary_http::DiaryHttp::new(tenants)?;
    if let Ok(layout) = std::env::var("NATIVE_CANDIDATE_DIARY_LAYOUT") {
        diary = diary.with_replay(serde_json::from_str(&layout)?)?;
    }
    if let Ok(config) = std::env::var("NATIVE_CANDIDATE_WORKSPACE_PROTECTION") {
        diary = diary.with_workspace(serde_json::from_str(&config)?);
    }
    Ok(Some(Arc::new(diary)))
}

#[cfg(target_os = "linux")]
type Sharing = (
    u16,
    Arc<server_dav::DavService<backend_runtime::dav::DavBackend>>,
);
#[cfg(target_os = "linux")]
fn configured_dav(
    api: &Api,
    data: &std::path::Path,
    ui_port: u16,
) -> Result<Option<Sharing>, Box<dyn std::error::Error>> {
    let port: u16 = std::env::var("COWORK_DAV_PORT")
        .unwrap_or_else(|_| "0".into())
        .parse()?;
    if port == 0 {
        return Ok(None);
    }
    if port < 1024 || port == ui_port {
        return Err("invalid separate DAV port".into());
    }
    let scope = match std::env::var("COWORK_DAV_SCOPE")?.as_str() {
        "lan" => server_dav::Scope::Lan,
        "public" => server_dav::Scope::Public,
        _ => return Err("invalid DAV scope".into()),
    };
    let public = std::env::var("PUBLIC_ORIGIN").unwrap_or_default();
    let origin = std::env::var("COWORK_DAV_ORIGIN").unwrap_or_else(|_| {
        if scope == server_dav::Scope::Public {
            public.clone()
        } else {
            String::new()
        }
    });
    if scope == server_dav::Scope::Lan
        && !public.is_empty()
        && url::Url::parse(&origin)?.origin() == url::Url::parse(&public)?.origin()
    {
        return Err("DAV LAN authority must be separate".into());
    }
    let config =
        server_dav::Config::new(&origin, scope, std::env::var("COWORK_DAV_PROXY_TOKEN").ok())?;
    let protection = std::env::var("NATIVE_CANDIDATE_WORKSPACE_PROTECTION")
        .ok()
        .map(|v| serde_json::from_str(&v))
        .transpose()?;
    Ok(Some((
        port,
        api.dav_service(data, scope, config, protection)?,
    )))
}
