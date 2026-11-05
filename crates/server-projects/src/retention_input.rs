//! Bounded request-field extraction with V8-compatible syntax validation. RawValue's iterative
//! validator skips ignored values without materializing a recursive tree or parsing their numbers.
//! Thus overflow, lone surrogates, reserved-plane text and deep ignored values remain valid JSON.
use serde_json::value::RawValue;

#[derive(Default)]
pub(crate) struct Input {
    pub days: Option<f64>,
    pub confirmed: Option<f64>,
}

fn take<'a>(rest: &mut &'a str) -> Option<&'a RawValue> {
    *rest = rest.trim_start_matches([' ', '\t', '\r', '\n']);
    let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<&RawValue>();
    let value = stream.next()?.ok()?;
    *rest = rest.get(stream.byte_offset()..)?;
    Some(value)
}

pub(crate) fn parse(bytes: &[u8]) -> Option<Input> {
    if bytes.is_empty() {
        return Some(Input::default());
    }
    let text = String::from_utf8_lossy(bytes);
    let raw = serde_json::from_str::<&RawValue>(&text).ok()?;
    let mut rest = raw.get();
    let mut input = Input::default();
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
        // Only numeric literals are consumed; strings/objects/booleans never coerce to numbers.
        let number = value.get().parse::<f64>().ok();
        match serde_json::from_str::<String>(key.get()).ok().as_deref() {
            Some("days") => input.days = number,
            Some("confirmDeletes") => input.confirmed = number,
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
