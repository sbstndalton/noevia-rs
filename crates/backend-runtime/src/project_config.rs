//! Original project metadata patches under the existing shared project write lock.
//! Source/index mutations and provider-catalogue selections remain refused until their owners are wired.
use serde_json::{json, Value};
use server_projects::{store, Ctx, Reply};
const ICONS: &[&str] = &[
    "folder", "book", "study", "pencil", "code", "terminal", "music", "heart", "plant", "work",
    "chart", "globe", "plane", "flask", "coffee", "home", "star", "spark",
];
fn clip(s: &str, n: usize) -> Result<String, Reply> {
    String::from_utf16(&s.encode_utf16().take(n).collect::<Vec<_>>()).map_err(|_| {
        Reply::error(
            503,
            "UTF-16 split boundary is not supported by this candidate",
        )
    })
}
fn invalid(message: &str) -> Reply {
    Reply::error(400, message)
}
fn patch(stored: &Value, body: &Value) -> Result<Value, Reply> {
    let Some(input) = body.as_object() else {
        return Err(invalid("request body must be a JSON object"));
    };
    let mut next = stored.as_object().cloned().ok_or_else(Reply::internal)?;
    // These fields also change external stores, indexes, catalogues or tool registries.
    // Never acknowledge a partial patch when one of those operations is requested.
    if ["files", "sourceFolders", "toolboxes", "provider", "model"]
        .iter()
        .any(|k| input.contains_key(*k))
    {
        return Err(Reply::error(503,"This native candidate has not adopted source, provider and toolbox configuration workers"));
    }
    if let Some(v) = input.get("icon") {
        if !v.as_str().is_some_and(|v| ICONS.contains(&v)) {
            return Err(invalid("Choose a supported project icon."));
        }
        next.insert("icon".into(), v.clone());
    }
    if let Some(v) = input.get("color") {
        let Some(c) = v.as_str().filter(|c| {
            *c == "default"
                || c.len() == 7
                    && c.starts_with('#')
                    && c[1..].bytes().all(|b| b.is_ascii_hexdigit())
        }) else {
            return Err(invalid(
                "Project color must be default or a six-digit hex color.",
            ));
        };
        next.insert("color".into(), json!(c.to_ascii_lowercase()));
    }
    for (key, max) in [("name", 120), ("goal", 2000), ("instructions", 8000)] {
        if let Some(mut value) = input.get(key).and_then(Value::as_str) {
            if key == "name" {
                value = server_projects::js::trim(value);
                if value.is_empty() {
                    continue;
                }
            }
            next.insert(key.into(), json!(clip(value, max)?));
        }
    }
    for key in ["pinned", "archived"] {
        if let Some(v) = input.get(key).filter(|v| v.is_boolean()) {
            next.insert(key.into(), v.clone());
        }
    }
    if let Some(v) = input.get("reasoningEffort") {
        if v.is_null() {
            next.remove("reasoningEffort");
        } else if v
            .as_str()
            .is_some_and(|s| ["default", "low", "high"].contains(&s))
        {
            next.insert("reasoningEffort".into(), v.clone());
        } else {
            return Err(invalid("Invalid reasoning effort"));
        }
    }
    if let Some(v) = input.get("contextProfile") {
        match v.as_str() {
            Some("high") => {
                next.insert("contextProfile".into(), v.clone());
            }
            Some("low") => {
                next.remove("contextProfile");
            }
            _ if v.is_null() => {
                next.remove("contextProfile");
            }
            _ => return Err(invalid("Invalid context profile")),
        }
    }
    if let Some(v) = input.get("routing").and_then(Value::as_str) {
        if !["auto", "manual"].contains(&v) {
            return Err(invalid("routing must be 'auto' or 'manual'"));
        }
        next.insert("routing".into(), json!(v));
        next.insert("routingChosen".into(), json!(true));
    }
    if let Some(v) = input.get("toolsMode") {
        if !v.as_str().is_some_and(|s| ["auto", "manual"].contains(&s)) {
            return Err(invalid("toolsMode must be 'auto' or 'manual'"));
        }
        next.insert("toolsMode".into(), v.clone());
    }
    if let Some(v) = input.get("routingMode") {
        if v.is_null() {
            next.remove("routingMode");
        } else {
            let Some(object) = v.as_object() else {
                return Err(invalid("routingMode must be an object or null"));
            };
            let Some(mode) = object
                .get("mode")
                .and_then(Value::as_str)
                .filter(|v| ["local", "cloud", "hybrid"].contains(v))
            else {
                return Err(invalid(
                    "routingMode.mode must be 'local', 'cloud' or 'hybrid'",
                ));
            };
            let sensitive = match object.get("whenSensitive") {
                None => "ask",
                Some(v) => v
                    .as_str()
                    .filter(|v| ["ask", "local"].contains(v))
                    .ok_or_else(|| invalid("routingMode.whenSensitive must be 'ask' or 'local'"))?,
            };
            next.insert(
                "routingMode".into(),
                json!({"mode":mode,"whenSensitive":sensitive}),
            );
        }
    }
    if let Some(v) = input.get("modes") {
        let Some(list) = v.as_array() else {
            return Err(invalid("modes must be an array"));
        };
        if list.iter().any(|v| {
            !v.as_str()
                .is_some_and(|s| ["chat", "cowork", "code"].contains(&s))
        }) {
            return Err(invalid("modes may only contain chat, cowork, code"));
        }
        let mut modes = vec!["chat"];
        for mode in ["cowork", "code"] {
            if list.contains(&json!(mode)) {
                modes.push(mode)
            }
        }
        next.insert("modes".into(), json!(modes));
    }
    if let Some(v) = input.get("sharedContext") {
        let Some(object) = v.as_object() else {
            return Err(invalid("sharedContext must be an object"));
        };
        if object
            .keys()
            .any(|k| !["chat", "code"].contains(&k.as_str()))
        {
            return Err(invalid("sharedContext may only contain chat, code"));
        }
        let mut out = json!({"chat":false,"code":false});
        for (key, value) in object {
            if !value.is_boolean() {
                return Err(invalid(&format!(
                    "sharedContext.{key} must be true or false"
                )));
            }
            *out.get_mut(key).ok_or_else(Reply::internal)? = value.clone();
        }
        next.insert("sharedContext".into(), out);
    }
    if let Some(list) = input.get("memories").and_then(Value::as_array) {
        next.insert(
            "memories".into(),
            json!(list
                .iter()
                .filter_map(Value::as_str)
                .map(server_projects::js::trim)
                .filter(|s| !s.is_empty())
                .take(50)
                .map(|s| clip(s, 500))
                .collect::<Result<Vec<_>, _>>()?),
        );
    }
    if let Some(v) = input.get("sampling") {
        let mut sampling = serde_json::Map::new();
        for key in ["temperature", "top_p", "top_k", "repeat_penalty"] {
            if let Some(n) = v.get(key).and_then(Value::as_f64) {
                let valid = match key {
                    "temperature" => (0.0..=2.0).contains(&n),
                    "top_p" => n > 0.0 && n <= 1.0,
                    "top_k" => n >= 0.0 && n.fract() == 0.0,
                    _ => (0.0..=3.0).contains(&n),
                };
                if valid {
                    sampling.insert(key.into(), v[key].clone());
                }
            }
        }
        if sampling.is_empty() {
            next.remove("sampling");
        } else {
            next.insert("sampling".into(), Value::Object(sampling));
        }
    }
    Ok(Value::Object(next))
}
pub fn configure(ctx: Ctx<'_>, raw_id: &str, bytes: &[u8]) -> Reply {
    let Some(id) = server_projects::js::decode_uri_component(raw_id) else {
        return invalid("Invalid project identifier");
    };
    if id == "cowork-diary-extras" {
        return Reply::error(404, "no such project");
    }
    let body: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return invalid("invalid JSON"),
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    store::update(&dir, ctx.switch, |projects| {
        let Some(project) = store::find_mut(projects, &id) else {
            return (Reply::error(404, "no such project"), false);
        };
        let Some(encoded) = js_json::wtf16::stringify(project) else {
            return (Reply::internal(), false);
        };
        let stored: Value = match serde_json::from_str(&encoded) {
            Ok(v) => v,
            Err(_) => {
                return (
                    Reply::error(503, "Project contains text not supported by this candidate"),
                    false,
                )
            }
        };
        let mut next = match patch(&stored, &body) {
            Ok(v) => v,
            Err(reply) => return (reply, false),
        };
        if next == stored {
            return (
                Reply::json(200, json!({"ok":true,"unchanged":true}).to_string()),
                false,
            );
        }
        let Some(object) = next.as_object_mut() else {
            return (Reply::internal(), false);
        };
        object.insert("updatedAt".into(), json!(ctx.now_ms));
        match js_json::wtf16::parse(&next.to_string()) {
            Ok(v) => *project = v,
            Err(_) => return (Reply::internal(), false),
        }
        (Reply::json(200, json!({"ok":true}).to_string()), true)
    })
    .unwrap_or_else(|_| Reply::internal())
}
/// File-free project creation uses the original record layout and current account routing default.
/// Uploads and remote folder reservation are refused until their native owners are connected.
pub fn create(ctx: Ctx<'_>, bytes: &[u8], storage_kind: Option<&str>) -> Reply {
    let body: Value = match serde_json::from_slice(bytes) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => return invalid("request body must be a JSON object"),
        Err(_) => return invalid("invalid JSON"),
    };
    if body.get("toolboxes").and_then(Value::as_array).is_some_and(|items|items.iter().any(|v|v.as_str().is_some_and(|s|s!="core"))) {
        return Reply::error(503,"Native non-core toolbox catalogue is not yet available");
    }
    if storage_kind.is_some_and(|s| s != "local")
        || body
            .get("files")
            .and_then(Value::as_array)
            .is_some_and(|files| {
                files.iter().any(|f| {
                    f.get("name").is_some_and(Value::is_string)
                        && f.get("content").is_some_and(Value::is_string)
                })
            })
    {
        return Reply::error(503,"Native project creation with uploads or remote folder reservation is not yet available");
    }
    if body.get("reasoningEffort").is_some_and(|v| {
        !v.as_str()
            .is_some_and(|s| ["default", "low", "high"].contains(&s))
    }) {
        return invalid("Invalid reasoning effort");
    }
    let parsed = match js_json::wtf16::parse(&body.to_string()) {
        Ok(v) => v,
        Err(_) => return invalid("invalid JSON"),
    };
    let coerce = |key: &str, max: usize| -> Result<String, Reply> {
        let v = parsed.get(key);
        let text = if v.truthy() {
            js_json::to_js_string(v).map_err(|_| Reply::internal())?
        } else {
            String::new()
        };
        clip(
            if key == "name" {
                server_projects::js::trim(&text)
            } else {
                &text
            },
            max,
        )
    };
    let name = match coerce("name", 120) {
        Ok(v) if !v.is_empty() => v,
        Ok(_) => return invalid("name required"),
        Err(v) => return v,
    };
    let goal = match coerce("goal", 2000) {
        Ok(v) => v,
        Err(v) => return v,
    };
    let instructions = match coerce("instructions", 8000) {
        Ok(v) => v,
        Err(v) => return v,
    };
    let mut appearance = serde_json::Map::new();
    for key in ["icon", "color", "modes"] {
        if let Some(v) = body.get(key) {
            appearance.insert(key.into(), v.clone());
        }
    }
    let appearance = match patch(&json!({}), &Value::Object(appearance)) {
        Ok(v) => v,
        Err(v) => return v,
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    let preferences = match store::read_json(&dir.join("preferences.json")) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    let default = if preferences
        .as_ref()
        .is_some_and(|v| v.get("defaultRouting").as_str() == Some("manual"))
    {
        "manual"
    } else {
        "auto"
    };
    let routing = body
        .get("routing")
        .and_then(Value::as_str)
        .filter(|r| ["manual", "auto"].contains(r))
        .unwrap_or(default);
    let boxes = body
        .get("toolboxes")
        .and_then(Value::as_array)
        .map(|items| {
            if items.iter().any(|v| v.as_str() == Some("core")) {
                vec!["core"]
            } else {
                Vec::new()
            }
        })
        .unwrap_or_else(|| vec!["core"]);
    let tools_mode = body
        .get("toolsMode")
        .and_then(Value::as_str)
        .filter(|s| ["manual", "auto"].contains(s))
        .unwrap_or(if body.get("toolboxes").is_some_and(Value::is_array) {
            "manual"
        } else {
            "auto"
        });
    let id = format!("proj-{}-{}", ctx.now_ms, server_projects::js::random36(6));
    let mut project = json!({"id":id,"name":name,"goal":goal,"instructions":instructions,"pinned":false,"archived":false,"sourceFolders":[],"memories":[],"files":[],"routing":routing,"modes":["chat"],"toolboxes":boxes,"toolsMode":tools_mode,"chats":[],"createdAt":ctx.now_ms,"updatedAt":ctx.now_ms});
    let Some(object) = project.as_object_mut() else {
        return Reply::internal();
    };
    if let Some(fields) = appearance.as_object() {
        object.extend(fields.clone());
    }
    for key in ["model", "provider"] {
        if let Some(v) = body
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            object.insert(key.into(), json!(v));
        }
    }
    if let Some(v) = body.get("reasoningEffort") {
        object.insert("reasoningEffort".into(), v.clone());
    }
    let original = match js_json::wtf16::parse(&project.to_string()) {
        Ok(v) => v,
        Err(_) => return Reply::internal(),
    };
    store::update(&dir, ctx.switch, |projects| {
        if store::find(projects, &id).is_some() {
            return (
                Reply::error(409, "Project identifier collision; retry creation"),
                false,
            );
        }
        projects.insert(0, original);
        (Reply::json(200, project.to_string()), true)
    })
    .unwrap_or_else(|_| Reply::internal())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_config_preserves_unknown_state_and_rejects_partial_updates(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let user = "11111111-2222-4333-8444-555555555555";
        let root = dir.path().join("users").join(user);
        std::fs::create_dir_all(&root)?;
        let file = root.join("projects.json");
        std::fs::write(
            &file,
            r#"{"projects":[{"id":"p","name":"Before","files":[],"sampling":{"temperature":0.6},"future":{"retain":true}}]}"#,
        )?;
        let ctx = Ctx {
            data_dir: dir.path(),
            user_id: user,
            switch: server_store::RustProjects::owned(),
            now_ms: 42,
        };
        let before = std::fs::read(&file)?;
        assert_eq!(
            configure(
                ctx,
                "p",
                br#"{"name":"Changed","sampling":null,"toolsMode":"bad"}"#
            )
            .status,
            400
        );
        assert_eq!(std::fs::read(&file)?, before);
        assert_eq!(
            configure(ctx, "p", br#"{"name":"Changed","files":[]}"#).status,
            503
        );
        assert_eq!(std::fs::read(&file)?, before);
        assert_eq!(configure(ctx,"p",br#"{"name":" Renamed ","sampling":{"top_p":5,"temperature":0.4},"memories":[" keep ",7],"modes":["code"],"pinned":true}"#).status,200);
        let stored: Value = serde_json::from_slice(&std::fs::read(&file)?)?;
        let p = stored.pointer("/projects/0").ok_or("project")?;
        assert_eq!(p["name"], "Renamed");
        assert_eq!(p["sampling"], json!({"temperature":0.4}));
        assert_eq!(p["memories"], json!(["keep"]));
        assert_eq!(p["modes"], json!(["chat", "code"]));
        assert_eq!(p.pointer("/future/retain").unwrap_or(&Value::Null), true);
        assert_eq!(p["updatedAt"], 42);
        let before = std::fs::read(&file)?;
        let reply = configure(ctx, "p", br#"{"name":"Renamed"}"#);
        assert!(matches!(reply.body,server_projects::Body::Json(ref s)if s.contains("unchanged")));
        assert_eq!(std::fs::read(&file)?, before);
        assert_eq!(configure(ctx, "absent", b"{}").status, 404);
        Ok(())
    }
}

#[cfg(test)]
mod create_tests {
    use super::*;
    #[test]
    fn native_creation_uses_original_record_and_preserves_existing_projects()->Result<(),Box<dyn std::error::Error>> {
        let root=tempfile::tempdir()?;let user="11111111-2222-4333-8444-555555555555";let dir=root.path().join("users").join(user);std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("projects.json"),r#"{"projects":[{"id":"existing","unknown":true}]}"#)?;
        std::fs::write(dir.join("preferences.json"),r#"{"defaultRouting":"manual"}"#)?;
        let ctx=Ctx{data_dir:root.path(),user_id:user,switch:server_store::RustProjects::owned(),now_ms:123};
        let before=std::fs::read(dir.join("projects.json"))?;
        assert_eq!(create(ctx,br#"{"name":"Remote"}"#,Some("webdav")).status,503);
        assert_eq!(create(ctx,br#"{"name":"With upload","files":[{"name":"n.md","content":"synthetic"}]}"#,None).status,503);
        assert_eq!(std::fs::read(dir.join("projects.json"))?,before);
        let reply=create(ctx,br#"{"name":" New project ","goal":7,"modes":["code"],"toolboxes":[],"color":"#ABCDEF"}"#,None);assert_eq!(reply.status,200);
        let server_projects::Body::Json(text)=reply.body else{return Err("body".into())};let project:Value=serde_json::from_str(&text)?;
        assert_eq!(project.get("name"),Some(&json!("New project")));assert_eq!(project.get("goal"),Some(&json!("7")));assert_eq!(project.get("routing"),Some(&json!("manual")));assert_eq!(project.get("toolsMode"),Some(&json!("manual")));assert_eq!(project.get("toolboxes"),Some(&json!([])));assert_eq!(project.get("modes"),Some(&json!(["chat","code"])));
        let stored:Value=serde_json::from_slice(&std::fs::read(dir.join("projects.json"))?)?;assert_eq!(stored.pointer("/projects/0"),Some(&project));assert_eq!(stored.pointer("/projects/1/unknown"),Some(&json!(true)));Ok(())
    }
}
