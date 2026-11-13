//! `window_service` handles the data plane incoming shreds, storing them in
//!   blockstore and retransmitting where required
//!

use {
    crate::{
        completed_data_sets_service::CompletedDataSetsSender,
        repair::{
            block_id_repair_service::{BlockIdRepairChannels, BlockIdRepairService},
            repair_service::{
                OutstandingShredRepairs, RepairInfo, RepairService, RepairServiceChannels,
            },
        },
        result::{Error, Result},
    },
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded},
    solana_ledger::{
        blockstore::{Blockstore, BlockstoreInsertionMetrics, PossibleDuplicateShred},
        blockstore_db::{DBPinnableSlice, WriteBatch},
        shred::filter::{RetransmitSender, ShredRecoveryContext},
        sigverify_shreds::VerifiedShred,
    },
    solana_measure::measure::Measure,
    solana_net_utils::PinnedXdpSender,
    solana_runtime::bank_forks::SharableBanks,
    std::{
        net::UdpSocket,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

#[derive(Default)]
struct WindowServiceMetrics {
    run_insert_count: u64,
    num_repairs: usize,
    num_shreds_received: usize,
    shred_receiver_elapsed_us: u64,
    num_errors: u64,
    num_errors_blockstore: u64,
    num_errors_cross_beam_recv_timeout: u64,
    num_errors_other: u64,
    num_errors_try_crossbeam_send: u64,
}

impl WindowServiceMetrics {
    const NAME: &str = "recv-window-insert-shreds";

    fn report_metrics(&self) {
        datapoint_info!(
            Self::NAME,
            ("run_insert_count", self.run_insert_count as i64, i64),
            ("num_repairs", self.num_repairs, i64),
            ("num_shreds_received", self.num_shreds_received, i64),
            (
                "shred_receiver_elapsed_us",
                self.shred_receiver_elapsed_us as i64,
                i64
            ),
            ("num_errors", self.num_errors, i64),
            ("num_errors_blockstore", self.num_errors_blockstore, i64),
            ("num_errors_other", self.num_errors_other, i64),
            (
                "num_errors_try_crossbeam_send",
                self.num_errors_try_crossbeam_send,
                i64
            ),
            (
                "num_errors_cross_beam_recv_timeout",
                self.num_errors_cross_beam_recv_timeout,
                i64
            ),
        );
    }

    fn record_error(&mut self, err: &Error) {
        self.num_errors += 1;
        match err {
            Error::TrySend => self.num_errors_try_crossbeam_send += 1,
            Error::RecvTimeout(_) => self.num_errors_cross_beam_recv_timeout += 1,
            Error::Blockstore(err) => {
                self.num_errors_blockstore += 1;
                error!("blockstore error: {err}");
            }
            _ => self.num_errors_other += 1,
        }
    }
}

fn run_check_duplicate(
    blockstore: &Blockstore,
    shred_receiver: &Receiver<PossibleDuplicateShred>,
) -> Result<()> {
    let check_duplicate = |shred: PossibleDuplicateShred| -> Result<()> {
        let shred_slot = shred.slot();
        blockstore.set_dead_slot_if_duplicate_and_not_full(shred_slot)?;

        Ok(())
    };
    const RECV_TIMEOUT: Duration = Duration::from_millis(200);
    std::iter::once(shred_receiver.recv_timeout(RECV_TIMEOUT)?)
        .chain(shred_receiver.try_iter())
        .try_for_each(check_duplicate)
}

#[allow(clippy::too_many_arguments)]
fn run_insert<'db, F>(
    verified_receiver: &Receiver<Vec<VerifiedShred>>,
    blockstore: &'db Blockstore,
    shred_recovery_context: &mut ShredRecoveryContext,
    pinnable_slice: &mut DBPinnableSlice<'db>,
    write_batch: &mut WriteBatch,
    handle_duplicate: F,
    metrics: &mut BlockstoreInsertionMetrics,
    ws_metrics: &mut WindowServiceMetrics,
    completed_data_sets_sender: Option<&CompletedDataSetsSender>,
) -> Result<()>
where
    F: Fn(PossibleDuplicateShred),
{
    const RECV_TIMEOUT: Duration = Duration::from_millis(200);
    let mut shred_receiver_elapsed = Measure::start("shred_receiver_elapsed");
    let mut shreds = verified_receiver.recv_timeout(RECV_TIMEOUT)?;
    shreds.extend(verified_receiver.try_iter().flatten());
    shred_receiver_elapsed.stop();
    ws_metrics.shred_receiver_elapsed_us += shred_receiver_elapsed.as_us();
    ws_metrics.run_insert_count += 1;
    ws_metrics.num_repairs += shreds
        .iter()
        .filter(|verified_shred| verified_shred.is_repaired)
        .count();
    ws_metrics.num_shreds_received += shreds.len();
    let completed_data_sets = blockstore.insert_shreds_at_location_handle_duplicate(
        shreds,
        false, // is_trusted
        shred_recovery_context,
        pinnable_slice,
        write_batch,
        &handle_duplicate,
        metrics,
    )?;

    if let Some(sender) = completed_data_sets_sender {
        sender.try_send(completed_data_sets)?;
    }

    Ok(())
}

pub struct WindowServiceChannels {
    pub verified_receiver: Receiver<Vec<VerifiedShred>>,
    pub retransmit_sender: RetransmitSender,
    pub completed_data_sets_sender: Option<CompletedDataSetsSender>,
    pub repair_service_channels: RepairServiceChannels,
    pub block_id_repair_channels: BlockIdRepairChannels,
}

impl WindowServiceChannels {
    pub fn new(
        verified_receiver: Receiver<Vec<VerifiedShred>>,
        retransmit_sender: RetransmitSender,
        completed_data_sets_sender: Option<CompletedDataSetsSender>,
        repair_service_channels: RepairServiceChannels,
        block_id_repair_channels: BlockIdRepairChannels,
    ) -> Self {
        Self {
            verified_receiver,
            retransmit_sender,
            completed_data_sets_sender,
            repair_service_channels,
            block_id_repair_channels,
        }
    }
}

pub(crate) struct WindowService {
    t_insert: JoinHandle<()>,
    t_check_duplicate: JoinHandle<()>,
    repair_service: RepairService,
    block_id_repair_service: BlockIdRepairService,
}

impl WindowService {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        blockstore: Arc<Blockstore>,
        repair_socket: Arc<UdpSocket>,
        block_id_repair_socket: Arc<UdpSocket>,
        exit: Arc<AtomicBool>,
        repair_info: RepairInfo,
        window_service_channels: WindowServiceChannels,
        shred_version: u16,
        outstanding_repair_requests: Arc<RwLock<OutstandingShredRepairs>>,
        repair_xdp_sender: Option<PinnedXdpSender>,
    ) -> WindowService {
        let bank_forks = repair_info.bank_forks.clone();

        let WindowServiceChannels {
            verified_receiver,
            retransmit_sender,
            completed_data_sets_sender,
            repair_service_channels,
            block_id_repair_channels,
        } = window_service_channels;

        let repair_service = RepairService::new(
            blockstore.clone(),
            exit.clone(),
            repair_socket.clone(),
            repair_info.clone(),
            outstanding_repair_requests.clone(),
            repair_service_channels,
            repair_xdp_sender,
        );

        let block_id_repair_service = BlockIdRepairService::new(
            exit.clone(),
            blockstore.clone(),
            block_id_repair_socket,
            repair_socket,
            block_id_repair_channels,
            repair_info,
            outstanding_repair_requests,
        );

        let (duplicate_sender, duplicate_receiver) = unbounded();

        let t_check_duplicate = Self::start_check_duplicate_thread(
            exit.clone(),
            blockstore.clone(),
            duplicate_receiver,
        );

        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let t_insert = Self::start_window_insert_thread(
            exit,
            blockstore,
            sharable_banks,
            shred_version,
            verified_receiver,
            duplicate_sender,
            completed_data_sets_sender,
            retransmit_sender,
        );

        WindowService {
            t_insert,
            t_check_duplicate,
            repair_service,
            block_id_repair_service,
        }
    }

    fn start_check_duplicate_thread(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        duplicate_receiver: Receiver<PossibleDuplicateShred>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solWinCheckDup".to_string())
            .spawn(move || {
                while !exit.load(Ordering::Relaxed) {
                    if let Err(e) = run_check_duplicate(&blockstore, &duplicate_receiver)
                        && Self::should_exit_on_error(e)
                    {
                        break;
                    }
                }
            })
            .unwrap()
    }

    fn start_window_insert_thread(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        sharable_banks: SharableBanks,
        shred_version: u16,
        verified_receiver: Receiver<Vec<VerifiedShred>>,
        check_duplicate_sender: Sender<PossibleDuplicateShred>,
        completed_data_sets_sender: Option<CompletedDataSetsSender>,
        retransmit_sender: RetransmitSender,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solWinInsert".to_string())
            .spawn(move || {
                let handle_duplicate = |possible_duplicate_shred| {
                    let _ = check_duplicate_sender.send(possible_duplicate_shred);
                };

                const METRICS_REPORTING_INTERVAL: Duration = Duration::from_secs(2);
                let mut metrics = BlockstoreInsertionMetrics::default();
                let mut ws_metrics = WindowServiceMetrics::default();
                let mut last_print = Instant::now();
                let mut shred_recovery_context = ShredRecoveryContext::new(
                    retransmit_sender,
                    sharable_banks.root(),
                    shred_version,
                );
                let mut pinnable_slice = blockstore.new_pinnable_slice();
                let mut write_batch = blockstore.get_write_batch();

                while !exit.load(Ordering::Relaxed) {
                    shred_recovery_context.maybe_update(sharable_banks.root());

                    if let Err(e) = run_insert(
                        &verified_receiver,
                        &blockstore,
                        &mut shred_recovery_context,
                        &mut pinnable_slice,
                        &mut write_batch,
                        handle_duplicate,
                        &mut metrics,
                        &mut ws_metrics,
                        completed_data_sets_sender.as_ref(),
                    ) {
                        ws_metrics.record_error(&e);
                        if Self::should_exit_on_error(e) {
                            break;
                        }
                    }

                    if last_print.elapsed() > METRICS_REPORTING_INTERVAL {
                        metrics.report_metrics();
                        metrics = BlockstoreInsertionMetrics::default();
                        ws_metrics.report_metrics();
                        ws_metrics = WindowServiceMetrics::default();
                        last_print = Instant::now();
                    }
                    shred_recovery_context.maybe_submit_stats();
                }
            })
            .unwrap()
    }

    fn should_exit_on_error(e: Error) -> bool {
        match e {
            Error::RecvTimeout(RecvTimeoutError::Disconnected) => true,
            Error::RecvTimeout(RecvTimeoutError::Timeout) => false,
            Error::Send => true,
            _ => {
                let version = solana_version::version!();
                datapoint_error!(
                    "error",
                    ("thread", thread::current().name().unwrap_or("?"), String),
                    ("message", format!("{e}"), String),
                    ("version", version, String)
                );
                error!("thread {:?} error {:?}", thread::current().name(), e);
                false
            }
        }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.t_insert.join()?;
        self.t_check_duplicate.join()?;
        self.repair_service.join()?;
        self.block_id_repair_service.join()
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crossbeam_channel::bounded,
        rand::Rng,
        solana_clock::Slot,
        solana_entry::entry::{Entry, create_ticks},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::{Blockstore, make_many_slot_entries},
            get_tmp_ledger_path_auto_delete,
            shred::{ProcessShredsStats, Shred, Shredder, filter::new_dummy_retransmit_sender},
        },
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{create_genesis_config, create_genesis_config_with_leader},
        },
    };

    fn local_entries_to_shred(
        entries: &[Entry],
        slot: Slot,
        parent: Slot,
        keypair: &Keypair,
    ) -> Vec<Shred> {
        let shredder = Shredder::new(slot, parent, 0, 0).unwrap();
        let (data_shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            keypair,
            entries,
            true, // is_last_in_slot
            // chained_merkle_root
            Hash::new_from_array(rand::rng().random()),
            0, // next_shred_index
            0, // next_code_index
            &mut ProcessShredsStats::default(),
        );
        data_shreds
    }

    #[test]
    fn test_process_shred() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let num_entries = 10;
        let original_entries = create_ticks(num_entries, 0, Hash::default());
        let mut shreds = local_entries_to_shred(&original_entries, 0, 0, &Keypair::new());
        shreds.reverse();
        blockstore
            .insert_shreds(shreds, false)
            .expect("Expect successful processing of shred");

        assert_eq!(blockstore.get_slot_entries(0, 0).unwrap(), original_entries);
    }

    /// A conflicting shred at an occupied index stores the proof and marks the non-full
    /// slot dead at insert time.
    #[test]
    fn test_run_check_duplicate_marks_existing_index_conflict_dead() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let genesis_config = create_genesis_config(10_000).genesis_config;
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let (sender, receiver) = bounded(1024);
        let (shreds, _) = make_many_slot_entries(5, 1, 10);
        let original_shred = shreds[0].clone();
        let duplicate_shred = {
            let (mut shreds, _) = make_many_slot_entries(5, 1, 10);
            shreds.swap_remove(0)
        };
        let slot = original_shred.slot();
        blockstore
            .insert_shreds(vec![original_shred.clone()], false)
            .unwrap();

        let dummy_retransmit_sender = new_dummy_retransmit_sender();
        let is_trusted = false;
        blockstore
            .insert_shreds_handle_duplicate(
                [(duplicate_shred, /*is_repaired:*/ false)],
                is_trusted,
                &mut ShredRecoveryContext::new(
                    dummy_retransmit_sender,
                    bank_forks.read().unwrap().root_bank(),
                    0, // shred_version
                ),
                &|shred| sender.send(shred).unwrap(),
                &mut BlockstoreInsertionMetrics::default(),
            )
            .unwrap();
        assert!(blockstore.has_duplicate_shreds_in_slot(slot));
        assert!(
            blockstore.is_dead(slot),
            "duplicate slot that is not full must be marked dead at insert time"
        );

        run_check_duplicate(&blockstore, &receiver).unwrap();
        assert!(
            blockstore.is_dead(slot),
            "duplicate slot that is not full must be marked dead"
        );
    }

    #[test]
    fn test_store_duplicate_shreds_same_batch() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (duplicate_shred_sender, duplicate_shred_receiver) = bounded(1024);
        let exit = Arc::new(AtomicBool::new(false));
        let genesis_config =
            create_genesis_config_with_leader(10_000, &Pubkey::new_unique(), 1_000).genesis_config;
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));

        // Start duplicate thread receiving and inserting duplicates
        let t_check_duplicate = WindowService::start_check_duplicate_thread(
            exit.clone(),
            blockstore.clone(),
            duplicate_shred_receiver,
        );

        let handle_duplicate = |shred| {
            let _ = duplicate_shred_sender.send(shred);
        };
        let num_trials = 100;
        for slot in 0..num_trials {
            let (shreds, _) = make_many_slot_entries(slot, 1, 10);
            let duplicate_index = 0;
            let original_shred = shreds[duplicate_index].clone();
            let duplicate_shred = {
                let (mut shreds, _) = make_many_slot_entries(slot, 1, 10);
                shreds.swap_remove(duplicate_index)
            };
            assert_eq!(duplicate_shred.slot(), slot);
            // Simulate storing both duplicate shreds in the same batch
            let shreds = [&original_shred, &duplicate_shred]
                .into_iter()
                .map(|shred| (shred.clone(), /*is_repaired:*/ false));
            let dummy_retransmit_sender = new_dummy_retransmit_sender();
            blockstore
                .insert_shreds_handle_duplicate(
                    shreds,
                    false, // is_trusted
                    &mut ShredRecoveryContext::new(
                        dummy_retransmit_sender,
                        bank_forks.read().unwrap().root_bank(),
                        0, // shred_version
                    ),
                    &handle_duplicate,
                    &mut BlockstoreInsertionMetrics::default(),
                )
                .unwrap();

            // Make sure the correct duplicate proof was stored
            let deadline = Instant::now() + Duration::from_secs(5);
            let duplicate_proof = loop {
                if let Some(proof) = blockstore.get_duplicate_slot(slot) {
                    break proof;
                }
                assert!(Instant::now() < deadline, "duplicate proof was not stored");
                std::thread::yield_now();
            };
            assert_eq!(duplicate_proof.shred1, *original_shred.payload());
            assert_eq!(duplicate_proof.shred2, *duplicate_shred.payload());
        }
        exit.store(true, Ordering::Relaxed);
        t_check_duplicate.join().unwrap();
    }
}
