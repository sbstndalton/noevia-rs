//! `dav-parse.wasm`: the WebAssembly face of noevia-rs's storage parsers: `dav-parse` (noevia#967),
//! `s3-list-parse` (noevia#976) and `storage-path` (noevia#978). One module, one pin: noevia-core's
//! `server/dav-parse.lock` names its sha256 and every switch (`DAV_PARSE_IMPL`, `S3_PARSE_IMPL`,
//! `STORAGE_PATH_IMPL`) loads the same bytes.
//!
//! A deliberately tiny ABI with no `unsafe` block and no imports (no WASI, no wasm-bindgen glue):
//!
//! 1. `dav_input(len) -> ptr`: sizes a zeroed buffer owned by this module and returns its address.
//!    The host writes `len` UTF-8 bytes there (the shape depends on the call below).
//! 2. One call that consumes the buffer and returns a status: 0 = the reply is the result,
//!    1 = the reply is `{"error":"code"}`, 2 = the input did not have the expected shape.
//!    - `dav_list()`: input `target NUL body`; reply `{"entries":[…]}` (dav-parse).
//!    - `s3_list()`: input `u32le(len(prefix)) prefix body`; reply
//!      `{"entries":[…],"truncated":bool,"next":"…"|null}` (s3-list-parse).
//!    - `storage_path(op)`: input `u32le(len(a)) a b`; op 1 safeRelativePath(a), 2 cleanRoot(a),
//!      3 joinRoot(a, b), 4 the upload filename rule on a; reply `{"value":…}` (storage-path).
//! 3. `dav_output_ptr()` / `dav_output_len()`: where the UTF-8 JSON reply is.
//!
//! The host must treat anything other than status 0 with a well-formed reply as a refusal; noevia
//! core's loader fails closed. Buffers live in this module's own linear memory; the host only
//! writes into the input buffer between `dav_input` and `dav_list`, while no Rust reference to it
//! is live. The export attributes are the only `unsafe_code` lint sites (rustc counts
//! `#[no_mangle]`); each is allowed individually.
#![deny(unsafe_code)]

use std::cell::RefCell;

/// Input cap: URL + NUL + body, so a host cannot make this module grow without bound.
pub const MAX_INPUT_BYTES: usize = dav_parse::MAX_BODY_BYTES + dav_parse::MAX_TARGET_BYTES + 1;

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Parse `target NUL body`; the status and JSON reply as described in the crate docs.
pub fn run(input: &[u8]) -> (u32, String) {
    let Ok(text) = std::str::from_utf8(input) else {
        return (2, "{\"error\":\"input_not_utf8\"}".to_owned());
    };
    let Some((target, body)) = text.split_once('\0') else {
        return (2, "{\"error\":\"input_shape\"}".to_owned());
    };
    let result = dav_parse::list_entries(body, target);
    let status = u32::from(result.is_err());
    (status, dav_parse::reply_json(&result))
}

/// Split `u32le(len(a)) a b` into `(a, b)`, both UTF-8.
fn framed(input: &[u8]) -> Option<(&str, &str)> {
    let head: [u8; 4] = input.get(..4)?.try_into().ok()?;
    let n = u32::from_le_bytes(head) as usize;
    let a = input.get(4..4usize.checked_add(n)?)?;
    let b = input.get(4 + n..)?;
    Some((std::str::from_utf8(a).ok()?, std::str::from_utf8(b).ok()?))
}

/// Parse `u32le(len(prefix)) prefix body` as one S3 ListObjectsV2 page.
pub fn run_s3(input: &[u8]) -> (u32, String) {
    let Some((prefix, body)) = framed(input) else {
        return (2, "{\"error\":\"input_shape\"}".to_owned());
    };
    let result = s3_list_parse::parse_page(body, prefix);
    (
        u32::from(result.is_err()),
        s3_list_parse::reply_json(&result),
    )
}

/// Run storage-path rule `op` on `u32le(len(a)) a b`.
pub fn run_path(op: u32, input: &[u8]) -> (u32, String) {
    let Some(op) = storage_path::Op::from_u32(op) else {
        return (2, "{\"error\":\"unknown_op\"}".to_owned());
    };
    let Some((a, b)) = framed(input) else {
        return (2, "{\"error\":\"input_shape\"}".to_owned());
    };
    let (ok, reply) = storage_path::reply_json(op, a, b);
    (u32::from(!ok), reply)
}

fn consume(run: impl FnOnce(&[u8]) -> (u32, String)) -> u32 {
    let (status, reply) = INPUT.with(|buf| run(&buf.borrow()));
    INPUT.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        buf.shrink_to_fit();
    });
    OUTPUT.with(|out| *out.borrow_mut() = reply.into_bytes());
    status
}

/// Prepare an input buffer of `len` zero bytes and return its address (0 if `len` is over the cap).
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn dav_input(len: u32) -> u32 {
    let len = len as usize;
    if len > MAX_INPUT_BYTES {
        return 0;
    }
    INPUT.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        buf.shrink_to(len);
        buf.resize(len, 0);
        buf.as_mut_ptr() as usize as u32
    })
}

/// Parse the input buffer; see the crate docs for the status codes.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn dav_list() -> u32 {
    consume(run)
}

/// Parse the input buffer as one S3 ListObjectsV2 page; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn s3_list() -> u32 {
    consume(run_s3)
}

/// Run storage-path rule `op` on the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn storage_path(op: u32) -> u32 {
    consume(|input| run_path(op, input))
}

/// Address of the last reply.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn dav_output_ptr() -> u32 {
    OUTPUT.with(|out| out.borrow().as_ptr() as usize as u32)
}

/// Length in bytes of the last reply.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn dav_output_len() -> u32 {
    OUTPUT.with(|out| out.borrow().len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_shapes() {
        let (s, r) = run(b"https://h/d/\0<d:response><d:href>/d/a</d:href></d:response>");
        assert_eq!(s, 0);
        assert_eq!(r, r#"{"entries":[{"name":"a","isDir":false,"size":null}]}"#);
        assert_eq!(run(b"no separator").0, 2);
        assert_eq!(run(b"\xff\0x").0, 2);
        let (s, r) = run(b"not a url\0");
        assert_eq!((s, r.as_str()), (1, r#"{"error":"invalid_target"}"#));
    }

    fn frame(a: &str, b: &str) -> Vec<u8> {
        let mut v = (a.len() as u32).to_le_bytes().to_vec();
        v.extend_from_slice(a.as_bytes());
        v.extend_from_slice(b.as_bytes());
        v
    }

    #[test]
    fn s3_and_path_shapes() {
        let (s, r) = run_s3(&frame(
            "p/",
            "<Contents><Key>p/a</Key><Size>1</Size></Contents>",
        ));
        assert_eq!(s, 0);
        assert_eq!(
            r,
            r#"{"entries":[{"name":"a","isDir":false,"size":"1"}],"truncated":false,"next":null}"#
        );
        assert_eq!(run_s3(b"\x05\0\0\0ab").0, 2);
        assert_eq!(run_s3(b"\x01\0").0, 2);
        assert_eq!(run_s3(&[1, 0, 0, 0, 0xff]).0, 2);
        assert_eq!(
            run_path(1, &frame(" a\\..\\b", "")),
            (0, r#"{"value":""}"#.to_owned())
        );
        assert_eq!(
            run_path(3, &frame("/root/", "a/b")),
            (0, r#"{"value":"root/a/b"}"#.to_owned())
        );
        assert_eq!(
            run_path(4, &frame("..", "")),
            (0, r#"{"value":false}"#.to_owned())
        );
        assert_eq!(run_path(9, &frame("a", "")).0, 2);
        let big = "a".repeat(storage_path::MAX_INPUT_BYTES + 1);
        assert_eq!(
            run_path(2, &frame(&big, "")),
            (1, r#"{"error":"too_large"}"#.to_owned())
        );
    }

    #[test]
    fn buffers_round_trip() {
        assert_eq!(dav_input(u32::MAX), 0);
        assert_ne!(dav_input(3), 0);
        assert_eq!(dav_list(), 1); // NUL bytes: an empty target, refused
        assert!(dav_output_len() > 0);
        assert_ne!(dav_output_ptr(), 0);
    }
}
