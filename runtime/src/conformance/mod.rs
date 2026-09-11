//! Solana runtime conformance harnesses.

#[cfg(feature = "conformance")]
pub mod block;
pub mod txn;

#[cfg(feature = "conformance")]
use {
    protosol::protos::{
        AcctState, BlockhashQueueEntry as ProtoBlockhashQueueEntry,
        FeeRateGovernor as ProtoFeeRateGovernor,
    },
    solana_account::AccountSharedData,
    solana_accounts_db::blockhash_queue::BlockhashQueue,
    solana_fee_calculator::FeeRateGovernor,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_svm::conformance::account_state::account_from_proto,
};
use {
    solana_accounts_db::{
        accounts::Accounts,
        accounts_db::{ACCOUNTS_DB_CONFIG_FOR_TESTING, AccountsDb, AccountsDbConfig},
    },
    std::{num::NonZeroUsize, sync::Arc},
};

/// Parse the input accounts into keyed `AccountSharedData`, dropping zero-lamport
/// accounts (treated as nonexistent).
#[cfg(feature = "conformance")]
pub(crate) fn deserialize_accounts(accounts: &[AcctState]) -> Vec<(Pubkey, AccountSharedData)> {
    accounts
        .iter()
        .filter(|account| account.lamports > 0)
        .map(|account| {
            let (pubkey, account) = account_from_proto(account.clone());
            (pubkey, account.into())
        })
        .collect()
}

#[cfg(feature = "conformance")]
pub(crate) fn restore_blockhash_queue(entries: &[ProtoBlockhashQueueEntry]) -> BlockhashQueue {
    let mut blockhash_queue = BlockhashQueue::default();
    for entry in entries {
        let blockhash =
            Hash::new_from_array(<[u8; 32]>::try_from(entry.blockhash.as_slice()).unwrap());
        blockhash_queue.register_hash(&blockhash, entry.lamports_per_signature);
    }
    blockhash_queue
}

#[cfg(feature = "conformance")]
pub(crate) fn fee_rate_governor_from_proto(
    value: &ProtoFeeRateGovernor,
    lamports_per_signature: u64,
) -> FeeRateGovernor {
    FeeRateGovernor {
        lamports_per_signature,
        target_lamports_per_signature: value.target_lamports_per_signature,
        target_signatures_per_slot: value.target_signatures_per_slot,
        min_lamports_per_signature: value.min_lamports_per_signature,
        max_lamports_per_signature: value.max_lamports_per_signature,
        burn_percent: value.burn_percent as u8,
    }
}

pub(crate) fn new_accounts_for_tests_single_threaded() -> Accounts {
    Accounts::new(Arc::new(AccountsDb::new_for_tests_with_config(
        Vec::new(),
        new_accounts_db_config_for_tests_single_threaded(),
    )))
}

pub(crate) fn new_accounts_db_config_for_tests_single_threaded() -> AccountsDbConfig {
    let single_thread = NonZeroUsize::new(1).unwrap();
    AccountsDbConfig {
        num_background_threads: Some(single_thread),
        num_foreground_threads: Some(single_thread),
        read_cache_num_shards: Some(2),
        skip_initial_hash_calc: true,
        ..ACCOUNTS_DB_CONFIG_FOR_TESTING
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_accounts_db::accounts_index::IndexLimit};

    #[test]
    fn test_new_accounts_for_tests_single_threaded() {
        let accounts = new_accounts_for_tests_single_threaded();
        let accounts_db = &accounts.accounts_db;
        assert!(accounts_db.skip_initial_hash_calc);
        assert_eq!(accounts_db.thread_pool_foreground.current_num_threads(), 1);
        assert_eq!(accounts_db.thread_pool_background.current_num_threads(), 1);
    }

    #[test]
    fn test_new_accounts_db_config_for_tests_single_threaded_config() {
        let one = NonZeroUsize::new(1).unwrap();
        let config = new_accounts_db_config_for_tests_single_threaded();
        let index = config.index.unwrap();
        assert_eq!(index.bins, Some(2));
        assert_eq!(index.num_flush_threads, Some(one));
        assert!(matches!(index.index_limit, IndexLimit::InMemOnly));
        assert!(config.skip_initial_hash_calc);
        assert!(!config.verify_index);
        assert_eq!(config.read_cache_num_shards, Some(2));
        assert_eq!(config.num_background_threads, Some(one));
        assert_eq!(config.num_foreground_threads, Some(one));
    }
}
