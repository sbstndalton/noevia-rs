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
    /// R9 answering, step 1: the internal answer endpoint (absent: it answers 404).
    #[serde(default)]
    pub answer: Option<crate::answer::AnswerConfig>,
    /// R9: Python's `external_source_paths()` resolved at start (its DIARY_EXTERNAL_SOURCES with
    /// `~` expanded by its own HOME). Absent: GET /api/external-sources stays with Python.
    #[serde(default)]
    pub external_sources: Option<Vec<String>>,
    /// R9 main mode (front.rs): the read routes the Python child names in DIARY_RUST_ROUTES (paths);
    /// the front serves only these natively and forwards every other request. Unused otherwise.
    #[serde(default)]
    pub native_routes: Vec<String>,
    /// R9 main mode, S3: the child's DIARY_RUST_WRITES (lower-case UUIDs, or `*`): the tenants whose
    /// writes the front serves natively under the writer slot the child leases.
    #[serde(default)]
    pub native_writers: Vec<String>,
}

pub fn parse(line: &str) -> Result<Config, String> {
    serde_json::from_str(line).map_err(|e| format!("invalid start-up config: {e}"))
}
