//! Properties over arbitrary input: no panics, bounded and sanitised reasons.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use proptest::prelude::*;

proptest! {
    #[test]
    fn analyze_never_panics(t in ".{0,400}", tags in proptest::collection::vec("[{}%#'\"a-z_ ().]{0,12}", 0..40)) {
        let joined = tags.concat();
        let _ = chat_template_caps::analyze(&t);
        let c = chat_template_caps::analyze(&joined).unwrap();
        prop_assert!(c.send_tools || (c.raises && !c.tools));
    }

    #[test]
    fn reasons_are_bounded_and_clean(status in 0u32..1000, msg in ".{0,600}") {
        let body = serde_json::json!({ "error": { "message": msg } }).to_string();
        let c = provider_error::classify(status, &body);
        prop_assert!(c.reason.chars().count() <= provider_error::MAX_REASON_CHARS);
        prop_assert!(!c.reason.chars().any(char::is_control));
        prop_assert!(!c.reason.contains("://"));
        let v = chat_template_caps::serving_verdict(status, &body);
        prop_assert!(!v.passed);
    }
}
