use roxmltree::{Document, Node};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Property {
    pub uri: String,
    pub local: String,
}

/// None means all supported DAV properties. This deliberately accepts only the
/// narrow property-selection dialect used by the existing Node listener.
pub fn selection(body: &str) -> Result<Option<Vec<Property>>, ()> {
    if body.trim().is_empty() {
        return Ok(None);
    }
    if body.len() > 8192 {
        return Err(());
    }
    let without_decl = if body.trim_start().starts_with("<?xml") {
        let start = body.find("<?xml").ok_or(())?;
        let end = body[start..].find("?>").ok_or(())? + start + 2;
        &body[end..]
    } else {
        body
    };
    if without_decl.contains("<!") || without_decl.contains("<?") || without_decl.contains('&') {
        return Err(());
    }
    let doc = Document::parse(body).map_err(|_| ())?;
    let elements: Vec<Node<'_, '_>> = doc.descendants().filter(Node::is_element).collect();
    if elements.len() > 128
        || elements.iter().any(|n| {
            n.ancestors().filter(Node::is_element).count() > 8 || n.attributes().len() != 0
        })
    {
        return Err(());
    }
    if doc
        .descendants()
        .any(|n| n.is_text() && !n.text().unwrap_or("").trim().is_empty())
    {
        return Err(());
    }
    let root = doc.root_element();
    if root.tag_name().namespace() != Some("DAV:") || root.tag_name().name() != "propfind" {
        return Err(());
    }
    let children: Vec<_> = root.children().filter(Node::is_element).collect();
    let [choice] = children.as_slice() else {
        return Err(());
    };
    if choice.tag_name().namespace() != Some("DAV:") {
        return Err(());
    }
    let props: Vec<_> = choice.children().filter(Node::is_element).collect();
    if choice.tag_name().name() == "allprop" && props.is_empty() {
        return Ok(None);
    }
    if choice.tag_name().name() != "prop" {
        return Err(());
    }
    let mut selected = Vec::with_capacity(props.len());
    for p in props {
        if p.children().any(|n| n.is_element()) {
            return Err(());
        }
        let uri = p.tag_name().namespace().ok_or(())?;
        selected.push(Property {
            uri: uri.to_owned(),
            local: p.tag_name().name().to_owned(),
        });
    }
    Ok(Some(selected))
}

pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}
