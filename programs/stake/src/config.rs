//! config for staking
//!  carries variables that the stake program cares about
#[deprecated(
    since = "1.8.0",
    note = "Please use `solana_stake_interface::config` instead"
)]
pub use solana_stake_interface::config::*;
use {
    bincode::deserialize,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_config_program::{create_config_account, get_config_data},
    solana_genesis_config::GenesisConfig,
    solana_transaction_context::BorrowedAccount,
};

#[allow(deprecated)]
pub fn from(account: &BorrowedAccount) -> Option<Config> {
    get_config_data(account.get_data())
        .ok()
        .and_then(|data| deserialize(data).ok())
}

#[allow(deprecated)]
pub fn create_account(lamports: u64, config: &Config) -> AccountSharedData {
    create_config_account(vec![], config, lamports)
}

#[allow(deprecated)]
pub fn add_genesis_account(genesis_config: &mut GenesisConfig) -> u64 {
    let mut account = create_config_account(vec![], &Config::default(), 0);
    let lamports = genesis_config.rent.minimum_balance(account.data().len());

    account.set_lamports(lamports.max(1));

    genesis_config.add_account(solana_sdk_ids::config::id(), account);

    lamports
}
