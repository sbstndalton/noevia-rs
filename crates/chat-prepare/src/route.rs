//! What a routed turn asks of the model besides its messages (slice P5a): chat.cjs ~1405-1430
//! (`resolvedEffort`, `effort`, `fastThinkingOff`, `thinkingOffFields`, `samplingAutoEnabled`,
//! `sampling`) and what the turn sends of them: the model request's spread
//! `...sampling.params, ...thinkingOffFields` ([`Route::request_fields`]), the meta event's
//! `sampling` ([`Route::meta_sampling`]) and the native executor's `effort`, `thinkingOff` and
//! `sampling` ([`Route::effort`], [`Route::thinking_off`], [`Route::sampling`]).
//!
//! Pure: the caller reads the two settings rows and the environment, and the routing (a later
//! slice) decides `wantsAuto`, `routedRole` and the provider row. Recorded by running chat.cjs's
//! own lines (cut out of the pinned file) in `tests/record_route.cjs`.

use js_json::JValue;

use crate::effort::{self, Effort, Engine};
use crate::sampling::{self, Sampling, Source};

/// What chat.cjs's block reads.
#[derive(Debug, Clone, Copy)]
pub struct RouteTurn<'a> {
    /// The turn's project record; `null` without one.
    pub project: &'a JValue,
    /// The settings row `reasoning_effort_default`'s `value` (`undefined` without the row or the
    /// table, `null` for SQL NULL).
    pub effort_default: &'a JValue,
    /// The settings row `auto_sampling_presets_enabled`'s `value`, the same way.
    pub sampling_presets: &'a JValue,
    /// `wantsAuto`.
    pub wants_auto: bool,
    /// `routedRole` after routing (`None` for `null`).
    pub routed_role: Option<&'a str>,
    /// The provider row the turn answers through (`provider`).
    pub provider: &'a JValue,
    /// `DEFAULT_PROVIDER_ID` (index.cjs: the variable of that name, else `'default'`).
    pub default_provider_id: &'a str,
    /// The environment [`effort::local_engine`] reads.
    pub engine: Engine<'a>,
    /// `body.message`.
    pub message: &'a str,
}

/// chat.cjs's values.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// `resolvedEffort`.
    pub resolved_effort: Effort,
    /// `effort`: the project's own effort is kept; the account default is lowered on a Fast route.
    pub effort: Effort,
    /// `fastThinkingOff`.
    pub thinking_off: bool,
    /// `samplingAutoEnabled`.
    pub sampling_auto: bool,
    /// `sampling`.
    pub sampling: Sampling,
}

/// `samplingAutoEnabled`: no row (or a NULL value) is on; any value but the text `'false'` is on.
pub fn sampling_auto_enabled(raw: &JValue) -> bool {
    raw.is_nullish() || raw.as_str() != Some("false")
}

/// chat.cjs's block for one turn.
pub fn route(turn: &RouteTurn<'_>) -> Route {
    let project_effort = turn.project.get("reasoningEffort");
    let resolved_effort = effort::resolve_effort(turn.project, turn.effort_default);
    let effort = if effort::valid_effort(project_effort) {
        resolved_effort
    } else {
        effort::effort_for_route(
            resolved_effort,
            turn.routed_role.filter(|_| turn.wants_auto),
        )
    };
    let on_engine = || {
        (turn.provider.get("id").as_str() == Some(turn.default_provider_id)
            && effort::local_engine(turn.provider, &turn.engine))
            || turn.provider.get("machine").truthy()
    };
    let thinking_off =
        effort::fast_route_thinking_off(turn.wants_auto, turn.routed_role, project_effort)
            && on_engine();
    let sampling_auto = sampling_auto_enabled(turn.sampling_presets);
    let sampling = sampling::select(
        turn.routed_role,
        turn.message,
        turn.project.get("sampling"),
        sampling_auto,
    );
    Route {
        resolved_effort,
        effort,
        thinking_off,
        sampling_auto,
        sampling,
    }
}

impl Route {
    /// `thinkingOffFields`: `{ chat_template_kwargs: { enable_thinking: false } }` or `{}`.
    pub fn thinking_off_fields(&self) -> JValue {
        if self.thinking_off {
            JValue::obj([(
                "chat_template_kwargs",
                JValue::obj([("enable_thinking", JValue::Bool(false))]),
            )])
        } else {
            JValue::Obj(Vec::new())
        }
    }

    /// `{ ...sampling.params, ...thinkingOffFields }`: the fields the model request adds after its
    /// own (`model`, `max_tokens`, `messages`, `stream`), before the tools.
    pub fn request_fields(&self) -> JValue {
        let mut items = match self.sampling.params_value() {
            JValue::Obj(items) => items,
            _ => Vec::new(),
        };
        if let JValue::Obj(extra) = self.thinking_off_fields() {
            items.extend(extra);
        }
        JValue::Obj(items)
    }

    /// The meta event's `sampling`: `undefined` when the source is `'none'`, else
    /// `{ preset, source, values }` (`preset` left out when the presets are off).
    pub fn meta_sampling(&self) -> JValue {
        if self.sampling.source == Source::None {
            return JValue::Undefined;
        }
        let mut items = Vec::new();
        if let Some(p) = self.sampling.preset {
            items.push(("preset".to_string(), JValue::from(p.as_str())));
        }
        items.push((
            "source".to_string(),
            JValue::from(self.sampling.source.as_str()),
        ));
        items.push(("values".to_string(), self.sampling.params_value()));
        JValue::Obj(items)
    }
}
