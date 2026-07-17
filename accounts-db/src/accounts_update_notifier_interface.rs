use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_transaction::sanitized::SanitizedTransaction,
    std::sync::Arc,
};

pub trait AccountsUpdateNotifierInterface: std::fmt::Debug {
    /// Enable account notifications from snapshot
    fn snapshot_notifications_enabled(&self) -> bool;

    /// Notified when an account is updated at runtime, due to transaction activities
    fn notify_account_update(
        &self,
        slot: Slot,
        account: &AccountSharedData,
        txn: &Option<&SanitizedTransaction>,
        pubkey: &Pubkey,
        write_version: u64,
    );

    /// Notified when the AccountsDb is initialized at start when restored
    /// from a snapshot.
    fn notify_account_restore_from_snapshot(
        &self,
        slot: Slot,
        write_version: u64,
        account: &AccountForGeyser<'_>,
    );

    /// Notified when all accounts have been notified when restoring from a snapshot.
    fn notify_end_of_restore_from_snapshot(&self);
}

pub type AccountsUpdateNotifier = Arc<dyn AccountsUpdateNotifierInterface + Sync + Send>;

/// Account type with only the fields necessary for Geyser
#[derive(Debug, Clone)]
pub struct AccountForGeyser<'a> {
    pub pubkey: &'a Pubkey,
    pub lamports: u64,
    pub owner: &'a Pubkey,
    pub executable: bool,
    pub rent_epoch: Epoch,
    pub data: &'a [u8],
}

impl ReadableAccount for AccountForGeyser<'_> {
    fn lamports(&self) -> u64 {
        self.lamports
    }
    fn data(&self) -> &[u8] {
        self.data
    }
    fn owner(&self) -> &Pubkey {
        self.owner
    }
    fn executable(&self) -> bool {
        self.executable
    }
    fn rent_epoch(&self) -> Epoch {
        self.rent_epoch
    }
}
