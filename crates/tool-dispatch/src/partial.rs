//! [`route`](crate::route) for a host that does not know everything core's executor knows. The native
//! front sees the turn's offer (`allowed`, `routes`) on every call, but not always which tools the
//! account's offline encyclopedia, Drive connection or discovered MCP boxes hold, nor whether the project
//! is a coworker conversation. [`decide`] walks core's order and stops with [`Decision::Core`] at the
//! first check whose answer depends on a fact the host left unknown; every [`Decision::Route`] it gives is
//! the route core's executor would take with the full facts (the oracle test replays every recorded call
//! with each fact hidden in turn and checks exactly that).

use crate::{checked, route_of, Route, CURRENT_TIME, READ_PROJECT_FILE};
use std::collections::HashSet;

/// What the host knows when one call arrives; `None` in a fact = not known here.
#[derive(Debug, Clone, Copy)]
pub struct Knowledge<'a> {
    /// As [`Offer::allowed`](crate::Offer::allowed) (the native bridge always has it).
    pub allowed: Option<&'a HashSet<String>>,
    /// As [`Offer::routes`](crate::Offer::routes).
    pub routes: Option<&'a [(String, String)]>,
    /// The coworker page tool's name (`specialistTools.NAME`), known even when its applicability is not.
    pub specialist_name: &'a str,
    /// Whether the page tool applies to the call's project (`specialistTools.appliesTo(project)`, and
    /// configured); `None` = not known.
    pub specialist_applies: Option<bool>,
    /// The offline-encyclopedia names (`kiwixTools.names`, empty when not configured).
    pub kiwix: Option<&'a [String]>,
    /// The Drive names (`driveTools.names`, empty when not configured).
    pub drive: Option<&'a [String]>,
    /// The names the discovered MCP boxes offer.
    pub mcp: Option<&'a HashSet<String>>,
}

impl<'a> Knowledge<'a> {
    /// Only the turn's offer: what the native front has on every call without asking anyone.
    pub fn offer_only(
        allowed: &'a HashSet<String>,
        routes: &'a [(String, String)],
        specialist_name: &'a str,
    ) -> Self {
        Knowledge {
            allowed: Some(allowed),
            routes: Some(routes),
            specialist_name,
            specialist_applies: None,
            kiwix: None,
            drive: None,
            mcp: None,
        }
    }
}

/// [`decide`]'s answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// The route core's executor takes.
    Route(Route),
    /// The route depends on a fact the host does not know: core decides (the host hands the call over).
    Core,
}

/// `runToolCall`'s choice of executor, as far as `known` settles it.
pub fn decide(name: &str, raw_args: &str, known: &Knowledge<'_>) -> Decision {
    let args = match checked(name, raw_args, known.allowed) {
        Ok(args) => args,
        Err(ended) => return Decision::Route(ended),
    };
    let routed = route_of(known.routes, name);
    if name == known.specialist_name && routed.is_none() {
        match known.specialist_applies {
            Some(true) => return Decision::Route(Route::Specialist(args)),
            Some(false) => {}
            None => return Decision::Core,
        }
    }
    if let Some(Some(server)) = routed {
        return Decision::Route(Route::Mcp {
            server: Some(server.to_owned()),
            args,
        });
    }
    let holds = |set: Option<&[String]>| set.map(|s| s.iter().any(|n| n == name));
    match holds(known.kiwix) {
        Some(true) => return Decision::Route(Route::Kiwix(args)),
        Some(false) => {}
        None => return Decision::Core,
    }
    match holds(known.drive) {
        Some(true) => return Decision::Route(Route::Drive(args)),
        Some(false) => {}
        None => return Decision::Core,
    }
    if name == CURRENT_TIME {
        let timezone = match args.get("timezone") {
            js_json::JValue::Str(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        };
        return Decision::Route(Route::CurrentTime { timezone });
    }
    if name == READ_PROJECT_FILE {
        return Decision::Route(Route::ReadProjectFile(args));
    }
    match known.mcp {
        Some(m) if m.contains(name) => Decision::Route(Route::Mcp { server: None, args }),
        Some(_) => Decision::Route(Route::Refused(format!("ERROR: unknown tool \"{name}\""))),
        None => Decision::Core,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_offer_alone_settles_the_refusals_and_the_routed_calls() {
        let allowed = set(&["a", "page", "web"]);
        let routes = vec![("a".to_string(), "srv".to_string())];
        let k = Knowledge::offer_only(&allowed, &routes, "page");
        assert_eq!(
            decide("b", "{}", &k),
            Decision::Route(Route::Refused(
                "ERROR: tool \"b\" is not enabled for this project".into()
            ))
        );
        assert!(matches!(
            decide("a", "[1]", &k),
            Decision::Route(Route::Refused(t)) if t.starts_with("ERROR: tool arguments must be a JSON object")
        ));
        assert!(matches!(
            decide("a", "{", &k),
            Decision::Route(Route::Refused(t)) if t.starts_with("ERROR: tool arguments were not valid JSON")
        ));
        assert!(matches!(
            decide("a", "{}", &k),
            Decision::Route(Route::Mcp { server: Some(s), .. }) if s == "srv"
        ));
        // The page tool's applicability is not known, nor the other sets: core decides.
        assert_eq!(decide("page", "{}", &k), Decision::Core);
        assert_eq!(decide("web", "{}", &k), Decision::Core);
    }

    #[test]
    fn a_routed_page_tool_name_is_the_servers_even_when_applicability_is_unknown() {
        let allowed = set(&["page"]);
        let routes = vec![("page".to_string(), "srv".to_string())];
        let k = Knowledge::offer_only(&allowed, &routes, "page");
        assert!(matches!(
            decide("page", "", &k),
            Decision::Route(Route::Mcp { server: Some(_), .. })
        ));
    }

    #[test]
    fn an_empty_route_still_holds_the_name_against_the_page_tool() {
        let allowed = set(&["page"]);
        let routes = vec![("page".to_string(), String::new())];
        let mut k = Knowledge::offer_only(&allowed, &routes, "page");
        // Held, so not the page tool; not routed, so on to the sets, which are unknown.
        assert_eq!(decide("page", "{}", &k), Decision::Core);
        let none: Vec<String> = Vec::new();
        let mcp = HashSet::new();
        k.kiwix = Some(&none);
        k.drive = Some(&none);
        k.mcp = Some(&mcp);
        assert_eq!(
            decide("page", "{}", &k),
            Decision::Route(Route::Refused("ERROR: unknown tool \"page\"".into()))
        );
    }
}
