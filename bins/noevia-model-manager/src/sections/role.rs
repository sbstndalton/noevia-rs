//! The system routing role: the value `models.roles.json` gives a section that the router runs as
//! its own service (never configured, tuned or auto-configured here).
//!
//! noevia#2093 phase 1: readers accept both the role name and the legacy role key. The legacy key
//! stays accepted until phase 3 of that migration; writers, the oracle generators and the files on
//! disk do not change in phase 1. A rename or delete of a section moves or drops whatever role
//! value the file holds, so a file that has the legacy key keeps it.

/// The role name for the system routing role.
pub const SYSTEM_ROUTING_ROLE: &str = "system-routing";

/// The legacy spelling of the role in existing files. The literal is kept where the code must match
/// it: the deprecated name/path rule (a name prefix), the `laya_section` error code (an API
/// contract) and the wording of the route's own answer.
pub const LEGACY_SYSTEM_ROUTING_ROLE: &str = "laya";

/// Every legacy role key readers still accept as the system routing role (until phase 3 of
/// noevia#2093).
pub const LEGACY_SYSTEM_ROUTING_ROLES: &[&str] = &[LEGACY_SYSTEM_ROUTING_ROLE];

/// True when `role` (a roles-file value, already stripped and lower-cased by the roles reader) is
/// the system routing role, spelled by name or by the legacy key.
pub fn is_system_routing_role(role: &str) -> bool {
    role == SYSTEM_ROUTING_ROLE || LEGACY_SYSTEM_ROUTING_ROLES.contains(&role)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_role_name_and_the_legacy_key_are_both_the_system_routing_role() {
        assert!(is_system_routing_role("system-routing"));
        assert!(is_system_routing_role("laya"));
        for legacy in LEGACY_SYSTEM_ROUTING_ROLES {
            assert!(is_system_routing_role(legacy), "{legacy}");
        }
    }

    #[test]
    fn an_unrelated_role_is_not_the_system_routing_role() {
        for other in [
            "",
            "chat",
            "embedding",
            "system",
            "routing",
            "system-routing-x",
            "x-system-routing",
            "system_routing",
            "laya-chat",
            "layas",
            "la",
        ] {
            assert!(!is_system_routing_role(other), "{other:?}");
        }
    }

    #[test]
    fn the_legacy_key_is_one_distinct_value() {
        assert_ne!(SYSTEM_ROUTING_ROLE, LEGACY_SYSTEM_ROUTING_ROLE);
        assert_eq!(LEGACY_SYSTEM_ROUTING_ROLES, [LEGACY_SYSTEM_ROUTING_ROLE]);
    }
}
