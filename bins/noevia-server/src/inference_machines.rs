#![allow(clippy::indexing_slicing)]
//! Native inference-machine registry. Mutations are committed under the database writer's
//! transaction; network validation is completed before entering that transaction.
use serde_json::{json, Value};

pub const KEY: &str = "inference:machines";
pub const PATH: &str = "/api/models/machines";
pub const SERVICES: &[&str] = &[
    "chatFast",
    "chatSmart",
    "chatCode",
    "chatCreative",
    "vision",
    "decision",
    "sensitivity",
    "embedding",
    "reranker",
    "ocr",
    "documents",
];
pub fn chat(service: &str) -> bool {
    SERVICES[..5].contains(&service)
}
pub fn empty() -> Value {
    json!({"version":1,"machines":[],"assignments":{}})
}
pub fn clean_model(v: &Value) -> Option<String> {
    let s = crate::decision_settings_routes::js_trim(v.as_str()?);
    (!s.is_empty() && js_json::wtf16::len(s) <= 200 && !s.chars().any(|c| c < ' ' || c == '\u{7f}'))
        .then(|| s.to_string())
}
pub fn state(raw: Option<&str>) -> Value {
    let Some(s) = raw.and_then(parse_json) else {
        return empty();
    };
    if s["version"].as_f64() != Some(1.0)
        || !s["machines"].is_array()
        || !(s["assignments"].is_object() || s["assignments"].is_array())
    {
        return empty();
    }
    let machines: Vec<Value> = s["machines"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| {
            m["id"].is_string()
                && m["id"] != "local"
                && m["name"].is_string()
                && m["address"].is_string()
        })
        .map(|m| {
            let mut row = json!({"id":m["id"],"name":m["name"],"address":m["address"]});
            if truthy(&m["unresolved"]) {
                row["unresolved"] = true.into()
            }
            row
        })
        .collect();
    let mut assignments = serde_json::Map::new();
    for service in SERVICES {
        let a = &s["assignments"][service];
        if *service == "embedding"
            || !machines.iter().any(|m| m["id"] == a["machine"])
            || !a["port"]
                .as_f64()
                .is_some_and(|n| n.is_finite() && n.fract() == 0.0)
        {
            continue;
        }
        let mut a = a.clone();
        if chat(service) {
            let Some(model) = clean_model(&a["model"]) else {
                continue;
            };
            a["model"] = model.into();
        }
        let mut row = json!({"machine":a["machine"],"port":a["port"]});
        if chat(service) {
            row["model"] = a["model"].clone();
            if a["needsConfirm"] == true {
                row["needsConfirm"] = true.into()
            }
        }
        if *service == "reranker" {
            row["protocol"] = if a["protocol"] == "llamacpp" {
                "llamacpp"
            } else {
                "tei"
            }
            .into()
        }
        assignments.insert(service.to_string(), row);
    }
    json!({"version":1,"machines":machines,"assignments":assignments})
}
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
        _ => true,
    }
}
pub fn text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Object(_) => "[object Object]".into(),
        Value::Array(a) => a.iter().map(text).collect::<Vec<_>>().join(","),
        _ => v.to_string(),
    }
}
#[derive(Debug, Clone)]
pub struct Refusal(pub u16, pub Value);
pub fn fail(status: u16, code: &str, message: &str) -> Refusal {
    Refusal(status, json!({"error":message,"code":code}))
}
pub fn name(v: &Value) -> Result<String, Refusal> {
    let s = coerce(v)?
        .split(crate::decision_settings_routes::js_space)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if s.is_empty() || js_json::wtf16::len(&s) > 60 || s.chars().any(|c| c < ' ' || c == '\u{7f}') {
        Err(fail(
            400,
            "invalidName",
            "Give the machine a name of up to 60 characters.",
        ))
    } else {
        Ok(s)
    }
}
pub fn port(v: &Value) -> Result<u16, Refusal> {
    let n = if let Some(s) = v.as_str() {
        let s = crate::decision_settings_routes::js_trim(s);
        if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
            None
        } else {
            s.parse::<f64>().ok()
        }
    } else {
        v.as_f64()
    };
    n.filter(|n| n.fract() == 0.0 && *n >= 1.0 && *n <= 65535.0)
        .map(|n| n as u16)
        .ok_or_else(|| {
            fail(
                400,
                "invalidPort",
                "Enter the service’s port, a number from 1 to 65535.",
            )
        })
}
pub fn address(v: &Value) -> Result<String, Refusal> {
    let s = coerce(v)?;
    let s = crate::decision_settings_routes::js_trim(&s);
    if s.is_empty() {
        return Err(fail(
            400,
            "invalidAddress",
            "Enter the machine’s address, for example 192.168.1.20 or pi5.lan.",
        ));
    }
    let s = if s.contains("://") {
        s.to_string()
    } else {
        format!("http://{s}")
    };
    let u = url::Url::parse(&s)
        .map_err(|_| fail(400, "invalidAddress", "That is not a machine address."))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(fail(
            400,
            "invalidAddress",
            "A machine address uses http or https.",
        ));
    }
    if u.port().is_some() {
        return Err(fail(
            400,
            "portInAddress",
            "Leave the port out of the machine address: each service on it has its own port.",
        ));
    }
    if !u.username().is_empty()
        || u.password().is_some_and(|p| !p.is_empty())
        || s.contains(['?', '#'])
        || u.path() != "/"
    {
        return Err(fail(
            400,
            "invalidAddress",
            "A machine address is only http(s) and a host name or IP address.",
        ));
    }
    let base = u.origin().ascii_serialization();
    if crate::decision_settings_routes::parse_service_url(&base, false).is_none() {
        let reason = match u.host() {
            Some(url::Host::Domain(_)) => "not a valid host name".to_string(),
            Some(host) => {
                let ip = match host {
                    url::Host::Ipv4(ip) => Some(std::net::IpAddr::V4(ip)),
                    url::Host::Ipv6(ip) => Some(std::net::IpAddr::V6(ip)),
                    _ => None,
                };
                let host = u.host_str().unwrap_or_default().trim_matches(['[', ']']);
                let why = network_reason(ip);
                format!("{host} is not on this network ({host} is {why})")
            }
            None => "no host".to_string(),
        };
        return Err(fail(
            400,
            "publicAddress",
            &format!("Machines must be on this network (a LAN or Tailscale address): {reason}."),
        ));
    }
    Ok(base)
}
pub fn machine<'a>(s: &'a Value, id: &str) -> Option<&'a Value> {
    s["machines"].as_array()?.iter().find(|m| m["id"] == id)
}
pub fn assignment(s: &Value, service: &str) -> Value {
    s["assignments"]
        .get(service)
        .cloned()
        .unwrap_or_else(|| json!({"machine":"local"}))
}
pub fn target(s: &Value, role: &str) -> Value {
    let service = match role {
        "fast" => "chatFast",
        "smart" => "chatSmart",
        "code" => "chatCode",
        "creative" => "chatCreative",
        "vision" => "vision",
        _ => return Value::Null,
    };
    let a = assignment(s, service);
    let Some(m) = a["machine"].as_str().and_then(|id| machine(s, id)) else {
        return Value::Null;
    };
    let mut t = json!({"role":role,"service":service,"machine":{"id":m["id"],"name":m["name"],"address":m["address"]},"port":a["port"],"model":a["model"],"baseUrl":format!("{}:{}",m["address"].as_str().unwrap_or_default(),a["port"])});
    if a["needsConfirm"] == true {
        t["needsConfirm"] = true.into()
    }
    t
}
pub fn view(s: &Value) -> Value {
    let on = |id: &str| {
        SERVICES
            .iter()
            .filter(|service| assignment(s, service)["machine"] == id)
            .map(|s| Value::from(*s))
            .collect::<Vec<_>>()
    };
    let mut machines = vec![
        json!({"id":"local","builtIn":true,"name":null,"address":null,"services":on("local")}),
    ];
    for m in s["machines"].as_array().into_iter().flatten() {
        let mut m = m.clone();
        m["services"] = json!(on(m["id"].as_str().unwrap_or_default()));
        machines.push(m)
    }
    let services = SERVICES
        .iter()
        .map(|id| {
            let mut a = assignment(s, id);
            a["id"] = (*id).into();
            a["mode"] = if chat(id) {
                "chat"
            } else if *id == "embedding" {
                "reindex"
            } else {
                "runtime"
            }
            .into();
            if ["ocr", "documents"].contains(id) {
                a["enabled"] = false.into()
            }
            a
        })
        .collect::<Vec<_>>();
    json!({"machines":machines,"services":services})
}
/// Pure commit half; the caller has validated DNS and remote served models before this call.
/// Every duplicate/use check observes the transaction's current row, including concurrent writes.
pub fn mutate(s: &mut Value, body: &Value, id: &str, unresolved: bool) -> Result<Value, Refusal> {
    let action = body["action"].as_str().unwrap_or_default();
    let unknown = || fail(404, "unknownMachine", "That machine is not on the list.");
    match action {
        "add" | "update" => {
            let old = if action == "update" {
                Some(
                    machine(s, &coerce(&body["id"])?)
                        .ok_or_else(unknown)?
                        .clone(),
                )
            } else {
                None
            };
            let n = if body.get("name").is_none() && old.is_some() {
                text(&old.as_ref().map(|v| v["name"].clone()).unwrap_or_default())
            } else {
                name(&body["name"])?
            };
            let a = if body.get("address").is_none() && old.is_some() {
                text(
                    &old.as_ref()
                        .map(|v| v["address"].clone())
                        .unwrap_or_default(),
                )
            } else {
                address(&body["address"])?
            };
            let rows = s["machines"].as_array().cloned().unwrap_or_default();
            if action == "add" && rows.len() >= 16 {
                return Err(fail(409, "tooMany", "At most 16 machines can be added."));
            }
            let own = old
                .as_ref()
                .map(|m| text(&m["id"]))
                .unwrap_or_else(|| id.into());
            if rows.iter().any(|m| {
                (action == "add" || m["id"] != own)
                    && text(&m["name"]).to_lowercase() == n.to_lowercase()
            }) {
                return Err(fail(
                    409,
                    "duplicateName",
                    "Another machine already has that name.",
                ));
            }
            let moved = old
                .as_ref()
                .is_some_and(|m| !a.eq_ignore_ascii_case(&text(&m["address"])));
            if (action == "add" || moved)
                && rows.iter().any(|m| {
                    (action == "add" || m["id"] != own)
                        && text(&m["address"]).to_lowercase() == a.to_lowercase()
                })
            {
                return Err(fail(
                    409,
                    "duplicateAddress",
                    "Another machine already has that address.",
                ));
            }
            let mut m = json!({"id":own,"name":n,"address":a});
            if unresolved {
                m["unresolved"] = true.into()
            }
            let mut rows = rows;
            if action == "update" {
                for row in &mut rows {
                    if row["id"] == own {
                        *row = m.clone();
                    }
                }
            } else {
                rows.push(m.clone());
            }
            s["machines"] = rows.into();
            let mut out = view(s);
            out["machine"] = m;
            Ok(out)
        }
        "remove" => {
            let id = coerce(&body["id"])?;
            if machine(s, &id).is_none() {
                return Err(unknown());
            }
            let using = SERVICES
                .iter()
                .filter(|service| assignment(s, service)["machine"] == id)
                .collect::<Vec<_>>();
            if !using.is_empty() {
                let mut e = fail(
                    409,
                    "inUse",
                    "Move the services on this machine to another machine before removing it.",
                );
                e.1["services"] = json!(using);
                return Err(e);
            }
            s["machines"] = s["machines"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["id"] != id)
                .cloned()
                .collect::<Vec<_>>()
                .into();
            Ok(view(s))
        }
        "assign" => {
            let service = coerce(&body["service"])?;
            if !SERVICES.contains(&service.as_str()) {
                return Err(fail(
                    400,
                    "unknownService",
                    "That is not a part of noevia that can be assigned.",
                ));
            }
            let id = coerce(&body["machine"])?;
            if id != "local" && machine(s, &id).is_none() {
                return Err(unknown());
            }
            if service == "embedding" {
                return Err(fail(
                    409,
                    "embeddingOff",
                    "Embeddings are not set up, so there is nothing to move.",
                ));
            }
            if id == "local" {
                if let Some(a) = s["assignments"].as_object_mut() {
                    a.remove(&service);
                }
            } else {
                let p = port(&body["port"])?;
                let mut a = json!({"machine":id,"port":p});
                if chat(&service) {
                    a["model"] = clean_model(&body["model"])
                        .ok_or_else(|| {
                            fail(
                                400,
                                "invalidModel",
                                "Choose the model this machine serves for the role.",
                            )
                        })?
                        .into()
                }
                if service == "reranker" {
                    let protocol = match body.get("protocol") {
                        Some(v) => Value::from(coerce(v)?),
                        None => s["assignments"]["reranker"]["protocol"]
                            .as_str()
                            .unwrap_or("tei")
                            .into(),
                    };
                    if protocol != "tei" && protocol != "llamacpp" {
                        return Err(fail(
                            400,
                            "invalidProtocol",
                            "Choose how the reranker answers: tei or llamacpp.",
                        ));
                    }
                    a["protocol"] = protocol
                }
                s["assignments"][service] = a
            }
            Ok(view(s))
        }
        _ => Err(fail(
            400,
            "invalidAction",
            "action must be one of add, update, remove, assign, tune, tuneAbort",
        )),
    }
}
/// Embedding placement is derived from its active endpoint, never persisted as an assignment.
pub fn embedding(s: &Value, active: &Value, local: Option<&str>) -> Value {
    let Some(raw) = active["baseUrl"].as_str().filter(|s| !s.is_empty()) else {
        return json!({"machine":"local","port":null,"url":null});
    };
    let Ok(url) = url::Url::parse(raw) else {
        return json!({"machine":"other","port":null,"url":raw});
    };
    let address = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    let port = url.port_or_known_default();
    if let Some(m) = s["machines"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| text(&m["address"]).eq_ignore_ascii_case(&address))
    {
        return json!({"machine":m["id"],"port":port,"url":raw});
    }
    let host = url.host_str().unwrap_or_default();
    let is_local = !host.contains('.')
        || host == "localhost"
        || host.starts_with("127.")
        || host == "[::1]"
        || local
            .and_then(|s| url::Url::parse(s).ok())
            .is_some_and(|local| local.origin() == url.origin());
    json!({"machine":if is_local{"local"}else{"other"},"port":if is_local{None}else{port},"url":raw})
}
pub fn view_with(s: &Value, active: &Value, local: Option<&str>, workers: [bool; 2]) -> Value {
    let mut v = view(s);
    let e = embedding(s, active, local);
    if let Some(services) = v["services"].as_array_mut() {
        for row in services {
            let id = text(&row["id"]);
            if id == "embedding" {
                row["machine"] = e["machine"].clone();
                if e["machine"] != "local" && !e["port"].is_null() {
                    row["port"] = e["port"].clone()
                }
            }
            if id == "ocr" || id == "documents" {
                row["enabled"] = workers[usize::from(id == "documents")].into()
            }
        }
    }
    let services = v["services"].as_array().cloned().unwrap_or_default();
    if let Some(machines) = v["machines"].as_array_mut() {
        for machine in machines {
            machine["services"] = services
                .iter()
                .filter(|service| service["machine"] == machine["id"])
                .map(|service| service["id"].clone())
                .collect::<Vec<_>>()
                .into()
        }
    }
    v
}
pub fn parse_json(text: &str) -> Option<Value> {
    let value = js_json::wtf16::parse(text).ok()?;
    serde_json::from_str(&js_json::stringify(&value)?).ok()
}
pub fn encode_json(value: &Value) -> String {
    js_json::parse(&value.to_string())
        .ok()
        .and_then(|v| js_json::wtf16::stringify(&v))
        .unwrap_or_else(|| "null".into())
}
pub(crate) fn coerce(v: &Value) -> Result<String, Refusal> {
    if v.is_null() {
        return Ok(String::new());
    }
    js_json::parse(&v.to_string())
        .ok()
        .and_then(|v| js_json::to_js_string(&v).ok())
        .ok_or_else(|| {
            Refusal(
                500,
                json!({"error":"The inference machines could not be changed."}),
            )
        })
}

pub(crate) fn network_reason(ip: Option<std::net::IpAddr>) -> &'static str {
    use std::net::IpAddr;
    match ip {
        Some(IpAddr::V4(ip)) => {
            let [a, b, c, d] = ip.octets();
            if a == 0 {
                "the unspecified address"
            } else if [a, b, c, d] == [169, 254, 169, 254] || [a, b, c, d] == [100, 100, 100, 200] {
                "a cloud metadata address"
            } else if a == 169 && b == 254 {
                "a link-local address"
            } else if a >= 224 {
                if a == 255 && b == 255 {
                    "the broadcast address"
                } else if a < 240 {
                    "a multicast address"
                } else {
                    "a reserved address"
                }
            } else if a == 192 && b == 0 && (c == 0 || c == 2) {
                "a reserved address"
            } else if (a == 198 && b == 51 && c == 100) || (a == 203 && b == 0 && c == 113) {
                "a documentation address"
            } else {
                "a public address"
            }
        }
        Some(IpAddr::V6(ip)) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return network_reason(Some(IpAddr::V4(v4)));
            }
            let h = ip.segments();
            if h == [0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254] {
                "a cloud metadata address"
            } else if ip.is_unspecified() {
                "the unspecified address"
            } else if h.iter().take(6).all(|n| *n == 0) {
                "a reserved address"
            } else if h[0] & 0xffc0 == 0xfe80 {
                "a link-local address"
            } else if h[0] & 0xff00 == 0xff00 {
                "a multicast address"
            } else if (h[0] == 0x64 && h[1] == 0xff9b)
                || (h[0] == 0x2001 && (h[1] == 0 || h[1] == 0xdb8))
                || (h[0] == 0x100 && h[1..4].iter().all(|n| *n == 0))
            {
                "a reserved address"
            } else {
                "a public address"
            }
        }
        None => "not an address",
    }
}
