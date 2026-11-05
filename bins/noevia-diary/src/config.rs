//! Start-up configuration. Python (agent/rust_forward.py) resolves its own config (config.yaml
//! plus environment) and sends the values this process needs as ONE JSON line on stdin, so
//! secrets never appear in argv or this process's environment and both sides read the same
//! settings.

use crate::corpus::BaseCorpus;
use crate::health::ClientConfig;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct Llm {
    pub main: ClientConfig,
    pub aux: ClientConfig,
    #[serde(default)]
    pub aux_prefers_main: bool,
    /// `_base_cfg.get("llm.chat_model")`, raw (health reports it as `model_configured`).
    #[serde(default)]
    pub configured: Value,
}

/// GET /api/workspace-export (D4b): Python's WORKSPACE_EXPORT_IMPL, its exporter binary path
/// (workspace_export_native.BINARY) and the base `corpus.<key>` value of each SETTING_KEYS key.
#[derive(Debug, Default, Deserialize)]
pub struct WorkspaceExport {
    #[serde(default, rename = "impl")]
    pub implementation: String,
    #[serde(default)]
    pub binary: String,
    #[serde(default)]
    pub settings: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub port: u16,
    pub control_token: String,
    /// Secret Python sends on every forwarded request (`x-noevia-diary-forward`).
    pub forward_secret: String,
    #[serde(default)]
    pub auth_token: String,
    #[serde(default)]
    pub tenant_key: String,
    /// dirname(retrieval.db_path): the directory holding `users/<uid>/`.
    pub data_root: String,
    #[serde(default)]
    pub legacy_user_id: String,
    pub corpus: BaseCorpus,
    pub llm: Llm,
    #[serde(default)]
    pub workspace_export: WorkspaceExport,
    /// Python's DIARY_CORPUS_WRITE_IMPL ("python" when absent).
    #[serde(default)]
    pub corpus_write_impl: String,
    /// D6: the folders `external_source_paths()` resolves (DIARY_EXTERNAL_SOURCES), as Python
    /// would list them; absent from an older Python, in which case the route declines.
    #[serde(default)]
    pub external_sources: Option<Vec<String>>,
}

pub fn parse(line: &str) -> Result<Config, String> {
    serde_json::from_str(line).map_err(|e| format!("invalid start-up config: {e}"))
}
