//! Deployment configuration and task-facts projection. Runtime flags, network readiness and
//! approval cards are supplied by their live authorities; none is inferred from journal rows.
use crate::egress::Network;
use js_json::JValue;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Repository {
    pub id: String,
    pub path: PathBuf,
}
#[derive(Clone, Debug)]
pub struct CodeConfig {
    pub repositories: Vec<Repository>,
    pub sandboxed: bool,
    pub owner: Option<(u32, u32)>,
    pub tree_root: Option<PathBuf>,
    harness: String,
    harness_version: Option<String>,
}
fn string(s: &str) -> JValue {
    JValue::Str(s.into())
}
fn object(fields: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn strings(values: &[&str]) -> JValue {
    JValue::Arr(values.iter().map(|s| string(s)).collect())
}
impl CodeConfig {
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Self {
        let repos = parse_repositories(env("CODE_REPOS").as_deref().unwrap_or(""));
        let sandboxed = env("CODE_HARNESS_ENDPOINT").is_some_and(|s| !s.is_empty());
        Self::registered(repos, sandboxed, env)
    }
    fn registered(
        repositories: Vec<Repository>,
        sandboxed: bool,
        env: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let raw = env("CODE_HARNESS_NAME").unwrap_or_default();
        let name = server_account::util::js_trim(&raw);
        Self {
            repositories,
            sandboxed,
            owner: parse_user(env("CODE_HARNESS_USER").as_deref().unwrap_or("")),
            tree_root: env("CODE_WORKSPACE_ROOT")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            harness: if name.is_empty() {
                "opencode".into()
            } else {
                name.into()
            },
            harness_version: env("CODE_HARNESS_VERSION").filter(|s| !s.is_empty()),
        }
    }
    pub fn harnesses(&self) -> JValue {
        let label = match self.harness.as_str() {
            "opencode" => string("OpenCode"),
            "claude-code" => string("Claude Code"),
            "codex" => string("Codex"),
            "pi" => string("pi"),
            "qwen-code" => string("Qwen Code"),
            "deepseek" => string("DeepSeek Harness"),
            // Preserve the legacy object's inherited-property projection without executing one.
            "__proto__" => object(vec![]),
            "constructor"
            | "toString"
            | "toLocaleString"
            | "valueOf"
            | "hasOwnProperty"
            | "isPrototypeOf"
            | "propertyIsEnumerable"
            | "__defineGetter__"
            | "__defineSetter__"
            | "__lookupGetter__"
            | "__lookupSetter__" => JValue::Undefined,
            name => string(name),
        };
        JValue::Arr(vec![object(vec![
            ("id", string(&self.harness)),
            ("label", label),
            (
                "version",
                self.harness_version
                    .as_deref()
                    .map(string)
                    .unwrap_or(JValue::Null),
            ),
        ])])
    }
    pub fn facts(&self, network: Network, pipeline_enabled: bool, approvals: JValue) -> JValue {
        object(vec![
            (
                "repositories",
                JValue::Arr(
                    self.repositories
                        .iter()
                        .map(|r| object(vec![("id", string(&r.id))]))
                        .collect(),
                ),
            ),
            (
                "capabilities",
                strings(&[
                    "read_repository",
                    "edit_file",
                    "execute_command",
                    "install_dependency",
                    "network",
                    "delete",
                    "git_push",
                ]),
            ),
            (
                "defaultCapabilities",
                strings(&["read_repository", "edit_file", "execute_command"]),
            ),
            ("harnesses", self.harnesses()),
            (
                "promptPreparation",
                preparations(pipeline_enabled && self.sandboxed),
            ),
            ("sandboxed", JValue::Bool(self.sandboxed)),
            ("network", JValue::Bool(network == Network::Ready)),
            ("networkState", string(network.as_str())),
            ("approvals", approvals),
        ])
    }
}
pub fn browser_facts(network: Network, approvals: JValue) -> JValue {
    object(vec![
        ("capabilities", strings(&["open_browser"])),
        ("network", JValue::Bool(network == Network::Ready)),
        ("networkState", string(network.as_str())),
        ("approvals", approvals),
    ])
}
fn preparation(id: &str, label: &str, available: bool, reason: &str) -> JValue {
    object(vec![
        ("id", string(id)),
        ("label", string(label)),
        ("available", JValue::Bool(available)),
        ("reason", string(reason)),
    ])
}
fn preparations(pipeline: bool) -> JValue {
    let mut values=vec![
        preparation("direct","Direct",true,"Your request goes to the model as you wrote it."),
        preparation("local","Local architect",false,"Not offered yet: as an architect the 4B returned 0 of 18 usable execution prompts (list fields came back as strings). Direct stays the default until that changes."),
        preparation("frontier","Frontier architect",false,"Not built: it needs a provider, official authentication and the outbound allowlist enforced in code (spec §2)."),
    ];
    if pipeline {
        values.push(preparation("planner","Planner",true,"The Planner plans the task, the coding agent carries it out, the operator’s tests verify it and the Planner reviews it, at most twice more, before you accept."));
    }
    JValue::Arr(values)
}
pub fn parse_user(raw: &str) -> Option<(u32, u32)> {
    let (uid, gid) = server_account::util::js_trim(raw).split_once(':')?;
    let valid = |s: &str| (1..=7).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
    (valid(uid) && valid(gid))
        .then(|| Some((uid.parse().ok()?, gid.parse().ok()?)))
        .flatten()
}
pub fn parse_repositories(raw: &str) -> Vec<Repository> {
    let mut result: Vec<Repository> = vec![];
    for entry in raw.split(',') {
        let mut parts = server_account::util::js_trim(entry).split('|');
        let name = server_account::util::js_trim(parts.next().unwrap_or(""));
        let location = server_account::util::js_trim(parts.next().unwrap_or(""));
        if name.is_empty()
            || location.is_empty()
            || !Path::new(location).is_absolute()
            || result.iter().any(|r| r.id == name)
        {
            continue;
        }
        let Ok(path) = std::fs::canonicalize(location) else {
            continue;
        };
        if path.join(".git").exists() {
            result.push(Repository {
                id: name.into(),
                path,
            });
        }
    }
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use js_json::wtf16;
    #[test]
    fn deployment_facts_match_the_node_service_factory() {
        let fixture = wtf16::parse(include_str!("../tests/fixtures/facts-node.json")).unwrap();
        let JValue::Arr(cases) = fixture.get("cases") else {
            panic!()
        };
        assert!(cases.len() >= 14);
        for case in cases {
            let input = case.get("input");
            let JValue::Arr(repositories) = input.get("repositories") else {
                panic!()
            };
            let repositories = repositories
                .iter()
                .map(|r| Repository {
                    id: r.get("id").as_str().unwrap().into(),
                    path: PathBuf::from(r.get("path").as_str().unwrap()),
                })
                .collect();
            let config = CodeConfig::registered(
                repositories,
                input.get("sandboxed").as_bool().unwrap(),
                |key| input.get("env").get(key).as_str().map(str::to_owned),
            );
            let network = match input.get("network").as_str().unwrap() {
                "ready" => Network::Ready,
                "unavailable" => Network::Unavailable,
                _ => Network::Unconfigured,
            };
            let actual = config.facts(
                network,
                input.get("pipeline").as_bool().unwrap(),
                object(vec![]),
            );
            let wire = wtf16::parse(&wtf16::stringify(&actual).unwrap()).unwrap();
            assert_eq!(&wire, case.get("expected"));
        }
    }
    #[test]
    fn harness_user_is_the_existing_bounded_decimal_pair() {
        assert_eq!(parse_user(" 000:0123 "), Some((0, 123)));
        for input in ["1", "1:2:3", "-1:2", "10000000:2", "1:a", "1: 2"] {
            assert_eq!(parse_user(input), None);
        }
    }
}
