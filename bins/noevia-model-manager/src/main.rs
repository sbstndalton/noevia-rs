#![forbid(unsafe_code)]
use noevia_model_manager::{
    docker::UnixDocker, legacy_client, router, validate_token, Redactor, StateData,
};
use std::{os::unix::process::CommandExt, sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--features") {
        if cfg!(feature = "native-logs") {
            println!("native-logs");
            if cfg!(feature = "native-host") {
                println!("native-host");
            }
            if cfg!(feature = "native-sections") {
                println!("native-sections");
            }
            return Ok(());
        }
        return Err("native-logs feature is not enabled".into());
    }
    let mode = environment("MODEL_MANAGER_IMPL", "python")?;
    let host_mode = environment("MODEL_MANAGER_HOST_IMPL", "python")?;
    let sections_mode = environment("MODEL_MANAGER_SECTIONS_IMPL", "python")?;
    if !matches!(sections_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_SECTIONS_IMPL must be python or rust".into());
    }
    if sections_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-sections")) {
        return Err(
            "native-sections requires rust model manager and compiled native-sections feature"
                .into(),
        );
    }
    if !matches!(host_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_HOST_IMPL must be python or rust".into());
    }
    if host_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-host")) {
        return Err(
            "native-host requires rust model manager and compiled native-host feature".into(),
        );
    }
    if mode == "python" {
        let error = std::process::Command::new("uvicorn")
            .env_remove("NOEVIA_NATIVE_MODEL_MANAGER_CONFIRMED")
            .env_remove("NOEVIA_NATIVE_MODEL_MANAGER_HOST_CONFIRMED")
            .env_remove("NOEVIA_NATIVE_MODEL_MANAGER_SECTIONS_CONFIRMED")
            .args(["app.main:app", "--host", "0.0.0.0", "--port", "8090"])
            .exec();
        return Err(error.into());
    }
    if mode != "rust" {
        return Err("MODEL_MANAGER_IMPL must be python or rust".into());
    }
    if !cfg!(feature = "native-logs") {
        return Err("native-logs feature is not enabled".into());
    }
    // All configuration/feature/address guards run before spawning the legacy process.
    let token = validate_token(&environment("MODEL_LOADER_TOKEN", "")?)?;
    let configured_raw = environment("LLAMA_CONTAINERS", "")?;
    if configured_raw.len() > 65536 {
        return Err("LLAMA_CONTAINERS exceeded limit".into());
    }
    let mut configured = Vec::new();
    configured.try_reserve(1024)?;
    for name in configured_raw
        .split(',')
        .map(|s| {
            s.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{001c}'..='\u{001f}'))
        })
        .filter(|s| !s.is_empty())
    {
        if configured.len() >= 1024 || name.len() > 256 {
            return Err("LLAMA_CONTAINERS exceeded limit".into());
        }
        configured.push(name.to_owned());
    }
    let docker = UnixDocker::from_env()?;
    let redactor = Redactor::new()?;
    let legacy = legacy_client()?;
    let sections = if sections_mode == "rust" {
        let models = environment("MODELS_DIR", "/models")?;
        let ini = environment("MODELS_INI_PATH", "/models/models.ini")?;
        Some(Arc::new(noevia_model_manager::sections::Sections::new(
            Arc::new(
                noevia_model_manager::sections::Files::new(
                    std::path::Path::new(&models),
                    std::path::Path::new(&ini),
                )
                .map_err(|_| "native section configuration roots unavailable")?,
            ),
        )))
    } else {
        None
    };
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8090").await?;
    // Refuse an existing loopback listener before spawn: ownership is never borrowed from
    // an unknown process. uvicorn receives only the fixed private loopback address.
    let legacy_probe = std::net::TcpListener::bind("127.0.0.1:8091")?;
    drop(legacy_probe);
    let host = (host_mode == "rust").then(|| {
        Arc::new(tokio::sync::Mutex::new(
            noevia_model_manager::host::HostHistory::default(),
        ))
    });
    let mut child = tokio::process::Command::new("uvicorn")
        .args(["app.main:app", "--host", "127.0.0.1", "--port", "8091"])
        .env("NOEVIA_NATIVE_MODEL_MANAGER_CONFIRMED", "1")
        .env(
            "NOEVIA_NATIVE_MODEL_MANAGER_SECTIONS_CONFIRMED",
            if sections.is_some() { "1" } else { "0" },
        )
        .env(
            "NOEVIA_NATIVE_MODEL_MANAGER_HOST_CONFIRMED",
            if host.is_some() { "1" } else { "0" },
        )
        .kill_on_drop(true)
        .spawn()?;
    let _host_sampler = host
        .as_ref()
        .map(|history| noevia_model_manager::host::HostSampler::start(history.clone()));
    let state = Arc::new(StateData {
        sections,
        host,
        token,
        configured,
        docker: Arc::new(docker),
        redactor,
        permits: tokio::sync::Semaphore::new(4),
        legacy,
    });
    // Python retains its existing business startup for its unported routes; Rust does not
    // duplicate DB initialisation, seed/reap, hardware sampling or INI migrations.
    let serve = axum::serve(listener, router(state));
    tokio::select! {
        result=serve => {result?;},
        result=child.wait() => { let _=result?; return Err("Python-owned endpoint process exited".into()); },
        _=tokio::signal::ctrl_c()=>{},
        _=terminate()=>{},
    }
    child.start_kill()?;
    tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
    Ok(())
}
async fn terminate() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut signal) => {
            signal.recv().await;
        }
        Err(_) => {
            std::future::pending::<()>().await;
        }
    }
}

fn environment(key: &str, default: &str) -> Result<String, &'static str> {
    match std::env::var(key) {
        Ok(value) => Ok(value),
        Err(std::env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(_) => Err("environment setting must be UTF-8"),
    }
}
