//! The keys the executor presses, as Playwright's US keyboard layout describes them: `key`, `code`,
//! `windowsVirtualKeyCode` and the text a key types. A shifted character pressed alone is sent as
//! itself (`press('!')`); with `Shift+` a character key becomes its shifted form (`Shift+a` is `A`).
//! Combinations are `Modifier+...+Key` (`Shift+Enter`, `Control+a`, `Control++`).

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
        "Insert" => ("Insert", "Insert", 45, None),
        _ => {
            const F: [&str; 12] = [
                "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
            ];
            let i = F.iter().position(|f| *f == name)?;
            let f = F.get(i)?;
            (f, f, 112 + i as u32, None)
        }
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

/// CDP's `modifiers` bit for Shift (Alt 1, Control 2, Meta 4, Shift 8).
pub(crate) const SHIFT: u32 = 8;

/// A modifier key and its bit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Modifier {
    pub key: Key,
    pub bit: u32,
}

fn modifier(name: &str) -> Option<Modifier> {
    let (key, code, key_code, bit) = match name {
        "Alt" => ("Alt", "AltLeft", 18, 1),
        // Playwright's ControlOrMeta is Control off macOS.
        "Control" | "ControlOrMeta" => ("Control", "ControlLeft", 17, 2),
        "Meta" => ("Meta", "MetaLeft", 91, 4),
        "Shift" => ("Shift", "ShiftLeft", 16, SHIFT),
        _ => return None,
    };
    Some(Modifier {
        key: Key {
            key: key.into(),
            code,
            key_code,
            text: None,
        },
        bit,
    })
}

/// A US-layout character with Shift held.
fn shifted(c: char) -> Option<char> {
    if c.is_ascii_lowercase() {
        return Some(c.to_ascii_uppercase());
    }
    if let Some(d) = c.to_digit(10) {
        return ")!@#$%^&*(".chars().nth(d as usize);
    }
    let (plain, shift) = ("`-=[]\\;',./", "~_+{}|:\"<>?");
    plain.find(c).and_then(|i| shift.chars().nth(i))
}

/// `Modifier+...+Key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Combo {
    pub modifiers: Vec<Modifier>,
    pub key: Key,
}

/// A key or a combination, as Playwright's `keyboard.press` takes it; `None` for an unknown key.
pub(crate) fn combo(name: &str) -> Option<Combo> {
    let (mods, key) = match name.strip_suffix("++") {
        Some(rest) => (rest, "+"),
        None => match name.rsplit_once('+') {
            Some((m, k)) if !k.is_empty() => (m, k),
            _ => ("", name),
        },
    };
    let modifiers = if mods.is_empty() {
        Vec::new()
    } else {
        mods.split('+').map(modifier).collect::<Option<Vec<_>>>()?
    };
    let mut key = match modifier(key) {
        Some(m) => m.key,
        None => lookup(key)?,
    };
    if modifiers.iter().any(|m| m.bit == SHIFT) {
        let mut chars = key.key.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if let Some(up) = shifted(c) {
                key.key = up.to_string();
                key.text = Some(up.to_string());
            }
        }
    }
    Some(Combo { modifiers, key })
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
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
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
        assert_eq!(lookup("F1").unwrap(), k("F1", "F1", 112, None));
        assert_eq!(lookup("F12").unwrap(), k("F12", "F12", 123, None));
        assert_eq!(lookup("Insert").unwrap(), k("Insert", "Insert", 45, None));
        assert_eq!(lookup("F13"), None);
        for unknown in ["", "Shift+Enter", "enter", "ab", "\u{7}"] {
            assert_eq!(lookup(unknown), None, "{unknown:?}");
        }
    }

    #[test]
    fn combinations() {
        let c = combo("Shift+Enter").unwrap();
        assert_eq!(c.modifiers.len(), 1);
        assert_eq!(
            (
                c.modifiers[0].key.key.as_str(),
                c.modifiers[0].key.code,
                c.modifiers[0].key.key_code,
                c.modifiers[0].bit
            ),
            ("Shift", "ShiftLeft", 16, 8)
        );
        assert_eq!(c.key, k("Enter", "Enter", 13, Some("\r")));
        let c = combo("Control+a").unwrap();
        assert_eq!(
            (c.modifiers[0].key.key.as_str(), c.modifiers[0].bit),
            ("Control", 2)
        );
        assert_eq!(c.key, k("a", "KeyA", 65, Some("a")));
        let c = combo("Control++").unwrap();
        assert_eq!(c.key, k("+", "Equal", 187, Some("+")));
        assert_eq!(combo("+").unwrap().key.key, "+");
        assert!(combo("+").unwrap().modifiers.is_empty());
        assert_eq!(combo("Shift+a").unwrap().key, k("A", "KeyA", 65, Some("A")));
        assert_eq!(
            combo("Shift+1").unwrap().key,
            k("!", "Digit1", 49, Some("!"))
        );
        assert_eq!(
            combo("Shift+/").unwrap().key,
            k("?", "Slash", 191, Some("?"))
        );
        let c = combo("Control+Alt+Meta+Shift+F5").unwrap();
        assert_eq!(
            c.modifiers.iter().map(|m| m.bit).collect::<Vec<_>>(),
            [2, 1, 4, 8]
        );
        assert_eq!(c.key, k("F5", "F5", 116, None));
        assert_eq!(
            combo("ControlOrMeta+Insert").unwrap().modifiers[0].key.key,
            "Control"
        );
        let c = combo("Shift").unwrap();
        assert!(c.modifiers.is_empty());
        assert_eq!(c.key.code, "ShiftLeft");
        for unknown in [
            "Hyper+a",
            "Control+",
            "Control+Hyper",
            "a+b",
            "+Control+a",
            "",
        ] {
            assert_eq!(combo(unknown), None, "{unknown:?}");
        }
    }
}
