//! noevia-core's `server/tool-gate.cjs` rules in Rust, exported from `dav-parse.wasm`
//! (TOOL_GATE_IMPL): whether one message should make the chat loop prefetch a read-only tool (run
//! it first with arguments that follow from the message) or require it (force the first model turn
//! to call it), and what the Stage 2 decision service is shown and how its answer is read.
//!
//! - [`rule`]: `ruleDecision`, the Stage 1 rules (URL, diary, search, date, drive) over the tools
//!   offered on this request, with `deriveArgs` (`publicUrlPattern`, `searchQuery`, `diaryMonth`,
//!   `prefetchableSearch`).
//! - [`options`]: what `readout` shows the decision service: the offered read-only tools, the
//!   frame's preferred boxes first, trimmed by `shapeOptions` to the backend's limits.
//! - [`stage2`]: how `readout` reads the service's answer (`none`, not offered, below the
//!   confidence bound, or a tool, prefetched with `deriveArgs` at stage `decision` or required).
//! - [`search_query`], [`diary_month`], [`public_url`], [`prefetchable`]: the helpers, for the
//!   host's direct checks.
//!
//! The decision service call itself stays in JS. The host (tool-gate.cjs under
//! TOOL_GATE_IMPL=wasm) computes the JS answer first and never uses the port to do more: a tool is
//! prefetched only if both prefetch it with the same arguments, it is required if one requires it
//! and the other prefetches it, and any other difference, fault or refusal means no tool is forced
//! (the gate's "none", which is what the chat does with the gate off).
//!
//! # Where the port is stricter than the JS (by design)
//!
//! - **A URL with a non-ASCII unit, or a host with an `xn--` label** (which is also what a
//!   percent-encoded non-ASCII host becomes) is not a public URL pattern: the URL is required, not
//!   prefetched. Node's and the `url` crate's IDNA tables differ.
//! - Requests over [`MAX_INPUT_BYTES`], or over their work budget, are refused (`too_large`); the
//!   host treats that as a fault.
//!
//! Linear in the input: no pattern nests a quantifier, every loop and every matcher step is
//! charged to a work budget proportional to the request size ([`WORK_PER_BYTE`]), no panics.

#![forbid(unsafe_code)]

pub mod re;

use prompt_framing::js::{is_js_space, trim, units};
use prompt_framing::json::{self, Value};
use re::{bounded, digit, group, rep, space, words, ws0, ws1, Re, P};
use std::collections::HashMap;

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024 + 1;
/// Work units a request may use per byte of its size (plus [`WORK_FLOOR`]).
pub const WORK_PER_BYTE: u64 = 128;
/// Work units every request may use, whatever its size.
pub const WORK_FLOOR: u64 = 1 << 18;

/// JSON nesting kept: the outer array, a projection list, a projection, and its lists.
const JSON_CAP: usize = 5;

/// `SEARCH_PREFETCH_MAX_CHARS`.
pub const SEARCH_PREFETCH_MAX_CHARS: usize = 120;
/// `MAX_OPTIONS`.
pub const MAX_OPTIONS: usize = 25;
/// `MIN_LABEL_CHARS`.
pub const MIN_LABEL_CHARS: f64 = 12.0;
/// `QUESTION`.
pub const QUESTION: &str =
    "Which tool, if any, must the assistant call before answering this user message?";
const NONE_ID: &str = "none";
const NONE_LABEL_FREE: &str = "none: answer directly, no tool is needed";
const NONE_LABEL_LIMITED: &str = "No tool: answer directly";
const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// Why the port gives no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Input,
    TooLarge,
}

impl Refusal {
    /// The refusal reply.
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
        }
    }
}

type R<T> = Result<T, Refusal>;

/// Work left for one request.
#[derive(Debug)]
pub struct Work {
    left: u64,
}

impl Work {
    /// A budget of `units`.
    pub fn new(units: u64) -> Self {
        Work { left: units }
    }

    /// The budget for a request of `bytes` bytes.
    pub fn for_request(bytes: usize) -> Self {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        Work::new(
            bytes
                .saturating_mul(WORK_PER_BYTE)
                .saturating_add(WORK_FLOOR),
        )
    }

    /// Charge `n` units (and one for the step itself).
    pub fn charge(&mut self, n: usize) -> R<()> {
        let n = u64::try_from(n).unwrap_or(u64::MAX).saturating_add(1);
        if n > self.left {
            self.left = 0;
            return Err(Refusal::TooLarge);
        }
        self.left -= n;
        Ok(())
    }
}

// ── The patterns ────────────────────────────────────────────────────────────

fn not_url_stop(c: u16) -> bool {
    // [^\s<>"')\]]
    !(space(c) || matches!(c, 0x3c | 0x3e | 0x22 | 0x27 | 0x29 | 0x5d))
}

fn first_h(c: u16) -> bool {
    matches!(c, 0x48 | 0x68)
}
fn first_2(c: u16) -> bool {
    c == 0x32
}
fn first_letters(c: u16) -> bool {
    c < 0x80 && (c as u8).is_ascii_alphabetic()
}
fn first_date(c: u16) -> bool {
    digit(c) || first_letters(c)
}

/// The compiled patterns of one request.
pub struct Patterns {
    url: Re,
    search: Re,
    diary: Re,
    diary_word: Re,
    iso: Re,
    named: Re,
    drive: Re,
    filler: Re,
    yesterday: Re,
    today: Re,
}

fn ordinal() -> P {
    P::Opt(Box::new(P::Alt(vec![
        P::Lit("st"),
        P::Lit("nd"),
        P::Lit("rd"),
        P::Lit("th"),
    ])))
}

fn one_or_two_digits() -> P {
    P::Cat(vec![P::Class(digit), P::Opt(Box::new(P::Class(digit)))])
}

impl Patterns {
    /// Build them.
    pub fn new() -> Patterns {
        let lit = P::Lit;
        // /\bhttps?:\/\/[^\s<>"')\]]+/i
        let url = Re::new(
            P::Cat(vec![
                P::WordB,
                lit("http"),
                P::Opt(Box::new(lit("s"))),
                lit("://"),
                P::Plus(Box::new(P::Class(not_url_stop))),
            ]),
            first_h,
        );
        // /\b(search|look\s*up|google|latest|news|today'?s|current\s+price|price\s+of|weather|forecast)\b/i
        let search = Re::new(
            bounded(vec![
                lit("search"),
                P::Cat(vec![lit("look"), ws0(), lit("up")]),
                lit("google"),
                lit("latest"),
                lit("news"),
                P::Cat(vec![lit("today"), P::Opt(Box::new(lit("'"))), lit("s")]),
                words("current price"),
                words("price of"),
                lit("weather"),
                lit("forecast"),
            ]),
            first_letters,
        );
        // /\b(in\s+my\s+diary|my\s+diary|my\s+journal|yesterday\s+I|last\s+(?:week|month)\s+I)\b/i
        let diary = Re::new(
            bounded(vec![
                words("in my diary"),
                words("my diary"),
                words("my journal"),
                words("yesterday I"),
                P::Cat(vec![
                    lit("last"),
                    ws1(),
                    P::Alt(vec![lit("week"), lit("month")]),
                    ws1(),
                    lit("I"),
                ]),
            ]),
            first_letters,
        );
        let diary_word = Re::new(bounded(vec![lit("diary"), lit("journal")]), first_letters);
        // /\b(20\d{2})-(0[1-9]|1[0-2])(?:-(0[1-9]|[12]\d|3[01]))?\b/
        let iso = Re::new(
            P::Cat(vec![
                P::WordB,
                P::Group(1, Box::new(P::Cat(vec![lit("20"), rep(2, digit)]))),
                lit("-"),
                P::Group(
                    2,
                    Box::new(P::Alt(vec![
                        P::Cat(vec![lit("0"), P::Class(|c| (0x31..=0x39).contains(&c))]),
                        P::Cat(vec![lit("1"), P::Class(|c| (0x30..=0x32).contains(&c))]),
                    ])),
                ),
                P::Opt(Box::new(P::Cat(vec![
                    lit("-"),
                    P::Group(
                        3,
                        Box::new(P::Alt(vec![
                            P::Cat(vec![lit("0"), P::Class(|c| (0x31..=0x39).contains(&c))]),
                            P::Cat(vec![P::Class(|c| c == 0x31 || c == 0x32), P::Class(digit)]),
                            P::Cat(vec![lit("3"), P::Class(|c| c == 0x30 || c == 0x31)]),
                        ])),
                    ),
                ]))),
                P::WordB,
            ]),
            first_2,
        );
        // \b(?:(\d{1,2})(?:st|nd|rd|th)?\s+)?(months)(?:\s+(\d{1,2})(?:st|nd|rd|th)?)?(?:,?\s+(20\d{2}))?\b /i
        let named = Re::new(
            P::Cat(vec![
                P::WordB,
                P::Opt(Box::new(P::Cat(vec![
                    P::Group(1, Box::new(one_or_two_digits())),
                    ordinal(),
                    ws1(),
                ]))),
                P::Group(2, Box::new(P::Alt(MONTHS.iter().map(|m| lit(m)).collect()))),
                P::Opt(Box::new(P::Cat(vec![
                    ws1(),
                    P::Group(3, Box::new(one_or_two_digits())),
                    ordinal(),
                ]))),
                P::Opt(Box::new(P::Cat(vec![
                    P::Opt(Box::new(lit(","))),
                    ws1(),
                    P::Group(4, Box::new(P::Cat(vec![lit("20"), rep(2, digit)]))),
                ]))),
                P::WordB,
            ]),
            first_date,
        );
        // /\b(my\s+files?|a\s+file|the\s+file|files|folder|folders|document\s+named|drive|google\s+drive|nextcloud)\b/i
        let drive = Re::new(
            bounded(vec![
                P::Cat(vec![
                    lit("my"),
                    ws1(),
                    lit("file"),
                    P::Opt(Box::new(lit("s"))),
                ]),
                words("a file"),
                words("the file"),
                lit("files"),
                lit("folder"),
                lit("folders"),
                words("document named"),
                lit("drive"),
                words("google drive"),
                lit("nextcloud"),
            ]),
            first_letters,
        );
        // FILLER_RE (/gi)
        let filler = Re::new(
            bounded(vec![
                lit("please"),
                words("can you"),
                words("could you"),
                words("would you"),
                words("will you"),
                words("for me"),
                P::Cat(vec![
                    lit("search"),
                    P::Opt(Box::new(P::Cat(vec![ws1(), lit("the"), ws1(), lit("web")]))),
                    P::Opt(Box::new(P::Cat(vec![ws1(), lit("for")]))),
                ]),
                P::Cat(vec![lit("look"), ws0(), lit("up")]),
                lit("google"),
                words("find out"),
                words("tell me"),
                P::Cat(vec![
                    lit("what"),
                    P::Alt(vec![
                        lit("'s"),
                        P::Cat(vec![ws1(), lit("is")]),
                        P::Cat(vec![ws1(), lit("are")]),
                    ]),
                ]),
                P::Cat(vec![
                    lit("who"),
                    P::Alt(vec![lit("'s"), P::Cat(vec![ws1(), lit("is")])]),
                ]),
                words("show me"),
                words("i want to know"),
                lit("quickly"),
                lit("hey"),
                lit("hi"),
            ]),
            first_letters,
        );
        let yesterday = Re::new(
            P::Cat(vec![P::WordB, lit("yesterday"), P::WordB]),
            first_letters,
        );
        let today = Re::new(
            P::Cat(vec![P::WordB, lit("today"), P::WordB]),
            first_letters,
        );
        Patterns {
            url,
            search,
            diary,
            diary_word,
            iso,
            named,
            drive,
            filler,
            yesterday,
            today,
        }
    }
}

impl Default for Patterns {
    fn default() -> Self {
        Patterns::new()
    }
}

fn slice(s: &[u16], (a, b): (usize, usize)) -> &[u16] {
    s.get(a..b).unwrap_or(&[])
}

/// `.replace(/[set]+$/, '')`: the longest trailing run of `set` removed.
fn strip_trailing(s: &[u16], set: &[u8], work: &mut Work) -> R<Vec<u16>> {
    work.charge(s.len())?;
    let mut end = s.len();
    while end > 0
        && s.get(end - 1)
            .is_some_and(|&c| c < 0x80 && set.contains(&(c as u8)))
    {
        end -= 1;
    }
    Ok(s.get(..end).unwrap_or(&[]).to_vec())
}

/// `.replace(/\s+/g, ' ').trim()`.
fn collapse(s: &[u16], work: &mut Work) -> R<Vec<u16>> {
    work.charge(s.len())?;
    let mut out = Vec::with_capacity(s.len());
    let mut in_space = false;
    for &c in s {
        if is_js_space(c) {
            if !in_space {
                out.push(0x20);
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    Ok(trim(&out).to_vec())
}

fn includes(hay: &[u16], needle: &str) -> bool {
    let n = units(needle);
    !n.is_empty() && hay.windows(n.len()).any(|w| w == n.as_slice())
}

/// `prefetchableSearch(message)`.
pub fn prefetchable(message: &[u16], work: &mut Work) -> R<bool> {
    work.charge(message.len().saturating_mul(4))?;
    if trim(message).len() > SEARCH_PREFETCH_MAX_CHARS {
        return Ok(false);
    }
    if message.iter().any(|&c| c == 0x0d || c == 0x0a) {
        return Ok(false);
    }
    if includes(message, "```") || includes(message, "~~~") {
        return Ok(false);
    }
    // /^\s*>/
    let first = message.iter().find(|&&c| !is_js_space(c));
    Ok(first != Some(&0x3e))
}

/// `searchQuery(message)`.
pub fn search_query(pats: &Patterns, message: &[u16], work: &mut Work) -> R<Vec<u16>> {
    let replaced = pats.filler.replace_all(message, &[0x20], work)?;
    let stripped = strip_trailing(&replaced, b"?!.", work)?;
    let q = collapse(&stripped, work)?;
    let q = if q.is_empty() {
        trim(message).to_vec()
    } else {
        q
    };
    Ok(q.get(..q.len().min(300)).unwrap_or(&[]).to_vec())
}

// ── Dates (ECMAScript Date, UTC) ────────────────────────────────────────────

const MS_PER_DAY: f64 = 86_400_000.0;

/// TimeClip: the time value `new Date(t)` holds, or `None` for NaN.
fn time_clip(t: f64) -> Option<f64> {
    if !t.is_finite() || t.abs() > 8.64e15 {
        return None;
    }
    Some(t.trunc() + 0.0)
}

/// (year, month 0..11) of a clipped time value.
fn year_month(t: f64) -> Option<(i64, u32)> {
    let t = time_clip(t)?;
    // |t| <= 8.64e15, so the day count fits an i64 exactly.
    let days = (t / MS_PER_DAY).floor() as i64;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    Some((y, u32::try_from(m - 1).unwrap_or(0)))
}

fn pad2(n: u32) -> String {
    format!("{n:02}")
}

/// `monthKey(new Date(t))`.
fn month_key(t: f64) -> String {
    match year_month(t) {
        Some((y, m)) => format!("{y}-{}", pad2(m + 1)),
        None => "NaN-NaN".into(),
    }
}

/// `diaryMonth(message, () => now)`; `now` NaN where the host's clock gave no finite number.
pub fn diary_month(
    pats: &Patterns,
    message: &[u16],
    now: f64,
    work: &mut Work,
) -> R<Option<Vec<u16>>> {
    if let Some(c) = pats.iso.find(message, 0, work)? {
        let (Some(y), Some(m)) = (group(&c, 1), group(&c, 2)) else {
            return Err(Refusal::Input);
        };
        let mut out = slice(message, y).to_vec();
        out.push(0x2d);
        out.extend_from_slice(slice(message, m));
        return Ok(Some(out));
    }
    if let Some(c) = pats.named.find(message, 0, work)? {
        if group(&c, 1).is_some() || group(&c, 3).is_some() || group(&c, 4).is_some() {
            let year = match group(&c, 4) {
                // "20dd": Number() of four ASCII digits.
                Some(g) => String::from_utf16_lossy(slice(message, g))
                    .parse::<u32>()
                    .map(|y| y.to_string())
                    .map_err(|_| Refusal::Input)?,
                None => match year_month(now) {
                    Some((y, _)) => y.to_string(),
                    None => "NaN".into(),
                },
            };
            let name = group(&c, 2).map(|g| slice(message, g)).unwrap_or(&[]);
            let lower: Vec<u16> = name.iter().map(|&u| u | 0x20).collect();
            let index = MONTHS
                .iter()
                .position(|m| units(m) == lower)
                .ok_or(Refusal::Input)?;
            let month = u32::try_from(index + 1).map_err(|_| Refusal::Input)?;
            return Ok(Some(units(&format!("{year}-{}", pad2(month)))));
        }
    }
    if pats.yesterday.test(message, work)? {
        return Ok(Some(units(&month_key(now - MS_PER_DAY))));
    }
    if pats.today.test(message, work)? {
        return Ok(Some(units(&month_key(now))));
    }
    Ok(None)
}

// ── publicUrlPattern ────────────────────────────────────────────────────────

fn is_local_suffix(host: &str) -> bool {
    // /(^|\.)(localhost|local|internal|lan|home\.arpa|intranet|corp)$/i
    [
        "localhost",
        "local",
        "internal",
        "lan",
        "home.arpa",
        "intranet",
        "corp",
    ]
    .iter()
    .any(|s| {
        host == *s
            || (host.len() > s.len()
                && host.ends_with(s)
                && host.as_bytes().get(host.len() - s.len() - 1) == Some(&b'.'))
    })
}

/// `publicUrlPattern(raw)` (with noevia#1224's empty-label rule), with the port's stricter
/// non-ASCII and `xn--` rules.
pub fn public_url(raw: &[u16], work: &mut Work) -> R<bool> {
    work.charge(raw.len().saturating_mul(4))?;
    if raw.iter().any(|&c| c >= 0x80) {
        return Ok(false); // stricter: the IDNA tables differ
    }
    let text = String::from_utf16_lossy(raw);
    let Ok(url) = url::Url::parse(&text) else {
        return Ok(false);
    };
    work.charge(url.as_str().len())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Ok(false);
    }
    if !url.username().is_empty() || url.password().is_some_and(|p| !p.is_empty()) {
        return Ok(false);
    }
    let hostname = url.host_str().unwrap_or("");
    let bare = hostname.strip_prefix('[').unwrap_or(hostname);
    let bare = bare.strip_suffix(']').unwrap_or(bare);
    // `.replace(/\.$/, '')`: one trailing dot.
    let host = bare.strip_suffix('.').unwrap_or(bare).to_ascii_lowercase();
    if host.is_empty() {
        return Ok(false);
    }
    // noevia#1224: an empty label (`nas.local..`, `10.0.0.1..`) is not public.
    if host.split('.').any(str::is_empty) {
        return Ok(false);
    }
    if host.split('.').any(|l| l.starts_with("xn--")) {
        return Ok(false); // stricter: IDNA
    }
    if matches!(url.host(), Some(url::Host::Ipv4(_) | url::Host::Ipv6(_))) {
        return Ok(!ssrf_policy::is_private_ip(&host));
    }
    if !host.contains('.') || is_local_suffix(&host) {
        return Ok(false);
    }
    Ok(true)
}

// ── Projections ─────────────────────────────────────────────────────────────

/// The argument names deriveArgs may write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Urls,
    Url,
    Query,
    Q,
    Month,
}

impl Key {
    const ALL: [Key; 5] = [Key::Urls, Key::Url, Key::Query, Key::Q, Key::Month];
    fn name(self) -> &'static str {
        match self {
            Key::Urls => "urls",
            Key::Url => "url",
            Key::Query => "query",
            Key::Q => "q",
            Key::Month => "month",
        }
    }
    fn bit(self) -> u8 {
        match self {
            Key::Urls => 1,
            Key::Url => 2,
            Key::Query => 4,
            Key::Q => 8,
            Key::Month => 16,
        }
    }
}

/// One offered tool as the host projects it: its name, `!isWriteTool(name)` (false where that
/// throws), its description (`String(description || '')`), which argument names its
/// `parameters.properties` has as own properties and which of them are truthy, whether it has any
/// own enumerable property, and `parameters.required` (each `String(k)`; `null` for a symbol).
#[derive(Clone, Debug, Default)]
pub struct Tool {
    pub name: Vec<u16>,
    pub read_only: bool,
    pub description: Vec<u16>,
    pub own: u8,
    pub truthy: u8,
    pub any_props: bool,
    pub required: Vec<Option<Vec<u16>>>,
}

impl Tool {
    fn own(&self, k: Key) -> bool {
        self.own & k.bit() != 0
    }
    fn truthy(&self, k: Key) -> bool {
        self.truthy & k.bit() != 0
    }
}

/// `boxes` as `Object.keys` order pairs: kind and its tool names (`null` for a non-string).
pub type Boxes = Vec<(Vec<u16>, Vec<Option<Vec<u16>>>)>;

fn boxes_of<'a>(boxes: &'a Boxes, kind: &str) -> &'a [Option<Vec<u16>>] {
    boxes
        .iter()
        .find(|(k, _)| k.iter().copied().eq(kind.encode_utf16()))
        .map_or(&[], |(_, v)| v.as_slice())
}

/// What deriveArgs returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Args {
    /// `{}` (a diary tool that needs no month).
    Empty,
    /// `{ key: value }` (`urls` holds a one-element list).
    One(Key, Vec<u16>),
}

/// A gate decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Prefetch { tool: Vec<u16>, args: Args },
    Require { tool: Vec<u16> },
}

impl Decision {
    fn tool(&self) -> &[u16] {
        match self {
            Decision::Prefetch { tool, .. } | Decision::Require { tool } => tool,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Rule,
    Decision,
}

/// `fits(args)`.
fn fits(tool: &Tool, key: Key, value: Vec<u16>) -> Option<Args> {
    let required_ok = tool.required.iter().all(|k| {
        k.as_ref()
            .is_some_and(|k| k.iter().copied().eq(key.name().encode_utf16()))
    });
    let keys_ok = tool.own(key) || !tool.any_props;
    (required_ok && keys_ok).then_some(Args::One(key, value))
}

/// `deriveArgs(kind, tool, message, now, stage)`.
fn derive(
    pats: &Patterns,
    kind: &str,
    tool: &Tool,
    message: &[u16],
    now: f64,
    stage: Stage,
    work: &mut Work,
) -> R<Option<Args>> {
    work.charge(tool.required.len())?;
    match kind {
        "url" => {
            let Some(c) = pats.url.find(message, 0, work)? else {
                return Ok(None);
            };
            let Some(m) = group(&c, 0) else {
                return Ok(None);
            };
            let url = strip_trailing(slice(message, m), b".,;:!?", work)?;
            if url.is_empty() || !public_url(&url, work)? {
                return Ok(None);
            }
            if tool.truthy(Key::Urls) {
                return Ok(fits(tool, Key::Urls, url));
            }
            if tool.truthy(Key::Url) {
                return Ok(fits(tool, Key::Url, url));
            }
            Ok(None)
        }
        "search" => {
            if stage != Stage::Rule || !prefetchable(message, work)? {
                return Ok(None);
            }
            if tool.truthy(Key::Query) {
                return Ok(fits(tool, Key::Query, search_query(pats, message, work)?));
            }
            if tool.truthy(Key::Q) {
                return Ok(fits(tool, Key::Q, search_query(pats, message, work)?));
            }
            Ok(None)
        }
        "diary" => {
            if tool.required.is_empty() && !tool.truthy(Key::Month) {
                return Ok(Some(Args::Empty));
            }
            let month = diary_month(pats, message, now, work)?;
            match month {
                Some(m) if !m.is_empty() && tool.truthy(Key::Month) => {
                    Ok(fits(tool, Key::Month, m))
                }
                _ => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

/// The offered tools by name (the host's Map: unique names, in insertion order).
pub struct Offered<'a> {
    tools: &'a [Tool],
    by_name: HashMap<&'a [u16], usize>,
}

impl<'a> Offered<'a> {
    /// Index `tools` (a repeated name keeps its first position and its last value, as Map does).
    pub fn new(tools: &'a [Tool], work: &mut Work) -> R<Offered<'a>> {
        let mut by_name = HashMap::with_capacity(tools.len());
        for (i, t) in tools.iter().enumerate() {
            work.charge(t.name.len())?;
            by_name.insert(t.name.as_slice(), i);
        }
        Ok(Offered { tools, by_name })
    }

    fn get(&self, name: &[u16]) -> Option<&'a Tool> {
        self.by_name.get(name).and_then(|&i| self.tools.get(i))
    }

    /// The tools in Map order (first position, last value).
    fn values(&self, work: &mut Work) -> R<Vec<&'a Tool>> {
        let mut seen = std::collections::HashSet::with_capacity(self.by_name.len());
        let mut out = Vec::with_capacity(self.by_name.len());
        for t in self.tools {
            work.charge(t.name.len())?;
            if seen.insert(t.name.as_slice()) {
                out.push(self.get(&t.name).unwrap_or(t));
            }
        }
        Ok(out)
    }
}

/// `ruleDecision(message, offered)`: the rule that matched and its decision, or `None`.
pub fn rule(
    pats: &Patterns,
    message: &[u16],
    offered: &Offered,
    boxes: &Boxes,
    now: f64,
    work: &mut Work,
) -> R<Option<(&'static str, Decision)>> {
    let mut kinds: Vec<&'static str> = Vec::new();
    if pats.url.test(message, work)? {
        kinds.push("url");
    }
    if pats.diary.test(message, work)? || pats.diary_word.test(message, work)? {
        kinds.push("diary");
    }
    if pats.search.test(message, work)? {
        kinds.push("search");
    }
    let named_date = match pats.named.find(message, 0, work)? {
        Some(c) => group(&c, 1).is_some() || group(&c, 3).is_some() || group(&c, 4).is_some(),
        None => false,
    };
    if pats.iso.test(message, work)? || named_date {
        kinds.push("diary");
    }
    if pats.drive.test(message, work)? {
        kinds.push("drive");
    }
    for kind in kinds {
        for name in boxes_of(boxes, kind).iter().flatten() {
            work.charge(name.len())?;
            let Some(tool) = offered.get(name) else {
                continue;
            };
            if !tool.read_only {
                continue;
            }
            let decision = match derive(pats, kind, tool, message, now, Stage::Rule, work)? {
                Some(args) => Decision::Prefetch {
                    tool: name.clone(),
                    args,
                },
                None => Decision::Require { tool: name.clone() },
            };
            return Ok(Some((kind, decision)));
        }
    }
    Ok(None)
}

// ── Stage 2 ─────────────────────────────────────────────────────────────────

/// A number as the host projects it (`null` for NaN; `"Infinity"`, `"-Infinity"`, `"NaN"`).
fn number(v: &Value) -> R<f64> {
    match v {
        Value::Num(n) => Ok(*n),
        Value::Null => Ok(f64::NAN),
        Value::Str(s) => {
            let s = String::from_utf16_lossy(s);
            match s.as_str() {
                "Infinity" => Ok(f64::INFINITY),
                "-Infinity" => Ok(f64::NEG_INFINITY),
                "NaN" => Ok(f64::NAN),
                _ => Err(Refusal::Input),
            }
        }
        _ => Err(Refusal::Input),
    }
}

/// The backend limits `shapeOptions` reads, as the JS reads them: `maxOptions` (finite),
/// `Math.max(1, Number(maxLabelChars) || 120)` and `Number(maxChoiceChars) || Infinity`.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_options: f64,
    pub max_label: f64,
    pub max_choice: f64,
}

/// One Stage 2 option.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opt {
    pub id: Vec<u16>,
    pub label: Vec<u16>,
}

/// `String#slice(0, end)` for a non-negative (possibly fractional or infinite) end.
fn slice_to(s: &[u16], end: f64) -> &[u16] {
    let n = if end.is_nan() || end <= 0.0 {
        0
    } else if end >= s.len() as f64 {
        s.len()
    } else {
        end.trunc() as usize
    };
    s.get(..n).unwrap_or(&[])
}

/// `readout`'s options: the offered read-only tools, the frame's preferred boxes first, then
/// `shapeOptions`. `(trimmed, options)`.
pub fn options(
    offered: &Offered,
    prefer: Option<&[Option<Vec<u16>>]>,
    hint: Option<&[u16]>,
    limits: Option<Limits>,
    boxes: &Boxes,
    work: &mut Work,
) -> R<(usize, Vec<Opt>)> {
    let mut read: Vec<&Tool> = offered
        .values(work)?
        .into_iter()
        .filter(|t| t.read_only)
        .collect();
    let preferred: Vec<Option<&[u16]>> = match prefer {
        Some(kinds) => {
            let mut out = Vec::new();
            for k in kinds.iter().flatten() {
                work.charge(k.len())?;
                if let Some((_, names)) = boxes.iter().find(|(kind, _)| kind == k) {
                    work.charge(names.len())?;
                    // A non-string element never ranks a tool; it keeps its position.
                    out.extend(names.iter().map(|n| n.as_deref()));
                }
            }
            out
        }
        None => Vec::new(),
    };
    let rank_in = |list: &[Option<&[u16]>], t: &Tool| -> usize {
        list.iter()
            .position(|n| *n == Some(t.name.as_slice()))
            .unwrap_or(list.len())
    };
    if !preferred.is_empty() {
        work.charge(read.len().saturating_mul(preferred.len().saturating_add(8)))?;
        let mut keyed: Vec<(usize, usize, &Tool)> = read
            .iter()
            .enumerate()
            .map(|(i, t)| (rank_in(&preferred, t), i, *t))
            .collect();
        keyed.sort_by_key(|&(r, i, _)| (r, i));
        read = keyed.into_iter().map(|(_, _, t)| t).collect();
    }
    let question_chars = match hint {
        Some(h) if !h.is_empty() => QUESTION.len() + 1 + h.len().min(160),
        _ => QUESTION.len(),
    };
    let describe = |t: &Tool, work: &mut Work| collapse(&t.description, work);
    let Some(limits) = limits else {
        let tools = read.get(..read.len().min(MAX_OPTIONS - 1)).unwrap_or(&[]);
        let mut opts = Vec::with_capacity(tools.len() + 1);
        for t in tools {
            let d = describe(t, work)?;
            let mut label = t.name.clone();
            label.extend(units(": "));
            label.extend_from_slice(slice_to(&d, 160.0));
            opts.push(Opt {
                id: t.name.clone(),
                label,
            });
        }
        opts.push(Opt {
            id: units(NONE_ID),
            label: units(NONE_LABEL_FREE),
        });
        return Ok((read.len() - tools.len(), opts));
    };
    // known = [...preferred, ...Object.values(boxes).flat()]
    let mut known: Vec<Option<&[u16]>> = preferred.clone();
    for (_, names) in boxes {
        work.charge(names.len())?;
        known.extend(names.iter().map(|n| n.as_deref()));
    }
    work.charge(read.len().saturating_mul(known.len().saturating_add(8)))?;
    let mut keyed: Vec<(usize, usize, &Tool)> = read
        .iter()
        .enumerate()
        .map(|(i, t)| (rank_in(&known, t), i, *t))
        .collect();
    keyed.sort_by_key(|&(r, i, _)| (r, i));
    let ordered: Vec<&Tool> = keyed.into_iter().map(|(_, _, t)| t).collect();
    let none_chars = (NONE_ID.len() + NONE_LABEL_LIMITED.len() + 2) as f64;
    let budget = limits.max_choice - question_chars as f64 - none_chars;
    let first = (limits.max_options.min(MAX_OPTIONS as f64) - 1.0).max(0.0);
    let mut tools: Vec<&Tool> = slice_to_tools(&ordered, first);
    loop {
        work.charge(tools.len())?;
        let id_chars: f64 = tools.iter().map(|t| (t.name.len() + 2) as f64).sum();
        let per = if tools.is_empty() {
            0.0
        } else {
            limits
                .max_label
                .min(((budget - id_chars) / tools.len() as f64).floor())
        };
        if tools.is_empty() || per >= MIN_LABEL_CHARS {
            let trimmed = read.len() - tools.len();
            if tools.is_empty() {
                return Ok((trimmed, Vec::new()));
            }
            let mut opts = Vec::with_capacity(tools.len() + 1);
            for t in &tools {
                let d = describe(t, work)?;
                let base = if d.is_empty() { &t.name } else { &d };
                opts.push(Opt {
                    id: t.name.clone(),
                    label: slice_to(base, per).to_vec(),
                });
            }
            opts.push(Opt {
                id: units(NONE_ID),
                label: units(NONE_LABEL_LIMITED),
            });
            return Ok((trimmed, opts));
        }
        tools.pop();
    }
}

fn slice_to_tools<'a>(tools: &[&'a Tool], end: f64) -> Vec<&'a Tool> {
    let n = if end.is_nan() || end <= 0.0 {
        0
    } else if end >= tools.len() as f64 {
        tools.len()
    } else {
        end.trunc() as usize
    };
    tools.get(..n).unwrap_or(&[]).to_vec()
}

/// What the service selected, as the host projects `result.selected`.
#[derive(Clone, Debug)]
pub enum Selected {
    /// A string.
    Name(Vec<u16>),
    /// Falsy.
    Nothing,
    /// Truthy and not a string.
    Other,
}

/// How readout reads an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    Reason(&'static str),
    Decision(Decision),
}

/// `readout` after the service answered (not a fallback): `none`, `not-offered`,
/// `low-confidence`, or the decision. `lower`, `score`, `confidence` are the selected option's
/// interval lower bound, share and the result's confidence (NaN where not finite).
#[allow(clippy::too_many_arguments)]
pub fn stage2(
    pats: &Patterns,
    message: &[u16],
    offered: &Offered,
    boxes: &Boxes,
    now: f64,
    selected: &Selected,
    (lower, score, confidence): (f64, f64, f64),
    min: f64,
    work: &mut Work,
) -> R<Read> {
    let name = match selected {
        Selected::Nothing => return Ok(Read::Reason("none")),
        Selected::Name(n) if n.iter().copied().eq(NONE_ID.encode_utf16()) || n.is_empty() => {
            return Ok(Read::Reason("none"))
        }
        Selected::Other => return Ok(Read::Reason("not-offered")),
        Selected::Name(n) => n,
    };
    let Some(tool) = offered.get(name).filter(|t| t.read_only) else {
        return Ok(Read::Reason("not-offered"));
    };
    let c = if lower.is_finite() {
        lower
    } else if score.is_finite() {
        score
    } else if confidence.is_finite() {
        confidence
    } else {
        0.0 // null >= min reads null as 0
    };
    // `!(confidence >= min)`: NaN on either side fails.
    if c.partial_cmp(&min)
        .is_none_or(|o| o == std::cmp::Ordering::Less)
    {
        return Ok(Read::Reason("low-confidence"));
    }
    let mut kind = None;
    for (k, names) in boxes {
        work.charge(names.len())?;
        if !k.iter().copied().eq("drive".encode_utf16())
            && names.iter().flatten().any(|n| n == name)
        {
            kind = Some(String::from_utf16_lossy(k));
            break;
        }
    }
    let args = match kind {
        Some(k) => derive(pats, &k, tool, message, now, Stage::Decision, work)?,
        None => None,
    };
    Ok(Read::Decision(match args {
        Some(args) => Decision::Prefetch {
            tool: name.clone(),
            args,
        },
        None => Decision::Require { tool: name.clone() },
    }))
}

// ── Wire ────────────────────────────────────────────────────────────────────

fn string(v: &Value) -> R<Vec<u16>> {
    v.as_str().map(<[u16]>::to_vec).ok_or(Refusal::Input)
}

fn opt_strings(v: &Value) -> R<Vec<Option<Vec<u16>>>> {
    match v {
        Value::Arr(items) => items
            .iter()
            .map(|i| match i {
                Value::Str(s) => Ok(Some(s.clone())),
                Value::Null => Ok(None),
                _ => Err(Refusal::Input),
            })
            .collect(),
        _ => Err(Refusal::Input),
    }
}

fn boolean(v: Option<&Value>) -> R<bool> {
    match v {
        Some(Value::Bool(b)) => Ok(*b),
        _ => Err(Refusal::Input),
    }
}

fn keys_of(v: Option<&Value>) -> R<u8> {
    let Some(Value::Arr(items)) = v else {
        return Err(Refusal::Input);
    };
    let mut bits = 0;
    for i in items {
        let s = i.as_str().ok_or(Refusal::Input)?;
        let k = Key::ALL
            .iter()
            .find(|k| s.iter().copied().eq(k.name().encode_utf16()))
            .ok_or(Refusal::Input)?;
        bits |= k.bit();
    }
    Ok(bits)
}

fn tool_of(v: &Value, work: &mut Work) -> R<Tool> {
    work.charge(0)?;
    Ok(Tool {
        name: v.get("name").map(string).ok_or(Refusal::Input)??,
        read_only: boolean(v.get("readOnly"))?,
        description: v.get("description").map(string).ok_or(Refusal::Input)??,
        own: keys_of(v.get("own"))?,
        truthy: keys_of(v.get("truthy"))?,
        any_props: boolean(v.get("anyProps"))?,
        required: v.get("required").map(opt_strings).ok_or(Refusal::Input)??,
    })
}

fn tools_of(v: &Value, work: &mut Work) -> R<Vec<Tool>> {
    let Value::Arr(items) = v else {
        return Err(Refusal::Input);
    };
    items.iter().map(|t| tool_of(t, work)).collect()
}

fn boxes_from(v: &Value, work: &mut Work) -> R<Boxes> {
    let Value::Arr(items) = v else {
        return Err(Refusal::Input);
    };
    let mut out = Vec::with_capacity(items.len());
    for pair in items {
        work.charge(0)?;
        let Value::Arr(kv) = pair else {
            return Err(Refusal::Input);
        };
        let [k, names] = kv.as_slice() else {
            return Err(Refusal::Input);
        };
        out.push((string(k)?, opt_strings(names)?));
    }
    Ok(out)
}

fn push_decision(out: &mut Vec<u8>, d: &Decision) {
    out.extend_from_slice(b"{\"tool\":");
    json::push_str(out, d.tool());
    match d {
        Decision::Require { .. } => out.extend_from_slice(b",\"mode\":\"require\"}"),
        Decision::Prefetch { args, .. } => {
            out.extend_from_slice(b",\"mode\":\"prefetch\",\"args\":");
            match args {
                Args::Empty => out.extend_from_slice(b"{}"),
                Args::One(k, v) => {
                    out.push(b'{');
                    json::push_ascii(out, k.name());
                    out.push(b':');
                    if *k == Key::Urls {
                        out.push(b'[');
                        json::push_str(out, v);
                        out.push(b']');
                    } else {
                        json::push_str(out, v);
                    }
                    out.push(b'}');
                }
            }
            out.push(b'}');
        }
    }
}

fn limits_of(v: &Value) -> R<Option<Limits>> {
    match v {
        Value::Null => Ok(None),
        Value::Obj(_) => {
            let max_options = match v.get("maxOptions") {
                Some(Value::Num(n)) if n.is_finite() => *n,
                _ => return Err(Refusal::Input),
            };
            let max_label = number(v.get("maxLabelChars").ok_or(Refusal::Input)?)?;
            let max_choice = number(v.get("maxChoiceChars").ok_or(Refusal::Input)?)?;
            if max_label.is_nan() || max_choice.is_nan() || max_label < 1.0 {
                return Err(Refusal::Input);
            }
            Ok(Some(Limits {
                max_options,
                max_label,
                max_choice,
            }))
        }
        _ => Err(Refusal::Input),
    }
}

fn texts(v: &Value) -> R<Vec<Vec<u16>>> {
    match v {
        Value::Arr(items) => items.iter().map(string).collect(),
        _ => Err(Refusal::Input),
    }
}

fn run(op: u8, args: &[Value], work: &mut Work) -> R<Vec<u8>> {
    let pats = Patterns::new();
    let mut out = Vec::new();
    match (op, args) {
        (1, [m, tools, now, boxes]) => {
            let message = string(m)?;
            let tools = tools_of(tools, work)?;
            let offered = Offered::new(&tools, work)?;
            let boxes = boxes_from(boxes, work)?;
            match rule(&pats, &message, &offered, &boxes, number(now)?, work)? {
                None => out.extend_from_slice(b"{\"rule\":null}"),
                Some((kind, d)) => {
                    out.extend_from_slice(b"{\"rule\":");
                    json::push_ascii(&mut out, kind);
                    out.extend_from_slice(b",\"decision\":");
                    push_decision(&mut out, &d);
                    out.push(b'}');
                }
            }
        }
        (2, [tools, prefer, hint, limits, boxes]) => {
            let tools = tools_of(tools, work)?;
            let offered = Offered::new(&tools, work)?;
            let prefer = match prefer {
                Value::Null => None,
                v => Some(opt_strings(v)?),
            };
            let hint = match hint {
                Value::Null => None,
                v => Some(string(v)?),
            };
            let boxes = boxes_from(boxes, work)?;
            let (trimmed, opts) = options(
                &offered,
                prefer.as_deref(),
                hint.as_deref(),
                limits_of(limits)?,
                &boxes,
                work,
            )?;
            out.extend_from_slice(format!("{{\"trimmed\":{trimmed},\"options\":[").as_bytes());
            for (k, o) in opts.iter().enumerate() {
                if k > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(b"{\"id\":");
                json::push_str(&mut out, &o.id);
                out.extend_from_slice(b",\"label\":");
                json::push_str(&mut out, &o.label);
                out.push(b'}');
            }
            out.extend_from_slice(b"]}");
        }
        (3, [m, tools, now, boxes, selected, lower, score, conf, min]) => {
            let message = string(m)?;
            let tools = tools_of(tools, work)?;
            let offered = Offered::new(&tools, work)?;
            let boxes = boxes_from(boxes, work)?;
            let selected = match selected {
                Value::Str(s) => Selected::Name(s.clone()),
                Value::Null => Selected::Nothing,
                Value::Bool(true) => Selected::Other,
                _ => return Err(Refusal::Input),
            };
            let read = stage2(
                &pats,
                &message,
                &offered,
                &boxes,
                number(now)?,
                &selected,
                (number(lower)?, number(score)?, number(conf)?),
                number(min)?,
                work,
            )?;
            match read {
                Read::Reason(r) => {
                    out.extend_from_slice(b"{\"reason\":");
                    json::push_ascii(&mut out, r);
                    out.push(b'}');
                }
                Read::Decision(d) => {
                    out.extend_from_slice(b"{\"decision\":");
                    push_decision(&mut out, &d);
                    out.push(b'}');
                }
            }
        }
        (4, [ms]) => {
            let ms = texts(ms)?;
            out.extend_from_slice(b"{\"queries\":[");
            for (k, m) in ms.iter().enumerate() {
                if k > 0 {
                    out.push(b',');
                }
                json::push_str(&mut out, &search_query(&pats, m, work)?);
            }
            out.extend_from_slice(b"],\"prefetchable\":[");
            for (k, m) in ms.iter().enumerate() {
                if k > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(if prefetchable(m, work)? {
                    b"true"
                } else {
                    b"false"
                });
            }
            out.extend_from_slice(b"]}");
        }
        (5, [ms, now]) => {
            let now = number(now)?;
            out.extend_from_slice(b"{\"months\":[");
            for (k, m) in texts(ms)?.iter().enumerate() {
                if k > 0 {
                    out.push(b',');
                }
                match diary_month(&pats, m, now, work)? {
                    Some(v) => json::push_str(&mut out, &v),
                    None => out.extend_from_slice(b"null"),
                }
            }
            out.extend_from_slice(b"]}");
        }
        (6, [urls]) => {
            out.extend_from_slice(b"{\"public\":[");
            for (k, u) in texts(urls)?.iter().enumerate() {
                if k > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(if public_url(u, work)? {
                    b"true"
                } else {
                    b"false"
                });
            }
            out.extend_from_slice(b"]}");
        }
        _ => return Err(Refusal::Input),
    }
    Ok(out)
}

/// One request: `u8(op)` and UTF-8 JSON (an array of arguments). A tool is the projection
/// `{"name","readOnly","description","own","truthy","anyProps","required"}` (see [`Tool`]); boxes
/// are `[[kind, [name|null, …]], …]`; a number is a JSON number, `null` (NaN) or one of
/// `"Infinity"`, `"-Infinity"`, `"NaN"`.
///
/// - op 1 `[message, tools, now, boxes]` → `{"rule":null}` or
///   `{"rule":kind,"decision":{"tool","mode":"require"}|{"tool","mode":"prefetch","args":{…}}}`;
/// - op 2 `[tools, prefer|null, hint|null, limits|null, boxes]` →
///   `{"trimmed":n,"options":[{"id","label"},…]}` (limits `{maxOptions, maxLabelChars,
///   maxChoiceChars}` as the JS reads them);
/// - op 3 `[message, tools, now, boxes, selected (string|null|true), lower, score, confidence,
///   minConfidence]` → `{"reason":"none"|"not-offered"|"low-confidence"}` or `{"decision":…}`;
/// - op 4 `[messages]` → `{"queries":[…],"prefetchable":[…]}`;
/// - op 5 `[messages, now]` → `{"months":[month|null,…]}`;
/// - op 6 `[urls]` → `{"public":[…]}`.
///
/// Status 0 and the reply, or status 1 and `{"error":"input"|"too_large"}`.
pub fn call(input: &[u8]) -> (u32, Vec<u8>) {
    let refuse = |r: Refusal| (1, r.json().as_bytes().to_vec());
    if input.len() > MAX_INPUT_BYTES {
        return refuse(Refusal::TooLarge);
    }
    let Some((&op, body)) = input.split_first() else {
        return refuse(Refusal::Input);
    };
    let mut work = Work::for_request(input.len());
    let Some(Value::Arr(args)) = json::parse_utf8(body, JSON_CAP) else {
        return refuse(Refusal::Input);
    };
    match run(op, &args, &mut work) {
        Ok(out) => (0, out),
        Err(e) => refuse(e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn w() -> Work {
        Work::new(1 << 30)
    }
    fn s(u: &[u16]) -> String {
        String::from_utf16(u).unwrap()
    }

    #[test]
    fn queries() {
        let p = Patterns::new();
        let q = |m: &str| s(&search_query(&p, &units(m), &mut w()).unwrap());
        assert_eq!(
            q("Can you search the web for the latest news???"),
            "the latest news"
        );
        assert_eq!(q("please"), "please");
        assert_eq!(q("What's the weather in Paris?"), "the weather in Paris");
        assert_eq!(q("search forward planning"), "forward planning");
        assert_eq!(q("hi"), "hi");
    }

    #[test]
    fn months() {
        let p = Patterns::new();
        let now = 1_788_000_000_000.0; // 2026-08-29
        let m = |t: &str| {
            diary_month(&p, &units(t), now, &mut w())
                .unwrap()
                .map(|u| s(&u))
        };
        assert_eq!(m("on 2026-09-01").as_deref(), Some("2026-09"));
        assert_eq!(m("on 3 March").as_deref(), Some("2026-03"));
        assert_eq!(m("march 2024").as_deref(), Some("2024-03"));
        assert_eq!(m("May I ask"), None);
        assert_eq!(m("yesterday").as_deref(), Some("2026-08"));
        assert_eq!(m("today").as_deref(), Some("2026-08"));
        assert_eq!(
            diary_month(&p, &units("on 3 march"), f64::NAN, &mut w())
                .unwrap()
                .map(|u| s(&u))
                .as_deref(),
            Some("NaN-03")
        );
        assert_eq!(month_key(f64::NAN), "NaN-NaN");
        assert_eq!(month_key(-1.0), "1969-12");
        assert_eq!(month_key(-8.64e15), "-271821-04");
        assert_eq!(month_key(8.64e15), "275760-09");
    }

    #[test]
    fn urls() {
        let pu = |u: &str| public_url(&units(u), &mut w()).unwrap();
        assert!(pu("https://example.com/a"));
        assert!(!pu("http://localhost/"));
        assert!(!pu("http://nas.local./"));
        assert!(!pu("http://nas.local../")); // noevia#1224
        assert!(!pu("http://10.0.0.1../x"));
        assert!(!pu("http://a..b.com/"));
        assert!(!pu("http://intranet/"));
        assert!(!pu("http://10.0.0.1/"));
        assert!(!pu("http://user@example.com/"));
        assert!(!pu("https://ex\u{e4}mple.com/"));
        assert!(!pu("https://xn--exmple-cua.com/"));
        assert!(!pu("https://a.home.arpa/"));
        assert!(pu("https://home.arpa.example.com/"));
    }
}
