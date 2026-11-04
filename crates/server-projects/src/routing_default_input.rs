//! The PUT /api/routing-default body, read as core `JSON.parse(await readBody(req))` then `body?.routing` and
//! `body.applyToExisting === true` read it. Syntax is validated with serde_json's iterative RawValue validator, which
//! skips ignored values without materialising them: overflowing numbers, lone surrogates, reserved-plane text and
//! deep nesting stay valid JSON, as in V8. Invalid UTF-8 reads as U+FFFD (Buffer#toString). An empty or
//! whitespace-only body is not JSON (unlike readJson, which answers `{}` for it).
use serde_json::value::RawValue;

pub(crate) struct Input {
    /// `body?.routing` when it is a string (any other value is neither 'auto' nor 'manual').
    pub routing: Option<String>,
    /// `body.applyToExisting === true`.
    pub apply: bool,
}

fn take<'a>(rest: &mut &'a str) -> Option<&'a RawValue> {
    *rest = rest.trim_start_matches([' ', '\t', '\r', '\n']);
    let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<&RawValue>();
    let value = stream.next()?.ok()?;
    *rest = rest.get(stream.byte_offset()..)?;
    Some(value)
}

/// `None`: `JSON.parse` would throw. A body that is not an object has no `routing` and no `applyToExisting`.
pub(crate) fn parse(bytes: &[u8]) -> Option<Input> {
    let text = String::from_utf8_lossy(bytes);
    let raw = serde_json::from_str::<&RawValue>(&text).ok()?;
    let mut rest = raw.get();
    let mut input = Input {
        routing: None,
        apply: false,
    };
    if !rest.starts_with('{') {
        return Some(input);
    }
    rest = rest.get(1..)?;
    loop {
        rest = rest.trim_start_matches([' ', '\t', '\r', '\n']);
        if rest.starts_with('}') {
            break;
        }
        let key = take(&mut rest)?;
        rest = rest
            .trim_start_matches([' ', '\t', '\r', '\n'])
            .strip_prefix(':')?;
        let value = take(&mut rest)?;
        // A duplicate key takes the last value, as JSON.parse does.
        match serde_json::from_str::<String>(key.get()).ok().as_deref() {
            Some("routing") => input.routing = serde_json::from_str::<String>(value.get()).ok(),
            Some("applyToExisting") => input.apply = value.get() == "true",
            _ => {}
        }
        rest = rest.trim_start_matches([' ', '\t', '\r', '\n']);
        if let Some(next) = rest.strip_prefix(',') {
            rest = next;
        } else {
            break;
        }
    }
    Some(input)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_two_fields_like_json_parse() {
        let p = |s: &str| parse(s.as_bytes());
        assert!(
            p("").is_none()
                && p("  ").is_none()
                && p("{").is_none()
                && p("{\"routing\":}").is_none()
        );
        let i = p(r#"{"routing":"manual","applyToExisting":true}"#).unwrap();
        assert_eq!((i.routing.as_deref(), i.apply), (Some("manual"), true));
        // Last duplicate wins; escapes decode; only the literal true applies.
        let i = p(
            r#"{"routing":"auto","routing":"manual","applyToExisting":true,"applyToExisting":1}"#,
        )
        .unwrap();
        assert_eq!((i.routing.as_deref(), i.apply), (Some("manual"), false));
        for s in ["null", "[]", "0", "\"routing\"", "true"] {
            let i = p(s).unwrap();
            assert_eq!((i.routing, i.apply), (None, false), "{s}");
        }
        let i = p(r#"{"routing":["auto"],"applyToExisting":"true"}"#).unwrap();
        assert_eq!((i.routing, i.apply), (None, false));
        // Values Node accepts and serde would refuse stay valid.
        let deep = format!(
            r#"{{"x":{}0{},"routing":"auto"}}"#,
            "[".repeat(1000),
            "]".repeat(1000)
        );
        assert_eq!(p(&deep).unwrap().routing.as_deref(), Some("auto"));
        for s in [
            r#"{"x":1e999,"routing":"auto"}"#,
            r#"{"x":"\ud800","routing":"auto"}"#,
        ] {
            assert_eq!(p(s).unwrap().routing.as_deref(), Some("auto"), "{s}");
        }
        assert!(p("{\"a\":1,}").is_none() && p("{\"a\":1}x").is_none());
        // Invalid UTF-8 inside an ignored string reads as U+FFFD, as Buffer#toString does.
        let mut bytes = br#"{"x":""#.to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(br#"","routing":"auto"}"#);
        assert_eq!(parse(&bytes).unwrap().routing.as_deref(), Some("auto"));
    }
}
