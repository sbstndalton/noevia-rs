//! Argon2 verification of the PHC strings Node's `@node-rs/argon2` writes
//! (`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`, auth.cjs createPasswordHash and
//! app-passwords.cjs). The parameters come from the stored string, as `verify` reads them.
//!
//! Stricter than Node in one way: a stored hash asking for more than [`MAX_M_COST`] KiB,
//! [`MAX_T_COST`] passes or [`MAX_P_COST`] lanes is refused instead of computed (Node only ever
//! writes 19456/2/1), so a tampered row cannot make one request pin a core and gigabytes.

use argon2::password_hash::{PasswordHash, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, PasswordHasher, Version};
use std::sync::OnceLock;

pub const MAX_M_COST: u32 = 256 * 1024;
pub const MAX_T_COST: u32 = 16;
pub const MAX_P_COST: u32 = 16;

/// `await verify(phc, password).catch(() => false)`.
pub fn verify(phc: &str, password: &str) -> bool {
    let Ok(hash) = PasswordHash::new(phc) else {
        return false;
    };
    let Ok(params) = Params::try_from(&hash) else {
        return false;
    };
    if params.m_cost() > MAX_M_COST || params.t_cost() > MAX_T_COST || params.p_cost() > MAX_P_COST
    {
        return false;
    }
    Argon2::default()
        .verify_password(password.as_bytes(), &hash)
        .is_ok()
}

/// A fixed Argon2id hash with Node's parameters, verified against when there is no stored hash
/// to check, so a missing account costs the same time as a wrong password (no user-existence
/// timing oracle). It matches no password anyone can send: the preimage is not a valid app
/// password and the result is discarded.
fn decoy() -> &'static str {
    static DECOY: OnceLock<String> = OnceLock::new();
    DECOY.get_or_init(|| {
        let params = Params::new(19456, 2, 1, Some(32)).ok();
        let salt = SaltString::from_b64("bm9ldmlhZGVjb3lzYWx0").ok();
        match (params, salt) {
            (Some(params), Some(salt)) => Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
                .hash_password(b"\0noevia decoy\0", &salt)
                .map(|h| h.to_string())
                .unwrap_or_default(),
            _ => String::new(),
        }
    })
}

/// Spend one verification's time against the decoy; always false.
pub fn burn(password: &str) -> bool {
    let _ = verify(decoy(), password);
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn node_style(password: &str) -> String {
        let params = Params::new(19456, 2, 1, Some(32)).unwrap();
        let salt = SaltString::from_b64("c3ludGhldGljc2FsdA").unwrap();
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    #[test]
    fn verifies_node_shaped_hashes() {
        let h = node_style("correct horse battery staple");
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(verify(&h, "correct horse battery staple"));
        assert!(!verify(&h, "correct horse battery stapl"));
        assert!(!verify("not a hash", "x"));
        assert!(!verify("", ""));
        assert!(!decoy().is_empty());
        assert!(!burn("anything"));
    }

    #[test]
    fn refuses_extreme_parameters() {
        let h = node_style("pw");
        let greedy = h.replace("m=19456", "m=4194304");
        assert!(!verify(&greedy, "pw"));
        let slow = h.replace("t=2", "t=1000");
        assert!(!verify(&slow, "pw"));
    }
}
