//! The Executor's brief (`renderExecutorBrief`) and the plan hash (`planHashOf`, role-context.cjs
//! `serializeProjection` hashed).

use crate::js::{join_part, tpl, units};
use crate::TypeError;
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};

/// `renderExecutorBrief(p)`: the Executor's prompt, rendered from its projection and nothing else.
/// Node's conversions are kept: values in `${}` go through `String()` (`undefined` and `null` as
/// words), the lines are joined as `Array.prototype.join` does (`null`/`undefined` as nothing).
/// `Err` where Node throws a TypeError: `p` `null`/`undefined`, a `null` plan step or capability,
/// or a value `String()` cannot convert (an object with its own `toString` key).
pub fn render_executor_brief(p: &JValue) -> Result<String, TypeError> {
    if p.is_nullish() {
        return Err(TypeError);
    }
    let mut out: Vec<String> = Vec::new();
    let role = p.get("role_instructions");
    if role.truthy() {
        out.push(join_part(role)?);
        out.push(String::new());
    }
    let revision = match p.get("revision") {
        r if r.is_nullish() => "1".to_string(),
        r => tpl(r)?,
    };
    out.push(format!("Task (revision {revision}):"));
    let request = p.get("request");
    out.push(if request.truthy() {
        join_part(request)?
    } else {
        String::new()
    });
    let instructions = p.get("project_instructions");
    if instructions.truthy() {
        out.extend([
            String::new(),
            "Project instructions:".into(),
            join_part(instructions)?,
        ]);
    }
    let empty = JValue::Obj(Vec::new());
    let plan = match p.get("plan") {
        v if v.truthy() => v,
        _ => &empty,
    };
    let goal = plan.get("goal");
    if goal.truthy() {
        out.extend([String::new(), format!("Goal: {}", tpl(goal)?)]);
    }
    if let JValue::Arr(steps) = plan.get("steps") {
        if !steps.is_empty() {
            out.extend([String::new(), "Plan:".into()]);
            for st in steps {
                if st.is_nullish() {
                    return Err(TypeError);
                }
                let done = st.get("done_when");
                let done = if done.truthy() {
                    format!(" (done when: {})", tpl(done)?)
                } else {
                    String::new()
                };
                out.push(format!(
                    "{}. {}{done}",
                    tpl(st.get("n"))?,
                    tpl(st.get("do"))?
                ));
            }
        }
    }
    let list = |out: &mut Vec<String>, title: &str, items: &JValue| -> Result<(), TypeError> {
        if let JValue::Arr(items) = items {
            if !items.is_empty() {
                out.extend([String::new(), format!("{title}:")]);
                for i in items {
                    out.push(format!("- {}", tpl(i)?));
                }
            }
        }
        Ok(())
    };
    list(&mut out, "Constraints", plan.get("constraints"))?;
    list(&mut out, "Needs approval", plan.get("approval_boundaries"))?;
    list(&mut out, "Verify by", plan.get("verification"))?;
    list(&mut out, "Not part of this task", plan.get("non_goals"))?;
    let completion = plan.get("completion");
    if completion.truthy() {
        out.extend([
            String::new(),
            format!("Complete when: {}", tpl(completion)?),
        ]);
    }
    list(&mut out, "Fix from the last round", p.get("feedback"))?;
    if let JValue::Arr(caps) = p.get("capabilities") {
        if !caps.is_empty() {
            let mut names = Vec::with_capacity(caps.len());
            for c in caps {
                if c.is_nullish() {
                    return Err(TypeError);
                }
                names.push(join_part(c.get("name"))?);
            }
            out.extend([
                String::new(),
                format!("Allowed capabilities: {}", names.join(", ")),
            ]);
        }
    }
    Ok(out.join("\n"))
}

/// role-context.cjs `canonicalize`: object keys sorted by UTF-16 code units (`Array.prototype.sort`)
/// and then laid out in JS property order (array-index keys first, ascending), `undefined`
/// properties dropped, arrays kept in order. An own `__proto__` key is dropped too: Node's
/// `out[key] = …` sets the copy's prototype instead, which `JSON.stringify` does not write.
pub fn canonicalize(v: &JValue) -> JValue {
    match v {
        JValue::Arr(xs) => JValue::Arr(xs.iter().map(canonicalize).collect()),
        JValue::Obj(items) => {
            let mut keyed: Vec<(Vec<u16>, &String, &JValue)> =
                items.iter().map(|(k, x)| (units(k), k, x)).collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out: Vec<(String, JValue)> = keyed
                .into_iter()
                .filter(|(_, k, x)| !matches!(x, JValue::Undefined) && k.as_str() != "__proto__")
                .map(|(_, k, x)| (k.clone(), canonicalize(x)))
                .collect();
            wtf16::order_keys(&mut out);
            JValue::Obj(out)
        }
        other => other.clone(),
    }
}

/// role-context.cjs `serializeProjection(v)`: `JSON.stringify(canonicalize(v))`; `None` for
/// `undefined` (JS: `undefined`).
pub fn serialize_projection(v: &JValue) -> Option<String> {
    wtf16::stringify(&canonicalize(v))
}

/// `planHashOf(plan)`: the sha256 (hex) of [`serialize_projection`]. `None` for `undefined`
/// (Node's hash throws a TypeError).
pub fn plan_hash(plan: &JValue) -> Option<String> {
    let text = serialize_projection(plan)?;
    let digest = Sha256::digest(text.as_bytes());
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}
