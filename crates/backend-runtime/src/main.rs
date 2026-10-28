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
    if action != "--experimental-state" {
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
    let store = Arc::new(StateStore::open(&path)?);
    store.recover_interrupted()?;
    let runtime = Arc::new(Runtime::new(store, Arc::new(model), BTreeMap::new())?);
    let app = Api::new(&data, runtime, |key| std::env::var(key).ok())?;
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
