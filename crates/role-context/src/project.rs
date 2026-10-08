//! role-context.cjs's per-role allowlists (`ROLE_SPECS`) and field sanitisers, value for value.

use crate::text::{self, Budget};
use crate::{cred, Out, Refusal};
use prompt_framing::json::Value;
use std::collections::BTreeMap;

pub const TRUNCATION_MARK: &str = "…[truncated]";
pub const REDACTED: &str = "[redacted credential]";

pub mod caps {
    pub const REQUEST: usize = 4000;
    pub const ROLE_INSTRUCTIONS: usize = 4000;
    pub const PROJECT_INSTRUCTIONS: usize = 3000;
    pub const SHORT_TEXT: usize = 600;
    pub const LIST_ITEM: usize = 400;
    pub const LIST_ITEMS: usize = 12;
    pub const STEPS: usize = 12;
    pub const SNIPPET_TEXT: usize = 1000;
    pub const SNIPPETS: usize = 3;
    pub const CAPABILITIES: usize = 24;
    pub const CAPABILITY_DESCRIPTION: usize = 300;
    pub const IDENTIFIER: usize = 120;
    pub const CHANGED_FILES: usize = 50;
    pub const TEST_RESULTS: usize = 20;
    pub const STEP_RESULTS: usize = 12;
    pub const CHANGE_FILES: usize = 20;
    pub const CHANGE_PATCH: usize = 4000;
    pub const CHANGE_TOTAL: usize = 24000;
    pub const TOTAL: usize = 40000;
}

pub const ROLES: [&str; 3] = ["planner", "executor", "auditor"];
pub const SNIPPET_SOURCES: [&str; 3] = ["project", "selected", "repo-public"];
pub const APPROVAL_DECISIONS: [&str; 3] = ["approve", "deny", "approve_all"];
pub const PLAN_KEYS_EXECUTOR: [&str; 8] = [
    "goal",
    "steps",
    "constraints",
    "capabilities",
    "approval_boundaries",
    "verification",
    "completion",
    "non_goals",
];
pub const PLAN_KEYS_AUDITOR: [&str; 7] = [
    "goal",
    "steps",
    "constraints",
    "approval_boundaries",
    "verification",
    "completion",
    "non_goals",
];

const COMMON: [&str; 6] = [
    "role",
    "role_name",
    "task_id",
    "revision",
    "request",
    "role_instructions",
];

/// The roles `ROLE_SPECS` has, and each one's display name.
pub fn role_name(role: &str) -> Option<&'static str> {
    match role {
        "planner" | "reviewer" => Some("Planner"),
        "executor" => Some("Executor"),
        "auditor" => Some("Auditor"),
        _ => None,
    }
}

/// `Object.keys(ROLE_SPECS[role])`, in spec order.
pub fn spec_keys(role: &str) -> Option<Vec<&'static str>> {
    let own: &[&'static str] = match role {
        "planner" => &[
            "project_instructions",
            "snippets",
            "capabilities",
            "constraints",
            "context_limit",
        ],
        "executor" => &[
            "project_instructions",
            "plan",
            "snippets",
            "capabilities",
            "feedback",
        ],
        "auditor" => &["plan", "lifecycle_state", "execution", "approval_outcomes"],
        "reviewer" => &["plan", "capabilities", "execution", "change"],
        _ => return None,
    };
    Some(COMMON.iter().chain(own).copied().collect())
}

pub fn u(s: &str) -> Vec<u16> {
    text::units(s)
}

fn str_eq(v: Option<&Value>, s: &str) -> bool {
    v.and_then(Value::as_str)
        .is_some_and(|x| x.iter().copied().eq(s.encode_utf16()))
}

pub fn is_obj(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Obj(_)))
}

/// `capText(value, max)`.
pub fn cap_text(value: Option<&Value>, max: usize) -> Option<Vec<u16>> {
    let s = value?.as_str()?;
    Some(cap_units(s, max))
}

pub fn cap_units(s: &[u16], max: usize) -> Vec<u16> {
    let t = text::nfc(s);
    if text::cp_len(&t) <= max {
        return t;
    }
    let keep = max.saturating_sub(12);
    let mut out = text::cp_prefix(&t, keep).to_vec();
    out.extend(u(TRUNCATION_MARK));
    out
}

/// `capIdentifier(value)`.
fn cap_identifier(value: Option<&Value>) -> Option<Vec<u16>> {
    if let Some(Value::Num(n)) = value {
        if n.is_finite() {
            return Some(u(&tool_exchange::js_number(*n)));
        }
    }
    cap_text(value, caps::IDENTIFIER)
}

/// `capList(value, maxItems, maxChars)`.
fn cap_list(value: Option<&Value>, max_items: usize, max_chars: usize) -> Option<Out> {
    let Some(Value::Arr(items)) = value else {
        return None;
    };
    let mut out = Vec::new();
    for item in items {
        if out.len() >= max_items {
            break;
        }
        if let Some(t) = cap_text(Some(item), max_chars) {
            out.push(Out::Str(t));
        }
    }
    Some(Out::Arr(out))
}

/// `capInt(value, { min, max })`.
fn cap_int(value: Option<&Value>, min: i64, max: i64) -> Option<i64> {
    let Some(Value::Num(n)) = value else {
        return None;
    };
    if !n.is_finite() || n.fract() != 0.0 {
        return None;
    }
    // Clamp in f64 (exact for these bounds), then convert.
    let c = n.max(min as f64).min(max as f64);
    Some(c as i64)
}

fn obj(pairs: Vec<(&str, Option<Out>)>) -> Out {
    let mut m = BTreeMap::new();
    for (k, v) in pairs {
        if let Some(v) = v {
            m.insert(u(k), v);
        }
    }
    Out::Obj(m)
}

fn get<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    v?.get(key)
}

fn cap_steps(value: Option<&Value>) -> Option<Out> {
    let Some(Value::Arr(items)) = value else {
        return None;
    };
    let mut out = Vec::new();
    for step in items {
        if out.len() >= caps::STEPS {
            break;
        }
        if !matches!(step, Value::Obj(_)) {
            continue;
        }
        let Some(do_text) = cap_text(step.get("do"), caps::SHORT_TEXT) else {
            continue;
        };
        let n = out.len() as i64 + 1;
        out.push(obj(vec![
            ("n", Some(Out::Int(n))),
            ("do", Some(Out::Str(do_text))),
            (
                "done_when",
                cap_text(step.get("done_when"), caps::SHORT_TEXT).map(Out::Str),
            ),
        ]));
    }
    Some(Out::Arr(out))
}

fn cap_capabilities(value: Option<&Value>) -> Option<Out> {
    let Some(Value::Arr(items)) = value else {
        return None;
    };
    let mut by_name: BTreeMap<Vec<u16>, Out> = BTreeMap::new();
    for cap in items {
        let is_o = matches!(cap, Value::Obj(_));
        let name = cap_identifier(if is_o { cap.get("name") } else { Some(cap) });
        let Some(name) = name.filter(|n| !n.is_empty()) else {
            continue;
        };
        if by_name.contains_key(&name) {
            continue;
        }
        let description = if is_o {
            cap_text(cap.get("description"), caps::CAPABILITY_DESCRIPTION)
        } else {
            None
        };
        let mut m = BTreeMap::new();
        m.insert(u("name"), Out::Str(name.clone()));
        if let Some(d) = description {
            m.insert(u("description"), Out::Str(d));
        }
        by_name.insert(name, Out::Obj(m));
    }
    // BTreeMap order is the default sort of the names (UTF-16 code units).
    Some(Out::Arr(
        by_name.into_values().take(caps::CAPABILITIES).collect(),
    ))
}

fn cap_snippets(value: Option<&Value>, tenant: &[u16]) -> Option<Out> {
    let Some(Value::Arr(items)) = value else {
        return None;
    };
    let mut out = Vec::new();
    for snip in items {
        if out.len() >= caps::SNIPPETS {
            break;
        }
        if !matches!(snip, Value::Obj(_)) {
            continue;
        }
        let Some(source) = SNIPPET_SOURCES
            .iter()
            .find(|s| str_eq(snip.get("source"), s))
        else {
            continue;
        };
        if snip.get("tenantId").and_then(Value::as_str) != Some(tenant) {
            continue;
        }
        let Some(t) = cap_text(snip.get("text"), caps::SNIPPET_TEXT) else {
            continue;
        };
        out.push(obj(vec![
            ("source", Some(Out::Str(u(source)))),
            ("text", Some(Out::Str(t))),
            (
                "label",
                cap_text(snip.get("label"), caps::IDENTIFIER).map(Out::Str),
            ),
        ]));
    }
    Some(Out::Arr(out))
}

fn cap_plan(plan: Option<&Value>, keys: &[&str]) -> Option<Out> {
    if !is_obj(plan) {
        return None;
    }
    let mut pairs = Vec::new();
    for &key in keys {
        let v = match key {
            "goal" | "completion" => cap_text(get(plan, key), caps::SHORT_TEXT).map(Out::Str),
            "steps" => cap_steps(get(plan, "steps")),
            "capabilities" => cap_list(get(plan, key), caps::CAPABILITIES, caps::IDENTIFIER),
            _ => cap_list(get(plan, key), caps::LIST_ITEMS, caps::LIST_ITEM),
        };
        pairs.push((key, v));
    }
    Some(obj(pairs))
}

fn sha(v: Option<&Value>) -> Option<Vec<u16>> {
    let s = v?.as_str()?;
    ((7..=64).contains(&s.len()) && s.iter().all(|&c| matches!(c, 0x30..=0x39 | 0x61..=0x66)))
        .then(|| s.to_vec())
}

fn cap_test_results(value: Option<&Value>) -> Option<Out> {
    let Some(Value::Arr(items)) = value else {
        return None;
    };
    let mut out = Vec::new();
    for t in items {
        if out.len() >= caps::TEST_RESULTS {
            break;
        }
        if !matches!(t, Value::Obj(_)) {
            continue;
        }
        let Some(name) = cap_text(t.get("name"), caps::IDENTIFIER) else {
            continue;
        };
        let Some(Value::Bool(passed)) = t.get("passed") else {
            continue;
        };
        out.push(obj(vec![
            ("name", Some(Out::Str(name))),
            ("passed", Some(Out::Bool(*passed))),
        ]));
    }
    Some(Out::Arr(out))
}

fn cap_step_results(value: Option<&Value>) -> Option<Out> {
    let Some(Value::Arr(items)) = value else {
        return None;
    };
    let mut out = Vec::new();
    for s in items {
        if out.len() >= caps::STEP_RESULTS {
            break;
        }
        if !matches!(s, Value::Obj(_)) {
            continue;
        }
        let n = cap_int(s.get("n"), 1, caps::STEPS as i64);
        let status = ["done", "skipped", "failed"]
            .into_iter()
            .find(|st| str_eq(s.get("status"), st));
        let (Some(n), Some(status)) = (n, status) else {
            continue;
        };
        out.push(obj(vec![
            ("n", Some(Out::Int(n))),
            ("status", Some(Out::Str(u(status)))),
            (
                "note",
                cap_text(s.get("note"), caps::SHORT_TEXT).map(Out::Str),
            ),
        ]));
    }
    Some(Out::Arr(out))
}

fn cap_execution(e: Option<&Value>) -> Option<Out> {
    if !is_obj(e) {
        return None;
    }
    Some(obj(vec![
        (
            "summary",
            cap_text(get(e, "summary"), caps::SHORT_TEXT).map(Out::Str),
        ),
        ("head_sha", sha(get(e, "headSha")).map(Out::Str)),
        (
            "changed_files",
            cap_list(
                get(e, "changedFiles"),
                caps::CHANGED_FILES,
                caps::IDENTIFIER * 2,
            ),
        ),
        ("test_results", cap_test_results(get(e, "testResults"))),
        ("step_results", cap_step_results(get(e, "stepResults"))),
    ]))
}

/// `fitSerialised(value, max)`: the text and whether it was cut.
fn fit_serialised(value: &[u16], max: usize) -> (Vec<u16>, bool) {
    let t = text::nfc(value);
    if text::serialised_cost(&t) <= max {
        return (t, false);
    }
    let room = max as i64 - text::serialised_cost(&u(TRUNCATION_MARK)) as i64;
    let mut kept = Vec::new();
    let mut used: i64 = 0;
    let mut i = 0;
    while i < t.len() {
        let n = if t.get(i).is_some_and(|&c| text::is_high(c))
            && t.get(i + 1).is_some_and(|&d| text::is_low(d))
        {
            2
        } else {
            1
        };
        let point = t.get(i..i + n).unwrap_or(&[]);
        let cost = text::serialised_cost(point) as i64;
        if used + cost > room {
            break;
        }
        kept.extend(point);
        used += cost;
        i += n;
    }
    kept.extend(u(TRUNCATION_MARK));
    (kept, true)
}

fn cap_change(change: Option<&Value>) -> Option<Out> {
    if !is_obj(change) {
        return None;
    }
    let mark_cost = text::serialised_cost(&u(TRUNCATION_MARK));
    let overhead = text::serialised_cost(&u("{\"path\":\"\",\"patch\":\"\"},"));
    let mut truncated = matches!(get(change, "truncated"), Some(Value::Bool(true)));
    let mut files = Vec::new();
    let mut budget = caps::CHANGE_TOTAL;
    let list: &[Value] = match get(change, "files") {
        Some(Value::Arr(xs)) => xs,
        _ => &[],
    };
    for file in list {
        if files.len() >= caps::CHANGE_FILES {
            truncated = true;
            break;
        }
        if !matches!(file, Value::Obj(_)) {
            continue;
        }
        let Some(path) = file.get("path").and_then(Value::as_str) else {
            continue;
        };
        let (path, _) = fit_serialised(path, caps::IDENTIFIER * 2);
        let path_cost = text::serialised_cost(&path) + overhead;
        if path_cost > budget {
            truncated = true;
            break;
        }
        budget -= path_cost;
        let room = caps::CHANGE_PATCH.min(budget);
        let mut entry = BTreeMap::new();
        entry.insert(u("path"), Out::Str(path));
        match file.get("patch").and_then(Value::as_str) {
            Some(patch) if room > mark_cost => {
                let (p, cut) = fit_serialised(patch, room);
                truncated |= cut;
                budget = budget.saturating_sub(text::serialised_cost(&p));
                entry.insert(u("patch"), Out::Str(p));
            }
            Some(_) => truncated = true,
            None => {}
        }
        files.push(Out::Obj(entry));
    }
    if list.len() > caps::CHANGE_FILES {
        truncated = true;
    }
    Some(obj(vec![
        ("files", Some(Out::Arr(files))),
        ("truncated", Some(Out::Bool(truncated))),
        ("base_sha", sha(get(change, "baseSha")).map(Out::Str)),
        ("head_sha", sha(get(change, "headSha")).map(Out::Str)),
    ]))
}

fn approval_outcomes(approvals: Option<&Value>) -> Out {
    let mut counts = [0i64; 3];
    if let Some(Value::Arr(items)) = approvals {
        for a in items {
            if !matches!(a, Value::Obj(_)) {
                continue;
            }
            if let Some(i) = APPROVAL_DECISIONS
                .iter()
                .position(|d| str_eq(a.get("decision"), d))
            {
                if let Some(c) = counts.get_mut(i) {
                    *c += 1;
                }
            }
        }
    }
    let [a, d, all] = counts;
    obj(vec![
        ("approve", Some(Out::Int(a))),
        ("deny", Some(Out::Int(d))),
        ("approve_all", Some(Out::Int(all))),
    ])
}

/// `leafStrings(value)`: strings of at least 8 code units, at most 32 levels down.
pub fn leaf_strings<'a>(value: Option<&'a Value>, out: &mut Vec<&'a [u16]>, depth: usize) {
    if depth > 32 {
        return;
    }
    match value {
        Some(Value::Str(s)) if s.len() >= 8 => out.push(s),
        Some(Value::Arr(xs)) => {
            for x in xs {
                leaf_strings(Some(x), out, depth + 1);
            }
        }
        Some(Value::Obj(m)) => {
            for (_, v) in m {
                leaf_strings(Some(v), out, depth + 1);
            }
        }
        _ => {}
    }
}

/// `credentialValues(state)`.
pub fn credential_values(state: &Value) -> Vec<&[u16]> {
    let mut out = Vec::new();
    for k in ["credentials", "secrets", "tokens"] {
        leaf_strings(state.get(k), &mut out, 1);
    }
    out
}

/// `redactCredentials(text, known, counter)` for a string.
pub fn redact(
    s: &[u16],
    known: &[&[u16]],
    budget: &mut Budget,
) -> Result<(Vec<u16>, usize), Refusal> {
    let redacted = u(REDACTED);
    let once = |input: &[u16], budget: &mut Budget| -> Result<(Vec<u16>, usize), Refusal> {
        let mut out = input.to_vec();
        let mut n = 0;
        for value in known {
            let (o, k) = text::replace_all(&out, value, &redacted, budget)?;
            out = o;
            n += k;
        }
        for p in 0..cred::PATTERNS {
            let (o, k) = cred::replace(p, &out, &redacted, budget)?;
            out = o;
            n += k;
        }
        Ok((out, n))
    };
    let (mut out, mut n) = once(s, budget)?;
    let normal = text::strip_format(&text::nfkc(&text::strip_format(&out)));
    if normal != out {
        let (o2, n2) = once(&normal, budget)?;
        if n2 > 0 {
            out = o2;
            n += n2;
        }
    }
    Ok((out, n))
}

/// What a field reads beyond the state: the role, the task's tenant and the known credentials.
pub struct Ctx<'a> {
    pub role: &'a str,
    pub tenant: &'a [u16],
    pub known: Vec<&'a [u16]>,
}

/// `ROLE_SPECS[role][key](state, ctx)`, and the credentials it redacted.
pub fn field(
    key: &str,
    state: &Value,
    ctx: &Ctx<'_>,
    budget: &mut Budget,
) -> Result<(Option<Out>, usize), Refusal> {
    let s = |k: &str| state.get(k);
    let mut redactions = 0;
    let mut redacted_text =
        |v: Option<&Value>, max: usize, budget: &mut Budget| -> Result<Option<Out>, Refusal> {
            let Some(t) = v.and_then(Value::as_str) else {
                return Ok(None);
            };
            let (r, n) = redact(t, &ctx.known, budget)?;
            redactions += n;
            Ok(Some(Out::Str(cap_units(&r, max))))
        };
    let v = match key {
        "role" => Some(Out::Str(u(ctx.role))),
        "role_name" => role_name(ctx.role).map(|n| Out::Str(u(n))),
        "task_id" => cap_identifier(s("taskId")).map(Out::Str),
        "revision" => cap_identifier(s("revision")).map(Out::Str),
        "request" => redacted_text(s("request"), caps::REQUEST, budget)?,
        "role_instructions" => match s("roleSystemPrompts") {
            Some(p @ Value::Obj(_)) => {
                cap_text(p.get(ctx.role), caps::ROLE_INSTRUCTIONS).map(Out::Str)
            }
            _ => None,
        },
        "project_instructions" => {
            redacted_text(s("projectInstructions"), caps::PROJECT_INSTRUCTIONS, budget)?
        }
        "snippets" => cap_snippets(s("snippets"), ctx.tenant),
        "capabilities" => cap_capabilities(s("capabilities")),
        "constraints" => cap_list(s("constraints"), caps::LIST_ITEMS, caps::LIST_ITEM),
        "context_limit" => cap_int(s("contextLimit"), 0, 10_000_000).map(Out::Int),
        "plan" => {
            if ctx.role == "executor" {
                cap_plan(s("plan"), &PLAN_KEYS_EXECUTOR)
            } else {
                cap_plan(s("plan"), &PLAN_KEYS_AUDITOR)
            }
        }
        "feedback" => cap_list(s("feedback"), caps::LIST_ITEMS, caps::SHORT_TEXT),
        "lifecycle_state" => cap_identifier(s("lifecycleState")).map(Out::Str),
        "execution" => cap_execution(s("execution")),
        "approval_outcomes" => Some(approval_outcomes(s("approvals"))),
        "change" => cap_change(s("change")),
        _ => None,
    };
    Ok((v, redactions))
}
