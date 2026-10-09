//! noevia-core's `server/browser-policy.cjs` in Rust, exported from `dav-parse.wasm`
//! (BROWSER_POLICY_IMPL): what noevia decides before every browser action, from the facts the
//! executor read off the real page (never from the model's description of it).
//!
//! - [`classify`]: `classifyAction`, `allow`, `needs_approval` or `blocked` for one action.
//! - [`navigation`]: `checkNavigation`, where the browser may go (http(s) only, no embedded
//!   credentials, no local names, no private IP literals, only allowlisted hosts).
//! - [`substitute`]: the decision half of `substituteSecrets`: which `{{secret:name}}`
//!   placeholders may be filled on an origin, or why not. The port never sees a secret value; the
//!   host keeps doing the substitution itself.
//! - [`fold`]: the label folding (`NFKD`, U+0300..U+036F removed, `toLowerCase`, `\s+` to one
//!   space, trimmed) the consequential-control check reads.
//!
//! The host (browser-policy.cjs under BROWSER_POLICY_IMPL=wasm) computes the JS answer first. An
//! action is allowed only if both allow it; it asks if either asks; it is blocked if either blocks;
//! an unknown, a refusal or a fault asks (or blocks, where the JS blocks or the caller has no
//! approval step). So wherever the port cannot be sure it says so ([`Status`] `None`).
//!
//! # Where the port is stricter than the JS (by design)
//!
//! - **A `<button>` in a form with a `type` other than `button` or `reset`** (`type="x"`, a typo,
//!   any invalid value) submits the form in HTML (the invalid value default is the Submit Button
//!   state); the JS only treats an empty or `submit` type as submitting. The port says
//!   "Submits a form." (noevia#1218.)
//! - **Local names with trailing dots.** `corp.internal.` and `localhost.` are the same names as
//!   `corp.internal` and `localhost`; the JS checks the local suffixes on the hostname as written,
//!   so an allowlisted local domain is reachable with a trailing dot. The port checks the name with
//!   its trailing dots removed too. (noevia#1219.)
//! - **Text outside [`KNOWN_RANGES`]** (a lone surrogate, or a code point outside the Latin, Greek,
//!   Cyrillic, punctuation, kana, CJK, Hangul, fullwidth and emoji blocks listed) in a label, tag,
//!   type, role or key that the decision reads: [`fold`] gives no answer and the action is
//!   unknown (the host asks). Inside the table the port folds with ICU4X's NFKD and the standard
//!   library's lowercasing; noevia-core's differential test checks every code point of the table,
//!   alone and between letters, against the runtime's own ICU.
//! - **A non-GET method with non-ASCII text** counts as not GET (exact: no non-ASCII text
//!   uppercases to `GET`).
//! - Requests over [`MAX_INPUT_BYTES`] (`too_large`), and requests that would need more than
//!   their work budget (`too_large`), which the host treats as a fault.
//!
//! URLs are parsed with the `url` crate (the WHATWG URL Standard, like Node's). Where the two
//! parsers disagree, the host sees a mismatch and takes the stricter answer.
//!
//! Linear in the input: every loop is charged to a work budget proportional to the request size
//! ([`WORK_PER_BYTE`]), recursion is one level (a key press judged as a click), no panics.

#![forbid(unsafe_code)]

use prompt_framing::js::{is_js_space, lossy, to_lower, trim, units};
use prompt_framing::json::{self, Value};

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024 + 1;

/// Work units a request may use per byte of its size (plus [`WORK_FLOOR`]). Every algorithm here
/// is linear with a small constant, so this is a backstop that well-formed requests never reach.
pub const WORK_PER_BYTE: u64 = 128;
/// Work units every request may use, whatever its size.
pub const WORK_FLOOR: u64 = 1 << 16;

/// JSON nesting kept: the outer array, the projections, and their arrays.
const JSON_CAP: usize = 4;

/// `CONSEQUENTIAL`, in the JS's order.
pub const CONSEQUENTIAL: [&str; 61] = [
    // en
    "send",
    "submit",
    "pay",
    "buy",
    "purchase",
    "order",
    "checkout",
    "check out",
    "delete",
    "remove",
    "publish",
    "post",
    "confirm",
    "save settings",
    "transfer",
    "subscribe",
    "unsubscribe",
    "sign up",
    "place order",
    "book",
    "reserve",
    "donate",
    "merge",
    "push",
    "deploy",
    "approve",
    "accept",
    "agree",
    // de
    "senden",
    "absenden",
    "bezahlen",
    "kaufen",
    "bestellen",
    "loschen",
    "entfernen",
    "veroffentlichen",
    "bestatigen",
    "speichern",
    "uberweisen",
    "zustimmen",
    "akzeptieren",
    // fr
    "envoyer",
    "payer",
    "acheter",
    "commander",
    "supprimer",
    "publier",
    "confirmer",
    "enregistrer",
    "valider",
    "accepter",
    // es
    "enviar",
    "pagar",
    "comprar",
    "pedir",
    "eliminar",
    "borrar",
    "publicar",
    "confirmar",
    "guardar",
    "aceptar",
];

fn consequential_words() -> impl Iterator<Item = &'static str> {
    CONSEQUENTIAL.iter().copied()
}

const READ_ONLY: [&str; 5] = ["screenshot", "extract", "scroll", "hover", "wait"];
const CONTROL_ROLES: [&str; 8] = [
    "button", "link", "menuitem", "tab", "switch", "checkbox", "radio", "option",
];
const CONTROL_INPUTS: [&str; 6] = ["submit", "image", "button", "reset", "checkbox", "radio"];

/// Code points [`fold`] answers for (inclusive ranges). Outside them, or for a lone surrogate, it
/// gives no answer. Every range was assigned (or left unassigned with no decomposition and no case)
/// long before any ICU noevia runs on; noevia-core's differential test checks each code point.
pub const KNOWN_RANGES: [(u32, u32); 9] = [
    (0x0000, 0x052f), // Latin, IPA, spacing modifiers, combining diacriticals, Greek, Cyrillic
    (0x1e00, 0x1fff), // Latin Extended Additional, Greek Extended
    (0x2000, 0x206f), // General Punctuation
    (0x3000, 0x30ff), // CJK Symbols and Punctuation, Hiragana, Katakana
    (0x4e00, 0x9fff), // CJK Unified Ideographs
    (0xac00, 0xd7a3), // Hangul Syllables
    (0xff01, 0xff9f), // fullwidth ASCII, halfwidth katakana
    (0x1f300, 0x1f6ff), // pictographs, emoticons, transport
    (0x1f900, 0x1faff), // supplemental symbols and pictographs, symbols extended-A
];

/// Whether `cp` is in [`KNOWN_RANGES`].
pub fn is_known(cp: u32) -> bool {
    KNOWN_RANGES.iter().any(|&(a, b)| (a..=b).contains(&cp))
}

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

fn eq_str(s: &[u16], t: &str) -> bool {
    s.iter().copied().eq(t.encode_utf16())
}

fn ends_with_str(s: &[u16], t: &str) -> bool {
    let t: Vec<u16> = t.encode_utf16().collect();
    s.len() >= t.len() && s.get(s.len() - t.len()..) == Some(&t[..])
}

fn cat(parts: &[&[u16]]) -> Vec<u16> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

// ── fold ────────────────────────────────────────────────────────────────────

/// The JS `fold(text)` of a string (the host passes `String(text || '')`): NFKD, U+0300..U+036F
/// removed, `toLowerCase`, every `\s+` run one space, trimmed. `None` when `text` holds a lone
/// surrogate or a code point outside [`KNOWN_RANGES`].
pub fn fold(text: &[u16], work: &mut Work) -> R<Option<Vec<u16>>> {
    work.charge(text.len())?;
    if text.iter().all(|&c| c < 0x80) {
        // NFKD is the identity on ASCII and nothing is in U+0300..U+036F.
        return Ok(Some(collapse(&to_lower(text), work)?));
    }
    for r in char::decode_utf16(text.iter().copied()) {
        match r {
            Ok(c) if is_known(u32::from(c)) => {}
            _ => return Ok(None),
        }
    }
    // Every unit is part of a known code point, so the text is well-formed.
    let Ok(whole) = String::from_utf16(text) else {
        return Ok(None);
    };
    let nfkd = units(&icu_normalizer::DecomposingNormalizerBorrowed::new_nfkd().normalize(&whole));
    // NFKD expands a known code point to a handful of units at most; charge what it produced.
    work.charge(nfkd.len())?;
    let stripped: Vec<u16> = nfkd
        .iter()
        .copied()
        .filter(|c| !(0x0300..=0x036f).contains(c))
        .collect();
    let lower = to_lower(&stripped);
    work.charge(lower.len())?;
    Ok(Some(collapse(&lower, work)?))
}

/// `.replace(/\s+/g, ' ').trim()`.
fn collapse(s: &[u16], work: &mut Work) -> R<Vec<u16>> {
    work.charge(s.len())?;
    let mut out = Vec::with_capacity(s.len());
    let mut space = false;
    for &c in s {
        if is_js_space(c) {
            if !space {
                out.push(0x20);
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    Ok(trim(&out).to_vec())
}

const fn is_word_unit(c: u16) -> bool {
    matches!(c, 0x30..=0x39 | 0x61..=0x7a)
}

/// `CONSEQUENTIAL_RE.test(label)` for a folded label (whitespace already single spaces). The
/// regex needs a non-`[a-z0-9]` unit (or an end) on both sides of a word, and every word is made of
/// `[a-z]` and single spaces, so a match is a run of whole `[a-z0-9]+` tokens, one space apart,
/// spelling the word. Linear: tokens are found once and compared with the few words of their
/// length.
pub fn consequential(label: &[u16], work: &mut Work) -> R<bool> {
    work.charge(label.len())?;
    // (start, end) of every maximal [a-z0-9]+ run.
    let mut tokens: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    for (i, &c) in label.iter().enumerate() {
        match (is_word_unit(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                tokens.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        tokens.push((s, label.len()));
    }
    let token = |k: usize| tokens.get(k).and_then(|&(a, b)| label.get(a..b));
    for k in 0..tokens.len() {
        work.charge(CONSEQUENTIAL.len())?;
        'words: for w in consequential_words() {
            let mut at = k;
            for (n, part) in w.split(' ').enumerate() {
                if n > 0 {
                    // The previous token and this one must be exactly one space apart.
                    let (Some(&(_, prev_end)), Some(&(next_start, _))) =
                        (tokens.get(at - 1), tokens.get(at))
                    else {
                        continue 'words;
                    };
                    if next_start != prev_end + 1 || label.get(prev_end) != Some(&0x20) {
                        continue 'words;
                    }
                }
                match token(at) {
                    Some(t) if eq_str(t, part) => at += 1,
                    _ => continue 'words,
                }
            }
            return Ok(true);
        }
    }
    Ok(false)
}

// ── navigation ──────────────────────────────────────────────────────────────

/// `checkNavigation`'s answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Nav {
    /// `{ ok: true, origin }`.
    Ok(Vec<u16>),
    /// `{ ok: false, reason }`.
    Refused(Vec<u16>),
}

/// `/^\*?\./` removed, then one trailing `.`.
fn domain_of(d: &[u16]) -> &[u16] {
    let d = match d {
        [0x2a, 0x2e, rest @ ..] | [0x2e, rest @ ..] => rest,
        _ => d,
    };
    match d {
        [rest @ .., 0x2e] => rest,
        _ => d,
    }
}

/// `hostAllowed(host, allowedDomains)` (the host passes each domain as `String(d)`). Linear: each
/// domain is compared with the host's suffix of its own length.
pub fn host_allowed(host: &[u16], domains: &[Vec<u16>], work: &mut Work) -> R<bool> {
    work.charge(host.len())?;
    let h = to_lower(host);
    let h = match h.as_slice() {
        [rest @ .., 0x2e] => rest.to_vec(),
        _ => h,
    };
    for d in domains {
        work.charge(d.len())?;
        let lower = to_lower(d);
        let dom = domain_of(&lower);
        if dom.is_empty() {
            continue;
        }
        if h == dom {
            return Ok(true);
        }
        if h.len() > dom.len() {
            let cut = h.len() - dom.len();
            if h.get(cut - 1) == Some(&0x2e) && h.get(cut..) == Some(dom) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn is_local(host: &[u16]) -> bool {
    eq_str(host, "localhost")
        || ends_with_str(host, ".localhost")
        || ends_with_str(host, ".internal")
        || ends_with_str(host, ".local")
}

/// `checkNavigation(rawUrl, allowedDomains)` (the host passes `String(rawUrl)` and each domain as
/// `String(d)`), with the port's stricter local-name check (see the crate docs).
pub fn navigation(raw: &[u16], domains: &[Vec<u16>], work: &mut Work) -> R<Nav> {
    work.charge(raw.len())?;
    // WebIDL USVString: a lone surrogate is U+FFFD, as `new URL()` sees it.
    let Ok(url) = url::Url::parse(&lossy(raw)) else {
        return Ok(Nav::Refused(units("Not a web address.")));
    };
    work.charge(url.as_str().len())?;
    let scheme = url.scheme();
    if scheme != "https" && scheme != "http" {
        return Ok(Nav::Refused(units(&format!(
            "{scheme}: links are not opened."
        ))));
    }
    if !url.username().is_empty() || url.password().is_some_and(|p| !p.is_empty()) {
        return Ok(Nav::Refused(units(
            "Addresses with embedded credentials are not opened.",
        )));
    }
    let hostname = url.host_str().unwrap_or("");
    // `.replace(/^\[|\]$/g, '').toLowerCase()`.
    let bare = hostname.strip_prefix('[').unwrap_or(hostname);
    let bare = bare.strip_suffix(']').unwrap_or(bare);
    let host = units(&bare.to_ascii_lowercase());
    // Stricter than the JS: the local suffixes also on the name without its trailing dots.
    let mut end = host.len();
    while end > 0 && host.get(end - 1) == Some(&0x2e) {
        end -= 1;
    }
    let undotted = host.get(..end).unwrap_or(&[]);
    if is_local(&host) || is_local(undotted) {
        return Ok(Nav::Refused(units("Local addresses are not opened.")));
    }
    let ip = matches!(url.host(), Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)));
    if ip && ssrf_policy::is_private_ip(&lossy(&host)) {
        return Ok(Nav::Refused(units(
            "Private network addresses are not opened.",
        )));
    }
    if !host_allowed(&host, domains, work)? {
        return Ok(Nav::Refused(cat(&[
            &host,
            &units(" is not on this task\u{2019}s allowed list."),
        ])));
    }
    Ok(Nav::Ok(units(&url.origin().ascii_serialization())))
}

// ── classifyAction ──────────────────────────────────────────────────────────

/// `'allow'`, `'needs_approval'` or `'blocked'`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decision {
    Allow,
    NeedsApproval,
    Blocked,
}

impl Decision {
    /// The JS string.
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::NeedsApproval => "needs_approval",
            Decision::Blocked => "blocked",
        }
    }
}

/// `None`: the port cannot be sure (the host asks).
pub type Status = Option<Decision>;

/// `classifyAction`'s answer: `{ status, reason }` (an unknown has an empty reason).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub status: Status,
    pub reason: Vec<u16>,
}

fn verdict(d: Decision, reason: &str) -> Verdict {
    Verdict {
        status: Some(d),
        reason: units(reason),
    }
}

fn unknown() -> Verdict {
    Verdict {
        status: None,
        reason: Vec::new(),
    }
}

/// The host's projection of a browser action.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Action {
    /// `String(action.type || '')`.
    pub kind: Vec<u16>,
    /// `String(action.url)` (navigate only).
    pub url: Vec<u16>,
    /// `String(action.method || 'GET')` (navigate only).
    pub method: Vec<u16>,
    /// `String(action.key ?? '')` (press only).
    pub key: Vec<u16>,
}

/// The host's projection of `action.element || {}`: each text field `v ? String(v) : ''`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Element {
    pub tag: Vec<u16>,
    pub kind: Vec<u16>,
    pub role: Vec<u16>,
    pub name: Vec<u16>,
    pub text: Vec<u16>,
    pub value: Vec<u16>,
    /// `!!el.inForm`.
    pub in_form: bool,
}

/// `page.origin`: falsy, a string, or a truthy non-string (never equal to an origin).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Falsy,
    Str(Vec<u16>),
    Other,
}

/// The host's projection of `page`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub origin: Origin,
    /// `(page.allowedDomains || []).map(String)` (navigate only).
    pub allowed_domains: Vec<Vec<u16>>,
}

impl Default for Page {
    fn default() -> Self {
        Page {
            origin: Origin::Falsy,
            allowed_domains: Vec::new(),
        }
    }
}

/// `label.slice(0, 60)` in code units.
fn head(label: &[u16], n: usize) -> &[u16] {
    label.get(..n.min(label.len())).unwrap_or(&[])
}

fn click(el: &Element, work: &mut Work) -> R<Verdict> {
    let (Some(tag), Some(kind)) = (fold(&el.tag, work)?, fold(&el.kind, work)?) else {
        return Ok(unknown());
    };
    // HTML: a <button>'s missing or invalid type is the Submit Button state; only `button` and
    // `reset` do not submit. (The JS reads only '' and 'submit': stricter here.)
    let button_submits = !eq_str(&kind, "button") && !eq_str(&kind, "reset");
    let submits = eq_str(&kind, "submit")
        || (eq_str(&tag, "input") && eq_str(&kind, "image"))
        || (eq_str(&tag, "button") && el.in_form && button_submits);
    if submits {
        return Ok(verdict(Decision::NeedsApproval, "Submits a form."));
    }
    // [el.name, el.text, el.value].filter(Boolean).join(' ')
    let parts: Vec<&[u16]> = [&el.name, &el.text, &el.value]
        .into_iter()
        .filter(|p| !p.is_empty())
        .map(Vec::as_slice)
        .collect();
    let mut joined = Vec::new();
    for (k, p) in parts.iter().enumerate() {
        if k > 0 {
            joined.push(0x20);
        }
        joined.extend_from_slice(p);
    }
    let Some(label) = fold(&joined, work)? else {
        return Ok(unknown());
    };
    if consequential(&label, work)? {
        return Ok(Verdict {
            status: Some(Decision::NeedsApproval),
            reason: cat(&[
                &units("\u{201c}"),
                head(&label, 60),
                &units("\u{201d} looks consequential."),
            ]),
        });
    }
    Ok(verdict(Decision::Allow, ""))
}

fn press(action: &Action, el: &Element, work: &mut Work) -> R<Verdict> {
    let raw = &action.key;
    work.charge(raw.len())?;
    let key: Vec<u16> = if !raw.is_empty() && trim(raw).is_empty() {
        units("space")
    } else {
        let Some(folded) = fold(raw, work)? else {
            return Ok(unknown());
        };
        folded.into_iter().filter(|&c| !is_js_space(c)).collect()
    };
    let last = key.rsplit(|&c| c == 0x2b).next().unwrap_or(&[]);
    let enter = ends_with_str(last, "enter") || eq_str(last, "return");
    let activates = enter || eq_str(last, "space") || key.last() == Some(&0x2b);
    let (Some(tag), Some(kind), Some(role)) = (
        fold(&el.tag, work)?,
        fold(&el.kind, work)?,
        fold(&el.role, work)?,
    ) else {
        return Ok(unknown());
    };
    let control = ["button", "a", "summary"].iter().any(|t| eq_str(&tag, t))
        || CONTROL_ROLES.iter().any(|r| eq_str(&role, r))
        || (eq_str(&tag, "input") && CONTROL_INPUTS.iter().any(|k| eq_str(&kind, k)));
    if activates && control {
        return click(el, work);
    }
    if enter && el.in_form {
        return Ok(verdict(Decision::NeedsApproval, "Enter submits the form."));
    }
    Ok(verdict(Decision::Allow, ""))
}

/// `classifyAction(action, page)` over the host's projections.
pub fn classify(action: &Action, el: &Element, page: &Page, work: &mut Work) -> R<Verdict> {
    let kind = &action.kind;
    work.charge(kind.len())?;
    if READ_ONLY.iter().any(|t| eq_str(kind, t)) {
        return Ok(verdict(Decision::Allow, "Read-only."));
    }
    if eq_str(kind, "navigate") {
        let origin = match navigation(&action.url, &page.allowed_domains, work)? {
            Nav::Refused(reason) => {
                return Ok(Verdict {
                    status: Some(Decision::Blocked),
                    reason,
                })
            }
            Nav::Ok(origin) => origin,
        };
        work.charge(action.method.len())?;
        // String(method).toUpperCase() !== 'GET': no non-ASCII text uppercases to ASCII G, E, T
        // alone, so a method with any non-ASCII unit is not GET.
        let get = action.method.len() == 3
            && action
                .method
                .iter()
                .zip("GET".bytes())
                .all(|(&c, g)| c < 0x80 && (c & !0x20) == u16::from(g));
        let post = !get;
        let elsewhere = match &page.origin {
            Origin::Falsy => false,
            Origin::Str(o) => *o != origin,
            Origin::Other => true,
        };
        if post && elsewhere {
            return Ok(verdict(
                Decision::NeedsApproval,
                "Sends data to another site.",
            ));
        }
        if post {
            return Ok(verdict(Decision::NeedsApproval, "Sends data."));
        }
        return Ok(verdict(Decision::Allow, ""));
    }
    if eq_str(kind, "upload") {
        return Ok(verdict(Decision::NeedsApproval, "Uploads a file."));
    }
    if eq_str(kind, "submit") {
        return Ok(verdict(Decision::NeedsApproval, "Submits a form."));
    }
    if eq_str(kind, "click") {
        return click(el, work);
    }
    if eq_str(kind, "press") {
        return press(action, el, work);
    }
    if eq_str(kind, "type") || eq_str(kind, "select") {
        return Ok(verdict(
            Decision::Allow,
            "Nothing is sent until a submit, which asks.",
        ));
    }
    let shown: &[u16] = if kind.is_empty() {
        &[0x6e, 0x6f, 0x6e, 0x65]
    } else {
        kind
    };
    Ok(Verdict {
        status: Some(Decision::NeedsApproval),
        reason: cat(&[
            &units("Unrecognised action \u{201c}"),
            shown,
            &units("\u{201d}."),
        ]),
    })
}

// ── substituteSecrets (the decision) ────────────────────────────────────────

/// `substituteSecrets`'s decision: `Ok(names used, in order)` or `Err(reason)`.
pub type Substitution = Result<Vec<Vec<u16>>, Vec<u16>>;

const fn is_name_unit(c: u16) -> bool {
    matches!(c, 0x30..=0x39 | 0x41..=0x5a | 0x61..=0x7a | 0x5f | 0x2e | 0x2d)
}

/// The `{{secret:name}}` placeholders of `text`, as the global regex finds them (leftmost, not
/// overlapping): the name's range each.
fn placeholders(text: &[u16], work: &mut Work) -> R<Vec<(usize, usize)>> {
    const OPEN: &[u16] = &[0x7b, 0x7b, 0x73, 0x65, 0x63, 0x72, 0x65, 0x74, 0x3a]; // {{secret:
    work.charge(text.len())?;
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text.get(i..i + OPEN.len()) != Some(OPEN) {
            i += 1;
            continue;
        }
        let start = i + OPEN.len();
        let mut end = start;
        while end < text.len()
            && end - start < 65
            && text.get(end).is_some_and(|&c| is_name_unit(c))
        {
            end += 1;
        }
        work.charge(end - start + OPEN.len())?;
        let len = end - start;
        if (1..=64).contains(&len) && text.get(end..end + 2) == Some(&[0x7d, 0x7d][..]) {
            out.push((start, end));
            i = end + 2;
        } else {
            i += 1;
        }
    }
    Ok(out)
}

/// The decision half of `substituteSecrets(text, secrets, origin)`. `secrets` lists, for each own
/// property of the secrets object whose value is truthy, its name and `(secret.domains || [])`
/// as strings; the port never sees a value.
pub fn substitute(
    text: &[u16],
    secrets: &[(Vec<u16>, Vec<Vec<u16>>)],
    origin: &[u16],
    work: &mut Work,
) -> R<Substitution> {
    work.charge(origin.len())?;
    let Ok(url) = url::Url::parse(&lossy(origin)) else {
        return Ok(Err(units("No page origin.")));
    };
    let host = units(url.host_str().unwrap_or(""));
    work.charge(secrets.len())?;
    let mut used = Vec::new();
    let mut failure: Option<Vec<u16>> = None;
    for (a, b) in placeholders(text, work)? {
        let name = text.get(a..b).unwrap_or(&[]);
        work.charge(secrets.len())?;
        match secrets.iter().find(|(n, _)| n.as_slice() == name) {
            None => {
                if failure.is_none() {
                    failure = Some(cat(&[&units("No secret named "), name, &units(".")]));
                }
            }
            Some((_, domains)) => {
                if host_allowed(&host, domains, work)? {
                    used.push(name.to_vec());
                } else if failure.is_none() {
                    failure = Some(cat(&[
                        &units("The "),
                        name,
                        &units(" secret is not for "),
                        &host,
                        &units("."),
                    ]));
                }
            }
        }
    }
    Ok(match failure {
        Some(reason) => Err(reason),
        None => Ok(used),
    })
}

// ── the wasm call ───────────────────────────────────────────────────────────

fn string(v: &Value) -> R<Vec<u16>> {
    v.as_str().map(<[u16]>::to_vec).ok_or(Refusal::Input)
}

fn strings(v: &Value) -> R<Vec<Vec<u16>>> {
    match v {
        Value::Arr(items) => items.iter().map(string).collect(),
        _ => Err(Refusal::Input),
    }
}

fn field(o: &Value, key: &str) -> R<Vec<u16>> {
    match o.get(key) {
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(Value::Null) => Ok(Vec::new()),
        _ => Err(Refusal::Input),
    }
}

fn exact_keys(o: &Value, keys: &[&str]) -> bool {
    match o {
        Value::Obj(m) => m.len() == keys.len() && keys.iter().all(|k| o.get(k).is_some()),
        _ => false,
    }
}

fn action_of(v: &Value) -> R<Action> {
    if !exact_keys(v, &["type", "url", "method", "key"]) {
        return Err(Refusal::Input);
    }
    Ok(Action {
        kind: field(v, "type")?,
        url: field(v, "url")?,
        method: field(v, "method")?,
        key: field(v, "key")?,
    })
}

fn element_of(v: &Value) -> R<Element> {
    if !exact_keys(
        v,
        &["tag", "type", "role", "name", "text", "value", "inForm"],
    ) {
        return Err(Refusal::Input);
    }
    let Some(Value::Bool(in_form)) = v.get("inForm") else {
        return Err(Refusal::Input);
    };
    Ok(Element {
        tag: field(v, "tag")?,
        kind: field(v, "type")?,
        role: field(v, "role")?,
        name: field(v, "name")?,
        text: field(v, "text")?,
        value: field(v, "value")?,
        in_form: *in_form,
    })
}

fn page_of(v: &Value) -> R<Page> {
    if !exact_keys(v, &["origin", "allowedDomains"]) {
        return Err(Refusal::Input);
    }
    let origin = match v.get("origin") {
        Some(Value::Str(s)) if s.is_empty() => Origin::Falsy,
        Some(Value::Str(s)) => Origin::Str(s.clone()),
        Some(Value::Bool(false)) => Origin::Falsy,
        Some(Value::Null) => Origin::Other,
        _ => return Err(Refusal::Input),
    };
    Ok(Page {
        origin,
        allowed_domains: strings(v.get("allowedDomains").ok_or(Refusal::Input)?)?,
    })
}

fn push_reason(out: &mut Vec<u8>, reason: &[u16]) {
    out.extend_from_slice(b"\"reason\":");
    json::push_str(out, reason);
}

fn run(op: u8, args: &[Value], work: &mut Work) -> R<Vec<u8>> {
    let mut out = Vec::new();
    match (op, args) {
        (1, [a, e, p]) => {
            let v = classify(&action_of(a)?, &element_of(e)?, &page_of(p)?, work)?;
            out.extend_from_slice(b"{\"status\":");
            match v.status {
                Some(d) => json::push_ascii(&mut out, d.as_str()),
                None => out.extend_from_slice(b"null"),
            }
            out.push(b',');
            push_reason(&mut out, &v.reason);
            out.push(b'}');
        }
        (2, [u, d]) => match navigation(&string(u)?, &strings(d)?, work)? {
            Nav::Ok(origin) => {
                out.extend_from_slice(b"{\"ok\":true,\"origin\":");
                json::push_str(&mut out, &origin);
                out.push(b'}');
            }
            Nav::Refused(reason) => {
                out.extend_from_slice(b"{\"ok\":false,");
                push_reason(&mut out, &reason);
                out.push(b'}');
            }
        },
        (3, [t, Value::Arr(pairs), o]) => {
            let mut secrets = Vec::with_capacity(pairs.len());
            for pair in pairs {
                let Value::Arr(kv) = pair else {
                    return Err(Refusal::Input);
                };
                let [name, domains] = kv.as_slice() else {
                    return Err(Refusal::Input);
                };
                secrets.push((string(name)?, strings(domains)?));
            }
            match substitute(&string(t)?, &secrets, &string(o)?, work)? {
                Ok(used) => {
                    out.extend_from_slice(b"{\"ok\":true,\"used\":[");
                    for (k, n) in used.iter().enumerate() {
                        if k > 0 {
                            out.push(b',');
                        }
                        json::push_str(&mut out, n);
                    }
                    out.extend_from_slice(b"]}");
                }
                Err(reason) => {
                    out.extend_from_slice(b"{\"ok\":false,");
                    push_reason(&mut out, &reason);
                    out.push(b'}');
                }
            }
        }
        (4, [texts]) => {
            out.extend_from_slice(b"{\"folded\":[");
            for (k, t) in strings(texts)?.iter().enumerate() {
                if k > 0 {
                    out.push(b',');
                }
                match fold(t, work)? {
                    Some(f) => json::push_str(&mut out, &f),
                    None => out.extend_from_slice(b"null"),
                }
            }
            out.extend_from_slice(b"]}");
        }
        _ => return Err(Refusal::Input),
    }
    Ok(out)
}

/// One request: `u8(op)` and UTF-8 JSON (an array of arguments):
///
/// - op 1 `[action, element, page]` (the projections above; `null` text fields are empty) →
///   `{"status":"allow"|"needs_approval"|"blocked"|null,"reason":…}` (`null`: unknown);
/// - op 2 `[url, domains]` → `{"ok":true,"origin":…}` or `{"ok":false,"reason":…}`;
/// - op 3 `[text, [[name, domains], …], origin]` → `{"ok":true,"used":[…]}` or
///   `{"ok":false,"reason":…}`;
/// - op 4 `[texts]` → `{"folded":[text|null, …]}`.
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

    fn s(u: &[u16]) -> String {
        String::from_utf16(u).unwrap()
    }
    fn w() -> Work {
        Work::new(1 << 30)
    }
    fn f(t: &str) -> Option<String> {
        fold(&units(t), &mut w()).unwrap().map(|u| s(&u))
    }
    fn el(tag: &str, kind: &str, text: &str, in_form: bool) -> Element {
        Element {
            tag: units(tag),
            kind: units(kind),
            text: units(text),
            in_form,
            ..Element::default()
        }
    }
    fn act(kind: &str) -> Action {
        Action {
            kind: units(kind),
            ..Action::default()
        }
    }
    fn status(a: &Action, e: &Element) -> Status {
        classify(a, e, &Page::default(), &mut w()).unwrap().status
    }

    #[test]
    fn list_matches_js() {
        assert_eq!(consequential_words().count(), 61);
    }

    #[test]
    fn folding() {
        assert_eq!(f("  Löschen\t\nJetzt ").as_deref(), Some("loschen jetzt"));
        assert_eq!(f("Ｓｅｎｄ").as_deref(), Some("send"));
        assert_eq!(f("Bestätigen").as_deref(), Some("bestatigen"));
        assert_eq!(f("\u{130}").as_deref(), Some("i"));
        assert_eq!(f("ΣΑΣ").as_deref(), Some("σας"));
        assert_eq!(f("x\u{a0}\u{2003}y").as_deref(), Some("x y"));
        assert_eq!(fold(&[0x61, 0xd800], &mut w()).unwrap(), None);
        assert_eq!(f("\u{fb01}"), None);
        assert_eq!(f("🛒 Cart").as_deref(), Some("🛒 cart"));
    }

    #[test]
    fn consequential_words_whole() {
        let c = |t: &str| consequential(&units(&f(t).unwrap()), &mut w()).unwrap();
        assert!(c("Place order"));
        assert!(c("check out now"));
        assert!(!c("checkout2"));
        assert!(c("x-send-y"));
        assert!(!c("Sender details"));
        assert!(!c("check, out"));
        assert!(c("sign\u{a0}up"));
        assert!(!c("Posts"));
    }

    #[test]
    fn clicks() {
        let click = act("click");
        assert_eq!(
            status(&click, &el("button", "", "Next", true)),
            Some(Decision::NeedsApproval)
        );
        assert_eq!(
            status(&click, &el("button", "button", "Show", true)),
            Some(Decision::Allow)
        );
        // Stricter than the JS: an invalid type submits.
        assert_eq!(
            status(&click, &el("button", "xyz", "Next", true)),
            Some(Decision::NeedsApproval)
        );
        assert_eq!(
            status(&click, &el("a", "", "Löschen", false)),
            Some(Decision::NeedsApproval)
        );
        assert_eq!(status(&click, &el("a", "", "\u{fb01}", false)), None);
    }

    #[test]
    fn presses() {
        let mut p = act("press");
        p.key = units(" ");
        assert_eq!(
            status(&p, &el("button", "", "Delete account", false)),
            Some(Decision::NeedsApproval)
        );
        p.key = units("Shift+Enter");
        assert_eq!(
            status(&p, &el("input", "text", "", true)),
            Some(Decision::NeedsApproval)
        );
        p.key = units("Tab");
        assert_eq!(status(&p, &el("", "", "", true)), Some(Decision::Allow));
        p.key = units("Control++");
        assert_eq!(
            status(&p, &el("a", "", "Send", false)),
            Some(Decision::NeedsApproval)
        );
    }

    #[test]
    fn navigations() {
        let d = vec![units("example.com"), units("*.corp.internal")];
        let nav = |u: &str| navigation(&units(u), &d, &mut w()).unwrap();
        assert_eq!(
            nav("https://a.example.com/x"),
            Nav::Ok(units("https://a.example.com"))
        );
        assert_eq!(
            nav("https://EXAMPLE.com.:8443/"),
            Nav::Ok(units("https://example.com.:8443"))
        );
        for u in [
            "https://corp.internal/",
            "https://corp.internal./",
            "http://localhost./",
            "http://127.0.0.1/",
            "http://[::1]/",
            "https://u:p@example.com/",
            "javascript:alert(1)",
            "not a url",
            "https://example.com.evil.net/",
        ] {
            assert!(matches!(nav(u), Nav::Refused(_)), "{u}");
        }
        assert_eq!(
            nav("javascript:x"),
            Nav::Refused(units("javascript: links are not opened."))
        );
    }

    #[test]
    fn substitutions() {
        let secrets = vec![(units("gh"), vec![units("github.com")])];
        let sub = |t: &str, o: &str| substitute(&units(t), &secrets, &units(o), &mut w()).unwrap();
        assert_eq!(
            sub("a={{secret:gh}}&{{secret:gh}}", "https://api.github.com"),
            Ok(vec![units("gh"), units("gh")])
        );
        assert_eq!(
            sub("{{secret:gh}}", "https://github.com.evil"),
            Err(units("The gh secret is not for github.com.evil."))
        );
        assert_eq!(
            sub("{{secret:nope}}{{secret:gh}}", "https://github.com"),
            Err(units("No secret named nope."))
        );
        assert_eq!(sub("{{secret:gh}}", "nope"), Err(units("No page origin.")));
        assert_eq!(
            sub("{{{secret:gh}}}", "https://github.com"),
            Ok(vec![units("gh")])
        );
        let long = format!("{{{{secret:{}}}}}", "a".repeat(65));
        assert_eq!(sub(&long, "https://github.com"), Ok(vec![]));
    }

    #[test]
    fn refusals() {
        assert_eq!(call(&[]).0, 1);
        assert_eq!(call(b"\x01[]").0, 1);
        assert_eq!(call(b"\x09[1]").0, 1);
        let big = vec![b' '; MAX_INPUT_BYTES + 1];
        assert_eq!(
            call(&big),
            (1, Refusal::TooLarge.json().as_bytes().to_vec())
        );
    }
}
