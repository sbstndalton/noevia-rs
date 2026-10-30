//! Private binary request contract, not authentication or endpoint authorization.
use crate::{
    isolated::{Error, INPUT},
    protocol::{Method, Request},
    LIST_BYTES, TEXT_BYTES,
};
use std::time::Duration;
use zeroize::Zeroizing;
// 002 (noevia#2191 round 3): the method byte names the writes too and a body follows the headers.
const TAG: &[u8; 8] = b"M4REQ002";
fn method_code(method: Method) -> u8 {
    match method {
        Method::Get => 0,
        Method::Propfind => 1,
        Method::Put => 2,
        Method::Mkcol => 3,
        Method::Delete => 4,
    }
}
fn method_of(code: u8) -> Result<Method, Error> {
    Ok(match code {
        0 => Method::Get,
        1 => Method::Propfind,
        2 => Method::Put,
        3 => Method::Mkcol,
        4 => Method::Delete,
        _ => return Err(Error::Input),
    })
}
fn valid(request: &Request) -> Result<(), Error> {
    if request.attempt_timeout.is_zero()
        || request.attempt_timeout > request.method.max_attempt()
        || !matches!(request.body_cap, LIST_BYTES | TEXT_BYTES)
        || request.body.len() > request.method.max_body()
        || request.url.len() > 65536
        || request.headers.len() > 100
    {
        return Err(Error::Input);
    }
    let url = url::Url::parse(&request.url).map_err(|_| Error::Input)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::Input);
    }
    for (name, value) in &request.headers {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            || value.len() > 12 * 1024 * 1024
            || value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
        {
            return Err(Error::Input);
        }
    }
    Ok(())
}
pub fn encode(request: &Request) -> Result<Zeroizing<Vec<u8>>, Error> {
    valid(request)?;
    let mut size = 8usize + 1 + 4 + 8 + 4 + request.url.len() + 2 + 4 + request.body.len();
    for (name, value) in &request.headers {
        size = size
            .checked_add(2 + name.len() + 4 + value.len())
            .ok_or(Error::Input)?;
    }
    if size > INPUT {
        return Err(Error::Input);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    bytes.try_reserve_exact(size).map_err(|_| Error::Input)?;
    bytes.extend_from_slice(TAG);
    bytes.push(method_code(request.method));
    bytes.extend_from_slice(
        &u32::try_from(request.body_cap)
            .map_err(|_| Error::Input)?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(
        &u64::try_from(request.attempt_timeout.as_nanos())
            .map_err(|_| Error::Input)?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(
        &u32::try_from(request.url.len())
            .map_err(|_| Error::Input)?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(request.url.as_bytes());
    bytes.extend_from_slice(
        &u16::try_from(request.headers.len())
            .map_err(|_| Error::Input)?
            .to_be_bytes(),
    );
    for (name, value) in &request.headers {
        bytes.extend_from_slice(
            &u16::try_from(name.len())
                .map_err(|_| Error::Input)?
                .to_be_bytes(),
        );
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(
            &u32::try_from(value.len())
                .map_err(|_| Error::Input)?
                .to_be_bytes(),
        );
        bytes.extend_from_slice(value.as_bytes());
    }
    bytes.extend_from_slice(
        &u32::try_from(request.body.len())
            .map_err(|_| Error::Input)?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(&request.body);
    Ok(bytes)
}
struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], Error> {
        let end = self.position.checked_add(count).ok_or(Error::Input)?;
        let part = self.bytes.get(self.position..end).ok_or(Error::Input)?;
        self.position = end;
        Ok(part)
    }
    fn number<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.take(N)?.try_into().map_err(|_| Error::Input)
    }
    fn text(&mut self, count: usize) -> Result<&'a str, Error> {
        std::str::from_utf8(self.take(count)?).map_err(|_| Error::Input)
    }
}
pub fn decode(bytes: &[u8]) -> Result<Request, Error> {
    if bytes.len() > INPUT {
        return Err(Error::Input);
    }
    let mut cursor = Cursor { bytes, position: 0 };
    if cursor.take(8)? != TAG {
        return Err(Error::Input);
    }
    let [code] = cursor.number::<1>()?;
    let method = method_of(code)?;
    let body_cap =
        usize::try_from(u32::from_be_bytes(cursor.number()?)).map_err(|_| Error::Input)?;
    let timeout = u64::from_be_bytes(cursor.number()?);
    let url_length =
        usize::try_from(u32::from_be_bytes(cursor.number()?)).map_err(|_| Error::Input)?;
    if url_length > 65536 {
        return Err(Error::Input);
    }
    let url = cursor.text(url_length)?.to_owned();
    let count = usize::from(u16::from_be_bytes(cursor.number()?));
    if count > 100 {
        return Err(Error::Input);
    }
    let mut headers = Vec::new();
    headers.try_reserve_exact(count).map_err(|_| Error::Input)?;
    for _ in 0..count {
        let length = usize::from(u16::from_be_bytes(cursor.number()?));
        if length > 128 {
            return Err(Error::Input);
        }
        let name = cursor.text(length)?.to_owned();
        let length =
            usize::try_from(u32::from_be_bytes(cursor.number()?)).map_err(|_| Error::Input)?;
        if length > 12 * 1024 * 1024 {
            return Err(Error::Input);
        }
        let value = Zeroizing::new(cursor.text(length)?.to_owned());
        headers.push((name, value));
    }
    let length = usize::try_from(u32::from_be_bytes(cursor.number()?)).map_err(|_| Error::Input)?;
    if length > method.max_body() {
        return Err(Error::Input);
    }
    let mut body = Zeroizing::new(Vec::new());
    body.try_reserve_exact(length).map_err(|_| Error::Input)?;
    body.extend_from_slice(cursor.take(length)?);
    if cursor.position != bytes.len() {
        return Err(Error::Input);
    }
    let request = Request {
        method,
        url,
        headers,
        body_cap,
        attempt_timeout: Duration::from_nanos(timeout),
        body,
    };
    valid(&request)?;
    Ok(request)
}
