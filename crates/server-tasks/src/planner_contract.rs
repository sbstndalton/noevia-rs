//! Planner artifact validation and executor brief. Constants are frozen existing role schemas
//! and instructions; no lifecycle or model execution lives in the asset.
use js_json::{wtf16, JValue};
use std::sync::OnceLock;
const LISTS: [&str; 7] = [
    "context",
    "constraints",
    "investigation",
    "capabilities",
    "approval_boundaries",
    "verification",
    "non_goals",
];
const KEYS: [&str; 10] = [
    "goal",
    "steps",
    "completion",
    "context",
    "constraints",
    "investigation",
    "capabilities",
    "approval_boundaries",
    "verification",
    "non_goals",
];
static CONTRACTS: OnceLock<Option<JValue>> = OnceLock::new();
pub fn contracts() -> Option<&'static JValue> {
    CONTRACTS
        .get_or_init(|| wtf16::parse(include_str!("../assets/role-contracts.json")).ok())
        .as_ref()
}
pub fn validate(plan: &JValue, capabilities: Option<&[String]>) -> Vec<String> {
    let JValue::Obj(fields) = plan else {
        return vec!["not_object".into()];
    };
    let mut problems = KEYS
        .iter()
        .filter(|k| plan.get(k) == &JValue::Undefined)
        .map(|k| format!("missing_{k}"))
        .collect::<Vec<_>>();
    if fields.iter().any(|(k, _)| !KEYS.contains(&k.as_str())) {
        problems.push("unexpected_key".into());
    }
    if !plan
        .get("goal")
        .as_str()
        .is_some_and(|s| !server_account::util::js_trim(s).is_empty())
    {
        problems.push("goal_not_text".into());
    }
    if plan.get("completion").as_str().is_none() {
        problems.push("completion_not_text".into());
    }
    for k in LISTS {
        if plan.get(k) != &JValue::Undefined
            && !matches!(plan.get(k),JValue::Arr(values) if values.iter().all(|v|v.as_str().is_some()))
        {
            problems.push(format!("{k}_not_list"));
        }
    }
    match plan.get("steps") {
        JValue::Arr(steps) if !steps.is_empty() && steps.len() <= 12 => {
            for (i, s) in steps.iter().enumerate() {
                if !s.is_object()
                    || s.get("n").as_num() != Some((i + 1) as f64)
                    || s.get("do").as_str().is_none()
                    || s.get("done_when").as_str().is_none()
                {
                    problems.push(format!("step_{}_malformed", i + 1));
                }
            }
        }
        _ => problems.push("steps_count".into()),
    }
    if let (Some(given), JValue::Arr(named)) = (capabilities, plan.get("capabilities")) {
        if named
            .iter()
            .filter_map(JValue::as_str)
            .any(|c| !given.iter().any(|g| g == c))
        {
            problems.push("unknown_capability".into());
        }
    }
    if wtf16::stringify(plan).is_none_or(|s| wtf16::len(&s).div_ceil(4) > 1200) {
        problems.push("too_long".into());
    }
    problems
}
fn js(value: &JValue) -> String {
    js_json::to_js_string(value).unwrap_or_default()
}
pub fn executor_brief(p: &JValue) -> String {
    let mut out = Vec::new();
    if p.get("role_instructions").truthy() {
        out.push(js(p.get("role_instructions")));
        out.push(String::new());
    }
    out.push(format!(
        "Task (revision {}):",
        if p.get("revision").is_nullish() {
            "1".into()
        } else {
            js(p.get("revision"))
        }
    ));
    out.push(if p.get("request").truthy() {
        js(p.get("request"))
    } else {
        String::new()
    });
    if p.get("project_instructions").truthy() {
        out.extend([
            String::new(),
            "Project instructions:".into(),
            js(p.get("project_instructions")),
        ]);
    }
    let plan = p.get("plan");
    if plan.get("goal").truthy() {
        out.extend([String::new(), format!("Goal: {}", js(plan.get("goal")))]);
    }
    if let JValue::Arr(steps) = plan.get("steps") {
        if !steps.is_empty() {
            out.extend([String::new(), "Plan:".into()]);
            for s in steps {
                out.push(format!(
                    "{}. {}{}",
                    js(s.get("n")),
                    js(s.get("do")),
                    if s.get("done_when").truthy() {
                        format!(" (done when: {})", js(s.get("done_when")))
                    } else {
                        String::new()
                    }
                ));
            }
        }
    }
    fn list(out: &mut Vec<String>, title: &str, items: &JValue) {
        if let JValue::Arr(items) = items {
            if !items.is_empty() {
                out.extend([String::new(), format!("{title}:")]);
                out.extend(items.iter().map(|i| format!("- {}", js(i))));
            }
        }
    }
    list(&mut out, "Constraints", plan.get("constraints"));
    list(&mut out, "Needs approval", plan.get("approval_boundaries"));
    list(&mut out, "Verify by", plan.get("verification"));
    list(&mut out, "Not part of this task", plan.get("non_goals"));
    if plan.get("completion").truthy() {
        out.extend([
            String::new(),
            format!("Complete when: {}", js(plan.get("completion"))),
        ]);
    }
    list(&mut out, "Fix from the last round", p.get("feedback"));
    if let JValue::Arr(caps) = p.get("capabilities") {
        if !caps.is_empty() {
            out.extend([
                String::new(),
                format!(
                    "Allowed capabilities: {}",
                    caps.iter()
                        .map(|c| if c.get("name").is_nullish() {
                            String::new()
                        } else {
                            js(c.get("name"))
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ]);
        }
    }
    out.join("\n")
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn node_validation_and_brief_vectors() {
        let f = wtf16::parse(include_str!(
            "../tests/fixtures/planner-contracts-node.json"
        ))
        .unwrap();
        let JValue::Arr(plans) = f.get("plans") else {
            panic!()
        };
        for p in plans {
            assert_eq!(
                JValue::Arr(
                    validate(p.get("input"), Some(&["edit_file".into()]))
                        .into_iter()
                        .map(JValue::Str)
                        .collect()
                ),
                *p.get("problems")
            );
        }
        let JValue::Arr(briefs) = f.get("briefs") else {
            panic!()
        };
        for b in briefs {
            assert_eq!(
                executor_brief(b.get("input")),
                b.get("brief").as_str().unwrap()
            );
        }
        assert!(contracts().unwrap().get("planSchema").is_object());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod verdict_tests {
    use super::*;
    #[test]
    fn reuses_existing_native_verdict_policy() {
        let f = wtf16::parse(include_str!(
            "../tests/fixtures/planner-contracts-node.json"
        ))
        .unwrap();
        let JValue::Arr(verdicts) = f.get("verdicts") else {
            panic!()
        };
        for v in verdicts {
            match server_projects::task_journals::read_review_verdict(v.get("input")) {
                Ok(read) => assert_eq!(read, *v.get("value")),
                Err(e) => assert_eq!(Some(e), v.get("error").as_str()),
            }
        }
    }
}
