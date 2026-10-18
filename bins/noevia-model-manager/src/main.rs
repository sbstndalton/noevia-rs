#![forbid(unsafe_code)]
mod child_lifecycle;
mod model_list_cmd;
mod section_autoconfig_cmd;
use noevia_model_manager::{
    docker::UnixDocker, legacy_client, router, validate_token, Redactor, StateData,
};
use std::{io::Write, sync::Arc};

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
            if cfg!(feature = "native-settings") {
                output.write_all(b"native-settings\n")?;
            }
            if cfg!(feature = "native-model-updates") {
                output.write_all(b"native-model-updates\n")?;
            }
            if cfg!(feature = "native-model-detail") {
                output.write_all(b"native-model-detail\n")?;
            }
            if cfg!(feature = "native-download-targets") {
                output.write_all(b"native-download-targets\n")?;
            }
            if cfg!(feature = "native-badges") {
                output.write_all(b"native-badges\n")?;
            }
            if cfg!(feature = "native-bench-runs") {
                output.write_all(b"native-bench-runs\n")?;
            }
            if cfg!(feature = "native-section-writes") {
                output.write_all(b"native-section-writes\n")?;
            }
            if cfg!(feature = "native-settings-write") {
                output.write_all(b"native-settings-write\n")?;
            }
            if cfg!(feature = "native-backend-control") {
                output.write_all(b"native-backend-control\n")?;
            }
            if cfg!(feature = "native-benchmark-overview") {
                output.write_all(b"native-benchmark-overview\n")?;
            }
            if cfg!(feature = "native-check-updates") {
                output.write_all(b"native-check-updates\n")?;
            }
            if cfg!(feature = "native-benchmark-runner") {
                output.write_all(b"native-benchmark-runner\n")?;
            }
            if cfg!(feature = "native-downloads") {
                output.write_all(b"native-downloads\n")?;
            }
            if cfg!(feature = "native-model-delete") {
                output.write_all(b"native-model-delete\n")?;
            }
            if cfg!(feature = "native-draft-heads") {
                output.write_all(b"native-draft-heads\n")?;
            }
            if cfg!(feature = "native-backends") {
                output.write_all(b"native-backends\n")?;
            }
            if cfg!(feature = "native-search") {
                output.write_all(b"native-search\n")?;
            }
            if cfg!(feature = "native-model-list") {
                output.write_all(b"native-model-list\n")?;
            }
            if cfg!(feature = "native-section-autoconfig") {
                output.write_all(b"native-section-autoconfig\n")?;
            }
            if cfg!(feature = "native-diagnose") {
                output.write_all(b"native-diagnose\n")?;
            }
            output.flush()?;
            return Ok(());
        }
        return Err("native-logs feature is not enabled".into());
    }
    // noevia#1071/#2011: the native parent is permanent (there is no Python-only mode to exec),
    // and the retired MODEL_MANAGER_IMPL and MODEL_FILES_IMPL keys are not read at all.
    // The sub-selectors below still name the parent mode; it is always native now.
    let mode = "rust";
    let host_mode = environment("MODEL_MANAGER_HOST_IMPL", "python")?;
    let sections_mode = environment("MODEL_MANAGER_SECTIONS_IMPL", "python")?;
    let prompts_mode = environment("MODEL_MANAGER_PROMPTS_IMPL", "python")?;
    let diagnose_mode = environment("MODEL_MANAGER_DIAGNOSE_IMPL", "python")?;
    let settings_mode = environment("MODEL_MANAGER_SETTINGS_IMPL", "python")?;
    if !matches!(settings_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_SETTINGS_IMPL must be python or rust".into());
    }
    if settings_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-settings")) {
        return Err(
            "native-settings requires rust model manager and compiled native-settings feature"
                .into(),
        );
    }
    #[cfg(feature = "native-settings")]
    let settings_root = if settings_mode == "rust" {
        Some(data_dir()?)
    } else {
        None
    };
    let updates_mode = environment("MODEL_MANAGER_MODEL_UPDATES_IMPL", "python")?;
    if !matches!(updates_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_MODEL_UPDATES_IMPL must be python or rust".into());
    }
    if updates_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-model-updates")) {
        return Err(
            "native-model-updates requires rust model manager and compiled native-model-updates feature"
                .into(),
        );
    }
    #[cfg(feature = "native-model-updates")]
    let updates_roots = if updates_mode == "rust" {
        Some(model_updates_roots(std::env::vars_os().collect())?)
    } else {
        None
    };
    #[cfg_attr(not(feature = "native-model-detail"), allow(unused_mut))]
    let mut detail_mode = environment("MODEL_MANAGER_MODEL_DETAIL_IMPL", "python")?;
    if !matches!(detail_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_MODEL_DETAIL_IMPL must be python or rust".into());
    }
    if detail_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-model-detail")) {
        return Err(
            "native-model-detail requires rust model manager and compiled native-model-detail feature"
                .into(),
        );
    }
    // noevia#1889: never refuse to start over this read. The native route formats UTC and gives
    // the child TZ=UTC; a zone the operator chose (or roots it cannot use) keeps Python serving it.
    #[cfg(feature = "native-model-detail")]
    let detail_roots = if detail_mode == "rust" {
        match detail_fallback(&environment("TZ", "")?, std::env::vars_os().collect()) {
            Ok(roots) => Some(roots),
            Err(reason) => {
                eprintln!(
                    "native-model-detail: {reason}; GET /api/v1/models/detail stays with python"
                );
                detail_mode = "python".to_owned();
                None
            }
        }
    } else {
        None
    };
    #[cfg_attr(not(feature = "native-download-targets"), allow(unused_mut))]
    let mut targets_mode = environment("MODEL_MANAGER_DOWNLOAD_TARGETS_IMPL", "python")?;
    if !matches!(targets_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_DOWNLOAD_TARGETS_IMPL must be python or rust".into());
    }
    if targets_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-download-targets")) {
        return Err(
            "native-download-targets requires rust model manager and compiled native-download-targets feature"
                .into(),
        );
    }
    // Like #1889: roots the native read cannot name exactly as Python does keep Python serving it.
    #[cfg(feature = "native-download-targets")]
    let targets_roots = if targets_mode == "rust" {
        match download_targets_roots(std::env::vars_os().collect()) {
            Ok(roots) => Some(roots),
            Err(reason) => {
                eprintln!(
                    "native-download-targets: {reason}; GET /api/v1/download-targets stays with python"
                );
                targets_mode = "python".to_owned();
                None
            }
        }
    } else {
        None
    };
    let badges_mode = environment("MODEL_MANAGER_BADGES_IMPL", "python")?;
    if !matches!(badges_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_BADGES_IMPL must be python or rust".into());
    }
    if badges_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-badges")) {
        return Err(
            "native-badges requires rust model manager and compiled native-badges feature".into(),
        );
    }
    #[cfg(feature = "native-badges")]
    let badges_root = if badges_mode == "rust" {
        Some(data_dir()?)
    } else {
        None
    };
    let control_mode = environment("MODEL_MANAGER_BACKEND_CONTROL_IMPL", "python")?;
    if !matches!(control_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_BACKEND_CONTROL_IMPL must be python or rust".into());
    }
    if control_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-backend-control")) {
        return Err(
            "native-backend-control requires rust model manager and compiled native-backend-control feature"
                .into(),
        );
    }
    // LLAMA_DEFAULT_PORT as Python's settings read it (0 means none).
    #[cfg(feature = "native-backend-control")]
    let default_port: Option<u16> = if control_mode == "rust" {
        match environment("LLAMA_DEFAULT_PORT", "8080")?
            .trim()
            .parse::<u16>()
        {
            Ok(0) => None,
            Ok(p) => Some(p),
            Err(_) => return Err("LLAMA_DEFAULT_PORT must be a port number".into()),
        }
    } else {
        None
    };
    let settings_write_mode = environment("MODEL_MANAGER_SETTINGS_WRITE_IMPL", "python")?;
    if !matches!(settings_write_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_SETTINGS_WRITE_IMPL must be python or rust".into());
    }
    if settings_write_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-settings-write"))
    {
        return Err(
            "native-settings-write requires rust model manager and compiled native-settings-write feature"
                .into(),
        );
    }
    #[cfg(feature = "native-settings-write")]
    let settings_write_root = if settings_write_mode == "rust" {
        Some(data_dir()?)
    } else {
        None
    };
    #[cfg_attr(not(feature = "native-section-writes"), allow(unused_mut))]
    let mut writes_mode = environment("MODEL_MANAGER_SECTION_WRITES_IMPL", "python")?;
    if !matches!(writes_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_SECTION_WRITES_IMPL must be python or rust".into());
    }
    if writes_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-section-writes")) {
        return Err(
            "native-section-writes requires rust model manager and compiled native-section-writes feature"
                .into(),
        );
    }
    // Like #1889: settings the native writer cannot read exactly as Python does keep Python
    // writing models.ini (all five routes together), with one log line.
    #[cfg(feature = "native-section-writes")]
    let writes_roots = if writes_mode == "rust" {
        match section_writes_roots(&environment("TZ", "")?, std::env::vars_os().collect()) {
            Ok(roots) => Some(roots),
            Err(reason) => {
                eprintln!("native-section-writes: {reason}; models.ini writes stay with python");
                writes_mode = "python".to_owned();
                None
            }
        }
    } else {
        None
    };
    let check_mode = environment("MODEL_MANAGER_CHECK_UPDATES_IMPL", "python")?;
    if !matches!(check_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_CHECK_UPDATES_IMPL must be python or rust".into());
    }
    if check_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-check-updates")) {
        return Err(
            "native-check-updates requires rust model manager and compiled native-check-updates feature"
                .into(),
        );
    }
    #[cfg(feature = "native-check-updates")]
    let check_roots = if check_mode == "rust" {
        Some(model_updates_roots(std::env::vars_os().collect())?)
    } else {
        None
    };
    #[cfg_attr(not(feature = "native-benchmark-overview"), allow(unused_mut))]
    let mut overview_mode = environment("MODEL_MANAGER_BENCHMARK_OVERVIEW_IMPL", "python")?;
    if !matches!(overview_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_BENCHMARK_OVERVIEW_IMPL must be python or rust".into());
    }
    if overview_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-benchmark-overview")) {
        return Err(
            "native-benchmark-overview requires rust model manager and compiled native-benchmark-overview feature"
                .into(),
        );
    }
    #[cfg(feature = "native-benchmark-overview")]
    let overview_roots = if overview_mode == "rust" {
        match overview_ini(std::env::vars_os().collect()) {
            Ok(ini) => Some((data_dir()?, ini)),
            Err(reason) => {
                eprintln!("native-benchmark-overview: {reason}; GET /benchmark stays with python");
                overview_mode = "python".to_owned();
                None
            }
        }
    } else {
        None
    };
    // noevia#1318: the benchmark runner owns the one job slot (prompt runs, sweeps, the
    // startup cleanup of leftover sweep containers), so all four routes move together, and
    // only with the native overview, which then reads the job from it.
    let runner_mode = environment("MODEL_MANAGER_BENCHMARK_RUNNER_IMPL", "python")?;
    if !matches!(runner_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_BENCHMARK_RUNNER_IMPL must be python or rust".into());
    }
    if runner_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-benchmark-runner")) {
        return Err(
            "native-benchmark-runner requires rust model manager and compiled native-benchmark-runner feature"
                .into(),
        );
    }
    #[cfg_attr(not(feature = "native-benchmark-runner"), allow(unused_mut))]
    let mut runner_mode = runner_mode;
    if runner_mode == "rust" && overview_mode != "rust" {
        eprintln!(
            "native-benchmark-runner: needs MODEL_MANAGER_BENCHMARK_OVERVIEW_IMPL=rust; the benchmark runner stays with python"
        );
        runner_mode = "python".to_owned();
    }
    // noevia#1318: the download job table. The five routes share one in-memory table, so they
    // move together; a start is still planned by the child.
    let downloads_mode = environment("MODEL_MANAGER_DOWNLOADS_IMPL", "python")?;
    if !matches!(downloads_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_DOWNLOADS_IMPL must be python or rust".into());
    }
    if downloads_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-downloads")) {
        return Err(
            "native-downloads requires rust model manager and compiled native-downloads feature"
                .into(),
        );
    }
    #[cfg_attr(not(feature = "native-downloads"), allow(unused_mut))]
    let mut downloads_mode = downloads_mode;
    #[cfg(feature = "native-downloads")]
    let downloads_roots = if downloads_mode == "rust" {
        match downloads_roots(std::env::vars_os().collect()) {
            Ok(roots) => Some(roots),
            Err(reason) => {
                eprintln!("native-downloads: {reason}; downloads stay with python");
                downloads_mode = "python".to_owned();
                None
            }
        }
    } else {
        None
    };
    // noevia#1318: model deletion (the whole POST /api/v1/models/delete).
    let delete_mode = environment("MODEL_MANAGER_MODEL_DELETE_IMPL", "python")?;
    if !matches!(delete_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_MODEL_DELETE_IMPL must be python or rust".into());
    }
    if delete_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-model-delete")) {
        return Err(
            "native-model-delete requires rust model manager and compiled native-model-delete feature"
                .into(),
        );
    }
    #[cfg_attr(not(feature = "native-model-delete"), allow(unused_mut))]
    let mut delete_mode = delete_mode;
    // noevia#1966: the delete must see every queued and running download, so it is native only
    // while this process also owns the download table (else both stay with Python, whose delete
    // checks its own jobs).
    #[cfg(feature = "native-model-delete")]
    if delete_mode == "rust" && downloads_mode != "rust" {
        eprintln!(
            "native-model-delete: needs MODEL_MANAGER_DOWNLOADS_IMPL=rust (the delete checks the download table); model deletion stays with python"
        );
        delete_mode = "python".to_owned();
    }
    #[cfg(feature = "native-model-delete")]
    let delete_roots = if delete_mode == "rust" {
        match delete_roots(std::env::vars_os().collect()) {
            Ok(roots) => Some(roots),
            Err(reason) => {
                eprintln!("native-model-delete: {reason}; model deletion stays with python");
                delete_mode = "python".to_owned();
                None
            }
        }
    } else {
        None
    };
    // noevia#1318 slice 12: GET /api/v1/sections/{name}/draft-heads.
    let heads_mode = environment("MODEL_MANAGER_DRAFT_HEADS_IMPL", "python")?;
    if !matches!(heads_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_DRAFT_HEADS_IMPL must be python or rust".into());
    }
    if heads_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-draft-heads")) {
        return Err(
            "native-draft-heads requires rust model manager and compiled native-draft-heads feature"
                .into(),
        );
    }
    #[cfg_attr(not(feature = "native-draft-heads"), allow(unused_mut))]
    let mut heads_mode = heads_mode;
    #[cfg(feature = "native-draft-heads")]
    let heads_roots = if heads_mode == "rust" {
        match model_updates_roots(std::env::vars_os().collect()) {
            Ok(roots) => Some(roots),
            Err(reason) => {
                eprintln!("native-draft-heads: {reason}; GET draft-heads stays with python");
                heads_mode = "python".to_owned();
                None
            }
        }
    } else {
        None
    };
    // noevia#1318 slice 13: the backend sampler (Docker discovery, container stats, sysfs, GPU_VRAM)
    // and GET /api/v1/backends. Like the other switches, startup is refused for a value other than
    // python or rust, and for `rust` without the compiled native-backends feature. Only once those hold does anything this side cannot own exactly
    // (`backends_fallback`) keep the sampler and the route with Python, with a notice, instead of
    // refusing.
    let backends_mode = environment("MODEL_MANAGER_BACKENDS_IMPL", "python")?;
    if !matches!(backends_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_BACKENDS_IMPL must be python or rust".into());
    }
    if backends_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-backends")) {
        return Err(
            "native-backends requires rust model manager and compiled native-backends feature"
                .into(),
        );
    }
    #[cfg_attr(not(feature = "native-backends"), allow(unused_mut))]
    let mut backends_mode = backends_mode;
    #[cfg(feature = "native-backends")]
    let backends_settings = if backends_mode == "rust" {
        match backends_fallback(
            &control_mode,
            &environment("TZ", "")?,
            std::env::vars_os().collect(),
        ) {
            Ok(settings) => Some(settings),
            Err(reason) => {
                eprintln!(
                    "native-backends: {reason}; the backend sampler and GET /api/v1/backends stay with python"
                );
                backends_mode = "python".to_owned();
                None
            }
        }
    } else {
        None
    };
    // noevia#1318 slice 14: GET /api/v1/search and GET /api/v1/search/repo.
    let search_mode = environment("MODEL_MANAGER_SEARCH_IMPL", "python")?;
    if !matches!(search_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_SEARCH_IMPL must be python or rust".into());
    }
    if search_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-search")) {
        return Err(
            "native-search requires rust model manager and compiled native-search feature".into(),
        );
    }
    #[cfg_attr(not(feature = "native-search"), allow(unused_mut))]
    let mut search_mode = search_mode;
    #[cfg(feature = "native-search")]
    let search_roots = if search_mode == "rust" {
        match model_updates_roots(std::env::vars_os().collect()) {
            Ok(roots) => Some(roots),
            Err(reason) => {
                eprintln!("native-search: {reason}; GET search stays with python");
                search_mode = "python".to_owned();
                None
            }
        }
    } else {
        None
    };
    let bench_mode = environment("MODEL_MANAGER_BENCH_RUNS_IMPL", "python")?;
    if !matches!(bench_mode.as_str(), "python" | "rust") {
        return Err("MODEL_MANAGER_BENCH_RUNS_IMPL must be python or rust".into());
    }
    if bench_mode == "rust" && (mode != "rust" || !cfg!(feature = "native-bench-runs")) {
        return Err(
            "native-bench-runs requires rust model manager and compiled native-bench-runs feature"
                .into(),
        );
    }
    #[cfg(feature = "native-bench-runs")]
    let bench_root = if bench_mode == "rust" {
        Some(data_dir()?)
    } else {
        None
    };
    // noevia#1318: GET /api/v1/models and GET /api/v1/overview.
    let model_list = model_list_cmd::select(&std::env::vars_os().collect::<Vec<_>>())?;
    // noevia#1318: GET /api/v1/sections/{name}/autoconfig.
    let section_autoconfig =
        section_autoconfig_cmd::select(&std::env::vars_os().collect::<Vec<_>>())?;
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
    #[cfg(feature = "native-benchmark-runner")]
    let bench_runner = match (runner_mode.as_str(), overview_roots.as_ref()) {
        ("rust", Some((data, ini))) => {
            let runner = noevia_model_manager::bench_runner::production(
                noevia_model_manager::bench_runner::Production {
                    data: data.into(),
                    ini: ini.into(),
                    models_dir: environment("MODELS_DIR", "/models")?,
                    socket: docker.socket.clone(),
                    docker: docker.clone(),
                    configured: configured.clone(),
                    legacy: legacy_client()?,
                    legacy_base: noevia_model_manager::LEGACY_BASE.to_owned(),
                    token: token.clone(),
                },
            )?;
            // bench.reap_orphans at startup: leftover sweep containers, by exact label only.
            match tokio::time::timeout(std::time::Duration::from_secs(60), runner.reap_orphans())
                .await
            {
                Ok((reaped, stuck)) if reaped + stuck > 0 => {
                    eprintln!("benchmark orphan check: {reaped} removed, {stuck} still stopping");
                }
                Ok(_) => {}
                Err(_) => eprintln!("benchmark orphan check timed out; startup continues"),
            }
            Some(runner)
        }
        _ => None,
    };
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
    // #1668: the settings-driven embedding service. The preset is rendered (and the env seed
    // read, once) before the listener binds, so a compose `depends_on: service_healthy` start
    // never shows the embed router a missing preset.
    let embed_container = environment("EMBED_CONTAINER", "")?;
    noevia_model_manager::require_embed_token(&embed_container, &token)?;
    let embedding = if embed_container.is_empty() {
        None
    } else {
        let data = environment("DATA_DIR", "/data")?;
        let service = noevia_model_manager::embedding::EmbeddingService::new(
            embed_container.clone(),
            environment("MODELS_DIR", "/models")?.into(),
            environment("EMBED_MODELS_MOUNT", "/models")?,
            environment("EMBED_PRESET_PATH", "/config/embedding.ini")?.into(),
            std::path::Path::new(&data).join("embedding-service.json"),
            Arc::new(noevia_model_manager::docker::ScopedControl::new(
                docker.socket.clone(),
                &embed_container,
            )?),
            Arc::new(noevia_model_manager::embedding::HttpProbe::new(
                &environment("EMBED_URL", "http://embed:8080")?,
            )?),
        )
        // #1726: image presets for multimodal RAG; off unless the same switch web reads is on.
        .with_vision(matches!(
            environment("NOEVIA_MULTIMODAL_RAG", "")?
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "on" | "yes"
        ));
        // A bad state file or a read-only /config leaves the service in an error state (deletes
        // refused, status reports it); it must not crash-loop the whole model manager.
        if let Err(error) = service.seed(
            &environment("EMBEDDING_MODEL", "")?,
            &environment("EMBED_MODEL_FILE", "")?,
        ) {
            eprintln!("embedding service: {error}");
        }
        let service = Arc::new(service);
        service.start_warm();
        // #1717: re-warm after an embed restart noevia did not make.
        drop(service.watch());
        Some(service)
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
    #[cfg(feature = "native-settings")]
    let settings = settings_root.as_deref().map(|root| {
        Arc::new(noevia_model_manager::settings::Settings::new(
            std::path::Path::new(root),
        ))
    });
    #[cfg(feature = "native-model-updates")]
    let model_updates = updates_roots.as_ref().map(|roots| {
        Arc::new(noevia_model_manager::model_updates::ModelUpdates::new(
            std::path::Path::new(&roots.models),
            std::path::Path::new(&roots.ini),
            std::path::Path::new(&roots.data),
        ))
    });
    #[cfg(feature = "native-model-detail")]
    let model_detail = detail_roots.as_ref().map(|roots| {
        Arc::new(noevia_model_manager::model_detail::ModelDetail::new(
            std::path::Path::new(&roots.models),
            std::path::Path::new(&roots.ini),
        ))
    });
    #[cfg(feature = "native-download-targets")]
    let download_targets = targets_roots.as_ref().map(|roots| {
        Arc::new(
            noevia_model_manager::download_targets::DownloadTargets::new(
                &roots.models,
                &roots.declared,
                std::path::Path::new(noevia_model_manager::download_targets::MOUNTINFO),
            ),
        )
    });
    #[cfg(feature = "native-badges")]
    let badges = badges_root.as_deref().map(|root| {
        Arc::new(noevia_model_manager::badges::Badges::new(
            std::path::Path::new(root),
        ))
    });
    #[cfg(feature = "native-backend-control")]
    let backends = if control_mode == "rust" {
        Some(Arc::new(noevia_model_manager::backends::Backends::new(
            default_port,
        )?))
    } else {
        None
    };
    #[cfg(feature = "native-check-updates")]
    let check_updates = match check_roots.as_ref() {
        Some(roots) => Some(Arc::new(
            noevia_model_manager::check_updates::CheckUpdates::new(
                std::path::Path::new(&roots.models),
                std::path::Path::new(&roots.ini),
                std::path::Path::new(&roots.data),
            )?,
        )),
        None => None,
    };
    #[cfg(feature = "native-benchmark-overview")]
    let benchmark_overview = overview_roots.as_ref().map(|(data, ini)| {
        Arc::new(
            noevia_model_manager::benchmark_overview::BenchmarkOverview::new(
                std::path::Path::new(data),
                std::path::Path::new(ini),
            ),
        )
    });
    #[cfg(feature = "native-settings-write")]
    let settings_write = settings_write_root.as_deref().map(|root| {
        Arc::new(noevia_model_manager::settings_write::SettingsWrite::new(
            std::path::Path::new(root),
        ))
    });
    #[cfg(feature = "native-section-writes")]
    let section_writes = writes_roots.as_ref().map(|roots| {
        Arc::new(noevia_model_manager::section_writes::SectionWrites::new(
            std::path::Path::new(&roots.ini),
            roots.cache,
        ))
    });
    #[cfg(feature = "native-bench-runs")]
    let bench_runs = bench_root.as_deref().map(|root| {
        Arc::new(noevia_model_manager::bench_runs::BenchRuns::new(
            std::path::Path::new(root),
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
    settings_child(
        &mut child_command,
        &settings_mode,
        #[cfg(feature = "native-settings")]
        settings_root.as_deref(),
    );
    model_updates_child(
        &mut child_command,
        &updates_mode,
        #[cfg(feature = "native-model-updates")]
        updates_roots.as_ref(),
    );
    model_detail_child(
        &mut child_command,
        &detail_mode,
        #[cfg(feature = "native-model-detail")]
        detail_roots.as_ref(),
    );
    download_targets_child(
        &mut child_command,
        &targets_mode,
        #[cfg(feature = "native-download-targets")]
        targets_roots.as_ref(),
    );
    model_updates_child_as(
        &mut child_command,
        "NOEVIA_NATIVE_MODEL_MANAGER_CHECK_UPDATES_CONFIRMED",
        &check_mode,
        #[cfg(feature = "native-check-updates")]
        check_roots.as_ref(),
    );
    benchmark_overview_child(
        &mut child_command,
        &overview_mode,
        #[cfg(feature = "native-benchmark-overview")]
        overview_roots.as_ref(),
    );
    benchmark_runner_child(&mut child_command, &runner_mode);
    downloads_child(
        &mut child_command,
        &downloads_mode,
        #[cfg(feature = "native-downloads")]
        downloads_roots.as_ref(),
    );
    model_delete_child(
        &mut child_command,
        &delete_mode,
        #[cfg(feature = "native-model-delete")]
        delete_roots.as_ref(),
    );
    draft_heads_child(
        &mut child_command,
        &heads_mode,
        #[cfg(feature = "native-draft-heads")]
        heads_roots.as_ref(),
    );
    backends_child(&mut child_command, &backends_mode);
    search_child(
        &mut child_command,
        &search_mode,
        #[cfg(feature = "native-search")]
        search_roots.as_ref(),
    );
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_BACKEND_CONTROL_CONFIRMED",
        if control_mode == "rust" { "1" } else { "0" },
    );
    settings_write_child(
        &mut child_command,
        &settings_write_mode,
        #[cfg(feature = "native-settings-write")]
        settings_write_root.as_deref(),
    );
    section_writes_child(
        &mut child_command,
        &writes_mode,
        #[cfg(feature = "native-section-writes")]
        writes_roots.as_ref(),
    );
    bench_runs_child(
        &mut child_command,
        &bench_mode,
        #[cfg(feature = "native-bench-runs")]
        bench_root.as_deref(),
    );
    badges_child(
        &mut child_command,
        &badges_mode,
        #[cfg(feature = "native-badges")]
        badges_root.as_deref(),
    );
    model_list_cmd::child(&mut child_command, &model_list);
    section_autoconfig_cmd::child(&mut child_command, &section_autoconfig);
    // #1668: the native front refuses deletes of the embedding service's models from its router
    // state, so the legacy holder scan must not also pin env names that point at that service
    // forever. It still pins names on any other server (#1638), so it gets the service's address.
    let embed_url = environment("EMBED_URL", "http://embed:8080")?;
    for (key, value) in noevia_model_manager::embed_guard_env(
        embedding
            .as_ref()
            .map(|s| (s.container.as_str(), embed_url.as_str())),
    ) {
        match value {
            Some(value) => child_command.env(key, value),
            None => child_command.env_remove(key),
        };
    }
    // #1747: idle chat models unload themselves (llama.cpp `sleep-idle-seconds` in the chat
    // preset's `[*]` section). A bad value stops startup; a preset that cannot be rewritten does not.
    // noevia#1863: an admin's choice, kept in DATA_DIR, wins over the env seed.
    let idle_unload = Arc::new(noevia_model_manager::idle_unload::Reconciler::with_store(
        environment("MODELS_INI_PATH", "/models/models.ini")?.into(),
        noevia_model_manager::idle_unload::parse_env(Some(&environment(
            noevia_model_manager::idle_unload::ENV,
            "",
        )?))?,
        Some(
            std::path::Path::new(&environment("DATA_DIR", "/data")?)
                .join(noevia_model_manager::idle_unload::STORE_FILE),
        ),
    ));
    if let Err(error) = idle_unload.reconcile().await {
        eprintln!("idle unload: {error}");
    }
    // noevia#2010: this writer runs the startup cache-ram migration, once, after the reconcile and
    // before the child starts: the old order, since the child that used to run it starts after the
    // reconcile. The child is never respawned (its exit ends this process), so once per start is
    // once per child start. A failure is logged, never fatal.
    #[cfg(feature = "native-section-writes")]
    if let (Some(writes), Some(true)) = (&section_writes, writes_roots.as_ref().map(|r| r.migrate))
    {
        match writes.clone().migrate_cache_ram_at_startup().await {
            Ok(changed) if changed.is_empty() => {}
            Ok(changed) => eprintln!(
                "cache-ram migration: set an explicit bounded cache-ram in {} section(s): {}",
                changed.len(),
                changed.join(", ")
            ),
            Err(reason) => eprintln!("cache-ram migration skipped: {reason}"),
        }
    }
    #[cfg(feature = "native-downloads")]
    let downloads = match downloads_roots.as_ref() {
        Some(roots) => Some(noevia_model_manager::downloads::Downloads::new(
            noevia_model_manager::downloads::Config::production(
                roots.models.clone(),
                &roots.data,
                roots.concurrent,
            ),
        )?),
        None => None,
    };
    #[cfg(feature = "native-model-delete")]
    let model_delete = delete_roots
        .as_ref()
        .zip(downloads.as_ref())
        .map(|(roots, table)| {
            Arc::new(
                noevia_model_manager::model_delete::ModelDelete::new(
                    noevia_model_manager::model_delete::Config {
                        models: roots.models.clone(),
                        ini: roots.ini.clone().into(),
                        host_path: roots.host_path.clone(),
                        targets: roots.targets.clone(),
                        mountinfo: "/proc/self/mountinfo".into(),
                        hostname: roots.hostname.clone(),
                        embed: embedding
                            .as_ref()
                            .map(|s| (s.container.clone(), embed_url.clone())),
                    },
                    docker.clone(),
                )
                .with_downloads(table.clone()),
            )
        });
    #[cfg(feature = "native-draft-heads")]
    let draft_heads = match heads_roots.as_ref() {
        Some(roots) => Some(Arc::new(
            noevia_model_manager::draft_heads::DraftHeads::new(
                std::path::Path::new(&roots.models),
                std::path::Path::new(&roots.ini),
                std::path::Path::new(&roots.data),
            )?,
        )),
        None => None,
    };
    #[cfg(feature = "native-search")]
    let search = match search_roots.as_ref() {
        Some(roots) => Some(Arc::new(noevia_model_manager::search::Search::new(
            std::path::Path::new(&roots.data),
            noevia_model_manager::LEGACY_BASE,
            &token,
        )?)),
        None => None,
    };
    #[cfg(feature = "native-backends")]
    let ram_reserve_gb = ram_reserve(std::env::vars_os().collect());
    #[cfg(feature = "native-backends")]
    let backend_stats = match backends_settings {
        Some(gpu_vram) => Some(Arc::new(
            noevia_model_manager::backend_stats::BackendStats::new(
                noevia_model_manager::backend_stats::Config {
                    configured: configured.clone(),
                    gpu_vram,
                    default_port,
                    drm_root: noevia_model_manager::backend_stats::DRM_ROOT.into(),
                    legacy_base: noevia_model_manager::LEGACY_BASE.to_owned(),
                    token: token.clone(),
                    meminfo: noevia_model_manager::backend_stats::fit::MEMINFO.into(),
                    ram_reserve_gb,
                },
                docker.clone(),
            )?,
        )),
        None => None,
    };
    // noevia#1318 step 1c: the search runtime is native while this process samples the backends.
    #[cfg(all(feature = "native-search", feature = "native-backends"))]
    if let (Some(search), Some(stats), Some(roots)) = (
        search.as_ref(),
        backend_stats.as_ref(),
        search_roots.as_ref(),
    ) {
        let path = environment(
            "SEARCH_TRUSTED_PATH",
            noevia_model_manager::search::live::TRUSTED_PATH,
        )?;
        match noevia_model_manager::search::live::load_trusted(std::path::Path::new(&path)) {
            Ok(trusted) => search.set_live(noevia_model_manager::search::live::Source {
                stats: stats.clone(),
                models: roots.models.clone().into(),
                ini: roots.ini.clone().into(),
                trusted,
            }),
            Err(reason) => {
                eprintln!("native-search: {reason}; the search runtime stays with the python child")
            }
        }
    }
    let mut child = child_command.spawn()?;
    #[cfg(feature = "native-backends")]
    let _backend_sampler = backend_stats.as_ref().map(|stats| stats.start());
    let _host_sampler = host
        .as_ref()
        .map(|history| noevia_model_manager::host::HostSampler::start(history.clone()));
    let state = Arc::new(StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis,
        #[cfg(feature = "native-prompts")]
        prompts,
        #[cfg(feature = "native-settings")]
        settings,
        #[cfg(feature = "native-model-updates")]
        model_updates,
        #[cfg(feature = "native-model-detail")]
        model_detail,
        #[cfg(feature = "native-model-list")]
        model_list: model_list_cmd::state(&model_list),
        #[cfg(feature = "native-download-targets")]
        download_targets,
        #[cfg(feature = "native-badges")]
        badges,
        #[cfg(feature = "native-bench-runs")]
        bench_runs,
        #[cfg(feature = "native-section-writes")]
        section_writes,
        #[cfg(feature = "native-settings-write")]
        settings_write,
        #[cfg(feature = "native-backend-control")]
        backends,
        #[cfg(feature = "native-benchmark-overview")]
        benchmark_overview,
        #[cfg(feature = "native-check-updates")]
        check_updates,
        #[cfg(feature = "native-downloads")]
        downloads,
        #[cfg(feature = "native-model-delete")]
        model_delete,
        #[cfg(feature = "native-draft-heads")]
        draft_heads,
        #[cfg(feature = "native-search")]
        search,
        #[cfg(feature = "native-backends")]
        backend_stats,
        #[cfg(feature = "native-benchmark-runner")]
        bench_runner,
        sections,
        embedding,
        host,
        token,
        configured,
        docker,
        redactor,
        permits: tokio::sync::Semaphore::new(4),
        legacy,
        legacy_base: noevia_model_manager::LEGACY_BASE.to_owned(),
        idle_unload: Some(idle_unload),
        #[cfg(feature = "native-section-autoconfig")]
        section_autoconfig: section_autoconfig_cmd::state(&section_autoconfig),
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
    data_dir()
}
#[cfg(any(
    feature = "native-prompts",
    feature = "native-settings",
    feature = "native-badges",
    feature = "native-bench-runs",
    feature = "native-settings-write",
    feature = "native-benchmark-overview"
))]
fn data_dir() -> Result<String, &'static str> {
    prompt_data_dir_from(std::env::vars_os())
}
#[cfg(any(
    feature = "native-prompts",
    feature = "native-settings",
    feature = "native-model-updates"
))]
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
            return Err(
                "native prompts and settings require canonical DATA_DIR environment spelling",
            );
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
        return Err("native DATA_DIR root exceeded limit");
    }
    Ok(root)
}

/// noevia#1318: the three roots `GET /api/v1/models/updates` reads, named exactly as the Python
/// child reads them (its settings fold names, so case-folded aliases are refused).
#[cfg(feature = "native-model-updates")]
#[derive(Debug, PartialEq, Eq)]
struct UpdatesRoots {
    models: String,
    ini: String,
    data: String,
}

#[cfg(feature = "native-model-updates")]
fn model_updates_roots(
    values: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Result<UpdatesRoots, &'static str> {
    let canonical = |name: &str, default: &str| -> Result<String, &'static str> {
        let mut found = None;
        for (key, value) in &values {
            if key
                .to_str()
                .is_some_and(|key| key != name && key.eq_ignore_ascii_case(name))
            {
                return Err("native model updates require canonical MODELS_DIR, MODELS_INI_PATH and DATA_DIR environment spelling");
            }
            if key == name {
                found = Some(
                    value
                        .to_str()
                        .ok_or("environment setting must be UTF-8")?
                        .to_owned(),
                );
            }
        }
        let found = found.unwrap_or_else(|| default.to_owned());
        if found.is_empty() || found.len() > 4096 {
            return Err("native model updates root exceeded limit");
        }
        Ok(found)
    };
    Ok(UpdatesRoots {
        models: canonical("MODELS_DIR", "/models")?,
        ini: canonical("MODELS_INI_PATH", "/models/models.ini")?,
        data: prompt_data_dir_from(values.clone())?,
    })
}

/// noevia#1318: the child refuses `GET /api/v1/models/updates` only when the native reader owns
/// it, and is handed the same three roots the native reader opens.
fn model_updates_child(
    child_command: &mut tokio::process::Command,
    updates_mode: &str,
    #[cfg(feature = "native-model-updates")] roots: Option<&UpdatesRoots>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_MODEL_UPDATES_CONFIRMED",
        if updates_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-model-updates")]
    if let Some(roots) = roots {
        child_command
            .env("MODELS_DIR", &roots.models)
            .env("MODELS_INI_PATH", &roots.ini)
            .env("DATA_DIR", &roots.data);
    }
}

/// noevia#1318: the child refuses `POST /api/v1/models/check-updates` only when the native writer
/// owns it, and is handed the same three roots (the status map reads the same models folder).
fn model_updates_child_as(
    child_command: &mut tokio::process::Command,
    confirmation: &str,
    mode: &str,
    #[cfg(feature = "native-check-updates")] roots: Option<&UpdatesRoots>,
) {
    child_command.env(confirmation, if mode == "rust" { "1" } else { "0" });
    #[cfg(feature = "native-check-updates")]
    if let Some(roots) = roots {
        child_command
            .env("MODELS_DIR", &roots.models)
            .env("MODELS_INI_PATH", &roots.ini)
            .env("DATA_DIR", &roots.data);
    }
}

#[cfg(all(test, feature = "native-check-updates"))]
mod check_updates_command_tests {
    #[test]
    fn confirmation_overrides_inherited_value() {
        let roots = super::UpdatesRoots {
            models: "/synthetic/models".into(),
            ini: "/synthetic/models.ini".into(),
            data: "/synthetic/data".into(),
        };
        for (mode, roots, expected) in [("rust", Some(&roots), "1"), ("python", None, "0")] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_CHECK_UPDATES_CONFIRMED",
                "hostile",
            );
            super::model_updates_child_as(
                &mut command,
                "NOEVIA_NATIVE_MODEL_MANAGER_CHECK_UPDATES_CONFIRMED",
                mode,
                roots,
            );
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_CHECK_UPDATES_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(
                get("MODELS_DIR"),
                roots.map(|r| std::ffi::OsStr::new(r.models.as_str()))
            );
        }
    }
}

/// noevia#1318: the roots `GET /api/v1/models/detail` reads. Python joins `MODELS_DIR` with each
/// file for `path`, so the configured folder must already be in pathlib's normal form.
#[cfg(feature = "native-model-detail")]
fn model_detail_roots(
    values: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Result<UpdatesRoots, &'static str> {
    let roots = model_updates_roots(values)
        .map_err(|_| "native model detail requires canonical MODELS_DIR, MODELS_INI_PATH and DATA_DIR environment spelling within limits")?;
    let models = roots.models.as_str();
    let normal = models.starts_with('/')
        && (models == "/"
            || (!models.ends_with('/')
                && models
                    .split('/')
                    .skip(1)
                    .all(|segment| !matches!(segment, "" | "." | ".."))));
    if !normal {
        return Err("native model detail requires an absolute, normalised MODELS_DIR");
    }
    Ok(roots)
}

/// noevia#1889: the roots for the native detail read, or why it falls back to python. `TZ` unset
/// or `UTC` is accepted (the child is given UTC either way); any other zone keeps Python's
/// local-time answers unchanged.
#[cfg(feature = "native-model-detail")]
fn detail_fallback(
    tz: &str,
    values: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Result<UpdatesRoots, &'static str> {
    if !(tz.is_empty() || tz == "UTC") {
        return Err("TZ is set to a zone other than UTC");
    }
    model_detail_roots(values)
}

/// noevia#1318: the child refuses `GET /api/v1/models/detail` and serves the private runtime call
/// only when the native reader owns the route; it then runs in UTC and reads the same roots.
fn model_detail_child(
    child_command: &mut tokio::process::Command,
    detail_mode: &str,
    #[cfg(feature = "native-model-detail")] roots: Option<&UpdatesRoots>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DETAIL_CONFIRMED",
        if detail_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-model-detail")]
    if let Some(roots) = roots {
        child_command
            .env("TZ", "UTC")
            .env("MODELS_DIR", &roots.models)
            .env("MODELS_INI_PATH", &roots.ini);
    }
}

/// noevia#1318: what `GET /api/v1/download-targets` reads, named exactly as the Python child's
/// settings read them (they fold names, so case-folded aliases are refused).
#[cfg(feature = "native-download-targets")]
#[derive(Debug, PartialEq, Eq)]
struct TargetsRoots {
    models: String,
    declared: String,
}

#[cfg(feature = "native-download-targets")]
fn download_targets_roots(
    values: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Result<TargetsRoots, &'static str> {
    let canonical = |name: &str, default: &str| -> Result<String, &'static str> {
        let mut found = None;
        for (key, value) in &values {
            if key
                .to_str()
                .is_some_and(|key| key != name && key.eq_ignore_ascii_case(name))
            {
                return Err(
                    "MODELS_DIR and MODEL_DOWNLOAD_TARGETS need canonical environment spelling",
                );
            }
            if key == name {
                found = Some(
                    value
                        .to_str()
                        .ok_or("environment setting must be UTF-8")?
                        .to_owned(),
                );
            }
        }
        let found = found.unwrap_or_else(|| default.to_owned());
        if found.len() > 65536 {
            return Err("download target settings exceeded limit");
        }
        Ok(found)
    };
    let models = canonical("MODELS_DIR", "/models")?;
    if models.len() > 4096 || !noevia_model_manager::download_targets::normal_root(&models) {
        return Err("MODELS_DIR is not an absolute, normalised path");
    }
    let declared = canonical("MODEL_DOWNLOAD_TARGETS", "")?;
    if noevia_model_manager::download_targets::declared_names(&declared).is_none() {
        return Err("MODEL_DOWNLOAD_TARGETS names too many folders");
    }
    Ok(TargetsRoots { models, declared })
}

/// noevia#1318: the child refuses `GET /api/v1/download-targets` only when the native reader owns
/// it, and validates `POST /api/v1/downloads` targets against the same two settings.
fn download_targets_child(
    child_command: &mut tokio::process::Command,
    targets_mode: &str,
    #[cfg(feature = "native-download-targets")] roots: Option<&TargetsRoots>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOAD_TARGETS_CONFIRMED",
        if targets_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-download-targets")]
    if let Some(roots) = roots {
        child_command
            .env("MODELS_DIR", &roots.models)
            .env("MODEL_DOWNLOAD_TARGETS", &roots.declared);
    }
}

#[cfg(all(test, feature = "native-download-targets"))]
#[allow(clippy::unwrap_used)]
mod download_targets_command_tests {
    fn roots(pairs: &[(&str, &str)]) -> Result<super::TargetsRoots, &'static str> {
        super::download_targets_roots(
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        )
    }

    #[test]
    fn roots_default_canonical_and_fallback_reasons() {
        assert_eq!(
            roots(&[]),
            Ok(super::TargetsRoots {
                models: "/models".into(),
                declared: String::new()
            })
        );
        assert_eq!(
            roots(&[("MODELS_DIR", "/srv/m"), ("MODEL_DOWNLOAD_TARGETS", "a, b")])
                .unwrap()
                .declared,
            "a, b"
        );
        for bad in [
            "models", "/models/", "//models", "/a/./b", "/a/../b", "/a//b", "",
        ] {
            assert!(roots(&[("MODELS_DIR", bad)]).is_err(), "{bad:?}");
        }
        for alias in [("models_dir", "/m"), ("Model_Download_Targets", "x")] {
            assert!(roots(&[alias]).is_err());
        }
        let many = (0..1025)
            .map(|i| format!("n{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(roots(&[("MODEL_DOWNLOAD_TARGETS", &many)]).is_err());
    }

    #[test]
    fn child_confirmation_overrides_inherited_value_and_carries_roots() {
        for (mode, expected) in [("rust", "1"), ("python", "0")] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOAD_TARGETS_CONFIRMED",
                "hostile",
            );
            let roots = super::TargetsRoots {
                models: "/synthetic/models".into(),
                declared: "extra".into(),
            };
            super::download_targets_child(&mut command, mode, (mode == "rust").then_some(&roots));
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOAD_TARGETS_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(get("MODEL_DOWNLOAD_TARGETS").is_some(), mode == "rust");
        }
    }
}

/// noevia#1318: the child refuses `PUT`/`DELETE /api/v1/badges` only when the native writer owns
/// them, and reads (benchmark runs, the models list) the same `DATA_DIR/model_loader.db`.
/// noevia#1318: the child refuses canonical `GET /api/v1/benchmark/runs/{id}` and serves the
/// private runtime call only when the native reader owns the route, on the same `DATA_DIR`.
/// noevia#1318: the child refuses `PUT /api/v1/settings` and serves the private token test only
/// when the native writer owns it, on the same `DATA_DIR`.
fn settings_write_child(
    child_command: &mut tokio::process::Command,
    mode: &str,
    #[cfg(feature = "native-settings-write")] root: Option<&str>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_WRITE_CONFIRMED",
        if mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-settings-write")]
    if let Some(root) = root {
        child_command.env("DATA_DIR", root);
    }
}

#[cfg(all(test, feature = "native-settings-write"))]
mod settings_write_command_tests {
    #[test]
    fn confirmation_overrides_inherited_value() {
        for (mode, root, expected) in [
            ("rust", Some("/synthetic/data"), "1"),
            ("python", None, "0"),
        ] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_WRITE_CONFIRMED",
                "hostile",
            );
            super::settings_write_child(&mut command, mode, root);
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_WRITE_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(get("DATA_DIR"), root.map(std::ffi::OsStr::new));
        }
    }
}

/// noevia#1318: what the native models.ini writer reads, named exactly as the Python child's
/// settings read them (they fold names, so case-folded aliases keep Python writing).
#[cfg(feature = "native-section-writes")]
#[derive(Debug, PartialEq, Eq)]
struct WritesRoots {
    ini: String,
    cache: (i64, i64),
    /// `settings.migrate_cache_ram_on_start` (noevia#2010).
    migrate: bool,
}

#[cfg(feature = "native-section-writes")]
fn section_writes_roots(
    tz: &str,
    values: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Result<WritesRoots, &'static str> {
    if !(tz.is_empty() || tz == "UTC") {
        return Err("TZ is set to a zone other than UTC (backup names use local time)");
    }
    let canonical = |name: &str| -> Result<Option<String>, &'static str> {
        let mut found = None;
        for (key, value) in &values {
            if key
                .to_str()
                .is_some_and(|key| key != name && key.eq_ignore_ascii_case(name))
            {
                return Err("models.ini writer settings need canonical environment spelling");
            }
            if key == name {
                found = Some(
                    value
                        .to_str()
                        .ok_or("environment setting must be UTF-8")?
                        .to_owned(),
                );
            }
        }
        Ok(found)
    };
    let ini = canonical("MODELS_INI_PATH")?.unwrap_or_else(|| "/models/models.ini".into());
    let normal = ini.starts_with('/')
        && !ini.ends_with('/')
        && ini.len() <= 4096
        && ini
            .split('/')
            .skip(1)
            .all(|segment| !matches!(segment, "" | "." | ".."));
    if !normal {
        return Err("MODELS_INI_PATH is not an absolute, normalised path");
    }
    let int = |name: &str, default: i64| -> Result<i64, &'static str> {
        match canonical(name)? {
            None => Ok(default),
            Some(v) => {
                let digits = v.strip_prefix('-').unwrap_or(&v);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Err("cache-ram limits must be plain integers");
                }
                v.parse()
                    .map_err(|_| "cache-ram limits must be plain integers")
            }
        }
    };
    let hard = int("LLAMACPP_CACHE_RAM_HARD_MAX_MIB", 2048)?.max(0);
    let cap = int("LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB", 1024)?
        .max(0)
        .min(hard);
    // pydantic's bool: these words in any letter case, nothing stripped (anything else stops the
    // child at startup, so it never migrates either).
    let migrate = match canonical("MIGRATE_CACHE_RAM_ON_START")? {
        None => true,
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "1" | "on" | "t" | "true" | "y" | "yes" => true,
            "0" | "off" | "f" | "false" | "n" | "no" => false,
            _ => return Err("MIGRATE_CACHE_RAM_ON_START is not a boolean"),
        },
    };
    Ok(WritesRoots {
        ini,
        cache: (cap, hard),
        migrate,
    })
}

/// noevia#1318: the native overview reads the same models.ini as the child, so its path must
/// have one spelling (pydantic reads the variable case-insensitively) and be absolute and normal.
#[cfg(feature = "native-benchmark-overview")]
fn overview_ini(
    values: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Result<String, &'static str> {
    let name = "MODELS_INI_PATH";
    let mut found = None;
    for (key, value) in &values {
        if key
            .to_str()
            .is_some_and(|key| key != name && key.eq_ignore_ascii_case(name))
        {
            return Err("MODELS_INI_PATH needs canonical environment spelling");
        }
        if key == name {
            found = Some(
                value
                    .to_str()
                    .ok_or("environment setting must be UTF-8")?
                    .to_owned(),
            );
        }
    }
    let ini = found.unwrap_or_else(|| "/models/models.ini".into());
    let normal = ini.starts_with('/')
        && !ini.ends_with('/')
        && ini.len() <= 4096
        && ini
            .split('/')
            .skip(1)
            .all(|segment| !matches!(segment, "" | "." | ".."));
    if !normal {
        return Err("MODELS_INI_PATH is not an absolute, normalised path");
    }
    Ok(ini)
}

/// noevia#1318: the child refuses `GET /api/v1/benchmark` and serves the private job read only
/// when the native overview owns it, and then reads the same store and models.ini.
fn benchmark_overview_child(
    child_command: &mut tokio::process::Command,
    overview_mode: &str,
    #[cfg(feature = "native-benchmark-overview")] roots: Option<&(String, String)>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_OVERVIEW_CONFIRMED",
        if overview_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-benchmark-overview")]
    if let Some((data, ini)) = roots {
        child_command
            .env("DATA_DIR", data)
            .env("MODELS_INI_PATH", ini);
    }
}

/// noevia#1318: the child refuses the four runner routes, skips `reap_orphans` and serves the
/// private observation route only when the native runner owns the job slot.
/// noevia#1318: what the native download table needs: the models folder in pathlib's normal
/// form (Python's `dest_path` is `models_dir / filename`), the store, and the job slots.
#[cfg(feature = "native-downloads")]
#[derive(Debug, PartialEq, Eq)]
struct DownloadsRoots {
    models: String,
    data: String,
    concurrent: usize,
}

#[cfg(feature = "native-downloads")]
fn downloads_roots(
    values: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Result<DownloadsRoots, &'static str> {
    let roots = model_updates_roots(values.clone()).map_err(|_| {
        "native downloads require canonical MODELS_DIR, MODELS_INI_PATH and DATA_DIR environment spelling within limits"
    })?;
    let models = roots.models.as_str();
    let normal = models.starts_with('/')
        && (models == "/"
            || (!models.ends_with('/')
                && models
                    .split('/')
                    .skip(1)
                    .all(|segment| !matches!(segment, "" | "." | ".."))));
    if !normal {
        return Err("native downloads require an absolute, normalised MODELS_DIR");
    }
    // `settings.max_concurrent_downloads` (default 2): plain digits only, 1..=64.
    let mut concurrent = 2;
    for (key, value) in &values {
        let Some(key) = key.to_str() else { continue };
        if key != "MAX_CONCURRENT_DOWNLOADS" && key.eq_ignore_ascii_case("MAX_CONCURRENT_DOWNLOADS")
        {
            return Err("native downloads require canonical MAX_CONCURRENT_DOWNLOADS spelling");
        }
        if key == "MAX_CONCURRENT_DOWNLOADS" {
            concurrent = value
                .to_str()
                .filter(|v| !v.is_empty() && v.len() <= 2 && v.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|v| v.parse().ok())
                .filter(|n| (1..=64).contains(n))
                .ok_or("MAX_CONCURRENT_DOWNLOADS must be a plain number from 1 to 64")?;
        }
    }
    Ok(DownloadsRoots {
        models: roots.models,
        data: roots.data,
        concurrent,
    })
}

/// noevia#1318: what the native model delete reads, named exactly as the Python child's settings
/// read them (they fold names, so case-folded aliases are refused), and this container's name.
#[cfg(feature = "native-model-delete")]
#[derive(Debug, PartialEq, Eq)]
struct DeleteRoots {
    models: String,
    ini: String,
    host_path: String,
    targets: String,
    hostname: String,
}

#[cfg(feature = "native-model-delete")]
fn delete_roots(
    values: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Result<DeleteRoots, &'static str> {
    let canonical = |name: &str, default: &str| -> Result<String, &'static str> {
        let mut found = None;
        for (key, value) in &values {
            if key
                .to_str()
                .is_some_and(|key| key != name && key.eq_ignore_ascii_case(name))
            {
                return Err("native model delete requires canonical MODELS_DIR, MODELS_INI_PATH, MODELS_HOST_PATH and MODEL_DOWNLOAD_TARGETS spelling");
            }
            if key == name {
                found = Some(
                    value
                        .to_str()
                        .ok_or("environment setting must be UTF-8")?
                        .to_owned(),
                );
            }
        }
        let found = found.unwrap_or_else(|| default.to_owned());
        if found.len() > 65536 {
            return Err("native model delete settings exceeded limit");
        }
        Ok(found)
    };
    let models = canonical("MODELS_DIR", "/models")?;
    let normal = models.len() <= 4096
        && models.starts_with('/')
        && !models.ends_with('/')
        && models
            .split('/')
            .skip(1)
            .all(|segment| !matches!(segment, "" | "." | ".."));
    if !normal {
        return Err("native model delete requires an absolute, normalised MODELS_DIR other than /");
    }
    let ini = canonical("MODELS_INI_PATH", "/models/models.ini")?;
    if ini.is_empty() || ini.len() > 4096 {
        return Err("native model delete requires a MODELS_INI_PATH within limits");
    }
    let hostname = std::env::var("HOSTNAME").unwrap_or_default();
    Ok(DeleteRoots {
        models,
        ini,
        host_path: canonical("MODELS_HOST_PATH", "")?,
        targets: canonical("MODEL_DOWNLOAD_TARGETS", "")?,
        hostname,
    })
}

/// noevia#1318: the child refuses `POST /api/v1/models/delete` only when the native side owns
/// it, and is handed the same settings the native side reads.
fn model_delete_child(
    child_command: &mut tokio::process::Command,
    delete_mode: &str,
    #[cfg(feature = "native-model-delete")] roots: Option<&DeleteRoots>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DELETE_CONFIRMED",
        if delete_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-model-delete")]
    if let Some(roots) = roots {
        child_command
            .env("MODELS_DIR", &roots.models)
            .env("MODELS_INI_PATH", &roots.ini)
            .env("MODELS_HOST_PATH", &roots.host_path)
            .env("MODEL_DOWNLOAD_TARGETS", &roots.targets);
    }
}

/// noevia#1318 slice 13: what the native sampler reads, or why both stay with Python: it keeps the
/// native restart's errors (so it needs MODEL_MANAGER_BACKEND_CONTROL_IMPL=rust), formats start
/// times in UTC (TZ unset or UTC; the child is given UTC too), and reads LLAMA_CONTAINERS,
/// GPU_VRAM and LLAMA_DEFAULT_PORT by their exact names (the child's settings fold names, so a
/// case-folded alias is refused).
#[cfg(feature = "native-backends")]
fn backends_fallback(
    control_mode: &str,
    tz: &str,
    values: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> Result<Vec<(String, f64)>, &'static str> {
    if control_mode != "rust" {
        return Err("needs MODEL_MANAGER_BACKEND_CONTROL_IMPL=rust (the restart errors it shows are kept natively)");
    }
    if !(tz.is_empty() || tz == "UTC") {
        return Err("TZ is set to a zone other than UTC");
    }
    let mut gpu_vram = String::new();
    for (key, value) in &values {
        let Some(key) = key.to_str() else { continue };
        let upper = key.to_ascii_uppercase();
        if matches!(
            upper.as_str(),
            "GPU_VRAM" | "LLAMA_CONTAINERS" | "LLAMA_DEFAULT_PORT"
        ) && key != upper
        {
            return Err(
                "a case-folded spelling of GPU_VRAM, LLAMA_CONTAINERS or LLAMA_DEFAULT_PORT is set",
            );
        }
        if key == "GPU_VRAM" {
            gpu_vram = value.to_str().ok_or("GPU_VRAM must be UTF-8")?.to_owned();
        }
    }
    noevia_model_manager::backend_stats::gpu_vram_map(&gpu_vram)
}

/// noevia#1318 step 1: HOST_RAM_RESERVE_GB for the fit chips' usable RAM, a strict subset of what
/// the child's settings read. `None` (the chips are refused) for a case-variant key, which the
/// child would also read, or a value that is not a plain decimal.
#[cfg(feature = "native-backends")]
fn ram_reserve(values: Vec<(std::ffi::OsString, std::ffi::OsString)>) -> Option<f64> {
    let mut raw = None;
    for (key, value) in &values {
        let Some(key) = key.to_str() else { continue };
        if key.eq_ignore_ascii_case("HOST_RAM_RESERVE_GB") {
            if key != "HOST_RAM_RESERVE_GB" {
                return None;
            }
            raw = Some(value.to_str()?.to_owned());
        }
    }
    noevia_model_manager::backend_stats::fit::reserve_gb(raw.as_deref())
}

/// noevia#1318 slice 13: the child stops sampling the backends and refuses GET /api/v1/backends
/// only when the native sampler owns them; it then runs in UTC like the native answer.
fn backends_child(child_command: &mut tokio::process::Command, backends_mode: &str) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_BACKENDS_CONFIRMED",
        if backends_mode == "rust" { "1" } else { "0" },
    );
    if backends_mode == "rust" {
        child_command.env("TZ", "UTC");
    }
}

#[cfg(all(test, feature = "native-backends"))]
mod backends_command_tests {
    #![allow(clippy::unwrap_used)]
    #[test]
    fn ram_reserve_reads_the_exact_name_only() {
        let reserve = |pairs: &[(&str, &str)]| super::ram_reserve(env(pairs));
        assert_eq!(reserve(&[]), Some(32.0));
        assert_eq!(reserve(&[("HOST_RAM_RESERVE_GB", " 8.5 ")]), Some(8.5));
        assert_eq!(reserve(&[("HOST_RAM_RESERVE_GB", "-4")]), Some(-4.0));
        assert_eq!(reserve(&[("HOST_RAM_RESERVE_GB", "1e3")]), None);
        assert_eq!(reserve(&[("HOST_RAM_RESERVE_GB", "inf")]), None);
        assert_eq!(reserve(&[("host_ram_reserve_gb", "8")]), None);
        assert_eq!(reserve(&[("OTHER", "8")]), Some(32.0));
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn confirmation_overrides_inherited_value() {
        for (mode, expected, tz) in [("rust", "1", Some("UTC")), ("python", "0", None)] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env("NOEVIA_NATIVE_MODEL_MANAGER_BACKENDS_CONFIRMED", "hostile");
            super::backends_child(&mut command, mode);
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_BACKENDS_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(get("TZ"), tz.map(std::ffi::OsStr::new));
        }
    }

    #[test]
    fn fallback_reasons() {
        let ok = super::backends_fallback("rust", "", env(&[("GPU_VRAM", "a:8, b:x,a:12")]));
        assert_eq!(ok, Ok(vec![("a".to_owned(), 12.0)]));
        assert!(super::backends_fallback("rust", "UTC", env(&[])).is_ok());
        assert!(super::backends_fallback("python", "", env(&[])).is_err());
        assert!(super::backends_fallback("rust", "Europe/Berlin", env(&[])).is_err());
        for alias in ["gpu_vram", "Llama_Containers", "llama_default_port"] {
            assert!(
                super::backends_fallback("rust", "", env(&[(alias, "x")])).is_err(),
                "{alias}"
            );
        }
        assert!(super::backends_fallback("rust", "", env(&[("GPU_VRAM", "a:\u{0661}")])).is_err());
    }
}

/// noevia#1318 slice 12: the child refuses the draft-head read only when the native read owns it,
/// and is handed the same three roots.
fn draft_heads_child(
    child_command: &mut tokio::process::Command,
    heads_mode: &str,
    #[cfg(feature = "native-draft-heads")] roots: Option<&UpdatesRoots>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_DRAFT_HEADS_CONFIRMED",
        if heads_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-draft-heads")]
    if let Some(roots) = roots {
        child_command
            .env("MODELS_DIR", &roots.models)
            .env("MODELS_INI_PATH", &roots.ini)
            .env("DATA_DIR", &roots.data);
    }
}

/// noevia#1318 slice 14: the child refuses the two search reads only when the native side owns
/// them, and is handed the same three roots (it keeps `DATA_DIR`'s store and the models folder
/// the live state is read from).
fn search_child(
    child_command: &mut tokio::process::Command,
    search_mode: &str,
    #[cfg(feature = "native-search")] roots: Option<&UpdatesRoots>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_SEARCH_CONFIRMED",
        if search_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-search")]
    if let Some(roots) = roots {
        child_command
            .env("MODELS_DIR", &roots.models)
            .env("MODELS_INI_PATH", &roots.ini)
            .env("DATA_DIR", &roots.data);
    }
}

#[cfg(all(test, feature = "native-search"))]
mod search_command_tests {
    #[test]
    fn confirmation_overrides_inherited_value() {
        let roots = super::UpdatesRoots {
            models: "/synthetic/models".into(),
            ini: "/synthetic/models.ini".into(),
            data: "/synthetic/data".into(),
        };
        for (mode, roots, expected) in [("rust", Some(&roots), "1"), ("python", None, "0")] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env("NOEVIA_NATIVE_MODEL_MANAGER_SEARCH_CONFIRMED", "hostile");
            super::search_child(&mut command, mode, roots);
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_SEARCH_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(
                get("DATA_DIR"),
                roots.map(|r| std::ffi::OsStr::new(r.data.as_str()))
            );
        }
    }

    /// A case-folded spelling of a root keeps Python serving the routes (one log line).
    #[test]
    fn folded_spellings_are_refused() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect()
        };
        assert!(
            super::model_updates_roots(env(&[("MODELS_DIR", "/m"), ("DATA_DIR", "/d")])).is_ok()
        );
        for alias in ["models_dir", "Models_Ini_Path", "data_dir"] {
            assert!(
                super::model_updates_roots(env(&[(alias, "/x")])).is_err(),
                "{alias}"
            );
        }
    }
}

#[cfg(all(test, feature = "native-draft-heads"))]
mod draft_heads_command_tests {
    #[test]
    fn confirmation_overrides_inherited_value() {
        let roots = super::UpdatesRoots {
            models: "/synthetic/models".into(),
            ini: "/synthetic/models.ini".into(),
            data: "/synthetic/data".into(),
        };
        for (mode, roots, expected) in [("rust", Some(&roots), "1"), ("python", None, "0")] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_DRAFT_HEADS_CONFIRMED",
                "hostile",
            );
            super::draft_heads_child(&mut command, mode, roots);
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_DRAFT_HEADS_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(
                get("DATA_DIR"),
                roots.map(|r| std::ffi::OsStr::new(r.data.as_str()))
            );
        }
    }
}

#[cfg(all(test, feature = "native-model-delete"))]
mod model_delete_command_tests {
    #![allow(clippy::unwrap_used)]
    fn env(pairs: &[(&str, &str)]) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn confirmation_overrides_inherited_value() {
        for (mode, expected) in [("rust", "1"), ("python", "0")] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DELETE_CONFIRMED",
                "hostile",
            );
            let roots = super::delete_roots(env(&[("MODELS_DIR", "/m")])).unwrap();
            super::model_delete_child(&mut command, mode, (mode == "rust").then_some(&roots));
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DELETE_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(
                get("MODELS_DIR"),
                (mode == "rust").then_some(std::ffi::OsStr::new("/m"))
            );
        }
    }

    #[test]
    fn roots_need_canonical_spelling_and_a_normal_models_dir() {
        let roots = super::delete_roots(env(&[
            ("MODELS_DIR", "/srv/m"),
            ("MODEL_DOWNLOAD_TARGETS", "hf, dl"),
            ("MODELS_HOST_PATH", "/mnt/user/models"),
        ]))
        .unwrap();
        assert_eq!(
            (
                roots.models.as_str(),
                roots.ini.as_str(),
                roots.targets.as_str()
            ),
            ("/srv/m", "/models/models.ini", "hf, dl")
        );
        for bad in [
            env(&[("MODELS_DIR", "/")]),
            env(&[("MODELS_DIR", "/m/")]),
            env(&[("MODELS_DIR", "m")]),
            env(&[("MODELS_DIR", "/m/../x")]),
            env(&[("models_dir", "/m")]),
            env(&[("Model_Download_Targets", "x")]),
            env(&[("MODELS_INI_PATH", "")]),
        ] {
            assert!(super::delete_roots(bad.clone()).is_err(), "{bad:?}");
        }
    }
}

/// noevia#1318: the child refuses the download routes and plans starts only when the native
/// table owns them, and is handed the same models folder and store.
fn downloads_child(
    child_command: &mut tokio::process::Command,
    downloads_mode: &str,
    #[cfg(feature = "native-downloads")] roots: Option<&DownloadsRoots>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOADS_CONFIRMED",
        if downloads_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-downloads")]
    if let Some(roots) = roots {
        child_command
            .env("MODELS_DIR", &roots.models)
            .env("DATA_DIR", &roots.data)
            .env("MAX_CONCURRENT_DOWNLOADS", roots.concurrent.to_string());
    }
}

#[cfg(all(test, feature = "native-downloads"))]
mod downloads_command_tests {
    #[test]
    fn confirmation_overrides_inherited_value() {
        let roots = super::DownloadsRoots {
            models: "/synthetic/models".into(),
            data: "/synthetic/data".into(),
            concurrent: 3,
        };
        for (mode, roots, expected) in [("rust", Some(&roots), "1"), ("python", None, "0")] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env("NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOADS_CONFIRMED", "hostile");
            super::downloads_child(&mut command, mode, roots);
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_DOWNLOADS_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(
                get("MODELS_DIR"),
                roots.map(|r| std::ffi::OsStr::new(r.models.as_str()))
            );
        }
    }

    #[test]
    fn roots_need_a_normal_models_dir_and_a_plain_slot_count() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect()
        };
        assert_eq!(
            super::downloads_roots(env(&[("MODELS_DIR", "/m"), ("DATA_DIR", "/d")])),
            Ok(super::DownloadsRoots {
                models: "/m".into(),
                data: "/d".into(),
                concurrent: 2
            })
        );
        assert_eq!(
            super::downloads_roots(env(&[("MAX_CONCURRENT_DOWNLOADS", "4")])).map(|r| r.concurrent),
            Ok(4)
        );
        for bad in [
            env(&[("MODELS_DIR", "/m/")]),
            env(&[("MODELS_DIR", "/m/./x")]),
            env(&[("MODELS_DIR", "m")]),
            env(&[("MAX_CONCURRENT_DOWNLOADS", "0")]),
            env(&[("MAX_CONCURRENT_DOWNLOADS", " 2")]),
            env(&[("MAX_CONCURRENT_DOWNLOADS", "65")]),
            env(&[("max_concurrent_downloads", "2")]),
            env(&[("models_dir", "/m")]),
        ] {
            assert!(super::downloads_roots(bad.clone()).is_err(), "{bad:?}");
        }
    }
}

fn benchmark_runner_child(child_command: &mut tokio::process::Command, runner_mode: &str) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_RUNNER_CONFIRMED",
        if runner_mode == "rust" { "1" } else { "0" },
    );
}

#[cfg(all(test, feature = "native-benchmark-runner"))]
mod benchmark_runner_command_tests {
    #[test]
    fn confirmation_overrides_inherited_value() {
        for (mode, expected) in [("rust", "1"), ("python", "0")] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_RUNNER_CONFIRMED",
                "hostile",
            );
            super::benchmark_runner_child(&mut command, mode);
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            assert_eq!(
                envs.get(std::ffi::OsStr::new(
                    "NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_RUNNER_CONFIRMED"
                ))
                .copied()
                .flatten(),
                Some(std::ffi::OsStr::new(expected))
            );
        }
    }
}

#[cfg(all(test, feature = "native-benchmark-overview"))]
mod benchmark_overview_command_tests {
    #[test]
    fn confirmation_overrides_inherited_value() {
        for (mode, roots, expected) in [
            (
                "rust",
                Some((
                    "/synthetic/data".to_owned(),
                    "/synthetic/models.ini".to_owned(),
                )),
                "1",
            ),
            ("python", None, "0"),
        ] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_OVERVIEW_CONFIRMED",
                "hostile",
            );
            super::benchmark_overview_child(&mut command, mode, roots.as_ref());
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_BENCHMARK_OVERVIEW_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(
                get("MODELS_INI_PATH"),
                roots
                    .as_ref()
                    .map(|(_, i)| std::ffi::OsStr::new(i.as_str()))
            );
        }
    }

    #[test]
    fn ini_path_needs_one_normal_spelling() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect()
        };
        assert_eq!(
            super::overview_ini(env(&[])).as_deref(),
            Ok("/models/models.ini")
        );
        assert_eq!(
            super::overview_ini(env(&[("MODELS_INI_PATH", "/m/x.ini")])).as_deref(),
            Ok("/m/x.ini")
        );
        assert!(super::overview_ini(env(&[("models_ini_path", "/m/x.ini")])).is_err());
        assert!(super::overview_ini(env(&[("MODELS_INI_PATH", "/m/../x.ini")])).is_err());
        assert!(super::overview_ini(env(&[("MODELS_INI_PATH", "m.ini")])).is_err());
    }
}

/// noevia#1318: the child refuses the five models.ini writers and serves the private
/// safe-defaults plan only when the native writer owns them; it then runs in UTC and reads the
/// same models.ini.
fn section_writes_child(
    child_command: &mut tokio::process::Command,
    writes_mode: &str,
    #[cfg(feature = "native-section-writes")] roots: Option<&WritesRoots>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_SECTION_WRITES_CONFIRMED",
        if writes_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-section-writes")]
    if let Some(roots) = roots {
        child_command
            .env("TZ", "UTC")
            .env("MODELS_INI_PATH", &roots.ini);
    }
}

#[cfg(all(test, feature = "native-section-writes"))]
#[allow(clippy::unwrap_used)]
mod section_writes_command_tests {
    fn roots(tz: &str, pairs: &[(&str, &str)]) -> Result<super::WritesRoots, &'static str> {
        super::section_writes_roots(
            tz,
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        )
    }

    #[test]
    fn roots_defaults_limits_and_fallbacks() {
        let r = roots("", &[]).unwrap();
        assert_eq!(
            (r.ini.as_str(), r.cache),
            ("/models/models.ini", (1024, 2048))
        );
        assert!(r.migrate);
        let r = roots("UTC", &[("LLAMACPP_CACHE_RAM_HARD_MAX_MIB", "512")]).unwrap();
        assert_eq!(r.cache, (512, 512));
        assert_eq!(
            roots("", &[("LLAMACPP_CACHE_RAM_HARD_MAX_MIB", "-5")])
                .unwrap()
                .cache,
            (0, 0)
        );
        assert!(roots("Europe/Berlin", &[]).is_err());
        for bad in ["models.ini", "/m/", "/m//x.ini", "/m/../x.ini"] {
            assert!(roots("", &[("MODELS_INI_PATH", bad)]).is_err(), "{bad}");
        }
        assert!(roots("", &[("models_ini_path", "/x.ini")]).is_err());
        assert!(roots("", &[("LLAMACPP_CACHE_RAM_HARD_MAX_MIB", "1_000")]).is_err());
    }

    /// noevia#2010: every value the oracle ran through the pinned Python `Settings` reads the same
    /// here; a case-folded spelling, which Python also reads, keeps Python writing instead.
    #[test]
    #[allow(clippy::indexing_slicing)]
    fn startup_migration_setting_matches_the_pinned_python_settings() {
        let fx: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/section-writes-python.v1.json"
        ))
        .unwrap();
        let flags = fx["migrate_flag"].as_array().unwrap();
        assert!(flags.len() >= 25);
        for f in flags {
            let (name, value, python) = (f[0].as_str().unwrap(), f[1].as_str(), f[2].as_bool());
            let pairs: Vec<(&str, &str)> = value.map(|v| vec![(name, v)]).unwrap_or_default();
            let native = roots("", &pairs);
            if name == "MIGRATE_CACHE_RAM_ON_START" {
                assert_eq!(native.ok().map(|r| r.migrate), python, "{value:?}");
            } else {
                assert!(python.is_some() && native.is_err(), "{name}");
            }
        }
    }

    #[test]
    fn startup_migration_setting_reads_as_pydantic_does() {
        // noevia#2010: the words pydantic accepts, in any case; anything else keeps Python
        // writing (and stops the child), as do non-canonical spellings.
        for (value, on) in [
            ("true", true),
            ("TRUE", true),
            ("tRuE", true),
            ("1", true),
            ("on", true),
            ("t", true),
            ("y", true),
            ("YES", true),
            ("false", false),
            ("0", false),
            ("off", false),
            ("f", false),
            ("n", false),
            ("No", false),
        ] {
            let r = roots("", &[("MIGRATE_CACHE_RAM_ON_START", value)]).unwrap();
            assert_eq!(r.migrate, on, "{value}");
        }
        for bad in ["", " true", "true ", "2", "1.0", "enabled"] {
            assert!(
                roots("", &[("MIGRATE_CACHE_RAM_ON_START", bad)]).is_err(),
                "{bad:?}"
            );
        }
        assert!(roots("", &[("migrate_cache_ram_on_start", "false")]).is_err());
    }

    #[test]
    fn child_confirmation_overrides_inherited_value() {
        for (mode, expected) in [("rust", "1"), ("python", "0")] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_SECTION_WRITES_CONFIRMED",
                "hostile",
            );
            let r = roots("", &[]).unwrap();
            super::section_writes_child(&mut command, mode, (mode == "rust").then_some(&r));
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_SECTION_WRITES_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(get("TZ").is_some(), mode == "rust");
        }
    }
}

fn bench_runs_child(
    child_command: &mut tokio::process::Command,
    bench_mode: &str,
    #[cfg(feature = "native-bench-runs")] root: Option<&str>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_BENCH_RUNS_CONFIRMED",
        if bench_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-bench-runs")]
    if let Some(root) = root {
        child_command.env("DATA_DIR", root);
    }
}

#[cfg(all(test, feature = "native-bench-runs"))]
mod bench_runs_command_tests {
    #[test]
    fn bench_runs_confirmation_overrides_inherited_value() {
        for (mode, root, expected) in [
            ("rust", Some("/synthetic/data"), "1"),
            ("python", None, "0"),
        ] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_BENCH_RUNS_CONFIRMED",
                "hostile",
            );
            super::bench_runs_child(&mut command, mode, root);
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_BENCH_RUNS_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(get("DATA_DIR"), root.map(std::ffi::OsStr::new));
        }
    }
}

fn badges_child(
    child_command: &mut tokio::process::Command,
    badges_mode: &str,
    #[cfg(feature = "native-badges")] root: Option<&str>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_BADGES_CONFIRMED",
        if badges_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-badges")]
    if let Some(root) = root {
        child_command.env("DATA_DIR", root);
    }
}

#[cfg(all(test, feature = "native-badges"))]
mod badges_command_tests {
    #[test]
    fn badges_confirmation_overrides_inherited_value() {
        for (mode, root, expected) in [
            ("rust", Some("/synthetic/data"), "1"),
            ("python", None, "0"),
        ] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env("NOEVIA_NATIVE_MODEL_MANAGER_BADGES_CONFIRMED", "hostile");
            super::badges_child(&mut command, mode, root);
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |k: &str| envs.get(std::ffi::OsStr::new(k)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_BADGES_CONFIRMED"),
                Some(std::ffi::OsStr::new(expected))
            );
            assert_eq!(get("DATA_DIR"), root.map(std::ffi::OsStr::new));
        }
    }
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

/// noevia#1318: the child refuses `GET /api/v1/settings` only when the native reader owns it,
/// and reads/writes the same `DATA_DIR/model_loader.db` the native reader opens.
fn settings_child(
    child_command: &mut tokio::process::Command,
    settings_mode: &str,
    #[cfg(feature = "native-settings")] root: Option<&str>,
) {
    child_command.env(
        "NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_CONFIRMED",
        if settings_mode == "rust" { "1" } else { "0" },
    );
    #[cfg(feature = "native-settings")]
    if let Some(root) = root {
        child_command.env("DATA_DIR", root);
    }
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

// Compiled with the feature only, so other features' exact test inventories stay unchanged.
#[cfg(all(test, feature = "native-settings"))]
mod settings_command_tests {
    #[test]
    fn settings_confirmation_overrides_inherited_value() {
        for (mode, marker) in [("python", "0"), ("rust", "1")] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env("NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_CONFIRMED", "hostile");
            super::settings_child(
                &mut command,
                mode,
                (mode == "rust").then_some("/synthetic/root"),
            );
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            assert_eq!(
                envs.get(std::ffi::OsStr::new(
                    "NOEVIA_NATIVE_MODEL_MANAGER_SETTINGS_CONFIRMED"
                ))
                .copied()
                .flatten(),
                Some(std::ffi::OsStr::new(marker))
            );
            assert_eq!(
                envs.get(std::ffi::OsStr::new("DATA_DIR"))
                    .copied()
                    .flatten(),
                (mode == "rust").then_some(std::ffi::OsStr::new("/synthetic/root"))
            );
        }
    }
}

// Compiled with the feature only, so other features' exact test inventories stay unchanged.
#[cfg(all(test, feature = "native-model-updates"))]
#[allow(clippy::unwrap_used)]
mod model_updates_command_tests {
    use std::ffi::{OsStr, OsString};

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn roots_default_and_explicit() {
        assert_eq!(
            super::model_updates_roots(vars(&[])).unwrap(),
            super::UpdatesRoots {
                models: "/models".into(),
                ini: "/models/models.ini".into(),
                data: "/data".into()
            }
        );
        let explicit = super::model_updates_roots(vars(&[
            ("MODELS_DIR", "/synthetic/models"),
            ("MODELS_INI_PATH", "/synthetic/presets.ini"),
            ("DATA_DIR", "/synthetic/data"),
        ]))
        .unwrap();
        assert_eq!(explicit.models, "/synthetic/models");
        assert_eq!(explicit.ini, "/synthetic/presets.ini");
        assert_eq!(explicit.data, "/synthetic/data");
    }

    #[test]
    fn case_folded_aliases_and_empty_roots_are_refused() {
        for alias in [
            ("models_dir", "/x"),
            ("Models_Ini_Path", "/x"),
            ("data_dir", "/x"),
            ("MODELS_DIR", ""),
        ] {
            assert!(
                super::model_updates_roots(vars(&[alias])).is_err(),
                "{alias:?}"
            );
        }
    }

    #[test]
    fn confirmation_overrides_inherited_value_and_roots_reach_child() {
        let roots = super::UpdatesRoots {
            models: "/synthetic/models".into(),
            ini: "/synthetic/models/models.ini".into(),
            data: "/synthetic/data".into(),
        };
        for (mode, marker) in [("python", "0"), ("rust", "1")] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_MODEL_UPDATES_CONFIRMED",
                "hostile",
            );
            super::model_updates_child(&mut command, mode, (mode == "rust").then_some(&roots));
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |key: &str| envs.get(OsStr::new(key)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_MODEL_UPDATES_CONFIRMED"),
                Some(OsStr::new(marker))
            );
            let expected = |value: &'static str| (mode == "rust").then_some(OsStr::new(value));
            assert_eq!(get("MODELS_DIR"), expected("/synthetic/models"));
            assert_eq!(
                get("MODELS_INI_PATH"),
                expected("/synthetic/models/models.ini")
            );
            assert_eq!(get("DATA_DIR"), expected("/synthetic/data"));
        }
    }
}

// Compiled with the feature only, so other features' exact test inventories stay unchanged.
#[cfg(all(test, feature = "native-model-detail"))]
#[allow(clippy::unwrap_used)]
mod model_detail_command_tests {
    use std::ffi::{OsStr, OsString};

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn roots_must_be_canonical_and_normal() {
        assert_eq!(
            super::model_detail_roots(vars(&[])).unwrap().models,
            "/models"
        );
        assert_eq!(
            super::model_detail_roots(vars(&[("MODELS_DIR", "/")]))
                .unwrap()
                .models,
            "/"
        );
        assert_eq!(
            super::model_detail_roots(vars(&[("MODELS_DIR", "/synthetic/models")]))
                .unwrap()
                .models,
            "/synthetic/models"
        );
        for models in [
            "models", "/models/", "//models", "/a//b", "/a/./b", "/a/../b", "/a/.",
        ] {
            assert!(
                super::model_detail_roots(vars(&[("MODELS_DIR", models)])).is_err(),
                "{models}"
            );
        }
        assert!(super::model_detail_roots(vars(&[("models_dir", "/x")])).is_err());
    }

    #[test]
    fn confirmation_zone_and_roots_reach_child_only_in_native_mode() {
        let roots = super::UpdatesRoots {
            models: "/synthetic/models".into(),
            ini: "/synthetic/models/models.ini".into(),
            data: "/synthetic/data".into(),
        };
        for (mode, marker) in [("python", "0"), ("rust", "1")] {
            let mut command = tokio::process::Command::new("uvicorn");
            command.env(
                "NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DETAIL_CONFIRMED",
                "hostile",
            );
            super::model_detail_child(&mut command, mode, (mode == "rust").then_some(&roots));
            let envs: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
            let get = |key: &str| envs.get(OsStr::new(key)).copied().flatten();
            assert_eq!(
                get("NOEVIA_NATIVE_MODEL_MANAGER_MODEL_DETAIL_CONFIRMED"),
                Some(OsStr::new(marker))
            );
            let expected = |value: &'static str| (mode == "rust").then_some(OsStr::new(value));
            assert_eq!(get("TZ"), expected("UTC"));
            assert_eq!(get("MODELS_DIR"), expected("/synthetic/models"));
            assert_eq!(
                get("MODELS_INI_PATH"),
                expected("/synthetic/models/models.ini")
            );
        }
    }
}

#[cfg(all(test, feature = "native-model-detail"))]
mod detail_fallback_tests {
    #[test]
    fn only_unset_or_utc_zone_and_usable_roots_select_native() {
        for tz in ["", "UTC"] {
            assert!(super::detail_fallback(tz, Vec::new()).is_ok(), "{tz:?}");
        }
        for tz in ["utc", "Europe/Berlin", "UTC0", " UTC"] {
            assert!(super::detail_fallback(tz, Vec::new()).is_err(), "{tz:?}");
        }
        assert!(
            super::detail_fallback("UTC", vec![("MODELS_DIR".into(), "/models/".into())]).is_err()
        );
    }
}
