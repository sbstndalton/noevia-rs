//! The sizing calls the runner makes through Node's llamacpp-autoconfig.cjs (`cacheRamMibOf`,
//! `estimateFootprintForAutotune`), answered by the same Rust program here, and `tuneCacheMib`.

use crate::deps::Fail;
use crate::js;
use js_json::{wtf16, JValue};

fn impl_fault(what: &str) -> Fail {
    Fail {
        code: Some("autoconfig_impl".into()),
        status: Some(503),
        ..Fail::new(format!("The Rust sizing port did not answer ({what})."))
    }
}

fn call(op: u8, request: &JValue, what: &str) -> Result<JValue, Fail> {
    let mut input = vec![op];
    input.extend_from_slice(wtf16::stringify(request).unwrap_or_default().as_bytes());
    let (status, reply) = llamacpp_autoconfig::call(&input);
    if status != 0 {
        return Err(impl_fault(what));
    }
    wtf16::parse(&reply).map_err(|_| impl_fault(what))
}

/// `cacheRamMibOf(value)`: the MiB a `cache-ram` value leaves (`Infinity` when unbounded).
pub fn cache_ram_mib_of(value: &str) -> Result<f64, Fail> {
    let r = call(
        4,
        &JValue::obj([("value", JValue::from(value))]),
        "prompt cache size",
    )?;
    if r.get("unbounded").truthy() {
        return Ok(f64::INFINITY);
    }
    r.get("mib")
        .as_num()
        .ok_or_else(|| impl_fault("prompt cache size"))
}

/// `tuneCacheMib(eff)`: the prompt cache every auto-tune write leaves (llamacpp-presets prepare):
/// unset becomes the cap, anything larger the hard maximum, `-1` stays unbounded.
pub fn tune_cache_mib(effective: &JValue, cap_mib: f64, hard_max_mib: f64) -> Result<f64, Fail> {
    let raw = match effective.get("cache-ram") {
        JValue::Undefined | JValue::Null => String::new(),
        v => js_json::to_js_string(v).unwrap_or_default(),
    };
    let raw = js::trim(&raw);
    if raw.is_empty() {
        return Ok(cap_mib);
    }
    if raw == "-1" {
        return Ok(f64::INFINITY);
    }
    let units: Vec<u16> = raw.encode_utf16().collect();
    let clamped = llamacpp_presets::clamp_cache_ram(&units, hard_max_mib.max(0.0) as u64);
    cache_ram_mib_of(&String::from_utf16_lossy(&clamped))
}

/// `estimateFootprintForAutotune(request)`: `{ sizeable, totalGib, ... }`.
pub fn estimate_footprint(request: &JValue) -> Result<JValue, Fail> {
    call(3, request, "autotune KV ceiling")
}
