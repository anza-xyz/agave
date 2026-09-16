use {
    solana_account::{
        AccountSharedData, InheritableAccountFields, ReadableAccount, WritableAccount,
    },
    solana_clock::Epoch,
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

fn new_account(lamports: u64, rent_epoch: Epoch, data_len: usize) -> AccountSharedData {
    let mut account = AccountSharedData::new(lamports, data_len, &sysvar::id());
    account.set_rent_epoch(rent_epoch);
    account
}

pub(crate) fn create_account<T>(sysvar: &T, fields: InheritableAccountFields) -> AccountSharedData
where
    T: wincode::Serialize<Src = T> + SysvarId,
{
    let serialized_len =
        wincode::serialized_size(sysvar).expect("failed to get serialized sysvar size") as usize;
    let (lamports, rent_epoch) = fields;
    let mut account = new_account(
        lamports,
        rent_epoch,
        required_data_len(&T::id(), serialized_len),
    );
    wincode::serialize_into(account.data_as_mut_slice(), sysvar).unwrap();
    account
}

pub(crate) fn create_account_with_bincode<T>(
    sysvar: &T,
    fields: InheritableAccountFields,
) -> AccountSharedData
where
    T: serde::Serialize + SysvarId,
{
    let serialized_len =
        bincode::serialized_size(sysvar).expect("failed to get serialized sysvar size") as usize;
    let (lamports, rent_epoch) = fields;
    let mut account = new_account(
        lamports,
        rent_epoch,
        required_data_len(&T::id(), serialized_len),
    );
    bincode::serialize_into(account.data_as_mut_slice(), sysvar).unwrap();
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
