//! CPython 3.12 `configparser.ConfigParser(strict=False, interpolation=None)` with
//! `optionxform = str`, as services `ini.py` uses it for writes, plus `ini.py`'s own text rules:
//! `split_preamble`, `duplicate_sections`, `_raw_layout`, `_render_preserving`, `_rename_header`
//! and `_render_full` (noevia#1318). Every function mirrors the named Python one; the module is
//! differential-tested against the actual services code.

/// Python `str.isspace` (what `strip()` and `\s` use).
pub fn space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{001c}'..='\u{001f}')
}

pub fn strip(s: &str) -> &str {
    s.trim_matches(space)
}

fn lstrip(s: &str) -> &str {
    s.trim_start_matches(space)
}

fn rstrip(s: &str) -> &str {
    s.trim_end_matches(space)
}

/// `str.splitlines(keepends=True)`.
pub fn splitlines_keep(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let end = match c {
            '\r' => {
                if let Some(&(j, '\n')) = chars.peek() {
                    chars.next();
                    j + 1
                } else {
                    i + 1
                }
            }
            '\n' | '\u{000b}' | '\u{000c}' | '\u{001c}' | '\u{001d}' | '\u{001e}' | '\u{0085}'
            | '\u{2028}' | '\u{2029}' => i + c.len_utf8(),
            _ => continue,
        };
        out.push(&text[start..end]);
        start = end;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// `str.splitlines()` (no line ends).
pub fn splitlines(text: &str) -> Vec<&str> {
    splitlines_keep(text)
        .into_iter()
        .map(|l| {
            l.strip_suffix("\r\n")
                .or_else(|| {
                    l.chars()
                        .last()
                        .filter(|c| {
                            matches!(
                                c,
                                '\n' | '\r'
                                    | '\u{000b}'
                                    | '\u{000c}'
                                    | '\u{001c}'
                                    | '\u{001d}'
                                    | '\u{001e}'
                                    | '\u{0085}'
                                    | '\u{2028}'
                                    | '\u{2029}'
                            )
                        })
                        .map(|c| &l[..l.len() - c.len_utf8()])
                })
                .unwrap_or(l)
        })
        .collect()
}

/// An ordered `dict` of str to str (assignment keeps a key's first position).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dict(pub Vec<(String, String)>);

impl Dict {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    }
    pub fn set(&mut self, k: &str, v: &str) {
        match self.0.iter_mut().find(|(key, _)| key == k) {
            Some(slot) => slot.1 = v.to_owned(),
            None => self.0.push((k.to_owned(), v.to_owned())),
        }
    }
    pub fn remove(&mut self, k: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|(key, _)| key != k);
        before != self.0.len()
    }
    pub fn contains(&self, k: &str) -> bool {
        self.get(k).is_some()
    }
    /// Python dict equality: same keys and values, any order.
    pub fn same(&self, other: &Dict) -> bool {
        self.0.len() == other.0.len() && self.0.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Parser {
    pub preamble: String,
    /// The text the parser was read from (`_noevia_source`), or `None` for a fresh parser.
    pub source: Option<String>,
    pub defaults: Dict,
    pub sections: Vec<(String, Dict)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// `DuplicateSectionsError`, names in first-duplicate order.
    Duplicate(Vec<String>),
    /// Any other `configparser.Error`.
    Invalid,
}

impl ParseError {
    /// `str(DuplicateSectionsError)`.
    pub fn message(&self) -> Option<String> {
        match self {
            ParseError::Duplicate(names) => Some(format!(
                "models.ini has duplicate sections: {}",
                names
                    .iter()
                    .map(|n| format!("[{n}]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            ParseError::Invalid => None,
        }
    }
}

/// `_SECTION_HEADER_RE.match(line)`: the header name's byte span in `line`.
pub fn header_match(line: &str) -> Option<(usize, usize)> {
    let rest = lstrip(line);
    let open = line.len() - rest.len();
    let inner = rest.strip_prefix('[')?;
    let name_len = inner.find([']', '\r', '\n']).unwrap_or(inner.len());
    if name_len == 0 || !inner[name_len..].starts_with(']') {
        return None;
    }
    let after = lstrip(&inner[name_len + 1..]);
    // `$` also matches before one trailing newline.
    let after = after.strip_suffix('\n').unwrap_or(after);
    let ok = after.is_empty() || (after.starts_with([';', '#']) && !after.contains('\n'));
    ok.then_some((open + 1, open + 1 + name_len))
}

/// `duplicate_sections(text)`.
pub fn duplicate_sections(text: &str) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut dups: Vec<String> = Vec::new();
    for line in splitlines(text) {
        if let Some((a, b)) = header_match(line) {
            let name = &line[a..b];
            if seen.contains(&name) && !dups.iter().any(|d| d == name) {
                dups.push(name.to_owned());
            }
            seen.push(name);
        }
    }
    dups
}

/// `split_preamble(text)`.
pub fn split_preamble(text: &str) -> (&str, &str) {
    let mut offset = 0;
    for line in splitlines_keep(text) {
        if lstrip(line).starts_with('[') {
            return (&text[..offset], &text[offset..]);
        }
        offset += line.len();
    }
    (text, "")
}

/// OPTCRE / `_OPTION_RE`: `(?P<option>.*?)\s*[=:]\s*(?P<value>.*)$` on a line without `\n`.
fn option_match(value: &str) -> Option<(&str, &str)> {
    let p = value.find(['=', ':'])?;
    let option = rstrip(&value[..p]);
    let rest = lstrip(&value[p + 1..]);
    Some((option, rest))
}

/// `parse_ini_text(text)` (duplicates refused).
pub fn parse(text: &str) -> Result<Parser, ParseError> {
    let dups = duplicate_sections(text);
    if !dups.is_empty() {
        return Err(ParseError::Duplicate(dups));
    }
    parse_allowing(text)
}

/// A section's keys before their value lines are joined.
type Pending = Vec<(String, Vec<String>)>;

/// `parse_ini_text(text, allow_duplicates=True)`.
pub fn parse_allowing(text: &str) -> Result<Parser, ParseError> {
    let (preamble, body) = split_preamble(text);
    // Values are lists until joined; `None` marks the DEFAULT section.
    let mut sections: Vec<(String, Pending)> = Vec::new();
    let mut defaults: Vec<(String, Vec<String>)> = Vec::new();
    // Cursor: Some(Some(i)) a section, Some(None) DEFAULT, None no section yet.
    let mut cursect: Option<Option<usize>> = None;
    let mut optname: Option<String> = None;
    let mut indent_level = 0usize;
    let mut error = false;
    for line in body.split_inclusive('\n') {
        let comment = strip(line).starts_with(['#', ';']);
        let value = if comment { "" } else { strip(line) };
        if value.is_empty() {
            if !comment {
                if let (Some(cs), Some(name)) = (cursect, optname.as_deref()) {
                    if !name.is_empty() {
                        let list = match cs {
                            Some(i) => sections.get_mut(i).map(|(_, v)| v),
                            None => Some(&mut defaults),
                        };
                        if let Some(vals) =
                            list.and_then(|l| l.iter_mut().find(|(k, _)| k == name).map(|(_, v)| v))
                        {
                            vals.push(String::new());
                        }
                    }
                }
            }
            continue;
        }
        let cur_indent = line.chars().take_while(|c| space(*c)).count();
        let continuation = cursect.is_some()
            && optname.as_deref().is_some_and(|n| !n.is_empty())
            && cur_indent > indent_level;
        if continuation {
            let name = optname.clone().unwrap_or_default();
            let list = match cursect.flatten() {
                Some(i) => sections.get_mut(i).map(|(_, v)| v),
                None => Some(&mut defaults),
            };
            if let Some(vals) =
                list.and_then(|l| l.iter_mut().find(|(k, _)| *k == name).map(|(_, v)| v))
            {
                vals.push(value.to_owned());
            }
            continue;
        }
        indent_level = cur_indent;
        // SECTCRE: `\[(?P<header>.+)\]` matched at the start, greedy to the last `]`.
        let header = value
            .strip_prefix('[')
            .and_then(|rest| rest.rfind(']').filter(|&e| e > 0).map(|e| &rest[..e]))
            .filter(|h| !h.contains('\n'));
        if let Some(name) = header {
            if let Some(i) = sections.iter().position(|(n, _)| n == name) {
                cursect = Some(Some(i));
            } else if name == "DEFAULT" {
                cursect = Some(None);
            } else {
                sections.push((name.to_owned(), Vec::new()));
                cursect = Some(Some(sections.len() - 1));
            }
            optname = None;
            continue;
        }
        let Some(cs) = cursect else {
            // MissingSectionHeaderError is raised at once.
            return Err(ParseError::Invalid);
        };
        match option_match(value) {
            Some((option, optval)) => {
                if option.is_empty() {
                    error = true;
                }
                let list = match cs {
                    Some(i) => sections.get_mut(i).map(|(_, v)| v),
                    None => Some(&mut defaults),
                };
                if let Some(list) = list {
                    let v = vec![strip(optval).to_owned()];
                    match list.iter_mut().find(|(k, _)| k == option) {
                        Some(slot) => slot.1 = v,
                        None => list.push((option.to_owned(), v)),
                    }
                }
                optname = Some(option.to_owned());
            }
            None => error = true,
        }
    }
    if error {
        return Err(ParseError::Invalid);
    }
    let join = |items: Vec<(String, Vec<String>)>| {
        Dict(
            items
                .into_iter()
                .map(|(k, v)| (k, rstrip(&v.join("\n")).to_owned()))
                .collect(),
        )
    };
    Ok(Parser {
        preamble: preamble.to_owned(),
        source: Some(text.to_owned()),
        defaults: join(defaults),
        sections: sections.into_iter().map(|(n, v)| (n, join(v))).collect(),
    })
}

impl Parser {
    pub fn has_section(&self, name: &str) -> bool {
        self.sections.iter().any(|(n, _)| n == name)
    }
    pub fn section(&self, name: &str) -> Option<&Dict> {
        self.sections
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, d)| d)
    }
    pub fn section_mut(&mut self, name: &str) -> Option<&mut Dict> {
        self.sections
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, d)| d)
    }
    pub fn names(&self) -> Vec<&str> {
        self.sections.iter().map(|(n, _)| n.as_str()).collect()
    }
    /// `cp.items(name)` / `cp.options(name)`: DEFAULT's keys, then the section's own new ones,
    /// own values winning.
    pub fn items(&self, name: &str) -> Option<Dict> {
        let own = self.section(name)?;
        let mut d = self.defaults.clone();
        for (k, v) in &own.0 {
            d.set(k, v);
        }
        Some(d)
    }
    /// `cp.get(name, key, fallback=...)`.
    pub fn get(&self, name: &str, key: &str) -> Option<String> {
        self.section(name)?
            .get(key)
            .or_else(|| self.defaults.get(key))
            .map(str::to_owned)
    }
    pub fn remove_section(&mut self, name: &str) -> bool {
        let before = self.sections.len();
        self.sections.retain(|(n, _)| n != name);
        before != self.sections.len()
    }
    /// `add_section`; `false` where Python raises (DEFAULT, or an existing name).
    pub fn add_section(&mut self, name: &str) -> bool {
        if name == "DEFAULT" || self.has_section(name) {
            return false;
        }
        self.sections.push((name.to_owned(), Dict::default()));
        true
    }
    /// `_own_sections(cp)` equality (dict ==, order-free).
    pub fn same_sections(&self, other: &[(String, Dict)]) -> bool {
        self.sections.len() == other.len()
            && self.sections.iter().all(|(n, d)| {
                other
                    .iter()
                    .find(|(m, _)| m == n)
                    .is_some_and(|(_, e)| d.same(e))
            })
    }
}

/// `_render_full(cp)`: configparser.write(space_around_delimiters=True), after the preamble.
pub fn render_full(cp: &Parser) -> String {
    let mut out = String::new();
    let section = |name: &str, items: &Dict, out: &mut String| {
        out.push_str(&format!("[{name}]\n"));
        for (k, v) in &items.0 {
            out.push_str(&format!("{k} = {}\n", v.replace('\n', "\n\t")));
        }
        out.push('\n');
    };
    if !cp.defaults.0.is_empty() {
        section("DEFAULT", &cp.defaults, &mut out);
    }
    for (name, items) in &cp.sections {
        section(name, items, &mut out);
    }
    let mut preamble = cp.preamble.clone();
    if !preamble.is_empty() && !preamble.ends_with('\n') {
        preamble.push('\n');
    }
    preamble + &out
}

#[derive(Debug)]
struct Key {
    name: String,
    first: usize,
    end: usize,
    indent: usize,
}

#[derive(Debug)]
struct Layout {
    name: String,
    header: usize,
    span: (usize, usize),
    keys: Vec<Key>,
}

/// `_raw_layout(text)`.
fn raw_layout(lines: &[&str]) -> Vec<Layout> {
    let first = lines
        .iter()
        .position(|l| lstrip(l).starts_with('['))
        .unwrap_or(lines.len());
    let mut sections: Vec<Layout> = Vec::new();
    let mut key: Option<usize> = None; // index into the current section's keys
    for (i, line) in lines.iter().enumerate().skip(first) {
        let bare = line.trim_end_matches(['\r', '\n']);
        let stripped = strip(bare);
        let indent = bare.chars().count() - lstrip(bare).chars().count();
        if let (Some(k), Some(cur)) = (key, sections.last_mut()) {
            if !stripped.is_empty()
                && !stripped.starts_with(['#', ';'])
                && cur.keys.get(k).is_some_and(|key| indent > key.indent)
            {
                if let Some(key) = cur.keys.get_mut(k) {
                    key.end = i + 1;
                }
                continue;
            }
        }
        if let Some(span) = header_match(bare) {
            sections.push(Layout {
                name: bare[span.0..span.1].to_owned(),
                header: i,
                span,
                keys: Vec::new(),
            });
            key = None;
            continue;
        }
        let Some(cur) = sections.last_mut() else {
            key = None;
            continue;
        };
        if stripped.is_empty() || stripped.starts_with(['#', ';']) {
            key = None;
            continue;
        }
        match option_match(stripped) {
            Some((option, _)) => {
                cur.keys.push(Key {
                    name: rstrip(option).to_owned(),
                    first: i,
                    end: i + 1,
                    indent,
                });
                key = Some(cur.keys.len() - 1);
            }
            None => key = None,
        }
    }
    sections
}

fn format_option(key: &str, value: &str, newline: &str) -> String {
    format!("{key} = {value}").replace('\n', &format!("{newline}\t")) + newline
}

fn line_ending<'a>(line: &str, default: &'a str) -> &'a str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        default
    }
}

/// `_render_preserving(source, cp)`.
pub fn render_preserving(source: &str, cp: &Parser) -> Option<String> {
    let old: Vec<(String, Dict)> = if strip(source).is_empty() {
        Vec::new()
    } else {
        parse(source).ok()?.sections
    };
    let new = &cp.sections;
    let nl = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let lines = splitlines_keep(source);
    let layout = raw_layout(&lines);
    let old_get = |n: &str| old.iter().find(|(m, _)| m == n).map(|(_, d)| d);
    let new_get = |n: &str| new.iter().find(|(m, _)| m == n).map(|(_, d)| d);
    let mut listed: Vec<&str> = layout
        .iter()
        .map(|s| s.name.as_str())
        .filter(|n| old_get(n).is_some())
        .collect();
    listed.sort_unstable();
    let mut olds: Vec<&str> = old.iter().map(|(n, _)| n.as_str()).collect();
    olds.sort_unstable();
    if listed != olds {
        return None;
    }
    let mut replace: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut after: std::collections::HashMap<usize, Vec<String>> = std::collections::HashMap::new();
    for sec in &layout {
        let Some(o) = old_get(&sec.name) else {
            continue;
        };
        let Some(n) = new_get(&sec.name) else {
            let stop = sec
                .keys
                .iter()
                .map(|k| k.end)
                .max()
                .unwrap_or(sec.header + 1);
            for i in sec.header..stop {
                replace.insert(i, String::new());
            }
            let mut j = stop;
            while lines.get(j).is_some_and(|l| strip(l).is_empty()) {
                j += 1;
            }
            let prev_blank = sec.header == 0
                || lines
                    .get(sec.header - 1)
                    .is_some_and(|l| strip(l).is_empty());
            if j == lines.len() {
                for i in stop..j {
                    replace.insert(i, String::new());
                }
                let mut k = sec.header as isize - 1;
                while k >= 0
                    && lines.get(k as usize).is_some_and(|l| strip(l).is_empty())
                    && replace.get(&(k as usize)).map(String::as_str) != Some("")
                {
                    replace.insert(k as usize, String::new());
                    k -= 1;
                }
            } else if prev_blank {
                for i in stop..j {
                    replace.insert(i, String::new());
                }
            }
            continue;
        };
        if o.same(n) {
            continue;
        }
        let mut spans: Vec<(&str, Vec<&Key>)> = Vec::new();
        for k in &sec.keys {
            match spans.iter_mut().find(|(name, _)| *name == k.name) {
                Some((_, v)) => v.push(k),
                None => spans.push((&k.name, vec![k])),
            }
        }
        for (key, kspans) in &spans {
            match n.get(key) {
                None => {
                    for span in kspans {
                        for i in span.first..span.end {
                            replace.insert(i, String::new());
                        }
                    }
                }
                Some(value) if o.get(key) != Some(value) => {
                    let last = kspans.last()?;
                    for i in last.first..last.end {
                        replace.insert(i, String::new());
                    }
                    replace.insert(
                        last.first,
                        format_option(
                            key,
                            value,
                            line_ending(lines.get(last.end - 1).copied().unwrap_or(""), nl),
                        ),
                    );
                }
                Some(_) => {}
            }
        }
        let added: Vec<(&String, &String)> =
            n.0.iter()
                .filter(|(k, _)| !spans.iter().any(|(s, _)| s == k))
                .map(|(k, v)| (k, v))
                .collect();
        if !added.is_empty() {
            let anchor = sec
                .keys
                .iter()
                .map(|k| k.end)
                .max()
                .unwrap_or(sec.header + 1)
                - 1;
            let line = lines.get(anchor)?;
            let ending = line_ending(line, nl);
            if !replace.contains_key(&anchor) && !line.ends_with('\n') {
                replace.insert(anchor, format!("{line}{nl}"));
            }
            after.insert(
                anchor,
                added
                    .iter()
                    .map(|(k, v)| format_option(k, v, ending))
                    .collect(),
            );
        }
    }
    let mut text = String::new();
    for (i, line) in lines.iter().enumerate() {
        match replace.get(&i) {
            Some(r) => text.push_str(r),
            None => text.push_str(line),
        }
        if let Some(extra) = after.get(&i) {
            for e in extra {
                text.push_str(e);
            }
        }
    }
    let appended: Vec<&(String, Dict)> = new.iter().filter(|(n, _)| old_get(n).is_none()).collect();
    if !appended.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push_str(nl);
        }
        for (name, items) in appended {
            if !strip(&text).is_empty() && !text.ends_with(&format!("{nl}{nl}")) && text != nl {
                text.push_str(nl);
            }
            text.push_str(&format!("[{name}]{nl}"));
            for (k, v) in &items.0 {
                text.push_str(&format_option(k, v, nl));
            }
        }
    }
    let check = parse(&text).ok()?;
    if !check.same_sections(new) || check.preamble != cp.preamble {
        return None;
    }
    Some(text)
}

/// `_rename_header(source, cp, old, new)`.
pub fn rename_header(source: &str, cp: &Parser, old: &str, new: &str) -> Option<String> {
    if source.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = splitlines_keep(source)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
    let hits: Vec<(usize, (usize, usize))> = raw_layout(&borrowed)
        .into_iter()
        .filter(|s| s.name == old)
        .map(|s| (s.header, s.span))
        .collect();
    let [(header, (a, b))] = hits.as_slice() else {
        return None;
    };
    let line = lines.get(*header)?.clone();
    let renamed = format!("{}{new}{}", line.get(..*a)?, line.get(*b..)?);
    *lines.get_mut(*header)? = renamed;
    let text: String = lines.concat();
    let mut expected = cp.sections.clone();
    let i = expected.iter().position(|(n, _)| n == old)?;
    let (_, items) = expected.remove(i);
    expected.push((new.to_owned(), items));
    let check = parse(&text).ok()?;
    check.same_sections(&expected).then_some(text)
}

/// `_atomic_write`'s text: an in-place edit of the source when possible, else a full render.
pub fn render(cp: &Parser) -> String {
    cp.source
        .as_deref()
        .and_then(|s| render_preserving(s, cp))
        .unwrap_or_else(|| render_full(cp))
}
