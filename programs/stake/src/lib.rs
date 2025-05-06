#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![allow(clippy::arithmetic_side_effects)]
#[deprecated(
    since = "1.8.0",
    note = "Please use `solana_sdk_ids::sysvar::stake::id` instead"
)]
pub use solana_sdk_ids::stake::{check_id, id};
use {solana_genesis_config::GenesisConfig, solana_native_token::LAMPORTS_PER_SOL};

pub mod config;
pub mod epoch_rewards;
#[deprecated(since = "2.2.0")]
pub mod points;
pub mod stake_instruction;
pub mod stake_state;

pub fn add_genesis_accounts(genesis_config: &mut GenesisConfig) -> u64 {
    let config_lamports = config::add_genesis_account(genesis_config);
    let rewards_lamports = epoch_rewards::add_genesis_account(genesis_config);
    config_lamports.saturating_add(rewards_lamports)
}

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
