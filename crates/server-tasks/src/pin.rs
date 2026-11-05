//! Harness permission/configuration pins. Static engine extensions are retained as engine assets;
//! task ownership and every approval decision remain native. Secrets have no Debug/serde surface.
use crate::start::Refusal;
use js_json::{wtf16, JValue};
use std::{
    fs,
    io::Write,
    path::{Component, Path},
    sync::OnceLock,
};
use zeroize::{Zeroize, Zeroizing};
const ENGINE: &str = "http://engine-placeholder.invalid";
static TEMPLATES: OnceLock<Result<JValue, Refusal>> = OnceLock::new();
pub struct Options<'a> {
    pub harness: &'a str,
    pub model: &'a str,
    pub engine: &'a str,
    pub api_key: Option<&'a str>,
    pub context_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
}
#[derive(Clone, Copy)]
enum Base {
    Cwd,
    Home,
}
struct File {
    base: Base,
    path: String,
    content: Zeroizing<String>,
}
pub struct Rendered {
    harness: String,
    files: Vec<File>,
    permission: JValue,
    model: String,
    endpoint: String,
}
fn refusal(status: u16, message: impl Into<String>) -> Refusal {
    Refusal {
        status,
        message: message.into(),
    }
}
fn unavailable() -> Refusal {
    refusal(503, "The harness configuration could not be prepared.")
}
fn string(value: impl Into<String>) -> JValue {
    JValue::Str(value.into())
}
fn object(fields: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn trim(value: &str) -> &str {
    server_account::util::js_trim(value)
}
struct SecretJson(JValue);
impl Drop for SecretJson {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}
fn wipe(value: &mut JValue) {
    match value {
        JValue::Str(s) => s.zeroize(),
        JValue::Arr(values) => {
            for v in values {
                wipe(v);
            }
        }
        JValue::Obj(values) => {
            for (key, v) in values {
                key.zeroize();
                wipe(v);
            }
        }
        _ => {}
    }
}
fn quoted(value: &str) -> Result<Zeroizing<String>, Refusal> {
    let value = SecretJson(string(value));
    Ok(Zeroizing::new(
        wtf16::stringify(&value.0).ok_or_else(unavailable)?,
    ))
}
struct Bindings<'a> {
    model: &'a str,
    endpoint: &'a str,
    key: &'a str,
    context: f64,
    output: f64,
    opencode: bool,
}
fn bind(value: &mut JValue, b: &Bindings<'_>) {
    match value {
        JValue::Str(s) => {
            let replacement = if s == "__TASK_MODEL__" {
                Some(b.model.to_owned())
            } else if s == "local/__TASK_MODEL__" {
                Some(format!("local/{}", b.model))
            } else if s == "__TASK_KEY__" {
                Some(b.key.to_owned())
            } else if s == ENGINE {
                Some(
                    if b.opencode {
                        b.endpoint
                    } else {
                        b.endpoint.strip_suffix("/v1").unwrap_or(b.endpoint)
                    }
                    .to_owned(),
                )
            } else if s == &format!("{ENGINE}/v1") {
                Some(b.endpoint.to_owned())
            } else {
                None
            };
            if let Some(replacement) = replacement {
                *s = replacement;
            }
        }
        JValue::Num(n) if *n == 32768.0 => *n = b.context,
        JValue::Num(n) if *n == 4096.0 => *n = b.output,
        JValue::Arr(values) => {
            for value in values {
                bind(value, b);
            }
        }
        JValue::Obj(values) => {
            for (key, value) in values {
                if key == "__TASK_MODEL__" {
                    *key = b.model.to_owned();
                }
                bind(value, b);
            }
        }
        _ => {}
    }
}
fn int32(value: f64) -> i32 {
    if !value.is_finite() || value == 0.0 {
        0
    } else {
        value.trunc().rem_euclid(4294967296.0) as u32 as i32
    }
}
fn dsh(template: &str, b: &Bindings<'_>, has_key: bool) -> Result<Zeroizing<String>, Refusal> {
    let mut out = Zeroizing::new(String::new());
    for source in template.lines() {
        if !has_key
            && (source == "        headers:" || source.starts_with("          Authorization:"))
        {
            continue;
        }
        let line = Zeroizing::new(if source.starts_with("        baseURL:") {
            format!("        baseURL: {}", quoted(b.endpoint)?.as_str())
        } else if source.starts_with("          Authorization:") {
            let bearer = Zeroizing::new(format!("Bearer {}", b.key));
            format!("          Authorization: {}", quoted(&bearer)?.as_str())
        } else if source.starts_with("          - id:") {
            format!("          - id: {}", quoted(b.model)?.as_str())
        } else if source.starts_with("            name:") {
            format!("            name: {}", quoted(b.model)?.as_str())
        } else if source.starts_with("    model:") {
            format!("    model: {}", quoted(b.model)?.as_str())
        } else if source.starts_with("            contextWindow:") {
            format!("            contextWindow: {}", int32(b.context))
        } else if source.starts_with("            maxTokens:") {
            format!("            maxTokens: {}", int32(b.output))
        } else {
            source.to_owned()
        });
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}
impl Rendered {
    pub fn render(options: Options<'_>) -> Result<Self, Refusal> {
        let harness = trim(options.harness);
        if harness == "codex" {
            return Err(refusal(409,"noevia cannot pin the permissions of the codex harness (its commands run without asking inside its own sandbox), so it will not run it."));
        }
        let templates = TEMPLATES
            .get_or_init(|| {
                wtf16::parse(include_str!("../assets/pin-templates.json"))
                    .map_err(|_| unavailable())
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let entry = templates.get(harness);
        if entry.is_nullish() {
            return Err(refusal(
                409,
                format!(
                    "noevia cannot pin the permissions of the {} harness, so it will not run it.",
                    if harness.is_empty() {
                        "unnamed"
                    } else {
                        harness
                    }
                ),
            ));
        }
        let model = trim(options.model);
        if model.is_empty() {
            return Err(refusal(
                409,
                "A coding task needs a model; none is loaded or chosen.",
            ));
        }
        let engine = trim(options.engine);
        if !engine.starts_with("http://") && !engine.starts_with("https://") {
            return Err(refusal(
                409,
                "This server has no model endpoint for coding tasks.",
            ));
        }
        let opencode = harness == "opencode";
        let mut endpoint = if opencode {
            engine.to_owned()
        } else {
            engine.trim_end_matches('/').to_owned()
        };
        if !opencode && !endpoint.ends_with("/v1") {
            endpoint.push_str("/v1");
        }
        let has_key = options.api_key.is_some_and(|s| !s.is_empty());
        let bindings = Bindings {
            model,
            endpoint: &endpoint,
            key: options.api_key.filter(|s| !s.is_empty()).unwrap_or("none"),
            context: options.context_tokens.unwrap_or(32768.0),
            output: options.output_tokens.unwrap_or(4096.0),
            opencode,
        };
        let template = entry.get("value");
        let JValue::Arr(source_files) = template.get("files") else {
            return Err(unavailable());
        };
        let mut files = Vec::new();
        for source in source_files {
            let base = match source.get("base").as_str() {
                Some("cwd") => Base::Cwd,
                Some("home") => Base::Home,
                _ => return Err(unavailable()),
            };
            let path = source.get("path").as_str().ok_or_else(unavailable)?;
            let original = source.get("content").as_str().ok_or_else(unavailable)?;
            let content = if path.ends_with(".json") {
                let mut value = SecretJson(wtf16::parse(original).map_err(|_| unavailable())?);
                bind(&mut value.0, &bindings);
                let mut content =
                    Zeroizing::new(wtf16::stringify_pretty(&value.0).ok_or_else(unavailable)?);
                content.push('\n');
                content
            } else if path.ends_with(".yml") {
                dsh(original, &bindings, has_key)?
            } else {
                Zeroizing::new(
                    match original {
                        "@engine/pi-gate.js" => include_str!("../assets/pi-gate.js"),
                        "@engine/dsh-gate.mjs" => include_str!("../assets/dsh-gate.mjs"),
                        other => other,
                    }
                    .to_owned(),
                )
            };
            files.push(File {
                base,
                path: path.into(),
                content,
            });
        }
        Ok(Self {
            harness: harness.into(),
            files,
            permission: template.get("permission").clone(),
            model: if opencode {
                format!("local/{model}")
            } else {
                model.into()
            },
            endpoint,
        })
    }
    pub fn metadata(&self) -> Result<JValue, Refusal> {
        let first = self.files.first().ok_or_else(unavailable)?;
        Ok(object(vec![
            ("file", string(first.path.clone())),
            (
                "files",
                JValue::Arr(
                    self.files
                        .iter()
                        .map(|f| {
                            string(format!(
                                "{}/{}",
                                match f.base {
                                    Base::Cwd => ".",
                                    Base::Home => "~",
                                },
                                f.path
                            ))
                        })
                        .collect(),
                ),
            ),
            ("permission", self.permission.clone()),
            ("model", string(self.model.clone())),
            ("endpoint", string(self.endpoint.clone())),
        ]))
    }
    pub fn write(
        &self,
        cwd: &Path,
        home: Option<&Path>,
        owner: Option<(u32, u32)>,
    ) -> Result<JValue, Refusal> {
        for file in &self.files {
            let root = match file.base {
                Base::Cwd => Some(cwd),
                Base::Home => home,
            }
            .ok_or_else(|| {
                refusal(
                    409,
                    format!(
                        "The {} harness needs a private home directory for its configuration.",
                        self.harness
                    ),
                )
            })?;
            write_pinned(root, &file.path, &file.content, owner)?;
        }
        self.metadata()
    }
}
pub fn cwd_pin_paths(harness: &str) -> Vec<String> {
    match trim(harness) {
        "opencode" => vec!["opencode.json".into()],
        "claude-code" => vec![".claude/settings.local.json".into()],
        "qwen-code" => vec![".qwen/settings.json".into()],
        _ => vec![],
    }
}
fn write_pinned(
    root: &Path,
    relative: &str,
    content: &str,
    owner: Option<(u32, u32)>,
) -> Result<(), Refusal> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let refuse = |message: String| {
        refusal(
            409,
            format!("Refused to pin the harness configuration: {message}."),
        )
    };
    let io = |e: std::io::Error| refusal(500, e.to_string());
    let parts = Path::new(relative).components().collect::<Vec<_>>();
    if parts.is_empty() || parts.iter().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(refusal(500, "Pinned file outside its root"));
    }
    let mut dir = root.to_path_buf();
    for component in parts.iter().take(parts.len().saturating_sub(1)) {
        dir.push(component.as_os_str());
        match fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(refuse(format!(
                    "{} is a symbolic link",
                    dir.strip_prefix(root).unwrap_or(&dir).display()
                )))
            }
            Ok(m) if !m.is_dir() => {
                return Err(refuse(format!(
                    "{} is not a directory",
                    dir.strip_prefix(root).unwrap_or(&dir).display()
                )))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(&dir).map_err(io)?;
                if let Some((uid, gid)) = owner {
                    std::os::unix::fs::lchown(&dir, Some(uid), Some(gid)).map_err(io)?;
                }
            }
            Err(e) => return Err(io(e)),
        }
    }
    let target = root.join(relative);
    match fs::symlink_metadata(&target) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(refuse(format!("{relative} is a symbolic link")))
        }
        Ok(m) if m.is_dir() => return Err(refuse(format!("{relative} is a directory"))),
        Ok(_) => fs::remove_file(&target).map_err(io)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&target)
        .map_err(io)?;
    file.write_all(content.as_bytes()).map_err(io)?;
    drop(file);
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::lchown(&target, Some(uid), Some(gid)).map_err(io)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn native_permission_files_match_all_node_harness_pins_byte_for_byte() {
        let fixture = wtf16::parse(include_str!("../tests/fixtures/pin-node.json")).unwrap();
        let JValue::Arr(cases) = fixture.get("cases") else {
            panic!()
        };
        assert_eq!(cases.len(), 35);
        for (index, case) in cases.iter().enumerate() {
            let input = case.get("input");
            let options = Options {
                harness: input.get("harness").as_str().unwrap(),
                model: input.get("model").as_str().unwrap(),
                engine: input.get("engine").as_str().unwrap(),
                api_key: input.get("apiKey").as_str(),
                context_tokens: input.get("contextTokens").as_num(),
                output_tokens: input.get("outputTokens").as_num(),
            };
            let expected = case.get("expected");
            match Rendered::render(options) {
                Err(error) => {
                    assert_eq!(*expected.get("ok"), JValue::Bool(false), "case {index}");
                    assert_eq!(
                        *expected.get("status"),
                        JValue::Num(f64::from(error.status)),
                        "case {index}"
                    );
                    assert_eq!(
                        *expected.get("error"),
                        string(error.message),
                        "case {index}"
                    );
                }
                Ok(pinned) => {
                    let files = JValue::Arr(
                        pinned
                            .files
                            .iter()
                            .map(|f| {
                                object(vec![
                                    (
                                        "base",
                                        string(match f.base {
                                            Base::Cwd => "cwd",
                                            Base::Home => "home",
                                        }),
                                    ),
                                    ("path", string(f.path.clone())),
                                    ("content", string(f.content.as_str())),
                                ])
                            })
                            .collect(),
                    );
                    let actual = object(vec![
                        ("harness", string(pinned.harness.clone())),
                        ("files", files),
                        ("permission", pinned.permission.clone()),
                        ("model", string(pinned.model.clone())),
                        ("endpoint", string(pinned.endpoint.clone())),
                    ]);
                    assert_eq!(&actual, expected.get("value"), "case {index}");
                    assert_eq!(
                        JValue::Arr(
                            cwd_pin_paths(input.get("harness").as_str().unwrap())
                                .into_iter()
                                .map(string)
                                .collect()
                        ),
                        *expected.get("cwdPins"),
                        "case {index}"
                    );
                }
            }
        }
    }
    #[test]
    fn pinned_files_refuse_parent_and_final_links_and_preserve_hardlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let external = dir.path().join("external");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&external).unwrap();
        symlink(&external, root.join(".claude")).unwrap();
        let error =
            write_pinned(&root, ".claude/settings.json", "synthetic-only", None).unwrap_err();
        assert_eq!(error.status, 409);
        assert!(!external.join("settings.json").exists());
        let secret = external.join("file");
        fs::write(&secret, "preserved").unwrap();
        symlink(&secret, root.join("config.json")).unwrap();
        assert_eq!(
            write_pinned(&root, "config.json", "synthetic-only", None)
                .unwrap_err()
                .status,
            409
        );
        fs::remove_file(root.join("config.json")).unwrap();
        fs::hard_link(&secret, root.join("config.json")).unwrap();
        write_pinned(&root, "config.json", "synthetic-only", None).unwrap();
        assert_eq!(fs::read_to_string(&secret).unwrap(), "preserved");
        assert_eq!(
            fs::read_to_string(root.join("config.json")).unwrap(),
            "synthetic-only"
        );
        assert_eq!(
            fs::metadata(root.join("config.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
