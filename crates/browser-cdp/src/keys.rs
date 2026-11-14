//! The keys the executor presses, as Playwright's US keyboard layout describes them: `key`, `code`,
//! `windowsVirtualKeyCode` and the text a key types (no modifiers: a shifted character is sent as
//! itself, as Playwright's `press('!')` does).

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    pub key: String,
    pub code: &'static str,
    pub key_code: u32,
    pub text: Option<String>,
}

fn named(name: &str) -> Option<(&'static str, &'static str, u32, Option<&'static str>)> {
    Some(match name {
        "Enter" | "\r" | "\n" => ("Enter", "Enter", 13, Some("\r")),
        "Tab" | "\t" => ("Tab", "Tab", 9, None),
        "Escape" => ("Escape", "Escape", 27, None),
        "Backspace" => ("Backspace", "Backspace", 8, None),
        "Delete" => ("Delete", "Delete", 46, None),
        "ArrowUp" => ("ArrowUp", "ArrowUp", 38, None),
        "ArrowDown" => ("ArrowDown", "ArrowDown", 40, None),
        "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37, None),
        "ArrowRight" => ("ArrowRight", "ArrowRight", 39, None),
        "Home" => ("Home", "Home", 36, None),
        "End" => ("End", "End", 35, None),
        "PageUp" => ("PageUp", "PageUp", 33, None),
        "PageDown" => ("PageDown", "PageDown", 34, None),
        "Space" | " " => (" ", "Space", 32, Some(" ")),
        _ => return None,
    })
}

/// The US-layout key a printable character is on; `("", 0)` for a character the layout lacks.
fn physical(c: char) -> (&'static str, u32) {
    const LETTERS: [&str; 26] = [
        "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK",
        "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV",
        "KeyW", "KeyX", "KeyY", "KeyZ",
    ];
    const DIGITS: [&str; 10] = [
        "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8",
        "Digit9",
    ];
    if c.is_ascii_alphabetic() {
        let upper = c.to_ascii_uppercase();
        let i = usize::from(upper as u8 - b'A');
        return (LETTERS.get(i).copied().unwrap_or(""), u32::from(upper));
    }
    if let Some(d) = c.to_digit(10) {
        return (DIGITS.get(d as usize).copied().unwrap_or(""), 48 + d);
    }
    let shifted_digit = ")!@#$%^&*(".find(c);
    if let Some(d) = shifted_digit {
        return (DIGITS.get(d).copied().unwrap_or(""), 48 + d as u32);
    }
    match c {
        '`' | '~' => ("Backquote", 192),
        '-' | '_' => ("Minus", 189),
        '=' | '+' => ("Equal", 187),
        '[' | '{' => ("BracketLeft", 219),
        ']' | '}' => ("BracketRight", 221),
        '\\' | '|' => ("Backslash", 220),
        ';' | ':' => ("Semicolon", 186),
        '\'' | '"' => ("Quote", 222),
        ',' | '<' => ("Comma", 188),
        '.' | '>' => ("Period", 190),
        '/' | '?' => ("Slash", 191),
        _ => ("", 0),
    }
}

/// The key named `name` (a key name, or any single character), `None` for an unknown key.
pub(crate) fn lookup(name: &str) -> Option<Key> {
    if let Some((key, code, key_code, text)) = named(name) {
        return Some(Key {
            key: key.into(),
            code,
            key_code,
            text: text.map(str::to_string),
        });
    }
    let mut chars = name.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    if c.is_control() {
        return None;
    }
    let (code, key_code) = physical(c);
    Some(Key {
        key: c.to_string(),
        code,
        key_code,
        text: Some(c.to_string()),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn k(key: &str, code: &'static str, key_code: u32, text: Option<&str>) -> Key {
        Key {
            key: key.into(),
            code,
            key_code,
            text: text.map(str::to_string),
        }
    }

    #[test]
    fn table() {
        assert_eq!(
            lookup("Enter").unwrap(),
            k("Enter", "Enter", 13, Some("\r"))
        );
        assert_eq!(lookup("Tab").unwrap(), k("Tab", "Tab", 9, None));
        assert_eq!(lookup("Escape").unwrap(), k("Escape", "Escape", 27, None));
        assert_eq!(
            lookup("Backspace").unwrap(),
            k("Backspace", "Backspace", 8, None)
        );
        assert_eq!(lookup("Delete").unwrap(), k("Delete", "Delete", 46, None));
        assert_eq!(lookup("ArrowUp").unwrap().key_code, 38);
        assert_eq!(lookup("ArrowDown").unwrap().key_code, 40);
        assert_eq!(lookup("ArrowLeft").unwrap().key_code, 37);
        assert_eq!(lookup("ArrowRight").unwrap().key_code, 39);
        assert_eq!(lookup("Home").unwrap().key_code, 36);
        assert_eq!(lookup("End").unwrap().key_code, 35);
        assert_eq!(lookup("PageUp").unwrap().key_code, 33);
        assert_eq!(lookup("PageDown").unwrap().key_code, 34);
        assert_eq!(lookup(" ").unwrap(), k(" ", "Space", 32, Some(" ")));
        assert_eq!(lookup("Space").unwrap(), k(" ", "Space", 32, Some(" ")));
        assert_eq!(lookup("a").unwrap(), k("a", "KeyA", 65, Some("a")));
        assert_eq!(lookup("Z").unwrap(), k("Z", "KeyZ", 90, Some("Z")));
        assert_eq!(lookup("7").unwrap(), k("7", "Digit7", 55, Some("7")));
        assert_eq!(lookup("!").unwrap(), k("!", "Digit1", 49, Some("!")));
        assert_eq!(lookup(")").unwrap(), k(")", "Digit0", 48, Some(")")));
        assert_eq!(lookup("/").unwrap(), k("/", "Slash", 191, Some("/")));
        assert_eq!(lookup("é").unwrap(), k("é", "", 0, Some("é")));
        for unknown in ["", "Shift+Enter", "F13", "enter", "ab", "\u{7}"] {
            assert_eq!(lookup(unknown), None, "{unknown:?}");
        }
    }
}
