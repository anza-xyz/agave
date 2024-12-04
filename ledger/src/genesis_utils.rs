pub use solana_runtime::genesis_utils::{
    bootstrap_validator_stake_lamports, create_genesis_config_with_leader, GenesisConfigInfo,
};
use solana_sdk::pubkey::Pubkey;

// same as genesis_config::create_genesis_config, but with bootstrap_validator staking logic
//  for the core crate tests
pub fn create_genesis_config(mint_lamports: u64) -> GenesisConfigInfo {
    create_genesis_config_with_leader(
        mint_lamports,
        &Pubkey::new_unique(),
        bootstrap_validator_stake_lamports(),
    )
}
