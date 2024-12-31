#![allow(clippy::arithmetic_side_effects)]
pub mod address_generator;
pub mod genesis_accounts;
pub mod stakes;
pub mod unlocks;

use serde::{Deserialize, Serialize};

/// An account where the data is encoded as a Base64 string.
#[derive(Serialize, Deserialize, Debug)]
pub struct Base64Account {
    pub balance: u64,
    pub owner: String,
    pub data: String,
    pub executable: bool,
}

/// A validator account where the data is encoded as a Base64 string.
/// Includes the vote account and stake account.
#[derive(Serialize, Deserialize, Debug)]
pub struct Base64ValidatorAccount {
    pub balance_lamports: u64,
    pub stake_lamports: u64,
    pub identity_account: String,
    pub vote_account: String,
    pub stake_account: String,
}
