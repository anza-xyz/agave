//! The `banking_stage` processes Transaction messages. It is intended to be used
//! to construct a software pipeline.

#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    self::{
        committer::Committer, consumer::Consumer, decision_maker::DecisionMaker,
        qos_service::QosService, vote_packet_receiver::VotePacketReceiver,
        vote_storage::VoteStorage,
    },
    crate::{
        banking_stage::{
            consume_worker::ConsumeWorker,
            transaction_scheduler::{
                prio_graph_scheduler::PrioGraphScheduler,
                scheduler_controller::{
                    SchedulerConfig, SchedulerController, DEFAULT_SCHEDULER_PACING_FILL_TIME_MILLIS,
                },
                scheduler_error::SchedulerError,
            },
        },
        validator::BlockProductionMethod,
    },
    agave_banking_stage_ingress_types::BankingPacketReceiver,
    crossbeam_channel::{unbounded, Receiver, Sender},
    futures::{stream::FuturesUnordered, StreamExt},
    histogram::Histogram,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfoQuery},
    solana_ledger::blockstore_processor::TransactionStatusSender,
    solana_perf::packet::PACKETS_PER_BATCH,
    solana_poh::{
        poh_controller::PohController, poh_recorder::PohRecorder,
        transaction_recorder::TransactionRecorder,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank, bank_forks::BankForks, prioritization_fee_cache::PrioritizationFeeCache,
        vote_sender_types::ReplayVoteSender,
    },
    solana_time_utils::AtomicInterval,
    std::{
        num::{NonZeroU64, NonZeroUsize, Saturating},
        ops::Deref,
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Arc, RwLock,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
    tokio::sync::mpsc,
    tokio_util::sync::CancellationToken,
    transaction_scheduler::{
        greedy_scheduler::{GreedyScheduler, GreedySchedulerConfig},
        prio_graph_scheduler::PrioGraphSchedulerConfig,
        receive_and_buffer::TransactionViewReceiveAndBuffer,
    },
    vote_worker::VoteWorker,
};

// Below modules are pub to allow use by banking_stage bench
pub mod committer;
pub mod consumer;
pub mod leader_slot_metrics;
pub mod qos_service;
pub mod vote_storage;

mod consume_worker;
mod vote_worker;

#[cfg(feature = "dev-context-only-utils")]
pub mod decision_maker;
#[cfg(not(feature = "dev-context-only-utils"))]
mod decision_maker;

mod latest_validator_vote_packet;
mod leader_slot_timing_metrics;
mod read_write_account_set;
mod vote_packet_receiver;

#[cfg(feature = "dev-context-only-utils")]
pub mod scheduler_messages;
#[cfg(not(feature = "dev-context-only-utils"))]
mod scheduler_messages;

pub mod transaction_scheduler;

#[cfg(feature = "dev-context-only-utils")]
pub mod unified_scheduler;
#[cfg(not(feature = "dev-context-only-utils"))]
pub(crate) mod unified_scheduler;

#[cfg(unix)]
mod progress_tracker;
#[cfg(unix)]
mod tpu_to_pack;

/// The maximum number of worker threads that can be spawned by banking stage.
/// 64 because `ThreadAwareAccountLocks` uses a `u64` as a bitmask to
/// track thread placement.
const MAX_NUM_WORKERS: NonZeroUsize = NonZeroUsize::new(64).unwrap();
const DEFAULT_NUM_WORKERS: NonZeroUsize = NonZeroUsize::new(4).unwrap();

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
const TOTAL_BUFFERED_PACKETS: usize = 100_000;
const SLOT_BOUNDARY_CHECK_PERIOD: Duration = Duration::from_millis(10);

#[derive(Debug, Default)]
pub struct BankingStageStats {
    last_report: AtomicInterval,
    tpu_counts: VoteSourceCounts,
    gossip_counts: VoteSourceCounts,
    pub(crate) dropped_duplicated_packets_count: AtomicUsize,
    dropped_forward_packets_count: AtomicUsize,
    current_buffered_packets_count: AtomicUsize,
    rebuffered_packets_count: AtomicUsize,
    consumed_buffered_packets_count: AtomicUsize,
    batch_packet_indexes_len: Histogram,

    // Timing
    consume_buffered_packets_elapsed: AtomicU64,
    receive_and_buffer_packets_elapsed: AtomicU64,
    filter_pending_packets_elapsed: AtomicU64,
    pub(crate) packet_conversion_elapsed: AtomicU64,
    transaction_processing_elapsed: AtomicU64,
}

#[derive(Debug, Default)]
struct VoteSourceCounts {
    receive_and_buffer_packets_count: AtomicUsize,
    dropped_packets_count: AtomicUsize,
    newly_buffered_packets_count: AtomicUsize,
    newly_buffered_forwarded_packets_count: AtomicUsize,
}

impl VoteSourceCounts {
    fn is_empty(&self) -> bool {
        0 == self
            .receive_and_buffer_packets_count
            .load(Ordering::Relaxed)
            + self.dropped_packets_count.load(Ordering::Relaxed)
            + self.newly_buffered_packets_count.load(Ordering::Relaxed)
            + self
                .newly_buffered_forwarded_packets_count
                .load(Ordering::Relaxed)
    }
}

impl BankingStageStats {
    pub fn new() -> Self {
        BankingStageStats {
            batch_packet_indexes_len: Histogram::configure()
                .max_value(PACKETS_PER_BATCH as u64)
                .build()
                .unwrap(),
            ..BankingStageStats::default()
        }
    }

    fn is_empty(&self) -> bool {
        self.gossip_counts.is_empty()
            && self.tpu_counts.is_empty()
            && 0 == self
                .dropped_duplicated_packets_count
                .load(Ordering::Relaxed) as u64
                + self.dropped_forward_packets_count.load(Ordering::Relaxed) as u64
                + self.current_buffered_packets_count.load(Ordering::Relaxed) as u64
                + self.rebuffered_packets_count.load(Ordering::Relaxed) as u64
                + self.consumed_buffered_packets_count.load(Ordering::Relaxed) as u64
                + self
                    .consume_buffered_packets_elapsed
                    .load(Ordering::Relaxed)
                + self
                    .receive_and_buffer_packets_elapsed
                    .load(Ordering::Relaxed)
                + self.filter_pending_packets_elapsed.load(Ordering::Relaxed)
                + self.packet_conversion_elapsed.load(Ordering::Relaxed)
                + self.transaction_processing_elapsed.load(Ordering::Relaxed)
                + self.batch_packet_indexes_len.entries()
    }

    fn report(&mut self, report_interval_ms: u64) {
        // skip reporting metrics if stats is empty
        if self.is_empty() {
            return;
        }
        if self.last_report.should_update(report_interval_ms) {
            datapoint_info!(
                "banking_stage-vote_loop_stats",
                (
                    "tpu_receive_and_buffer_packets_count",
                    self.tpu_counts
                        .receive_and_buffer_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "tpu_dropped_packets_count",
                    self.tpu_counts
                        .dropped_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "tpu_newly_buffered_packets_count",
                    self.tpu_counts
                        .newly_buffered_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "tpu_newly_buffered_forwarded_packets_count",
                    self.tpu_counts
                        .newly_buffered_forwarded_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "gossip_receive_and_buffer_packets_count",
                    self.gossip_counts
                        .receive_and_buffer_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "gossip_dropped_packets_count",
                    self.gossip_counts
                        .dropped_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "gossip_newly_buffered_packets_count",
                    self.gossip_counts
                        .newly_buffered_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "gossip_newly_buffered_forwarded_packets_count",
                    self.gossip_counts
                        .newly_buffered_forwarded_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "dropped_duplicated_packets_count",
                    self.dropped_duplicated_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "dropped_forward_packets_count",
                    self.dropped_forward_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "current_buffered_packets_count",
                    self.current_buffered_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "rebuffered_packets_count",
                    self.rebuffered_packets_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "consumed_buffered_packets_count",
                    self.consumed_buffered_packets_count
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "consume_buffered_packets_elapsed",
                    self.consume_buffered_packets_elapsed
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "receive_and_buffer_packets_elapsed",
                    self.receive_and_buffer_packets_elapsed
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "filter_pending_packets_elapsed",
                    self.filter_pending_packets_elapsed
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "packet_conversion_elapsed",
                    self.packet_conversion_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "transaction_processing_elapsed",
                    self.transaction_processing_elapsed
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "packet_batch_indices_len_min",
                    self.batch_packet_indexes_len.minimum().unwrap_or(0),
                    i64
                ),
                (
                    "packet_batch_indices_len_max",
                    self.batch_packet_indexes_len.maximum().unwrap_or(0),
                    i64
                ),
                (
                    "packet_batch_indices_len_mean",
                    self.batch_packet_indexes_len.mean().unwrap_or(0),
                    i64
                ),
                (
                    "packet_batch_indices_len_90pct",
                    self.batch_packet_indexes_len.percentile(90.0).unwrap_or(0),
                    i64
                )
            );
            self.batch_packet_indexes_len.clear();
        }
    }
}

#[derive(Debug, Default)]
pub struct BatchedTransactionDetails {
    pub costs: BatchedTransactionCostDetails,
    pub errors: BatchedTransactionErrorDetails,
}

#[derive(Debug, Default)]
pub struct BatchedTransactionCostDetails {
    pub batched_signature_cost: Saturating<u64>,
    pub batched_write_lock_cost: Saturating<u64>,
    pub batched_data_bytes_cost: Saturating<u64>,
    pub batched_loaded_accounts_data_size_cost: Saturating<u64>,
    pub batched_programs_execute_cost: Saturating<u64>,
}

#[derive(Debug, Default)]
pub struct BatchedTransactionErrorDetails {
    pub batched_retried_txs_per_block_limit_count: Saturating<u64>,
    pub batched_retried_txs_per_vote_limit_count: Saturating<u64>,
    pub batched_retried_txs_per_account_limit_count: Saturating<u64>,
    pub batched_retried_txs_per_account_data_block_limit_count: Saturating<u64>,
    pub batched_dropped_txs_per_account_data_total_limit_count: Saturating<u64>,
}

pub trait LikeClusterInfo: Send + Sync + 'static + Clone {
    fn id(&self) -> Pubkey;

    fn lookup_contact_info<R>(&self, id: &Pubkey, query: impl ContactInfoQuery<R>) -> Option<R>;
}

impl LikeClusterInfo for Arc<ClusterInfo> {
    fn id(&self) -> Pubkey {
        self.deref().id()
    }

    fn lookup_contact_info<R>(&self, id: &Pubkey, query: impl ContactInfoQuery<R>) -> Option<R> {
        self.deref().lookup_contact_info(id, query)
    }
}

pub struct BankingStage {
    banking_shutdown_signal: CancellationToken,
    worker_exit_signal: Arc<AtomicBool>,
    banking_control_receiver: mpsc::Receiver<BankingControlMsg>,
    tpu_vote_receiver: BankingPacketReceiver,
    gossip_vote_receiver: BankingPacketReceiver,
    non_vote_receiver: BankingPacketReceiver,
    transaction_recorder: TransactionRecorder,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    bank_forks: Arc<RwLock<BankForks>>,
    committer: Committer,
    log_messages_bytes_limit: Option<usize>,
    threads: FuturesUnordered<NamedTask<std::thread::Result<()>>>,
}

impl BankingStage {
    #[allow(clippy::too_many_arguments)]
    pub fn new_num_threads(
        block_production_method: BlockProductionMethod,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        transaction_recorder: TransactionRecorder,
        non_vote_receiver: BankingPacketReceiver,
        tpu_vote_receiver: BankingPacketReceiver,
        gossip_vote_receiver: BankingPacketReceiver,
        banking_control_receiver: mpsc::Receiver<BankingControlMsg>,
        num_workers: NonZeroUsize,
        scheduler_config: SchedulerConfig,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: ReplayVoteSender,
        log_messages_bytes_limit: Option<usize>,
        bank_forks: Arc<RwLock<BankForks>>,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    ) -> BankingStageHandle {
        let committer = Committer::new(
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
        );

        // Setup the manager thread state.
        let banking_shutdown_signal = CancellationToken::new();
        let manager = BankingStage {
            banking_shutdown_signal: banking_shutdown_signal.clone(),
            worker_exit_signal: Arc::new(AtomicBool::new(false)),
            banking_control_receiver,
            tpu_vote_receiver,
            gossip_vote_receiver,
            non_vote_receiver,
            transaction_recorder,
            poh_recorder,
            bank_forks,
            committer,
            log_messages_bytes_limit,
            threads: FuturesUnordered::default(),
        };

        // Spawn the manager thread.
        let thread = std::thread::Builder::new()
            .name("BankingMgr".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(manager.run(BankingControlMsg::Internal {
                    block_production_method,
                    num_workers,
                    config: scheduler_config,
                }))
            })
            .unwrap();

        BankingStageHandle {
            banking_shutdown_signal,
            thread,
        }
    }

    async fn run(mut self, initial_args: BankingControlMsg) -> std::thread::Result<()> {
        self.spawn_scheduler(initial_args);

        loop {
            tokio::select! {
                biased;

                _ = self.banking_shutdown_signal.cancelled() => break,
                Some(args) = self.banking_control_receiver.recv() => self.cycle_threads(args).await,
                opt = self.threads.next() => {
                    let (name, res) = opt.unwrap();
                    match res.unwrap() {
                        Ok(()) => error!("Banking worker exited unexpectedly; name={name}"),
                        Err(err) => error!("Banking worker exited with error; name={name}; err={err:?}"),
                    };

                    self.cycle_threads(BankingControlMsg::Internal {
                        block_production_method: BlockProductionMethod::default(),
                        num_workers: BankingStage::default_num_workers(),
                        config: SchedulerConfig::default(),
                    }).await;
                },
            }
        }

        // Signal shutdown & wait for all threads to exit.
        self.worker_exit_signal.store(true, Ordering::Relaxed);
        while let Some((_, res)) = self.threads.next().await {
            res.unwrap()?;
        }

        Ok(())
    }

    async fn cycle_threads(&mut self, args: BankingControlMsg) {
        // Shutdown all current threads.
        self.worker_exit_signal.store(true, Ordering::Relaxed);
        while let Some((name, res)) = self.threads.next().await {
            match res.unwrap() {
                Ok(()) => info!("Banking worker exited cleanly; name={name}"),
                Err(err) => error!("Banking worker exited with error; name={name}; err={err:?}"),
            }
        }

        // Revert the exit signal.
        self.worker_exit_signal.store(false, Ordering::Relaxed);

        // Spawn the requested threads.
        self.spawn_scheduler(args);
    }

    fn spawn_scheduler(&mut self, args: BankingControlMsg) {
        let threads = match args {
            BankingControlMsg::Internal {
                block_production_method,
                num_workers,
                config,
            } => self.spawn_internal(
                matches!(
                    block_production_method,
                    BlockProductionMethod::CentralSchedulerGreedy
                ),
                num_workers,
                config,
            ),
            #[cfg(unix)]
            BankingControlMsg::External { session } => self.spawn_external(session),
        };

        self.threads.extend(threads.into_iter().map(|handle| {
            let name = handle.thread().name().unwrap().to_string();

            NamedTask::new(tokio::task::spawn_blocking(|| handle.join()), name)
        }));

        info!("Scheduler spawned");
    }

    fn spawn_internal(
        &self,
        use_greedy_scheduler: bool,
        num_workers: NonZeroUsize,
        scheduler_config: SchedulerConfig,
    ) -> Vec<JoinHandle<()>> {
        info!("Spawning internal scheduler");
        assert!(num_workers <= BankingStage::max_num_workers());
        let num_workers = num_workers.get();

        let exit = self.worker_exit_signal.clone();

        // Setup receive & buffer.
        let receive_and_buffer = TransactionViewReceiveAndBuffer {
            receiver: self.non_vote_receiver.clone(),
            bank_forks: self.bank_forks.clone(),
        };

        // Spawn vote worker.
        let mut threads = Vec::with_capacity(num_workers + 2);
        threads.push(self.spawn_vote_worker());

        // Create channels for communication between scheduler and workers
        let (work_senders, work_receivers): (Vec<Sender<_>>, Vec<Receiver<_>>) =
            (0..num_workers).map(|_| unbounded()).unzip();
        let (finished_work_sender, finished_work_receiver) = unbounded();

        // Spawn the worker threads
        let decision_maker = DecisionMaker::from(self.poh_recorder.read().unwrap().deref());
        let mut worker_metrics = Vec::with_capacity(num_workers);
        for (index, work_receiver) in work_receivers.into_iter().enumerate() {
            let id = index as u32;
            let consume_worker = ConsumeWorker::new(
                id,
                exit.clone(),
                work_receiver,
                Consumer::new(
                    self.committer.clone(),
                    self.transaction_recorder.clone(),
                    QosService::new(id),
                    self.log_messages_bytes_limit,
                ),
                finished_work_sender.clone(),
                self.poh_recorder.read().unwrap().shared_leader_state(),
            );

            worker_metrics.push(consume_worker.metrics_handle());
            threads.push(
                Builder::new()
                    .name(format!("solCoWorker{id:02}"))
                    .spawn(|| {
                        if let Err(err) = consume_worker.run() {
                            error!("Internal consume worker error; err={err}");
                        }
                    })
                    .unwrap(),
            )
        }

        // Macro to spawn the scheduler. Different type on `scheduler` and thus
        // scheduler_controller mean we cannot have an easy if for `scheduler`
        // assignment without introducing `dyn`.
        macro_rules! spawn_scheduler {
            ($scheduler:ident) => {
                let exit = exit.clone();
                let bank_forks = self.bank_forks.clone();
                threads.push(
                    Builder::new()
                        .name("solBnkTxSched".to_string())
                        .spawn(move || {
                            let scheduler_controller = SchedulerController::new(
                                exit,
                                scheduler_config,
                                decision_maker,
                                receive_and_buffer,
                                bank_forks,
                                $scheduler,
                                worker_metrics,
                            );

                            match scheduler_controller.run() {
                                Ok(_) => {}
                                Err(SchedulerError::DisconnectedRecvChannel(_)) => {}
                                Err(SchedulerError::DisconnectedSendChannel(_)) => {
                                    warn!("Unexpected worker disconnect from scheduler")
                                }
                            }
                        })
                        .unwrap(),
                );
            };
        }

        // Spawn the central scheduler thread
        if use_greedy_scheduler {
            let scheduler = GreedyScheduler::new(
                work_senders,
                finished_work_receiver,
                GreedySchedulerConfig::default(),
            );
            spawn_scheduler!(scheduler);
        } else {
            let scheduler = PrioGraphScheduler::new(
                work_senders,
                finished_work_receiver,
                PrioGraphSchedulerConfig::default(),
            );
            spawn_scheduler!(scheduler);
        }

        threads
    }

    fn spawn_vote_worker(&self) -> JoinHandle<()> {
        let vote_storage = VoteStorage::new(&self.bank_forks.read().unwrap().working_bank());
        let tpu_receiver = VotePacketReceiver::new(self.tpu_vote_receiver.clone());
        let gossip_receiver = VotePacketReceiver::new(self.gossip_vote_receiver.clone());
        let consumer = Consumer::new(
            self.committer.clone(),
            self.transaction_recorder.clone(),
            QosService::new(0),
            self.log_messages_bytes_limit,
        );
        let decision_maker = DecisionMaker::from(self.poh_recorder.read().unwrap().deref());

        let worker_exit_signal = self.worker_exit_signal.clone();
        let bank_forks = self.bank_forks.clone();
        Builder::new()
            .name("solBanknStgVote".to_string())
            .spawn(move || {
                VoteWorker::new(
                    worker_exit_signal,
                    decision_maker,
                    tpu_receiver,
                    gossip_receiver,
                    vote_storage,
                    bank_forks,
                    consumer,
                )
                .run()
            })
            .unwrap()
    }

    pub fn default_num_workers() -> NonZeroUsize {
        DEFAULT_NUM_WORKERS
    }

    pub const fn max_num_workers() -> NonZeroUsize {
        MAX_NUM_WORKERS
    }

    pub const fn default_fill_time_millis() -> NonZeroU64 {
        DEFAULT_SCHEDULER_PACING_FILL_TIME_MILLIS
    }
}

#[cfg(unix)]
mod external {
    use {
        super::*,
        crate::banking_stage::consume_worker::external::ExternalWorker,
        agave_scheduling_utils::handshake::{
            logon_flags,
            server::{AgaveSession, AgaveWorkerSession},
        },
        tpu_to_pack::BankingPacketReceivers,
    };

    impl BankingStage {
        pub(super) fn spawn_external(
            &self,
            AgaveSession {
                flags,
                tpu_to_pack,
                progress_tracker,
                workers,
            }: AgaveSession,
        ) -> Vec<JoinHandle<()>> {
            info!("Spawning external scheduler");
            static_assertions::const_assert!(
                agave_scheduling_utils::handshake::MAX_WORKERS
                    == BankingStage::max_num_workers().get()
            );
            assert!(workers.len() <= BankingStage::max_num_workers().get());

            // Potentially spawn vote worker.
            let mut threads = Vec::with_capacity(workers.len() + 3);
            let tpu_to_pack_receivers = if flags & logon_flags::REROUTE_VOTES != 0 {
                BankingPacketReceivers {
                    non_vote_receiver: self.non_vote_receiver.clone(),
                    gossip_vote_receiver: Some(self.gossip_vote_receiver.clone()),
                    tpu_vote_receiver: Some(self.tpu_vote_receiver.clone()),
                }
            } else {
                threads.push(self.spawn_vote_worker());

                BankingPacketReceivers {
                    non_vote_receiver: self.non_vote_receiver.clone(),
                    gossip_vote_receiver: None,
                    tpu_vote_receiver: None,
                }
            };

            // Spawn the external consumer workers.
            let mut worker_metrics = Vec::with_capacity(workers.len());
            for (
                index,
                AgaveWorkerSession {
                    allocator,
                    pack_to_worker,
                    worker_to_pack,
                },
            ) in workers.into_iter().enumerate()
            {
                let id = index as u32;
                let consume_worker = ExternalWorker::new(
                    id,
                    self.worker_exit_signal.clone(),
                    Consumer::new(
                        self.committer.clone(),
                        self.transaction_recorder.clone(),
                        QosService::new(id),
                        self.log_messages_bytes_limit,
                    ),
                    worker_to_pack,
                    allocator,
                    self.poh_recorder.read().unwrap().shared_leader_state(),
                    self.bank_forks.read().unwrap().sharable_banks(),
                );

                worker_metrics.push(consume_worker.metrics_handle());
                threads.push(
                    Builder::new()
                        .name(format!("solECoWorker{id:02}"))
                        .spawn(move || {
                            if let Err(err) = consume_worker.run(pack_to_worker) {
                                error!("External consume worker error; err={err}");
                            }
                        })
                        .unwrap(),
                );
            }

            // Spawn tpu to pack.
            threads.push(tpu_to_pack::spawn(
                self.worker_exit_signal.clone(),
                tpu_to_pack_receivers,
                tpu_to_pack,
            ));

            // Spawn progress tracker.
            let (shared_leader_state, ticks_per_slot) = {
                let poh = self.poh_recorder.read().unwrap();

                (poh.shared_leader_state(), poh.ticks_per_slot())
            };
            threads.push(progress_tracker::spawn(
                self.worker_exit_signal.clone(),
                progress_tracker,
                shared_leader_state,
                worker_metrics,
                ticks_per_slot,
            ));

            threads
        }
    }
}

pub struct BankingStageHandle {
    banking_shutdown_signal: CancellationToken,
    thread: JoinHandle<std::thread::Result<()>>,
}

impl BankingStageHandle {
    pub fn join(self) -> thread::Result<()> {
        self.banking_shutdown_signal.cancel();
        self.thread.join().unwrap()
    }
}

pub enum BankingControlMsg {
    Internal {
        block_production_method: BlockProductionMethod,
        num_workers: NonZeroUsize,
        config: SchedulerConfig,
    },
    #[cfg(unix)]
    External {
        session: agave_scheduling_utils::handshake::server::AgaveSession,
    },
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) fn update_bank_forks_and_poh_recorder_for_new_tpu_bank(
    bank_forks: &RwLock<BankForks>,
    poh_controller: &mut PohController,
    tpu_bank: Bank,
) {
    let tpu_bank = bank_forks.write().unwrap().insert(tpu_bank);
    if poh_controller.set_bank(tpu_bank).is_err() {
        warn!("Failed to set poh bank, poh service is disconnected");
    }
}

#[derive(Debug)]
struct NamedTask<Ret = (), Name = String>
where
    Name: Clone + Unpin,
{
    task: std::pin::Pin<Box<tokio::task::JoinHandle<Ret>>>,
    name: Name,
}

impl<Ret, Name> NamedTask<Ret, Name>
where
    Name: Clone + Unpin,
{
    fn new(task: tokio::task::JoinHandle<Ret>, name: Name) -> Self {
        NamedTask {
            task: Box::pin(task),
            name,
        }
    }
}

impl<R, I> std::future::Future for NamedTask<R, I>
where
    I: Clone + Unpin,
{
    type Output = (I, Result<R, tokio::task::JoinError>);

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.task.as_mut().poll(cx).map(|v| (self.name.clone(), v))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            banking_trace::{BankingTracer, Channels},
            validator::SchedulerPacing,
        },
        agave_banking_stage_ingress_types::BankingPacketBatch,
        crossbeam_channel::unbounded,
        itertools::Itertools,
        solana_entry::entry::{self, EntrySlice},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::Blockstore,
            genesis_utils::{
                create_genesis_config, create_genesis_config_with_leader, GenesisConfigInfo,
            },
            get_tmp_ledger_path_auto_delete,
        },
        solana_perf::packet::to_packet_batches,
        solana_poh::{
            poh_recorder::{create_test_recorder, PohRecorderError},
            record_channels::record_channels,
            transaction_recorder::RecordTransactionsSummary,
        },
        solana_poh_config::PohConfig,
        solana_pubkey::Pubkey,
        solana_runtime::{bank::Bank, genesis_utils::bootstrap_validator_stake_lamports},
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_system_transaction as system_transaction,
        solana_transaction::{sanitized::SanitizedTransaction, Transaction},
        solana_vote::vote_transaction::new_tower_sync_transaction,
        solana_vote_program::vote_state::TowerSync,
        std::{sync::atomic::Ordering, thread::sleep, time::Instant},
    };

    pub(crate) fn sanitize_transactions(
        txs: Vec<Transaction>,
    ) -> Vec<RuntimeTransaction<SanitizedTransaction>> {
        txs.into_iter()
            .map(RuntimeTransaction::from_transaction_for_tests)
            .collect()
    }

    #[test]
    fn test_banking_stage_shutdown1() {
        let genesis_config = create_genesis_config(2).genesis_config;
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer.create_channels(false);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger"),
        );
        let (
            exit,
            poh_recorder,
            _poh_controller,
            transaction_recorder,
            poh_service,
            _entry_receiever,
        ) = create_test_recorder(bank, blockstore, None, None);
        let (replay_vote_sender, _replay_vote_receiver) = unbounded();

        let banking_stage = BankingStage::new_num_threads(
            BlockProductionMethod::CentralScheduler,
            poh_recorder.clone(),
            transaction_recorder,
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_vote_receiver,
            mpsc::channel(1).1,
            DEFAULT_NUM_WORKERS,
            SchedulerConfig {
                scheduler_pacing: SchedulerPacing::Disabled,
            },
            None,
            replay_vote_sender,
            None,
            bank_forks,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        drop(non_vote_sender);
        drop(tpu_vote_sender);
        drop(gossip_vote_sender);
        exit.store(true, Ordering::Relaxed);
        banking_stage.join().unwrap();
        poh_service.join().unwrap();
    }

    #[test]
    fn test_banking_stage_tick() {
        agave_logger::setup();
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(2);
        genesis_config.ticks_per_slot = 4;
        let num_extra_ticks = 2;
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let start_hash = bank.last_blockhash();
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer.create_channels(false);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger"),
        );
        let poh_config = PohConfig {
            target_tick_count: Some(bank.max_tick_height() + num_extra_ticks),
            ..PohConfig::default()
        };
        let (
            exit,
            poh_recorder,
            _poh_controller,
            transaction_recorder,
            poh_service,
            entry_receiver,
        ) = create_test_recorder(bank.clone(), blockstore, Some(poh_config), None);
        let (replay_vote_sender, _replay_vote_receiver) = unbounded();

        let banking_stage = BankingStage::new_num_threads(
            BlockProductionMethod::CentralScheduler,
            poh_recorder.clone(),
            transaction_recorder,
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_vote_receiver,
            mpsc::channel(1).1,
            DEFAULT_NUM_WORKERS,
            SchedulerConfig {
                scheduler_pacing: SchedulerPacing::Disabled,
            },
            None,
            replay_vote_sender,
            None,
            bank_forks,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        trace!("sending bank");
        drop(non_vote_sender);
        drop(tpu_vote_sender);
        drop(gossip_vote_sender);
        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
        drop(poh_recorder);
        banking_stage.join().unwrap();

        trace!("getting entries");
        let entries: Vec<_> = entry_receiver
            .iter()
            .map(|(_bank, (entry, _tick_height))| entry)
            .collect();
        trace!("done");
        assert_eq!(entries.len(), genesis_config.ticks_per_slot as usize);
        assert!(entries
            .verify(&start_hash, &entry::thread_pool_for_tests())
            .status());
        assert_eq!(entries[entries.len() - 1].hash, bank.last_blockhash());
    }

    #[test]
    fn test_banking_stage_entries_only_central_scheduler() {
        agave_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10);
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let start_hash = bank.last_blockhash();
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer.create_channels(false);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger"),
        );
        let (
            exit,
            poh_recorder,
            _poh_controller,
            transaction_recorder,
            poh_service,
            entry_receiver,
        ) = create_test_recorder(bank.clone(), blockstore, None, None);
        let (replay_vote_sender, _replay_vote_receiver) = unbounded();

        let banking_stage = BankingStage::new_num_threads(
            BlockProductionMethod::CentralScheduler,
            poh_recorder.clone(),
            transaction_recorder,
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_vote_receiver,
            mpsc::channel(1).1,
            DEFAULT_NUM_WORKERS,
            SchedulerConfig {
                scheduler_pacing: SchedulerPacing::Disabled,
            },
            None,
            replay_vote_sender,
            None,
            bank_forks.clone(), // keep a local-copy of bank-forks so worker threads do not lose weak access to bank-forks
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );

        // good tx, and no verify
        let to = solana_pubkey::new_rand();
        let tx_no_ver = system_transaction::transfer(&mint_keypair, &to, 2, start_hash);

        // good tx
        let to2 = solana_pubkey::new_rand();
        let tx = system_transaction::transfer(&mint_keypair, &to2, 1, start_hash);

        // bad tx, AccountNotFound
        let keypair = Keypair::new();
        let to3 = solana_pubkey::new_rand();
        let tx_anf = system_transaction::transfer(&keypair, &to3, 1, start_hash);

        // send 'em over
        let mut packet_batches = to_packet_batches(&[tx_no_ver, tx_anf, tx], 3);
        packet_batches[0]
            .first_mut()
            .unwrap()
            .meta_mut()
            .set_discard(true); // set discard on `tx_no_ver`

        // glad they all fit
        assert_eq!(packet_batches.len(), 1);

        non_vote_sender // no_ver, anf, tx
            .send(BankingPacketBatch::new(packet_batches))
            .unwrap();

        // capture the entry receiver until we've received all our entries.
        let mut entries = Vec::with_capacity(100);
        loop {
            if let Ok((_bank, (entry, _))) = entry_receiver.try_recv() {
                let tx_entry = !entry.transactions.is_empty();
                entries.push(entry);
                if tx_entry {
                    break; // once we have the entry break. don't expect more than one.
                }
            }
            sleep(Duration::from_millis(10));
        }

        drop(non_vote_sender);
        drop(tpu_vote_sender);
        drop(gossip_vote_sender);
        banking_stage.join().unwrap();

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
        drop(poh_recorder);

        let blockhash = start_hash;
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);

        // receive entries + ticks. The sender has been dropped, so there
        // are no more entries that will ever come in after the `iter` here.
        entries.extend(
            entry_receiver
                .iter()
                .map(|(_bank, (entry, _tick_height))| entry),
        );

        assert!(entries
            .verify(&blockhash, &entry::thread_pool_for_tests())
            .status());
        for entry in entries {
            bank.process_entry_transactions(entry.transactions)
                .iter()
                .for_each(|x| assert_eq!(*x, Ok(())));
        }

        assert_eq!(bank.get_balance(&to2), 1);
        assert_eq!(bank.get_balance(&to), 0);

        drop(entry_receiver);
    }

    #[test]
    fn test_banking_stage_entryfication() {
        agave_logger::setup();
        // In this attack we'll demonstrate that a verifier can interpret the ledger
        // differently if either the server doesn't signal the ledger to add an
        // Entry OR if the verifier tries to parallelize across multiple Entries.
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(2);
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer.create_channels(false);

        // Process a batch that includes a transaction that receives two lamports.
        let alice = Keypair::new();
        let tx =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 2, genesis_config.hash());

        let packet_batches = to_packet_batches(&[tx], 1);
        non_vote_sender
            .send(BankingPacketBatch::new(packet_batches))
            .unwrap();

        // Process a second batch that uses the same from account, so conflicts with above TX
        let tx =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 1, genesis_config.hash());
        let packet_batches = to_packet_batches(&[tx], 1);
        non_vote_sender
            .send(BankingPacketBatch::new(packet_batches))
            .unwrap();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger"),
        );

        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let entry_receiver = {
            // start a banking_stage to eat verified receiver
            let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
            let (
                exit,
                poh_recorder,
                _poh_controller,
                transaction_recorder,
                poh_service,
                entry_receiver,
            ) = create_test_recorder(bank.clone(), blockstore, None, None);
            let banking_stage = BankingStage::new_num_threads(
                BlockProductionMethod::CentralScheduler,
                poh_recorder.clone(),
                transaction_recorder,
                non_vote_receiver,
                tpu_vote_receiver,
                gossip_vote_receiver,
                mpsc::channel(1).1,
                DEFAULT_NUM_WORKERS,
                SchedulerConfig {
                    scheduler_pacing: SchedulerPacing::Disabled,
                },
                None,
                replay_vote_sender,
                None,
                bank_forks,
                Arc::new(PrioritizationFeeCache::new(0u64)),
            );

            // wait for banking_stage to eat the packets
            const TIMEOUT: Duration = Duration::from_secs(10);
            let start = Instant::now();
            while bank.get_balance(&alice.pubkey()) < 1 {
                if start.elapsed() > TIMEOUT {
                    panic!("banking stage took too long to process transactions");
                }
                sleep(Duration::from_millis(10));
            }
            exit.store(true, Ordering::Relaxed);
            banking_stage.join().unwrap();
            poh_service.join().unwrap();
            entry_receiver
        };
        drop(non_vote_sender);
        drop(tpu_vote_sender);
        drop(gossip_vote_sender);

        // consume the entire entry_receiver, feed it into a new bank
        // check that the balance is what we expect.
        let entries: Vec<_> = entry_receiver
            .iter()
            .map(|(_bank, (entry, _tick_height))| entry)
            .collect();

        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        for entry in entries {
            let _ = bank
                .try_process_entry_transactions(entry.transactions)
                .expect("All transactions should be processed");
        }

        // Assert the user doesn't hold three lamports. If the stage only outputs one
        // entry, then one of the transactions will be rejected, because it drives
        // the account balance below zero before the credit is added.
        assert!(bank.get_balance(&alice.pubkey()) != 3);
    }

    #[test]
    fn test_bank_record_transactions() {
        agave_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);

        let (record_sender, mut record_receiver) = record_channels(false);
        let recorder = TransactionRecorder::new(record_sender);
        record_receiver.restart(bank.bank_id());

        let pubkey = solana_pubkey::new_rand();
        let keypair2 = Keypair::new();
        let pubkey2 = solana_pubkey::new_rand();

        let txs = vec![
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()).into(),
            system_transaction::transfer(&keypair2, &pubkey2, 1, genesis_config.hash()).into(),
        ];

        let summary = recorder.record_transactions(bank.bank_id(), txs.clone());
        assert!(summary.result.is_ok());
        assert_eq!(
            record_receiver.try_recv().unwrap().transaction_batches,
            vec![txs.clone()]
        );
        assert!(record_receiver.try_recv().is_err());

        // Once bank is set to a new bank (setting bank id + 1 in record_transactions),
        // record_transactions should throw MaxHeightReached
        let next_bank_id = bank.bank_id() + 1;
        let RecordTransactionsSummary { result, .. } =
            recorder.record_transactions(next_bank_id, txs);
        assert_matches!(result, Err(PohRecorderError::MaxHeightReached));
        // Should receive nothing from PohRecorder b/c record failed
        assert!(record_receiver.try_recv().is_err());
    }

    pub(crate) fn create_slow_genesis_config(lamports: u64) -> GenesisConfigInfo {
        create_slow_genesis_config_with_leader(lamports, &solana_pubkey::new_rand())
    }

    pub(crate) fn create_slow_genesis_config_with_leader(
        lamports: u64,
        validator_pubkey: &Pubkey,
    ) -> GenesisConfigInfo {
        let mut config_info = create_genesis_config_with_leader(
            lamports,
            validator_pubkey,
            // See solana_ledger::genesis_utils::create_genesis_config.
            bootstrap_validator_stake_lamports(),
        );

        // For these tests there's only 1 slot, don't want to run out of ticks
        config_info.genesis_config.ticks_per_slot *= 1024;
        config_info
    }

    #[test]
    fn test_vote_storage_full_send() {
        agave_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10000);
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let start_hash = bank.last_blockhash();
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer.create_channels(false);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger"),
        );
        let (
            exit,
            poh_recorder,
            _poh_controller,
            transaction_recorder,
            poh_service,
            _entry_receiver,
        ) = create_test_recorder(bank.clone(), blockstore, None, None);
        let (replay_vote_sender, _replay_vote_receiver) = unbounded();

        let banking_stage = BankingStage::new_num_threads(
            BlockProductionMethod::CentralScheduler,
            poh_recorder.clone(),
            transaction_recorder,
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_vote_receiver,
            mpsc::channel(1).1,
            DEFAULT_NUM_WORKERS,
            SchedulerConfig {
                scheduler_pacing: SchedulerPacing::Disabled,
            },
            None,
            replay_vote_sender,
            None,
            bank_forks,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );

        let keypairs = (0..100).map(|_| Keypair::new()).collect_vec();
        let vote_keypairs = (0..100).map(|_| Keypair::new()).collect_vec();
        for keypair in keypairs.iter() {
            bank.process_transaction(&system_transaction::transfer(
                &mint_keypair,
                &keypair.pubkey(),
                20,
                start_hash,
            ))
            .unwrap();
        }

        // Send a bunch of votes and transfers
        let tpu_votes = (0..100_usize)
            .map(|i| {
                new_tower_sync_transaction(
                    TowerSync::from(vec![(0, 8), (1, 7), (i as u64 + 10, 6), (i as u64 + 11, 1)]),
                    Hash::new_unique(),
                    &keypairs[i],
                    &vote_keypairs[i],
                    &vote_keypairs[i],
                    None,
                )
            })
            .collect_vec();
        let gossip_votes = (0..100_usize)
            .map(|i| {
                new_tower_sync_transaction(
                    TowerSync::from(vec![(0, 9), (1, 8), (i as u64 + 5, 6), (i as u64 + 63, 1)]),
                    Hash::new_unique(),
                    &keypairs[i],
                    &vote_keypairs[i],
                    &vote_keypairs[i],
                    None,
                )
            })
            .collect_vec();
        let txs = (0..100_usize)
            .map(|i| {
                system_transaction::transfer(
                    &keypairs[i],
                    &keypairs[(i + 1) % 100].pubkey(),
                    10,
                    start_hash,
                );
            })
            .collect_vec();

        let non_vote_packet_batches = to_packet_batches(&txs, 10);
        let tpu_packet_batches = to_packet_batches(&tpu_votes, 10);
        let gossip_packet_batches = to_packet_batches(&gossip_votes, 10);

        // Send em all
        [
            (non_vote_packet_batches, non_vote_sender),
            (tpu_packet_batches, tpu_vote_sender),
            (gossip_packet_batches, gossip_vote_sender),
        ]
        .into_iter()
        .map(|(packet_batches, sender)| {
            Builder::new()
                .spawn(move || {
                    sender
                        .send(BankingPacketBatch::new(packet_batches))
                        .unwrap()
                })
                .unwrap()
        })
        .for_each(|handle| handle.join().unwrap());

        banking_stage.join().unwrap();
        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }
}
