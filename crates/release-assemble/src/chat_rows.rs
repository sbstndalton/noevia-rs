//! The chat rows pairing (council p12-3): `assemble-release.sh`'s guard for chat turns on provider rows and on
//! the sign-in row (noevia-rs `chat_provider.rs`, Core's `rust-chat-native.cjs` `plainProviderRow`/`signinRow`).
//! Its shell block is new in noevia council/p12-3-chat-rows-guard and kept verbatim in
//! `fixtures/assemble-release-chat-rows-excerpt.sh`; `tests/guard_chat_rows.rs` compares the two on generated
//! release trees.
//!
//! Core hands such a turn to the front per turn, only for a front whose hand-off caps name the row's cap, so a
//! mismatched pair never answers twice. What the guard catches is a switch that would change nothing: a front
//! whose FEATURES says it runs these turns (`chat-providers-v1`, `chat-signin-v1`) must declare the cap and the
//! switch the owner flips, and Core must hand the turns off on the same cap. A Core that hands them off beside a
//! front without the entry stays as today (that front never names the cap). Core lifts the sign-in row only for
//! a front naming both caps, so `chat-signin-v1` needs `chat-providers-v1`.
//!
//! Inert: nothing in the release path calls this crate yet.

use crate::bridged::code_has;
use crate::guard::{move_capability, read_file, MoveKind};
use std::path::Path;

/// The front's FEATURES entry for provider-row turns (noevia-server `chat_provider::PROVIDERS_FEATURE`).
pub const PROVIDERS_FEATURE: &str = "chat-providers-v1";
/// The front's FEATURES entry for sign-in row turns (noevia-server `chat_provider::SIGNIN_FEATURE`).
pub const SIGNIN_FEATURE: &str = "chat-signin-v1";
/// The hand-off cap of provider-row turns, on both sides.
pub const PROVIDERS_CAP: &str = "providers";
/// The hand-off cap of sign-in row turns, on both sides.
pub const SIGNIN_CAP: &str = "signin-transport-v1";
/// The front's switches and Core's.
pub const PROVIDERS_SWITCH: &str = "NOEVIA_RUST_CHAT_PROVIDERS";
pub const SIGNIN_SWITCH: &str = "NOEVIA_RUST_CHAT_SIGNIN";
pub const CORE_SIGNIN_SWITCH: &str = "NOEVIA_RUST_SIGNIN_TRANSPORT";

const LABEL: &str = "chat rows pairing";

/// What the guard read from one release tree (each a verdict of the shell's primitives).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChatRows {
    /// FEATURES lists [`PROVIDERS_FEATURE`] exactly once.
    pub rs_providers: bool,
    /// FEATURES lists [`SIGNIN_FEATURE`] exactly once.
    pub rs_signin: bool,
    /// `chat_provider.rs` declares `CAPS_TOKEN` [`PROVIDERS_CAP`] and `SWITCH` [`PROVIDERS_SWITCH`].
    pub rs_providers_decl: bool,
    /// `chat_provider.rs` declares `SIGNIN_CAPS_TOKEN` [`SIGNIN_CAP`] and `SIGNIN_SWITCH` [`SIGNIN_SWITCH`].
    pub rs_signin_decl: bool,
    /// Core's `plainProviderRow` and chat.cjs's `providers` cap.
    pub core_providers: bool,
    /// Core's exported `SIGNIN_CAP`/`SIGNIN_SWITCH`, `signinRow` and chat.cjs's sign-in cap.
    pub core_signin: bool,
}

impl ChatRows {
    /// Read the facts from a release tree laid out as the assembly lays it out (`rs/`, `core/`).
    pub fn read(tree: &Path) -> ChatRows {
        let lib = read_file(&tree.join("rs/bins/noevia-server/src/lib.rs"));
        let provider = read_file(&tree.join("rs/bins/noevia-server/src/chat_provider.rs"));
        let native = read_file(&tree.join("core/server/rust-chat-native.cjs"));
        let chat = read_file(&tree.join("core/server/chat.cjs"));
        let (lib, provider, native, chat) = (
            lib.as_deref(),
            provider.as_deref(),
            native.as_deref(),
            chat.as_deref(),
        );
        ChatRows {
            rs_providers: move_capability(lib, "FEATURES", PROVIDERS_FEATURE, MoveKind::Features),
            rs_signin: move_capability(lib, "FEATURES", SIGNIN_FEATURE, MoveKind::Features),
            rs_providers_decl: move_capability(
                provider,
                "CAPS_TOKEN",
                PROVIDERS_CAP,
                MoveKind::Rust,
            ) && move_capability(
                provider,
                "SWITCH",
                PROVIDERS_SWITCH,
                MoveKind::Rust,
            ),
            rs_signin_decl: move_capability(
                provider,
                "SIGNIN_CAPS_TOKEN",
                SIGNIN_CAP,
                MoveKind::Rust,
            ) && move_capability(
                provider,
                "SIGNIN_SWITCH",
                SIGNIN_SWITCH,
                MoveKind::Rust,
            ),
            core_providers: code_has(native, "^function plainProviderRow[(]")
                && code_has(chat, "providers: handoffCaps[.]has[(]'providers'[)]"),
            core_signin: move_capability(native, "SIGNIN_CAP", SIGNIN_CAP, MoveKind::Js)
                && move_capability(native, "SIGNIN_SWITCH", CORE_SIGNIN_SWITCH, MoveKind::Js)
                && code_has(native, "^function signinRow[(]")
                && code_has(chat, "signin: handoffCaps[.]has[(]native[.]SIGNIN_CAP[)]"),
        }
    }

    /// The block's verdict, in its order: `Err` holds the `die` text without the script's prefix.
    pub fn verdict(&self) -> Result<(), String> {
        if self.rs_signin && !self.rs_providers {
            return Err(format!(
                "{LABEL}: the Rust front lists {SIGNIN_FEATURE} without {PROVIDERS_FEATURE} (Core lifts the sign-in row only for a front naming both caps)"
            ));
        }
        if self.rs_providers {
            if !self.rs_providers_decl {
                return Err(format!(
                    "{LABEL}: the Rust front lists {PROVIDERS_FEATURE} but chat_provider.rs does not declare CAPS_TOKEN {PROVIDERS_CAP} and SWITCH {PROVIDERS_SWITCH}"
                ));
            }
            if !self.core_providers {
                return Err(format!(
                    "{LABEL}: the Rust front lists {PROVIDERS_FEATURE} but Core never hands a provider row's turn off (no plainProviderRow in rust-chat-native.cjs or no providers cap in chat.cjs): {PROVIDERS_SWITCH} would change nothing"
                ));
            }
        }
        if self.rs_signin {
            if !self.rs_signin_decl {
                return Err(format!(
                    "{LABEL}: the Rust front lists {SIGNIN_FEATURE} but chat_provider.rs does not declare SIGNIN_CAPS_TOKEN {SIGNIN_CAP} and SIGNIN_SWITCH {SIGNIN_SWITCH}"
                ));
            }
            if !self.core_signin {
                return Err(format!(
                    "{LABEL}: the Rust front lists {SIGNIN_FEATURE} but Core never hands a sign-in row's turn off on {SIGNIN_CAP} (rust-chat-native.cjs SIGNIN_CAP, SIGNIN_SWITCH {CORE_SIGNIN_SWITCH} and signinRow, chat.cjs's signin cap): {SIGNIN_SWITCH} would change nothing"
                ));
            }
        }
        Ok(())
    }
}

/// The pairing over a release tree: `Ok` when the block passes, else its refusal.
pub fn chat_rows_pairing(tree: &Path) -> Result<(), String> {
    ChatRows::read(tree).verdict()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const ALL: ChatRows = ChatRows {
        rs_providers: true,
        rs_signin: true,
        rs_providers_decl: true,
        rs_signin_decl: true,
        core_providers: true,
        core_signin: true,
    };

    #[test]
    fn a_front_without_either_entry_passes_whatever_core_does() {
        for core_providers in [false, true] {
            for core_signin in [false, true] {
                let f = ChatRows {
                    core_providers,
                    core_signin,
                    rs_providers_decl: core_providers,
                    ..ChatRows::default()
                };
                assert_eq!(f.verdict(), Ok(()));
            }
        }
    }

    #[test]
    fn the_whole_pair_passes() {
        assert_eq!(ALL.verdict(), Ok(()));
        let providers_only = ChatRows {
            rs_signin: false,
            rs_signin_decl: false,
            core_signin: false,
            ..ALL
        };
        assert_eq!(providers_only.verdict(), Ok(()));
    }

    #[test]
    fn signin_without_providers_is_refused_first() {
        let f = ChatRows {
            rs_providers: false,
            ..ChatRows::default()
        };
        let f = ChatRows {
            rs_signin: true,
            ..f
        };
        let e = f.verdict().unwrap_err();
        assert!(e.contains("without chat-providers-v1"), "{e}");
    }

    #[test]
    fn each_missing_half_has_its_own_refusal_in_the_blocks_order() {
        let cases = [
            (
                ChatRows {
                    rs_providers_decl: false,
                    ..ALL
                },
                "does not declare CAPS_TOKEN providers",
            ),
            (
                ChatRows {
                    core_providers: false,
                    ..ALL
                },
                "NOEVIA_RUST_CHAT_PROVIDERS would change nothing",
            ),
            (
                ChatRows {
                    rs_signin_decl: false,
                    ..ALL
                },
                "does not declare SIGNIN_CAPS_TOKEN",
            ),
            (
                ChatRows {
                    core_signin: false,
                    ..ALL
                },
                "NOEVIA_RUST_CHAT_SIGNIN would change nothing",
            ),
            // The provider checks come before the sign-in ones.
            (
                ChatRows {
                    core_providers: false,
                    core_signin: false,
                    ..ALL
                },
                "NOEVIA_RUST_CHAT_PROVIDERS would change nothing",
            ),
        ];
        for (f, want) in cases {
            let e = f.verdict().unwrap_err();
            assert!(e.starts_with("chat rows pairing: "), "{e}");
            assert!(e.contains(want), "{e} / {want}");
        }
    }

    #[test]
    fn core_sign_in_alone_beside_a_providers_front_passes() {
        // A front that runs provider rows but not sign-in rows never names the sign-in cap.
        let f = ChatRows {
            rs_signin: false,
            rs_signin_decl: false,
            ..ALL
        };
        assert_eq!(f.verdict(), Ok(()));
    }
}
