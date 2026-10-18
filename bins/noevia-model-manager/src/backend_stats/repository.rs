//! Repository-runtime discovery has the reference's exception boundaries (#217).
//! Existing optional sampler/fit callers retain their established discover() contract.
use super::{quote, DockerApi, Fail, Raw, DOCKER_JSON_CAP, J};
use tokio::time::{timeout_at, Instant};

fn json(body: &[u8]) -> Result<J, Fail> {
    let text = std::str::from_utf8(body).map_err(|_| Fail::Raised)?;
    model_files::json::parse_with_limits(
        text,
        model_files::json::Limits {
            bytes: DOCKER_JSON_CAP,
            depth: 64,
            nodes: 200_000,
            string_chars: DOCKER_JSON_CAP,
            string_bytes: DOCKER_JSON_CAP,
            number_bytes: 64,
        },
    )
    .map_err(|error| match error {
        model_files::Error::Json(_) | model_files::Error::NotUtf8 => Fail::Raised,
        _ => Fail::Refused,
    })
}

fn transport(error: &str) -> Fail {
    if error == crate::docker::EXCEEDED || error == "Docker redirect refused" {
        Fail::Refused
    } else {
        Fail::Raised
    }
}

/// After negotiation, RequestException/JSON errors are not DockerException and escape.
/// APIError/NotFound are caught by the reference discovery's DockerException handler.
async fn get(docker: &dyn DockerApi, path: &str, deadline: Instant) -> Result<Option<J>, Fail> {
    let raw = timeout_at(deadline, docker.get(path))
        .await
        .map_err(|_| Fail::Raised)?
        .map_err(transport)?;
    if !(200..300).contains(&raw.status) {
        // create_api_error_from_http_exception catches JSON ValueError only.
        // A non-string explanation is legal for APIError, but 404 calls .lower().
        let explanation = match json(&raw.body) {
            Err(Fail::Raised) => return Ok(None), // invalid JSON -> raw text explanation.
            Err(Fail::Refused) => return Err(Fail::Refused),
            Ok(J::Obj(pairs)) => pairs
                .into_iter()
                .find(|(key, _)| key == "message")
                .map(|(_, value)| value)
                .ok_or(Fail::Raised)?,
            Ok(_) => return Err(Fail::Raised), // ['message'] of non-mapping.
        };
        if raw.status == 404 && explanation.truthy() && !matches!(explanation, J::Str(_)) {
            return Err(Fail::Raised);
        }
        return Ok(None);
    }
    json(&raw.body).map(Some)
}

async fn version(docker: &dyn DockerApi, deadline: Instant) -> Result<Option<String>, Fail> {
    // docker-py's _retrieve_server_version wraps all negotiation exceptions in DockerException.
    let raw: Raw = match timeout_at(deadline, docker.get("/version")).await {
        Err(_) => return Ok(None),
        Ok(Err(error)) => {
            return match transport(error) {
                Fail::Refused => Err(Fail::Refused),
                Fail::Raised => Ok(None),
            }
        }
        Ok(Ok(raw)) => raw,
    };
    if !(200..300).contains(&raw.status) {
        return Ok(None);
    }
    let value = match json(&raw.body) {
        Ok(value) => value,
        Err(Fail::Raised) => return Ok(None),
        Err(Fail::Refused) => return Err(Fail::Refused),
    };
    let Some(J::Str(api)) = value.get("ApiVersion") else {
        return Ok(None);
    };
    let Some((major, minor)) = api.split_once('.') else {
        return Err(Fail::Refused);
    };
    let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>()) else {
        return Err(Fail::Refused);
    };
    if (major, minor) < (1, 24) {
        return Ok(None);
    }
    Ok(Some(api.clone()))
}

fn image_tag(image: &J) -> Result<String, Fail> {
    // docker.models.images.Image.tags filters placeholders, then the service takes the
    // first tag, falling back to Image.short_id. Dict/string iteration is valid Python.
    let first = match image.get("RepoTags") {
        Some(J::Arr(tags)) => tags
            .iter()
            .find(|tag| !matches!(tag, J::Str(s) if s == "<none>:<none>"))
            .cloned(),
        Some(J::Obj(tags)) => tags
            .iter()
            .find(|(tag, _)| tag != "<none>:<none>")
            .map(|(tag, _)| J::Str(tag.clone())),
        Some(J::Str(tags)) => tags.chars().next().map(|ch| J::Str(ch.to_string())),
        None | Some(J::Null) => None,
        Some(_) => return Err(Fail::Raised),
    };
    match first {
        Some(J::Str(tag)) => Ok(tag.to_lowercase()),
        Some(_) => Err(Fail::Raised),
        None => {
            let Some(J::Str(id)) = image.get("Id") else {
                return Err(Fail::Raised);
            };
            let length = if id.starts_with("sha256:") { 19 } else { 12 };
            Ok(id.chars().take(length).collect::<String>().to_lowercase())
        }
    }
}

pub(super) async fn discover(docker: &dyn DockerApi) -> Result<Vec<(String, String)>, Fail> {
    let deadline = Instant::now() + super::fallback::TIMEOUT;
    let Some(_api) = version(docker, deadline).await? else {
        return Ok(Vec::new());
    };
    let Some(list) = get(
        docker,
        "/containers/json?all=true&limit=1025&size=false",
        deadline,
    )
    .await?
    else {
        return Ok(Vec::new());
    };
    let J::Arr(list) = list else {
        return Err(Fail::Raised);
    };
    if list.len() > crate::INVENTORY_CAP {
        return Err(Fail::Refused);
    }
    // Docker SDK containers.list performs ALL inspections before returning its models.
    // An API failure in a later inspect discards the whole list before image/name access.
    let mut containers = Vec::with_capacity(list.len());
    for item in list {
        let Some(J::Str(id)) = item.get("Id") else {
            return Err(Fail::Raised);
        };
        if !super::sendable(id) {
            return Err(Fail::Refused);
        }
        let path = format!("/containers/{}/json", quote(id));
        let Some(attrs) = get(docker, &path, deadline).await? else {
            return Ok(Vec::new());
        };
        if !matches!(attrs, J::Obj(_)) {
            return Err(Fail::Raised);
        }
        containers.push(attrs);
    }
    let mut out = Vec::new();
    for attrs in containers {
        // .get('ImageID', attrs['Image']) evaluates its default eagerly.
        let default = attrs.get("Image").ok_or(Fail::Raised)?;
        let Some(J::Str(image)) = attrs.get("ImageID").or(Some(default)) else {
            return Err(Fail::Raised);
        };
        let hex = image.split(':').nth(1).ok_or(Fail::Raised)?;
        if !super::sendable(hex) {
            return Err(Fail::Refused);
        }
        let path = format!("/images/{}/json", quote(hex));
        let Some(image) = get(docker, &path, deadline).await? else {
            continue;
        };
        if !matches!(image, J::Obj(_)) {
            return Err(Fail::Raised);
        }
        let tag = image_tag(&image)?;
        if !tag.contains("llama.cpp") {
            continue;
        }
        let name = match attrs.get("Name") {
            Some(J::Str(name)) => name.trim_start_matches('/').to_owned(),
            None | Some(J::Null) => continue,
            Some(_) => return Err(Fail::Raised),
        };
        if name != "model-loader" {
            out.push((name, tag));
        }
    }
    Ok(out)
}
