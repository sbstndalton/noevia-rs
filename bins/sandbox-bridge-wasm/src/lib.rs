//! `sandbox-bridge.wasm` (noevia#999): the WebAssembly face of the `sandbox-bridge` crate, loaded
//! by noevia-core's `code-sandbox/sandbox-bridge-wasm.cjs` under `SANDBOX_BRIDGE_IMPL=rust`.
//! noevia-core pins its sha256 in `code-sandbox/sandbox-bridge.lock`. Separate from
//! `dav-parse.wasm` on purpose: it ships in the code-sandbox image, not the web one.
//!
//! A tiny ABI with no `unsafe` block and no imports (no WASI, no wasm-bindgen glue):
//!
//! 1. `sb_input(len) -> ptr`: sizes a zeroed buffer owned by this module (0 if `len` is over
//!    [`MAX_INPUT_BYTES`]); the host writes `len` bytes there.
//! 2. One call that consumes the buffer and returns a status:
//!    - `sb_frame_new(limit) -> handle`: a new framing state (`lines()`), 1-based; 0 when
//!      [`MAX_FRAMERS`] are live. Does not read the buffer.
//!    - `sb_frame_push(handle)`: input one UTF-8 chunk. Status 0: the reply is the complete JSON
//!      lines as `(u32le(len) utf8)*`; 1: the buffer overflowed, reply `u32le(size)` in UTF-16
//!      code units; 2: bad handle or input.
//!    - `sb_frame_reset(handle) -> units`: drop the buffered text and return its length in UTF-16
//!      code units (`u32::MAX` for a bad handle), for a chunk the host knows overflows by itself.
//!    - `sb_frame_free(handle)`: 0, or 2 for a bad handle.
//!    - `sb_tool_call()`: input `JSON.stringify(payload)`; status 0 reply the tool call JSON, 1
//!      `{"error":"input"|"type_error"|"depth"}`.
//!    - `sb_start()`: input the start line; status 0 reply as in `sandbox_bridge::start`.
//!    - `sb_contained()`: input `u32le(len(root)) root resolved`; status 0 reply
//!      `{"contained":bool,"rel":"…"|null}`.
//!
//!    Status 2 always means the input did not have the expected shape (not UTF-8, short).
//! 3. `sb_output_ptr()` / `sb_output_len()`: where the reply is.
//!
//! The host treats anything but status 0/1 with a well-formed reply as a failure and fails
//! closed. The export attributes are the only `unsafe_code` lint sites; each is allowed
//! individually.
#![deny(unsafe_code)]

use sandbox_bridge::contain::contained;
use sandbox_bridge::frame::{Framer, Pushed};
use sandbox_bridge::json::write_str;
use sandbox_bridge::start::start_json;
use sandbox_bridge::tool_call::tool_call_json;
use std::cell::RefCell;

/// One chunk's cap: 16 MiB UTF-16 code units at up to 3 UTF-8 bytes each, plus slack. The host
/// never sends a chunk longer than the framing limit (it already knows that one overflows).
pub const MAX_INPUT_BYTES: usize = 3 * 16 * 1024 * 1024 + 16;
/// Live framing states (the bridge needs two: the client's stream and pi's).
pub const MAX_FRAMERS: usize = 64;

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static FRAMERS: RefCell<Vec<Option<Framer>>> = const { RefCell::new(Vec::new()) };
}

fn shape() -> (u32, Vec<u8>) {
    (2, b"{\"error\":\"input_shape\"}".to_vec())
}

/// Push one chunk into framer `handle`.
pub fn run_push(handle: u32, input: &[u8]) -> (u32, Vec<u8>) {
    let Ok(chunk) = std::str::from_utf8(input) else {
        return shape();
    };
    FRAMERS.with(|f| {
        let mut f = f.borrow_mut();
        let Some(Some(framer)) = (handle as usize).checked_sub(1).and_then(|i| f.get_mut(i)) else {
            return shape();
        };
        match framer.push(chunk) {
            Pushed::Lines(lines) => {
                let mut out = Vec::new();
                for line in lines {
                    out.extend_from_slice(&(line.len() as u32).to_le_bytes());
                    out.extend_from_slice(line.as_bytes());
                }
                (0, out)
            }
            Pushed::Overflow(size) => (1, (size as u32).to_le_bytes().to_vec()),
        }
    })
}

/// `toolCallFor` on `JSON.stringify(payload)`.
pub fn run_tool_call(input: &[u8]) -> (u32, Vec<u8>) {
    let Ok(text) = std::str::from_utf8(input) else {
        return shape();
    };
    match tool_call_json(text) {
        Ok(json) => (0, json.into_bytes()),
        Err(e) => (1, format!("{{\"error\":\"{}\"}}", e.code()).into_bytes()),
    }
}

/// The supervisor's start-message parse.
pub fn run_start(input: &[u8]) -> (u32, Vec<u8>) {
    match std::str::from_utf8(input) {
        Ok(line) => (0, start_json(line).into_bytes()),
        Err(_) => shape(),
    }
}

/// `insideRoot`'s pure decision on `u32le(len(root)) root resolved`.
pub fn run_contained(input: &[u8]) -> (u32, Vec<u8>) {
    let Some(head) = input.get(..4).and_then(|h| <[u8; 4]>::try_from(h).ok()) else {
        return shape();
    };
    let n = u32::from_le_bytes(head) as usize;
    let (Some(a), Some(b)) = (
        input.get(4..4usize.saturating_add(n)),
        input.get(4usize.saturating_add(n)..),
    ) else {
        return shape();
    };
    let (Ok(root), Ok(resolved)) = (std::str::from_utf8(a), std::str::from_utf8(b)) else {
        return shape();
    };
    let (ok, rel) = contained(root, resolved);
    let mut out = format!("{{\"contained\":{ok},\"rel\":");
    match rel {
        Some(r) => write_str(&r, &mut out),
        None => out.push_str("null"),
    }
    out.push('}');
    (0, out.into_bytes())
}

fn consume(run: impl FnOnce(&[u8]) -> (u32, Vec<u8>)) -> u32 {
    OUTPUT.with(|out| *out.borrow_mut() = Vec::new());
    let (status, reply) = INPUT.with(|buf| run(&buf.borrow()));
    INPUT.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        if buf.capacity() > 1024 * 1024 {
            buf.shrink_to_fit();
        }
    });
    OUTPUT.with(|out| *out.borrow_mut() = reply);
    status
}

/// Prepare an input buffer of `len` zero bytes and return its address (0 if over the cap).
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn sb_input(len: u32) -> u32 {
    let len = len as usize;
    if len > MAX_INPUT_BYTES {
        return 0;
    }
    INPUT.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        buf.resize(len, 0);
        buf.as_mut_ptr() as usize as u32
    })
}

/// A new framing state with `limit` UTF-16 code units; 0 when none is free.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn sb_frame_new(limit: u32) -> u32 {
    FRAMERS.with(|f| {
        let mut f = f.borrow_mut();
        let framer = Some(Framer::new(limit as usize));
        if let Some(i) = f.iter().position(Option::is_none) {
            if let Some(slot) = f.get_mut(i) {
                *slot = framer;
                return i as u32 + 1;
            }
        }
        if f.len() >= MAX_FRAMERS {
            return 0;
        }
        f.push(framer);
        f.len() as u32
    })
}

/// Push the input buffer into framer `handle`; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn sb_frame_push(handle: u32) -> u32 {
    consume(|input| run_push(handle, input))
}

/// Drop framer `handle`'s buffered text; its length in UTF-16 code units (`u32::MAX`: bad handle).
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn sb_frame_reset(handle: u32) -> u32 {
    FRAMERS.with(|f| {
        let mut f = f.borrow_mut();
        match (handle as usize).checked_sub(1).and_then(|i| f.get_mut(i)) {
            Some(Some(framer)) => framer.reset() as u32,
            _ => u32::MAX,
        }
    })
}

/// Free framer `handle`: 0, or 2 for a bad handle.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn sb_frame_free(handle: u32) -> u32 {
    FRAMERS.with(|f| {
        let mut f = f.borrow_mut();
        match (handle as usize).checked_sub(1).and_then(|i| f.get_mut(i)) {
            Some(slot @ Some(_)) => {
                *slot = None;
                0
            }
            _ => 2,
        }
    })
}

/// `toolCallFor` on the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn sb_tool_call() -> u32 {
    consume(run_tool_call)
}

/// Parse the input buffer as the supervisor's start line; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn sb_start() -> u32 {
    consume(run_start)
}

/// `insideRoot`'s pure decision on the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn sb_contained() -> u32 {
    consume(run_contained)
}

/// Address of the last reply.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn sb_output_ptr() -> u32 {
    OUTPUT.with(|out| out.borrow().as_ptr() as usize as u32)
}

/// Length of the last reply.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn sb_output_len() -> u32 {
    OUTPUT.with(|out| out.borrow().len() as u32)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn framer_handles_lines_overflow_and_bad_handles() {
        let h = sb_frame_new(8);
        assert!(h > 0);
        assert_eq!(
            run_push(h, b"[1]\n[2"),
            (0, [3u32.to_le_bytes().as_slice(), b"[1]"].concat())
        );
        assert_eq!(run_push(h, b"123456789"), (1, 11u32.to_le_bytes().to_vec()));
        assert_eq!(sb_frame_reset(h), 0);
        assert_eq!(run_push(h, b"\xff").0, 2);
        assert_eq!(sb_frame_free(h), 0);
        assert_eq!(sb_frame_free(h), 2);
        assert_eq!(run_push(h, b"1\n").0, 2);
        assert_eq!(sb_frame_reset(h), u32::MAX);
        assert_eq!(run_push(0, b"1\n").0, 2);
    }

    #[test]
    fn framer_slots_are_capped_and_reused() {
        let handles: Vec<u32> = (0..MAX_FRAMERS).map(|_| sb_frame_new(1)).collect();
        assert!(handles.iter().all(|h| *h > 0));
        assert_eq!(sb_frame_new(1), 0);
        assert_eq!(sb_frame_free(handles[3]), 0);
        assert_eq!(sb_frame_new(1), handles[3]);
    }

    #[test]
    fn contained_and_shapes() {
        let mut input = 2u32.to_le_bytes().to_vec();
        input.extend_from_slice(b"/w/w/a");
        assert_eq!(
            run_contained(&input),
            (0, b"{\"contained\":true,\"rel\":\"a\"}".to_vec())
        );
        assert_eq!(run_contained(b"\x09\0\0\0/w").0, 2);
        assert_eq!(run_tool_call(b"[]").0, 1);
        assert_eq!(
            run_tool_call(b"{\"toolName\":{\"toString\":1}}"),
            (1, b"{\"error\":\"type_error\"}".to_vec())
        );
        assert_eq!(run_start(b"x"), (0, b"{\"start\":false}".to_vec()));
        assert_eq!(sb_input((MAX_INPUT_BYTES + 1) as u32), 0);
    }
}
