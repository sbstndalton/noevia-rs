//! `dav-parse.wasm`: the WebAssembly face of noevia-rs's storage parsers: `dav-parse` (noevia#967),
//! `s3-list-parse` (noevia#976), `storage-path` (noevia#978), `upload-sniff` (noevia#977) and
//! `secret-envelope` (noevia#979). One module, one pin: noevia-core's `server/dav-parse.lock` names
//! its sha256 and every switch (`DAV_PARSE_IMPL`, `S3_PARSE_IMPL`, `STORAGE_PATH_IMPL`,
//! `UPLOAD_SNIFF_IMPL`, `SECRET_ENVELOPE_IMPL`) loads the same bytes.
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
//!    - `upload_validate()`: input `u32le(len) u32le(len(name)) name head`, where `len` is the
//!      upload's length (a host clamps it to `CAP + 1`) and `head` its first bytes (at most
//!      `upload_sniff::SNIFF_BYTES` matter); reply `{"value":null|{"refusal":"…","status":N}}`.
//!    - `upload_classify()`: input the UTF-8 name; reply `{"value":"Group"}`.
//!    - `upload_decode()`: input the upload's bytes (at most `upload_sniff::MAX_DECODE_BYTES`);
//!      on status 0 the reply is NOT JSON but one tag byte (0 = not text, 1 utf-8, 2 utf-16le,
//!      3 utf-16be, 4 windows-1252) followed by the decoded text in UTF-8.
//!    - `secret_open()`: input `n(1|2) key*n user value`, where each key is 32 bytes, `user` is
//!      `0` (no user) or `1 u32le(len) utf8`, and `value` is the stored text as UTF-16LE units; on
//!      status 0 the reply is NOT JSON but one byte (0 = not an envelope, 1 = opened with the
//!      first key, 2 = with the second) followed by the plaintext bytes (none for 0).
//!    - `secret_seal()`: input `key(32) nonce(12) user plaintext`, `user` as above; reply the
//!      ASCII envelope `enc:v1:…` (no user) or `enc:v2:…`. The nonce comes from the host's CSPRNG.
//!
//!    - `mcp_rpc_body()`: input `sse(0|1) id text`, where `id` is `0` (matches nothing), `1` null,
//!      `2` false, `3` true, `4 f64le` a number or `5 u32le(n) units` a string, and `text` the body
//!      as UTF-16LE units (at most `mcp_frame::MAX_BODY_UNITS`); on status 0 the reply is NOT
//!      JSON but a tag (0 reply, 1 id mismatch, 2 no message, 3 other traffic, 4 invalid JSON)
//!      followed, for 0 and 1, by the message as UTF-8 JSON text (mcp-frame, noevia#980).
//!    - `mcp_schema_refs()`: input the schema (mcp-frame's wire form) as UTF-16LE units (at most
//!      `mcp_frame::schema::MAX_SCHEMA_UNITS`); on status 0 the reply is a tag (0 the resolved
//!      tree, 1 `{"code":…,"ref":…}`, what the JS throws) followed by UTF-8 JSON.
//!
//!    - `template_caps()`: input a model's chat template as UTF-8 (at most
//!      `chat_template_caps::MAX_TEMPLATE_BYTES`); reply `{"known":…,"tools":…,"toolCalls":…,
//!      "toolRole":…,"systemRole":…,"strictAlternation":…,"raises":…,"thinking":…,
//!      "sendTools":…}` (chat-template-caps, noevia#1002); a longer one is `{"error":"too_large"}`.
//!    - `provider_error()`: input `u32le(status) body`, body UTF-8 (status 0: no response);
//!      reply `{"kind":"…","reason":"…"}` (provider-error, noevia#1002).
//!    - `serving_verdict()`: same input, autotune's serving-check reply; reply
//!      `{"passed":bool,"kind":"…"|null,"reason":"…"}` (chat-template-caps, noevia#1003).
//!    - `autotune_plan()`: input auto-tune's planner request as UTF-8 JSON (at most
//!      `autotune_plan::MAX_INPUT_BYTES`): the model's facts, the memory budget, the context
//!      ladder, the allowed KV cache types and every result so far; reply the next step, e.g.
//!      `{"step":"probe","ctx":…,"kv":"…","fill":…,"estimateMib":…}` (autotune-plan,
//!      noevia#1003). A refused request is status 1 with `{"error":"input"|"too_large"}`.
//!    - `preset_reload()`: input `{"baseline":"…","current":"…","loaded":["…"]}` as UTF-8 JSON (at
//!      most `preset_reload::MAX_INPUT_BYTES`): the models.ini text the llama.cpp router last
//!      read, the text now and the models it has loaded; reply
//!      `{"safe":bool,"reason":"unchanged"|"changed"|"ambiguous","changed":[…],"detail":…}`
//!      (preset-reload, noevia#1012). A refused request is status 1 with
//!      `{"error":"input"|"too_large"}`.
//!    - `load_verdict()`: input `{"cause":"…","evidence":{"status":…,"exitCode":…,"text":"…"}|null,
//!      "advice":{"label":"…","confidence":…}|null}` as UTF-8 JSON (at most
//!      `load_verdict::MAX_INPUT_BYTES`): a failed auto-tune step's cause, the engine's evidence
//!      and the decision service's advisory label; reply `{"outcome":"…","source":"measured"|
//!      "rule"|"advisor"|"fallback","rule":"…","ruleId":…,"ask":bool,"advice":…,
//!      "adviceUsed":bool,"reason":"…"}` (load-verdict, noevia#1004). A refused request is
//!      status 1 with `{"error":"input"|"too_large"}`.
//!    - `tune_contention()`: input `{"tuning":"…","rows":[{"id":"…","status":"…","busy":…}],
//!      "prev":{"fingerprint":"…","since":…}|null,"startedAt":…,"now":…,"maxWaitMs":…,
//!      "quietMs":…}` as UTF-8 JSON (at most `tune_contention::MAX_INPUT_BYTES`): the router's rows
//!      while auto-tune runs and the previous reply's fingerprint; reply `{"action":"proceed"|
//!      "wait"|"unload"|"give_up","reason":"…","foreign":[…],"unload":[…],"fingerprint":"…",
//!      "since":…,"waitedMs":…}` (tune-contention, noevia#1062). A refused request is status 1
//!      with `{"error":"input"|"too_large"}`.
//!    - `long_profile()`: input `{"op":"pairs"|"section"|"pick",…}` as UTF-8 JSON (at most
//!      `long_profile::MAX_INPUT_BYTES`): which router rows are a model's `<id>-long` profile, the
//!      models.ini text with that section appended for a Long tune, or which entry serves a chat
//!      with Context Low or High; reply `{"pairs":[…]}`, `{"ok":true,"id":…,"text":…}` /
//!      `{"ok":false,"reason":…}` or `{"model":…,"long":bool,"reason":…}` (long-profile,
//!      noevia#1079). A refused request is status 1 with `{"error":"input"|"too_large"}`.
//!
//!    Both secret calls wipe their input buffer (keys, user, value) and the previous reply before
//!    returning; refusals are `{"error":"bound"|"unopenable"|"too_large"|"input"}` and never carry
//!    input bytes. The host still wipes the whole linear memory and drops the instance after each
//!    secret call (the reply holds plaintext until then).
//! 3. `dav_output_ptr()` / `dav_output_len()`: where the UTF-8 JSON reply is.
//!
//! The host must treat anything other than status 0 with a well-formed reply as a refusal; noevia
//! core's loader fails closed. Buffers live in this module's own linear memory; the host only
//! writes into the input buffer between `dav_input` and `dav_list`, while no Rust reference to it
//! is live. The export attributes are the only `unsafe_code` lint sites (rustc counts
//! `#[no_mangle]`); each is allowed individually.
#![deny(unsafe_code)]

use secret_envelope::zeroize::{Zeroize, Zeroizing};
use std::cell::RefCell;

/// Input cap for any call, so a host cannot make this module grow without bound: the larger of a
/// DAV listing (URL + NUL + body) and an upload to decode. Every call also enforces its own cap.
pub const MAX_INPUT_BYTES: usize = {
    let dav = dav_parse::MAX_BODY_BYTES + dav_parse::MAX_TARGET_BYTES + 1;
    if dav > upload_sniff::MAX_DECODE_BYTES {
        dav
    } else {
        upload_sniff::MAX_DECODE_BYTES
    }
};

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

/// `upload-sniff` validate on `u32le(len) u32le(len(name)) name head`.
pub fn run_validate(input: &[u8]) -> (u32, String) {
    let Some(len) = input.get(..4).and_then(|h| <[u8; 4]>::try_from(h).ok()) else {
        return (2, "{\"error\":\"input_shape\"}".to_owned());
    };
    let Some(rest) = input.get(4..) else {
        return (2, "{\"error\":\"input_shape\"}".to_owned());
    };
    let Some(head) = rest.get(..4).and_then(|h| <[u8; 4]>::try_from(h).ok()) else {
        return (2, "{\"error\":\"input_shape\"}".to_owned());
    };
    let n = u32::from_le_bytes(head) as usize;
    let (Some(name), Some(bytes)) = (
        rest.get(4..4usize.saturating_add(n)),
        rest.get(4usize.saturating_add(n)..),
    ) else {
        return (2, "{\"error\":\"input_shape\"}".to_owned());
    };
    let Ok(name) = std::str::from_utf8(name) else {
        return (2, "{\"error\":\"input_not_utf8\"}".to_owned());
    };
    (
        0,
        upload_sniff::validate_json(name, u64::from(u32::from_le_bytes(len)), bytes),
    )
}

/// `upload-sniff` classify on a UTF-8 name.
pub fn run_classify(input: &[u8]) -> (u32, String) {
    match std::str::from_utf8(input) {
        Ok(name) => (0, upload_sniff::classify_json(name)),
        Err(_) => (2, "{\"error\":\"input_not_utf8\"}".to_owned()),
    }
}

/// `upload-sniff` decodeText; the raw reply described in the crate docs.
pub fn run_decode(input: &[u8]) -> (u32, Vec<u8>) {
    match upload_sniff::decode_reply(input) {
        Ok(reply) => (0, reply),
        Err(e) => (1, upload_sniff::error_json(&e).into_bytes()),
    }
}

/// Split the `user` field: `0` or `1 u32le(len) utf8`; returns the user and the rest.
fn secret_user(input: &[u8]) -> Option<(Option<&[u8]>, &[u8])> {
    match input.first()? {
        0 => Some((None, input.get(1..)?)),
        1 => {
            let n = u32::from_le_bytes(input.get(1..5)?.try_into().ok()?) as usize;
            let end = 5usize.checked_add(n)?;
            Some((Some(input.get(5..end)?), input.get(end..)?))
        }
        _ => None,
    }
}

const SECRET_SHAPE: &str = "{\"error\":\"input\"}";

fn secret_error(e: secret_envelope::Error) -> (u32, Vec<u8>) {
    (1, format!("{{\"error\":\"{}\"}}", e.code()).into_bytes())
}

/// `secret-envelope` open; the raw reply described in the crate docs.
pub fn run_secret_open(input: &[u8]) -> (u32, Vec<u8>) {
    let shape = || (2, SECRET_SHAPE.as_bytes().to_vec());
    let Some(&n) = input.first() else {
        return shape();
    };
    let keys_end = 1 + usize::from(n) * secret_envelope::KEY_BYTES;
    let (Some(keys), Some(rest)) = (input.get(1..keys_end), input.get(keys_end..)) else {
        return shape();
    };
    if !(n == 1 || n == 2) {
        return shape();
    }
    let (current, previous) = keys.split_at(secret_envelope::KEY_BYTES);
    let previous = (n == 2).then_some(previous);
    let Some((user, value)) = secret_user(rest) else {
        return shape();
    };
    if value.len() / 2 > secret_envelope::MAX_ENVELOPE_UNITS {
        return secret_error(secret_envelope::Error::TooLarge);
    }
    let Some(units) = secret_envelope::units_from_le(value).map(Zeroizing::new) else {
        return shape();
    };
    match secret_envelope::open(current, previous, &units, user) {
        Ok((used, plain)) => {
            let mut reply = Vec::with_capacity(1 + plain.len());
            reply.push(used.tag());
            reply.extend_from_slice(&plain);
            (0, reply)
        }
        Err(e) => secret_error(e),
    }
}

/// `secret-envelope` seal; the ASCII envelope described in the crate docs.
pub fn run_secret_seal(input: &[u8]) -> (u32, Vec<u8>) {
    let shape = || (2, SECRET_SHAPE.as_bytes().to_vec());
    let k = secret_envelope::KEY_BYTES;
    let nonce_end = k + secret_envelope::NONCE_BYTES;
    let (Some(key), Some(nonce), Some(rest)) = (
        input.get(..k),
        input.get(k..nonce_end),
        input.get(nonce_end..),
    ) else {
        return shape();
    };
    let Some((user, plain)) = secret_user(rest) else {
        return shape();
    };
    match secret_envelope::seal(key, nonce, plain, user) {
        Ok(text) => (0, text.into_bytes()),
        Err(e) => secret_error(e),
    }
}

fn units(b: &[u8]) -> Option<Vec<u16>> {
    let (pairs, rest) = b.as_chunks::<2>();
    rest.is_empty()
        .then(|| pairs.iter().map(|&p| u16::from_le_bytes(p)).collect())
}

const SHAPE: &str = "{\"error\":\"input_shape\"}";
const TOO_LARGE: &str = "{\"error\":\"too_large\"}";

/// `mcp-frame` parseRpcBody; the raw reply described in the crate docs.
pub fn run_mcp_rpc(input: &[u8]) -> (u32, Vec<u8>) {
    let shape = || (2, SHAPE.as_bytes().to_vec());
    let (Some(&sse), Some(&kind)) = (input.first(), input.get(1)) else {
        return shape();
    };
    let (expected, rest) = match kind {
        0 => (mcp_frame::Expected::Never, input.get(2..)),
        1 => (mcp_frame::Expected::Null, input.get(2..)),
        2 => (mcp_frame::Expected::Bool(false), input.get(2..)),
        3 => (mcp_frame::Expected::Bool(true), input.get(2..)),
        4 => match input.get(2..10).and_then(|b| <[u8; 8]>::try_from(b).ok()) {
            Some(b) => (
                mcp_frame::Expected::Number(f64::from_le_bytes(b)),
                input.get(10..),
            ),
            None => return shape(),
        },
        5 => {
            let Some(n) = input.get(2..6).and_then(|b| <[u8; 4]>::try_from(b).ok()) else {
                return shape();
            };
            let end = (u32::from_le_bytes(n) as usize)
                .saturating_mul(2)
                .saturating_add(6);
            match input.get(6..end).and_then(units) {
                Some(u) => (mcp_frame::Expected::String(u), input.get(end..)),
                None => return shape(),
            }
        }
        _ => return shape(),
    };
    if sse > 1 {
        return shape();
    }
    let Some(rest) = rest else { return shape() };
    if rest.len() / 2 > mcp_frame::MAX_BODY_UNITS {
        return (1, TOO_LARGE.as_bytes().to_vec());
    }
    let Some(text) = units(rest) else {
        return shape();
    };
    match mcp_frame::parse_rpc_body(sse == 1, &text, &expected) {
        Ok(o) => (0, mcp_frame::rpc_reply(&text, &o)),
        Err(mcp_frame::TooLarge) => (1, TOO_LARGE.as_bytes().to_vec()),
    }
}

/// `mcp-frame` resolveSchemaRefs; the raw reply described in the crate docs.
pub fn run_mcp_schema(input: &[u8]) -> (u32, Vec<u8>) {
    if input.len() / 2 > mcp_frame::schema::MAX_SCHEMA_UNITS {
        return (1, TOO_LARGE.as_bytes().to_vec());
    }
    let Some(text) = units(input) else {
        return (2, SHAPE.as_bytes().to_vec());
    };
    match mcp_frame::schema::resolve_schema_refs(&text) {
        Ok(Ok(tree)) => {
            let mut r = vec![0];
            r.extend_from_slice(&tree);
            (0, r)
        }
        Ok(Err(e)) => {
            let mut r = vec![1];
            r.extend_from_slice(&e.json());
            (0, r)
        }
        Err(mcp_frame::schema::InputError::TooLarge) => (1, TOO_LARGE.as_bytes().to_vec()),
        Err(mcp_frame::schema::InputError::NotJson) => (2, SHAPE.as_bytes().to_vec()),
    }
}

/// `chat-template-caps` analyze on a UTF-8 template (noevia#1002).
pub fn run_template_caps(input: &[u8]) -> (u32, String) {
    let Ok(text) = std::str::from_utf8(input) else {
        return (2, "{\"error\":\"input_not_utf8\"}".to_owned());
    };
    let result = chat_template_caps::analyze(text);
    (
        u32::from(result.is_err()),
        chat_template_caps::reply_json(&result),
    )
}

/// Split `u32le(status) body` (body UTF-8).
fn status_body(input: &[u8]) -> Option<(u32, &str)> {
    let head: [u8; 4] = input.get(..4)?.try_into().ok()?;
    Some((
        u32::from_le_bytes(head),
        std::str::from_utf8(input.get(4..)?).ok()?,
    ))
}

/// `provider-error` classify on `u32le(status) body` (noevia#1002).
pub fn run_provider_error(input: &[u8]) -> (u32, String) {
    let Some((status, body)) = status_body(input) else {
        return (2, "{\"error\":\"input_shape\"}".to_owned());
    };
    (
        0,
        provider_error::reply_json(&provider_error::classify(status, body)),
    )
}

/// Autotune's serving verdict on `u32le(status) body` (noevia#1003).
pub fn run_serving_verdict(input: &[u8]) -> (u32, String) {
    let Some((status, body)) = status_body(input) else {
        return (2, "{\"error\":\"input_shape\"}".to_owned());
    };
    (
        0,
        chat_template_caps::verdict_json(&chat_template_caps::serving_verdict(status, body)),
    )
}

/// Auto-tune's next step for a UTF-8 JSON request (noevia#1003).
pub fn run_autotune_plan(input: &[u8]) -> (u32, String) {
    if input.len() > autotune_plan::MAX_INPUT_BYTES {
        return (1, "{\"error\":\"too_large\"}".to_owned());
    }
    let Ok(text) = std::str::from_utf8(input) else {
        return (2, "{\"error\":\"input_not_utf8\"}".to_owned());
    };
    autotune_plan::plan_json(text)
}

/// Decide whether a router preset reload keeps every loaded model; see the crate docs.
pub fn run_preset_reload(input: &[u8]) -> (u32, String) {
    if input.len() > preset_reload::MAX_INPUT_BYTES {
        return (1, "{\"error\":\"too_large\"}".to_owned());
    }
    let Ok(text) = std::str::from_utf8(input) else {
        return (2, "{\"error\":\"input_not_utf8\"}".to_owned());
    };
    preset_reload::check_json(text)
}

/// `load-verdict`: why a failed auto-tune step failed; the JSON reply described in the crate docs.
pub fn run_load_verdict(input: &[u8]) -> (u32, String) {
    if input.len() > load_verdict::MAX_INPUT_BYTES {
        return (1, "{\"error\":\"too_large\"}".to_owned());
    }
    let Ok(text) = std::str::from_utf8(input) else {
        return (2, "{\"error\":\"input_not_utf8\"}".to_owned());
    };
    load_verdict::verdict_json(text)
}

/// `tune-contention`: may auto-tune go on while another client uses the router; see the crate docs.
pub fn run_tune_contention(input: &[u8]) -> (u32, String) {
    if input.len() > tune_contention::MAX_INPUT_BYTES {
        return (1, "{\"error\":\"too_large\"}".to_owned());
    }
    let Ok(text) = std::str::from_utf8(input) else {
        return (2, "{\"error\":\"input_not_utf8\"}".to_owned());
    };
    tune_contention::decide_json(text)
}

/// `long-profile`: low- and high-context profiles per model; see the crate docs.
pub fn run_long_profile(input: &[u8]) -> (u32, String) {
    if input.len() > long_profile::MAX_INPUT_BYTES {
        return (1, "{\"error\":\"too_large\"}".to_owned());
    }
    let Ok(text) = std::str::from_utf8(input) else {
        return (2, "{\"error\":\"input_not_utf8\"}".to_owned());
    };
    long_profile::run_json(text)
}

/// Run a secret call: wipe the previous reply first and the input (keys, user, value) after.
fn consume_secret(run: impl FnOnce(&[u8]) -> (u32, Vec<u8>)) -> u32 {
    OUTPUT.with(|out| out.borrow_mut().zeroize());
    let (status, reply) = INPUT.with(|buf| run(&buf.borrow()));
    INPUT.with(|buf| {
        let mut buf = buf.borrow_mut();
        // Vec::zeroize wipes the whole capacity, then clears.
        buf.zeroize();
        buf.shrink_to_fit();
    });
    OUTPUT.with(|out| *out.borrow_mut() = reply);
    status
}

fn consume(run: impl FnOnce(&[u8]) -> (u32, String)) -> u32 {
    consume_bytes(|input| {
        let (status, reply) = run(input);
        (status, reply.into_bytes())
    })
}

fn consume_bytes(run: impl FnOnce(&[u8]) -> (u32, Vec<u8>)) -> u32 {
    // Drop the previous reply first, so a large one is not held while this call runs.
    OUTPUT.with(|out| *out.borrow_mut() = Vec::new());
    let (status, reply) = INPUT.with(|buf| run(&buf.borrow()));
    INPUT.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        buf.shrink_to_fit();
    });
    OUTPUT.with(|out| *out.borrow_mut() = reply);
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

/// Run upload validate on the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn upload_validate() -> u32 {
    consume(run_validate)
}

/// Run upload classify on the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn upload_classify() -> u32 {
    consume(run_classify)
}

/// Run upload decodeText on the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn upload_decode() -> u32 {
    consume_bytes(run_decode)
}

/// Open a stored credential from the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn secret_open() -> u32 {
    consume_secret(run_secret_open)
}

/// Seal a credential from the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn secret_seal() -> u32 {
    consume_secret(run_secret_seal)
}

/// Parse an MCP response body from the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn mcp_rpc_body() -> u32 {
    consume_bytes(run_mcp_rpc)
}

/// Inline a tool schema's local refs from the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn mcp_schema_refs() -> u32 {
    consume_bytes(run_mcp_schema)
}

/// Analyse a chat template from the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn template_caps() -> u32 {
    consume(run_template_caps)
}

/// Classify an upstream provider error from the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn provider_error() -> u32 {
    consume(run_provider_error)
}

/// Judge autotune's serving-check reply from the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn serving_verdict() -> u32 {
    consume(run_serving_verdict)
}

/// Plan auto-tune's next step from the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn autotune_plan() -> u32 {
    consume(run_autotune_plan)
}

/// Check a router preset reload from the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn preset_reload() -> u32 {
    consume(run_preset_reload)
}

/// Name a failed auto-tune step's outcome from the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn load_verdict() -> u32 {
    consume(run_load_verdict)
}

/// Decide auto-tune's next move against another router client from the input buffer.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn tune_contention() -> u32 {
    consume(run_tune_contention)
}

/// Pair, derive or pick a long-context profile from the input buffer; see the crate docs.
#[allow(unsafe_code)]
#[no_mangle]
pub extern "C" fn long_profile() -> u32 {
    consume(run_long_profile)
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
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
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
    fn upload_shapes() {
        let mut v = 300u32.to_le_bytes().to_vec();
        v.extend_from_slice(&5u32.to_le_bytes());
        v.extend_from_slice(b"a.zip");
        assert_eq!(
            run_validate(&v),
            (
                0,
                r#"{"value":{"refusal":"archive","status":400}}"#.to_owned()
            )
        );
        let mut v = 3u32.to_le_bytes().to_vec();
        v.extend_from_slice(&5u32.to_le_bytes());
        v.extend_from_slice(b"a.txtabc");
        assert_eq!(run_validate(&v), (0, r#"{"value":null}"#.to_owned()));
        assert_eq!(run_validate(&[1, 0, 0, 0, 9, 0, 0, 0, b'a']).0, 2);
        assert_eq!(run_validate(&[1, 0]).0, 2);
        assert_eq!(run_classify(b"x.MD"), (0, r#"{"value":"Text"}"#.to_owned()));
        assert_eq!(run_classify(b"\xff").0, 2);
        assert_eq!(
            run_decode(b"\x80"),
            (0, "\u{4}\u{20ac}".as_bytes().to_vec())
        );
        assert_eq!(run_decode(b"a\0"), (0, vec![0]));
        let big = vec![b'a'; upload_sniff::MAX_DECODE_BYTES + 1];
        assert_eq!(run_decode(&big).0, 1);
    }

    fn secret_input(keys: &[[u8; 32]], user: Option<&[u8]>, value: &str) -> Vec<u8> {
        let mut v = vec![keys.len() as u8];
        for k in keys {
            v.extend_from_slice(k);
        }
        push_user(&mut v, user);
        for u in value.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    fn push_user(v: &mut Vec<u8>, user: Option<&[u8]>) {
        match user {
            None => v.push(0),
            Some(u) => {
                v.push(1);
                v.extend_from_slice(&(u.len() as u32).to_le_bytes());
                v.extend_from_slice(u);
            }
        }
    }

    #[test]
    fn secret_shapes() {
        let (k, other) = ([3u8; 32], [4u8; 32]);
        let mut seal_in = k.to_vec();
        seal_in.extend_from_slice(&[9; 12]);
        push_user(&mut seal_in, Some(b"u1"));
        seal_in.extend_from_slice("caf\u{e9}".as_bytes());
        let (s, env) = run_secret_seal(&seal_in);
        assert_eq!(s, 0);
        let env = String::from_utf8(env).unwrap();
        assert!(env.starts_with("enc:v2:"));
        let (s, r) = run_secret_open(&secret_input(&[other, k], Some(b"u1"), &env));
        assert_eq!((s, r.as_slice()), (0, &b"\x02caf\xc3\xa9"[..]));
        assert_eq!(
            run_secret_open(&secret_input(&[k], Some(b"u2"), &env)),
            (1, br#"{"error":"unopenable"}"#.to_vec())
        );
        assert_eq!(
            run_secret_open(&secret_input(&[k], None, &env)),
            (1, br#"{"error":"bound"}"#.to_vec())
        );
        assert_eq!(
            run_secret_open(&secret_input(&[k], None, "plain")),
            (0, vec![0])
        );
        assert_eq!(run_secret_open(&[]).0, 2);
        assert_eq!(run_secret_open(&[3]).0, 2);
        assert_eq!(run_secret_open(&secret_input(&[k], None, "a")[..35]).0, 2);
        let mut odd = secret_input(&[k], None, "a");
        odd.push(0);
        assert_eq!(run_secret_open(&odd).0, 2);
        let mut bad_user = k.to_vec();
        bad_user.extend_from_slice(&[9; 12]);
        bad_user.extend_from_slice(&[1, 9, 0, 0, 0, b'a']);
        assert_eq!(run_secret_seal(&bad_user).0, 2);
        assert_eq!(run_secret_seal(&k[..20]).0, 2);
    }

    #[test]
    fn secret_calls_wipe_the_input() {
        let k = [0x5au8; 32];
        let mut input = k.to_vec();
        input.extend_from_slice(&[1; 12]);
        input.push(0);
        input.extend_from_slice(b"synthetic plaintext");
        let ptr = dav_input(input.len() as u32);
        assert_ne!(ptr, 0);
        INPUT.with(|b| b.borrow_mut().copy_from_slice(&input));
        assert_eq!(secret_seal(), 0);
        INPUT.with(|b| {
            let b = b.borrow();
            assert!(b.is_empty());
        });
        let reply = OUTPUT.with(|o| o.borrow().clone());
        assert!(reply.starts_with(b"enc:v1:"));
        assert!(!reply.windows(4).any(|w| w == [0x5a; 4]));
    }

    fn u16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn mcp_shapes() {
        let mut v = vec![1, 4];
        v.extend_from_slice(&1f64.to_le_bytes());
        v.extend_from_slice(&u16le("data: {\"id\":1,\"result\":{}}\n"));
        assert_eq!(run_mcp_rpc(&v), (0, b"\0{\"id\":1,\"result\":{}}".to_vec()));
        let mut v = vec![0, 5, 1, 0, 0, 0];
        v.extend_from_slice(&u16le("a{\"id\":\"b\"}"));
        assert_eq!(run_mcp_rpc(&v), (0, b"\x01{\"id\":\"b\"}".to_vec()));
        assert_eq!(run_mcp_rpc(&[0, 0, 0x7b]).0, 2);
        assert_eq!(run_mcp_rpc(&[2, 0]).0, 2);
        assert_eq!(run_mcp_rpc(&[0, 9]).0, 2);
        assert_eq!(run_mcp_rpc(&[0, 0]), (0, vec![4]));
        assert_eq!(
            run_mcp_schema(&u16le("{\"a\":1}")),
            (0, b"\0{\"=a\":1}".to_vec())
        );
        assert_eq!(
            run_mcp_schema(&u16le("{\"$ref\":\"x\"}")),
            (0, b"\x01{\"code\":\"non_local\",\"ref\":\"x\"}".to_vec())
        );
        assert_eq!(run_mcp_schema(&u16le("{")).0, 2);
        assert_eq!(run_mcp_schema(&[0x7b]).0, 2);
    }

    #[test]
    fn caps_and_error_shapes() {
        let (s, r) = run_template_caps(b"{{ raise_exception('roles must alternate') }}");
        assert_eq!(s, 0);
        assert!(r.contains("\"sendTools\":false"));
        assert_eq!(run_template_caps(&[0xff]).0, 2);
        let mut v = 400u32.to_le_bytes().to_vec();
        v.extend_from_slice(br#"{"error":"System role not supported"}"#);
        assert_eq!(
            run_provider_error(&v),
            (
                0,
                r#"{"kind":"template_or_tools_unsupported","reason":"System role not supported"}"#
                    .to_owned()
            )
        );
        assert_eq!(run_provider_error(&[1, 0]).0, 2);
        let mut v = 200u32.to_le_bytes().to_vec();
        v.extend_from_slice(br#"{"choices":[{"message":{"content":"ok"}}]}"#);
        assert_eq!(
            run_serving_verdict(&v),
            (0, r#"{"kind":null,"passed":true,"reason":""}"#.to_owned())
        );
    }

    #[test]
    fn autotune_plan_shapes() {
        let (s, r) = run_autotune_plan(
            br#"{"facts":{"nCtxTrain":8192,"blockCount":2,"headCount":2,"embeddingLength":64,"modelBytes":1000},"memory":{"budgetMib":4096},"ladder":[4096,8192],"kv":["f16"]}"#,
        );
        assert_eq!(s, 0);
        assert!(r.starts_with(r#"{"ctx":8192,"#), "{r}");
        assert_eq!(run_autotune_plan(b"{").0, 1);
        assert_eq!(run_autotune_plan(&[0xff]).0, 2);
        let big = vec![b' '; autotune_plan::MAX_INPUT_BYTES + 1];
        assert_eq!(
            run_autotune_plan(&big),
            (1, r#"{"error":"too_large"}"#.to_owned())
        );
    }

    #[test]
    fn tune_contention_shapes() {
        let (s, r) = run_tune_contention(
            br#"{"tuning":"t","rows":[{"id":"q","status":"loaded","busy":2}],"prev":null,"startedAt":0,"now":5,"maxWaitMs":10,"quietMs":0}"#,
        );
        assert_eq!(s, 0);
        assert_eq!(
            r,
            r#"{"action":"wait","fingerprint":"[[\"q\",\"busy\"]]","foreign":["q"],"reason":"busy","since":5,"unload":[],"waitedMs":5}"#
        );
        assert_eq!(run_tune_contention(b"{").0, 1);
        assert_eq!(run_tune_contention(&[0xff]).0, 2);
        let big = vec![b' '; tune_contention::MAX_INPUT_BYTES + 1];
        assert_eq!(
            run_tune_contention(&big),
            (1, r#"{"error":"too_large"}"#.to_owned())
        );
    }

    #[test]
    fn preset_reload_shapes() {
        let (s, r) = run_preset_reload(
            br#"{"baseline":"[a]\nctx-size = 1\n","current":"[a]\nctx-size = 1\n[b]\n","loaded":["a"]}"#,
        );
        assert_eq!(s, 0);
        assert_eq!(
            r,
            r#"{"changed":[],"detail":null,"reason":"unchanged","safe":true}"#
        );
        assert_eq!(run_preset_reload(b"{").0, 1);
        assert_eq!(run_preset_reload(&[0xff]).0, 2);
        let big = vec![b' '; preset_reload::MAX_INPUT_BYTES + 1];
        assert_eq!(
            run_preset_reload(&big),
            (1, r#"{"error":"too_large"}"#.to_owned())
        );
    }

    #[test]
    fn load_verdict_shapes() {
        let (s, r) = run_load_verdict(
            br#"{"cause":"load","evidence":{"status":500,"text":"ErrorOutOfDeviceMemory"}}"#,
        );
        assert_eq!(s, 0);
        assert!(r.contains(r#""outcome":"oom""#) && r.contains(r#""source":"rule""#));
        assert_eq!(run_load_verdict(b"{").0, 1);
        assert_eq!(run_load_verdict(&[0xff]).0, 2);
        let big = vec![b' '; load_verdict::MAX_INPUT_BYTES + 1];
        assert_eq!(
            run_load_verdict(&big),
            (1, r#"{"error":"too_large"}"#.to_owned())
        );
    }

    #[test]
    fn long_profile_shapes() {
        let (s, r) = run_long_profile(
            br#"{"op":"pairs","rows":[{"id":"a","model":"/f"},{"id":"a-long","model":"/f"}]}"#,
        );
        assert_eq!(s, 0);
        assert_eq!(r, r#"{"pairs":[{"base":"a","long":"a-long"}]}"#);
        assert_eq!(run_long_profile(b"{").0, 1);
        assert_eq!(run_long_profile(&[0xff]).0, 2);
        let big = vec![b' '; long_profile::MAX_INPUT_BYTES + 1];
        assert_eq!(
            run_long_profile(&big),
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
