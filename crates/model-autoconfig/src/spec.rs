//! Speculative-decoding profiles: autoconfig_core.py's `match_spec_profile` and `resolve_spec`,
//! that is which profile a saved section is, which one applies, the draft head, and the spec
//! keys a recommendation writes (in order).

use crate::pystr::strip;
use crate::pyval::{get, int_or};
use crate::values::Values;
use crate::{Error, Work};
use model_files::json::Value;

pub struct Profile {
    pub key: &'static str,
    pub spec_type: &'static str,
    pub needs_head: bool,
    pub knobs: &'static [(&'static str, &'static str)],
}

/// SPEC_PROFILES, in order.
pub const PROFILES: [Profile; 5] = [
    Profile {
        key: "off",
        spec_type: "",
        needs_head: false,
        knobs: &[],
    },
    Profile {
        key: "balanced",
        spec_type: "draft-mtp",
        needs_head: true,
        knobs: &[("spec-draft-n-max", "4"), ("spec-draft-p-min", "0.25")],
    },
    Profile {
        key: "coding",
        spec_type: "draft-mtp,ngram-simple",
        needs_head: true,
        knobs: &[
            ("spec-draft-n-max", "8"),
            ("spec-draft-n-min", "1"),
            ("spec-draft-p-min", "0.05"),
        ],
    },
    Profile {
        key: "writing",
        spec_type: "draft-mtp",
        needs_head: true,
        knobs: &[("spec-draft-n-max", "2"), ("spec-draft-p-min", "0.60")],
    },
    Profile {
        key: "ngram",
        spec_type: "ngram-simple",
        needs_head: false,
        knobs: &[],
    },
];

/// SPEC_PROFILE_KEYS.
pub const PROFILE_KEYS: [&str; 3] = ["spec-draft-n-max", "spec-draft-n-min", "spec-draft-p-min"];
/// SPEC_SECTION_KEYS: what of the saved section is read.
pub const SECTION_KEYS: [&str; 6] = [
    "spec-type",
    "spec-draft-model",
    "spec-draft-ngl",
    "spec-draft-n-max",
    "spec-draft-n-min",
    "spec-draft-p-min",
];
/// MODE_SPEC_PROFILE.
const MODES: [(&str, &str); 4] = [
    ("chat", "balanced"),
    ("code", "coding"),
    ("agent", "coding"),
    ("writing", "writing"),
];
const SPEC_KEYS: [&str; 7] = [
    "section",
    "current",
    "spec_profile",
    "mode",
    "files",
    "found_mtp",
    "nextn",
];

fn profile(key: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.key == key)
}

/// `section.get(k) or ""` for a saved section's string values (None and "" alike), stripped
/// where Python strips it.
fn sec_str<'a>(sec: &'a Value, k: &str) -> Result<&'a str, Error> {
    match get(sec, k) {
        Value::Str(s) => Ok(s),
        v if !v.truthy() => Ok(""),
        // .strip() of a non-string raises (the service never sends one: the ini holds strings).
        _ => Err(Error::Python("AttributeError")),
    }
}

/// `match_spec_profile(section)`.
pub fn match_profile(section: &Value) -> Result<&'static str, Error> {
    if *section == Value::Null {
        return Ok("");
    }
    let stype = strip(sec_str(section, "spec-type")?);
    if stype.is_empty() || stype == "none" {
        return Ok("off");
    }
    for prof in &PROFILES {
        if prof.spec_type != stype {
            continue;
        }
        let mut all = true;
        for (k, v) in prof.knobs {
            if strip(sec_str(section, k)?) != *v {
                all = false;
                break;
            }
        }
        if !all {
            continue;
        }
        let mut extra = false;
        for k in PROFILE_KEYS {
            if !prof.knobs.iter().any(|(pk, _)| *pk == k) && !strip(sec_str(section, k)?).is_empty()
            {
                extra = true;
            }
        }
        if !extra {
            return Ok(prof.key);
        }
    }
    Ok("custom")
}

/// `d.get(k, "").strip()` on the saved section (a missing key is "", a present one must be a
/// string: Python strips it).
fn cur_str<'a>(cs: &'a Value, k: &str) -> Result<&'a str, Error> {
    match cs.get(k) {
        None => Ok(""),
        Some(Value::Str(s)) => Ok(strip(s)),
        Some(_) => Err(Error::Python("AttributeError")),
    }
}

fn string<'a>(v: &'a Value, what: &'static str) -> Result<&'a str, Error> {
    match v {
        Value::Str(s) => Ok(s),
        _ => Err(Error::Schema(what)),
    }
}

pub struct Resolved {
    pub mtp_rel: String,
    pub saved: &'static str,
    pub key: String,
    pub head: String,
    pub values: Values,
}

/// `resolve_spec(inp)`.
pub fn resolve(inp: &Value, work: &mut Work) -> Result<Resolved, Error> {
    let Value::Obj(pairs) = inp else {
        return Err(Error::Schema("spec"));
    };
    if pairs.iter().any(|(k, _)| !SPEC_KEYS.contains(&k.as_str())) {
        return Err(Error::Schema("spec"));
    }
    let field = |k: &str| inp.get(k).ok_or(Error::Schema("spec: missing field"));
    let current = field("current")?;
    let empty = Value::Obj(Vec::new());
    let cs = match current {
        Value::Null => &empty,
        Value::Obj(p) => {
            // Only SPEC_SECTION_KEYS are sent; reading them is a handful of lookups.
            if p.len() > SECTION_KEYS.len()
                || p.iter().any(|(k, _)| !SECTION_KEYS.contains(&k.as_str()))
            {
                return Err(Error::Schema("spec.current"));
            }
            current
        }
        _ => return Err(Error::Schema("spec.current")),
    };
    work.charge(64)?;
    let files = match field("files")? {
        Value::Bool(b) => *b,
        _ => return Err(Error::Schema("spec.files")),
    };
    let found = string(field("found_mtp")?, "spec.found_mtp")?;
    let mut mtp_rel = String::new();
    if files {
        let saved_head = cur_str(cs, "spec-draft-model")?;
        mtp_rel = if saved_head.is_empty() {
            found
        } else {
            saved_head
        }
        .to_owned();
    }
    let builtin = int_or(field("nextn")?, 0, "spec.nextn")? > 0;
    let saved = match_profile(current)?;
    let picked = match field("spec_profile")? {
        Value::Str(s) => strip(s).to_owned(),
        v if !v.truthy() => String::new(),
        _ => return Err(Error::Python("AttributeError")),
    };
    let mut key = picked;
    if profile(&key).is_none() && key != "custom" {
        key = if !saved.is_empty() {
            saved.to_owned()
        } else if !mtp_rel.is_empty() || builtin {
            let mode = field("mode")?;
            match mode {
                Value::Str(m) => MODES
                    .iter()
                    .find(|(k, _)| k == m)
                    .map_or("balanced", |(_, p)| *p)
                    .to_owned(),
                Value::Arr(_) | Value::Obj(_) => return Err(Error::Python("TypeError")),
                // A non-string key matches none of the (string) modes.
                _ => "balanced".to_owned(),
            }
        } else {
            "off".to_owned()
        };
    }
    let saved_head = cur_str(cs, "spec-draft-model")?;
    let head = if saved_head.is_empty() {
        mtp_rel.clone()
    } else {
        saved_head.to_owned()
    };
    let mut prof = profile(&key);
    if let Some(p) = prof {
        if p.needs_head && head.is_empty() && !builtin {
            prof = profile("off");
            key = "off".to_owned();
        }
    }

    let mut values = Values::default();
    if field("section")?.truthy() {
        if key == "custom" {
            for k in SECTION_KEYS {
                let v = cur_str(cs, k)?;
                if !v.is_empty() {
                    values.set(k, v.to_owned());
                }
            }
        } else if let Some(p) = prof {
            values.set("spec-type", p.spec_type.to_owned());
            for k in PROFILE_KEYS {
                let v = p
                    .knobs
                    .iter()
                    .find(|(pk, _)| *pk == k)
                    .map_or("", |(_, v)| *v);
                values.set(k, v.to_owned());
            }
            if !p.spec_type.is_empty() && p.needs_head && head.is_empty() {
                values.set("spec-draft-model", String::new());
                values.set("spec-draft-ngl", String::new());
            } else if !p.spec_type.is_empty() && p.needs_head {
                values.set("spec-draft-model", head.clone());
                let ngl = cur_str(cs, "spec-draft-ngl")?;
                values.set(
                    "spec-draft-ngl",
                    if ngl.is_empty() { "999" } else { ngl }.to_owned(),
                );
            } else {
                values.set("spec-draft-model", String::new());
                values.set("spec-draft-ngl", String::new());
            }
        }
    }
    Ok(Resolved {
        mtp_rel,
        saved,
        key,
        head,
        values,
    })
}

/// The answer as `resolve_spec` returns it.
pub fn resolved_json(r: &Resolved) -> String {
    let mut out = String::from("{\"mtp_rel\":");
    crate::json_str(&mut out, &r.mtp_rel);
    out.push_str(",\"saved\":");
    crate::json_str(&mut out, r.saved);
    out.push_str(",\"key\":");
    crate::json_str(&mut out, &r.key);
    out.push_str(",\"head\":");
    crate::json_str(&mut out, &r.head);
    out.push_str(",\"values\":");
    out.push_str(&crate::values::values_json(&r.values));
    out.push('}');
    out
}
