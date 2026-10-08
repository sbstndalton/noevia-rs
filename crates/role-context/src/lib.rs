//! noevia-core's `server/role-context.cjs` in Rust, exported from `dav-parse.wasm` as
//! `role_context` (ROLE_CONTEXT_IMPL): the per-role context projection of the multi-agent pipeline
//! (#511/#515, #519 reviewer, #702 shared dossier). Given a role (or roles) and the task state it
//! answers exactly what `projectRoleContext` / `projectSharedDossier` would: the projection built
//! field by field from the role's allowlist, with the same caps, credential redaction, total-size
//! refusal and leak guard (sensitive values from the state in any searched form, copied excerpts of
//! at least 63 folded code points, credential patterns).
//!
//! The host (role-context.cjs) always computes the JS answer and returns it; under
//! ROLE_CONTEXT_IMPL=wasm it also asks this port and refuses the context (as a leak) unless the port
//! agrees byte for byte. So the port can only remove context, never add any.
//!
//! # Wire format
//!
//! Input: `u8(op)` then UTF-8 JSON (at most [`MAX_INPUT_BYTES`]):
//! - op 1 `[role, state]`: projectRoleContext. Reply `{"projection":{…},"redactions":n}`.
//! - op 2 `[roles, state]`: projectSharedDossier. Reply `{"dossier":{…},"redactions":n}`.
//!
//! An outcome the JS throws is a status-0 reply too: `{"refused":"unknown_role"|"invalid_state"|
//! "missing_tenant"|"invalid_tenant"|"too_large"}` or `{"leak":["class",…]}` (the classes of
//! `RoleContextLeakError`, in its order). Status 1 is the port's own refusal:
//! `{"error":"input"|"too_large"|"ambiguous"}`; the host then fails closed.
//!
//! # Stricter than the JS (refused as `ambiguous` or `too_large`)
//!
//! - a lone surrogate in any string or key of the request, or one written by decoding a literal
//!   `\uXXXX` escape in the projection (ICU decides how NFKC, case mapping and `\p{…}` treat it);
//! - a request over [`MAX_INPUT_BYTES`], or one whose leak search would take more than
//!   [`WORK_LIMIT`] code-unit steps (the JS has no bound).
//!
//! Unicode tables: NFC/NFKC from `icu_normalizer`, case mapping from Rust's `std`, `\p{Cf}`,
//! `\p{L}`, `\p{N}` from `icu_properties`. The JS follows its runtime's ICU, so for code points
//! whose properties changed between Unicode versions the two may differ; the host treats any
//! difference as a refusal. The shared fixture table therefore holds only ASCII rows with JS
//! answers; non-ASCII rows record no JS answer (see the fixture generator).
//!
//! Linear in the input apart from the leak search, which is bounded by [`WORK_LIMIT`]; no panics.

#![forbid(unsafe_code)]

pub mod cred;
pub mod guard;
pub mod project;
pub mod text;

use project::{caps, u, Ctx};
use prompt_framing::json::{self, Value};
use std::collections::BTreeMap;
use text::Budget;

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024 + 1;
/// The most code-unit steps one call's redaction and leak search may take.
pub const WORK_LIMIT: u64 = 1 << 29;
/// JSON nesting kept (the deepest value the JS reads is 35 levels down).
const MAX_DEPTH: usize = 40;

/// Why the port will not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Input,
    TooLarge,
    /// The JS answer could depend on the runtime's ICU (see the crate docs).
    Ambiguous,
}

impl Refusal {
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
            Refusal::Ambiguous => r#"{"error":"ambiguous"}"#,
        }
    }
}

/// A projection value, as `canonicalize` leaves it (object keys sorted by code units).
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Str(Vec<u16>),
    Int(i64),
    Bool(bool),
    Arr(Vec<Out>),
    Obj(BTreeMap<Vec<u16>, Out>),
}

fn write(v: &Out, out: &mut Vec<u8>) {
    match v {
        Out::Str(s) => json::push_str(out, s),
        Out::Int(n) => out.extend(n.to_string().bytes()),
        Out::Bool(b) => out.extend(if *b { &b"true"[..] } else { &b"false"[..] }),
        Out::Arr(xs) => {
            out.push(b'[');
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write(x, out);
            }
            out.push(b']');
        }
        Out::Obj(m) => {
            out.push(b'{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                json::push_str(out, k);
                out.push(b':');
                write(x, out);
            }
            out.push(b'}');
        }
    }
}

/// `serializeProjection`: sorted keys, no whitespace.
pub fn serialize(v: &Out) -> String {
    let mut out = Vec::new();
    write(v, &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// What the JS returns or throws.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Projection {
        value: Out,
        redactions: usize,
    },
    /// A `RoleContextError` code.
    Refused(&'static str),
    /// A `RoleContextLeakError`'s classes.
    Leak(Vec<&'static str>),
}

fn any_lone_surrogate(v: &Value) -> bool {
    match v {
        Value::Str(s) => text::has_lone_surrogate(s),
        Value::Arr(xs) => xs.iter().any(any_lone_surrogate),
        Value::Obj(m) => m
            .iter()
            .any(|(k, x)| text::has_lone_surrogate(k) || any_lone_surrogate(x)),
        _ => false,
    }
}

/// The task's tenant id, or the code the JS throws.
fn tenant_of(state: &Value) -> Result<&[u16], &'static str> {
    if !matches!(state, Value::Obj(_)) {
        return Err("invalid_state");
    }
    match state.get("tenantId") {
        None | Some(Value::Null) => Err("missing_tenant"),
        Some(Value::Str(s)) if s.is_empty() => Err("missing_tenant"),
        Some(Value::Str(s)) => Ok(s),
        Some(_) => Err("invalid_tenant"),
    }
}

fn role_of(v: &Value) -> Option<&'static str> {
    let s = v.as_str()?;
    ["planner", "executor", "auditor", "reviewer"]
        .into_iter()
        .find(|r| s.iter().copied().eq(r.encode_utf16()))
}

fn too_large(v: &Out) -> bool {
    serialize(v).chars().count() > caps::TOTAL
}

/// `projectRoleContext(role, state)`.
pub fn project_role(role: &Value, state: &Value, budget: &mut Budget) -> Result<Outcome, Refusal> {
    let Some(role) = role_of(role) else {
        return Ok(Outcome::Refused("unknown_role"));
    };
    let tenant = match tenant_of(state) {
        Ok(t) => t,
        Err(code) => return Ok(Outcome::Refused(code)),
    };
    let ctx = Ctx {
        role,
        tenant,
        known: project::credential_values(state),
    };
    let mut m = BTreeMap::new();
    let mut redactions = 0;
    for key in project::spec_keys(role).unwrap_or_default() {
        let (v, n) = project::field(key, state, &ctx, budget)?;
        redactions += n;
        if let Some(v) = v {
            m.insert(u(key), v);
        }
    }
    let value = Out::Obj(m);
    if too_large(&value) {
        return Ok(Outcome::Refused("too_large"));
    }
    let hay = guard::Hay::new(&value)?;
    let leaked = guard::guard(&value, &hay, state, role, tenant, budget)?;
    if !leaked.is_empty() {
        return Ok(Outcome::Leak(leaked));
    }
    Ok(Outcome::Projection { value, redactions })
}

const PERSONA_FIELDS: [&str; 4] = ["role", "role_name", "role_instructions", "revision"];
const REVISION_FIELDS: [&str; 6] = [
    "plan",
    "execution",
    "change",
    "lifecycle_state",
    "approval_outcomes",
    "feedback",
];

/// `projectSharedDossier(state, { roles })`.
pub fn dossier(roles: &Value, state: &Value, budget: &mut Budget) -> Result<Outcome, Refusal> {
    let Value::Arr(list) = roles else {
        return Ok(Outcome::Refused("unknown_role"));
    };
    if list.is_empty() {
        return Ok(Outcome::Refused("unknown_role"));
    }
    let mut specs = Vec::new();
    for r in list {
        let Some(role) = role_of(r) else {
            return Ok(Outcome::Refused("unknown_role"));
        };
        specs.push((role, project::spec_keys(role).unwrap_or_default()));
    }
    let tenant = match tenant_of(state) {
        Ok(t) => t,
        Err(code) => return Ok(Outcome::Refused(code)),
    };
    let known = project::credential_values(state);
    let Some((_, first)) = specs.first() else {
        return Ok(Outcome::Refused("unknown_role"));
    };
    let mut keys: Vec<&str> = first
        .iter()
        .copied()
        .filter(|k| {
            !PERSONA_FIELDS.contains(k)
                && !REVISION_FIELDS.contains(k)
                && specs.iter().all(|(_, s)| s.contains(k))
        })
        .collect();
    keys.sort_unstable();
    let mut m = BTreeMap::new();
    let mut redactions = 0;
    for key in keys {
        let mut outputs = Vec::new();
        for (role, _) in &specs {
            let ctx = Ctx {
                role,
                tenant,
                known: known.clone(),
            };
            let (v, n) = project::field(key, state, &ctx, budget)?;
            let text = v.as_ref().map(serialize);
            outputs.push((v, text, n));
        }
        let Some((Some(v0), t0, n0)) = outputs.first().cloned() else {
            continue;
        };
        if outputs.iter().any(|(_, t, _)| *t != t0) {
            continue;
        }
        m.insert(u(key), v0);
        redactions += n0;
    }
    let value = Out::Obj(m);
    if too_large(&value) {
        return Ok(Outcome::Refused("too_large"));
    }
    let hay = guard::Hay::new(&value)?;
    for (role, _) in &specs {
        let leaked = guard::guard(&value, &hay, state, role, tenant, budget)?;
        if !leaked.is_empty() {
            return Ok(Outcome::Leak(leaked));
        }
    }
    Ok(Outcome::Projection { value, redactions })
}

fn reply(outcome: &Outcome, key: &str) -> String {
    match outcome {
        Outcome::Projection { value, redactions } => {
            format!(
                "{{\"{key}\":{},\"redactions\":{redactions}}}",
                serialize(value)
            )
        }
        Outcome::Refused(code) => format!("{{\"refused\":\"{code}\"}}"),
        Outcome::Leak(classes) => {
            let list: Vec<String> = classes.iter().map(|c| format!("\"{c}\"")).collect();
            format!("{{\"leak\":[{}]}}", list.join(","))
        }
    }
}

/// Answer one wire request (see the crate docs): `(0, reply)` or `(1, refusal)`.
pub fn call(input: &[u8]) -> (u32, String) {
    if input.len() > MAX_INPUT_BYTES {
        return (1, Refusal::TooLarge.json().to_owned());
    }
    let Some((&op, body)) = input.split_first() else {
        return (1, Refusal::Input.json().to_owned());
    };
    let Some(Value::Arr(args)) = json::parse_utf8(body, MAX_DEPTH) else {
        return (1, Refusal::Input.json().to_owned());
    };
    let [first, state] = args.as_slice() else {
        return (1, Refusal::Input.json().to_owned());
    };
    if any_lone_surrogate(first) || any_lone_surrogate(state) {
        return (1, Refusal::Ambiguous.json().to_owned());
    }
    let mut budget = Budget::new(WORK_LIMIT);
    let r = match op {
        1 => project_role(first, state, &mut budget).map(|o| reply(&o, "projection")),
        2 => dossier(first, state, &mut budget).map(|o| reply(&o, "dossier")),
        _ => Err(Refusal::Input),
    };
    match r {
        Ok(s) => (0, s),
        Err(e) => (1, e.json().to_owned()),
    }
}
