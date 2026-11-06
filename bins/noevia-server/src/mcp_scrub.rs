//! The credential values an MCP call carried, taken out of any text a server sends back before that text goes
//! anywhere else (the model, a log, a key-check answer). A server can echo what it was sent -- in an HTTP body, a
//! JSON-RPC message, a tool's own text -- and clipping or hiding the field it came in is not redaction.
//!
//! What counts as a credential value, and every length of it (a one-byte key is as much a credential as a long one):
//! every header value, the part after an auth scheme (`Bearer x` gives `x`), and for `Basic` the decoded
//! `user:secret` and its two halves; each also in its JSON-escaped and percent-encoded spelling when that differs.
//!
//! How it is looked for: a value of [`SUBSTRING_MIN`] bytes or more anywhere in the text (nothing ordinary
//! collides with it); a shorter one only as a whole token, i.e. not touching a letter, digit or underscore on
//! either side, so that a 2-byte key does not blank the words that happen to contain it.
//!
//! Scrub **first**, clip afterwards: a credential that crosses the cutoff of a clipped body must not leave its prefix.
use base64::Engine;
use js_json::JValue;

/// Values at least this long are looked for as substrings; shorter ones as whole tokens.
pub const SUBSTRING_MIN: usize = 8;
pub const REDACTED: &str = "[redacted]";

#[derive(Default, Clone, Debug)]
pub struct Secrets(Vec<String>);

fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `text[start..end]` stands alone: nothing word-like touches it.
fn bounded(text: &str, start: usize, end: usize) -> bool {
    let before = text.get(..start).and_then(|t| t.chars().next_back());
    let after = text.get(end..).and_then(|t| t.chars().next());
    !before.is_some_and(word) && !after.is_some_and(word)
}

fn percent_encoded(v: &str) -> String {
    let mut out = String::new();
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Secrets {
    /// The values of a call's request headers (all of them are credentials, apart from the account id a Nextcloud
    /// server is told, which is an identifier).
    pub fn from_headers(headers: &[(String, String)]) -> Secrets {
        let mut s = Secrets::default();
        s.add_headers(headers);
        s
    }

    pub fn add_headers(&mut self, headers: &[(String, String)]) {
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("x-cowork-user-id") {
                self.add_value(value);
            }
        }
    }

    /// One header value and the secrets inside it.
    pub fn add_value(&mut self, value: &str) {
        self.add(value);
        if let Some((scheme, rest)) = value.split_once(' ') {
            let rest = rest.trim();
            self.add(rest);
            if scheme.eq_ignore_ascii_case("basic") {
                if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(rest) {
                    let raw = String::from_utf8_lossy(&raw).into_owned();
                    self.add(&raw);
                    if let Some((user, secret)) = raw.split_once(':') {
                        self.add(user);
                        self.add(secret);
                    }
                }
            }
        }
    }

    fn add(&mut self, v: &str) {
        if v.trim().is_empty() {
            return;
        }
        let json = js_json::stringify(&JValue::Str(v.to_owned()))
            .and_then(|q| q.get(1..q.len().saturating_sub(1)).map(str::to_owned));
        for form in [Some(v.to_owned()), json, Some(percent_encoded(v))]
            .into_iter()
            .flatten()
        {
            if !self.0.contains(&form) {
                self.0.push(form);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `text` with every credential value replaced by [`REDACTED`].
    pub fn scrub(&self, text: &str) -> String {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for v in &self.0 {
            let whole_token = v.len() < SUBSTRING_MIN;
            let mut from = 0;
            while let Some(i) = text.get(from..).and_then(|t| t.find(v.as_str())) {
                let (start, end) = (from + i, from + i + v.len());
                if !whole_token || bounded(text, start, end) {
                    spans.push((start, end));
                }
                from = start
                    + text
                        .get(start..)
                        .and_then(|t| t.chars().next())
                        .map_or(1, char::len_utf8);
            }
        }
        if spans.is_empty() {
            return text.to_owned();
        }
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in spans {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (start, end) in merged {
            out.push_str(text.get(at..start).unwrap_or(""));
            out.push_str(REDACTED);
            at = end;
        }
        out.push_str(text.get(at..).unwrap_or(""));
        out
    }

    /// Scrub, then keep the first `chars` characters.
    pub fn scrub_clip(&self, text: &str, chars: usize) -> String {
        self.scrub(text).chars().take(chars).collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn with(values: &[&str]) -> Secrets {
        let mut s = Secrets::default();
        for v in values {
            s.add_value(v);
        }
        s
    }

    #[test]
    fn every_length_of_credential_is_redacted_as_a_whole_token() {
        for key in [
            "a", "q7", "q7Z", "q7Z!", "q7Z!2", "q7Z!2x", "q7Z!2xy", "q7Z!2xyz",
        ] {
            let s = with(&[key]);
            for (text, want) in [
                (format!("bad token {key}"), format!("bad token {REDACTED}")),
                (
                    format!("bad token {key}."),
                    format!("bad token {REDACTED}."),
                ),
                (format!("({key})"), format!("({REDACTED})")),
                (key.to_owned(), REDACTED.to_owned()),
            ] {
                assert_eq!(s.scrub(&text), want, "{key}");
            }
        }
    }

    #[test]
    fn a_short_value_inside_a_longer_word_is_left_alone_and_a_long_one_is_not() {
        let short = with(&["ab"]);
        assert_eq!(short.scrub("cabin about ab"), "cabin about [redacted]");
        assert_eq!(short.scrub("ab_c ab9 9ab"), "ab_c ab9 9ab");
        // Seven bytes: still a whole token only; eight: a substring wherever it is.
        assert_eq!(with(&["abcdefg"]).scrub("xabcdefgx"), "xabcdefgx");
        assert_eq!(with(&["abcdefgh"]).scrub("xabcdefghx"), "x[redacted]x");
    }

    #[test]
    fn the_scheme_stripped_token_the_basic_halves_and_the_other_spellings_are_covered() {
        let basic = base64::engine::general_purpose::STANDARD.encode("al:pw12");
        let s = with(&[&format!("Basic {basic}"), "Bearer t9"]);
        let text = format!("{basic} al:pw12 al pw12 t9 Basic {basic} Bearer t9");
        assert_eq!(
            s.scrub(&text),
            "[redacted] [redacted] [redacted] [redacted] [redacted] [redacted] [redacted]"
        );
        let odd = with(&["a\"b\\c"]);
        assert_eq!(
            odd.scrub(r#"{"k":"a\"b\\c"} a"b\c"#),
            r#"{"k":"[redacted]"} [redacted]"#
        );
        assert_eq!(
            with(&["p w&1"]).scrub("p%20w%261 and p w&1"),
            "[redacted] and [redacted]"
        );
    }

    #[test]
    fn nothing_empty_is_a_credential_and_overlaps_merge() {
        assert!(with(&["", "  ", "X-Cowork"]).scrub("a b").eq("a b"));
        let s = with(&["abcdefgh", "cdefghij"]);
        assert_eq!(s.scrub("abcdefghij"), "[redacted]");
        let mut ids = Secrets::default();
        ids.add_headers(&[("X-Cowork-User-ID".into(), "user-123456".into())]);
        assert!(ids.is_empty());
    }

    #[test]
    fn scrubbing_comes_before_clipping() {
        let secret = "SYNTH-api-secret-1234";
        let body = format!("{}{secret}", "x".repeat(190));
        let s = with(&[secret]);
        let clipped = s.scrub_clip(&body, 200);
        assert!(!clipped.contains("SYNTH"), "{clipped}");
        // A clip taken first would have left the first ten characters of it.
        assert!(body.chars().take(200).collect::<String>().contains("SYNTH"));
    }
}
