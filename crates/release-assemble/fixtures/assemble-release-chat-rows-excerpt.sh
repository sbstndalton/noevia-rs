# Chat turns on provider rows and on the sign-in row (council p8-3 and p10-1; noevia-rs chat_provider.rs, Core's
# rust-chat-native.cjs plainProviderRow/signinRow): Core hands such a turn to the front per turn, only for a front
# whose hand-off caps name the row's cap. The front's FEATURES entry chat-providers-v1 (chat-signin-v1) says the
# build runs them; then the front must declare the cap and switch the owner flips (NOEVIA_RUST_CHAT_PROVIDERS,
# NOEVIA_RUST_CHAT_SIGNIN) and Core must hand them off on the same cap (the sign-in row beside its own
# NOEVIA_RUST_SIGNIN_TRANSPORT), or flipping the switch would change nothing. A Core that hands them off beside a
# front without the entry stays as today: that front never names the cap. Core lifts the sign-in row only for a
# front naming both caps, so chat-signin-v1 needs chat-providers-v1.
cr_rs_providers=0
move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES chat-providers-v1 features && cr_rs_providers=1
cr_rs_signin=0
move_capability "$tree/rs/bins/noevia-server/src/lib.rs" FEATURES chat-signin-v1 features && cr_rs_signin=1
if [ "$cr_rs_signin" = 1 ] && [ "$cr_rs_providers" = 0 ]; then
  die "chat rows pairing: the Rust front lists chat-signin-v1 without chat-providers-v1 (Core lifts the sign-in row only for a front naming both caps)"
fi
if [ "$cr_rs_providers" = 1 ]; then
  { move_capability "$tree/rs/bins/noevia-server/src/chat_provider.rs" CAPS_TOKEN providers rust \
    && move_capability "$tree/rs/bins/noevia-server/src/chat_provider.rs" SWITCH NOEVIA_RUST_CHAT_PROVIDERS rust; } \
    || die "chat rows pairing: the Rust front lists chat-providers-v1 but chat_provider.rs does not declare CAPS_TOKEN providers and SWITCH NOEVIA_RUST_CHAT_PROVIDERS"
  { code_has "$tree/core/server/rust-chat-native.cjs" "^function plainProviderRow[(]" \
    && code_has "$tree/core/server/chat.cjs" "providers: handoffCaps[.]has[(]'providers'[)]"; } \
    || die "chat rows pairing: the Rust front lists chat-providers-v1 but Core never hands a provider row's turn off (no plainProviderRow in rust-chat-native.cjs or no providers cap in chat.cjs): NOEVIA_RUST_CHAT_PROVIDERS would change nothing"
fi
if [ "$cr_rs_signin" = 1 ]; then
  { move_capability "$tree/rs/bins/noevia-server/src/chat_provider.rs" SIGNIN_CAPS_TOKEN signin-transport-v1 rust \
    && move_capability "$tree/rs/bins/noevia-server/src/chat_provider.rs" SIGNIN_SWITCH NOEVIA_RUST_CHAT_SIGNIN rust; } \
    || die "chat rows pairing: the Rust front lists chat-signin-v1 but chat_provider.rs does not declare SIGNIN_CAPS_TOKEN signin-transport-v1 and SIGNIN_SWITCH NOEVIA_RUST_CHAT_SIGNIN"
  { move_capability "$tree/core/server/rust-chat-native.cjs" SIGNIN_CAP signin-transport-v1 js \
    && move_capability "$tree/core/server/rust-chat-native.cjs" SIGNIN_SWITCH NOEVIA_RUST_SIGNIN_TRANSPORT js \
    && code_has "$tree/core/server/rust-chat-native.cjs" "^function signinRow[(]" \
    && code_has "$tree/core/server/chat.cjs" "signin: handoffCaps[.]has[(]native[.]SIGNIN_CAP[)]"; } \
    || die "chat rows pairing: the Rust front lists chat-signin-v1 but Core never hands a sign-in row's turn off on signin-transport-v1 (rust-chat-native.cjs SIGNIN_CAP, SIGNIN_SWITCH NOEVIA_RUST_SIGNIN_TRANSPORT and signinRow, chat.cjs's signin cap): NOEVIA_RUST_CHAT_SIGNIN would change nothing"
fi
