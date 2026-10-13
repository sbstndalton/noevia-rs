//! #1140: a backend that reports FEWER per-card VRAM entries than gpu_count must plan, not fail.
//!
//! Python (`autoconfig_core.py`) gives a short `cards` list the same even-division fallback an
//! empty list gets, and a card with no reported capacity holds nothing. The Rust port must agree
//! exactly, so each short-list request is compared with the same request with `"cards":[]`.
//! Synthetic numbers only; nothing here loads a model.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use model_autoconfig::size_plan_json;

fn request(gpu_count: u32, cards: &str) -> String {
    format!(
        r#"{{"shape":{{"gemma":false,"layers":32,"kv_heads":8,"k_dim":128,"v_dim":128,"hybrid_interval":null,"window":null,"k_swa":null,"v_swa":null,"shared":0,"period":null}},"layers":32,"native_ctx":131072,"model_gb_raw":30.0,"moe_ratio":0.0,"is_moe":false,"mmproj_vram_gb":0.0,"n_sessions":1,"backends":[{{"vram_gb":36.0,"gpu_count":{gpu_count},"cards":[{cards}],"host_ram_gb":64.0,"same_as":0}}],"preset":"","prompt_tps":0.0,"prompt_budget_s":120.0,"verified_ctx":0,"cache_ram_cap_mib":1024}}"#
    )
}

#[test]
fn one_of_three_cards_plans_like_no_cards() {
    let short = size_plan_json(request(3, "12.0").as_bytes());
    let absent = size_plan_json(request(3, "").as_bytes());
    assert!(short.is_ok(), "short cards must not be an error: {short:?}");
    assert_eq!(short.unwrap(), absent.unwrap());
}

#[test]
fn two_of_three_cards_plans_like_no_cards() {
    let short = size_plan_json(request(3, "12.0,12.0").as_bytes());
    let absent = size_plan_json(request(3, "").as_bytes());
    assert!(short.is_ok(), "short cards must not be an error: {short:?}");
    assert_eq!(short.unwrap(), absent.unwrap());
}

#[test]
fn full_card_list_is_used_per_card() {
    let uneven = size_plan_json(request(3, "4.0,14.0,14.0").as_bytes()).unwrap();
    let even = size_plan_json(request(3, "").as_bytes()).unwrap();
    assert_ne!(uneven, even);
}
