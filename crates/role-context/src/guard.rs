//! role-context.cjs's leak guard (`guardProjection`): the sensitive values a state carries, by
//! class, and whether a projection holds one whole (in any of `haystacks`' forms) or a copied
//! excerpt of at least 63 folded code points, plus the credential patterns.

use crate::project::{self, leaf_strings, u};
use crate::text::{self, Budget, Needle};
use crate::{cred, Out, Refusal};
use prompt_framing::json::Value;
use std::collections::HashSet;

pub const EXCERPT_WINDOW: usize = 48;
pub const EXCERPT_STRIDE: usize = 16;
const ORCHESTRATOR_SENSITIVE_KEYS: [&str; 5] = [
    "metaPrompt",
    "systemPrompt",
    "routing",
    "notes",
    "scratchpad",
];
const APPROVAL_SENSITIVE_KEYS: [&str; 4] = ["id", "token", "userId", "chatId"];

/// The leak classes, in the order the JS reports them (sorted names, then the pattern class).
pub const CLASSES: [&str; 8] = [
    "approval_internals",
    "credentials",
    "diary",
    "orchestrator",
    "other_role_prompts",
    "other_tenant_ids",
    "other_tenants",
    "credential_pattern",
];

fn never_exempt(name: &str) -> bool {
    matches!(
        name,
        "credentials" | "other_tenant_ids" | "approval_internals"
    )
}

/// `VOCABULARY`: the folded words this module writes itself.
fn vocabulary() -> HashSet<Vec<u16>> {
    let mut words: Vec<&str> = Vec::new();
    words.extend(project::ROLES);
    words.extend(["Planner", "Executor", "Auditor"]);
    words.extend(project::APPROVAL_DECISIONS);
    words.extend(project::SNIPPET_SOURCES);
    for role in ["planner", "executor", "auditor", "reviewer"] {
        words.extend(project::spec_keys(role).unwrap_or_default());
    }
    words.extend(project::PLAN_KEYS_EXECUTOR);
    words.extend([
        "done_when",
        "head_sha",
        "changed_files",
        "test_results",
        "step_results",
        "summary",
        "base_sha",
        "files",
        "patch",
        "path",
        "truncated",
        "done",
        "skipped",
        "failed",
        "label",
        "source",
        "text",
        "name",
        "description",
        "passed",
        "note",
    ]);
    words.into_iter().map(|w| text::fold(&u(w))).collect()
}

/// One class's values (owned: other-tenant ids may be numbers written as text).
type Class = (&'static str, Vec<Vec<u16>>);

/// `sensitiveClasses(state, role)`, sorted by class name.
fn sensitive_classes(state: &Value, role: &str, tenant: &[u16]) -> Vec<Class> {
    let prose = |vs: Vec<&[u16]>| -> Vec<Vec<u16>> {
        vs.into_iter()
            .filter(|v| !text::is_structured_token(v))
            .map(<[u16]>::to_vec)
            .collect()
    };
    let owned =
        |vs: Vec<&[u16]>| -> Vec<Vec<u16>> { vs.into_iter().map(<[u16]>::to_vec).collect() };

    let mut orch = Vec::new();
    if let Some(o @ Value::Obj(_)) = state.get("orchestrator") {
        for k in ORCHESTRATOR_SENSITIVE_KEYS {
            leaf_strings(o.get(k), &mut orch, 1);
        }
    }
    let mut prompts = Vec::new();
    if let Some(Value::Obj(m)) = state.get("roleSystemPrompts") {
        for (k, v) in m {
            if !k.iter().copied().eq(role.encode_utf16()) {
                leaf_strings(Some(v), &mut prompts, 1);
            }
        }
    }
    let credentials = owned(project::credential_values(state));
    let mut diary = Vec::new();
    leaf_strings(state.get("diary"), &mut diary, 0);
    let mut others = Vec::new();
    leaf_strings(state.get("otherTenants"), &mut others, 0);
    let mut approvals = Vec::new();
    if let Some(Value::Arr(xs)) = state.get("approvals") {
        for a in xs {
            if !matches!(a, Value::Obj(_)) {
                continue;
            }
            for k in APPROVAL_SENSITIVE_KEYS {
                leaf_strings(a.get(k), &mut approvals, 2);
            }
            if let Some(card @ Value::Obj(_)) = a.get("card") {
                for k in APPROVAL_SENSITIVE_KEYS {
                    leaf_strings(card.get(k), &mut approvals, 2);
                }
            }
        }
    }
    let mut ids: Vec<Vec<u16>> = Vec::new();
    if let Some(Value::Obj(m)) = state.get("otherTenants") {
        ids.extend(m.iter().map(|(k, _)| k.clone()));
    }
    if let Some(Value::Arr(snips)) = state.get("snippets") {
        // Other tenants' snippet texts, then the Diary's, as the JS pushes them.
        for s in snips {
            if matches!(s, Value::Obj(_)) {
                if let Some(t) = s.get("tenantId") {
                    if t.as_str() != Some(tenant) {
                        leaf_strings(s.get("text"), &mut others, 1);
                    }
                }
            }
        }
        for s in snips {
            if let Some(src) = s.get("source").and_then(Value::as_str) {
                if matches!(s, Value::Obj(_))
                    && text::trim(&text::fold(src)) == u("diary").as_slice()
                {
                    leaf_strings(s.get("text"), &mut diary, 1);
                }
            }
        }
        for s in snips {
            if !matches!(s, Value::Obj(_)) {
                continue;
            }
            match s.get("tenantId") {
                Some(Value::Str(t)) if t.as_slice() != tenant => ids.push(t.clone()),
                Some(Value::Num(n)) => ids.push(u(&tool_exchange::js_number(*n))),
                _ => {}
            }
        }
    }
    let ids: Vec<Vec<u16>> = ids.into_iter().filter(|id| id.len() >= 3).collect();

    let vocab = vocabulary();
    let keep = |vs: Vec<Vec<u16>>| -> Vec<Vec<u16>> {
        vs.into_iter()
            .filter(|v| !vocab.contains(&text::fold(v)))
            .collect()
    };
    vec![
        ("approval_internals", keep(owned(approvals))),
        ("credentials", credentials),
        ("diary", keep(owned(diary))),
        ("orchestrator", keep(prose(orch))),
        ("other_role_prompts", keep(prose(prompts))),
        ("other_tenant_ids", keep(ids)),
        ("other_tenants", keep(owned(others))),
    ]
}

/// `collectStrings(projection)`: every string and key, in order.
fn collect<'a>(v: &'a Out, out: &mut Vec<&'a [u16]>) {
    match v {
        Out::Str(s) => out.push(s),
        Out::Arr(xs) => xs.iter().for_each(|x| collect(x, out)),
        Out::Obj(m) => {
            for (k, x) in m {
                out.push(k);
                collect(x, out);
            }
        }
        Out::Int(_) | Out::Bool(_) => {}
    }
}

/// A projection's searchable forms (`haystacks`), deduplicated (only membership matters).
pub struct Hay {
    hays: Vec<Vec<u16>>,
    windows: Vec<u32>,
}

impl Hay {
    pub fn new(projection: &Out) -> Result<Self, Refusal> {
        let serialized = text::units(&crate::serialize(projection));
        let mut strings = Vec::new();
        collect(projection, &mut strings);
        let mut base: Vec<Vec<u16>> = vec![text::escape_non_ascii(&serialized), serialized];
        base.extend(strings.iter().map(|s| s.to_vec()));
        let mut expanded = Vec::new();
        for h in base {
            let d = text::decode_literal_escapes(&h);
            if d != h {
                expanded.push(d);
            }
            expanded.push(h);
        }
        // Decoding literal \u escapes can write a lone surrogate, whose NFKC, case and \p
        // treatment is ICU's business: refuse rather than guess.
        if expanded.iter().any(|h| text::has_lone_surrogate(h)) {
            return Err(Refusal::Ambiguous);
        }
        let stripped: Vec<Vec<u16>> = expanded
            .iter()
            .map(|h| text::strip_format(h))
            .zip(&expanded)
            .filter(|(s, h)| s != *h)
            .map(|(s, _)| s)
            .collect();
        let mut all = expanded;
        all.extend(stripped);
        let mut set: HashSet<Vec<u16>> = HashSet::new();
        let mut hays = Vec::new();
        for h in all {
            let n = text::strip_format(&text::nfkc(&h));
            let f = text::to_lower(&n);
            for x in [h, n, f] {
                if set.insert(x.clone()) {
                    hays.push(x);
                }
            }
        }
        // The folded projection the excerpt windows are cut from.
        let mut folded: Vec<u16> = Vec::new();
        for (i, h) in strings.iter().enumerate() {
            if i > 0 {
                folded.push(0);
            }
            folded.extend(text::fold(h));
            let d = text::decode_literal_escapes(h);
            if d.as_slice() != *h {
                folded.push(0);
                folded.extend(text::fold(&d));
            }
        }
        if text::has_lone_surrogate(&folded) {
            return Err(Refusal::Ambiguous);
        }
        Ok(Hay {
            hays,
            windows: text::code_points(&folded),
        })
    }

    fn has(&self, needle: &[u16], budget: &mut Budget) -> Result<bool, Refusal> {
        let n = Needle::new(needle);
        for h in &self.hays {
            if n.found_in(h, budget)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The boundary form: `(?<![\p{L}\p{N}_])needle(?![\p{L}\p{N}_])` (u mode).
    fn has_word(&self, needle: &[u16], budget: &mut Budget) -> Result<bool, Refusal> {
        let n = Needle::new(needle);
        for h in &self.hays {
            let found = n.scan(h, true, budget, |i| {
                !text::cp_before(h, i).is_some_and(text::is_word)
                    && !text::cp_at(h, i + needle.len()).is_some_and(text::is_word)
            })?;
            if found {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// `needles(item)`.
fn needles(item: &[u16]) -> Vec<Vec<u16>> {
    let bare = text::strip_format(item);
    let lowered = text::to_lower(&text::strip_format(&text::nfkc(&bare)));
    let mut out: Vec<Vec<u16>> = Vec::new();
    for n in [
        item.to_vec(),
        bare,
        text::escape_non_ascii(item),
        text::json_body(item),
        lowered,
    ] {
        if !n.is_empty() && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

fn window_set(points: &[u32]) -> HashSet<&[u32]> {
    points.windows(EXCERPT_WINDOW).collect()
}

/// `excerptLeaked(value, projectionWindows, trustedWindows)` over a folded value.
fn excerpt_leaked(folded: &[u16], projection: &HashSet<&[u32]>, trusted: &HashSet<&[u32]>) -> bool {
    let points = text::code_points(folded);
    if points.len() < EXCERPT_WINDOW {
        return false;
    }
    let hit = |i: usize| {
        points
            .get(i..i + EXCERPT_WINDOW)
            .is_some_and(|w| projection.contains(w) && !trusted.contains(w))
    };
    let mut i = 0;
    while i + EXCERPT_WINDOW <= points.len() {
        if hit(i) {
            return true;
        }
        i += EXCERPT_STRIDE;
    }
    hit(points.len() - EXCERPT_WINDOW)
}

/// `guardProjection(projection, state, role)`: the leaked classes (empty when none).
pub fn guard(
    projection: &Out,
    hay: &Hay,
    state: &Value,
    role: &str,
    tenant: &[u16],
    budget: &mut Budget,
) -> Result<Vec<&'static str>, Refusal> {
    let mut leaked = Vec::new();
    let classes = sensitive_classes(state, role, tenant);
    let projection_windows = window_set(&hay.windows);
    let folded_own = match projection {
        Out::Obj(m) => match m.get(&u("role_instructions")) {
            Some(Out::Str(s)) => text::fold(s),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    let own_points = text::code_points(&folded_own);
    let trusted = window_set(&own_points);
    let no_trust = HashSet::new();
    for (name, values) in classes {
        let exemptable = !never_exempt(name);
        let mut kept = Vec::new();
        for v in values {
            let f = text::fold(&v);
            budget.spend(v.len() + f.len())?;
            if !exemptable || !text::includes(&folded_own, &f, budget)? {
                kept.push((v, f));
            }
        }
        if kept.is_empty() {
            continue;
        }
        let mut whole = false;
        'values: for (v, _) in &kept {
            for n in needles(v) {
                let hit = if name == "other_tenant_ids" {
                    hay.has_word(&n, budget)?
                } else {
                    hay.has(&n, budget)?
                };
                if hit {
                    whole = true;
                    break 'values;
                }
            }
        }
        let excerpt = !whole
            && kept.iter().any(|(_, f)| {
                excerpt_leaked(
                    f,
                    &projection_windows,
                    if exemptable { &trusted } else { &no_trust },
                )
            });
        if whole || excerpt {
            leaked.push(name);
        }
    }
    'pattern: for h in &hay.hays {
        for p in 0..cred::PATTERNS {
            if cred::test(p, h, budget)? {
                leaked.push("credential_pattern");
                break 'pattern;
            }
        }
    }
    Ok(leaked)
}
