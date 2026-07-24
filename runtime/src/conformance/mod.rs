//! Solana runtime conformance harnesses.

#[cfg(feature = "conformance")]
pub mod block;
#[cfg(feature = "conformance")]
pub mod cost;
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
    solana_accounts_db::{accounts::Accounts, accounts_db::AccountsDb},
    std::sync::Arc,
};

pub(crate) fn new_accounts_for_tests_single_threaded() -> Accounts {
    Accounts::new(Arc::new(AccountsDb::new_for_tests_single_threaded()))
}

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
