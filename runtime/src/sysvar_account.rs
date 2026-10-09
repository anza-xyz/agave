#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    solana_account::{
        AccountSharedData, InheritableAccountFields, ReadableAccount, WritableAccount,
        state_traits::StateMutWincode,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::sysvar,
    solana_sysvar_id::SysvarId,
};

#[allow(deprecated)]
fn canonical_data_len(sysvar_id: &Pubkey) -> Option<usize> {
    match *sysvar_id {
        sysvar::clock::ID => Some(solana_clock::SIZE),
        sysvar::epoch_rewards::ID => Some(solana_sysvar::epoch_rewards::SIZE),
        sysvar::epoch_schedule::ID => Some(solana_epoch_schedule::SIZE),
        sysvar::fees::ID => Some(solana_sysvar::fees::SIZE),
        sysvar::last_restart_slot::ID => Some(solana_sysvar::last_restart_slot::SIZE),
        sysvar::recent_blockhashes::ID => Some(solana_sysvar::recent_blockhashes::SIZE),
        sysvar::rent::ID => Some(solana_rent::SIZE),
        sysvar::rewards::ID => Some(solana_sysvar::rewards::SIZE),
        sysvar::slot_hashes::ID => Some(solana_slot_hashes::SIZE),
        sysvar::slot_history::ID => Some(solana_slot_history::SIZE),
        sysvar::stake_history::ID => Some(solana_stake_history::SIZE),
        _ => None,
    }
}

// Preserve the canonical account size for built-in sysvars, but never allocate less than the
// current serialized value requires. Unknown sysvar IDs have no canonical size, so they use the
// serialized size directly.
fn required_data_len(sysvar_id: &Pubkey, serialized_len: usize) -> usize {
    canonical_data_len(sysvar_id)
        .unwrap_or(serialized_len)
        .max(serialized_len)
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) fn create_account<T>(sysvar: &T, fields: InheritableAccountFields) -> AccountSharedData
where
    T: wincode::SchemaWrite<solana_account::WincodeConfig, Src = T>
        + for<'de> wincode::SchemaRead<'de, solana_account::WincodeConfig, Dst = T>
        + SysvarId,
{
    let serialized_len = wincode::config::serialized_size(sysvar, solana_account::WINCODE_CONFIG)
        .expect("failed to get serialized sysvar size") as usize;
    let (lamports, rent_epoch) = fields;
    let mut account = AccountSharedData::new_data_with_space(
        lamports,
        sysvar,
        required_data_len(&T::id(), serialized_len),
        &sysvar::id(),
    )
    .unwrap_or_else(|err| panic!("failed to serialize sysvar {}: {err}", T::id()));
    account.set_rent_epoch(rent_epoch);
    account
}

pub(crate) fn from_account<T>(account: &AccountSharedData) -> Option<T>
where
    T: wincode::DeserializeOwned<Dst = T> + SysvarId,
{
    wincode::deserialize(account.data()).ok()
}

pub(crate) fn to_account<T>(sysvar: &T, account: &mut impl WritableAccount) -> Option<()>
where
    T: wincode::Serialize<Src = T> + SysvarId,
{
    wincode::serialize_into(account.data_as_mut_slice(), sysvar).ok()
}
