//! The task-local malformed-call guard, using the existing native stream validator. It adds
//! refusals and stops after three strikes; permission and containment policy remain separate.
use crate::{acp::RpcError, policy};
use js_json::{wtf16, JValue};
use std::sync::OnceLock;
const KINDS: &[&str] = &[
    "read",
    "edit",
    "delete",
    "move",
    "search",
    "execute",
    "think",
    "fetch",
    "switch_mode",
    "other",
];
const PATHS: &[&str] = &["path", "file_path", "filePath", "filepath"];
static SCHEMAS: OnceLock<Option<JValue>> = OnceLock::new();
fn schemas() -> Option<&'static JValue> {
    SCHEMAS
        .get_or_init(|| wtf16::parse(include_str!("../assets/guard-schemas.json")).ok())
        .as_ref()
}
fn object(rows: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn text(s: &str) -> JValue {
    JValue::Str(s.into())
}
fn set(value: &mut JValue, key: &str, item: JValue) {
    if let JValue::Obj(rows) = value {
        if let Some(row) = rows.iter_mut().find(|(k, _)| k == key) {
            row.1 = item;
        } else {
            rows.push((key.into(), item));
        }
    }
}
fn units(s: &str) -> Vec<u16> {
    let mut out = vec![];
    for c in s.chars() {
        if let Some(u) = wtf16::lone_unit(c) {
            out.push(u);
        } else {
            let mut pair = [0; 2];
            out.extend_from_slice(c.encode_utf16(&mut pair));
        }
    }
    out
}
fn string(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|c| {
            c.unwrap_or_else(|e| wtf16::lone_char(e.unpaired_surrogate()).unwrap_or('\u{fffd}'))
        })
        .collect()
}
fn byte_len(value: &str) -> usize {
    value
        .chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                3
            } else {
                c.len_utf8()
            }
        })
        .sum()
}
fn hint(kind: &str) -> String {
    match kind{
    "execute"=>"Send the shell command as one non-empty string in rawInput.command.".into(),
    "edit"|"delete"|"move"=>format!("Name the file to {} as a non-empty string in locations[].path (or rawInput.path).",if kind=="edit"{"change"}else{kind}),
    "read"|"search"=>"Send each path as a plain non-empty string.".into(),
    "fetch"=>"Send the address as a string in rawInput.url.".into(),
    "other"=>"Send rawInput as a JSON object and each path as a plain non-empty string.".into(),
    "think"|"switch_mode"=>"Send rawInput as a JSON object.".into(),
    "kind"=>format!("Use one of the ACP tool kinds: {}.",KINDS.join(", ")),
    "fs/read_text_file"=>"Send { path } as a plain non-empty string; line and limit, when given, are whole numbers from 1.".into(),
    "fs/write_text_file"=>"Send { path, content } with a plain non-empty path and the whole file as a string.".into(),_=>String::new(),}
}
#[derive(Clone)]
pub struct Found {
    message: String,
    path: String,
    kind: JValue,
    hint: String,
}
impl Found {
    fn new(
        message: impl Into<String>,
        path: impl Into<String>,
        kind: Option<&str>,
        hint_: &str,
    ) -> Self {
        Self {
            message: message.into(),
            path: path.into(),
            kind: kind.map_or(JValue::Null, text),
            hint: hint(hint_),
        }
    }
    pub fn value(&self) -> JValue {
        object(vec![
            ("message", text(&self.message)),
            ("path", text(&self.path)),
            ("kind", self.kind.clone()),
            ("hint", text(&self.hint)),
        ])
    }
}
fn project(value: &JValue, schema: &JValue) -> JValue {
    if !value.is_object() {
        return value.clone();
    }
    let JValue::Obj(properties) = schema.get("properties") else {
        return value.clone();
    };
    JValue::Obj(
        properties
            .iter()
            .filter(|(key, _)| value.get(key) != &JValue::Undefined)
            .map(|(key, _)| (key.clone(), value.get(key).clone()))
            .collect(),
    )
}
fn validate(schema: &JValue, value: &JValue, kind: &str) -> Option<Found> {
    let unchecked = || Found::new("Arguments could not be checked.", "$", Some(kind), kind);
    let Some(encoded) = wtf16::stringify(value) else {
        return Some(Found::new("Arguments are missing.", "$", Some(kind), kind));
    };
    let Some(schema) =
        wtf16::stringify(schema).and_then(|s| stream_guard::Schema::parse(s.as_bytes()).ok())
    else {
        return Some(unchecked());
    };
    let Ok(state) = stream_guard::State::new(
        stream_guard::DEFAULT_MAX_DEPTH,
        stream_guard::DEFAULT_MAX_BYTES,
    ) else {
        return Some(unchecked());
    };
    let mut validator = stream_guard::Validator::new(&schema, state);
    validator.feed(&units(&encoded));
    validator.end();
    validator.violation().map(|v| {
        Found::new(
            string(&v.message),
            {
                let path = string(&v.path);
                if path.is_empty() {
                    "$".into()
                } else {
                    path
                }
            },
            Some(kind),
            kind,
        )
    })
}
fn check_path(path: &str, at: &str, kind: &str) -> Option<Found> {
    if server_account::util::js_trim(path).is_empty() {
        Some(Found::new(
            format!("Empty path at {at}"),
            at,
            Some(kind),
            kind,
        ))
    } else if path.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') {
        Some(Found::new(
            format!("Path at {at} contains a NUL or control character"),
            at,
            Some(kind),
            kind,
        ))
    } else {
        None
    }
}
fn paths(tool: &JValue, kind: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    if let JValue::Arr(rows) = tool.get("locations") {
        for (index, row) in rows.iter().enumerate() {
            if row.is_object() {
                if let Some(path) = row.get("path").as_str() {
                    out.push((path.into(), format!("$.locations[{index}].path")));
                }
            }
        }
    }
    let raw = tool.get("rawInput");
    if raw.is_object() {
        for key in PATHS {
            if let Some(path) = raw.get(key).as_str() {
                out.push((path.into(), format!("$.rawInput.{key}")));
            }
        }
        if kind == "execute" {
            if let Some(path) = raw.get("cwd").as_str() {
                out.push((path.into(), "$.rawInput.cwd".into()));
            }
        }
    }
    out
}
pub fn check_tool(tool: &JValue) -> Result<Option<Found>, RpcError> {
    if !tool.is_object() {
        return Ok(Some(Found::new(
            "Type mismatch at $: expected object",
            "$",
            None,
            "kind",
        )));
    }
    let raw_kind = tool.get("kind");
    if !raw_kind.is_nullish() && !raw_kind.as_str().is_some_and(|s| KINDS.contains(&s)) {
        let kind = js_json::to_js_string(raw_kind).map_err(|_| RpcError {
            code: -32603,
            message: "Cannot convert object to primitive value".into(),
            data: None,
        })?;
        let quoted =
            wtf16::stringify(&text(&wtf16::slice(&kind, 40))).unwrap_or_else(|| "null".into());
        return Ok(Some(Found::new(
            format!("Unknown tool kind {quoted} at $.kind"),
            "$.kind",
            None,
            "kind",
        )));
    }
    let kind = raw_kind.as_str().unwrap_or("other");
    let Some(schema) = schemas().map(|s| s.get("tool").get(kind)) else {
        return Ok(Some(Found::new(
            "Arguments could not be checked.",
            "$",
            Some(kind),
            kind,
        )));
    };
    let mut projected = object(vec![]);
    for key in ["toolCallId", "kind", "title", "status", "locations"] {
        if !tool.get(key).is_nullish() {
            set(&mut projected, key, tool.get(key).clone());
        }
    }
    if !tool.get("rawInput").is_nullish() {
        set(
            &mut projected,
            "rawInput",
            project(
                tool.get("rawInput"),
                schema.get("properties").get("rawInput"),
            ),
        );
    }
    if !tool.get("content").is_nullish() {
        set(
            &mut projected,
            "content",
            if matches!(tool.get("content"), JValue::Arr(_)) {
                JValue::Arr(vec![])
            } else {
                tool.get("content").clone()
            },
        );
    }
    if let Some(found) = validate(schema, &projected, kind) {
        return Ok(Some(found));
    }
    let paths = paths(tool, kind);
    for (path, at) in &paths {
        if let Some(found) = check_path(path, at, kind) {
            return Ok(Some(found));
        }
    }
    let classified = policy::classify(tool);
    let command = classified.get("command").as_str().unwrap_or("");
    if kind == "execute" {
        if command.is_empty() {
            return Ok(Some(Found::new(
                "Missing command at $.rawInput.command",
                "$.rawInput.command",
                Some(kind),
                kind,
            )));
        }
        if command.contains('\0') {
            return Ok(Some(Found::new(
                "Command at $.rawInput.command contains a NUL byte",
                "$.rawInput.command",
                Some(kind),
                kind,
            )));
        }
    }
    if matches!(kind, "edit" | "delete" | "move") && paths.is_empty() {
        return Ok(Some(Found::new(
            format!("A {kind} names no file: missing $.locations[].path"),
            "$.locations",
            Some(kind),
            kind,
        )));
    }
    if kind == "fetch"
        && tool
            .get("rawInput")
            .get("url")
            .as_str()
            .is_none_or(|s| server_account::util::js_trim(s).is_empty())
        && command.is_empty()
    {
        return Ok(Some(Found::new(
            "Missing URL at $.rawInput.url",
            "$.rawInput.url",
            Some(kind),
            kind,
        )));
    }
    Ok(None)
}
pub fn check_file(method: &str, params: &JValue, max_bytes: usize) -> Option<Found> {
    let Some(schema) = schemas()
        .map(|s| s.get("files").get(method))
        .filter(|s| s.is_object())
    else {
        return Some(Found::new(
            format!("Unknown file method {method}"),
            "$",
            Some(method),
            "",
        ));
    };
    let projected = if params.is_object() && params.get("content").as_str().is_some() {
        let mut out = params.clone();
        set(&mut out, "content", text(""));
        out
    } else {
        project(params, schema)
    };
    if let Some(found) = validate(schema, &projected, method) {
        return Some(found);
    }
    if let Some(found) = check_path(params.get("path").as_str().unwrap_or(""), "$.path", method) {
        return Some(found);
    }
    for key in ["line", "limit"] {
        let value = params.get(key);
        if !value.is_nullish() && number(value) < 1.0 {
            return Some(Found::new(
                format!("{key} at $.{key} must be 1 or more"),
                format!("$.{key}"),
                Some(method),
                method,
            ));
        }
    }
    if method == "fs/write_text_file"
        && params
            .get("content")
            .as_str()
            .is_some_and(|s| byte_len(s) > max_bytes)
    {
        return Some(Found::new(
            format!("Content at $.content is larger than {max_bytes} bytes"),
            "$.content",
            Some(method),
            method,
        ));
    }
    None
}
fn number(value: &JValue) -> f64 {
    match value {
        JValue::Null => 0.0,
        JValue::Undefined => f64::NAN,
        JValue::Bool(v) => {
            if *v {
                1.0
            } else {
                0.0
            }
        }
        JValue::Num(n) => *n,
        _ => js_json::to_js_string(value)
            .ok()
            .map_or(f64::NAN, |s| server_account::util::js_number(&s)),
    }
}
fn correction(found: &Found) -> JValue {
    let message = units(&found.message);
    let path = units(&found.path);
    if message.len().min(300) > stream_guard::MAX_CORRECTION_UNITS
        || path.len() > stream_guard::MAX_CORRECTION_UNITS
    {
        return object(vec![
            ("type", text("schema_violation_correction")),
            (
                "violation",
                object(vec![
                    ("message", text("Arguments could not be checked.")),
                    ("path", text("$")),
                ]),
            ),
        ]);
    }
    wtf16::parse(&String::from_utf8_lossy(&stream_guard::correction(
        &message,
        Some(&path),
        Some(300),
    )))
    .unwrap_or_else(|_| {
        object(vec![
            ("type", text("schema_violation_correction")),
            (
                "violation",
                object(vec![
                    ("message", text("Arguments could not be checked.")),
                    ("path", text("$")),
                ]),
            ),
        ])
    })
}
pub fn violation_text(value: &JValue) -> String {
    let count = value.get("count").as_num().unwrap_or(0.0) as usize;
    let limit = value.get("limit").as_num().unwrap_or(0.0) as usize;
    let lead = if value.get("blocked") == &JValue::Bool(true) {
        format!("noevia refused this malformed tool call and stopped the task (violation {count} of {limit}).")
    } else {
        format!("noevia refused this malformed tool call (violation {count} of {limit}). Correct the arguments and call the tool again.")
    };
    format!(
        "{lead} {}. {}",
        value.get("violation").get("message").as_str().unwrap_or(""),
        value.get("hint").as_str().unwrap_or("")
    )
}
pub fn reject(mut selected: JValue, violation: JValue) -> JValue {
    set(
        &mut selected,
        "_meta",
        object(vec![(
            "noevia",
            object(vec![
                ("reason", text(&violation_text(&violation))),
                ("violation", violation),
            ]),
        )]),
    );
    selected
}
fn violation_error(violation: JValue) -> RpcError {
    let data = object(vec![("noeviaViolation", violation.clone())]);
    RpcError {
        code: -32602,
        message: format!(
            "{}\n{}",
            violation_text(&violation),
            wtf16::stringify(&data).unwrap_or_else(|| "null".into())
        ),
        data: Some(data),
    }
}
pub struct Guard {
    count: usize,
    limit: usize,
    max_bytes: usize,
    blocked: Option<JValue>,
    history: Vec<JValue>,
}
impl Guard {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            count: 0,
            limit: 3,
            max_bytes,
            blocked: None,
            history: vec![],
        }
    }
    pub fn count(&self) -> usize {
        self.count
    }
    pub fn blocked(&self) -> bool {
        self.blocked.is_some()
    }
    fn refuse(
        &mut self,
        found: Found,
        tool: &str,
        action: JValue,
        permission: bool,
        event: &mut impl FnMut(&str, JValue) -> Result<(), RpcError>,
    ) -> JValue {
        let violation = if let Some(blocked) = &self.blocked {
            let mut again = blocked.clone();
            set(&mut again, "tool", text(tool));
            if !found.kind.is_nullish() {
                set(&mut again, "kind", found.kind.clone());
            }
            again
        } else {
            self.count += 1;
            let mut value = correction(&found);
            for (key, item) in [
                ("tool", text(tool)),
                ("kind", found.kind.clone()),
                ("hint", text(&found.hint)),
                ("count", JValue::Num(self.count as f64)),
                ("limit", JValue::Num(self.limit as f64)),
                ("blocked", JValue::Bool(self.count >= self.limit)),
            ] {
                set(&mut value, key, item);
            }
            self.history.push(object(vec![
                ("tool", text(tool)),
                ("kind", found.kind),
                ("path", value.get("violation").get("path").clone()),
                ("message", value.get("violation").get("message").clone()),
            ]));
            value
        };
        let data = if permission {
            object(vec![
                ("decision", text("denied")),
                ("action", action),
                ("automatic", JValue::Bool(true)),
                ("reason", text(&violation_text(&violation))),
                ("violation", violation.clone()),
            ])
        } else {
            object(vec![
                (
                    "name",
                    text(if tool == "fs/read_text_file" {
                        "read_file"
                    } else {
                        "write_file"
                    }),
                ),
                ("failed", JValue::Bool(true)),
                ("violation", violation.clone()),
            ])
        };
        let _ = event(
            if permission {
                "approval.decided"
            } else {
                "tool.completed"
            },
            data,
        );
        if self.blocked.is_none() && violation.get("blocked") == &JValue::Bool(true) {
            self.blocked = Some(violation.clone());
            let _ = event(
                "step.started",
                object(vec![
                    ("id", text("executor.guard")),
                    ("title", text("Tool-call guard")),
                ]),
            );
            let _ = event(
                "step.completed",
                object(vec![
                    ("id", text("executor.guard")),
                    ("failed", JValue::Bool(true)),
                    ("blocked", JValue::Bool(true)),
                    ("violations", JValue::Num(self.count as f64)),
                    (
                        "error",
                        text(&format!(
                            "Stopped after {} malformed tool calls.",
                            self.count
                        )),
                    ),
                ]),
            );
        }
        violation
    }
    pub fn permission(
        &mut self,
        tool: &JValue,
        action: JValue,
        event: &mut impl FnMut(&str, JValue) -> Result<(), RpcError>,
    ) -> Result<Option<JValue>, RpcError> {
        let found = if self.blocked() {
            Some(Found::new("", "", None, ""))
        } else {
            check_tool(tool)?
        };
        Ok(
            found
                .map(|found| self.refuse(found, "session/request_permission", action, true, event)),
        )
    }
    pub fn file(
        &mut self,
        method: &str,
        params: &JValue,
        event: &mut impl FnMut(&str, JValue) -> Result<(), RpcError>,
    ) -> Result<(), RpcError> {
        let found = if self.blocked() {
            Some(Found::new("", "", None, ""))
        } else {
            check_file(method, params, self.max_bytes)
        };
        if let Some(found) = found {
            Err(violation_error(self.refuse(
                found,
                method,
                JValue::Null,
                false,
                event,
            )))
        } else {
            Ok(())
        }
    }
    pub fn blocked_error(&self) -> Option<JValue> {
        self.blocked.as_ref()?;
        let message = format!(
            "Blocked: the coding agent sent {} malformed tool calls, so the task was stopped.",
            self.count
        );
        Some(object(vec![
            ("message", text(&message)),
            ("code", text("blocked")),
            ("publicMessage", text(&message)),
            (
                "result",
                object(vec![
                    ("blocked", JValue::Bool(true)),
                    ("reason", text("executor_guard")),
                    ("violations", JValue::Arr(self.history.clone())),
                ]),
            ),
        ]))
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    fn fixture() -> JValue {
        wtf16::parse(include_str!("../tests/fixtures/guard-node.json")).unwrap()
    }
    #[test]
    fn guard_schema_projection_matches_node() {
        let fixture = fixture();
        let JValue::Arr(cases) = fixture.get("calls") else {
            panic!()
        };
        assert_eq!(cases.len(), 21);
        for (index, case) in cases.iter().enumerate() {
            let output = check_tool(case.get("input"))
                .unwrap()
                .map_or(JValue::Null, |f| f.value());
            assert_eq!(output, *case.get("output"), "permission {index}");
        }
        let JValue::Arr(cases) = fixture.get("fsCalls") else {
            panic!()
        };
        assert_eq!(cases.len(), 18);
        for (index, case) in cases.iter().enumerate() {
            let output = check_file(
                case.get("method").as_str().unwrap(),
                case.get("params"),
                case.get("maxBytes").as_num().unwrap() as usize,
            )
            .map_or(JValue::Null, |f| f.value());
            assert_eq!(output, *case.get("output"), "file {index}");
        }
    }
    #[test]
    fn guard_corrections_strikes_events_and_blocked_history_match_node() {
        let fixture = fixture();
        let expected = fixture.get("strikes");
        let mut guard = Guard::new(8);
        let mut events = vec![];
        let mut refusals = vec![];
        for tool in [
            JValue::Null,
            object(vec![("kind", text("execute"))]),
            object(vec![("kind", text("edit"))]),
            object(vec![("kind", text("think"))]),
        ] {
            refusals.push(
                guard
                    .permission(&tool, text("execute_command"), &mut |kind, data| {
                        events.push(object(vec![("type", text(kind)), ("data", data)]));
                        Ok(())
                    })
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(guard.count(), 3);
        assert!(guard.blocked());
        assert_eq!(JValue::Arr(refusals), *expected.get("refusals"));
        assert_eq!(JValue::Arr(events), *expected.get("events"));
        assert_eq!(guard.blocked_error().unwrap(), *expected.get("error"));
    }
}
