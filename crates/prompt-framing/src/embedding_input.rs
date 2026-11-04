//! The text transformation in Core's `server/embedding-profile.cjs`.
//! This boundary takes already-coerced JS strings as UTF-16 code units. It does not
//! select the active profile or model, coerce JSON values, or send an embedding request.

use crate::js::{is_js_space, units};

pub const DEFAULT_PROFILE: Profile = Profile::NomicV1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    NomicV1,
    EmbeddingGemma2,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    Query,
    #[default]
    Document,
    Similarity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    UnknownProfile,
    UnknownKind,
}

impl TryFrom<&str> for Profile {
    type Error = Refusal;

    fn try_from(name: &str) -> Result<Self, Self::Error> {
        match name {
            "nomic-v1" => Ok(Self::NomicV1),
            "embeddinggemma-2" => Ok(Self::EmbeddingGemma2),
            _ => Err(Refusal::UnknownProfile),
        }
    }
}

impl TryFrom<&str> for Kind {
    type Error = Refusal;

    fn try_from(name: &str) -> Result<Self, Self::Error> {
        match name {
            "query" => Ok(Self::Query),
            "document" => Ok(Self::Document),
            "similarity" => Ok(Self::Similarity),
            _ => Err(Refusal::UnknownKind),
        }
    }
}

/// Collapse runs of JS whitespace or the prefix separator, trim, then take
/// the first 160 code units. The slice can end with a lone high surrogate.
fn clean_title(title: &[u16]) -> Vec<u16> {
    const MAX_UNITS: usize = 160;
    let mut clean = Vec::with_capacity(title.len().min(MAX_UNITS));
    let mut separator = false;
    for &unit in title {
        if unit == u16::from(b'|') || is_js_space(unit) {
            separator = !clean.is_empty();
            continue;
        }
        if separator {
            clean.push(u16::from(b' '));
            separator = false;
        }
        if clean.len() == MAX_UNITS {
            break;
        }
        clean.push(unit);
        if clean.len() == MAX_UNITS {
            break;
        }
    }
    if clean.is_empty() {
        return units("none");
    }
    clean
}

/// Frame one text. Batch callers can select a kind and title for each item.
/// Missing titles are equivalent to empty titles. The text is copied as code
/// units, so even an unpaired surrogate is retained.
pub fn frame(profile: Profile, kind: Kind, text: &[u16], title: Option<&[u16]>) -> Vec<u16> {
    if profile == Profile::NomicV1 {
        return text.to_vec();
    }
    let mut out = match kind {
        Kind::Query => units("task: search result | query: "),
        Kind::Document => {
            let mut prefix = units("title: ");
            prefix.extend(clean_title(title.unwrap_or_default()));
            prefix.extend(" | text: ".encode_utf16());
            prefix
        }
        Kind::Similarity => units("task: sentence similarity | query: "),
    };
    out.extend_from_slice(text);
    out
}

#[cfg(test)]
mod tests {
    use super::{frame, Kind, Profile, Refusal, DEFAULT_PROFILE};
    use crate::js::units;

    #[test]
    fn core_profile_examples_and_unknown_values() {
        // Existing synthetic cases in server/embedding-config-1636.test.cjs.
        assert_eq!(DEFAULT_PROFILE, Profile::NomicV1);
        for kind in [Kind::Query, Kind::Document, Kind::Similarity] {
            let text = units("  spaced\n|\r\n ");
            assert_eq!(frame(DEFAULT_PROFILE, kind, &text, Some(&units("title"))), text);
        }
        let profile = Profile::try_from("embeddinggemma-2").ok();
        assert_eq!(profile, Some(Profile::EmbeddingGemma2));
        let profile = Profile::EmbeddingGemma2;
        assert_eq!(
            frame(profile, Kind::Query, &units("what causes rain"), None),
            units("task: search result | query: what causes rain")
        );
        assert_eq!(
            frame(profile, Kind::Document, &units("Rain forms when…"), None),
            units("title: none | text: Rain forms when…")
        );
        assert_eq!(
            frame(
                profile,
                Kind::Document,
                &units("Rain forms when…"),
                Some(&units("Weather | notes\nv2")),
            ),
            units("title: Weather notes v2 | text: Rain forms when…")
        );
        assert_eq!(
            frame(profile, Kind::Similarity, &units("x"), None),
            units("task: sentence similarity | query: x")
        );
        assert_eq!(Profile::try_from("constructor"), Err(Refusal::UnknownProfile));
        assert_eq!(Kind::try_from("passage"), Err(Refusal::UnknownKind));
        assert_eq!(Kind::try_from("document"), Ok(Kind::default()));
    }

    #[test]
    fn js_whitespace_and_utf16_truncation_are_lossless() {
        let profile = Profile::EmbeddingGemma2;
        assert_eq!(
            frame(
                profile,
                Kind::Document,
                &[0xdc00],
                Some(&units("\u{feff} \r|\n\u{a0}")),
            ),
            [units("title: none | text: "), vec![0xdc00]].concat()
        );
        assert_eq!(
            frame(
                profile,
                Kind::Document,
                &[],
                Some(&units("\u{feff}a\u{a0}|\u{2028}b\u{0085}c\u{feff}")),
            ),
            units("title: a b\u{0085}c | text: ")
        );

        let mut title = vec![u16::from(b'a'); 159];
        title.extend([0xd83d, 0xde00]);
        let mut expected = units("title: ");
        expected.extend(vec![u16::from(b'a'); 159]);
        expected.push(0xd83d);
        expected.extend(units(" | text: "));
        assert_eq!(frame(profile, Kind::Document, &[], Some(&title)), expected);

        let title = units(&format!("{} | b", "a".repeat(159)));
        let expected = units(&format!("title: {}  | text: ", "a".repeat(159)));
        assert_eq!(frame(profile, Kind::Document, &[], Some(&title)), expected);
    }
}
