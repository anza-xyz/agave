use {
    solana_account::AccountSharedData,
    solana_accounts_db::accounts_update_notifier_interface::{
        AccountForGeyser, AccountsUpdateNotifierInterface,
    },
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_runtime::bank::BankId,
    solana_transaction::sanitized::SanitizedTransaction,
    std::sync::Mutex,
};

/// Records `(pubkey, txn_index)` for every account update, in order
#[derive(Debug, Default)]
pub(crate) struct RecordingAccountsNotifier {
    pub(crate) updates: Mutex<Vec<(Pubkey, Option<usize>)>>,
}

impl AccountsUpdateNotifierInterface for RecordingAccountsNotifier {
    fn snapshot_notifications_enabled(&self) -> bool {
        false
    }

    fn notify_account_update(
        &self,
        _slot: Slot,
        _bank_id: BankId,
        _account: &AccountSharedData,
        _txn: &Option<&SanitizedTransaction>,
        pubkey: &Pubkey,
        _write_version: u64,
        txn_index: Option<usize>,
    ) {
        self.updates.lock().unwrap().push((*pubkey, txn_index));
    }

    fn notify_account_restore_from_snapshot(
        &self,
        _slot: Slot,
        _write_version: u64,
        _account: &AccountForGeyser<'_>,
    ) {
    }

    fn notify_end_of_restore_from_snapshot(&self) {}
}
