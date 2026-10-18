//! Retired selectors cannot bypass mandatory native ownership or startup preflight.
#[path = "support/native_startup.rs"]
mod native_startup;
const KEYS: &[&str] = &["MODEL_MANAGER_MODELS_IMPL", "MODEL_MANAGER_OVERVIEW_IMPL"];
#[test]
fn retired_selectors_cannot_bypass_timezone_or_capability_preflight() {
    native_startup::refuse(KEYS, None, None);
}
#[test]
fn python_confirmation_cannot_bypass_empty_trusted_sets() {
    native_startup::refuse(KEYS, Some("UTC"), Some(r#"{"quantisers":[],"labs":[]}"#));
}
#[test]
fn malformed_trust_never_reaches_schema_or_preset_migration() {
    native_startup::refuse(KEYS, Some("UTC"), Some("{"));
}
