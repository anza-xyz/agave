//! Utilities for driving program cache scenarios through `Bank`.

use {
    crate::bank::Bank,
    solana_program_runtime::{
        loaded_programs::ProgramRuntimeEnvironment,
        program_cache_entry::{ProgramCacheEntry, ProgramCacheEntryType},
    },
    solana_pubkey::Pubkey,
    std::sync::Arc,
};

pub enum EntryType {
    FailedVerification,
    Loaded,
}

pub struct ExpectedEntry<'a> {
    pub entry_type: EntryType,
    pub deployment_slot: u64,
    pub environment: &'a ProgramRuntimeEnvironment,
}

pub fn current_env(bank: &Bank) -> ProgramRuntimeEnvironment {
    bank.get_transaction_processor()
        .program_runtime_environment_for_epoch(bank.epoch())
}

fn get_entry_env(entry: &ProgramCacheEntry) -> &ProgramRuntimeEnvironment {
    entry
        .program
        .get_environment()
        .expect("entry carries an environment")
}

fn find_all_cached_entries(bank: &Bank, program_id: &Pubkey) -> Vec<Arc<ProgramCacheEntry>> {
    let processor = bank.get_transaction_processor();
    let cache = processor.global_program_cache.read().unwrap();
    cache
        .get_flattened_entries_for_tests()
        .into_iter()
        .filter_map(|(key, entry)| (key == *program_id).then_some(entry))
        .collect()
}

pub fn assert_cache_contents(bank: &Bank, program_id: &Pubkey, expected_entries: &[ExpectedEntry]) {
    let entries = find_all_cached_entries(bank, program_id);

    let len = entries.len();
    let expected_len = expected_entries.len();
    assert_eq!(
        len, expected_len,
        "program should have {expected_len} entries. Got {len}: {entries:?}",
    );

    for (i, expected_entry) in expected_entries.iter().enumerate() {
        let entry = entries.get(i).unwrap();
        match expected_entry.entry_type {
            EntryType::FailedVerification => {
                assert!(
                    matches!(entry.program, ProgramCacheEntryType::FailedVerification(_)),
                    "Entry {i} should be cached as a failed verification entry",
                );
            }
            EntryType::Loaded => {
                assert!(
                    matches!(entry.program, ProgramCacheEntryType::Loaded(_)),
                    "Entry {i} should be cached as a loaded entry",
                );
            }
        }
        let deployment_slot = entry.deployment_slot;
        let expected_deployment_slot = expected_entry.deployment_slot;
        assert_eq!(
            deployment_slot, expected_deployment_slot,
            "Entry {i} should have deployment slot of {expected_deployment_slot}. Got \
             {deployment_slot}",
        );
        let environment = get_entry_env(entry);
        let expected_environment = expected_entry.environment;
        assert_eq!(
            environment, expected_environment,
            "Entry {i} has unexpected environment",
        );
    }
}
