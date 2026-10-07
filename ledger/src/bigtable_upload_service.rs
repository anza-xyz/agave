use {
    crate::{
        bigtable_upload::{self, ConfirmedBlockUploadConfig},
        blockstore::Blockstore,
    },
    solana_clock::Slot,
    std::{
        cmp::min,
        sync::{
            Arc,
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
        max_complete_transaction_status_slot: Arc<AtomicU64>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        Self::new_with_config(
            runtime,
            bigtable_ledger_storage,
            blockstore,
            max_complete_transaction_status_slot,
            ConfirmedBlockUploadConfig::default(),
            exit,
        )
    }

    pub fn new_with_config(
        runtime: Arc<Runtime>,
        bigtable_ledger_storage: solana_storage_bigtable::LedgerStorage,
        blockstore: Arc<Blockstore>,
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
/// The pass never extends past `blockstore.max_root()`, because
/// `upload_confirmed_blocks` treats a range with no rooted slots as done.
fn next_upload_end_slot(
    start_slot: Slot,
    max_complete_transaction_status_slot: &AtomicU64,
    blockstore: &Blockstore,
    config: &ConfirmedBlockUploadConfig,
) -> Option<Slot> {
    let highest_complete_root = min(
        max_complete_transaction_status_slot.load(Ordering::SeqCst),
        blockstore.max_root(),
    );
    let end_slot = min(
        highest_complete_root,
        start_slot.saturating_add(config.max_num_slots_to_check as u64 * 2),
    );
    (end_slot > start_slot).then_some(end_slot)
}

#[cfg(test)]
mod tests {
    use {super::*, crate::get_tmp_ledger_path_auto_delete};

    #[test]
    fn test_block_after_skipped_window_waits_for_root_marker() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let config = ConfirmedBlockUploadConfig {
            max_num_slots_to_check: 16,
            ..ConfirmedBlockUploadConfig::default()
        };

        // Everything up to root 150 is uploaded. Slots 151..=155 were skipped,
        // and block 156 is the new root. Its transaction statuses are written,
        // but its root marker isn't yet.
        blockstore.set_roots([0, 150].iter()).unwrap();
        let max_complete_transaction_status_slot = AtomicU64::new(156);

        // The pass must not run past the unwritten root.
        let start_slot = 151;
        assert_eq!(
            next_upload_end_slot(
                start_slot,
                &max_complete_transaction_status_slot,
                &blockstore,
                &config,
            ),
            None,
            "pass would skip slot 156",
        );

        // A pass starting before the highest written root runs up to it...
        assert_eq!(
            next_upload_end_slot(
                149,
                &max_complete_transaction_status_slot,
                &blockstore,
                &config
            ),
            Some(150),
        );
        // ...but not when it starts at the root; that waits for the next root.
        assert_eq!(
            next_upload_end_slot(
                150,
                &max_complete_transaction_status_slot,
                &blockstore,
                &config
            ),
            None,
        );

        // Once the marker is written, the pass from 151 includes 156.
        blockstore.set_roots([156].iter()).unwrap();
        assert_eq!(
            next_upload_end_slot(
                start_slot,
                &max_complete_transaction_status_slot,
                &blockstore,
                &config,
            ),
            Some(156),
        );
    }
}
