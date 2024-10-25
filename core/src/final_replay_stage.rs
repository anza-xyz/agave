use {
    crate::{
        cost_update_service::CostUpdate,
        rewards_recorder_service::{
            RewardsMessage, RewardsRecorderSender
        },
    }, crossbeam_channel::{Receiver, RecvTimeoutError, Sender}, rayon::{iter::{IntoParallelIterator, ParallelIterator}, ThreadPool}, solana_entry::entry::VerifyRecyclers,
    solana_geyser_plugin_manager::block_metadata_notifier_interface::BlockMetadataNotifierArc,
    solana_ledger::{blockstore::Blockstore, blockstore_processor::{
        self, BlockstoreProcessorError, ConfirmationProgress, ExecuteBatchesInternalMetrics, ReplaySlotStats, TransactionStatusSender},
        entry_notifier_service::EntryNotifierSender,
    }, solana_measure::measure::Measure,
    solana_rpc::optimistically_confirmed_bank_tracker::{
        BankNotification, BankNotificationSenderConfig,
    },
    solana_runtime::{
        bank::Bank, bank_forks::BankForks, prioritization_fee_cache::PrioritizationFeeCache
    }, solana_sdk::{
        clock::Slot,
        hash::Hash,
        pubkey::Pubkey,
    },
    solana_timings::ExecuteTimings,
    std::{
        collections::HashMap,
        num::NonZeroUsize,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, RwLock,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    }
};

/// Timing information for the ReplayStage main processing loop
#[derive(Default)]
struct FinalReplayLoopTiming {
    replay_active_banks_elapsed_us: u64,
    wait_receive_elapsed_us: u64,
    /// The time it took to replay the blockstore into the bank
    replay_blockstore_us: u64,
}

impl FinalReplayLoopTiming {
    fn update(&mut self, replay_active_banks_elapsed_us: u64, wait_receive_elapsed_us: u64) {
        self.replay_active_banks_elapsed_us = replay_active_banks_elapsed_us;
        self.wait_receive_elapsed_us = wait_receive_elapsed_us;
    }
}

#[derive(Clone, Default)]
struct FinalReplayProgress {
    pub stats: Arc<RwLock<ReplaySlotStats>>,
    pub progress: Arc<RwLock<ConfirmationProgress>>,
}

#[derive(Default)]
struct FinalReplayProgressMap {
    pub replay_stats: RwLock<HashMap<Slot, FinalReplayProgress>>,
}

impl FinalReplayProgressMap {
    pub fn insert_if_not_exist(&mut self, slot: &Slot) {
        self.replay_stats.write().unwrap().entry(*slot).or_default();
    }

    pub fn get(&self, slot: &Slot) -> Option<FinalReplayProgress> {
        self.replay_stats.read().unwrap().get(slot).cloned()
    }
}

pub struct FinalReplayConfig {
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub bank_notification_sender: Option<BankNotificationSenderConfig>,
    pub block_metadata_notifier: Option<BlockMetadataNotifierArc>,
    pub blockstore: Arc<Blockstore>,
    pub cost_update_sender: Sender<CostUpdate>,
    pub entry_notification_sender: Option<EntryNotifierSender>,
    pub exit: Arc<AtomicBool>,
    pub log_messages_bytes_limit: Option<usize>,
    pub my_pubkey: Pubkey,
    pub prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    pub replay_forks_threads: NonZeroUsize,
    pub replay_signal_receiver: Receiver<bool>,
    pub replay_transactions_threads: NonZeroUsize,
    pub rewards_recorder_sender: Option<RewardsRecorderSender>,
    pub transaction_status_sender: Option<TransactionStatusSender>,
}

pub struct FinalReplayStage {
    thread_hdl: JoinHandle<()>,
}

struct ReplayResult {
    pub bank_slot: Slot,
    pub replay_error: Option<BlockstoreProcessorError>,
}

impl FinalReplayStage {
    #[allow(clippy::too_many_arguments)]
    fn replay_active_banks(
        blockstore: &Blockstore,
        bank_forks: &RwLock<BankForks>,
        my_pubkey: &Pubkey,
        transaction_status_sender: Option<&TransactionStatusSender>,
        entry_notification_sender: Option<&EntryNotifierSender>,
        verify_recyclers: &VerifyRecyclers,
        bank_notification_sender: &Option<BankNotificationSenderConfig>,
        rewards_recorder_sender: &Option<RewardsRecorderSender>,
        cost_update_sender: &Sender<CostUpdate>,
        block_metadata_notifier: Option<BlockMetadataNotifierArc>,
        log_messages_bytes_limit: Option<usize>,
        fork_thread_pool: &ThreadPool,
        replay_tx_thread_pool: &ThreadPool,
        prioritization_fee_cache: &PrioritizationFeeCache,
        replay_timing: &mut FinalReplayLoopTiming,
        progress_map: &mut FinalReplayProgressMap,
    ) -> bool /* completed a bank */ {
        let active_bank_slots = bank_forks.read().unwrap().active_bank_slots();
        let num_active_banks = active_bank_slots.len();
        trace!(
            "{} active bank(s) to replay: {:?}",
            num_active_banks,
            active_bank_slots
        );
        if active_bank_slots.is_empty() {
            return false;
        }

        let replay_result_vec = Self::replay_active_banks_concurrently(
            blockstore,
            bank_forks,
            fork_thread_pool,
            replay_tx_thread_pool,
            my_pubkey,
            transaction_status_sender,
            entry_notification_sender,
            verify_recyclers,
            log_messages_bytes_limit,
            &active_bank_slots,
            prioritization_fee_cache,
            &mut replay_timing.replay_blockstore_us,
            progress_map,
        );

        Self::process_replay_results(
            bank_forks,
            transaction_status_sender,
            bank_notification_sender,
            rewards_recorder_sender,
            cost_update_sender,
            block_metadata_notifier,
            &replay_result_vec,
            progress_map,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn replay_active_banks_concurrently(
        blockstore: &Blockstore,
        bank_forks: &RwLock<BankForks>,
        fork_thread_pool: &ThreadPool,
        replay_tx_thread_pool: &ThreadPool,
        my_pubkey: &Pubkey,
        transaction_status_sender: Option<&TransactionStatusSender>,
        entry_notification_sender: Option<&EntryNotifierSender>,
        verify_recyclers: &VerifyRecyclers,
        log_messages_bytes_limit: Option<usize>,
        active_bank_slots: &[Slot],
        prioritization_fee_cache: &PrioritizationFeeCache,
        replay_blockstore_us: &mut u64,
        replay_stats: &mut FinalReplayProgressMap,
    ) -> Vec<ReplayResult> {
        let longest_replay_time_us = AtomicU64::new(0);
        // Apply insert_if_not_exist on all active_bank_slots.
        active_bank_slots.iter().for_each(|slot| {
            replay_stats.insert_if_not_exist(slot);
        });
        let replay_result_vec: Vec<ReplayResult> = fork_thread_pool.install(|| {
            active_bank_slots
                .into_par_iter()
                .map(|bank_slot| {
                    let bank_slot = *bank_slot;
                    let mut replay_result = ReplayResult {
                        bank_slot,
                        replay_error: None,
                    };
                    trace!(
                        "Replay active bank: slot {}, thread_idx {}",
                        bank_slot,
                        fork_thread_pool.current_thread_index().unwrap_or_default()
                    );

                    let bank = bank_forks
                        .read()
                        .unwrap()
                        .get_with_scheduler(bank_slot)
                        .unwrap();

                    if bank.collector_id() != my_pubkey {
                        let mut replay_blockstore_time =
                            Measure::start("replay_blockstore_into_bank");
                        let r_replay_progress = replay_stats.get(&bank_slot).unwrap();
                        let mut w_replay_stats = r_replay_progress.stats.write().unwrap();
                        let mut w_replay_progress = r_replay_progress.progress.write().unwrap();
                        if let Err(error) = blockstore_processor::confirm_slot(
                            blockstore,
                            &bank,
                            replay_tx_thread_pool,
                            &mut w_replay_stats,
                            &mut w_replay_progress,
                            false,
                            transaction_status_sender,
                            entry_notification_sender,
                            None,
                            verify_recyclers,
                            false,
                            log_messages_bytes_limit,
                            prioritization_fee_cache,
                            true,
                        ) {
                            warn!("All errors should have been handled during replay stage {bank_slot}: {error:?}");
                            replay_result.replay_error = Some(error);
                        }
                
                        replay_blockstore_time.stop();
                        longest_replay_time_us
                            .fetch_max(replay_blockstore_time.as_us(), Ordering::Relaxed);
                    }
                    replay_result
                })
                .collect()
        });

        *replay_blockstore_us += longest_replay_time_us.load(Ordering::Relaxed);
        replay_result_vec
    }

    #[allow(clippy::too_many_arguments)]
    fn process_replay_results(
        bank_forks: &RwLock<BankForks>,
        transaction_status_sender: Option<&TransactionStatusSender>,
        bank_notification_sender: &Option<BankNotificationSenderConfig>,
        rewards_recorder_sender: &Option<RewardsRecorderSender>,
        cost_update_sender: &Sender<CostUpdate>,
        block_metadata_notifier: Option<BlockMetadataNotifierArc>,
        replay_result_vec: &[ReplayResult],
        progress_map: &mut FinalReplayProgressMap,
    ) -> bool {
        let mut did_complete_bank = false;
        let mut execute_timings = ExecuteTimings::default();
        for replay_result in replay_result_vec {
            if replay_result.replay_error.is_some() {
                continue;
            }

            let bank_slot = replay_result.bank_slot;
            let bank = &bank_forks
                .read()
                .unwrap()
                .get_with_scheduler(bank_slot)
                .unwrap();

            assert_eq!(bank_slot, bank.slot());
            if bank.is_complete() {
                let mut bank_complete_time = Measure::start("bank_complete_time");
                let mut is_unified_scheduler_enabled = false;

                let r_replay_progress = progress_map.get(&bank_slot).unwrap();
                if let Some((result, completed_execute_timings)) =
                    bank.wait_for_completed_scheduler()
                {
                    // It's guaranteed that wait_for_completed_scheduler() returns Some(_), iff the
                    // unified scheduler is enabled for the bank.
                    is_unified_scheduler_enabled = true;
                    let metrics = ExecuteBatchesInternalMetrics::new_with_timings_from_all_threads(
                        completed_execute_timings,
                    );
                    r_replay_progress
                        .stats
                        .write()
                        .unwrap()
                        .batch_execute
                        .accumulate(metrics, is_unified_scheduler_enabled);

                    if let Err(error) = result {
                        warn!("All errors should have been handled during replay stage {bank_slot}: {error:?}");
                        continue;
                    }
                }

                let r_replay_stats = r_replay_progress.stats.read().unwrap();
                debug!(
                    "bank {} has completed replay from blockstore, contribute to update cost with \
                     {:?}",
                    bank.slot(),
                    r_replay_stats.batch_execute.totals
                );
                did_complete_bank = true;
                if let Some(transaction_status_sender) = transaction_status_sender {
                    transaction_status_sender.send_transaction_status_freeze_message(bank);
                }
                bank.freeze();
                datapoint_info!(
                    "bank_frozen",
                    ("slot", bank_slot, i64),
                    ("hash", bank.hash().to_string(), String),
                );
                // report cost tracker stats
                cost_update_sender
                    .send(CostUpdate::FrozenBank {
                        bank: bank.clone_without_scheduler(),
                    })
                    .unwrap_or_else(|err| {
                        warn!("cost_update_sender failed sending bank stats: {:?}", err)
                    });

                assert_ne!(bank.hash(), Hash::default());
                if let Some(sender) = bank_notification_sender {
                    sender
                        .sender
                        .send(BankNotification::Frozen(bank.clone_without_scheduler()))
                        .unwrap_or_else(|err| warn!("bank_notification_sender failed: {:?}", err));
                }

                Self::record_rewards(bank, rewards_recorder_sender);

                let r_replay_progress = r_replay_progress.progress.read().unwrap();
                if let Some(ref block_metadata_notifier) = block_metadata_notifier {
                    let parent_blockhash = bank
                        .parent()
                        .map(|bank| bank.last_blockhash())
                        .unwrap_or_default();
                    block_metadata_notifier.notify_block_metadata(
                        bank.parent_slot(),
                        &parent_blockhash.to_string(),
                        bank.slot(),
                        &bank.last_blockhash().to_string(),
                        &bank.get_rewards_and_num_partitions(),
                        Some(bank.clock().unix_timestamp),
                        Some(bank.block_height()),
                        bank.executed_transaction_count(),
                        r_replay_progress.num_entries as u64,
                    )
                }
                bank_complete_time.stop();

                r_replay_stats.report_stats(
                    bank.slot(),
                    r_replay_progress.num_txs,
                    r_replay_progress.num_entries,
                    r_replay_progress.num_shreds,
                    bank_complete_time.as_us(),
                    is_unified_scheduler_enabled,
                );
                execute_timings.accumulate(&r_replay_stats.batch_execute.totals);
            } else {
                trace!(
                    "bank {} not completed tick_height: {}, max_tick_height: {}",
                    bank.slot(),
                    bank.tick_height(),
                    bank.max_tick_height()
                );
            }
        }

        did_complete_bank
    }

    fn record_rewards(bank: &Bank, rewards_recorder_sender: &Option<RewardsRecorderSender>) {
        if let Some(rewards_recorder_sender) = rewards_recorder_sender {
            let rewards = bank.get_rewards_and_num_partitions();
            if rewards.should_record() {
                rewards_recorder_sender
                    .send(RewardsMessage::Batch((bank.slot(), rewards)))
                    .unwrap_or_else(|err| warn!("rewards_recorder_sender failed: {:?}", err));
            }
            rewards_recorder_sender
                .send(RewardsMessage::Complete(bank.slot()))
                .unwrap_or_else(|err| warn!("rewards_recorder_sender failed: {:?}", err));
        }
    }

    pub fn new(
        config: FinalReplayConfig,
    ) -> Self {
        let fork_thread_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.replay_forks_threads.get())
            .thread_name(|i| format!("solReplayFork{i:02}"))
            .build()
            .expect("new rayon threadpool");
        let replay_tx_thread_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.replay_transactions_threads.get())
            .thread_name(|i| format!("solReplayTx{i:02}"))
            .build()
            .expect("new rayon threadpool");
        let verify_recyclers = VerifyRecyclers::default();
        let mut replay_timing = FinalReplayLoopTiming::default();
        let mut progress_map = FinalReplayProgressMap::default();
        let t_replay = Builder::new()
            .name("solana-final-replay-stage".to_string())
            .spawn(move || {
                loop {
                    // Stop getting entries if we get exit signal
                    if config.exit.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut replay_active_banks_time = Measure::start("replay_active_banks_time");
                    let did_complete_bank = Self::replay_active_banks(
                        &config.blockstore,
                        &config.bank_forks,
                        &config.my_pubkey,
                        config.transaction_status_sender.as_ref(),
                        config.entry_notification_sender.as_ref(),
                        &verify_recyclers,
                        &config.bank_notification_sender,
                        &config.rewards_recorder_sender,
                        &config.cost_update_sender,
                        config.block_metadata_notifier.clone(),
                        config.log_messages_bytes_limit,
                        &fork_thread_pool,
                        &replay_tx_thread_pool,
                        &config.prioritization_fee_cache,
                        &mut replay_timing,
                        &mut progress_map,
                    );
                    replay_active_banks_time.stop();

                    let mut wait_receive_time = Measure::start("wait_receive_time");
                    if !did_complete_bank {
                        // only wait for the signal if we did not just process a bank; maybe there are more slots available
    
                        let timer = Duration::from_millis(100);
                        let result = config.replay_signal_receiver.recv_timeout(timer);
                        match result {
                            Err(RecvTimeoutError::Timeout) => (),
                            Err(_) => break,
                            Ok(_) => trace!("blockstore signal"),
                        };
                    }
                    wait_receive_time.stop();

                    replay_timing.update(
                        replay_active_banks_time.as_us(),
                        wait_receive_time.as_us(),
                    );
                }
            })
            .unwrap();
        Self { thread_hdl: t_replay }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}