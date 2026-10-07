//! `dav-parse.wasm`: the WebAssembly face of the `dav-parse` crate (noevia#967).
//!
//! A deliberately tiny ABI with no `unsafe` block and no imports (no WASI, no wasm-bindgen glue):
//!
//! 1. `dav_input(len) -> ptr`: sizes a zeroed buffer owned by this module and returns its address.
//!    The host writes `len` bytes there: the request URL, one NUL byte, then the body (UTF-8).
//! 2. `dav_list() -> status`: parses the buffer. 0 = the reply is `{"entries":[…]}`,
//!    1 = the reply is `{"error":"code"}`, 2 = the input was not `target NUL body` in UTF-8.
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
    let (status, reply) = INPUT.with(|buf| run(&buf.borrow()));
    INPUT.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        buf.shrink_to_fit();
    });
    OUTPUT.with(|out| *out.borrow_mut() = reply.into_bytes());
    status
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

    #[test]
    fn buffers_round_trip() {
        assert_eq!(dav_input(u32::MAX), 0);
        assert_ne!(dav_input(3), 0);
        assert_eq!(dav_list(), 1); // NUL bytes: an empty target, refused
        assert!(dav_output_len() > 0);
        assert_ne!(dav_output_ptr(), 0);
    }
}
