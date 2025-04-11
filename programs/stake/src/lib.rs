#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![allow(clippy::arithmetic_side_effects)]
use solana_genesis_config::GenesisConfig;
#[deprecated(
    since = "1.8.0",
    note = "Please use `solana_sdk_ids::sysvar::stake::id` instead"
)]
pub use solana_sdk_ids::stake::{check_id, id};

pub mod config;
#[deprecated(since = "2.2.0")]
pub mod points;
pub mod stake_instruction;
pub mod stake_state;

pub fn add_genesis_accounts(genesis_config: &mut GenesisConfig) -> u64 {
    config::add_genesis_account(genesis_config)
}

/// The minimum stake amount that can be delegated, in lamports.
/// NOTE: This is also used to calculate the minimum balance of a delegated stake account,
/// which is the rent exempt reserve _plus_ the minimum stake delegation.
#[inline(always)]
pub fn get_minimum_delegation() -> u64 {
    1
}
