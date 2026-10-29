//! Original DAV mutation translation. Storage rules remain in the guarded workspace kernel.
use super::*;
use serde_json::{json, Value};
fn tag(value: &str) -> Option<String> {
    let v = value.strip_prefix('"')?.strip_suffix('"')?;
    (v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then(|| v.to_owned())
}
fn destination(raw: &str, origin: &str, username: &str) -> Result<(String, Url), StatusCode> {
    if raw.is_empty() || raw.len() > 2000 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let base = Url::parse(origin).map_err(|_| StatusCode::BAD_REQUEST)?;
    let url = base.join(raw).map_err(|_| StatusCode::BAD_REQUEST)?;
    if url.origin() != base.origin() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let (user, path) = parse_path(url.path())?;
    if !user.eq_ignore_ascii_case(username) || path.is_empty() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok((path, url))
}
fn destination_tag(raw: &str, destination: &Url) -> Option<String> {
    // Accept only tagged strong entity conditions. Other DAV If expressions do not
    // become a guessed precondition; the actual current target is statted below.
    for part in raw.split('<').skip(1) {
        let (uri, rest) = part.split_once('>')?;
        let rest = rest
            .trim_start()
            .strip_prefix('(')?
            .trim_start()
            .strip_prefix('[')?;
        let (etag, end) = rest.split_once(']')?;
        if !end.trim_start().starts_with(')') {
            continue;
        }
        if destination.join(uri).ok().as_ref() == Some(destination) {
            return tag(etag);
        }
    }
    None
}
impl<B: Backend> DavService<B> {
    async fn operation(
        &self,
        identity: &Identity,
        body: Value,
    ) -> Result<Value, Box<Response<Body>>> {
        match self.call(self.backend.workspace(identity, body)).await {
            Ok((status, value)) if (200..300).contains(&status) => Ok(value),
            Ok((status, _)) => Err(Box::new(reply(
                StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                "Workspace operation refused",
                false,
            ))),
            Err(f) => Err(Box::new(fault_reply(f, false))),
        }
    }
    pub(super) async fn workspace_operation(
        &self,
        req: Request<Body>,
        identity: &Identity,
        username: &str,
        path: &str,
    ) -> Response<Body> {
        match self.workspace_inner(req, identity, username, path).await {
            Ok(r) => r,
            Err(r) => *r,
        }
    }
    async fn workspace_inner(
        &self,
        req: Request<Body>,
        identity: &Identity,
        username: &str,
        path: &str,
    ) -> Result<Response<Body>, Box<Response<Body>>> {
        let bad = |status, text| Box::new(reply(status, text, false));
        if path.is_empty() {
            return Err(bad(
                StatusCode::FORBIDDEN,
                "The diary root cannot be changed",
            ));
        }
        let method = req.method().as_str().to_owned();
        let headers = req.headers().clone();
        for name in [
            "destination",
            "overwrite",
            "depth",
            "if",
            "content-encoding",
        ] {
            if headers.get_all(name).iter().nth(1).is_some() {
                return Err(bad(StatusCode::BAD_REQUEST, "Duplicate operation header"));
            }
        }
        let header = |name: &str| -> Result<Option<&str>, Box<Response<Body>>> {
            headers
                .get(name)
                .map(|v| {
                    v.to_str()
                        .map_err(|_| bad(StatusCode::BAD_REQUEST, "Invalid operation header"))
                })
                .transpose()
        };
        if header("content-encoding")?.is_some_and(|v| v != "identity") {
            return Err(bad(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Encoded bodies are unsupported",
            ));
        }
        let body =
            tokio::time::timeout(self.config.deadline, to_bytes(req.into_body(), 8192)).await;
        if !matches!(body, Ok(Ok(ref b)) if b.is_empty()) {
            return Err(bad(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Request bodies are unsupported",
            ));
        }
        let depth = header("depth")?;
        if depth.is_some_and(|d| d != "infinity" && !(method == "COPY" && d == "0")) {
            return Err(bad(StatusCode::BAD_REQUEST, "Invalid Depth"));
        }
        let mut version = header("if-match")?
            .map(|v| {
                tag(v)
                    .ok_or_else(|| bad(StatusCode::BAD_REQUEST, "A single strong ETag is required"))
            })
            .transpose()?;
        if !self.is_allowed(identity).await {
            return Err(bad(StatusCode::FORBIDDEN, "Sharing was disabled"));
        }
        if version.is_none() && method != "COPY" {
            let stat = self
                .operation(identity, json!({"op":"stat","path":path}))
                .await?;
            version = Some(
                stat.get("version")
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad(StatusCode::SERVICE_UNAVAILABLE, "Invalid stat result"))?
                    .to_owned(),
            );
        }
        if method == "DELETE" {
            self.operation(
                identity,
                json!({"op":"delete","path":path,"version":version}),
            )
            .await?;
            return Ok(*bad(StatusCode::NO_CONTENT, ""));
        }
        let (target, url) = destination(
            header("destination")?.unwrap_or(""),
            &self.config.origin,
            username,
        )
        .map_err(|s| bad(s, "Invalid Destination"))?;
        let overwrite = match header("overwrite")? {
            None => true,
            Some(v) if v.eq_ignore_ascii_case("T") => true,
            Some(v) if v.eq_ignore_ascii_case("F") => false,
            _ => return Err(bad(StatusCode::BAD_REQUEST, "Overwrite must be T or F")),
        };
        let mut target_version = header("if")?.and_then(|v| destination_tag(v, &url));
        if overwrite && target_version.is_none() {
            match self
                .operation(identity, json!({"op":"stat","path":target}))
                .await
            {
                Ok(stat) => {
                    target_version = Some(
                        stat.get("version")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                bad(StatusCode::SERVICE_UNAVAILABLE, "Invalid stat result")
                            })?
                            .to_owned(),
                    )
                }
                Err(reply) if reply.status() == StatusCode::NOT_FOUND => (),
                Err(reply) => return Err(reply),
            }
        }
        if method == "COPY" && depth == Some("0") {
            let stat = self
                .operation(identity, json!({"op":"stat","path":path}))
                .await?;
            if stat.get("isDir") != Some(&Value::Bool(false)) {
                return Err(bad(
                    StatusCode::FORBIDDEN,
                    "Copying a folder without contents is unsupported",
                ));
            }
        }
        let mut body = json!({"op":method.to_ascii_lowercase(),"path":path,"destination":target,"overwrite":overwrite});
        if let Some(v) = version {
            body.as_object_mut()
                .ok_or_else(|| bad(StatusCode::INTERNAL_SERVER_ERROR, "Invalid request object"))?
                .insert("version".into(), json!(v));
        }
        if let Some(v) = target_version {
            body.as_object_mut()
                .ok_or_else(|| bad(StatusCode::INTERNAL_SERVER_ERROR, "Invalid request object"))?
                .insert("destinationVersion".into(), json!(v));
        }
        let result = self.operation(identity, body).await?;
        Ok(*bad(
            if result.get("replaced") == Some(&Value::Bool(true)) {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::CREATED
            },
            "",
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn destination_is_same_origin_same_tenant_and_strong_tagged() -> Result<(), StatusCode> {
        assert!(destination("https://evil.test/dav/me/a.md", "https://share.test", "me").is_err());
        assert!(destination("/dav/other/a.md", "https://share.test", "me").is_err());
        assert!(destination("/dav/me/%2eprivate/a.md", "https://share.test", "me").is_err());
        let (path, url) = destination("/dav/me/Notes/a.md", "https://share.test", "me")?;
        assert_eq!(path, "Notes/a.md");
        let hash = "a".repeat(64);
        assert_eq!(
            destination_tag(&format!("<{}> ([\"{}\"])", url, hash), &url),
            Some(hash)
        );
        assert_eq!(tag("W/\"abc\""), None);
        Ok(())
    }
}
