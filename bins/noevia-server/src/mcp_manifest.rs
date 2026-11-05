//! The curated MCP toolboxes and how they bind to what the servers offer (sbstndalton/noevia row T):
//! core `mcp-toolbox-manifest.cjs`, `mcp-boxes.cjs` (`bindBoxes`) and `tool-hints.cjs` (`withHint`).
//!
//! A box binds **only tools from its own server**: curation is by explicit tool name, and a second
//! server, rogue or merely careless, must not be able to slip a tool into a box the user trusts by
//! offering the same name. A box that lost every tool is not shown at all.
//!
//! The manifest is data: `mcp_toolbox_manifest.json` is core's manifest built with `diaryMcpWrite`
//! off; the Diary box's append tool is added when that construction value is on (the same single
//! difference core's builder has). `tests/mcp_discovery_actor.rs` compares both variants with core's
//! own output (`tests/fixtures/mcp-discovery-node.v1.json`).
use js_json::JValue;
use prompt_framing::js::trim;
use std::collections::HashMap;
use std::sync::OnceLock;

const MANIFEST: &str = include_str!("mcp_toolbox_manifest.json");
/// core `tool-hints.cjs` `MAX_HINT`.
const MAX_HINT: usize = 200;

/// The boxes core's `buildToolboxManifest({ diaryMcpWrite })` returns, as parsed values.
pub fn manifest(diary_mcp_write: bool) -> Vec<JValue> {
    static BASE: OnceLock<Vec<JValue>> = OnceLock::new();
    let base = BASE.get_or_init(|| match js_json::parse(MANIFEST) {
        Ok(JValue::Arr(boxes)) => boxes,
        _ => Vec::new(),
    });
    base.iter()
        .map(|b| {
            if diary_mcp_write && b.get("id").as_str() == Some("diary") {
                with_tools(b, |names| names.push(JValue::Str("diary_append".into())))
            } else {
                b.clone()
            }
        })
        .collect()
}

fn with_tools(b: &JValue, edit: impl Fn(&mut Vec<JValue>)) -> JValue {
    let JValue::Obj(items) = b else {
        return b.clone();
    };
    JValue::Obj(
        items
            .iter()
            .map(|(k, v)| match (k.as_str(), v) {
                ("tools", JValue::Arr(names)) => {
                    let mut names = names.clone();
                    edit(&mut names);
                    (k.clone(), JValue::Arr(names))
                }
                _ => (k.clone(), v.clone()),
            })
            .collect(),
    )
}

/// `withHint(tool, hint)`: the hint (trimmed, cut at 200 UTF-16 units) is appended to the tool's own
/// description, never replacing it, and never twice.
fn with_hint(tool: &JValue, hint: &str) -> JValue {
    let units: Vec<u16> = hint.encode_utf16().collect();
    let text = String::from_utf16_lossy(trim(&units));
    let text = js_json::js_slice(&text, MAX_HINT);
    if text.is_empty() || !matches!(tool.get("function"), JValue::Obj(_)) {
        return tool.clone();
    }
    let original = match tool.get("function").get("description") {
        JValue::Str(d) => {
            let units: Vec<u16> = d.encode_utf16().collect();
            String::from_utf16_lossy(trim(&units))
        }
        _ => String::new(),
    };
    if original.contains(&text) {
        return tool.clone();
    }
    let description = if original.is_empty() {
        text
    } else {
        format!("{original} {text}")
    };
    set_function_description(tool, description)
}

fn set_function_description(tool: &JValue, description: String) -> JValue {
    let JValue::Obj(items) = tool else {
        return tool.clone();
    };
    JValue::Obj(
        items
            .iter()
            .map(|(k, v)| {
                if k == "function" {
                    if let JValue::Obj(f) = v {
                        let mut f = f.clone();
                        for (fk, fv) in f.iter_mut() {
                            if fk == "description" {
                                *fv = JValue::Str(description.clone());
                            }
                        }
                        return (k.clone(), JValue::Obj(f));
                    }
                }
                (k.clone(), v.clone())
            })
            .collect(),
    )
}

/// One tool a server offered: the converted definition.
pub struct Offered<'a> {
    pub tool: &'a JValue,
}

/// `bindBoxes`: the boxes of `manifest` (then the directory's) that have at least one tool, in order.
/// `per_server` maps a server id to the tools it offered by name; `known` says whether a box's server is
/// configured at all (a box naming an unknown server is skipped).
pub fn bind_boxes<'a>(
    manifest: &[JValue],
    per_server: &HashMap<String, HashMap<String, Offered<'a>>>,
    known: &dyn Fn(&str) -> bool,
    warn: &mut dyn FnMut(String),
) -> Vec<JValue> {
    let empty = HashMap::new();
    let mut boxes = Vec::new();
    for b in manifest {
        let server = b.get("server").as_str().unwrap_or_default();
        let owned = per_server.get(server).unwrap_or(&empty);
        let mut tools = Vec::new();
        let mut missing = Vec::new();
        let hints = b.get("hints");
        if let JValue::Arr(names) = b.get("tools") {
            for name in names.iter().filter_map(JValue::as_str) {
                match owned.get(name) {
                    Some(hit) => tools.push(match hints.get(name).as_str() {
                        Some(hint) => with_hint(hit.tool, hint),
                        None => hit.tool.clone(),
                    }),
                    None => missing.push(name.to_owned()),
                }
            }
        }
        if !known(server) {
            warn(format!(
                "[mcp] box {} names unknown server \"{server}\"; skipping",
                b.get("id").as_str().unwrap_or_default()
            ));
            continue;
        }
        if tools.is_empty() {
            continue;
        }
        boxes.push(bound(b, tools));
    }
    boxes
}

/// `{ ...box, source: 'mcp', inApp: true, tools }`.
fn bound(b: &JValue, tools: Vec<JValue>) -> JValue {
    let JValue::Obj(items) = b else {
        return b.clone();
    };
    let mut out: Vec<(String, JValue)> = Vec::new();
    let mut put = |k: &str, v: JValue| match out.iter_mut().find(|(ek, _)| ek == k) {
        Some(slot) => slot.1 = v,
        None => out.push((k.to_owned(), v)),
    };
    for (k, v) in items {
        put(k, v.clone());
    }
    put("source", JValue::Str("mcp".into()));
    put("inApp", JValue::Bool(true));
    put("tools", JValue::Arr(tools));
    JValue::Obj(out)
}

/// `directoryMcp.boxFor(server, toolNames)`.
pub fn directory_box(id: &str, title: &str, tool_names: &[String]) -> JValue {
    JValue::obj([
        ("id", JValue::Str(id.into())),
        ("server", JValue::Str(id.into())),
        ("label", JValue::Str(title.into())),
        ("directory", JValue::Bool(true)),
        (
            "description",
            JValue::Str("Added from the MCP directory. Every call asks first.".into()),
        ),
        (
            "tools",
            JValue::Arr(tool_names.iter().cloned().map(JValue::Str).collect()),
        ),
    ])
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[allow(clippy::panic)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str) -> JValue {
        JValue::obj([
            ("type", JValue::Str("function".into())),
            (
                "function",
                JValue::obj([
                    ("name", JValue::Str(name.into())),
                    ("description", JValue::Str(description.into())),
                ]),
            ),
        ])
    }

    #[test]
    fn the_manifest_is_the_cores_with_the_one_flagged_tool() {
        let off = manifest(false);
        let on = manifest(true);
        assert_eq!(off.len(), 24);
        assert_eq!(on.len(), 24);
        let diary = |m: &[JValue]| m[0].get("tools").clone();
        assert_ne!(diary(&off), diary(&on));
        assert_eq!(off[1], on[1]);
    }

    #[test]
    fn a_hint_is_appended_once_and_never_replaces() {
        let t = tool("a", "does a thing");
        let once = with_hint(&t, "  for tasks ");
        assert_eq!(
            once.get("function").get("description").as_str(),
            Some("does a thing for tasks")
        );
        assert_eq!(with_hint(&once, "for tasks"), once);
        assert_eq!(with_hint(&t, "   "), t);
        let bare = tool("b", "");
        assert_eq!(
            with_hint(&bare, "x")
                .get("function")
                .get("description")
                .as_str(),
            Some("x")
        );
    }

    #[test]
    fn a_box_binds_only_tools_of_its_own_server() {
        let m = vec![JValue::obj([
            ("id", JValue::Str("box".into())),
            ("server", JValue::Str("mine".into())),
            (
                "tools",
                JValue::Arr(vec![JValue::Str("t1".into()), JValue::Str("t2".into())]),
            ),
        ])];
        let theirs = tool("t1", "rogue");
        let mine = tool("t2", "ok");
        let mut per: HashMap<String, HashMap<String, Offered>> = HashMap::new();
        per.entry("other".into())
            .or_default()
            .insert("t1".into(), Offered { tool: &theirs });
        per.entry("mine".into())
            .or_default()
            .insert("t2".into(), Offered { tool: &mine });
        let boxes = bind_boxes(&m, &per, &|_| true, &mut |_| {});
        assert_eq!(boxes.len(), 1);
        let JValue::Arr(tools) = boxes[0].get("tools") else {
            panic!()
        };
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].get("function").get("name").as_str(), Some("t2"));
        assert_eq!(boxes[0].get("source").as_str(), Some("mcp"));
        // No tool of its own: not shown. An unknown server: skipped.
        per.remove("mine");
        assert!(bind_boxes(&m, &per, &|_| true, &mut |_| {}).is_empty());
        assert!(bind_boxes(&m, &per, &|_| false, &mut |_| {}).is_empty());
    }
}
