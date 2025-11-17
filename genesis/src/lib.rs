#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
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

#[derive(Serialize, Deserialize, Debug)]
pub struct ValidatorAccountsFile {
    pub validator_accounts: Vec<StakedValidatorAccountInfo>,
}

/// Info needed to create a staked validator account,
/// including relevant balances and vote- and stake-account addresses
#[derive(Serialize, Deserialize, Debug)]
pub struct StakedValidatorAccountInfo {
    pub balance_lamports: u64,
    pub stake_lamports: u64,
    pub identity_account: String,
    pub vote_account: String,
    pub stake_account: String,
    pub bls_pubkey: Option<String>,
}
