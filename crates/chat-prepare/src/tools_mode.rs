//! noevia-core's `server/tools-mode.cjs` parts a turn's tool list reads (slice P6a):
//! `toolsModeOf`, what Automatic adds for one message (#1550: `wantsWeb`, `automaticBoxesFor`,
//! `automaticEligible`, `automaticAdditions`), what a follow-up carries (#1692:
//! `carriedPublicBoxes`), `orderWithAdditions`, and the private/public rules (#1646/#1592:
//! `isPrivateDataBox`, `isPublicToolName`, `PRIVATE_CORE_TOOLS`). `toolCapableRoute` (the move to
//! a tool-capable role) and `createAutomaticToolsReady` (the account's Nextcloud check, a seam
//! here) are later slices.
//!
//! The patterns are non-Unicode `/i` JS regexes, written here as the JS source they are and
//! translated by [`js_regex`]: ASCII-only case folding (the message is lower-cased in ASCII and
//! the patterns are lower case, so `ſ`, the Kelvin sign and a dotless `ı` never fold to ASCII),
//! ASCII `\b`, `\w` and `\d`, and JS `\s` (U+FEFF is space, U+0085 is not). A lone surrogate
//! (held as a private-use character in `wtf16` text) is neither a word character nor space, in
//! both. One construct the `regex` crate lacks is dropped where it cannot change a match: see
//! `EXPLICIT_WEB`.

use std::sync::OnceLock;

use js_json::{wtf16, JValue};
use regex::Regex;

use crate::history::Entry;
use crate::js;
use crate::toolboxes::Toolbox;

/// `MAX_ADDED`: boxes Automatic adds to one turn at most.
pub const MAX_ADDED: usize = 3;
/// `PUBLIC_BOXES`: boxes holding no user data; Automatic may add them for any provider.
pub const PUBLIC_BOXES: [&str; 2] = ["web-search", "offline-wikipedia"];
/// `CLOUD_SAFE_BOXES`: the allow-list a hybrid cloud route without a grant may be offered.
pub const CLOUD_SAFE_BOXES: [&str; 4] = ["core", "web-search", "web-crawl", "offline-wikipedia"];
/// `PRIVATE_CORE_TOOLS`: the project reader, taken out of `core` on such a route.
pub const PRIVATE_CORE_TOOLS: [&str; 1] = ["read_project_file"];
/// `HEALTH_PUBLIC_BOXES`.
pub const HEALTH_PUBLIC_BOXES: [&str; 2] = ["web-search", "offline-wikipedia"];
/// `CARRY_USER_TURNS`.
pub const CARRY_USER_TURNS: usize = 3;

/// JS `\s` (WhiteSpace and LineTerminator), as the body of a class.
const SPACE: &str = r"\t\n\x0b\x0c\r \u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";
/// Non-Unicode JS `\w`, as the body of a class.
const WORD: &str = "0-9A-Za-z_";

/// A non-Unicode JS regex source in the `regex` crate's syntax: `\s`, `\w`, `\d` spelt out (in a
/// class or as one), `\b` as the ASCII word boundary, `\/` as `/`. `None` for anything else this
/// translation does not know to be the same (another letter escape, a lookaround, a backreference).
pub fn js_regex(src: &str) -> Option<String> {
    let mut out = String::new();
    let mut class = false;
    let mut it = src.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                let e = it.next()?;
                let set = match e {
                    's' => Some(SPACE),
                    'w' => Some(WORD),
                    'd' => Some("0-9"),
                    _ => None,
                };
                match (e, set) {
                    (_, Some(set)) if class => out.push_str(set),
                    (_, Some(set)) => out.push_str(&format!("[{set}]")),
                    ('b', _) if !class => out.push_str(r"(?-u:\b)"),
                    ('/', _) => out.push('/'),
                    (e, _) if e.is_ascii_punctuation() => {
                        out.push('\\');
                        out.push(e);
                    }
                    _ => return None,
                }
            }
            '[' if class => return None, // a set operation in the regex crate
            '[' => {
                class = true;
                out.push(c);
            }
            ']' if class => {
                class = false;
                out.push(c);
            }
            '(' if !class && it.as_str().starts_with('?') && !it.as_str().starts_with("?:") => {
                return None;
            }
            '&' | '~' if class => return None,
            _ => out.push(c),
        }
    }
    Some(out)
}

/// `URL_RE`.
const URL: &str = r#"\bhttps?:\/\/[^\s<>"')\]]+"#;
/// `EXPLICIT_WEB_RE` without its one lookahead, `google(?!\s+(?:drive|docs?|sheets?|slides|calendar
/// |photos|keep|mail))\s+(?:it|this|that|for)\b`: where `\s+(?:it|this|that|for)` follows, the first
/// character after the spaces is `i`, `t` or `f`, and none of the lookahead's words starts with one
/// of those, so the lookahead always holds there and dropping it changes no match.
const EXPLICIT_WEB: &str = r"\b(?:search(?:\s+(?:the|on\s+the))?\s+(?:web|internet|online)|web\s*search|(?:on|from)\s+the\s+(?:web|internet)|(?:search|look(?:\s+it)?\s*up|find\s+it)\s+online|google\s+(?:it|this|that|for)\b|browse\s+(?:the\s+)?(?:web|internet))\b";
/// `WEB_RE`'s parts, joined with `|`.
const WEB: [&str; 7] = [
    r"\b(?:latest|newest)\b",
    r"\b(?:most\s+)?recent(?:ly)?\s+(?:news|releases?|versions?|updates?|events|changes)\b",
    r"\bcurrent(?:ly)?\s+(?:stable\s+)?(?:versions?|releases?|prices?|rates?|exchange\s+rates?|news|events|weather|status|president|prime\s+minister|ceo|champions?|lts)\b",
    r"\b(?:today|tonight|this\s+(?:week|month|year))'?s\s+(?:top\s+)?(?:news|headlines|weather|forecast|prices?|rates?|exchange\s+rates?|scores?|results|releases?)\b",
    r"\b(?:news|headlines|weather|forecast|exchange\s+rates?|stock\s+prices?|price\s+of)\b",
    r"\b(?:cite\s+(?:your\s+|the\s+)?sources?|with\s+sources?|source\s+urls?)\b",
    r"\blook\s*up\b",
];
const NEXTCLOUD: &str = r"\bnext\s?cloud\b";
/// `NEXTCLOUD_APPS`: the box and the words that name its app.
const NEXTCLOUD_APPS: [(&str, &str); 5] = [
    ("nextcloud-notes", "notes?"),
    (
        "nextcloud-calendar",
        "calendars?|appointments?|meetings?|events?|agenda|schedule",
    ),
    ("nextcloud-tasks", "tasks?|to-?dos?|reminders?"),
    ("nextcloud-contacts", r"contacts?|address\s*books?"),
    ("nextcloud-mail", "e-?mails?|mail|inbox"),
];
const FILES_WORDS: &str = r"files?|folders?|director(?:y|ies)|documents?|docs";
const WIKIPEDIA: &str = r"\bwikipedia\b";

/// The JS sources as written here, by the name of the constant in tools-mode.cjs (`WEB_RE` joined
/// as `new RegExp` joins it), for the recording's check against the pinned module;
/// `NEXTCLOUD_APPS` as `box:words` lines.
pub fn sources() -> [(&'static str, String); 7] {
    let apps: Vec<String> = NEXTCLOUD_APPS
        .iter()
        .map(|(b, w)| format!("{b}:{w}"))
        .collect();
    [
        ("NEXTCLOUD_APPS", apps.join("\n")),
        ("FILES_WORDS", FILES_WORDS.to_string()),
        ("URL_RE", URL.to_string()),
        ("EXPLICIT_WEB_RE", EXPLICIT_WEB.to_string()),
        ("WEB_RE", WEB.join("|")),
        ("NEXTCLOUD_RE", NEXTCLOUD.to_string()),
        ("WIKIPEDIA_RE", WIKIPEDIA.to_string()),
    ]
}

/// `possessive(words)`.
fn possessive(words: &str) -> String {
    format!(r"\b(?:my|our)\s+(?:[\w-]+\s+){{0,2}}(?:{words})\b")
}

/// `bare(words)`.
fn bare(words: &str) -> String {
    format!(r"\b(?:{words})\b")
}

struct Patterns {
    url: Regex,
    explicit_web: Regex,
    web: Regex,
    nextcloud: Regex,
    /// Each app's box with its bare and possessive patterns.
    apps: Vec<(&'static str, Regex, Regex)>,
    files_bare: Regex,
    files_possessive: Regex,
    wikipedia: Regex,
}

/// A JS source compiled as [`js_regex`] translates it.
pub fn compile(src: &str) -> Option<Regex> {
    Regex::new(&js_regex(src)?).ok()
}

fn patterns() -> Option<&'static Patterns> {
    static P: OnceLock<Option<Patterns>> = OnceLock::new();
    P.get_or_init(|| {
        let mut apps = Vec::new();
        for (b, words) in NEXTCLOUD_APPS {
            apps.push((b, compile(&bare(words))?, compile(&possessive(words))?));
        }
        Some(Patterns {
            url: compile(URL)?,
            explicit_web: compile(EXPLICIT_WEB)?,
            web: compile(&WEB.join("|"))?,
            nextcloud: compile(NEXTCLOUD)?,
            apps,
            files_bare: compile(&bare(FILES_WORDS))?,
            files_possessive: compile(&possessive(FILES_WORDS))?,
            wikipedia: compile(WIKIPEDIA)?,
        })
    })
    .as_ref()
}

/// `toolsModeOf(project)`: true for `'auto'`.
pub fn automatic(project: &JValue) -> bool {
    match project.get("toolsMode").as_str() {
        Some("auto") => true,
        Some("manual") => false,
        _ => !(project.truthy() && matches!(project.get("toolboxes"), JValue::Arr(_))),
    }
}

/// `isPrivateDataBox(id)`.
pub fn is_private_data_box(id: &str) -> bool {
    !CLOUD_SAFE_BOXES.contains(&id)
}

/// `isPublicToolName(name, boxes)`: offered by a search or Wikipedia box and by no private-data box.
pub fn is_public_tool_name(name: Option<&str>, boxes: &[Toolbox]) -> bool {
    let Some(name) = name.filter(|n| !n.is_empty() && !PRIVATE_CORE_TOOLS.contains(n)) else {
        return false;
    };
    !boxes
        .iter()
        .any(|b| is_private_data_box(&b.id) && b.offers(name))
        && boxes
            .iter()
            .any(|b| HEALTH_PUBLIC_BOXES.contains(&b.id.as_str()) && b.offers(name))
}

/// `tool-gate.cjs` `prefetchableSearch(text)`: the port core's dav-parse.wasm answers (op 4,
/// `toolGateQueries`), and a fault or refusal is `false` as there.
fn prefetchable_search(text: &str) -> bool {
    let body = wtf16::stringify(&JValue::Arr(vec![JValue::Arr(vec![JValue::from(text)])]));
    let mut input = vec![4u8];
    input.extend_from_slice(body.unwrap_or_default().as_bytes());
    let (status, out) = tool_gate::call(&input);
    status == 0 && out.ends_with(br#","prefetchable":[true]}"#)
}

/// `message.slice(0, 4000)`, `None` when it is only JS space.
fn head(message: &str) -> Option<String> {
    let text = js::slice(message, 4000);
    (!js::trim(&text).is_empty()).then_some(text)
}

fn wants_web_in(p: &Patterns, text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let has_url = p.url.is_match(&lower);
    let short = prefetchable_search(text);
    if has_url && !short {
        return p.explicit_web.is_match(&lower);
    }
    p.explicit_web.is_match(&lower) || p.web.is_match(&lower) || (has_url && short)
}

/// `wantsWeb(message)`: the message asks for current information from the web.
pub fn wants_web(message: &str) -> bool {
    match (patterns(), head(message)) {
        (Some(p), Some(text)) => wants_web_in(p, &text),
        _ => false,
    }
}

/// `automaticBoxesFor(message)`: the box ids a message plainly needs, most specific first.
pub fn automatic_boxes_for(message: &str) -> Vec<&'static str> {
    let (Some(p), Some(text)) = (patterns(), head(message)) else {
        return Vec::new();
    };
    let lower = text.to_ascii_lowercase();
    let mut out = Vec::new();
    let named = p.nextcloud.is_match(&lower);
    let mut app = false;
    for (b, bare_re, possessive_re) in &p.apps {
        if (named && bare_re.is_match(&lower)) || possessive_re.is_match(&lower) {
            out.push(*b);
            app = true;
        }
    }
    if (named && (!app || p.files_bare.is_match(&lower))) || p.files_possessive.is_match(&lower) {
        out.push("nextcloud-files");
    }
    if p.wikipedia.is_match(&lower) {
        out.push("offline-wikipedia");
    }
    let private = out.iter().any(|id| id.starts_with("nextcloud-"));
    let web = if private {
        p.explicit_web.is_match(&lower)
    } else {
        wants_web_in(p, &text)
    };
    if web {
        out.push("web-search");
    }
    out
}

/// What `automaticEligible` and `automaticAdditions` read besides the message.
pub struct Additions<'a> {
    /// `base`: the turn's own box ids.
    pub base: &'a [String],
    /// `allToolboxes()`.
    pub boxes: &'a [Toolbox],
    /// chat.cjs's `isWriteTool`.
    pub is_write: &'a dyn Fn(&str) -> bool,
    /// `ready(box) === true` (a `ready` that throws is `false`): the account can use the box.
    pub ready: &'a dyn Fn(&Toolbox) -> bool,
    /// `privateData`: the answering model is local.
    pub private_data: bool,
    /// `carried`: [`carried_public_boxes`].
    pub carried: &'a [String],
}

/// `automaticEligible(box, ...)`.
pub fn automatic_eligible(b: &Toolbox, a: &Additions<'_>) -> bool {
    if crate::toolboxes::CONNECTOR_BOXES.contains(&b.id.as_str()) {
        return false;
    }
    if !a.private_data && !PUBLIC_BOXES.contains(&b.id.as_str()) {
        return false;
    }
    b.text_names().any(|n| !(a.is_write)(n)) && (a.ready)(b)
}

/// `automaticAdditions({ message, base, boxes, connectorBoxes, isWriteTool, ready, privateData,
/// carried })` with `max` = [`MAX_ADDED`].
pub fn automatic_additions(message: &str, a: &Additions<'_>) -> Vec<String> {
    let own = automatic_boxes_for(message);
    let private_turn = own.iter().any(|id| id.starts_with("nextcloud-"))
        || a.base.iter().any(|id| id.starts_with("nextcloud-"));
    let keep: Vec<&str> = if private_turn {
        Vec::new()
    } else {
        a.carried
            .iter()
            .map(String::as_str)
            .filter(|id| PUBLIC_BOXES.contains(id))
            .collect()
    };
    let mut out: Vec<String> = Vec::new();
    for id in own.into_iter().chain(keep) {
        if out.len() >= MAX_ADDED {
            break;
        }
        if a.base.iter().any(|b| b == id) || out.iter().any(|o| o == id) {
            continue;
        }
        // `new Map(boxes.map((b) => [b.id, b]))`: the last box of an id is the one checked.
        let Some(b) = a.boxes.iter().rev().find(|b| b.id == id) else {
            continue;
        };
        if automatic_eligible(b, a) {
            out.push(id.to_string());
        }
    }
    out
}

/// `carriedPublicBoxes({ history, boxes })`: the public boxes the chat's recent turns used.
pub fn carried_public_boxes(history: &[Entry], boxes: &[Toolbox]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let add = |id: &str, out: &mut Vec<String>| {
        if PUBLIC_BOXES.contains(&id) && !out.iter().any(|o| o == id) {
            out.push(id.to_string());
        }
    };
    let mut users = 0;
    for h in history.iter().rev() {
        if users >= CARRY_USER_TURNS {
            break;
        }
        if h.role == "user" {
            users += 1;
            let ids = automatic_boxes_for(&h.content);
            if ids.iter().any(|id| id.starts_with("nextcloud-")) {
                return Vec::new();
            }
            for id in ids {
                add(id, &mut out);
            }
        } else if h.role == "tool" || h.role == "function" {
            if !is_public_tool_name(h.name.as_deref(), boxes) {
                return Vec::new();
            }
            let name = h.name.as_deref().unwrap_or_default();
            for b in boxes.iter().filter(|b| b.offers(name)) {
                add(&b.id, &mut out);
            }
        }
    }
    out
}

/// `orderWithAdditions(ids, added, head)`: the head boxes, then the additions, then the rest.
pub fn order_with_additions(ids: &[String], added: &[String], head: &[&str]) -> Vec<String> {
    let is_added = |id: &String| added.contains(id);
    let is_head = |id: &String| head.contains(&id.as_str()) && !is_added(id);
    let mut out: Vec<String> = ids.iter().filter(|id| is_head(id)).cloned().collect();
    out.extend(added.iter().filter(|id| ids.contains(id)).cloned());
    out.extend(
        ids.iter()
            .filter(|id| !is_head(id) && !is_added(id))
            .cloned(),
    );
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_translation_keeps_js_classes_and_refuses_what_it_cannot_keep() {
        assert_eq!(js_regex(r"\bq\/a\b").unwrap(), r"(?-u:\b)q/a(?-u:\b)");
        assert_eq!(js_regex(r"[\w-]").unwrap(), "[0-9A-Za-z_-]");
        assert!(js_regex(r"[^\s]").unwrap().contains(r"\u{feff}"));
        assert!(!js_regex(r"\s").unwrap().contains("85"));
        for refused in [
            r"a(?!b)", r"a(?=b)", r"(?<n>a)", r"(a)\1", r"\S", r"[[a]]", r"[a&&b]",
        ] {
            assert_eq!(js_regex(refused), None, "{refused}");
        }
    }

    #[test]
    fn case_folds_in_ascii_only() {
        // The Kelvin sign and a long s never stand for k or s; U+0085 is no JS space, U+FEFF is.
        assert!(!wants_web("loo\u{212a} up"));
        assert!(!automatic_boxes_for("\u{17f}earch the web").contains(&"web-search"));
        assert!(automatic_boxes_for("LOOK UP qa").contains(&"web-search"));
        assert!(automatic_boxes_for("my\u{feff}notes").contains(&"nextcloud-notes"));
        assert!(!automatic_boxes_for("my\u{85}notes").contains(&"nextcloud-notes"));
    }
}
