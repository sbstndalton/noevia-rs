#![forbid(unsafe_code)]
mod child_lifecycle;
use noevia_model_manager::{
    docker::UnixDocker, legacy_client, router, validate_token, Redactor, StateData,
};
use std::{io::Write, os::unix::process::CommandExt, sync::Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--features") {
        if cfg!(feature = "native-logs") {
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            output.write_all(b"native-logs\n")?;
            if cfg!(feature = "native-host") {
                output.write_all(b"native-host\n")?;
            }
            if cfg!(feature = "native-sections") {
                output.write_all(b"native-sections\n")?;
            }
            if cfg!(feature = "native-prompts") {
                output.write_all(b"native-prompts\n")?;
            }
            if cfg!(feature = "native-diagnose") {
                output.write_all(b"native-diagnose\n")?;
            }
            output.flush()?;
            return Ok(());
        }
        return Err("native-logs feature is not enabled".into());
    }
    let mode = environment("MODEL_MANAGER_IMPL", "python")?;
    let host_mode = environment("MODEL_MANAGER_HOST_IMPL", "python")?;
    let sections_mode = environment("MODEL_MANAGER_SECTIONS_IMPL", "python")?;
    let prompts_mode = environment("MODEL_MANAGER_PROMPTS_IMPL", "python")?;
    let diagnose_mode = environment("MODEL_MANAGER_DIAGNOSE_IMPL", "python")?;
    if !matches!(diagnose_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_DIAGNOSE_IMPL must be python or rust".into());
    }
    if diagnose_mode == "rust" {
        if mode != "rust" || !cfg!(feature = "native-diagnose") {
            return Err(
                "native-diagnose requires rust model manager and compiled native-diagnose feature"
                    .into(),
            );
        }
        if environment("TZ", "")? != "UTC" {
            return Err("native-diagnose requires exact TZ=UTC".into());
        }
    }
    if !matches!(prompts_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_PROMPTS_IMPL must be python or rust".into());
    }
    if prompts_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-prompts")) {
        return Err(
            "native-prompts requires rust model manager and compiled native-prompts feature".into(),
        );
    }
    #[cfg(feature = "native-prompts")]
    let prompt_root = if prompts_mode == "rust" {
        Some(prompt_data_dir()?)
    } else {
        None
    };
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
            .env_remove("NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED")
            .env_remove("NOEVIA_NATIVE_MODEL_MANAGER_DIAGNOSE_CONFIRMED")
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
    let docker = Arc::new(UnixDocker::from_env()?);
    #[cfg(feature = "native-diagnose")]
    let diagnosis = if diagnose_mode == "rust" {
        Some(Arc::new(
            noevia_model_manager::diagnose_docker::Diagnosis::new(docker.clone())?,
        ))
    } else {
        None
    };
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
    let legacy_probe = reusable_probe(std::net::SocketAddrV4::new(
        std::net::Ipv4Addr::LOCALHOST,
        8091,
    ))?;
    drop(legacy_probe);
    #[cfg(feature = "native-prompts")]
    let prompts = if prompts_mode == "rust" {
        let data = prompt_root.as_deref().ok_or("native prompt root missing")?;
        Some(Arc::new(
            noevia_model_manager::prompts::Prompts::open(std::path::Path::new(&data)).await?,
        ))
    } else {
        None
    };
    let host = (host_mode == "rust").then(|| {
        Arc::new(tokio::sync::Mutex::new(
            noevia_model_manager::host::HostHistory::default(),
        ))
    });
    let mut child_command = legacy_command(
        tokio::process::Command::new("uvicorn"),
        &prompts_mode,
        sections.is_some(),
        host.is_some(),
        &diagnose_mode,
        #[cfg(feature = "native-prompts")]
        prompt_root.as_deref(),
    );
    let mut child = child_command.spawn()?;
    let _host_sampler = host
        .as_ref()
        .map(|history| noevia_model_manager::host::HostSampler::start(history.clone()));
    let state = Arc::new(StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis,
        #[cfg(feature = "native-prompts")]
        prompts,
        sections,
        host,
        token,
        configured,
        docker,
        redactor,
        permits: tokio::sync::Semaphore::new(4),
        legacy,
    });
    // Retain native stores and their lifetime writer locks until the child is reaped.
    let serve = axum::serve(listener, router(state.clone()));
    let outcome: Result<(), Box<dyn std::error::Error>> = tokio::select! {
        result=serve => result.map_err(Into::into),
        result=child.wait() => match result {Ok(_)=>Err("Python-owned endpoint process exited".into()),Err(e)=>Err(e.into())},
        _=tokio::signal::ctrl_c()=>Ok(()),
        _=terminate()=>Ok(()),
    };
    #[cfg(feature = "native-diagnose")]
    if let Some(diagnosis) = &state.diagnosis {
        diagnosis.shutdown().await;
    }
    child_lifecycle::reap(&mut child, state).await?;
    outcome
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

#[cfg(feature = "native-prompts")]
fn prompt_data_dir() -> Result<String, &'static str> {
    prompt_data_dir_from(std::env::vars_os())
}
#[cfg(feature = "native-prompts")]
fn prompt_data_dir_from(
    values: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Result<String, &'static str> {
    // Python folds settings names; reject aliases so both writers/readers name one file.
    let mut root = None;
    for (key, value) in values {
        if key
            .to_str()
            .is_some_and(|key| key != "DATA_DIR" && key.eq_ignore_ascii_case("DATA_DIR"))
        {
            return Err("native prompts require canonical DATA_DIR environment spelling");
        }
        if key == "DATA_DIR" {
            root = Some(
                value
                    .into_string()
                    .map_err(|_| "environment setting must be UTF-8")?,
            );
        }
    }
    let root = root.unwrap_or_else(|| "/data".to_owned());
    if root.len().saturating_add("/model_loader.db".len()) > 4096 {
        return Err("native prompt root exceeded limit");
    }
    Ok(root)
}

fn legacy_command(
    mut child_command: tokio::process::Command,
    prompts_mode: &str,
    sections: bool,
    host: bool,
    diagnose_mode: &str,
    #[cfg(feature = "native-prompts")] prompt_root: Option<&str>,
) -> tokio::process::Command {
    child_command
        .args(["app.main:app", "--host", "127.0.0.1", "--port", "8091"])
        .env("NOEVIA_NATIVE_MODEL_MANAGER_CONFIRMED", "1")
        .env(
            "NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED",
            if prompts_mode == "rust" { "1" } else { "0" },
        )
        .env(
            "NOEVIA_NATIVE_MODEL_MANAGER_SECTIONS_CONFIRMED",
            if sections { "1" } else { "0" },
        )
        .env(
            "NOEVIA_NATIVE_MODEL_MANAGER_HOST_CONFIRMED",
            if host { "1" } else { "0" },
        )
        .env(
            "NOEVIA_NATIVE_MODEL_MANAGER_DIAGNOSE_CONFIRMED",
            if diagnose_mode == "rust" { "1" } else { "0" },
        )
        .kill_on_drop(true);
    if diagnose_mode == "rust" {
        child_command.env("TZ", "UTC");
    }
    #[cfg(feature = "native-prompts")]
    if let Some(root) = prompt_root {
        child_command.env("DATA_DIR", root);
    }
    child_command
}

fn reusable_probe(address: std::net::SocketAddrV4) -> std::io::Result<std::net::TcpListener> {
    use rustix::net::{ipproto, socket, AddressFamily, SocketType};
    let fd = socket(AddressFamily::INET, SocketType::STREAM, Some(ipproto::TCP))?;
    rustix::net::sockopt::set_socket_reuseaddr(&fd, true)?;
    rustix::net::bind(&fd, &address)?;
    rustix::net::listen(&fd, 1)?;
    Ok(std::net::TcpListener::from(fd))
}

#[cfg(all(test, feature = "native-prompts"))]
mod prompt_configuration_tests {
    #[test]
    fn canonical_root_default_and_alias_policy_is_pure() {
        assert_eq!(super::prompt_data_dir_from([]), Ok("/data".into()));
        assert_eq!(
            super::prompt_data_dir_from([("DATA_DIR".into(), "/synthetic/private".into())]),
            Ok("/synthetic/private".into())
        );
        for alias in ["data_dir", "Data_Dir", "dAtA_dIr"] {
            assert!(
                super::prompt_data_dir_from([(alias.into(), "/synthetic/alias".into())]).is_err()
            );
            assert!(super::prompt_data_dir_from([
                ("DATA_DIR".into(), "/synthetic/canonical".into()),
                (alias.into(), "/synthetic/alias".into())
            ])
            .is_err());
        }
    }
}

#[cfg(test)]
mod diagnosis_command_tests {
    #[test]
    fn native_child_confirmation_semantics() -> Result<(), Box<dyn std::error::Error>> {
        for selected in [true, false] {
            let mode = if selected { "rust" } else { "python" };
            let mut seed = tokio::process::Command::new("uvicorn");
            seed.env("NOEVIA_NATIVE_MODEL_MANAGER_DIAGNOSE_CONFIRMED", "hostile");
            let command = super::legacy_command(
                seed,
                "python",
                false,
                false,
                mode,
                #[cfg(feature = "native-prompts")]
                None,
            );
            // Override a hostile inherited input without mutating process-global environment.
            let actual = command
                .as_std()
                .get_envs()
                .find(|(k, _)| *k == "NOEVIA_NATIVE_MODEL_MANAGER_DIAGNOSE_CONFIRMED")
                .and_then(|(_, v)| v)
                .ok_or("confirmation missing")?;
            let marker = actual.to_str().ok_or("confirmation encoding")?.to_owned();
            assert_eq!(marker, if selected { "1" } else { "0" });
            assert_eq!(command.as_std().get_program(), "uvicorn");
            assert_eq!(
                command.as_std().get_args().collect::<Vec<_>>(),
                ["app.main:app", "--host", "127.0.0.1", "--port", "8091"]
            );
            let rebuilt = super::legacy_command(
                tokio::process::Command::new("uvicorn"),
                "rust",
                true,
                true,
                mode,
                #[cfg(feature = "native-prompts")]
                Some("/synthetic/root"),
            );
            let keys: std::collections::BTreeMap<_, _> = rebuilt.as_std().get_envs().collect();
            assert_eq!(
                keys.get(std::ffi::OsStr::new(
                    "NOEVIA_NATIVE_MODEL_MANAGER_DIAGNOSE_CONFIRMED"
                ))
                .copied()
                .flatten(),
                Some(std::ffi::OsStr::new(if selected { "1" } else { "0" }))
            );
            assert_eq!(
                keys.get(std::ffi::OsStr::new("TZ")).copied().flatten(),
                selected.then_some(std::ffi::OsStr::new("UTC"))
            );
        }
        Ok(())
    }
}
