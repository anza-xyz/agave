#![cfg(feature = "conformance")]

//! Gossip conformance tests and fixture generation.

mod fixtures;
mod helpers;
mod tests;

use protosol::protos::{GossipEffects, gossip_msg};

pub(crate) const MAX_WALLCLOCK: u64 = 1_000_000_000_000_000;

pub(crate) fn get_effects(input: &[u8]) -> GossipEffects {
    solana_gossip::gossip_decode_to_effects(input)
}

pub(crate) fn check(input: &[u8], expect_valid: bool) {
    let effects = get_effects(input);
    assert_eq!(
        effects.valid,
        expect_valid,
        "effects.valid mismatch: input len={}, expected {expect_valid}",
        input.len(),
    );
    if !expect_valid {
        assert!(effects.msg.is_none(), "invalid input should have no msg");
    } else {
        assert!(effects.msg.is_some(), "valid input should have a msg");
    }
}

/// Returns the inner gossip_msg::Msg variant from effects, panicking if invalid.
pub(crate) fn unwrap_msg(effects: &GossipEffects) -> &gossip_msg::Msg {
    effects
        .msg
        .as_ref()
        .expect("effects.msg is None")
        .msg
        .as_ref()
        .expect("msg.msg is None")
}
