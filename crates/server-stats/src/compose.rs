//! The reply core `server/routes/models.cjs` sends for `/api/stats`, byte for byte: the same keys
//! in the same order, `null` where a field is not a number, numbers as `JSON.stringify` writes them
//! (`Infinity` as `null`), and a 500 `{"error":"Internal error"}` where the JS throws
//! (index.cjs's catch, http.cjs `errorResponse`).

use crate::js::{self, get, number, push_number, stringify, truthy, Fault};
use crate::metrics::{Summary, SCOPE};
use crate::mtp::{acceptance, Acceptance};
use prompt_framing::json::{self as pjson, Value};

/// A settled `fetchJson` (the `Promise.allSettled` entry).
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    /// The fetch rejected (network error, timeout, redirect, invalid URL or header).
    Rejected,
    /// `{ ok, status, body }`; an oversized body is `ok: false` with no body (`Null` here).
    Done { ok: bool, body: Value },
}

impl Read {
    /// The body of a fulfilled, ok read.
    pub fn ok_body(&self) -> Option<&Value> {
        match self {
            Read::Done { ok: true, body } => Some(body),
            _ => None,
        }
    }
}

/// The reply: status and the exact body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    /// index.cjs's catch for a thrown TypeError: errorResponse gives 500 `Internal error`.
    pub fn internal_error() -> Reply {
        Reply {
            status: 500,
            body: r#"{"error":"Internal error"}"#.into(),
        }
    }

    /// The port does not model the engine's answer (a value nested past the parse cap).
    pub fn unavailable() -> Reply {
        Reply {
            status: 503,
            body: r#"{"error":"Engine statistics are unavailable."}"#.into(),
        }
    }

    fn of(result: Result<Vec<u8>, Fault>) -> Reply {
        match result {
            Ok(body) => Reply {
                status: 200,
                body: String::from_utf8_lossy(&body).into_owned(),
            },
            Err(Fault::Throws) => Reply::internal_error(),
            Err(Fault::Unsupported) => Reply::unavailable(),
        }
    }
}

/// engine-stats.cjs `reportedTokenRate(g)`: a finite positive rate over a safe-integer count of at
/// least 2 that spans at least one second.
pub fn reported_token_rate(rate: Option<&Value>, count: Option<&Value>) -> Option<f64> {
    let rate = number(rate).filter(|r| r.is_finite() && *r > 0.0)?;
    let count = number(count).filter(|c| {
        c.is_finite() && c.fract() == 0.0 && c.abs() <= 9_007_199_254_740_991.0 && *c >= 2.0
    })?;
    (count / rate >= 1.0).then_some(rate)
}

fn key(out: &mut Vec<u8>, name: &str) {
    out.push(b',');
    out.push(b'"');
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b"\":");
}

fn opt_number(out: &mut Vec<u8>, n: Option<f64>) {
    match n {
        Some(n) => push_number(out, n),
        None => out.extend_from_slice(b"null"),
    }
}

/// The twelve trailing fields, all `typeof x === 'number' ? x : null` except the token rate.
struct Fields {
    tokens_per_second: Option<f64>,
    time_to_first_token: Option<f64>,
    input_tokens: Option<f64>,
    output_tokens: Option<f64>,
    input_tokens_total: Option<f64>,
    output_tokens_total: Option<f64>,
    request_count: Option<f64>,
    cpu_percent: Option<f64>,
    gpu_percent: Option<f64>,
    vram_gb: Option<f64>,
    memory_gb: Option<f64>,
}

fn finish(out: &mut Vec<u8>, f: &Fields) {
    for (name, n) in [
        ("tokensPerSecond", f.tokens_per_second),
        ("timeToFirstToken", f.time_to_first_token),
        ("inputTokens", f.input_tokens),
        ("outputTokens", f.output_tokens),
        ("inputTokensTotal", f.input_tokens_total),
        ("outputTokensTotal", f.output_tokens_total),
        ("requestCount", f.request_count),
        ("cpuPercent", f.cpu_percent),
        ("gpuPercent", f.gpu_percent),
        ("vramGb", f.vram_gb),
        ("memoryGb", f.memory_gb),
    ] {
        key(out, name);
        opt_number(out, n);
    }
    out.push(b'}');
}

/// One `{ "model": ..., ... }` member, left out where the JS value is `undefined`.
fn model_member(out: &mut Vec<u8>, model: Option<&Value>) -> Result<bool, Fault> {
    match model {
        None => Ok(false),
        Some(m) => {
            out.extend_from_slice(b"\"model\":");
            stringify(out, m)?;
            Ok(true)
        }
    }
}

/// The llama.cpp kind: `g` is `summarize`'s answer when the engine read succeeded, else `{}`
/// (`None`); `mtp` is `g.mtp || []`. The adapter has no host telemetry and no per-request counts,
/// so those fields are always `null`.
pub fn llamacpp(gen: Result<Option<Summary>, Fault>) -> Reply {
    let gen = match gen {
        Ok(g) => g,
        Err(f) => return Reply::of(Err(f)),
    };
    Reply::of(llamacpp_body(gen.as_ref()))
}

fn llamacpp_body(g: Option<&Summary>) -> Result<Vec<u8>, Fault> {
    let mut out = b"{\"up\":".to_vec();
    out.extend_from_slice(if g.is_some() { b"true" } else { b"false" });
    key(&mut out, "telemetryScope");
    match g {
        Some(_) => pjson::push_ascii(&mut out, SCOPE),
        None => out.extend_from_slice(b"null"),
    }
    key(&mut out, "mtp");
    out.push(b'[');
    for (i, row) in g
        .map(|g| g.mtp.as_slice())
        .unwrap_or(&[])
        .iter()
        .enumerate()
    {
        if i > 0 {
            out.push(b',');
        }
        out.push(b'{');
        if model_member(&mut out, row.model.as_ref())? {
            out.push(b',');
        }
        out.extend_from_slice(b"\"drafted\":");
        push_number(&mut out, row.drafted);
        out.extend_from_slice(b",\"accepted\":");
        push_number(&mut out, row.accepted);
        out.extend_from_slice(b",\"rate\":");
        push_number(&mut out, row.rate);
        out.extend_from_slice(b",\"source\":\"backend total\"}");
    }
    out.push(b']');
    finish(
        &mut out,
        &Fields {
            // summarize() has no output_tokens, so reportedTokenRate is always null here.
            tokens_per_second: None,
            time_to_first_token: None,
            input_tokens: None,
            output_tokens: None,
            input_tokens_total: g.and_then(|g| g.input_tokens_total),
            output_tokens_total: g.and_then(|g| g.output_tokens_total),
            request_count: None,
            cpu_percent: None,
            gpu_percent: None,
            vram_gb: None,
            memory_gb: None,
        },
    );
    Ok(out)
}

/// The five reads of the shared answer, for a kind other than llama.cpp.
#[derive(Debug, Clone, PartialEq)]
pub struct Reads {
    pub gen: Read,
    pub sys: Read,
    pub health: Read,
    pub metrics: Read,
    pub models: Read,
}

impl Reads {
    /// `modelManager.enabled` false (the `none` kind): every read is `{ ok: false }`.
    pub fn disabled() -> Reads {
        let off = || Read::Done {
            ok: false,
            body: Value::Null,
        };
        Reads {
            gen: off(),
            sys: off(),
            health: off(),
            metrics: off(),
            models: off(),
        }
    }
}

/// Lemonade and `none`: the fields read off `/v1/stats` and `/v1/system-stats`, and `mtp` from
/// mtp.cjs `acceptance`.
pub fn general(r: &Reads) -> Reply {
    Reply::of(general_body(r))
}

fn general_body(r: &Reads) -> Result<Vec<u8>, Fault> {
    // Every read the JS makes, in its order (the object literal's), before anything is written: a
    // throw anywhere is the 500, and only then can writing meet a value this port does not model.
    let empty = Value::Obj(Vec::new());
    let g = r.gen.ok_body().unwrap_or(&empty);
    let s = r.sys.ok_body().unwrap_or(&empty);
    // `g.scope || null`
    let scope = get(Some(g), "scope")?.filter(|v| truthy(Some(v)));
    // acceptance(metrics || '', health.all_models_loaded || [], models.data || [], userId)
    let blank = Value::Str(Vec::new());
    let metrics = r.metrics.ok_body().unwrap_or(&blank);
    let none = Value::Arr(Vec::new());
    let loaded = match r.health.ok_body() {
        Some(b) => get(Some(b), "all_models_loaded")?,
        None => Some(&none),
    };
    let installed = match r.models.ok_body() {
        Some(b) => get(Some(b), "data")?,
        None => Some(&none),
    };
    let rows = acceptance(metrics, loaded, installed)?;
    let gn = |k: &str| get(Some(g), k).map(number);
    let sn = |k: &str| get(Some(s), k).map(number);
    let fields = Fields {
        tokens_per_second: reported_token_rate(
            js::opt(Some(g), "tokens_per_second")?,
            js::opt(Some(g), "output_tokens")?,
        ),
        time_to_first_token: gn("time_to_first_token")?,
        input_tokens: gn("input_tokens")?,
        output_tokens: gn("output_tokens")?,
        input_tokens_total: gn("input_tokens_total")?,
        output_tokens_total: gn("output_tokens_total")?,
        request_count: gn("request_count_total")?,
        cpu_percent: sn("cpu_percent")?,
        gpu_percent: sn("gpu_percent")?,
        vram_gb: sn("vram_gb")?,
        memory_gb: sn("memory_gb")?,
    };
    let mut out = b"{\"up\":".to_vec();
    out.extend_from_slice(if r.gen.ok_body().is_some() {
        b"true"
    } else {
        b"false"
    });
    key(&mut out, "telemetryScope");
    match scope {
        Some(v) => stringify(&mut out, v)?,
        None => out.extend_from_slice(b"null"),
    }
    key(&mut out, "mtp");
    out.push(b'[');
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        acceptance_row(&mut out, row)?;
    }
    out.push(b']');
    finish(&mut out, &fields);
    Ok(out)
}

fn acceptance_row(out: &mut Vec<u8>, row: &Acceptance) -> Result<(), Fault> {
    out.push(b'{');
    if model_member(out, row.model.as_ref())? {
        out.push(b',');
    }
    out.extend_from_slice(b"\"rate\":");
    opt_number(out, row.rate);
    out.extend_from_slice(b",\"drafted\":");
    opt_number(out, row.drafted);
    out.extend_from_slice(b",\"accepted\":");
    opt_number(out, row.accepted);
    out.extend_from_slice(b",\"source\":\"backend total\"}");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn token_rate_rules() {
        let n = |x: f64| Some(Value::Num(x));
        assert_eq!(
            reported_token_rate(n(13.18504153409047).as_ref(), n(109.0).as_ref()),
            Some(13.18504153409047)
        );
        assert_eq!(
            reported_token_rate(n(20.0).as_ref(), n(20.0).as_ref()),
            Some(20.0)
        );
        for (r, c) in [
            (1e6, 109.0),
            (100.0, 20.0),
            (10.0, 2.5),
            (0.0, 10.0),
            (-1.0, 10.0),
            (5.0, 1.0),
            (1.0, 9007199254740992.0),
        ] {
            assert_eq!(reported_token_rate(n(r).as_ref(), n(c).as_ref()), None);
        }
        assert_eq!(reported_token_rate(None, n(10.0).as_ref()), None);
    }

    #[test]
    fn engine_down_is_all_null() {
        let want = r#"{"up":false,"telemetryScope":null,"mtp":[],"tokensPerSecond":null,"timeToFirstToken":null,"inputTokens":null,"outputTokens":null,"inputTokensTotal":null,"outputTokensTotal":null,"requestCount":null,"cpuPercent":null,"gpuPercent":null,"vramGb":null,"memoryGb":null}"#;
        assert_eq!(general(&Reads::disabled()).body, want);
        assert_eq!(llamacpp(Ok(None)).body, want);
    }
}
