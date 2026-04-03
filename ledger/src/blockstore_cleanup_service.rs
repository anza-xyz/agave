//! The `blockstore_cleanup_service` drops older ledger data to limit disk space usage.
//! The service works by counting the number of live data shreds in the ledger; this
//! can be done quickly and should have a fairly stable correlation to actual bytes.
//! Once the shred count (and thus roughly the byte count) reaches a threshold,
//! the services begins removing data in FIFO order.

use {
    crate::blockstore::{
        Blockstore, PurgeType,
        column::{ColumnName, columns},
    },
    crossbeam_channel::{Receiver, Sender, TrySendError, bounded},
    solana_clock::{DEFAULT_MS_PER_SLOT, Slot},
    solana_measure::measure::Measure,
    std::{
        string::ToString,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

// - To try and keep the RocksDB size under 400GB:
//   Seeing about 1600b/shred, using 2000b/shred for margin, so 200m shreds can be stored in 400gb.
//   at 5k shreds/slot at 50k tps, this is 40k slots (~4.4 hours).
//   At idle, 60 shreds/slot this is about 3.33m slots (~15 days)
// This is chosen to allow enough time for
// - A validator to download a snapshot from a peer and boot from it
// - To make sure that if a validator needs to reboot from its own snapshot, it has enough slots locally
//   to catch back up to where it was when it stopped
pub const DEFAULT_MAX_LEDGER_SHREDS: u64 = 200_000_000;

// Allow down to 50m, or 3.5 days at idle, 1hr at 50k load, around ~100GB
pub const DEFAULT_MIN_MAX_LEDGER_SHREDS: u64 = 50_000_000;

// Perform blockstore cleanup at this interval to limit the overhead of cleanup
// Cleanup will be considered after the latest root has advanced by this value
const DEFAULT_CLEANUP_SLOT_INTERVAL: u64 = 512;
// The above slot interval can be roughly equated to a time interval. So, scale
// how often we check for cleanup with the interval. Doing so will avoid wasted
// checks when we know that the latest root could not have advanced far enough
//
// Given that the timing of new slots/roots is not exact, divide by 10 to avoid
// a long wait incase a check occurs just before the interval has elapsed
const LOOP_LIMITER: Duration =
    Duration::from_millis(DEFAULT_CLEANUP_SLOT_INTERVAL * DEFAULT_MS_PER_SLOT / 10);

pub struct BlockstoreCleanupService {
    t_cleanup: JoinHandle<()>,
}

impl BlockstoreCleanupService {
    pub fn new(
        blockstore: Arc<Blockstore>,
        max_ledger_shreds: Option<u64>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let mut last_purge_slot = 0;
        let mut last_check_time = Instant::now();

        let t_cleanup = Builder::new()
            .name("solBstoreClean".to_string())
            .spawn(move || {
                let (cleanup_request_sender, cleanup_request_receiver) = bounded(1);
                blockstore.register_manual_purge_request_sender(cleanup_request_sender.clone());

                info!(
                    "BlockstoreCleanupService has started with {}",
                    if let Some(max_shreds) = max_ledger_shreds {
                        format!("max shred limit {max_shreds}")
                    } else {
                        "no shred limit, automatic cleanup is disabled".to_string()
                    }
                );

                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }
                    if last_check_time.elapsed() > LOOP_LIMITER {
                        Self::cleanup_ledger(
                            &blockstore,
                            &cleanup_request_sender,
                            &cleanup_request_receiver,
                            max_ledger_shreds,
                            &mut last_purge_slot,
                            DEFAULT_CLEANUP_SLOT_INTERVAL,
                        );

                        last_check_time = Instant::now();
                    }
                    // Only sleep for 1 second instead of LOOP_LIMITER so that this
                    // thread can respond to the exit flag in a timely manner
                    thread::sleep(Duration::from_secs(1));
                }

                info!("BlockstoreCleanupService has stopped");
            })
            .unwrap();

        Self { t_cleanup }
    }

    /// Push a cleanup request into `cleanup_request_sender` if an automatic
    /// cleanup is due
    fn maybe_generate_automatic_cleanup_request(
        blockstore: &Blockstore,
        cleanup_request_sender: &Sender<Slot>,
        max_ledger_shreds: Option<u64>,
        last_purge_slot: &mut u64,
        purge_interval: u64,
    ) {
        let Some(max_ledger_shreds) = max_ledger_shreds else {
            // Automatic blockstore cleanup is disabled
            return;
        };

        if cleanup_request_sender.is_full() {
            // An unprocessed cleanup request already exists so bail now
            return;
        }

        let root = blockstore.max_root();
        if root - *last_purge_slot <= purge_interval {
            // Not enough roots have passed since the last cleanup so bail now
            return;
        }
        *last_purge_slot = root;

        info!("Looking for Blockstore data to cleanup, latest root: {root}");

        let live_files = blockstore
            .live_files_metadata()
            .expect("Blockstore::live_files_metadata()");
        let num_shreds: u64 = live_files
            .iter()
            .filter(|live_file| live_file.column_family_name == columns::ShredData::NAME)
            .map(|file_meta| file_meta.num_entries)
            .sum();

        // Using the difference between the lowest and highest slot seen will
        // result in overestimating the number of slots in the blockstore since
        // there are likely to be some missing slots, such as when a leader is
        // delinquent for their leader slots.
        //
        // With the below calculations, we will then end up underestimating the
        // mean number of shreds per slot present in the blockstore which will
        // result in cleaning more slots than necessary to get us
        // below max_ledger_shreds.
        //
        // Given that the service runs on an interval, this is good because it
        // means that we are building some headroom so the peak number of alive
        // shreds doesn't get too large before the service's next run.
        //
        // Finally, we have a check to make sure that we don't purge any slots
        // newer than the passed in root. This check is practically only
        // relevant when a cluster has extended periods of not rooting slots.
        // With healthy cluster operation, the minimum ledger size ensures
        // that purged slots will be quite old in relation to the newest root.
        let lowest_slot = blockstore.lowest_slot();
        let highest_slot = blockstore
            .highest_slot()
            .expect("Blockstore::highest_slot()")
            .unwrap_or(lowest_slot);
        if highest_slot < lowest_slot {
            error!(
                "Skipping Blockstore cleanup: highest slot {highest_slot} < lowest slot \
                 {lowest_slot}",
            );
            return;
        }
        // The + 1 ensures we count the correct number of slots. Additionally,
        // it guarantees num_slots >= 1 for the subsequent division.
        let num_slots = highest_slot - lowest_slot + 1;
        let mean_shreds_per_slot = num_shreds / num_slots;
        info!(
            "Blockstore has {num_shreds} alive shreds in slots [{lowest_slot}, {highest_slot}], \
             mean of {mean_shreds_per_slot} shreds per slot",
        );

        if num_shreds <= max_ledger_shreds {
            // Cleanup is not necessary at this time
            return;
        }

        // Add an extra (mean_shreds_per_slot - 1) in the numerator
        // so that our integer division rounds up
        let num_slots_to_clean = (num_shreds - max_ledger_shreds + mean_shreds_per_slot - 1)
            .checked_div(mean_shreds_per_slot);
        let Some(num_slots_to_clean) = num_slots_to_clean else {
            error!("Skipping Blockstore automatic cleanup: calculated mean of 0 shreds per slot");
            return;
        };

        // Ensure we don't cleanup anything past the last root we saw
        let lowest_cleanup_slot = std::cmp::min(lowest_slot + num_slots_to_clean - 1, root);

        match cleanup_request_sender.try_send(lowest_cleanup_slot) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                info!("Dropping Blockstore automatic cleanup request: a pending request exists");
            }
            Err(TrySendError::Disconnected(_)) => {
                unreachable!(
                    "Channel disconnected while this thread holds both ends of the channel"
                );
            }
        };
    }

    /// Cleanup the ledger if a cleanup request is present. Cleanup requests may
    /// be automatically created given the configuration options, or they may
    /// come from an external caller who holds a Blockstore
    pub fn cleanup_ledger(
        blockstore: &Blockstore,
        cleanup_request_sender: &Sender<Slot>,
        cleanup_request_receiver: &Receiver<Slot>,
        max_ledger_shreds: Option<u64>,
        last_purge_slot: &mut u64,
        purge_interval: u64,
    ) {
        Self::maybe_generate_automatic_cleanup_request(
            blockstore,
            cleanup_request_sender,
            max_ledger_shreds,
            last_purge_slot,
            purge_interval,
        );

        // `Receiver::try_recv()` will error if the channel is disconnected or
        // empty. Both sides of the channel are passed in so it is impossible
        // for the channel to be disconnected. If the channel is empty, there
        // is nothing to do and `ok()` will convert to an `Option` for us
        let lowest_cleanup_slot = cleanup_request_receiver.try_recv().ok();

        if let Some(lowest_cleanup_slot) = lowest_cleanup_slot {
            *blockstore.lowest_cleanup_slot.write().unwrap() = lowest_cleanup_slot;

            let mut purge_time = Measure::start("purge_slots()");
            // purge any slots older than lowest_cleanup_slot.
            let _ = blockstore
                .purge_slots(0, lowest_cleanup_slot, PurgeType::CompactionFilter)
                .inspect_err(|e| {
                    error!("Purge failed when cleaning ledger to {lowest_cleanup_slot}: {e:?}")
                });
            // Update only after purge operation.
            // Safety: This value can be used by compaction_filters shared via Arc<AtomicU64>.
            // Compactions are async and run as a multi-threaded background job. However, this
            // shouldn't cause consistency issues for iterators and getters because we have
            // already expired all affected keys (older than or equal to lowest_cleanup_slot)
            // by the above `purge_slots`. According to the general RocksDB design where SST
            // files are immutable, even running iterators aren't affected; the database grabs
            // a snapshot of the live set of sst files at iterator's creation.
            // Also, we passed the PurgeType::CompactionFilter, meaning no delete_range for
            // transaction_status and address_signatures CFs. These are fine because they
            // don't require strong consistent view for their operation.
            blockstore.set_max_expired_slot(lowest_cleanup_slot);
            purge_time.stop();
            info!("Cleaned up Blockstore data older than slot {lowest_cleanup_slot}. {purge_time}");
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.t_cleanup.join()
    }
}
#[cfg(test)]
mod tests {
    use {super::*, crate::blockstore::make_many_slot_entries};

    fn flush_blockstore_contents_to_disk(blockstore: Blockstore) -> Blockstore {
        // The maybe_generate_automatic_cleanup_request() routine uses a method
        // that queries data from RocksDB SST files. On a running validator,
        // these are created fairly regularly as new data comes in and older
        // data is pushed to disk. In these unit tests, we aren't pushing nearly
        // enough data for this to happen organically. So, instead open and
        // close the Blockstore which will perform the flush to SSTs.
        let ledger_path = blockstore.ledger_path().clone();
        drop(blockstore);
        Blockstore::open(&ledger_path).unwrap()
    }

    #[test]
    fn test_maybe_generate_automatic_cleanup_request() {
        // maybe_generate_automatic_cleanup_request() does not modify Blockstore
        // state so multiple calls can be made on the same range of slots
        agave_logger::setup();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let (sender, receiver) = bounded(1);

        // Construct and build some shreds for slots [1, 10]
        let num_slots: u64 = 10;
        let num_entries = 200;
        let (shreds, _) = make_many_slot_entries(1, num_slots, num_entries);
        let total_num_shreds = shreds.len() as u64;
        let shreds_per_slot = (shreds.len() / num_slots as usize) as u64;
        assert!(shreds_per_slot > 1);
        blockstore.insert_shreds(shreds, None, false).unwrap();

        // Initiate a flush so inserted shreds found by find_slots_to_clean()
        let blockstore = Arc::new(flush_blockstore_contents_to_disk(blockstore));

        // Note that last_purge_slot gets updated after a couple basic checks to
        // avoid rescanning all the Blockstore SST files again. This is good for
        // production but our unit test will reset the value after each step
        let mut last_purge_slot = 0;
        // Keep purge_interval at 0 to keep math in our unit test easy
        let purge_interval = 0;

        // Start with 1 as the latest root
        let mut latest_root = 1;
        blockstore.set_roots(std::iter::once(&latest_root)).unwrap();
        // Auto clean will select slot 1 (latest_root) as min clean slot
        let max_ledger_shreds = Some(1);
        BlockstoreCleanupService::maybe_generate_automatic_cleanup_request(
            &blockstore,
            &sender,
            max_ledger_shreds,
            &mut last_purge_slot,
            purge_interval,
        );
        assert_eq!(receiver.try_recv().unwrap(), latest_root);

        // Reset last_purge_slot
        assert_eq!(last_purge_slot, 1);
        last_purge_slot = 0;
        BlockstoreCleanupService::maybe_generate_automatic_cleanup_request(
            &blockstore,
            &sender,
            max_ledger_shreds,
            &mut last_purge_slot,
            purge_interval,
        );
        assert_eq!(receiver.try_recv().unwrap(), latest_root);
        // Reset last_purge_slot
        assert_eq!(last_purge_slot, 1);
        last_purge_slot = 0;

        // The auto clean request dropped when a request already exists
        sender.try_send(100).unwrap();
        BlockstoreCleanupService::maybe_generate_automatic_cleanup_request(
            &blockstore,
            &sender,
            max_ledger_shreds,
            &mut last_purge_slot,
            purge_interval,
        );
        assert_eq!(receiver.try_recv().unwrap(), 100);
        assert!(receiver.is_empty());

        // No auto clean request when max_ledger_shreds is None
        let max_ledger_shreds = None;
        BlockstoreCleanupService::maybe_generate_automatic_cleanup_request(
            &blockstore,
            &sender,
            max_ledger_shreds,
            &mut last_purge_slot,
            purge_interval,
        );
        assert!(receiver.is_empty());

        // No auto clean request when max_ledger_shreds exceeds blockstore load
        let max_ledger_shreds = Some(total_num_shreds + 1);
        BlockstoreCleanupService::maybe_generate_automatic_cleanup_request(
            &blockstore,
            &sender,
            max_ledger_shreds,
            &mut last_purge_slot,
            purge_interval,
        );
        assert!(receiver.is_empty());
        // Reset last_purge_slot
        assert_eq!(last_purge_slot, 1);
        last_purge_slot = 0;

        // Auto clean can once again clean up to latest_root
        let max_ledger_shreds = Some(total_num_shreds - 1);
        BlockstoreCleanupService::maybe_generate_automatic_cleanup_request(
            &blockstore,
            &sender,
            max_ledger_shreds,
            &mut last_purge_slot,
            purge_interval,
        );
        assert_eq!(receiver.try_recv().unwrap(), latest_root);
        // Reset last_purge_slot
        assert_eq!(last_purge_slot, 1);
        last_purge_slot = 0;

        for slot in 1..=num_slots {
            // Set last_root to make slots <= slot eligible for cleaning
            latest_root = slot;
            blockstore.set_roots(std::iter::once(&latest_root)).unwrap();
            // Set max_ledger_shreds to 0 so that all eligible slots are cleaned
            let max_ledger_shreds = Some(0);
            BlockstoreCleanupService::maybe_generate_automatic_cleanup_request(
                &blockstore,
                &sender,
                max_ledger_shreds,
                &mut last_purge_slot,
                purge_interval,
            );
            assert_eq!(receiver.try_recv().unwrap(), latest_root);
        }
    }

    #[test]
    fn test_cleanup() {
        agave_logger::setup();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let (sender, receiver) = bounded(1);

        let (shreds, _) = make_many_slot_entries(0, 50, 5);
        blockstore.insert_shreds(shreds, None, false).unwrap();

        // Initiate a flush so inserted shreds found by maybe_generate_automatic_cleanup_request()
        let blockstore = Arc::new(flush_blockstore_contents_to_disk(blockstore));

        // Mark 40 as a root to cleanup all older slots
        let root = 40;
        blockstore.set_roots(std::iter::once(&root)).unwrap();

        let mut last_purge_slot = 0;
        let max_ledger_shreds = Some(5);
        let purge_interval = 10;
        BlockstoreCleanupService::cleanup_ledger(
            &blockstore,
            &sender,
            &receiver,
            max_ledger_shreds,
            &mut last_purge_slot,
            purge_interval,
        );
        assert_eq!(last_purge_slot, root);
        // A request will be generated and consumed so channel should be empty
        assert!(receiver.is_empty());

        // Ensure that slots 0-40 are not present
        blockstore
            .slot_meta_iterator(0)
            .unwrap()
            .for_each(|(slot, _)| assert!(slot > 40));
    }
}
