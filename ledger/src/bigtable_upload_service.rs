use {
    crate::{
        bigtable_upload::{self, ConfirmedBlockUploadConfig},
        blockstore::Blockstore,
    },
    solana_clock::Slot,
    solana_runtime::commitment::BlockCommitmentCache,
    std::{
        cmp::min,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread::{self, Builder, JoinHandle},
    },
    tokio::runtime::Runtime,
};

pub struct BigTableUploadService {
    thread: JoinHandle<()>,
}

impl BigTableUploadService {
    pub fn new(
        runtime: Arc<Runtime>,
        bigtable_ledger_storage: solana_storage_bigtable::LedgerStorage,
        blockstore: Arc<Blockstore>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        max_complete_transaction_status_slot: Arc<AtomicU64>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        Self::new_with_config(
            runtime,
            bigtable_ledger_storage,
            blockstore,
            block_commitment_cache,
            max_complete_transaction_status_slot,
            ConfirmedBlockUploadConfig::default(),
            exit,
        )
    }

    pub fn new_with_config(
        runtime: Arc<Runtime>,
        bigtable_ledger_storage: solana_storage_bigtable::LedgerStorage,
        blockstore: Arc<Blockstore>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        max_complete_transaction_status_slot: Arc<AtomicU64>,
        config: ConfirmedBlockUploadConfig,
        exit: Arc<AtomicBool>,
    ) -> Self {
        info!("Starting BigTable upload service");
        let thread = Builder::new()
            .name("solBigTUpload".to_string())
            .spawn(move || {
                Self::run(
                    runtime,
                    bigtable_ledger_storage,
                    blockstore,
                    block_commitment_cache,
                    max_complete_transaction_status_slot,
                    config,
                    exit,
                )
            })
            .unwrap();

        Self { thread }
    }

    fn run(
        runtime: Arc<Runtime>,
        bigtable_ledger_storage: solana_storage_bigtable::LedgerStorage,
        blockstore: Arc<Blockstore>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        max_complete_transaction_status_slot: Arc<AtomicU64>,
        config: ConfirmedBlockUploadConfig,
        exit: Arc<AtomicBool>,
    ) {
        let mut start_slot = blockstore.get_first_available_block().unwrap_or_default();
        loop {
            if exit.load(Ordering::Relaxed) {
                break;
            }

            let Some(end_slot) = next_upload_end_slot(
                start_slot,
                &max_complete_transaction_status_slot,
                &block_commitment_cache,
                &blockstore,
                &config,
            ) else {
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            };

            let result = runtime.block_on(bigtable_upload::upload_confirmed_blocks(
                blockstore.clone(),
                bigtable_ledger_storage.clone(),
                start_slot,
                end_slot,
                config.clone(),
                exit.clone(),
            ));

            match result {
                Ok(last_slot_uploaded) => start_slot = last_slot_uploaded.saturating_add(1),
                Err(err) => {
                    warn!("bigtable: upload_confirmed_blocks: {err}");
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if start_slot == 0 {
                        start_slot = blockstore.get_first_available_block().unwrap_or_default();
                    }
                }
            }
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread.join()
    }
}

/// Returns the last slot of the next upload pass starting at `start_slot`, or
/// `None` if there is nothing new to upload yet.
///
/// The commitment cache root can lead the blockstore's root markers: under
/// Alpenglow, votor publishes a new root before `Blockstore::set_roots` runs.
/// `upload_confirmed_blocks` reports a range with no rooted slots as done, so
/// a pass must not extend past `max_root`, which only advances once the root
/// markers are written. Otherwise the first block after a skipped leader
/// window can be passed over and never uploaded.
fn next_upload_end_slot(
    start_slot: Slot,
    max_complete_transaction_status_slot: &AtomicU64,
    block_commitment_cache: &RwLock<BlockCommitmentCache>,
    blockstore: &Blockstore,
    config: &ConfirmedBlockUploadConfig,
) -> Option<Slot> {
    // The highest slot eligible for upload is the highest root that has
    // complete block metadata
    let highest_complete_root = max_complete_transaction_status_slot
        .load(Ordering::SeqCst)
        .min(block_commitment_cache.read().unwrap().root())
        .min(blockstore.max_root());
    let end_slot = min(
        highest_complete_root,
        start_slot.saturating_add(config.max_num_slots_to_check as u64 * 2),
    );
    (end_slot > start_slot).then_some(end_slot)
}

#[cfg(test)]
mod tests {
    use {super::*, crate::get_tmp_ledger_path_auto_delete};

    // Runs one pass of the service loop and returns the new `start_slot`.
    fn run_upload_pass(
        runtime: &Runtime,
        bigtable: &solana_storage_bigtable::LedgerStorage,
        start_slot: Slot,
        max_complete_transaction_status_slot: &AtomicU64,
        block_commitment_cache: &RwLock<BlockCommitmentCache>,
        blockstore: &Arc<Blockstore>,
        config: &ConfirmedBlockUploadConfig,
    ) -> Slot {
        let Some(end_slot) = next_upload_end_slot(
            start_slot,
            max_complete_transaction_status_slot,
            block_commitment_cache,
            blockstore,
            config,
        ) else {
            return start_slot;
        };
        let last_slot_uploaded = runtime
            .block_on(bigtable_upload::upload_confirmed_blocks(
                blockstore.clone(),
                bigtable.clone(),
                start_slot,
                end_slot,
                config.clone(),
                Arc::new(AtomicBool::new(false)),
            ))
            .unwrap();
        last_slot_uploaded.saturating_add(1)
    }

    #[test]
    fn test_block_after_skipped_window_waits_for_root_marker() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let runtime = Runtime::new().unwrap();
        // Nothing listens here. The only pass that calls upload_confirmed_blocks
        // has no rooted slots in its range, so it returns before making any
        // bigtable request.
        let bigtable = runtime
            .block_on(async {
                solana_storage_bigtable::LedgerStorage::new_for_emulator(
                    "test",
                    "default",
                    "127.0.0.1:1",
                    None,
                )
            })
            .unwrap();
        let config = ConfirmedBlockUploadConfig {
            max_num_slots_to_check: 16,
            ..ConfirmedBlockUploadConfig::default()
        };

        // Everything up to root 150 is uploaded. Slots 151..=155 were skipped,
        // and block 156 is the new root.
        blockstore.set_roots([0, 150].iter()).unwrap();
        let start_slot = 151;
        let max_complete_transaction_status_slot = AtomicU64::new(156);

        // The commitment cache sees root 156 before its root marker is written.
        let block_commitment_cache = RwLock::new(BlockCommitmentCache::default());
        block_commitment_cache.write().unwrap().set_root(156);
        let start_slot = run_upload_pass(
            &runtime,
            &bigtable,
            start_slot,
            &max_complete_transaction_status_slot,
            &block_commitment_cache,
            &blockstore,
            &config,
        );

        // The pass must not advance past the unwritten root...
        assert_eq!(start_slot, 151, "slot 156 was skipped");

        // ...and once the marker is written, the next pass includes 156.
        blockstore.set_roots([156].iter()).unwrap();
        assert_eq!(
            next_upload_end_slot(
                start_slot,
                &max_complete_transaction_status_slot,
                &block_commitment_cache,
                &blockstore,
                &config,
            ),
            Some(156),
        );
    }
}
