#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![allow(clippy::arithmetic_side_effects)]
use solana_native_token::LAMPORTS_PER_SOL;
#[deprecated(
    since = "1.8.0",
    note = "Please use `solana_sdk_ids::sysvar::stake::id` instead"
)]
pub use solana_sdk_ids::stake::{check_id, id};

pub mod stake_state;

/// The minimum stake amount that can be delegated, in lamports.
/// NOTE: This is also used to calculate the minimum balance of a delegated stake account,
/// which is the rent exempt reserve _plus_ the minimum stake delegation.
#[inline(always)]
pub fn get_minimum_delegation(is_stake_raise_minimum_delegation_to_1_sol_active: bool) -> u64 {
    if is_stake_raise_minimum_delegation_to_1_sol_active {
        const MINIMUM_DELEGATION_SOL: u64 = 1;
        MINIMUM_DELEGATION_SOL * LAMPORTS_PER_SOL
    } else {
        1
    }
}
