//! Pure `noevia-diary-backup-v1` format, not workspace ZIP or whole-app backup.
//! No filesystem, database, network, activation or clock operations.
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const FORMAT: &str = "noevia-diary-backup-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Limit,
    Invalid,
    Format,
    Namespace,
    Tenant,
    Checksum,
    UnsupportedInput,
}
pub type Result<T> = std::result::Result<T, Error>;

/// Every caller supplies explicit finite budgets. These are not production policy.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub manifest_bytes: usize,
    pub files: usize,
    pub directories: usize,
    pub json_depth: usize,
    pub json_nodes: usize,
    pub string_bytes: usize,
    pub payload_bytes: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PythonJson {
    Null,
    Bool(bool),
    Integer(String),
    Float(f64),
    String(String),
    Array(Vec<PythonJson>),
    Object(BTreeMap<String, PythonJson>),
}

/// CPython repr/JSON float spelling. Nonfinite values match json.dumps default.
/// Python str(float) uses lowercase nan/inf instead, so this is not that API.
pub fn python_float(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value == f64::INFINITY {
        return "Infinity".into();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".into();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .into();
    }
    let mut buffer = ryu::Buffer::new();
    let raw = buffer.format_finite(value.abs());
    let (mantissa, exponent) = match raw.split_once('e') {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (raw, 0),
    };
    let integer_len = mantissa.find('.').unwrap_or(mantissa.len());
    let mut digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let leading = digits.bytes().take_while(|c| *c == b'0').count();
    let mut power = exponent + integer_len as i32 - 1;
    if leading > 0 {
        digits = digits.chars().skip(leading).collect();
        power -= leading as i32;
    }
    while digits.ends_with('0') && digits.len() > 1 {
        digits.pop();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if !(-4..16).contains(&power) {
        let first = digits.get(..1).unwrap_or("");
        let tail = digits.get(1..).unwrap_or("");
        let fraction = if tail.is_empty() {
            String::new()
        } else {
            format!(".{tail}")
        };
        let exponent_sign = if power >= 0 { '+' } else { '-' };
        format!(
            "{sign}{first}{fraction}e{exponent_sign}{:02}",
            power.unsigned_abs()
        )
    } else {
        let position = power + 1;
        if position <= 0 {
            format!("{sign}0.{}{digits}", "0".repeat((-position) as usize))
        } else if position as usize >= digits.len() {
            format!(
                "{sign}{digits}{}.0",
                "0".repeat(position as usize - digits.len())
            )
        } else {
            let (left, right) = digits.split_at(position as usize);
            format!("{sign}{left}.{right}")
        }
    }
}

fn integer_text(text: &str) -> Result<&str> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty()
        || !digits.bytes().all(|c| c.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return Err(Error::Invalid);
    }
    Ok(if text == "-0" { "0" } else { text })
}

impl PythonJson {
    pub fn parse(bytes: &[u8], limits: Limits) -> Result<Self> {
        if bytes.len() > limits.manifest_bytes {
            return Err(Error::Limit);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| Error::Invalid)?;
        let mut parser = Parser {
            text,
            offset: 0,
            nodes: 0,
            limits,
        };
        let value = parser.value(0)?;
        parser.space();
        if parser.offset != text.len() {
            return Err(Error::Invalid);
        }
        Ok(value)
    }

    pub fn canonical_bytes(&self, limits: Limits) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut nodes = 0;
        self.write(&mut output, limits, 0, &mut nodes)?;
        Ok(output)
    }

    fn write(
        &self,
        out: &mut Vec<u8>,
        limits: Limits,
        depth: usize,
        nodes: &mut usize,
    ) -> Result<()> {
        *nodes = nodes.checked_add(1).ok_or(Error::Limit)?;
        if depth > limits.json_depth || *nodes > limits.json_nodes {
            return Err(Error::Limit);
        }
        match self {
            Self::Null => append(out, b"null", limits)?,
            Self::Bool(v) => append(out, if *v { b"true" } else { b"false" }, limits)?,
            Self::Integer(v) => {
                if v.len() > limits.manifest_bytes {
                    return Err(Error::Limit);
                }
                append(out, integer_text(v)?.as_bytes(), limits)?;
            }
            Self::Float(v) => append(out, python_float(*v).as_bytes(), limits)?,
            Self::String(v) => write_string(v, out, limits)?,
            Self::Array(values) => {
                append(out, b"[", limits)?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        append(out, b", ", limits)?;
                    }
                    value.write(out, limits, depth + 1, nodes)?;
                }
                append(out, b"]", limits)?;
            }
            Self::Object(values) => {
                append(out, b"{", limits)?;
                for (index, (key, value)) in values.iter().enumerate() {
                    if index > 0 {
                        append(out, b", ", limits)?;
                    }
                    write_string(key, out, limits)?;
                    append(out, b": ", limits)?;
                    value.write(out, limits, depth + 1, nodes)?;
                }
                append(out, b"}", limits)?;
            }
        }
        Ok(())
    }
}

fn append(out: &mut Vec<u8>, bytes: &[u8], limits: Limits) -> Result<()> {
    if bytes.len() > limits.manifest_bytes.saturating_sub(out.len()) {
        return Err(Error::Limit);
    }
    out.extend_from_slice(bytes);
    Ok(())
}
fn write_string(value: &str, out: &mut Vec<u8>, limits: Limits) -> Result<()> {
    if value.len() > limits.string_bytes {
        return Err(Error::Limit);
    }
    let encoded = serde_json::to_string(value).map_err(|_| Error::Invalid)?;
    append(out, encoded.as_bytes(), limits)
}

// Python json.loads can retain lone UTF-16 surrogate escapes in handcrafted
// reader-only settings. UTF-8 manifests emitted by the current writer cannot.
fn refuse_lone_surrogate(raw: &str) -> Result<()> {
    let bytes = raw.as_bytes();
    let mut offset = 1;
    while offset < bytes.len().saturating_sub(1) {
        if bytes.get(offset) != Some(&b'\\') {
            offset += 1;
            continue;
        }
        if bytes.get(offset + 1) != Some(&b'u') {
            offset += 2;
            continue;
        }
        let unit = raw
            .get(offset + 2..offset + 6)
            .and_then(|text| u16::from_str_radix(text, 16).ok());
        match unit {
            Some(0xd800..=0xdbff) => {
                let next = if raw.get(offset + 6..offset + 8) == Some("\\u") {
                    raw.get(offset + 8..offset + 12)
                        .and_then(|text| u16::from_str_radix(text, 16).ok())
                } else {
                    None
                };
                if !matches!(next, Some(0xdc00..=0xdfff)) {
                    return Err(Error::UnsupportedInput);
                }
                offset += 12;
            }
            Some(0xdc00..=0xdfff) => return Err(Error::UnsupportedInput),
            _ => offset += 6,
        }
    }
    Ok(())
}

struct Parser<'a> {
    text: &'a str,
    offset: usize,
    nodes: usize,
    limits: Limits,
}
impl Parser<'_> {
    fn space(&mut self) {
        while self
            .text
            .as_bytes()
            .get(self.offset)
            .is_some_and(|b| b" \t\n\r".contains(b))
        {
            self.offset += 1;
        }
    }
    fn byte(&self) -> Option<u8> {
        self.text.as_bytes().get(self.offset).copied()
    }
    fn string(&mut self) -> Result<String> {
        if self.byte() != Some(b'"') {
            return Err(Error::Invalid);
        }
        let start = self.offset;
        self.offset += 1;
        loop {
            match self.byte() {
                Some(b'"') => {
                    self.offset += 1;
                    break;
                }
                Some(b'\\') => {
                    self.offset = self.offset.checked_add(2).ok_or(Error::Invalid)?;
                }
                Some(_) => {
                    self.offset += 1;
                }
                None => return Err(Error::Invalid),
            }
            if self.offset.saturating_sub(start) > self.limits.manifest_bytes {
                return Err(Error::Limit);
            }
        }
        let raw = self.text.get(start..self.offset).ok_or(Error::Invalid)?;
        refuse_lone_surrogate(raw)?;
        let value: String = serde_json::from_str(raw).map_err(|_| Error::Invalid)?;
        if value.len() > self.limits.string_bytes {
            return Err(Error::Limit);
        }
        Ok(value)
    }
    fn value(&mut self, depth: usize) -> Result<PythonJson> {
        self.space();
        self.nodes = self.nodes.checked_add(1).ok_or(Error::Limit)?;
        if depth > self.limits.json_depth || self.nodes > self.limits.json_nodes {
            return Err(Error::Limit);
        }
        match self.byte() {
            Some(b'"') => Ok(PythonJson::String(self.string()?)),
            Some(b'[') => {
                self.offset += 1;
                self.space();
                let mut values = Vec::new();
                if self.byte() == Some(b']') {
                    self.offset += 1;
                    return Ok(PythonJson::Array(values));
                }
                loop {
                    values.push(self.value(depth + 1)?);
                    self.space();
                    match self.byte() {
                        Some(b',') => self.offset += 1,
                        Some(b']') => {
                            self.offset += 1;
                            break;
                        }
                        _ => return Err(Error::Invalid),
                    }
                }
                Ok(PythonJson::Array(values))
            }
            Some(b'{') => {
                self.offset += 1;
                self.space();
                let mut values = BTreeMap::new();
                if self.byte() == Some(b'}') {
                    self.offset += 1;
                    return Ok(PythonJson::Object(values));
                }
                loop {
                    self.space();
                    let key = self.string()?;
                    self.space();
                    if self.byte() != Some(b':') {
                        return Err(Error::Invalid);
                    }
                    self.offset += 1;
                    values.insert(key, self.value(depth + 1)?);
                    self.space();
                    match self.byte() {
                        Some(b',') => self.offset += 1,
                        Some(b'}') => {
                            self.offset += 1;
                            break;
                        }
                        _ => return Err(Error::Invalid),
                    }
                }
                Ok(PythonJson::Object(values))
            }
            _ => {
                let start = self.offset;
                while self.byte().is_some_and(|b| !b" \t\n\r,]}".contains(&b)) {
                    self.offset += 1;
                }
                let token = self.text.get(start..self.offset).ok_or(Error::Invalid)?;
                match token {
                    "null" => Ok(PythonJson::Null),
                    "true" => Ok(PythonJson::Bool(true)),
                    "false" => Ok(PythonJson::Bool(false)),
                    "NaN" => Ok(PythonJson::Float(f64::NAN)),
                    "Infinity" => Ok(PythonJson::Float(f64::INFINITY)),
                    "-Infinity" => Ok(PythonJson::Float(f64::NEG_INFINITY)),
                    _ if token.contains(['.', 'e', 'E']) => {
                        // Validate JSON number grammar independently of f64 parsing (which accepts +1/.5).
                        validate_number(token)?;
                        Ok(PythonJson::Float(
                            token.parse().map_err(|_| Error::Invalid)?,
                        ))
                    }
                    _ => Ok(PythonJson::Integer(integer_text(token)?.into())),
                }
            }
        }
    }
}
fn validate_number(token: &str) -> Result<()> {
    let mut chars = token.bytes().peekable();
    if chars.peek() == Some(&b'-') {
        chars.next();
    }
    match chars.next() {
        Some(b'0') => {}
        Some(b'1'..=b'9') => {
            while chars.peek().is_some_and(u8::is_ascii_digit) {
                chars.next();
            }
        }
        _ => return Err(Error::Invalid),
    }
    if chars.peek() == Some(&b'.') {
        chars.next();
        if !chars.peek().is_some_and(u8::is_ascii_digit) {
            return Err(Error::Invalid);
        }
        while chars.peek().is_some_and(u8::is_ascii_digit) {
            chars.next();
        }
    }
    if chars.peek().is_some_and(|c| matches!(c, b'e' | b'E')) {
        chars.next();
        if chars.peek().is_some_and(|c| matches!(c, b'+' | b'-')) {
            chars.next();
        }
        if !chars.peek().is_some_and(u8::is_ascii_digit) {
            return Err(Error::Invalid);
        }
        while chars.peek().is_some_and(u8::is_ascii_digit) {
            chars.next();
        }
    }
    if chars.next().is_some() {
        return Err(Error::Invalid);
    }
    Ok(())
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn safe_key(path: &str) -> Result<()> {
    if path.starts_with('/')
        || path.contains(['\\', '\0'])
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
fn checksum(text: &str) -> Result<()> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Checksum);
    }
    Ok(())
}
/// Canonical writer UUIDs plus ordinary hex, braced and urn spellings.
/// Exotic reader-only Python int(hex) coercions are deliberately unsupported.
pub fn tenant_uuid(value: &str) -> Result<String> {
    let hex = value
        .replace("urn:", "")
        .replace("uuid:", "")
        .trim_matches(['{', '}'])
        .replace('-', "");
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Tenant);
    }
    let hex = hex.to_ascii_lowercase();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        hex.get(..8).ok_or(Error::Tenant)?,
        hex.get(8..12).ok_or(Error::Tenant)?,
        hex.get(12..16).ok_or(Error::Tenant)?,
        hex.get(16..20).ok_or(Error::Tenant)?,
        hex.get(20..).ok_or(Error::Tenant)?
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub path: String,
    pub sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub path: String,
    pub sha256: String,
    pub object: String,
}
#[derive(Debug, Clone)]
pub struct Manifest {
    pub tenant: String,
    pub generation: String,
    pub saved_at: f64,
    pub settings: PythonJson,
    pub directories: Vec<String>,
    pub files: Vec<FileRecord>,
}
#[derive(Debug, Clone)]
pub struct Plan {
    pub manifest: Manifest,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub manifest_key: String,
    pub objects: Vec<FileRecord>,
}
#[derive(Debug, Clone)]
pub struct PlanInput {
    pub corpus_root: String,
    pub tenant: String,
    pub generation: String,
    pub saved_at: f64,
    pub settings: PythonJson,
    pub directories: Vec<String>,
    pub files: Vec<SourceFile>,
}

/// Plan immutable keys only. The caller still owns snapshots, byte verification and remote I/O.
pub fn plan(mut input: PlanInput, limits: Limits) -> Result<Plan> {
    if input.files.len() > limits.files || input.directories.len() > limits.directories {
        return Err(Error::Limit);
    }
    let tenant = tenant_uuid(&input.tenant)?;
    if input.generation.len() > limits.manifest_bytes
        || input.corpus_root.len() > limits.string_bytes
    {
        return Err(Error::Limit);
    }
    let prefix = input.corpus_root.trim_matches('/');
    let base = if prefix.is_empty() {
        format!("noevia-backups/{tenant}")
    } else {
        format!("{prefix}/noevia-backups/{tenant}")
    };
    safe_key(&base)?;
    input.files.sort_by(|a, b| a.path.cmp(&b.path));
    input.directories.sort();
    let mut seen = BTreeSet::new();
    let mut objects = Vec::new();
    for file in input.files {
        if file.path.len() > limits.string_bytes {
            return Err(Error::Limit);
        }
        safe_key(&file.path)?;
        checksum(&file.sha256)?;
        if !seen.insert(file.path.clone()) {
            return Err(Error::Invalid);
        }
        let name = file.path.rsplit('/').next().ok_or(Error::Invalid)?;
        let object = format!("{base}/objects/{}/{name}", file.sha256);
        objects.push(FileRecord {
            path: file.path,
            sha256: file.sha256,
            object,
        });
    }
    if input
        .directories
        .iter()
        .any(|path| path.len() > limits.string_bytes)
    {
        return Err(Error::Limit);
    }
    validate_paths(&objects, &input.directories)?;
    let manifest = Manifest {
        tenant,
        generation: integer_text(&input.generation)?.into(),
        saved_at: input.saved_at,
        settings: input.settings,
        directories: input.directories,
        files: objects.clone(),
    };
    let bytes = manifest.canonical_bytes(limits)?;
    let hash = sha256(&bytes);
    Ok(Plan {
        manifest,
        bytes,
        sha256: hash.clone(),
        manifest_key: format!("{base}/manifests/{hash}.json"),
        objects,
    })
}
fn object(items: impl IntoIterator<Item = (&'static str, PythonJson)>) -> PythonJson {
    PythonJson::Object(
        items
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}
impl Manifest {
    pub fn canonical_bytes(&self, limits: Limits) -> Result<Vec<u8>> {
        if self.files.len() > limits.files || self.directories.len() > limits.directories {
            return Err(Error::Limit);
        }
        if self.generation.len() > limits.manifest_bytes {
            return Err(Error::Limit);
        }
        let mut out = Vec::new();
        let mut nodes = self
            .files
            .len()
            .checked_mul(4)
            .and_then(|n| n.checked_add(self.directories.len()))
            .and_then(|n| n.checked_add(7))
            .ok_or(Error::Limit)?;
        let minimum_depth = if !self.files.is_empty() {
            3
        } else if !self.directories.is_empty() {
            2
        } else {
            1
        };
        if nodes > limits.json_nodes || limits.json_depth < minimum_depth {
            return Err(Error::Limit);
        }
        // Already in CPython sort_keys order. Stream settings by reference, never clone
        // a caller's unvalidated recursive tree before applying depth/node budgets.
        append(&mut out, b"{\"directories\": ", limits)?;
        append(&mut out, b"[", limits)?;
        for (index, path) in self.directories.iter().enumerate() {
            if index > 0 {
                append(&mut out, b", ", limits)?;
            }
            write_string(path, &mut out, limits)?;
        }
        append(&mut out, b"], \"files\": [", limits)?;
        for (index, file) in self.files.iter().enumerate() {
            if index > 0 {
                append(&mut out, b", ", limits)?;
            }
            append(&mut out, b"{\"object\": ", limits)?;
            write_string(&file.object, &mut out, limits)?;
            append(&mut out, b", \"path\": ", limits)?;
            write_string(&file.path, &mut out, limits)?;
            append(&mut out, b", \"sha256\": ", limits)?;
            write_string(&file.sha256, &mut out, limits)?;
            append(&mut out, b"}", limits)?;
        }
        append(&mut out, b"], \"format\": ", limits)?;
        write_string(FORMAT, &mut out, limits)?;
        append(&mut out, b", \"generation\": ", limits)?;
        append(&mut out, integer_text(&self.generation)?.as_bytes(), limits)?;
        append(&mut out, b", \"savedAt\": ", limits)?;
        append(&mut out, python_float(self.saved_at).as_bytes(), limits)?;
        append(&mut out, b", \"settings\": ", limits)?;
        self.settings.write(&mut out, limits, 1, &mut nodes)?;
        append(&mut out, b", \"tenant\": ", limits)?;
        write_string(&self.tenant, &mut out, limits)?;
        append(&mut out, b"}", limits)?;
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ReadPolicy<'a> {
    /// Exact old component placement: the declared tenant namespace may appear anywhere.
    LegacyRead,
    /// Stronger opt-in: tenant expectation and canonical writer suffix/object name.
    StrictWriterLayout { expected_tenant: &'a str },
}
#[derive(Debug, Clone)]
pub struct ReadManifest {
    pub tenant: String,
    pub files: Vec<FileRecord>,
    pub directories: Vec<String>,
    pub settings: PythonJson,
    pub generation: Option<PythonJson>,
    pub saved_at: Option<PythonJson>,
    pub original_sha256: String,
}
fn field<'a>(object: &'a BTreeMap<String, PythonJson>, key: &str) -> Result<&'a PythonJson> {
    object.get(key).ok_or(Error::Invalid)
}
fn text(value: &PythonJson) -> Result<&str> {
    if let PythonJson::String(v) = value {
        Ok(v)
    } else {
        Err(Error::Invalid)
    }
}
fn array(value: &PythonJson) -> Result<&[PythonJson]> {
    if let PythonJson::Array(v) = value {
        Ok(v)
    } else {
        Err(Error::Invalid)
    }
}
fn map(value: &PythonJson) -> Result<&BTreeMap<String, PythonJson>> {
    if let PythonJson::Object(v) = value {
        Ok(v)
    } else {
        Err(Error::Invalid)
    }
}
fn validate_paths(files: &[FileRecord], directories: &[String]) -> Result<()> {
    let mut names = BTreeSet::new();
    for file in files {
        safe_key(&file.path)?;
        if !names.insert(file.path.as_str()) {
            return Err(Error::Invalid);
        }
    }
    let dirs: BTreeSet<_> = directories.iter().map(String::as_str).collect();
    for path in files.iter().map(|file| &file.path).chain(directories) {
        safe_key(path)?;
        if dirs.contains(path.as_str()) && names.contains(path.as_str()) {
            return Err(Error::Invalid);
        }
        for (index, _) in path.match_indices('/') {
            if names.contains(path.get(..index).ok_or(Error::Invalid)?) {
                return Err(Error::Invalid);
            }
        }
    }
    Ok(())
}
fn namespace(row: &FileRecord, tenant: &str, policy: ReadPolicy<'_>) -> Result<()> {
    safe_key(&row.object)?;
    let parts: Vec<_> = row.object.split('/').collect();
    let legacy = parts.windows(3).enumerate().any(|(index, part)| {
        part.first() == Some(&"noevia-backups")
            && part.get(1) == Some(&tenant)
            && part.get(2) == Some(&"objects")
            && parts.len() > index + 3
    });
    if !legacy {
        return Err(Error::Namespace);
    }
    if let ReadPolicy::StrictWriterLayout { expected_tenant } = policy {
        if tenant_uuid(expected_tenant)? != tenant {
            return Err(Error::Tenant);
        }
        let expected = [
            "noevia-backups",
            tenant,
            "objects",
            &row.sha256,
            row.path.rsplit('/').next().ok_or(Error::Invalid)?,
        ];
        if !parts.ends_with(&expected) {
            return Err(Error::Namespace);
        }
    }
    Ok(())
}

/// Verify immutable filename over ORIGINAL bytes, then parse with explicit caller budgets.
/// LegacyRead does not assert canonical writer placement, trusted tenant, or payload integrity.
pub fn read(
    bytes: &[u8],
    immutable_stem: &str,
    policy: ReadPolicy<'_>,
    limits: Limits,
) -> Result<ReadManifest> {
    if bytes.len() > limits.manifest_bytes {
        return Err(Error::Limit);
    }
    let original_sha256 = sha256(bytes);
    if original_sha256 != immutable_stem {
        return Err(Error::Checksum);
    }
    let value = PythonJson::parse(bytes, limits)?;
    let root = map(&value)?;
    if text(field(root, "format")?)? != FORMAT {
        return Err(Error::Format);
    }
    let tenant = tenant_uuid(text(field(root, "tenant")?)?)?;
    if let ReadPolicy::StrictWriterLayout { expected_tenant } = policy {
        if tenant_uuid(expected_tenant)? != tenant {
            return Err(Error::Tenant);
        }
    }
    let rows = array(field(root, "files")?)?;
    if rows.len() > limits.files {
        return Err(Error::Limit);
    }
    let mut files = Vec::new();
    for row in rows {
        let row = map(row)?;
        let record = FileRecord {
            path: text(field(row, "path")?)?.into(),
            sha256: text(field(row, "sha256")?)?.into(),
            object: text(field(row, "object")?)?.into(),
        };
        checksum(&record.sha256)?;
        namespace(&record, &tenant, policy)?;
        files.push(record);
    }
    let directories = match root.get("directories") {
        Some(value) => array(value)?
            .iter()
            .map(|value| text(value).map(str::to_owned))
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    if directories.len() > limits.directories {
        return Err(Error::Limit);
    }
    validate_paths(&files, &directories)?;
    Ok(ReadManifest {
        tenant,
        files,
        directories,
        settings: root.get("settings").cloned().unwrap_or_else(|| object([])),
        generation: root.get("generation").cloned(),
        saved_at: root.get("savedAt").cloned(),
        original_sha256,
    })
}

/// Pure optional integrity check; caller supplies ordered downloaded bytes, never paths/IO.
pub fn verify_payloads(
    manifest: &ReadManifest,
    payloads: &[&[u8]],
    limits: Limits,
) -> Result<usize> {
    if payloads.len() != manifest.files.len() {
        return Err(Error::Invalid);
    }
    let mut total = 0usize;
    for (row, bytes) in manifest.files.iter().zip(payloads) {
        total = total.checked_add(bytes.len()).ok_or(Error::Limit)?;
        if total > limits.payload_bytes {
            return Err(Error::Limit);
        }
        if sha256(bytes) != row.sha256 {
            return Err(Error::Checksum);
        }
    }
    Ok(total)
}
