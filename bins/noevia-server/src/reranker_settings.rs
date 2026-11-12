//! Reranker settings policy, retaining JavaScript strings as UTF-16 throughout.
use crate::model_maint::v8_json::{self, W};
use serde_json::{json, Value};
use std::collections::BTreeMap;
pub const KEY: &str = "rerank:configuration";
pub type Environment = BTreeMap<String, String>;
pub type EffectiveEnvironment = BTreeMap<String, Vec<u16>>;
#[derive(Clone, Default)]
pub struct Deployment(pub(crate) Environment);
impl std::fmt::Debug for Deployment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deployment").finish_non_exhaustive()
    }
}
impl FromIterator<(String, String)> for Deployment {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}
#[derive(Debug, PartialEq)]
pub struct Refusal {
    pub status: u16,
    pub body: Value,
}
fn refusal(status: u16, error: &str, id: &str) -> Refusal {
    Refusal {
        status,
        body: json!({"error":error,"errorId":id}),
    }
}
pub fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}
pub fn quote(s: &[u16]) -> String {
    let mut out = String::new();
    v8_json::quote(s, &mut out);
    out
}
fn space(c: u32) -> bool {
    matches!(c,9..=13|32|160|0x1680|0x2000..=0x200a|0x2028|0x2029|0x202f|0x205f|0x3000|0xfeff)
}
fn trim(s: &str) -> &str {
    s.trim_matches(|c| space(c as u32))
}
fn trim_units(s: &[u16]) -> &[u16] {
    let first = s
        .iter()
        .position(|c| !space(u32::from(*c)))
        .unwrap_or(s.len());
    let last = s
        .iter()
        .rposition(|c| !space(u32::from(*c)))
        .map_or(first, |n| n + 1);
    s.get(first..last).unwrap_or_default()
}
fn text<'a>(env: &'a Environment, key: &str) -> &'a str {
    env.get(key).map(|s| trim(s)).unwrap_or("")
}
fn backend(env: &Environment) -> Option<&'static str> {
    match text(env, "RERANK_BACKEND").to_ascii_lowercase().as_str() {
        "" | "llamacpp" | "llama.cpp" | "llama" => Some("llamacpp"),
        "tei" => Some("tei"),
        _ => None,
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub enum ChoiceInput {
    #[default]
    Missing,
    Null,
    Text(Vec<u16>),
    Invalid,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Input {
    pub reset: bool,
    pub enabled: Option<bool>,
    pub choice: ChoiceInput,
}
pub fn parse_input(bytes: &[u8]) -> Option<Input> {
    let text = String::from_utf8_lossy(bytes);
    let value = v8_json::parse(&units(if bytes.is_empty() { "{}" } else { &text })).ok()?;
    let W::Obj(_) = &value else {
        return Some(Input::default());
    };
    Some(Input {
        reset: matches!(value.get("reset"), W::Bool(true)),
        enabled: match value.get("enabled") {
            W::Bool(b) => Some(*b),
            _ => None,
        },
        choice: match value.get("choice") {
            W::Undefined => ChoiceInput::Missing,
            W::Null => ChoiceInput::Null,
            W::Str(s) => ChoiceInput::Text(s.clone()),
            _ => ChoiceInput::Invalid,
        },
    })
}
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    pub enabled: bool,
    pub choice: Option<Vec<u16>>,
}
impl Saved {
    pub fn json(&self) -> String {
        format!(
            "{{\"enabled\":{},\"choice\":{}}}",
            self.enabled,
            self.choice
                .as_deref()
                .map(quote)
                .unwrap_or_else(|| "null".into())
        )
    }
}
#[derive(Clone)]
pub struct InstalledModel {
    name: Vec<u16>,
    rerank_label: bool,
    missing: bool,
}
fn truthy(value: &W) -> bool {
    match value {
        W::Undefined | W::Null | W::Bool(false) => false,
        W::Num(n) => *n != 0.0 && !n.is_nan(),
        W::Str(s) => !s.is_empty(),
        _ => true,
    }
}
fn ascii_lower(s: &[u16]) -> Vec<u16> {
    s.iter()
        .map(|c| if (65..=90).contains(c) { *c + 32 } else { *c })
        .collect()
}
pub fn installed_models(value: &W) -> Option<Vec<InstalledModel>> {
    let W::Arr(rows) = value else {
        return None;
    };
    Some(rows.iter().filter_map(|m| {
        let W::Str(name) = m.get("name") else { return None; };
        let rerank_label = match m.get("labels") {
            W::Arr(labels) => labels.iter().any(|label| matches!(label, W::Str(s) if ascii_lower(s).starts_with(&units("rerank")))),
            _ => false,
        };
        Some(InstalledModel { name: name.clone(), rerank_label, missing: truthy(m.get("missingFile")) })
    }).collect())
}
pub fn parse_installed(bytes: &[u8]) -> Option<Vec<InstalledModel>> {
    installed_models(&v8_json::parse(&units(&String::from_utf8_lossy(bytes))).ok()?)
}
pub fn parse_consumer_request(bytes: &[u8]) -> Option<Option<Vec<InstalledModel>>> {
    Some(installed_models(
        v8_json::parse(&units(&String::from_utf8_lossy(bytes)))
            .ok()?
            .get("installed"),
    ))
}
#[derive(Clone)]
pub struct OptionEntry {
    pub id: Vec<u16>,
    pub backend: &'static str,
    pub model: Vec<u16>,
}
impl OptionEntry {
    fn json(&self) -> String {
        format!(
            "{{\"id\":{},\"backend\":{},\"model\":{}}}",
            quote(&self.id),
            quote(&units(self.backend)),
            quote(&self.model)
        )
    }
}
#[derive(Clone)]
pub struct Settings {
    pub deployment: Environment,
    pub machine: Option<(Vec<u16>, String)>,
    pub saved: Option<Saved>,
}
impl Settings {
    pub fn new(
        deployment: Environment,
        machine: Option<(Vec<u16>, String)>,
        stored: Option<&str>,
    ) -> Self {
        let saved = stored
            .and_then(|s| parse_input(s.as_bytes()))
            .and_then(|v| {
                let enabled = v.enabled?;
                let choice = match v.choice {
                    ChoiceInput::Null => None,
                    ChoiceInput::Text(s) => Some(s),
                    _ => return None,
                };
                Some(Saved { enabled, choice })
            });
        Self {
            deployment,
            machine,
            saved,
        }
    }
    fn endpoints(&self) -> (Option<Vec<u16>>, Option<Vec<u16>>, Vec<u16>) {
        let env = &self.deployment;
        let b = backend(env);
        let seed = text(env, "RERANK_BASE_URL");
        let tei = text(env, "RERANK_TEI_BASE_URL");
        let llama = text(env, "RERANK_LLAMACPP_BASE_URL");
        let model = text(env, "RERANK_TEI_MODEL");
        let model = if model.is_empty() && b == Some("tei") {
            text(env, "RERANK_MODEL")
        } else {
            model
        };
        let model = units(if model.is_empty() {
            "gte-multilingual-reranker-base"
        } else {
            model
        });
        if let Some((url, p)) = &self.machine {
            return (
                if p == "tei" { Some(url.clone()) } else { None },
                if p == "llamacpp" {
                    Some(url.clone())
                } else {
                    None
                },
                model,
            );
        }
        let tei = if !tei.is_empty() {
            Some(units(tei))
        } else if b == Some("tei") {
            Some(units(if seed.is_empty() {
                "http://rerank:8080"
            } else {
                seed
            }))
        } else {
            None
        };
        let llama = if !llama.is_empty() {
            Some(units(llama))
        } else if b == Some("llamacpp") && !seed.is_empty() {
            Some(units(seed))
        } else {
            None
        };
        (tei, llama, model)
    }
    pub fn options(&self, installed: Option<&[InstalledModel]>) -> Vec<OptionEntry> {
        let (tei, llama, model) = self.endpoints();
        let mut out = Vec::new();
        if tei.is_some() {
            out.push(OptionEntry {
                id: units("tei"),
                backend: "tei",
                model,
            });
        }
        if llama.is_some() {
            for m in installed.into_iter().flatten() {
                let name = ascii_lower(&m.name);
                let rerank = name.windows(6).any(|w| w == units("rerank")) || m.rerank_label;
                if rerank && !m.missing {
                    let mut id = units("llamacpp:");
                    id.extend(&m.name);
                    out.push(OptionEntry {
                        id,
                        backend: "llamacpp",
                        model: m.name.clone(),
                    });
                }
            }
        }
        out
    }
    fn deployment_choice(&self) -> Option<Vec<u16>> {
        let b = self
            .machine
            .as_ref()
            .map(|(_, p)| p.as_str())
            .or_else(|| backend(&self.deployment));
        match b {
            Some("tei") => Some(units("tei")),
            Some("llamacpp") if !text(&self.deployment, "RERANK_MODEL").is_empty() => Some(units(
                &format!("llamacpp:{}", text(&self.deployment, "RERANK_MODEL")),
            )),
            _ => None,
        }
    }
    fn choice(&self) -> Option<Vec<u16>> {
        self.saved
            .as_ref()
            .map(|s| s.choice.clone())
            .unwrap_or_else(|| self.deployment_choice())
    }
    pub fn effective_env(&self) -> EffectiveEnvironment {
        let mut out = self
            .deployment
            .iter()
            .map(|(k, v)| (k.clone(), units(v)))
            .collect::<EffectiveEnvironment>();
        // An absent native key is explicit, so an engine cannot inherit another process key.
        out.entry("RERANK_API_KEY".into()).or_default();
        let (tei, llama, model) = self.endpoints();
        if let Some((url, p)) = &self.machine {
            out.insert("RERANK_BACKEND".into(), units(p));
            out.insert("RERANK_BASE_URL".into(), url.clone());
            if p == "tei" {
                out.insert("RERANK_MODEL".into(), model.clone());
            }
            let host = |s: &[u16]| {
                url::Url::parse(&String::from_utf16_lossy(trim_units(s)))
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            };
            if host(url) != host(&units(text(&self.deployment, "RERANK_BASE_URL"))) {
                out.insert("RERANK_API_KEY".into(), Vec::new());
            }
        }
        if let Some(saved) = &self.saved {
            out.insert(
                "NOEVIA_FEATURE_RAG_RERANK".into(),
                units(if saved.enabled { "true" } else { "false" }),
            );
            if let Some(choice) = &saved.choice {
                let prefix = units("llamacpp:");
                let picked = if choice == &units("tei") {
                    tei.map(|url| ("tei", url, model))
                } else if choice.starts_with(&prefix) {
                    llama.map(|url| {
                        (
                            "llamacpp",
                            url,
                            choice.get(prefix.len()..).unwrap_or_default().to_vec(),
                        )
                    })
                } else {
                    None
                };
                if let Some((b, url, model)) = picked {
                    out.insert("RERANK_BACKEND".into(), units(b));
                    out.insert("RERANK_BASE_URL".into(), url);
                    out.insert("RERANK_MODEL".into(), model);
                } else if !choice.is_empty() {
                    out.insert("NOEVIA_FEATURE_RAG_RERANK".into(), units("false"));
                }
            }
        }
        out
    }
    pub fn effective_env_json(&self) -> String {
        let fields = self
            .effective_env()
            .into_iter()
            .map(|(k, v)| format!("{}:{}", quote(&units(&k)), quote(&v)))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{{fields}}}")
    }
    pub fn view_json(&self, installed: Option<&[InstalledModel]>) -> String {
        let env = self.effective_env();
        let enabled = env
            .get("NOEVIA_FEATURE_RAG_RERANK")
            .and_then(|s| String::from_utf16(s).ok())
            .is_some_and(|s| matches!(trim(&s).to_ascii_lowercase().as_str(), "1" | "true" | "on"));
        let choice = self
            .choice()
            .as_deref()
            .map(quote)
            .unwrap_or_else(|| "null".into());
        let options = self
            .options(installed)
            .iter()
            .map(OptionEntry::json)
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"enabled\":{enabled},\"choice\":{choice},\"source\":\"{}\",\"options\":[{options}]}}",if self.saved.is_some(){"admin"}else{"deployment"})
    }
    pub fn validate(
        &self,
        value: &Input,
        installed: Option<&[InstalledModel]>,
    ) -> Result<Saved, Refusal> {
        let Some(enabled) = value.enabled else {
            return Err(refusal(
                400,
                "Send whether reranking is on and which reranker to use.",
                "invalidInput",
            ));
        };
        if value.choice == ChoiceInput::Invalid {
            return Err(refusal(
                400,
                "Send whether reranking is on and which reranker to use.",
                "invalidInput",
            ));
        }
        let current = self.choice();
        let choice = match &value.choice {
            ChoiceInput::Missing => current.clone(),
            ChoiceInput::Text(s) => Some(trim_units(s).to_vec()),
            _ => None,
        };
        let choice = choice.filter(|s| !s.is_empty());
        if let Some(id) = &choice {
            let keeps = !enabled && choice == current;
            if !keeps && !self.options(installed).iter().any(|o| o.id == *id) {
                if id.starts_with(&units("llamacpp:")) && installed.is_none() {
                    return Err(refusal(
                        503,
                        "The installed models could not be read. Try again in a moment.",
                        "catalogueUnavailable",
                    ));
                }
                return Err(refusal(
                    400,
                    "That reranker is not available on this server.",
                    "unknownReranker",
                ));
            }
        }
        if enabled && choice.is_none() {
            return Err(refusal(
                400,
                "Choose a reranker before turning reranking on.",
                "noReranker",
            ));
        }
        Ok(Saved { enabled, choice })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn env_string(settings: &Settings, key: &str) -> Option<String> {
        settings
            .effective_env()
            .get(key)
            .and_then(|s| String::from_utf16(s).ok())
    }
    #[test]
    fn original_reference_cases() -> Result<(), Box<dyn std::error::Error>> {
        assert!(crate::FEATURES.contains(&"reranker-settings-v1"));
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/reranker-settings.json"))?;
        let deployment = [
            ("RERANK_BACKEND", "tei"),
            ("RERANK_LLAMACPP_BASE_URL", "http://synthetic-engine"),
            ("NOEVIA_FEATURE_RAG_RERANK", "false"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect::<Environment>();
        let installed = parse_installed(br#"[{"name":"synthetic-reranker","labels":["reranking"]},{"name":"missing-reranker","missingFile":true}]"#).ok_or("catalogue")?;
        for case in cases {
            let mut owner = Settings::new(deployment.clone(), None, None);
            let name = case
                .get("name")
                .and_then(Value::as_str)
                .ok_or("case name")?;
            let list = if name == "unreadable" {
                None
            } else {
                Some(installed.as_slice())
            };
            let (status, body) = if matches!(name, "deployment" | "reset" | "delete") {
                (200, owner.view_json(list))
            } else {
                let input = parse_input(case.get("input").ok_or("input")?.to_string().as_bytes())
                    .ok_or("valid input")?;
                match owner.validate(&input, list) {
                    Ok(saved) => {
                        owner.saved = Some(saved);
                        (200, owner.view_json(list))
                    }
                    Err(r) => (r.status, r.body.to_string()),
                }
            };
            assert_eq!(
                json!(status),
                case.get("status").cloned().ok_or("status")?,
                "{name}"
            );
            assert_eq!(
                serde_json::from_str::<Value>(&body)?,
                case.get("body").cloned().ok_or("body")?,
                "{name}"
            );
            assert_eq!(
                json!(owner.saved.map(|s| s.json())),
                case.get("stored").cloned().ok_or("stored")?,
                "{name}"
            );
        }
        Ok(())
    }
    #[test]
    fn omitted_unavailable_choice_stays_verbatim() -> Result<(), Box<dyn std::error::Error>> {
        let settings = Settings::new(
            Environment::new(),
            None,
            Some(r#"{"enabled":true,"choice":" stale "}"#),
        );
        let input = parse_input(br#"{"enabled":false}"#).ok_or("synthetic JSON")?;
        assert_eq!(
            settings.validate(&input, None),
            Ok(Saved {
                enabled: false,
                choice: Some(units(" stale "))
            })
        );
        Ok(())
    }
    #[test]
    fn machine_key_policy_and_redacted_deployment() {
        assert_eq!(
            env_string(
                &Settings::new(Environment::new(), None, None),
                "RERANK_API_KEY"
            )
            .as_deref(),
            Some("")
        );
        let env = [
            ("RERANK_BACKEND", "llamacpp"),
            ("RERANK_BASE_URL", "http://synthetic-host:8000"),
            ("RERANK_API_KEY", "synthetic-key"),
            ("RERANK_MODEL", "synthetic-model"),
            ("NOEVIA_FEATURE_RAG_RERANK", "true"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect::<Environment>();
        let same = Settings::new(
            env.clone(),
            Some((units("http://synthetic-host:9000"), "tei".into())),
            None,
        );
        assert_eq!(
            env_string(&same, "RERANK_API_KEY").as_deref(),
            Some("synthetic-key")
        );
        let remote = Settings::new(
            env.clone(),
            Some((units("http://synthetic-other:9000"), "tei".into())),
            None,
        );
        assert_eq!(env_string(&remote, "RERANK_API_KEY").as_deref(), Some(""));
        assert_eq!(
            env_string(&remote, "RERANK_MODEL").as_deref(),
            Some("gte-multilingual-reranker-base")
        );
        assert!(!remote.view_json(None).contains("http://"));
        assert!(!format!("{:?}", Deployment(env)).contains("synthetic-key"));
    }
    #[test]
    fn legacy_utf16_choices_are_lossless_and_can_be_disabled(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let env = [
            ("RERANK_BACKEND".into(), "llamacpp".into()),
            ("RERANK_BASE_URL".into(), "http://synthetic-engine".into()),
        ]
        .into_iter()
        .collect();
        let mut owner = Settings::new(
            env,
            None,
            Some(r#"{"enabled":true,"choice":"llamacpp:\ud800","unused":1e999}"#),
        );
        assert!(owner
            .view_json(None)
            .contains(r#""choice":"llamacpp:\ud800""#));
        assert!(owner
            .effective_env_json()
            .contains(r#""RERANK_MODEL":"\ud800""#));
        let catalogue = parse_installed(br#"[{"name":"reranker-\ud800"}]"#).ok_or("catalogue")?;
        assert!(owner
            .view_json(Some(&catalogue))
            .contains(r#""id":"llamacpp:reranker-\ud800""#));
        let choice = parse_input(br#"{"enabled":true,"choice":"llamacpp:reranker-\ud800"}"#)
            .ok_or("choice")?;
        assert!(owner.validate(&choice, Some(&catalogue)).is_ok());
        let input =
            parse_input(br#"{"enabled":false,"choice":"llamacpp:\ud800"}"#).ok_or("input")?;
        owner.saved = Some(
            owner
                .validate(&input, None)
                .map_err(|_| "disable must retain saved choice")?,
        );
        assert_eq!(
            owner.saved.as_ref().map(Saved::json).as_deref(),
            Some(r#"{"enabled":false,"choice":"llamacpp:\ud800"}"#)
        );
        Ok(())
    }
}
