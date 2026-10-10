//! `/[\p{C}\p{Zl}\p{Zp}]/u` (device-auth.cjs cleanClientName), from ICU's general categories.
//! ICU4X's data may be a newer Unicode version than Node's ICU; only characters assigned in
//! between can be classified differently.

use icu_properties::props::GeneralCategory;
use icu_properties::CodePointMapData;

/// Other (Cc, Cf, Cs, Co, Cn), line separator or paragraph separator.
pub fn is_other_or_separator(c: char) -> bool {
    matches!(
        CodePointMapData::<GeneralCategory>::new().get(c),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::Surrogate
            | GeneralCategory::PrivateUse
            | GeneralCategory::Unassigned
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories() {
        for c in [
            '\u{0}', '\u{7f}', '\u{ad}', '\u{200b}', '\u{2028}', '\u{2029}', '\u{e000}', '\u{fffe}',
        ] {
            assert!(is_other_or_separator(c), "{c:?}");
        }
        for c in ['a', ' ', '\u{a0}', 'é', '😀', '\u{3000}'] {
            assert!(!is_other_or_separator(c), "{c:?}");
        }
    }
}
