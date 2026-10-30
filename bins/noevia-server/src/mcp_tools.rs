//! What a listing of an MCP server's tools means to noevia (sbstndalton/noevia#2202 round 2): core
//! mcp.cjs `convertTool`, applied to the tools [`crate::mcp_egress::list_tools`]
//! brought back, so the admin directory can say how many tools it can use before a server is added.
//!
//! A tool is usable when its name is `[A-Za-z0-9_-]{1,64}`, its `inputSchema` is an object schema
//! (`type: "object"`, `properties` absent or an object, `required` absent or an array) and its
//! `$ref`s resolve (mcp-frame's `resolve_schema_refs`, the port core calls). Anything else is dropped
//! with a reason: [`usable`] returns the reasons, and no caller shows or logs them (core only logged them,
//! and the front's admin pages say only how many tools are usable). Two tools with one name count once (core keeps them in a map
//! by name, the later one replacing the earlier). The catalogue a preview was shown is a digest of the
//! usable tools' name, description and schema, so a server whose tools changed between the preview
//! and the connect is noticed.
use js_json::JValue;
use sha2::{Digest, Sha256};

/// One usable tool, as the admin page and the preview list it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    pub name: String,
    /// core `description`: the tool's own, else its title, else empty.
    pub description: String,
    /// The input schema as the server sent it (a digest input only).
    schema: String,
}

fn name_ok(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// core `convertTool`: the tool, or the reason it is dropped.
pub fn convert(tool: &JValue) -> Result<Tool, String> {
    let name = match tool.get("name") {
        JValue::Str(n) if name_ok(n) => n.clone(),
        other => {
            return Err(format!(
                "invalid tool name {}",
                js_json::stringify(other).unwrap_or_else(|| "undefined".into())
            ))
        }
    };
    let schema = tool.get("inputSchema");
    if !matches!(schema, JValue::Obj(_)) {
        return Err("inputSchema missing or not an object".into());
    }
    if schema.get("type").as_str() != Some("object") {
        return Err(format!(
            "inputSchema.type is {}, expected \"object\"",
            js_json::stringify(schema.get("type")).unwrap_or_else(|| "undefined".into())
        ));
    }
    // `typeof properties !== 'object' || Array.isArray(properties)`; null passes (typeof null).
    if !matches!(
        schema.get("properties"),
        JValue::Undefined | JValue::Null | JValue::Obj(_)
    ) {
        return Err("inputSchema.properties is not an object".into());
    }
    if !matches!(schema.get("required"), JValue::Undefined | JValue::Arr(_)) {
        return Err("inputSchema.required is not an array".into());
    }
    let text = js_json::stringify(schema).ok_or("unresolvable schema")?;
    let units: Vec<u16> = text.encode_utf16().collect();
    match mcp_frame::schema::resolve_schema_refs(&units) {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(format!("unresolvable schema: {}", schema_reason(&e))),
        Err(_) => return Err("unresolvable schema: could not be read".into()),
    }
    let description = match (tool.get("description"), tool.get("title")) {
        (JValue::Str(d), _) if !d.is_empty() => d.clone(),
        (_, JValue::Str(t)) => t.clone(),
        _ => String::new(),
    };
    Ok(Tool {
        name,
        description,
        schema: text,
    })
}

fn schema_reason(e: &mcp_frame::schema::SchemaError) -> String {
    use mcp_frame::schema::SchemaError as E;
    let text = |r: &[u16]| String::from_utf16_lossy(r);
    match e {
        E::Nest => "schema nests deeper than we will walk".into(),
        E::RefDepth => "refs expand deeper than we will inline".into(),
        E::Nodes => "schema expands past 20000 nodes".into(),
        E::Chars => "schema expands past 262144 characters".into(),
        E::NonLocal(r) => format!("cannot resolve non-local ref {}", text(r)),
        E::Circular(r) => format!("circular ref {}", text(r)),
        E::Missing(r) => format!("ref {} points at a definition that is not present", text(r)),
    }
}

/// The usable tools of a listing, in the order the server listed them (core keeps them in a `Map`
/// by name: a name offered twice keeps its first position and takes the later definition), and the
/// reasons for the ones dropped.
pub fn usable(listed: &[JValue]) -> (Vec<Tool>, Vec<String>) {
    let mut tools: Vec<Tool> = Vec::new();
    let mut dropped = Vec::new();
    for tool in listed {
        match convert(tool) {
            Ok(t) => match tools.iter_mut().find(|x| x.name == t.name) {
                Some(held) => *held = t,
                None => tools.push(t),
            },
            Err(reason) => dropped.push(format!(
                "{}: {reason}",
                tool.get("name").as_str().unwrap_or("(unnamed)")
            )),
        }
    }
    (tools, dropped)
}

/// core `catalog(tools)` as a digest: what a preview vouches for, independent of listing order
/// (core sorts by name).
pub fn catalogue_digest(tools: &[Tool]) -> String {
    let mut sorted: Vec<&Tool> = tools.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut h = Sha256::new();
    for t in sorted {
        for part in [&t.name, &t.description, &t.schema] {
            h.update((part.len() as u64).to_be_bytes());
            h.update(part.as_bytes());
        }
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The `tools` array of the preview and the directory listing: `{ name, description }`, at most 40,
/// the name cut at 120 and the description at 240 (core's slices), and whether more were cut.
pub fn shown(tools: &[Tool]) -> (JValue, bool) {
    let list: Vec<JValue> = tools
        .iter()
        .take(40)
        .map(|t| {
            JValue::obj([
                ("name", JValue::Str(js_json::js_slice(&t.name, 120))),
                (
                    "description",
                    JValue::Str(js_json::js_slice(&t.description, 240)),
                ),
            ])
        })
        .collect();
    (JValue::Arr(list), tools.len() > 40)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn j(s: &str) -> JValue {
        js_json::parse(s).unwrap()
    }

    #[test]
    fn a_tool_is_usable_the_way_core_convert_tool_says() {
        let ok = convert(&j(
            r#"{"name":"echo-1","description":"Echo","inputSchema":{"type":"object","properties":{"t":{"type":"string"}},"required":["t"]},"annotations":{"readOnlyHint":true}}"#,
        ))
        .unwrap();
        assert_eq!(
            (ok.name.as_str(), ok.description.as_str()),
            ("echo-1", "Echo")
        );
        // The title stands in for a missing or empty description.
        let titled = convert(&j(
            r#"{"name":"t","title":"A title","description":"","inputSchema":{"type":"object"}}"#,
        ))
        .unwrap();
        assert_eq!(titled.description, "A title");
        for (bad, why) in [
            (
                r#"{"name":"has space","inputSchema":{"type":"object"}}"#,
                "invalid tool name",
            ),
            (r#"{"inputSchema":{"type":"object"}}"#, "invalid tool name"),
            (r#"{"name":"x"}"#, "inputSchema missing or not an object"),
            (
                r#"{"name":"x","inputSchema":[]}"#,
                "inputSchema missing or not an object",
            ),
            (
                r#"{"name":"x","inputSchema":{"type":"array"}}"#,
                "expected \"object\"",
            ),
            (
                r#"{"name":"x","inputSchema":{"type":"object","properties":[]}}"#,
                "properties is not an object",
            ),
            (
                r#"{"name":"x","inputSchema":{"type":"object","required":"a"}}"#,
                "required is not an array",
            ),
            (
                r##"{"name":"x","inputSchema":{"type":"object","properties":{"a":{"$ref":"#/$defs/none"}}}}"##,
                "unresolvable schema",
            ),
        ] {
            let e = convert(&j(bad)).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        // A name is 1..64 characters of the plain set.
        assert!(name_ok(&"a".repeat(64)) && !name_ok(&"a".repeat(65)) && !name_ok(""));
    }

    #[test]
    fn a_listing_counts_usable_names_once_and_the_digest_moves_with_any_change() {
        let first = [
            j(r#"{"name":"a","inputSchema":{"type":"object"}}"#),
            j(r#"{"name":"a","description":"later","inputSchema":{"type":"object"}}"#),
            j(r#"{"name":"b","inputSchema":{"type":"string"}}"#),
        ];
        let (tools, dropped) = usable(&first);
        assert_eq!(tools.len(), 1);
        assert_eq!(dropped.len(), 1);
        assert_eq!(tools.first().map(|t| t.description.as_str()), Some("later"));
        let digest = catalogue_digest(&tools);
        let (same, _) = usable(&first);
        assert_eq!(catalogue_digest(&same), digest);
        let (other, _) = usable(&[j(
            r#"{"name":"a","description":"later","inputSchema":{"type":"object","properties":{"x":{}}}}"#,
        )]);
        assert_ne!(catalogue_digest(&other), digest, "a schema change moves it");
        let (renamed, _) = usable(&[j(
            r#"{"name":"a","description":"changed","inputSchema":{"type":"object"}}"#,
        )]);
        assert_ne!(
            catalogue_digest(&renamed),
            digest,
            "a description change moves it"
        );
    }
}
