//! Project-to-Code reference text. Account/project selection belongs to the native front;
//! rendering reuses the existing untrusted prompt framing, including its closing-marker escape.
use crate::code::Failure;
use js_json::{wtf16, JValue};
pub struct Rendered {
    pub enabled: bool,
    pub text: String,
}
fn text(value: &JValue) -> Result<String, Failure> {
    js_json::to_js_string(value).map_err(|_| Failure::internal("Code request failed"))
}
fn clip(value: &JValue, max: usize) -> Result<String, Failure> {
    let source = if value.truthy() {
        text(value)?
    } else {
        String::new()
    };
    let mut folded = String::new();
    let mut space = false;
    for c in source.chars() {
        if server_projects::js::is_space(c) {
            space = !folded.is_empty();
        } else {
            if space {
                folded.push(' ');
            }
            space = false;
            folded.push(c);
        }
    }
    Ok(if wtf16::len(&folded) > max {
        format!("{}…", wtf16::slice(&folded, max.saturating_sub(1)))
    } else {
        folded
    })
}
fn units(source: &str) -> Vec<u16> {
    let mut out = vec![];
    for c in source.chars() {
        if let Some(unit) = wtf16::lone_unit(c) {
            out.push(unit);
        } else {
            let mut buffer = [0; 2];
            out.extend_from_slice(c.encode_utf16(&mut buffer));
        }
    }
    out
}
fn from_units(source: &[u16]) -> String {
    char::decode_utf16(source.iter().copied())
        .map(|c| match c {
            Ok(c) => c,
            Err(e) => wtf16::lone_char(e.unpaired_surrogate()).unwrap_or('�'),
        })
        .collect()
}
fn frame(label: &str, body: &str) -> String {
    from_units(&prompt_framing::framing::frame_untrusted(
        &units("shared project context"),
        &units(label),
        &units(body),
    ))
}
pub fn for_code(project: &JValue, chats: &[JValue]) -> Result<Rendered, Failure> {
    let enabled = project.get("sharedContext").get("code") == &JValue::Bool(true);
    if !enabled {
        return Ok(Rendered {
            enabled,
            text: String::new(),
        });
    }
    let mut parts = vec![];
    if project.get("goal").truthy() {
        parts.push(format!("Project goal: {}", clip(project.get("goal"), 600)?));
    }
    if project.get("instructions").truthy() {
        parts.push(format!(
            "Project instructions:\n{}",
            wtf16::slice(&text(project.get("instructions"))?, 3000)
        ));
    }
    let memories = match project.get("memories") {
        JValue::Arr(values) => values
            .iter()
            .filter(|v| {
                v.as_str()
                    .is_some_and(|s| !server_account::util::js_trim(s).is_empty())
            })
            .take(20)
            .collect::<Vec<_>>(),
        value if !value.truthy() => vec![],
        _ => return Err(Failure::internal("Code request failed")),
    };
    if !memories.is_empty() {
        let lines = memories
            .into_iter()
            .map(|m| clip(m, 300).map(|m| format!("- {m}")))
            .collect::<Result<Vec<_>, _>>()?;
        parts.push(format!("Project memories:\n{}", lines.join("\n")));
    }
    let mut recent = chats
        .iter()
        .filter(|c| c.truthy() && c.get("title").truthy())
        .collect::<Vec<_>>();
    recent.sort_by(|a, b| {
        let a = a.get("updatedAt");
        let b = b.get("updatedAt");
        let a = if a.truthy() {
            crate::meta::numeric(a)
        } else {
            0.0
        };
        let b = if b.truthy() {
            crate::meta::numeric(b)
        } else {
            0.0
        };
        b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal)
    });
    recent.truncate(8);
    if !recent.is_empty() {
        let lines = recent
            .into_iter()
            .map(|c| {
                Ok(format!(
                    "- {}{}",
                    clip(c.get("title"), 100)?,
                    if c.get("preview").truthy() {
                        format!(" — {}", clip(c.get("preview"), 160)?)
                    } else {
                        String::new()
                    }
                ))
            })
            .collect::<Result<Vec<_>, Failure>>()?;
        parts.push(format!(
            "Recent chats in this project (titles and last lines, for background):\n{}",
            lines.join("\n")
        ));
    }
    if parts.is_empty() {
        return Ok(Rendered {
            enabled,
            text: String::new(),
        });
    }
    let body = parts.join("\n\n");
    let label = clip(project.get("name"), 80)?;
    let limit = 6000usize.saturating_sub(wtf16::len(&frame(&label, "")));
    let body = if wtf16::len(&body) > limit {
        format!("{}…", wtf16::slice(&body, limit.saturating_sub(1)))
    } else {
        body
    };
    Ok(Rendered {
        enabled,
        text: frame(&label, &body),
    })
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn shared_code_reference_matches_node() {
        let fixture =
            wtf16::parse(include_str!("../tests/fixtures/shared-context-node.json")).unwrap();
        let JValue::Arr(rows) = fixture else {
            panic!("fixture");
        };
        for (index, row) in rows.iter().enumerate() {
            let JValue::Arr(chats) = row.get("chats") else {
                panic!("chats");
            };
            let actual = for_code(row.get("project"), chats);
            if row.get("error") == &JValue::Bool(true) {
                assert!(actual.is_err());
            } else {
                let actual = actual.unwrap();
                assert_eq!(
                    actual.text,
                    row.get("text").as_str().unwrap(),
                    "case {index}"
                );
                assert_eq!(actual.enabled, row.get("enabled") == &JValue::Bool(true));
            }
        }
    }
}
