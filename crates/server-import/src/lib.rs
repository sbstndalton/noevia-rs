//! Pure conversation-import planning; no parser, clock, filesystem, HTTP or tenant lookup.
//! See README for the admitted domain and the exact Node predecessor.
#![forbid(unsafe_code)]

use js_json::JValue;
use std::collections::{HashMap, HashSet};

pub const FORMAT: &str = "noevia-conversations-v1";
pub const MAX_CHATS: usize = 2000;
pub const MAX_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DEPTH: usize = 64;
pub const MAX_NODES: usize = 262_144;
pub const MAX_SNAPSHOT_PROJECTS: usize = 2000;
pub const MAX_SNAPSHOT_IDS: usize = 16_000;
pub const DEFAULT_MAX_MESSAGES: usize = 5000;
pub const DEFAULT_MAX_CHARS: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// Not plain JSON (including manually duplicated object keys or nonfinite numbers).
    Input,
    /// Project matching needs ECMAScript Unicode lowercasing outside the admitted ASCII domain.
    GroupingName,
    /// The exact JS UTF-16 prefix ends with a lone high surrogate.
    SurrogateCut,
    /// Plain JSON String coercion would throw (an own noncallable toString).
    Coercion,
    /// A consumed generated ID is outside the admitted safe-ID domain.
    GeneratedId,
    /// No next ID is available, including after collisions.
    IdSupplyExhausted,
    /// The supplied clock/options are outside this foundation's admitted domain.
    Configuration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Envelope,
    TooManyChats,
    Unsupported(Unsupported),
    Limit,
    Allocation,
}
impl Error {
    /// Only errors matching the predecessor's explicit readable400 errors have a public tuple.
    pub fn node_public_error(self) -> Option<(u16, &'static str)> {
        match self {
            Self::Envelope => Some((
                400,
                "Choose a conversations.json from a noevia conversations export.",
            )),
            Self::TooManyChats => Some((
                400,
                "An import holds at most 2000 chats. Split the file and import each part.",
            )),
            _ => None,
        }
    }
}

/// A project from the authenticated tenant's snapshot. `name` stays its original JSON value in
/// the returned group, as Node's `match.name`, although its matching key uses String(name).
#[derive(Debug, Clone, Copy)]
pub struct Project<'a> {
    pub id: &'a str,
    pub name: &'a JValue,
}

/// All collaborators are data: this API never invokes a supplied function or reads a clock.
/// `now` supplies each Date.now fallback. Positive options preserve the predecessor's defaults.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot<'a> {
    pub projects: &'a [Project<'a>],
    pub existing_chat_ids: &'a [&'a str],
    pub tombstones: &'a [&'a str],
    pub generated_ids: &'a [&'a str],
    pub now: f64,
    pub max_messages: usize,
    pub max_chars: usize,
}
impl<'a> Snapshot<'a> {
    pub fn empty(generated_ids: &'a [&'a str], now: f64) -> Self {
        Self {
            projects: &[],
            existing_chat_ids: &[],
            tombstones: &[],
            generated_ids,
            now,
            max_messages: DEFAULT_MAX_MESSAGES,
            max_chars: DEFAULT_MAX_CHARS,
        }
    }
}

#[derive(Default)]
struct Budget {
    bytes: usize,
    nodes: usize,
}
impl Budget {
    fn bytes(&mut self, n: usize) -> Result<(), Error> {
        self.bytes = self.bytes.checked_add(n).ok_or(Error::Limit)?;
        if self.bytes > MAX_BYTES {
            return Err(Error::Limit);
        }
        Ok(())
    }
    fn nodes(&mut self, n: usize) -> Result<(), Error> {
        self.nodes = self.nodes.checked_add(n).ok_or(Error::Limit)?;
        if self.nodes > MAX_NODES {
            return Err(Error::Limit);
        }
        Ok(())
    }
}
fn string_bytes(s: &str) -> Result<usize, Error> {
    s.chars().try_fold(2usize, |n, c| {
        let width = match c {
            '"' | '\\' | '\u{8}' | '\t' | '\n' | '\u{c}' | '\r' => 2,
            c if u32::from(c) < 0x20 => 6,
            c => c.len_utf8(),
        };
        n.checked_add(width).ok_or(Error::Limit)
    })
}

// Charge all sibling values/keys BEFORE reserving a duplicate-key set. A caller-created JValue
// is admitted only as ordinary finite JSON; missing properties still read as Undefined via get.
fn preflight(value: &JValue, depth: usize, budget: &mut Budget) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Limit);
    }
    match value {
        JValue::Undefined => return Err(Error::Unsupported(Unsupported::Input)),
        JValue::Null => budget.bytes(4)?,
        JValue::Bool(v) => budget.bytes(if *v { 4 } else { 5 })?,
        JValue::Num(n) => {
            if !n.is_finite() {
                return Err(Error::Unsupported(Unsupported::Input));
            }
            budget.bytes(js_json::number_to_string(*n).len())?;
        }
        JValue::Str(s) => budget.bytes(string_bytes(s)?)?,
        JValue::Arr(items) => {
            budget.nodes(items.len())?;
            budget.bytes(
                2usize
                    .checked_add(items.len().saturating_sub(1))
                    .ok_or(Error::Limit)?,
            )?;
            for item in items {
                preflight(item, depth + 1, budget)?;
            }
        }
        JValue::Obj(items) => {
            budget.nodes(items.len().checked_mul(2).ok_or(Error::Limit)?)?;
            budget.bytes(
                2usize
                    .checked_add(items.len().saturating_sub(1))
                    .ok_or(Error::Limit)?,
            )?;
            let mut keys = HashSet::new();
            keys.try_reserve(items.len())
                .map_err(|_| Error::Allocation)?;
            for (key, item) in items {
                if !keys.insert(key.as_str()) {
                    return Err(Error::Unsupported(Unsupported::Input));
                }
                budget.bytes(string_bytes(key)?.checked_add(1).ok_or(Error::Limit)?)?;
                preflight(item, depth + 1, budget)?;
            }
        }
    }
    Ok(())
}

struct Buffer(String);
impl Buffer {
    fn new() -> Self {
        Self(String::new())
    }
    fn push(&mut self, text: &str) -> Result<(), Error> {
        let len = self.0.len().checked_add(text.len()).ok_or(Error::Limit)?;
        if len > MAX_BYTES {
            return Err(Error::Limit);
        }
        if len > self.0.capacity() {
            let mut capacity = self.0.capacity().max(64);
            while capacity < len {
                capacity = capacity.checked_mul(2).ok_or(Error::Limit)?.min(MAX_BYTES);
            }
            self.0
                .try_reserve_exact(capacity.checked_sub(self.0.len()).ok_or(Error::Limit)?)
                .map_err(|_| Error::Allocation)?;
        }
        self.0.push_str(text);
        Ok(())
    }
    fn character(&mut self, c: char) -> Result<(), Error> {
        let mut bytes = [0; 4];
        self.push(c.encode_utf8(&mut bytes))
    }
    fn quote(&mut self, text: &str) -> Result<(), Error> {
        self.push("\"")?;
        for c in text.chars() {
            match c {
                '"' => self.push("\\\"")?,
                '\\' => self.push("\\\\")?,
                '\u{8}' => self.push("\\b")?,
                '\t' => self.push("\\t")?,
                '\n' => self.push("\\n")?,
                '\u{c}' => self.push("\\f")?,
                '\r' => self.push("\\r")?,
                c if u32::from(c) < 0x20 => {
                    self.push("\\u00")?;
                    // Only controls0..31 reach this branch. No format-sized temporary.
                    for nibble in [u32::from(c) / 16, u32::from(c) % 16] {
                        let digit = char::from_digit(nibble, 16).ok_or(Error::Limit)?;
                        self.character(digit)?;
                    }
                }
                c => self.character(c)?,
            }
        }
        self.push("\"")
    }
    // Values have already passed preflight; bounded emission never calls js-json::stringify,
    // which uses an infallibly growing String.
    fn value(&mut self, value: &JValue) -> Result<(), Error> {
        match value {
            JValue::Undefined => Err(Error::Unsupported(Unsupported::Input)),
            JValue::Null => self.push("null"),
            JValue::Bool(b) => self.push(if *b { "true" } else { "false" }),
            JValue::Num(n) => self.push(&js_json::number_to_string(*n)),
            JValue::Str(s) => self.quote(s),
            JValue::Arr(items) => {
                self.push("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.push(",")?;
                    }
                    self.value(item)?;
                }
                self.push("]")
            }
            JValue::Obj(items) => {
                self.push("{")?;
                // JSON.stringify orders canonical array-index keys numerically before
                // other strings, even when JSON.parse encountered them out of order.
                // Sorting is O(k log k), within the preflight node cap; no linear claim.
                let mut indexed = Vec::new();
                reserve(&mut indexed, items.len())?;
                for (position, (key, _)) in items.iter().enumerate() {
                    if let Some(index) = array_index(key) {
                        indexed.push((index, position));
                    }
                }
                indexed.sort_unstable_by_key(|entry| entry.0);
                let mut first = true;
                for (_, position) in indexed {
                    let (key, item) = items
                        .get(position)
                        .ok_or(Error::Unsupported(Unsupported::Input))?;
                    if !first {
                        self.push(",")?;
                    }
                    first = false;
                    self.quote(key)?;
                    self.push(":")?;
                    self.value(item)?;
                }
                for (key, item) in items.iter().filter(|(key, _)| array_index(key).is_none()) {
                    if !first {
                        self.push(",")?;
                    }
                    first = false;
                    self.quote(key)?;
                    self.push(":")?;
                    self.value(item)?;
                }
                self.push("}")
            }
        }
    }
}

// A bounded String(value).slice(0,n) builder. It still visits the remaining array items to
// preserve coercion failure after the prefix is full; omitted UTF-16 units never allocate.
struct Prefix {
    text: Buffer,
    units: usize,
    cap: usize,
}
impl Prefix {
    fn new(cap: usize) -> Self {
        Self {
            text: Buffer::new(),
            units: 0,
            cap,
        }
    }
    fn push(&mut self, text: &str) -> Result<(), Error> {
        for c in text.chars() {
            if self.units == self.cap {
                break;
            }
            let next = self.units.checked_add(c.len_utf16()).ok_or(Error::Limit)?;
            if next > self.cap {
                return Err(Error::Unsupported(Unsupported::SurrogateCut));
            }
            self.text.character(c)?;
            self.units = next;
        }
        Ok(())
    }
}
fn coerce(value: &JValue, out: &mut Prefix) -> Result<(), Error> {
    match value {
        JValue::Undefined => out.push("undefined"),
        JValue::Null => out.push("null"),
        JValue::Bool(b) => out.push(if *b { "true" } else { "false" }),
        JValue::Num(n) => out.push(&js_json::number_to_string(*n)),
        JValue::Str(s) => out.push(s),
        JValue::Obj(items) => {
            // JSON cannot supply a callable toString. An own valueOf alone does NOT throw
            // with the String hint: inherited Object.toString succeeds first.
            if items.iter().any(|(key, _)| key == "toString") {
                return Err(Error::Unsupported(Unsupported::Coercion));
            }
            out.push("[object Object]")
        }
        JValue::Arr(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(",")?;
                }
                if !item.is_nullish() {
                    coerce(item, out)?;
                }
            }
            Ok(())
        }
    }
}
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| u16::try_from(u32::from(c)).is_ok_and(policy_leaves::is_js_space))
}
fn prefix(text: &str, cap: usize) -> Result<&str, Error> {
    let mut units = 0usize;
    let mut end = 0;
    for (offset, c) in text.char_indices() {
        if units == cap {
            break;
        }
        units = units.checked_add(c.len_utf16()).ok_or(Error::Limit)?;
        if units > cap {
            return Err(Error::Unsupported(Unsupported::SurrogateCut));
        }
        end = offset.checked_add(c.len_utf8()).ok_or(Error::Limit)?;
    }
    text.get(..end)
        .ok_or(Error::Unsupported(Unsupported::Input))
}
fn lower_ascii(text: &str) -> Result<String, Error> {
    if !text.is_ascii() {
        return Err(Error::Unsupported(Unsupported::GroupingName));
    }
    let mut out = Buffer::new();
    for byte in text.bytes() {
        out.character(char::from(byte.to_ascii_lowercase()))?;
    }
    Ok(out.0)
}
fn safe_id(id: &str) -> bool {
    id.strip_prefix("c-").is_some_and(|tail| {
        (1..=80).contains(&tail.len())
            && tail
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}
fn ids<'a>(input: &'a [&'a str]) -> Result<HashSet<&'a str>, Error> {
    let mut out = HashSet::new();
    out.try_reserve(input.len())
        .map_err(|_| Error::Allocation)?;
    out.extend(input.iter().copied());
    Ok(out)
}
fn reserve<T>(items: &mut Vec<T>, count: usize) -> Result<(), Error> {
    items
        .try_reserve_exact(count)
        .map_err(|_| Error::Allocation)
}

struct Meta<'a> {
    id: &'a str,
    title: String,
    updated_at: f64,
}
struct Message<'a> {
    role: &'static str,
    content: &'a str,
    tools: Vec<&'a str>,
}
struct History<'a> {
    id: &'a str,
    messages: Vec<Message<'a>>,
}
#[derive(Clone, Copy)]
enum Name<'a> {
    Snapshot(&'a JValue),
    Imported(&'a str),
}
struct Group<'a> {
    project_id: Option<&'a str>,
    name: Name<'a>,
    chats: Vec<Meta<'a>>,
}
fn readable_message(value: &JValue) -> Option<(&'static str, &str)> {
    let role = match value.get("role").as_str()? {
        "user" => "user",
        "assistant" => "assistant",
        _ => return None,
    };
    Some((role, value.get("content").as_str()?))
}
fn history(
    value: &JValue,
    max_messages: usize,
    max_chars: usize,
) -> Result<Vec<Message<'_>>, Error> {
    let JValue::Arr(input) = value else {
        return Ok(Vec::new());
    };
    let valid = input
        .iter()
        .filter(|v| readable_message(v).is_some())
        .count();
    let skip = valid.saturating_sub(max_messages);
    let mut out = Vec::new();
    reserve(&mut out, valid.min(max_messages))?;
    for value in input
        .iter()
        .filter(|v| readable_message(v).is_some())
        .skip(skip)
    {
        let (role, text) = readable_message(value).ok_or(Error::Unsupported(Unsupported::Input))?;
        let content = prefix(text, max_chars)?;
        let mut tools = Vec::new();
        if let JValue::Arr(input) = value.get("toolCalls") {
            let names = input.iter().filter_map(|t| t.get("name").as_str()).take(50);
            // Exact upper capacity known before growth, after global input node admission.
            reserve(&mut tools, input.len().min(50))?;
            for name in names {
                tools.push(prefix(name, 120)?);
            }
        }
        out.push(Message {
            role,
            content,
            tools,
        });
    }
    Ok(out)
}
fn emit_meta(out: &mut Buffer, meta: &Meta<'_>) -> Result<(), Error> {
    out.push("{\"id\":")?;
    out.quote(meta.id)?;
    out.push(",\"title\":")?;
    out.quote(&meta.title)?;
    out.push(",\"updatedAt\":")?;
    out.push(&js_json::number_to_string(meta.updated_at))?;
    out.push("}")
}
fn emit_metas(out: &mut Buffer, metas: &[Meta<'_>]) -> Result<(), Error> {
    out.push("[")?;
    for (i, meta) in metas.iter().enumerate() {
        if i > 0 {
            out.push(",")?;
        }
        emit_meta(out, meta)?;
    }
    out.push("]")
}
fn emit_history(out: &mut Buffer, history: &History<'_>) -> Result<(), Error> {
    out.quote(history.id)?;
    out.push(":[")?;
    for (i, message) in history.messages.iter().enumerate() {
        if i > 0 {
            out.push(",")?;
        }
        out.push("{\"role\":")?;
        out.quote(message.role)?;
        out.push(",\"content\":")?;
        out.quote(message.content)?;
        if !message.tools.is_empty() {
            out.push(",\"toolCalls\":[")?;
            for (i, name) in message.tools.iter().enumerate() {
                if i > 0 {
                    out.push(",")?;
                }
                out.push("{\"name\":")?;
                out.quote(name)?;
                out.push(",\"status\":\"done\"}")?;
            }
            out.push("]")?;
        }
        out.push("}")?;
    }
    out.push("]")
}

/// Return the whole plan as JSON, in Node's field/insertion order, within MAX_BYTES.
///
/// Caller parsing/allocation is OUTSIDE this API's contract. This function preflights an already
/// constructed finite plain-data JValue and the tenant snapshot before building any plan. Data
/// plus snapshot string/name serialization must total at most64MiB; aggregate values+keys are
/// capped at262144 nodes and depth64. It returns a stricter typed refusal for unsupported input,
/// never a substituted public400 error or silent replacement text. Logical bounds are not RSS/OOM
/// guarantees. Only the future authenticated Rust caller may apply this plan to its own tenant.
pub fn plan_import<'a>(data: &'a JValue, snapshot: Snapshot<'a>) -> Result<String, Error> {
    // Preserve the predecessor's two readable validation errors before stricter admission.
    if !data.truthy() || data.type_of() != "object" || data.get("format").as_str() != Some(FORMAT) {
        return Err(Error::Envelope);
    }
    let JValue::Arr(chats) = data.get("chats") else {
        return Err(Error::Envelope);
    };
    if chats.len() > MAX_CHATS {
        return Err(Error::TooManyChats);
    }
    if !snapshot.now.is_finite()
        || snapshot.max_messages == 0
        || snapshot.max_messages > DEFAULT_MAX_MESSAGES
        || snapshot.max_chars == 0
        || snapshot.max_chars > DEFAULT_MAX_CHARS
    {
        return Err(Error::Unsupported(Unsupported::Configuration));
    }
    if snapshot.projects.len() > MAX_SNAPSHOT_PROJECTS
        || [
            snapshot.existing_chat_ids.len(),
            snapshot.tombstones.len(),
            snapshot.generated_ids.len(),
        ]
        .into_iter()
        .any(|n| n > MAX_SNAPSHOT_IDS)
    {
        return Err(Error::Limit);
    }
    let mut budget = Budget::default();
    budget.nodes(1)?;
    preflight(data, 0, &mut budget)?;
    // Count a compact JSON representation of the explicit snapshot alongside data. Includes
    // field names, delimiters and scalar options, not just the strings' payload bytes.
    budget.nodes(15)?;
    budget.bytes("{\"projects\":[],\"existingChatIds\":[],\"tombstones\":[],\"generatedIds\":[],\"now\":,\"maxMessages\":,\"maxChars\":}".len())?;
    budget.bytes(js_json::number_to_string(snapshot.now).len())?;
    for count in [snapshot.max_messages, snapshot.max_chars] {
        let count = u32::try_from(count).map_err(|_| Error::Limit)?;
        budget.bytes(js_json::number_to_string(f64::from(count)).len())?;
    }
    budget.bytes(snapshot.projects.len().saturating_sub(1))?;
    budget.nodes(snapshot.projects.len().checked_mul(5).ok_or(Error::Limit)?)?;
    for project in snapshot.projects {
        budget.bytes("{\"id\":,\"name\":}".len())?;
        budget.bytes(string_bytes(project.id)?)?;
        preflight(project.name, 0, &mut budget)?;
    }
    for list in [
        snapshot.existing_chat_ids,
        snapshot.tombstones,
        snapshot.generated_ids,
    ] {
        budget.nodes(list.len())?;
        budget.bytes(list.len().saturating_sub(1))?;
        for id in list {
            budget.bytes(string_bytes(id)?)?;
        }
    }
    let existing = ids(snapshot.existing_chat_ids)?;
    let tombstones = ids(snapshot.tombstones)?;
    let mut used = HashSet::new();
    used.try_reserve(chats.len())
        .map_err(|_| Error::Allocation)?;
    let mut by_name = HashMap::new();
    by_name
        .try_reserve(snapshot.projects.len())
        .map_err(|_| Error::Allocation)?;
    let mut key_bytes = 0usize;
    for project in snapshot.projects {
        let mut name = Prefix::new(MAX_BYTES);
        coerce(project.name, &mut name)?;
        // Prefix MAX_BYTES cannot silently truncate a long snapshot key: the entire input is
        // admitted within64MiB, while a full coercion can expand tiny objects/array commas.
        // Coercion for keys must use the full string; refuse if the cap was reached exactly.
        if name.units == MAX_BYTES {
            return Err(Error::Limit);
        }
        let trimmed = trim(&name.text.0);
        key_bytes = key_bytes.checked_add(trimmed.len()).ok_or(Error::Limit)?;
        if key_bytes > MAX_BYTES {
            return Err(Error::Limit);
        }
        let key = lower_ascii(trimmed)?;
        by_name.insert(key, project);
    }
    let mut free = Vec::new();
    let mut groups: Vec<Group<'a>> = Vec::new();
    let mut group_keys = HashMap::new();
    let mut histories = Vec::new();
    let mut skipped = Vec::new();
    for list in [&mut free, &mut skipped] {
        reserve(list, chats.len())?;
    }
    reserve(&mut groups, chats.len())?;
    reserve(&mut histories, chats.len())?;
    group_keys
        .try_reserve(chats.len())
        .map_err(|_| Error::Allocation)?;
    let mut generated = snapshot.generated_ids.iter().copied();
    for raw in chats {
        if !raw.truthy() || raw.type_of() != "object" {
            continue;
        }
        let mut title = Prefix::new(200);
        if raw.get("title").truthy() {
            coerce(raw.get("title"), &mut title)?;
        }
        let title = title.text.0;
        let supplied = raw.get("id").as_str().unwrap_or("");
        if safe_id(supplied) && existing.contains(supplied) {
            skipped.push(Meta {
                id: "",
                title,
                updated_at: 0.0,
            });
            continue;
        }
        let mut id =
            if safe_id(supplied) && !tombstones.contains(supplied) && !used.contains(supplied) {
                supplied
            } else {
                next_id(&mut generated)?
            };
        while used.contains(id) || existing.contains(id) {
            id = next_id(&mut generated)?;
        }
        used.insert(id);
        let updated_at = raw
            .get("updatedAt")
            .as_num()
            .filter(|n| n.is_finite())
            .unwrap_or(snapshot.now);
        let meta = Meta {
            id,
            title,
            updated_at,
        };
        histories.push(History {
            id,
            messages: history(
                raw.get("history"),
                snapshot.max_messages,
                snapshot.max_chars,
            )?,
        });
        let name = raw.get("project").get("name").as_str().unwrap_or("");
        let name = prefix(trim(name), 120)?;
        if name.is_empty() {
            free.push(meta);
            continue;
        }
        let key = lower_ascii(name)?;
        let group_index = if let Some(index) = group_keys.get(&key) {
            *index
        } else {
            let index = groups.len();
            let matched = by_name.get(&key).copied();
            let mut members = Vec::new();
            reserve(&mut members, 1)?;
            groups.push(Group {
                project_id: matched.map(|p| p.id),
                name: matched.map_or(Name::Imported(name), |p| Name::Snapshot(p.name)),
                chats: members,
            });
            group_keys.insert(key, index);
            index
        };
        let group = groups
            .get_mut(group_index)
            .ok_or(Error::Unsupported(Unsupported::Input))?;
        // Bounded geometric Vec growth, never an unchecked input-derived push allocation.
        if group.chats.len() == group.chats.capacity() {
            let additional = group.chats.capacity().max(1).min(
                MAX_CHATS
                    .checked_sub(group.chats.len())
                    .ok_or(Error::Limit)?,
            );
            group
                .chats
                .try_reserve_exact(additional)
                .map_err(|_| Error::Allocation)?;
        }
        group.chats.push(meta);
    }
    let mut out = Buffer::new();
    out.push("{\"freeChats\":")?;
    emit_metas(&mut out, &free)?;
    out.push(",\"projectChats\":[")?;
    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            out.push(",")?;
        }
        out.push("{\"projectId\":")?;
        if let Some(id) = group.project_id {
            out.quote(id)?;
        } else {
            out.push("null")?;
        }
        out.push(",\"name\":")?;
        match group.name {
            Name::Snapshot(v) => out.value(v)?,
            Name::Imported(s) => out.quote(s)?,
        }
        out.push(",\"chats\":")?;
        emit_metas(&mut out, &group.chats)?;
        out.push("}")?;
    }
    out.push("],\"histories\":{")?;
    for (i, history) in histories.iter().enumerate() {
        if i > 0 {
            out.push(",")?;
        }
        emit_history(&mut out, history)?;
    }
    out.push("},\"skipped\":[")?;
    for (i, meta) in skipped.iter().enumerate() {
        if i > 0 {
            out.push(",")?;
        }
        out.push("{\"title\":")?;
        out.quote(if meta.title.is_empty() {
            "Untitled chat"
        } else {
            &meta.title
        })?;
        out.push(",\"reason\":\"already in this account\"}")?;
    }
    out.push("],\"imported\":")?;
    let count = u32::try_from(histories.len()).map_err(|_| Error::Limit)?;
    out.push(&js_json::number_to_string(f64::from(count)))?;
    out.push("}")?;
    Ok(out.0)
}
fn next_id<'a>(generated: &mut impl Iterator<Item = &'a str>) -> Result<&'a str, Error> {
    let id = generated
        .next()
        .ok_or(Error::Unsupported(Unsupported::IdSupplyExhausted))?;
    if !safe_id(id) {
        return Err(Error::Unsupported(Unsupported::GeneratedId));
    }
    // Deliberately no tombstone check: the actual generated-ID loop tests only used/existing.
    Ok(id)
}

fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    let index = key.bytes().try_fold(0u32, |n, b| {
        if !b.is_ascii_digit() {
            return None;
        }
        n.checked_mul(10)?.checked_add(u32::from(b - b'0'))
    })?;
    (index < u32::MAX).then_some(index)
}
