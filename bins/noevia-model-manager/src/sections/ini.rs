//! Case-preserving, duplicate-tolerant ConfigParser read dialect, with explicit bounds.
use super::{Budget, Error};

pub const MAX_SECTIONS: usize = 4096;
pub const MAX_KEYS: usize = 65536;
pub const MAX_VALUE: usize = 65536;
pub type Items = Vec<(String, String)>;
fn space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{001c}'..='\u{001f}')
}
pub fn strip(text: &str) -> &str {
    text.trim_matches(space)
}

#[derive(Default, Debug)]
struct Ordered {
    values: Items,
    positions: std::collections::HashMap<String, usize>,
}
impl Ordered {
    fn get_mut(&mut self, key: &str) -> Option<&mut String> {
        self.positions
            .get(key)
            .and_then(|i| self.values.get_mut(*i).map(|(_, value)| value))
    }
    fn put(&mut self, key: &str, value: String) -> Result<(), Error> {
        if let Some(old) = self.get_mut(key) {
            *old = value;
            return Ok(());
        }
        if self.values.len() >= MAX_KEYS {
            return Err(Error::Refused);
        }
        self.positions.try_reserve(1).map_err(|_| Error::Refused)?;
        self.values.try_reserve(1).map_err(|_| Error::Refused)?;
        self.positions.insert(key.to_owned(), self.values.len());
        self.values.push((key.to_owned(), value));
        Ok(())
    }
}

#[derive(Default, Debug)]
pub struct Ini {
    sections: Vec<(String, Ordered)>,
    positions: std::collections::HashMap<String, usize>,
    defaults: Ordered,
}

impl Ini {
    pub fn parse(text: &str, budget: &mut Budget) -> Result<Self, Error> {
        if text.len() > super::INI_CAP {
            return Err(Error::Refused);
        }
        let mut out = Self::default();
        let mut offset = 0usize;
        let mut body = "";
        for piece in text.split_inclusive([
            '\n', '\r', '\u{000b}', '\u{000c}', '\u{001c}', '\u{001d}', '\u{001e}', '\u{0085}',
            '\u{2028}', '\u{2029}',
        ]) {
            if piece.trim_start_matches(space).starts_with('[') {
                body = text.get(offset..).ok_or(Error::Refused)?;
                break;
            }
            offset = offset.checked_add(piece.len()).ok_or(Error::Refused)?;
        }
        let mut current: Option<usize> = None;
        let mut default_section = false;
        let mut option: Option<String> = None;
        let mut indentation = usize::MAX;
        let mut key_count = 0usize;
        for line in body.split('\n') {
            budget.work()?;
            if line.len() > MAX_VALUE {
                return Err(Error::Refused);
            }
            let value = strip(line);
            if value.starts_with(['#', ';']) {
                continue;
            }
            let items = if default_section {
                Some(&mut out.defaults)
            } else {
                current.and_then(|i| out.sections.get_mut(i).map(|(_, values)| values))
            };
            if value.is_empty() {
                if let (Some(items), Some(key)) = (items, option.as_deref()) {
                    if let Some(old) = items.get_mut(key) {
                        if old.len() >= MAX_VALUE {
                            return Err(Error::Refused);
                        }
                        old.push('\n');
                    }
                }
                continue;
            }
            let indent = line.chars().take_while(|c| space(*c)).count();
            if indent > indentation {
                if let (Some(items), Some(key)) = (items, option.as_deref()) {
                    if let Some(old) = items.get_mut(key) {
                        if old.len().saturating_add(value.len()).saturating_add(1) > MAX_VALUE {
                            return Err(Error::Refused);
                        }
                        old.push('\n');
                        old.push_str(value);
                        continue;
                    }
                }
            }
            indentation = indent;
            if value.starts_with('[') {
                if let Some(end) = value.rfind(']') {
                    if end > 1 {
                        let name = value.get(1..end).ok_or(Error::Refused)?;
                        default_section = name == "DEFAULT";
                        current = if default_section {
                            None
                        } else {
                            match out.positions.get(name).copied() {
                                Some(i) => Some(i),
                                None => {
                                    if out.sections.len() >= MAX_SECTIONS {
                                        return Err(Error::Refused);
                                    }
                                    out.sections.try_reserve(1).map_err(|_| Error::Refused)?;
                                    out.positions.try_reserve(1).map_err(|_| Error::Refused)?;
                                    out.positions.insert(name.to_owned(), out.sections.len());
                                    out.sections.push((name.to_owned(), Ordered::default()));
                                    Some(out.sections.len() - 1)
                                }
                            }
                        };
                        option = None;
                        continue;
                    }
                }
            }
            let delimiter = value.find(['=', ':']).ok_or(Error::Refused)?;
            let key = value
                .get(..delimiter)
                .ok_or(Error::Refused)?
                .trim_end_matches(space);
            if key.is_empty() {
                return Err(Error::Refused);
            }
            let val = strip(value.get(delimiter + 1..).ok_or(Error::Refused)?).to_owned();
            key_count = key_count.checked_add(1).ok_or(Error::Refused)?;
            if key_count > MAX_KEYS {
                return Err(Error::Refused);
            }
            let items = if default_section {
                &mut out.defaults
            } else {
                &mut out
                    .sections
                    .get_mut(current.ok_or(Error::Refused)?)
                    .ok_or(Error::Refused)?
                    .1
            };
            items.put(key, val)?;
            option = Some(key.to_owned());
        }
        for (_, items) in &mut out.sections {
            for (_, value) in &mut items.values {
                *value = value.trim_end_matches(space).to_owned();
            }
        }
        for (_, value) in &mut out.defaults.values {
            *value = value.trim_end_matches(space).to_owned();
        }
        let mut expanded = 0usize;
        for (_, items) in &out.sections {
            budget.work()?;
            expanded = expanded
                .checked_add(items.values.len())
                .ok_or(Error::Refused)?;
            if expanded > MAX_KEYS {
                return Err(Error::Refused);
            }
            for (key, _) in &out.defaults.values {
                if !items.positions.contains_key(key) {
                    expanded = expanded.checked_add(1).ok_or(Error::Refused)?;
                }
                if expanded > MAX_KEYS {
                    return Err(Error::Refused);
                }
            }
        }
        Ok(out)
    }

    pub fn items(&self, name: &str, budget: &mut Budget) -> Result<Option<Items>, Error> {
        let Some(index) = self.positions.get(name) else {
            return Ok(None);
        };
        let (_, own) = self.sections.get(*index).ok_or(Error::Refused)?;
        let size = self
            .defaults
            .values
            .len()
            .checked_add(own.values.len())
            .ok_or(Error::Refused)?;
        if size > MAX_KEYS {
            return Err(Error::Refused);
        }
        let mut bytes = 0usize;
        for (key, value) in self.defaults.values.iter().chain(&own.values) {
            bytes = bytes
                .checked_add(key.len())
                .and_then(|n| n.checked_add(value.len()))
                .and_then(|n| n.checked_add(8))
                .ok_or(Error::Refused)?;
        }
        budget.charge(bytes)?;
        let mut items = Ordered::default();
        items.values.try_reserve(size).map_err(|_| Error::Refused)?;
        items
            .positions
            .try_reserve(size)
            .map_err(|_| Error::Refused)?;
        for (key, val) in &self.defaults.values {
            items.put(key, val.clone())?;
        }
        for (key, val) in &own.values {
            items.put(key, val.clone())?;
        }
        Ok(Some(items.values))
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.sections.iter().map(|(name, _)| name.as_str())
    }
}

pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&c))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

pub fn cli(
    name: &str,
    items: &Items,
    kind: impl Fn(&str) -> Option<&'static str>,
) -> Result<String, Error> {
    let explicit = items
        .iter()
        .find(|(k, v)| k == "model" && !v.is_empty())
        .map(|(_, v)| v.as_str());
    let model = match explicit {
        None => format!("/models/{name}.gguf"),
        Some(v) if v.starts_with('/') => v.to_owned(),
        Some(v) => format!("/models/{v}"),
    };
    let mut text = format!(
        "--alias {} --model {}",
        shell_quote(name),
        shell_quote(&model)
    );
    for (key, value) in items {
        if key == "model" {
            continue;
        }
        let val = strip(value);
        let low = val.to_lowercase();
        let truthy = ["true", "on", "yes", "1"].contains(&low.as_str());
        let falsy = ["false", "off", "no", "0"].contains(&low.as_str());
        let piece = if matches!(kind(key), Some("bool" | "flag")) {
            if falsy {
                continue;
            }
            format!(" --{key}")
        } else {
            if val.is_empty() {
                continue;
            }
            format!(
                " --{key} {}",
                if truthy || falsy {
                    low
                } else {
                    shell_quote(val)
                }
            )
        };
        if text.len().saturating_add(piece.len()) > super::RESPONSE_CAP {
            return Err(Error::Refused);
        }
        text.push_str(&piece);
    }
    Ok(text)
}
