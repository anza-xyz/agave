//! Sysvar account construction for tests and benches.

#![cfg(feature = "dev-context-only-utils")]

use {
    solana_account::{
        AccountSharedData, WINCODE_CONFIG, WincodeConfig, state_traits::StateMutWincode,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::sysvar,
    solana_sysvar_id::SysvarId,
    wincode::{SchemaRead, SchemaWrite},
};

/// The canonical on-chain data length of the sysvar account at `sysvar_id`.
fn canonical_data_len(sysvar_id: &Pubkey) -> usize {
    match *sysvar_id {
        sysvar::clock::ID => solana_sysvar::clock::SIZE,
        sysvar::epoch_rewards::ID => solana_sysvar::epoch_rewards::SIZE,
        sysvar::epoch_schedule::ID => solana_sysvar::epoch_schedule::SIZE,
        sysvar::fees::ID => solana_sysvar::fees::SIZE,
        sysvar::last_restart_slot::ID => solana_sysvar::last_restart_slot::SIZE,
        sysvar::recent_blockhashes::ID => solana_sysvar::recent_blockhashes::SIZE,
        sysvar::rent::ID => solana_sysvar::rent::SIZE,
        sysvar::rewards::ID => solana_sysvar::rewards::SIZE,
        sysvar::slot_hashes::ID => solana_sysvar::slot_hashes::SIZE,
        sysvar::slot_history::ID => solana_sysvar::slot_history::SIZE,
        sysvar::stake_history::ID => solana_sysvar::stake_history::SIZE,
        id => panic!("unsupported sysvar: {id}"),
    }
}

/// Build the sysvar account holding `value`, sized to the sysvar's canonical
/// data length, or to the serialized value when that is larger.
pub fn create_sysvar_account<T>(value: &T) -> AccountSharedData
where
    T: SchemaWrite<WincodeConfig, Src = T>
        + for<'de> SchemaRead<'de, WincodeConfig, Dst = T>
        + SysvarId,
{
    let serialized_len = wincode::config::serialized_size(value, WINCODE_CONFIG).unwrap() as usize;
    let space = canonical_data_len(&T::id()).max(serialized_len);
    AccountSharedData::new_data_with_space(1, value, space, &sysvar::id()).unwrap()
}

/// [`create_sysvar_account`], keyed by the sysvar's address.
pub fn keyed_sysvar_account<T>(value: &T) -> (Pubkey, AccountSharedData)
where
    T: SchemaWrite<WincodeConfig, Src = T>
        + for<'de> SchemaRead<'de, WincodeConfig, Dst = T>
        + SysvarId,
{
    (T::id(), create_sysvar_account(value))
}
