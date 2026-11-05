//! Code-task request normalization. Admission and resource claims happen only after this succeeds.
use crate::{
    egress::Network,
    facts::{CodeConfig, Repository},
};
use js_json::JValue;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}
#[derive(Debug)]
pub struct Request {
    pub repository: Repository,
    pub prompt: String,
    pub capabilities: Vec<String>,
    pub domains: Vec<String>,
    pub harness: String,
    pub preparation: String,
    pub model: Option<String>,
    pub approve_plan: bool,
}
fn refusal(status: u16, message: &str) -> Refusal {
    Refusal {
        status,
        message: message.into(),
    }
}
fn text(value: &JValue, default: &str) -> Result<String, Refusal> {
    if value.truthy() {
        js_json::to_js_string(value).map_err(|_| refusal(500, "Code request failed"))
    } else {
        Ok(default.into())
    }
}
impl Request {
    pub fn parse(
        config: &CodeConfig,
        network: Network,
        pipeline: bool,
        body: &JValue,
    ) -> Result<Self, Refusal> {
        if body == &JValue::Null {
            return Err(refusal(500, "Code request failed"));
        }
        let id = text(body.get("repository"), "")?;
        let repository = config
            .repositories
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| {
                refusal(
                    400,
                    "Choose a repository the operator registered on this server.",
                )
            })?;
        let raw = text(body.get("prompt"), "")?;
        let prompt = server_account::util::js_trim(&raw).to_owned();
        if prompt.is_empty() {
            return Err(refusal(400, "Describe the task."));
        }
        if js_json::wtf16::len(&prompt) > 8000 {
            return Err(refusal(400, "That task description is too long."));
        }
        let facts = config.facts(network, pipeline, JValue::Null);
        let JValue::Arr(grantable) = facts.get("capabilities") else {
            return Err(refusal(503, "Code mode is unavailable."));
        };
        let asked = match body.get("capabilities") {
            JValue::Arr(values) => values,
            _ => match facts.get("defaultCapabilities") {
                JValue::Arr(values) => values,
                _ => unreachable!(),
            },
        };
        let capabilities = grantable
            .iter()
            .filter(|c| asked.contains(c))
            .filter_map(|c| c.as_str())
            .filter(|c| {
                network != Network::Unconfigured || (*c != "network" && *c != "install_dependency")
            })
            .map(str::to_owned)
            .collect();
        let JValue::Arr(harnesses) = facts.get("harnesses") else {
            unreachable!()
        };
        let default_harness = harnesses
            .first()
            .and_then(|h| h.get("id").as_str())
            .unwrap_or("");
        let harness = text(body.get("harness"), default_harness)?;
        if !harnesses
            .iter()
            .any(|h| h.get("id").as_str() == Some(harness.as_str()))
        {
            return Err(refusal(
                400,
                "That coding harness is not configured on this server.",
            ));
        }
        let preparation = text(body.get("promptPreparation"), "direct")?;
        let JValue::Arr(preparations) = facts.get("promptPreparation") else {
            unreachable!()
        };
        let selected = preparations
            .iter()
            .find(|p| p.get("id").as_str() == Some(preparation.as_str()))
            .ok_or_else(|| refusal(400, "Unknown prompt preparation."))?;
        if selected.get("available") != &JValue::Bool(true) {
            return Err(refusal(
                409,
                selected
                    .get("reason")
                    .as_str()
                    .unwrap_or("Unknown prompt preparation."),
            ));
        }
        let domains = match body.get("domains") {
            JValue::Arr(values) => values
                .iter()
                .map(|v| text(v, ""))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|v| server_account::util::js_trim(&v).to_lowercase())
                .filter(|v| domain(v))
                .take(20)
                .collect(),
            _ => vec![],
        };
        let model = if body.get("model").truthy() {
            Some(text(body.get("model"), "")?)
        } else {
            None
        };
        Ok(Self {
            repository,
            prompt,
            capabilities,
            domains,
            harness,
            preparation,
            model,
            approve_plan: body.get("approvePlan") == &JValue::Bool(true),
        })
    }
}
fn domain(value: &str) -> bool {
    let Some((prefix, suffix)) = value.rsplit_once('.') else {
        return false;
    };
    !prefix.is_empty()
        && prefix
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-')
        && suffix.len() >= 2
        && suffix.bytes().all(|c| c.is_ascii_lowercase())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use js_json::wtf16;
    fn strings(values: &[String]) -> JValue {
        JValue::Arr(values.iter().map(|v| JValue::Str(v.clone())).collect())
    }
    #[test]
    fn code_start_normalization_matches_node_service() {
        let fixture = wtf16::parse(include_str!("../tests/fixtures/start-node.json")).unwrap();
        let JValue::Arr(cases) = fixture.get("cases") else {
            panic!()
        };
        assert_eq!(cases.len(), 60);
        for (index, case) in cases.iter().enumerate() {
            let input = case.get("input");
            let mut config = CodeConfig::from_env(|key| {
                (key == "CODE_HARNESS_ENDPOINT").then(|| "synthetic".into())
            });
            config.repositories.push(Repository {
                id: "repo".into(),
                path: "/synthetic/repo".into(),
            });
            let network = if input.get("network").as_str() == Some("ready") {
                Network::Ready
            } else {
                Network::Unconfigured
            };
            let actual = Request::parse(
                &config,
                network,
                input.get("pipeline") == &JValue::Bool(true),
                input.get("body"),
            );
            let expected = case.get("expected");
            match actual {
                Err(error) => {
                    assert_eq!(
                        JValue::Num(f64::from(error.status)),
                        *expected.get("status"),
                        "case {index}"
                    );
                    assert_eq!(
                        JValue::Str(error.message),
                        *expected.get("error"),
                        "case {index}"
                    );
                }
                Ok(request) => {
                    assert_eq!(*expected.get("status"), JValue::Num(202.0), "case {index}");
                    let execution = expected.get("execution");
                    assert_eq!(
                        execution.get("prompt").as_str(),
                        Some(request.prompt.as_str()),
                        "case {index}"
                    );
                    assert_eq!(
                        *execution.get("capabilities"),
                        strings(&request.capabilities),
                        "case {index}"
                    );
                    assert_eq!(
                        *execution.get("domains"),
                        strings(&request.domains),
                        "case {index}"
                    );
                    assert_eq!(
                        execution.get("harness").as_str(),
                        Some(request.harness.as_str()),
                        "case {index}"
                    );
                    assert_eq!(
                        execution.get("model").as_str(),
                        request.model.as_deref(),
                        "case {index}"
                    );
                    assert_eq!(
                        expected.get("result").get("repository").as_str(),
                        Some(request.repository.id.as_str()),
                        "case {index}"
                    );
                    assert_eq!(
                        expected.get("result").get("promptPreparation").as_str(),
                        Some(request.preparation.as_str()),
                        "case {index}"
                    );
                }
            }
        }
    }
}
