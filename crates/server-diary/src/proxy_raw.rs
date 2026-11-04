//! The moved proxies read only named fields, and relay JSON without a recursive tree or a
//! reserved Unicode range. The shared local-exchange parser owns JS number/key/string rewriting.
use crate::js_raw::{self, Field, Top};
use crate::routes::{Reply, ReplyBody, Route};

pub(crate) struct Doc {
    pub written: String,
    fields: Vec<Field>,
}
#[derive(Clone, Copy)]
struct Value<'a> {
    written: &'a str,
    number: Option<f64>,
    source: &'a str,
}
impl Value<'_> {
    fn truthy(self) -> bool {
        self.number.map_or_else(
            || !matches!(self.written, "null" | "false" | "0" | "\"\""),
            |n| n != 0.0 && !n.is_nan(),
        )
    }
    fn string(self) -> bool {
        self.written.starts_with('"')
    }
    fn nullish(self) -> bool {
        self.written == "null" && self.number.is_none()
    }
}
impl Doc {
    fn parse(text: &str) -> Option<Self> {
        match js_raw::read(text)? {
            Top::Object(fields) => Some(Self {
                written: js_raw::join(&fields),
                fields,
            }),
            Top::Other(written, _) => Some(Self {
                written,
                fields: Vec::new(),
            }),
        }
    }
    pub fn body(bytes: Option<&[u8]>) -> Self {
        let text = bytes.map(String::from_utf8_lossy);
        text.as_deref()
            .and_then(Self::parse)
            .unwrap_or_else(|| Self {
                written: js_raw::quote(text.as_deref().unwrap_or("")),
                fields: Vec::new(),
            })
    }
    fn get(&self, key: &str) -> Option<Value<'_>> {
        self.fields
            .iter()
            .find(|f| js_raw::is_key(&f.key, key))
            .map(|f| Value {
                written: &f.value,
                number: f.number,
                source: &f.source,
            })
    }
    pub fn is(&self, key: &str, value: &str) -> bool {
        self.get(key).is_some_and(|v| v.written == value)
    }
    fn pick<'a>(&'a self, keys: &[&str], fallback: &'a str) -> &'a str {
        keys.iter()
            .find_map(|key| self.get(key).filter(|v| v.truthy()).map(|v| v.written))
            .unwrap_or(fallback)
    }
}
fn reply(status: u16, written: String) -> Reply {
    Reply {
        status,
        body: ReplyBody::Json(written),
        retry_after: None,
    }
}
fn error(status: u16, value: &str) -> Reply {
    reply(status, format!("{{\"error\":{value}}}"))
}
fn internal() -> Reply {
    error(500, "\"Internal error\"")
}

/// Validate only Core's named fields; ignored data remains accepted regardless of its depth.
pub(crate) fn request(route: Route, text: &str) -> Result<String, &'static str> {
    let d = Doc::parse(text).ok_or("invalid JSON")?;
    let string = |key| d.get(key).is_some_and(|v| v.string());
    if route == Route::ExternalImport {
        if !string("sourcePath") || !string("relPath") {
            return Err("sourcePath and relPath required");
        }
        return Ok(format!(
            "{{\"source_path\":{},\"rel_path\":{}}}",
            d.pick(&["sourcePath"], "\"\""),
            d.pick(&["relPath"], "\"\"")
        ));
    }
    if !string("xid") || !string("me") || d.get("assistant").is_some_and(|v| !v.string()) {
        return Err("xid and me required");
    }
    if d.get("base_hash")
        .is_some_and(|v| !v.nullish() && !v.string())
    {
        return Err("base_hash must be a string");
    }
    let mut out = format!(
        "{{\"xid\":{},\"me\":{},\"assistant\":{},\"month\":{}",
        d.pick(&["xid"], "\"\""),
        d.pick(&["me"], "\"\""),
        d.pick(&["assistant"], "\"\""),
        d.pick(&["month"], "null")
    );
    if let Some(v) = d.get("base_hash").filter(|v| v.string()) {
        out.push_str(",\"base_hash\":");
        out.push_str(v.written);
    }
    out.push('}');
    Ok(out)
}

/// String(value), kept as a JSON string literal so a lone surrogate is never replaced.
fn string_value(value: Value<'_>) -> Option<String> {
    enum Work<'a> {
        Value(Value<'a>, usize),
        Comma,
    }
    let mut work = vec![Work::Value(value, 0)];
    let mut out = String::from("\"");
    while let Some(item) = work.pop() {
        let Work::Value(v, depth) = item else {
            out.push(',');
            continue;
        };
        if depth > 1000 {
            return None;
        }
        let source = v.source.trim_matches([' ', '\t', '\r', '\n']);
        if source.starts_with('"') {
            let written = js_raw::canonical(source)?;
            out.push_str(written.strip_prefix('"')?.strip_suffix('"')?);
        } else if let Some(n) = v.number.or_else(|| source.parse::<f64>().ok()) {
            let quoted = js_raw::quote(&js_json::number_to_string(n));
            out.push_str(quoted.strip_prefix('"')?.strip_suffix('"')?);
        } else if source.starts_with('[') {
            let values: Vec<&serde_json::value::RawValue> = serde_json::from_str(source).ok()?;
            for (i, item) in values.into_iter().enumerate().rev() {
                if item.get() != "null" {
                    work.push(Work::Value(
                        Value {
                            written: item.get(),
                            number: None,
                            source: item.get(),
                        },
                        depth + 1,
                    ));
                }
                if i != 0 {
                    work.push(Work::Comma);
                }
            }
        } else if source.starts_with('{') {
            let d = Doc::parse(source)?;
            if d.get("toString").is_some() {
                return None;
            }
            out.push_str("[object Object]");
        } else {
            out.push_str(v.written);
        }
    }
    out.push('"');
    Some(out)
}

pub(crate) fn response(route: Route, ok: bool, status: u16, body: Doc) -> Reply {
    if ok {
        return reply(
            if matches!(
                route,
                Route::ExternalSources | Route::ExternalImport | Route::Edit
            ) {
                200
            } else {
                status
            },
            body.written,
        );
    }
    if route == Route::Edit && status == 409 && body.is("conflict", "true") {
        return reply(409, body.written);
    }
    if route == Route::WorkspaceTrash {
        return error(
            status,
            body.pick(
                &["detail"],
                "\"Diary recovery request failed. Retry or refresh Trash.\"",
            ),
        );
    }
    if matches!(
        route,
        Route::ExternalSources | Route::ExternalImport | Route::Edit
    ) {
        let fallback = js_raw::quote(&format!("diary sidecar {status}"));
        let keys: &[&str] = if route == Route::ExternalSources {
            &[]
        } else if route == Route::Edit {
            &["detail", "error"]
        } else {
            &["detail"]
        };
        let value = keys.iter().find_map(|k| body.get(k).filter(|v| v.truthy()));
        let written = match value {
            Some(v) => match string_value(v) {
                Some(s) => s,
                None => return internal(),
            },
            None => fallback,
        };
        return error(if status >= 500 { 502 } else { status }, &written);
    }
    let mut out = format!(
        "{{\"error\":{}",
        body.pick(&["detail", "error"], "\"Diary storage request failed\"")
    );
    let throttled = body.is("code", "\"storageThrottled\"");
    if throttled || body.is("code", "\"storageLoginRejected\"") {
        out.push_str(",\"code\":");
        out.push_str(body.pick(&["code"], "null"));
    }
    let mut wait = None;
    if throttled {
        if let Some(v) = body.get("retryAfter") {
            let number = if let Some(n) = v.number {
                n
            } else {
                let written = match string_value(v) {
                    Some(s) => s,
                    None => return internal(),
                };
                let decoded = js_raw::string_of(&written).unwrap_or_default();
                // Number(null) and Number(false) are zero, Number(true) is one.
                if v.written == "true" {
                    1.0
                } else if matches!(v.written, "null" | "false") {
                    0.0
                } else {
                    crate::routes::js_number(&js_json::JValue::Str(decoded)).unwrap_or(f64::NAN)
                }
            };
            if number.is_finite() && number > 0.0 {
                let seconds = number.ceil().min(86400.0) as u64;
                out.push_str(&format!(",\"retryAfter\":{seconds}"));
                wait = Some(seconds);
            }
        }
    }
    out.push('}');
    let mut r = reply(status, out);
    r.retry_after = wait;
    r
}

pub(crate) fn history(body: Doc) -> Reply {
    let log = body
        .get("log")
        .filter(|v| !v.nullish())
        .or_else(|| body.get("today_log"))
        .filter(|v| v.truthy())
        .map_or("\"\"", |v| v.written);
    reply(
        200,
        format!(
            "{{\"todayLog\":{log},\"standing\":{}}}",
            body.pick(&["standing"], "\"\"")
        ),
    )
}
