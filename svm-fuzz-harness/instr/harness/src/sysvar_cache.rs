use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Clock,
    solana_epoch_schedule::EpochSchedule,
    solana_program_runtime::sysvar_cache::SysvarCache,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
};

pub fn setup_sysvar_cache(input_accounts: &[(Pubkey, AccountSharedData)]) -> SysvarCache {
    let mut sysvar_cache = SysvarCache::default();

    sysvar_cache.fill_missing_entries(|pubkey, callbackback| {
        if let Some(account) = input_accounts.iter().find(|(key, _)| key == pubkey) {
            if account.1.lamports() > 0 {
                callbackback(account.1.data());
            }
        }
    });

    // Any default values for missing sysvar values should be set here
    sysvar_cache.fill_missing_entries(|pubkey, callbackback| {
        if *pubkey == solana_sdk_ids::sysvar::clock::id() {
            // Set the default clock slot to something arbitrary beyond 0
            // This prevents DelayedVisibility errors when executing BPF programs
            callbackback(
                &bincode::serialize(&Clock {
                    slot: 10,
                    ..Default::default()
                })
                .unwrap(),
            );
        }
        if *pubkey == solana_sdk_ids::sysvar::epoch_schedule::id() {
            callbackback(&bincode::serialize(&EpochSchedule::default()).unwrap());
        }
        if *pubkey == solana_sdk_ids::sysvar::rent::id() {
            callbackback(&bincode::serialize(&Rent::default()).unwrap());
        }
        if *pubkey == solana_sdk_ids::sysvar::last_restart_slot::id() {
            let slot_val = 5000_u64;
            callbackback(&bincode::serialize(&slot_val).unwrap());
        }
    });

    sysvar_cache
}
