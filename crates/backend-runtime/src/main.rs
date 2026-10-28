//! Isolated candidate entry point. Not selected by release packaging or production flags.
use backend_runtime::{
    api::Api,
    chat::{OpenAiModel, Runtime},
    state::StateStore,
    transport::Endpoint,
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
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
    let model = OpenAiModel::new(
        Endpoint::new(
            &endpoint,
            &[origin],
            std::env::var("INFERENCE_API_KEY").ok().as_deref(),
        )?,
        std::env::var("INFERENCE_MODEL")?,
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
            Arc::new(Runtime::new(store, Arc::new(model), BTreeMap::new())?)
        }
        #[cfg(not(target_os = "linux"))]
        {
            return Err("existing project authority requires Linux".into());
        }
    } else {
        let store = Arc::new(StateStore::open(&path)?);
        store.recover_interrupted()?;
        Arc::new(Runtime::new(store, Arc::new(model), BTreeMap::new())?)
    };
    let diary = diary_registry()?;
    let app = Api::with_diary(&data, runtime, |key| std::env::var(key).ok(), diary)?;
    #[cfg(target_os = "linux")]
    if action == "--existing-front" {
        let config = noevia_server::config::Config::from_lookup(|key| std::env::var(key).ok())?;
        let front = backend_runtime::frontend::assemble(config, app)?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(async move {
            // Candidate remains loopback-only. Production networking is not selected here.
            let listener =
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, front.config.port))
                    .await?;
            noevia_server::serve::serve(
                listener,
                front,
                noevia_server::serve::Limits::default(),
                std::future::pending(),
            )
            .await;
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
    Ok(Some(Arc::new(backend_runtime::diary_http::DiaryHttp::new(
        tenants,
    )?)))
}
