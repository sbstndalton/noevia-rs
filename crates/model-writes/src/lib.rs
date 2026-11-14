//! The model settings' writes noevia-core still answers, ported as pure functions (track R11):
//!
//! - [`budget`]: `PUT /api/models/inference-budget` (`routes/models.cjs` and `inference-budget.cjs`
//!   `hostRamGib`, `range`, `save`, `get`): the request body read as `http.cjs` `readJson` reads it,
//!   the administrator check of the models write prelude, the validation against the host's RAM, the
//!   settings row to write and the answer.
//! - [`idle`]: `PUT /api/models/idle-unload` (`routes/models.cjs` and `idle-unload-setting.cjs`
//!   `save`): the model manager's answers, the write and its reload under one maintenance hold, the
//!   audit and the answer, over a [`idle::Host`] the caller implements; `globalIdleSeconds`.
//!
//! Recorded in `tests/fixtures/node-model-writes.v1.json`, written by
//! `tests/record_model_writes.cjs`, which runs routes/models.cjs's own lines (cut out of the pinned
//! file) over the real modules with stubs for the settings store, /proc/meminfo, the model manager,
//! the held writer and the router's presets.
//!
//! Named difference: a request body Node's `JSON.parse` reads but the port cannot hold (a number
//! outside the `f64` range, nesting deeper than 128, text in U+10F800..U+10FFFF) is not answered
//! here: [`budget::put`] and [`idle::put`] return `None` and the caller passes the request to core.

pub mod body;
pub mod budget;
pub mod idle;

/// JavaScript's `Math.round(n * 10) / 10` (`round1`): halves round towards +∞.
pub fn round1(n: f64) -> f64 {
    let x = n * 10.0;
    let f = x.floor();
    let r = if x - f >= 0.5 { f + 1.0 } else { f };
    r / 10.0
}
