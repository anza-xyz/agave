//! Lock-free queue-based notifier using per-thread producers
//! 
//! This replaces synchronous plugin callbacks with non-blocking queue pushes
//! Follows the scheduler pattern: each banking thread gets its own
//! Producer instances, eliminating lock contention entirely.

use {
    geyser_shaq_bindings::*,
    geyser_shaq_queues::{GeyserQueues, QueueMode},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_accounts_db::accounts_update_notifier_interface::{
        AccountForGeyser, AccountsUpdateNotifierInterface,
    },
    solana_clock::Slot,
    solana_entry::entry::EntrySummary,
    solana_hash::Hash,
    solana_ledger::entry_notifier_interface::EntryNotifier,
    solana_pubkey::Pubkey,
    solana_rpc::{
        slot_status_notifier::SlotStatusNotifierInterface,
        transaction_notifier_interface::TransactionNotifier,
    },
    solana_signature::Signature,
    solana_transaction::{
        sanitized::SanitizedTransaction, 
        versioned::VersionedTransaction,
    },
    solana_transaction_status::TransactionStatusMeta,
    std::cell::RefCell,
};

// Per-thread queue producers - each thread gets its own set of producers
thread_local! {
    static THREAD_QUEUES: RefCell<Option<GeyserQueues>> = RefCell::new(None);
}

/// Lock-free queue-based notifier
/// Uses thread-local producers following the scheduler pattern
#[derive(Debug)]
pub struct QueueNotifier {
    base_path: String,
    mode: QueueMode,
    num_threads: usize,
}

impl QueueNotifier {
    /// Create notifier with configuration
    /// Queues are created lazily per-thread on first use
    pub fn new(base_path: &str, mode: QueueMode, num_threads: usize) -> Result<Self, geyser_shaq_queues::GeyserQueueError> {
        Ok(Self {
            base_path: base_path.to_string(),
            mode,
            num_threads,
        })
    }

    /// Get or create thread-local queues
    /// Each thread creates its own producers on first access
    fn with_thread_queues<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut GeyserQueues) -> R,
    {
        THREAD_QUEUES.with(|cell| {
            let mut opt = cell.borrow_mut();
            
            // Lazy initialization per thread
            if opt.is_none() {
                let thread_id = Self::get_thread_id();
                let thread_path = format!("{}/thread_{}", self.base_path, thread_id);
                
                match GeyserQueues::create_with_mode(&thread_path, self.mode) {
                    Ok(queues) => *opt = Some(queues),
                    Err(e) => {
                        eprintln!("Failed to create thread-local queues: {:?}", e);
                        panic!("Cannot initialize geyser queues");
                    }
                }
            }
            
            f(opt.as_mut().unwrap())
        })
    }

    /// Get a unique ID for the current thread
    /// This determines which queue files to use
    fn get_thread_id() -> usize {
        // Use thread-local counter for stable IDs across the thread's lifetime
        thread_local! {
            static THREAD_ID: RefCell<Option<usize>> = RefCell::new(None);
        }
        
        THREAD_ID.with(|cell| {
            let mut opt = cell.borrow_mut();
            if opt.is_none() {
                // Use a global atomic counter to assign unique IDs
                static NEXT_THREAD_ID: std::sync::atomic::AtomicUsize = 
                    std::sync::atomic::AtomicUsize::new(0);
                *opt = Some(NEXT_THREAD_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
            }
            opt.unwrap()
        })
    }

    pub fn sync(&self) {
        self.with_thread_queues(|queues| queues.sync());
    }

    pub fn num_threads(&self) -> usize {
        self.num_threads
    }

    pub fn base_path(&self) -> &str {
        &self.base_path
    }
}

impl AccountsUpdateNotifierInterface for QueueNotifier {
    fn notify_account_update(
        &self,
        slot: Slot,
        account: &AccountSharedData,
        txn: &Option<&SanitizedTransaction>,
        pubkey: &Pubkey,
        write_version: u64,
    ) {
        self.with_thread_queues(|queues| {
            // Allocate account data in shared memory
            let data_region = match queues.allocate_data(account.data()) {
                Ok(r) => r,
                Err(_) => return,
            };

            let (txn_signature, has_txn) = if let Some(txn) = txn {
                let mut sig = [0u8; 64];
                sig.copy_from_slice(txn.signature().as_ref());
                (sig, true)
            } else {
                ([0u8; 64], false)
            };

            let msg = AccountUpdateMessage {
                slot,
                write_version,
                lamports: account.lamports(),
                rent_epoch: account.rent_epoch(),
                pubkey: pubkey.to_bytes(),
                owner: account.owner().to_bytes(),
                executable: account.executable(),
                is_startup: false,
                data: data_region,
                txn_signature,
                has_txn,
            };

            let _ = queues.push_account_update(msg);
        });
    }

    fn notify_account_restore_from_snapshot(
        &self,
        slot: Slot,
        write_version: u64,
        account: &AccountForGeyser<'_>,
    ) {
        self.with_thread_queues(|queues| {
            let data_region = match queues.allocate_data(account.data) {
                Ok(r) => r,
                Err(_) => return,
            };

            let msg = AccountUpdateMessage {
                slot,
                write_version,
                lamports: account.lamports,
                rent_epoch: account.rent_epoch,
                pubkey: account.pubkey.to_bytes(),
                owner: account.owner.to_bytes(),
                executable: account.executable,
                is_startup: true,
                data: data_region,
                txn_signature: [0u8; 64],
                has_txn: false,
            };

            let _ = queues.push_account_update(msg);
        });
    }

    fn notify_end_of_restore_from_snapshot(&self) {
        // No-op - consumer detects from slot progression
    }

    fn snapshot_notifications_enabled(&self) -> bool {
        true
    }
}

impl TransactionNotifier for QueueNotifier {
    fn notify_transaction(
        &self,
        slot: Slot,
        index: usize,
        signature: &Signature,
        _message_hash: &Hash,
        is_vote: bool,
        transaction_status_meta: &TransactionStatusMeta,
        transaction: &VersionedTransaction,
    ) {
        self.with_thread_queues(|queues| {
            // Serialize transaction using bincode
            // TODO: Handle serialization error properly
            let txn_bytes = match bincode::serialize(transaction) {
                Ok(bytes) => bytes,
                Err(_) => {
                    // Skip notification if serialization fails
                    return;
                }
            };
            
            let transaction_region = match queues.allocate_data(&txn_bytes) {
                Ok(r) => SharableTransactionRegion {
                    offset: r.offset,
                    length: r.length,
                },
                Err(_) => return,
            };

            let mut sig_array = [0u8; 64];
            sig_array.copy_from_slice(signature.as_ref());

            let msg = TransactionMessage {
                slot,
                index,
                signature: sig_array,
                is_vote,
                transaction: transaction_region,
                status: TransactionStatus {
                    success: transaction_status_meta.status.is_ok(),
                    error_code: 0,
                },
                compute_units_consumed: transaction_status_meta.compute_units_consumed.unwrap_or(0),
            };

            let _ = queues.push_transaction(msg);
        });
    }
}

impl SlotStatusNotifierInterface for QueueNotifier {
    fn notify_slot_confirmed(&self, slot: Slot, parent: Option<Slot>) {
        self.notify_slot_status_internal(slot, parent, SlotStatusCode::Confirmed);
    }

    fn notify_slot_processed(&self, slot: Slot, parent: Option<Slot>) {
        self.notify_slot_status_internal(slot, parent, SlotStatusCode::Processed);
    }

    fn notify_slot_rooted(&self, slot: Slot, parent: Option<Slot>) {
        self.notify_slot_status_internal(slot, parent, SlotStatusCode::Rooted);
    }

    fn notify_first_shred_received(&self, slot: Slot) {
        self.notify_slot_status_internal(slot, None, SlotStatusCode::FirstShredReceived);
    }

    fn notify_completed(&self, slot: Slot) {
        self.notify_slot_status_internal(slot, None, SlotStatusCode::Completed);
    }

    fn notify_created_bank(&self, slot: Slot, parent: Slot) {
        self.notify_slot_status_internal(slot, Some(parent), SlotStatusCode::CreatedBank);
    }

    fn notify_slot_dead(&self, slot: Slot, parent: Slot, _error: String) {
        self.notify_slot_status_internal(slot, Some(parent), SlotStatusCode::Dead);
    }
}

impl QueueNotifier {
    fn notify_slot_status_internal(&self, slot: Slot, parent: Option<Slot>, status: SlotStatusCode) {
        self.with_thread_queues(|queues| {
            let msg = SlotStatusMessage {
                slot,
                parent: parent.unwrap_or(0),
                has_parent: parent.is_some(),
                status,
                timestamp_ns: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64,
            };

            let _ = queues.push_slot_status(msg);
        });
    }
}

impl EntryNotifier for QueueNotifier {
    fn notify_entry(
        &self,
        slot: Slot,
        index: usize,
        entry: &EntrySummary,
        starting_transaction_index: usize,
    ) {
        self.with_thread_queues(|queues| {
            let msg = EntryMessage {
                slot,
                index,
                num_hashes: entry.num_hashes,
                hash: entry.hash.to_bytes(),
                executed_transaction_count: entry.num_transactions,
                starting_transaction_index,
            };

            let _ = queues.push_entry(msg);
        });
    }
}