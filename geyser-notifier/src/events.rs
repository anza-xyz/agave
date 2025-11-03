use crate::GeyserNotifier;
use geyser_shaq_bindings::{
    AccountUpdateMessage, BlockMetadataMessage, SharableTransactionRegion, SlotStatusCode,
    SlotStatusMessage, TransactionMessage, TransactionStatus,
};
use geyser_shaq_queues::GeyserQueueError;

use solana_account::{AccountSharedData, ReadableAccount};
use solana_clock::Slot;
use solana_pubkey::Pubkey;
use solana_signature::Signature;

pub type Result<T> = std::result::Result<T, GeyserQueueError>;

impl GeyserNotifier {
    /// Notify account update
    ///
    /// Constructs and sends an AccountUpdateMessage to the shared memory queue.
    /// Account data is allocated in shared memory via the allocator.
    pub fn notify_account_update(
        &mut self,
        pubkey: &Pubkey,
        account: &AccountSharedData,
        slot: Slot,
        write_version: u64,
        is_startup: bool,
        txn_signature: Option<&Signature>,
    ) -> Result<()> {
        let data_region = self.queues.allocate_data(account.data())?;

        let update = AccountUpdateMessage {
            slot,
            write_version,
            lamports: account.lamports(),
            rent_epoch: account.rent_epoch(),
            pubkey: pubkey.to_bytes(),
            owner: account.owner().to_bytes(),
            executable: account.executable(),
            is_startup,
            data: data_region,
            txn_signature: txn_signature
                .map(|s| s.as_ref())
                .unwrap_or(&[0u8; 64])
                .try_into()
                .unwrap(),
            has_txn: txn_signature.is_some(),
        };

        self.queues.sync();
        self.queues.push_account_update(update)
    }

    /// Notify transaction
    ///
    /// Constructs and sends a TransactionMessage.
    /// Transaction data is allocated in shared memory.
    pub fn notify_transaction(
        &mut self,
        signature: &Signature,
        slot: Slot,
        index: usize,
        is_vote: bool,
        transaction_data: &[u8], // Serialized transaction
        success: bool,
        error_code: u32,
        compute_units_consumed: u64,
    ) -> Result<()> {
        let transaction_region = self.queues.allocate_data(transaction_data)?;

        let notification = TransactionMessage {
            slot,
            index,
            signature: signature.as_ref().try_into().unwrap(),
            is_vote,
            transaction: SharableTransactionRegion {
                offset: transaction_region.offset,
                length: transaction_region.length,
            },
            status: TransactionStatus {
                success,
                error_code,
            },
            compute_units_consumed,
        };

        self.queues.push_transaction(notification)
    }

    /// Notify block metadata
    ///
    /// Constructs and sends a BlockMetadataMessage.
    pub fn notify_block_metadata(
        &mut self,
        slot: Slot,
        parent_slot: Slot,
        blockhash: &[u8; 32],
        parent_blockhash: &[u8; 32],
        block_height: u64,
        block_time: Option<i64>,
        executed_transaction_count: u64,
        entry_count: u64,
    ) -> Result<()> {
        let metadata = BlockMetadataMessage {
            slot,
            parent_slot,
            blockhash: *blockhash,
            parent_blockhash: *parent_blockhash,
            block_height,
            block_time: block_time.unwrap_or(0),
            has_block_time: block_time.is_some(),
            executed_transaction_count,
            entry_count,
            rewards: geyser_shaq_bindings::SharableRewardsRegion {
                offset: 0,
                count: 0,
            },
        };

        self.queues.push_block_metadata(metadata)
    }

    /// Notify slot status
    ///
    /// Constructs and sends a SlotStatusMessage.
    pub fn notify_slot_status(
        &mut self,
        slot: Slot,
        parent: Option<Slot>,
        status: SlotStatusCode,
    ) -> Result<()> {
        let slot_status = SlotStatusMessage {
            slot,
            parent: parent.unwrap_or(0),
            has_parent: parent.is_some(),
            status,
            timestamp_ns: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
        };

        self.queues.push_slot_status(slot_status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_global_once() {
        // Should succeed first time
        assert!(GeyserNotifier::init_global("/tmp/test-queues".to_string()).is_ok());

        // Should fail second time (already initialized)
        assert!(GeyserNotifier::init_global("/tmp/other-path".to_string()).is_err());
    }

    #[test]
    fn test_thread_local_without_init() {
        // Should return None if init_global not called
        let result = GeyserNotifier::thread_local(|_notifier| 42);

        // Will be None because we can't create real queues in test
        // But this tests the code path
        assert!(result.is_none());
    }

    // TODO: Add integration tests with actual queue creation
    // once we have mock/test helpers for GeyserQueues
}
