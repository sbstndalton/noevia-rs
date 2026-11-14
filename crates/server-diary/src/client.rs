//! core `http.cjs` `fetchJson` and `diary.cjs` `withStorageCredential` over reqwest.
//!
//! * A redirect is an error (`fetchJson` passes `redirect: 'error'`; the sidecar never redirects).
//! * A reply is read up to a cap; a longer one is "too large", never parsed from a part.
//! * With a tenant key set a call goes out without the storage secret and, when the sidecar
//!   answers 428 (it holds no state for that credential reference), once more WITH it, re-signed
//!   with a fresh nonce over the same buffered body. Without a key the secret rides on the
//!   first call and there is no second.

use crate::config::DiaryConfig;
use crate::tenant::{Content, Tenant, TenantError};
use bytes::Bytes;
use js_json::{wtf16, JValue};
use std::time::Duration;
use tokio::time::{timeout_at, Instant};
use url::Url;

/// `DEFAULT_MAX_RESPONSE_BYTES` in http.cjs.
pub const JSON_REPLY_CAP: usize = 8 * 1024 * 1024;
/// undici's default connect timeout, which `fetch` had.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a call to the sidecar produced no reply. Each route maps it to what core's thrown error
/// became there; none carries a detail (an address, a header, a secret).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallError {
    /// The base URL is not a URL, or a header value cannot be sent.
    Config,
    Tenant(TenantError),
    Transport,
    /// A permanent certificate, TLS handshake or protocol configuration failure.
    Tls,
    /// No connection at all: refused, or the name did not resolve (core's `ECONNREFUSED`,
    /// `ENOTFOUND`, `EAI_AGAIN`). Every route treats it as [`CallError::Transport`]; the local
    /// exchange's stream says "nothing was saved" for it alone.
    Refused,
    Timeout,
    Redirect,
    /// A reply this port cannot hold as JS would (see `js_json::wtf16::Refusal::Unsupported`).
    Unsupported,
}

impl From<TenantError> for CallError {
    fn from(e: TenantError) -> Self {
        CallError::Tenant(e)
    }
}

/// `{ ok, status, body }` of `fetchJson`. `body` is `None` for a reply over the cap (core returned
/// `{ ok: false, status, error }`, whose `body` is undefined).
#[derive(Debug, Clone, PartialEq)]
pub struct JsonReply {
    pub ok: bool,
    pub status: u16,
    pub body: Option<JValue>,
}

/// The cap-bounded read of a reply body.
pub enum Capped {
    Bytes(Vec<u8>),
    Overflow,
}

#[derive(Clone)]
pub struct Sidecar {
    config: DiaryConfig,
    http: reqwest::Client,
}

impl Sidecar {
    pub fn new(config: DiaryConfig) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|_| "the Diary sidecar client could not be built".to_owned())?;
        Ok(Self { config, http })
    }

    pub fn config(&self) -> &DiaryConfig {
        &self.config
    }

    /// `${DIARY_BASE}${path_and_query}`, parsed as `fetch` parses it.
    pub(crate) fn url(&self, path_and_query: &str) -> Result<Url, CallError> {
        Url::parse(&format!("{}{path_and_query}", self.config.base)).map_err(|_| CallError::Config)
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_once(
        &self,
        tenant: &Tenant,
        method: &reqwest::Method,
        url: &Url,
        content: Content<'_>,
        body: Option<&Bytes>,
        secret: bool,
        deadline: Instant,
    ) -> Result<reqwest::Response, CallError> {
        let mut headers = tenant.headers(&self.config, method.as_str(), url, secret, content)?;
        if tenant.storage_retry {
            headers.push(("X-Cowork-Storage-Retry", "1".into()));
        }
        let mut request = self.http.request(method.clone(), url.clone());
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(body) = body {
            request = request.body(body.clone());
        }
        let response = timeout_at(deadline, request.send())
            .await
            .map_err(|_| CallError::Timeout)?
            .map_err(map_error)?;
        if response.status().is_redirection() {
            return Err(CallError::Redirect);
        }
        Ok(response)
    }

    /// `withStorageCredential(send)`. `fresh` gives each attempt its deadline (`fetchJson` timed
    /// each call on its own; the export and import proxies share one deadline for both).
    pub(crate) async fn send(
        &self,
        tenant: &Tenant,
        method: reqwest::Method,
        url: &Url,
        content: Content<'_>,
        body: Option<&Bytes>,
        deadline: impl Fn() -> Instant,
    ) -> Result<(reqwest::Response, Instant), CallError> {
        let keyed = !self.config.tenant_key().is_empty();
        let first_deadline = deadline();
        let first = self
            .send_once(tenant, &method, url, content, body, !keyed, first_deadline)
            .await?;
        if !keyed || first.status().as_u16() != 428 {
            return Ok((first, first_deadline));
        }
        // The first reply is dropped unread, which closes its stream.
        drop(first);
        let second_deadline = deadline();
        let second = self
            .send_once(tenant, &method, url, content, body, true, second_deadline)
            .await?;
        Ok((second, second_deadline))
    }

    /// core routes/auth.cjs's last step of an account's deletion: `DELETE /api/internal/tenant` as
    /// that tenant, with the bearer, the tenant id and the assertion only
    /// ([`Tenant::bare`]: the account no longer exists to read a storage connection from), for at
    /// most `timeout`. Whether the sidecar answered ok; a failure is never an error here (the
    /// deletion has happened, and the sidecar's own start-up reconciliation owns what it kept).
    pub async fn delete_tenant(&self, user_id: &str, timeout: Duration) -> bool {
        let tenant = Tenant::bare(user_id);
        self.fetch_json(
            &tenant,
            reqwest::Method::DELETE,
            "/api/internal/tenant",
            None,
            timeout,
        )
        .await
        .is_ok_and(|r| r.ok)
    }

    /// Backup always carries the signed tenant storage credential, including the first attempt.
    pub async fn storage_backup(&self, tenant: &Tenant) -> Result<JsonReply, CallError> {
        let url = self.url("/api/storage-backup")?;
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut response = self
            .send_once(
                tenant,
                &reqwest::Method::POST,
                &url,
                Content::Json(None),
                None,
                true,
                deadline,
            )
            .await?;
        let status = response.status().as_u16();
        let ok = response.status().is_success();
        let body = match read_capped(&mut response, JSON_REPLY_CAP, deadline).await? {
            Capped::Overflow => None,
            Capped::Bytes(bytes) => Some(parse_or_text(&bytes)?),
        };
        Ok(JsonReply {
            status,
            ok: ok && body.is_some(),
            body,
        })
    }

    /// [`Sidecar::fetch_json`] without the parse: `(ok, status, body)`, the body `None` past the cap.
    /// For a reply the caller reads as `JSON.parse` would, without a parsed tree's limits.
    pub(crate) async fn fetch_raw(
        &self,
        tenant: &Tenant,
        method: reqwest::Method,
        path_and_query: &str,
        body: Option<Bytes>,
        timeout: Duration,
    ) -> Result<(bool, u16, Option<Vec<u8>>), CallError> {
        let url = self.url(path_and_query)?;
        let (mut response, deadline) = self
            .send(
                tenant,
                method,
                &url,
                Content::Json(body.as_deref()),
                body.as_ref(),
                || Instant::now() + timeout,
            )
            .await?;
        let status = response.status().as_u16();
        let ok = response.status().is_success();
        match read_capped(&mut response, JSON_REPLY_CAP, deadline).await? {
            Capped::Overflow => Ok((false, status, None)),
            Capped::Bytes(bytes) => Ok((ok, status, Some(bytes))),
        }
    }

    /// `fetchJson` with the tenant headers and the 428 retry (`diaryFetchJson`).
    pub async fn fetch_json(
        &self,
        tenant: &Tenant,
        method: reqwest::Method,
        path_and_query: &str,
        body: Option<Bytes>,
        timeout: Duration,
    ) -> Result<JsonReply, CallError> {
        let url = self.url(path_and_query)?;
        let (mut response, deadline) = self
            .send(
                tenant,
                method,
                &url,
                Content::Json(body.as_deref()),
                body.as_ref(),
                || Instant::now() + timeout,
            )
            .await?;
        let status = response.status().as_u16();
        let ok = response.status().is_success();
        match read_capped(&mut response, JSON_REPLY_CAP, deadline).await? {
            Capped::Overflow => Ok(JsonReply {
                ok: false,
                status,
                body: None,
            }),
            Capped::Bytes(bytes) => Ok(JsonReply {
                ok,
                status,
                body: Some(parse_or_text(&bytes)?),
            }),
        }
    }
}

/// `try { JSON.parse(text) } catch { text }`.
pub(crate) fn parse_or_text(bytes: &[u8]) -> Result<JValue, CallError> {
    let text = String::from_utf8_lossy(bytes).into_owned();
    match wtf16::parse(&text) {
        Ok(value) => Ok(value),
        Err(wtf16::Refusal::Invalid) => Ok(JValue::Str(text)),
        Err(wtf16::Refusal::Unsupported) => Err(CallError::Unsupported),
    }
}

/// Reads at most `cap` bytes; one byte more is [`Capped::Overflow`] and the rest of the stream is
/// left unread (dropping the response closes it).
pub(crate) async fn read_capped(
    response: &mut reqwest::Response,
    cap: usize,
    deadline: Instant,
) -> Result<Capped, CallError> {
    let mut bytes = Vec::new();
    loop {
        let next = timeout_at(deadline, response.chunk())
            .await
            .map_err(|_| CallError::Timeout)?
            .map_err(map_error)?;
        match next {
            None => return Ok(Capped::Bytes(bytes)),
            Some(chunk) => {
                if chunk.len() > cap.saturating_sub(bytes.len()) {
                    return Ok(Capped::Overflow);
                }
                bytes.extend_from_slice(&chunk);
            }
        }
    }
}

pub(crate) fn map_error(error: reqwest::Error) -> CallError {
    if error.is_timeout() {
        CallError::Timeout
    } else if error.is_builder() {
        CallError::Config
    } else if tls_failure(&error) {
        CallError::Tls
    } else if error.is_connect() && never_connected(&error) {
        CallError::Refused
    } else {
        CallError::Transport
    }
}

/// reqwest is built with rustls only. Preserve typed certificate/handshake/protocol errors,
/// including rustls errors carried inside an I/O error, without inspecting or retaining text.
/// A TLS socket that merely drops or times out has no rustls error and stays unavailable.
fn tls_failure(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if error.is::<rustls::Error>() {
            return true;
        }
        if let Some(inner) = error
            .downcast_ref::<std::io::Error>()
            .and_then(|e| e.get_ref())
        {
            if tls_failure(inner) {
                return true;
            }
        }
        cause = error.source();
    }
    false
}

/// A connect error whose cause is a refused connection or a failed name lookup (hyper-util's
/// `dns error`). A connect timeout is not one (core's `ETIMEDOUT`/undici's connect timeout were not).
fn never_connected(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = cause {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::ConnectionRefused {
                return true;
            }
        }
        if e.to_string() == "dns error" {
            return true;
        }
        cause = e.source();
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn check_failure(base: String, http: reqwest::Client, expected: CallError) {
        let sidecar = Sidecar {
            config: DiaryConfig::new(base, "synthetic-bearer", "", String::new),
            http,
        };
        let error = sidecar
            .fetch_json(
                &Tenant::bare("synthetic-user"),
                reqwest::Method::GET,
                "/api/files",
                None,
                // Only a hang that must end in `Timeout` runs on the short clock. Every other case gets a
                // deadline against a hang, so a slow runner cannot turn its answer into a timeout.
                if expected == CallError::Timeout {
                    Duration::from_millis(200)
                } else {
                    Duration::from_secs(30)
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        let reply = crate::routes::call_failed(error);
        let crate::ReplyBody::Json(body) = reply.body else {
            panic!("expected JSON")
        };
        if expected == CallError::Tls {
            assert_eq!(reply.status, 500);
            assert_eq!(body, r#"{"error":"Internal error"}"#);
        } else {
            assert_eq!(reply.status, 503);
            assert_eq!(
                body,
                r#"{"error":"Diary is unavailable right now. Try again shortly.","code":"diaryUnavailable"}"#
            );
        }
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn tls_hostname_failure_is_internal() {
        let cert = rcgen::generate_simple_self_signed(vec!["other.invalid".into()]).unwrap();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key.into())
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = tokio_rustls::TlsAcceptor::from(Arc::new(config))
                .accept(socket)
                .await;
        });
        // Trust this certificate so the failure is specifically the hostname, not unknown issuer.
        let http = reqwest::Client::builder()
            .no_proxy()
            .add_root_certificate(reqwest::Certificate::from_der(cert.cert.der()).unwrap())
            .build()
            .unwrap();
        check_failure(format!("https://{address}"), http, CallError::Tls).await;
        task.await.unwrap();
    }

    #[tokio::test]
    async fn tls_wrong_protocol_is_internal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut hello = [0; 4096];
            let _ = socket.read(&mut hello).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        check_failure(format!("https://{address}"), http(), CallError::Tls).await;
        task.await.unwrap();
    }

    #[tokio::test]
    async fn resolver_failure_remains_unavailable() {
        // Empty static resolution exercises reqwest's resolver failure without external DNS.
        let http = reqwest::Client::builder()
            .no_proxy()
            .resolve_to_addrs("synthetic.invalid", &[])
            .build()
            .unwrap();
        check_failure(
            "http://synthetic.invalid".into(),
            http,
            CallError::Transport,
        )
        .await;
    }

    #[tokio::test]
    async fn refused_timeout_and_dropped_remain_unavailable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        check_failure(format!("http://{address}"), http(), CallError::Refused).await;
        for (scheme, hang) in [
            ("http", false),
            ("http", true),
            ("https", false),
            ("https", true),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                let _ = socket.read(&mut request).await;
                if hang {
                    std::future::pending::<()>().await;
                }
            });
            check_failure(
                format!("{scheme}://{address}"),
                http(),
                if hang {
                    CallError::Timeout
                } else {
                    CallError::Transport
                },
            )
            .await;
            task.abort();
            let _ = task.await;
        }
    }
}
