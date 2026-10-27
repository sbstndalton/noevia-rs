//! Buffered HTTP admission for the control engine, not a live listener route.
//! The transport must bound Incoming collection/read time before this function.
//! Authenticated bytes are not proof of current boot/session or mutation authority.
use super::{replay, wire};
use egress::control::{
    derive_control_key, parse_deadline_ms, ControlKey, FileKey, RequestMetadata, Route,
    MAX_REQUEST_BYTES, MAX_SAFE_INTEGER,
};
use hyper::{
    header::{HeaderMap, HeaderValue},
    Method, Request, Response, Version,
};
pub const ID_HEADER: &str = "x-noevia-request-id";
pub const DEADLINE_HEADER: &str = "x-noevia-deadline-ms";
pub const BOOT_HEADER: &str = "x-noevia-boot-id";
pub const FINGERPRINT_HEADER: &str = "x-noevia-key-fingerprint";
pub const MAC_HEADER: &str = "x-noevia-control-mac";
pub const MAX_HEADERS: usize = 16;
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Framing,
    Capacity,
    Authentication,
    Stale,
    Body,
    Internal,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "control HTTP {self:?}")
    }
}
impl std::error::Error for Error {}
/// Immutable proof of bounded, fresh, MAC-authenticated bytes at admission only.
/// Controller must recheck current key/boot/session and time before any mutation.
pub struct Authenticated {
    id: String,
    deadline_ms: u64,
    boot: Option<String>,
    fingerprint: String,
    mac: String,
    key: ControlKey,
    body: wire::Request,
}
impl std::fmt::Debug for Authenticated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthenticatedControl")
            .field("route", &self.body.route())
            .finish_non_exhaustive()
    }
}
impl Authenticated {
    pub fn body(&self) -> &wire::Request {
        &self.body
    }
    pub fn boot_id(&self) -> Option<&str> {
        self.boot.as_deref()
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn request_id(&self) -> &str {
        &self.id
    }
    pub fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }
    /// Reserve before mutation. `wall_ms` must be current native server wall time.
    pub fn reserve(
        &self,
        cache: &mut replay::Replay,
        wall_ms: u64,
    ) -> Result<replay::Admission, replay::Error> {
        cache.begin(&self.id, &self.mac, self.deadline_ms, wall_ms)
    }
    /// Exact original result bytes/status are signed, including replayed historical
    /// outcomes. Controller must supply bounded terminal JSON and never infer live
    /// state from the act of signing. Clients bind elapsed time to original send.
    pub fn response(&self, status: u16, body: Vec<u8>) -> Result<Response<Vec<u8>>, Error> {
        let mac = self
            .key
            .sign_response(&self.mac, status, &body)
            .map_err(|e| {
                if e == egress::control::Error::Capacity {
                    Error::Capacity
                } else {
                    Error::Internal
                }
            })?;
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .header("content-length", body.len())
            .header("cache-control", "no-store")
            .header("connection", "close")
            .header(MAC_HEADER, mac)
            .body(body)
            .map_err(|_| Error::Internal)
    }
}
pub fn authenticate(
    req: &Request<Vec<u8>>,
    file: &FileKey,
    wall_ms: u64,
) -> Result<Authenticated, Error> {
    if req.body().len() > MAX_REQUEST_BYTES {
        return Err(Error::Capacity);
    }
    if req.method() != Method::POST
        || req.version() != Version::HTTP_11
        || req.uri().scheme().is_some()
        || req.uri().authority().is_some()
        || req.uri().query().is_some()
    {
        return Err(Error::Framing);
    }
    let route = Route::parse(req.uri().path()).map_err(|_| Error::Framing)?;
    let h = req.headers();
    if h.len() > MAX_HEADERS {
        return Err(Error::Capacity);
    }
    let mut size = 2usize; // final CRLF, plus each field's colon/space/CRLF below
    for name in h.keys() {
        if h.get_all(name).iter().count() != 1 {
            return Err(Error::Framing);
        }
        if !matches!(
            name.as_str(),
            "host" | "content-type" | "content-length" | "connection" | "user-agent" | "accept"
        ) && ![
            ID_HEADER,
            DEADLINE_HEADER,
            BOOT_HEADER,
            FINGERPRINT_HEADER,
            MAC_HEADER,
        ]
        .contains(&name.as_str())
        {
            return Err(Error::Framing);
        }
        let value = h.get(name).ok_or(Error::Framing)?;
        size = size
            .checked_add(name.as_str().len() + value.as_bytes().len() + 4)
            .ok_or(Error::Capacity)?;
        if size > MAX_HEADER_BYTES {
            return Err(Error::Capacity);
        }
    }
    let host = one(h, "host")?;
    if host.is_empty() || host.len() > 255 || host.bytes().any(|v| v.is_ascii_whitespace()) {
        return Err(Error::Framing);
    }
    if one(h, "content-type")? != "application/json"
        || one(h, "content-length")? != req.body().len().to_string()
    {
        return Err(Error::Framing);
    }
    if let Some(v) = h.get("connection") {
        if !matches!(ascii(v)?, "close" | "keep-alive") {
            return Err(Error::Framing);
        }
    }
    let id = one(h, ID_HEADER)?;
    let deadline_ms = parse_deadline_ms(one(h, DEADLINE_HEADER)?).map_err(|_| Error::Framing)?;
    let boot = h.get(BOOT_HEADER).map(ascii).transpose()?;
    let fingerprint = one(h, FINGERPRINT_HEADER)?;
    let mac = one(h, MAC_HEADER)?;
    if wall_ms == 0
        || wall_ms > MAX_SAFE_INTEGER
        || deadline_ms <= wall_ms
        || deadline_ms - wall_ms > replay::MAX_FRESHNESS_MS
    {
        return Err(Error::Stale);
    }
    if fingerprint != file.fingerprint() {
        return Err(Error::Authentication);
    }
    let key = derive_control_key(file).map_err(|_| Error::Internal)?;
    let meta = RequestMetadata {
        request_id: id,
        deadline_ms,
        boot_id: boot,
        key_fingerprint: fingerprint,
    };
    key.verify_request(route, meta, req.body(), mac)
        .map_err(|_| Error::Authentication)?;
    let body = wire::Request::decode(route, meta, req.body()).map_err(|_| Error::Body)?;
    Ok(Authenticated {
        id: id.into(),
        deadline_ms,
        boot: boot.map(str::to_owned),
        fingerprint: fingerprint.into(),
        mac: mac.into(),
        key,
        body,
    })
}
fn one<'a>(h: &'a HeaderMap, name: &str) -> Result<&'a str, Error> {
    ascii(h.get(name).ok_or(Error::Framing)?)
}
fn ascii(v: &HeaderValue) -> Result<&str, Error> {
    v.to_str().map_err(|_| Error::Framing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const ID: &str = "00112233445566778899aabbccddeeff";
    fn file() -> FileKey {
        FileKey::from_bytes([7; 32])
    }
    fn request() -> Result<Request<Vec<u8>>, Box<dyn std::error::Error>> {
        let file = file();
        let fp = file.fingerprint();
        let body=json!({"version":1,"mode":"initial","challenge":ID,"expectedKeyFingerprint":fp,"priorSession":null}).to_string().into_bytes();
        let meta = RequestMetadata {
            request_id: ID,
            deadline_ms: 2000,
            boot_id: None,
            key_fingerprint: &fp,
        };
        let mac = derive_control_key(&file)?.sign_request(Route::Ready, meta, &body)?;
        Ok(Request::builder()
            .method("POST")
            .uri(Route::Ready.path())
            .version(Version::HTTP_11)
            .header("host", "egress-proxy:3001")
            .header("content-type", "application/json")
            .header("content-length", body.len())
            .header(ID_HEADER, ID)
            .header(DEADLINE_HEADER, "2000")
            .header(FINGERPRINT_HEADER, &fp)
            .header(MAC_HEADER, mac)
            .body(body)?)
    }
    #[test]
    fn authenticated_request_binds_reservation_and_exact_signed_response(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let req = request()?;
        let auth = authenticate(&req, &file(), 1000)?;
        assert_eq!(auth.body().route(), Route::Ready);
        assert_eq!(auth.request_id(), ID);
        assert_eq!(auth.boot_id(), None);
        assert_eq!(auth.deadline_ms(), 2000);
        assert_eq!(auth.fingerprint(), file().fingerprint());
        assert!(!format!("{auth:?}").contains(ID));
        let mut cache = replay::Replay::new();
        let t = match auth.reserve(&mut cache, 1000)? {
            replay::Admission::Execute(t) => t,
            _ => return Err("missing execution ticket".into()),
        };
        let bytes = b"{\"state\":\"unavailable\"}".to_vec();
        cache.complete(t, 503, bytes.clone())?;
        let result = match auth.reserve(&mut cache, 1001)? {
            replay::Admission::Cached(c) => c,
            _ => return Err("missing cached result".into()),
        };
        let response = auth.response(result.status, result.body.to_vec())?;
        assert_eq!(response.body(), &bytes);
        assert_eq!(response.status(), 503);
        let mac = one(response.headers(), MAC_HEADER)?;
        let request_mac = one(req.headers(), MAC_HEADER)?;
        let key = derive_control_key(&file())?;
        key.verify_response(request_mac, 503, response.body(), mac)?;
        assert!(key
            .verify_response(request_mac, 200, response.body(), mac)
            .is_err());
        assert!(key.verify_response(request_mac, 503, b"{}", mac).is_err());
        assert_eq!(
            auth.reserve(&mut cache, 2000).err(),
            Some(replay::Error::Stale)
        );
        assert!(auth
            .response(200, vec![b' '; egress::control::MAX_RESPONSE_BYTES + 1])
            .is_err());
        Ok(())
    }
    #[test]
    fn authenticated_renew_requires_known_boot_and_preserves_body_identity(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let f = file();
        let fp = f.fingerprint();
        let mut r = request()?;
        *r.uri_mut() = Route::Renew.path().parse()?;
        *r.body_mut() = json!({"version":1,"sessionId":ID,"sequence":1})
            .to_string()
            .into_bytes();
        let length = r.body().len();
        r.headers_mut()
            .insert("content-length", length.to_string().parse()?);
        r.headers_mut().insert(BOOT_HEADER, ID.parse()?);
        let meta = RequestMetadata {
            request_id: ID,
            deadline_ms: 2000,
            boot_id: Some(ID),
            key_fingerprint: &fp,
        };
        let tag = derive_control_key(&f)?.sign_request(Route::Renew, meta, r.body())?;
        r.headers_mut().insert(MAC_HEADER, tag.parse()?);
        let auth = authenticate(&r, &f, 1000)?;
        assert_eq!(auth.boot_id(), Some(ID));
        assert_eq!(auth.body().renewal(), Some((ID, 1)));
        r.headers_mut().remove(BOOT_HEADER);
        assert_eq!(
            authenticate(&r, &f, 1000).err(),
            Some(Error::Authentication)
        );
        r.headers_mut().insert(BOOT_HEADER, ID.parse()?);
        *r.uri_mut() = Route::Activity.path().parse()?;
        assert_eq!(
            authenticate(&r, &f, 1000).err(),
            Some(Error::Authentication)
        );
        Ok(())
    }
    #[test]
    fn ambiguous_http_framing_and_lookalike_paths_refuse() -> Result<(), Box<dyn std::error::Error>>
    {
        for (name, value) in [
            ("transfer-encoding", "chunked"),
            ("expect", "100-continue"),
            ("upgrade", "websocket"),
            ("trailer", "content-length"),
            ("proxy-authorization", "Basic task-grant"),
            ("authorization", "Bearer task-grant"),
            ("x-noevia-unknown", "1"),
            ("cookie", "secret"),
        ] {
            let mut r = request()?;
            r.headers_mut().insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes())?,
                value.parse()?,
            );
            assert_eq!(authenticate(&r, &file(), 1000).err(), Some(Error::Framing));
        }
        for name in [
            ID_HEADER,
            DEADLINE_HEADER,
            FINGERPRINT_HEADER,
            MAC_HEADER,
            "host",
            "content-type",
            "content-length",
        ] {
            let mut r = request()?;
            r.headers_mut().append(
                hyper::header::HeaderName::from_bytes(name.as_bytes())?,
                "duplicate".parse()?,
            );
            assert_eq!(authenticate(&r, &file(), 1000).err(), Some(Error::Framing));
        }
        for path in [
            "http://egress-proxy:3001/v1/control/ready",
            "/v1/control/ready?x=1",
            "/v1/control/ready/",
            "/v1/control/%72eady",
        ] {
            let mut r = request()?;
            *r.uri_mut() = path.parse()?;
            assert_eq!(authenticate(&r, &file(), 1000).err(), Some(Error::Framing));
        }
        for method in [Method::GET, Method::CONNECT] {
            let mut r = request()?;
            *r.method_mut() = method;
            assert_eq!(authenticate(&r, &file(), 1000).err(), Some(Error::Framing));
        }
        let mut r = request()?;
        *r.version_mut() = Version::HTTP_10;
        assert_eq!(authenticate(&r, &file(), 1000).err(), Some(Error::Framing));
        for (name, value) in [
            ("content-length", "00"),
            ("content-length", "1"),
            ("content-type", "application/json; charset=utf-8"),
            ("connection", "upgrade"),
            ("host", "has space"),
        ] {
            let mut r = request()?;
            r.headers_mut().insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes())?,
                value.parse()?,
            );
            assert_eq!(authenticate(&r, &file(), 1000).err(), Some(Error::Framing));
        }
        Ok(())
    }
    #[test]
    fn mac_key_body_and_freshness_fail_before_execution() -> Result<(), Box<dyn std::error::Error>>
    {
        let r = request()?;
        assert_eq!(
            authenticate(&r, &FileKey::from_bytes([8; 32]), 1000).err(),
            Some(Error::Authentication)
        );
        for wall in [0, 2000, 2001, MAX_SAFE_INTEGER + 1] {
            assert_eq!(authenticate(&r, &file(), wall).err(), Some(Error::Stale));
        }
        for (name, value) in [
            (MAC_HEADER, "task-grant"),
            (MAC_HEADER, &"0".repeat(64)),
            (ID_HEADER, &"0".repeat(32)),
        ] {
            let mut r = request()?;
            r.headers_mut().insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes())?,
                value.parse()?,
            );
            assert_eq!(
                authenticate(&r, &file(), 1000).err(),
                Some(Error::Authentication)
            );
        }
        let mut r = request()?;
        r.body_mut().push(b' ');
        let length = r.body().len();
        r.headers_mut()
            .insert("content-length", length.to_string().parse()?);
        assert_eq!(
            authenticate(&r, &file(), 1000).err(),
            Some(Error::Authentication)
        );
        let mut r = request()?;
        r.headers_mut().insert(DEADLINE_HEADER, "02000".parse()?);
        assert_eq!(authenticate(&r, &file(), 1000).err(), Some(Error::Framing));
        let mut r = request()?;
        r.headers_mut().insert(DEADLINE_HEADER, "31001".parse()?);
        assert_eq!(authenticate(&r, &file(), 1000).err(), Some(Error::Stale));
        Ok(())
    }
    #[test]
    fn authenticated_json_still_needs_semantics_and_capacity_limits(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut r = request()?;
        *r.body_mut() = b"{}".to_vec();
        r.headers_mut().insert("content-length", "2".parse()?);
        let f = file();
        let fp = f.fingerprint();
        let m = RequestMetadata {
            request_id: ID,
            deadline_ms: 2000,
            boot_id: None,
            key_fingerprint: &fp,
        };
        let tag = derive_control_key(&f)?.sign_request(Route::Ready, m, r.body())?;
        r.headers_mut().insert(MAC_HEADER, tag.parse()?);
        assert_eq!(authenticate(&r, &f, 1000).err(), Some(Error::Body));
        let mut r = request()?;
        *r.body_mut() = vec![b' '; MAX_REQUEST_BYTES + 1];
        assert_eq!(authenticate(&r, &f, 1000).err(), Some(Error::Capacity));
        let mut r = request()?;
        r.headers_mut().insert(
            "user-agent",
            HeaderValue::from_bytes(&vec![b'a'; MAX_HEADER_BYTES])?,
        );
        assert_eq!(authenticate(&r, &f, 1000).err(), Some(Error::Capacity));
        let mut r = request()?;
        r.headers_mut().insert(BOOT_HEADER, "".parse()?);
        assert!(authenticate(&r, &f, 1000).is_err());
        Ok(())
    }
}
