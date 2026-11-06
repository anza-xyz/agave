//! The `banking_stage` processes Transaction messages. It is intended to be used
//! to construct a software pipeline.

use {
    self::{committer::Committer, consumer::Consumer, decision_maker::DecisionMaker},
    crate::{
        banking_stage::{
            consume_worker::ConsumeWorker,
            transaction_scheduler::{
                scheduler_controller::{
                    DEFAULT_SCHEDULER_PACING_FILL_TIME_MILLIS, SchedulerConfig, SchedulerController,
                },
                scheduler_error::SchedulerError,
            },
        },
        validator::BlockProductionMethod,
    },
    agave_banking_stage_ingress_types::{BankingPacketReceiver, SchedulerPriorityFloor},
    agave_jemalloc::{
        arena_for_worker,
        group::ArenaGroup,
        jemalloc::{Arena, DisableThreadTcacheOnDrop},
    },
    agave_votor::slot_clock::SharedAlpenglowSlotClock,
    crossbeam_channel::{Receiver, Sender, bounded},
    futures::{StreamExt, stream::FuturesUnordered},
    solana_perf::packet::{BytesPacket, bytes::Bytes},
    solana_poh::{poh_recorder::PohRecorder, transaction_recorder::TransactionRecorder},
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank_forks::BankForks, prioritization_fee_cache::PrioritizationFeeCache,
        transaction_execution::TransactionStatusSender,
    },
    std::{
        collections::HashSet,
        num::{NonZeroU64, NonZeroUsize},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
    },
    tokio::sync::mpsc,
    tokio_util::sync::CancellationToken,
    transaction_scheduler::{
        check_worker::spawn_check_workers,
        greedy_scheduler::{GreedyScheduler, GreedySchedulerConfig},
        receive_and_buffer::TransactionViewReceiveAndBuffer,
    },
};

pub mod transaction_scheduler;

mod alpenglow_timing;
mod committer;
mod consume_worker;
mod consumer;
mod decision_maker;
mod leader_slot_timing_metrics;
mod qos_service;
mod scheduler_messages;

#[cfg(unix)]
mod progress_tracker;
#[cfg(unix)]
mod tpu_to_pack;

/// The maximum number of worker threads that can be spawned by banking stage.
/// 64 because `ThreadAwareAccountLocks` uses a `u64` as a bitmask to
/// track thread placement.
const MAX_NUM_WORKERS: NonZeroUsize = NonZeroUsize::new(64).unwrap();
const DEFAULT_NUM_WORKERS: NonZeroUsize = NonZeroUsize::new(4).unwrap();

const TOTAL_BUFFERED_PACKETS: usize = 100_000;

/// Returns the packet's payload, or None if the packet is discarded.
fn packet_bytes(packet: &BytesPacket) -> Option<Bytes> {
    (!packet.meta().discard()).then(|| packet.buffer().clone())
}

fn bind_arena(assigned_arena: Option<Arena>) -> Option<DisableThreadTcacheOnDrop> {
    assigned_arena.map(|arena| {
        arena
            .bind_current_thread_permanently()
            .expect("failed to bind banking worker thread to jemalloc arena");

        DisableThreadTcacheOnDrop::new(arena)
    })
}

pub struct BankingStage {
    banking_shutdown_signal: CancellationToken,
    worker_exit_signal: Arc<AtomicBool>,
    worker_thread_arenas: Option<Arc<ArenaGroup>>,
    banking_control_receiver: mpsc::Receiver<BankingControlMsg>,
    non_vote_receiver: BankingPacketReceiver,
    transaction_recorder: TransactionRecorder,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    bank_forks: Arc<RwLock<BankForks>>,
    #[cfg_attr(not(unix), allow(dead_code))]
    alpenglow_slot_clock: SharedAlpenglowSlotClock,
    committer: Committer,
    log_messages_bytes_limit: Option<usize>,
    filter_keys: Arc<HashSet<Pubkey>>,
    priority_floor: Arc<SchedulerPriorityFloor>,
    threads: FuturesUnordered<NamedTask<std::thread::Result<()>>>,
}

impl BankingStage {
    #[allow(clippy::too_many_arguments)]
    pub fn new_num_threads(
        block_production_method: BlockProductionMethod,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        transaction_recorder: TransactionRecorder,
        non_vote_receiver: BankingPacketReceiver,
        banking_control_receiver: mpsc::Receiver<BankingControlMsg>,
        num_workers: NonZeroUsize,
        scheduler_config: SchedulerConfig,
        worker_thread_arenas: Option<Arc<ArenaGroup>>,
        transaction_status_sender: Option<TransactionStatusSender>,
        log_messages_bytes_limit: Option<usize>,
        bank_forks: Arc<RwLock<BankForks>>,
        alpenglow_slot_clock: SharedAlpenglowSlotClock,
        prioritization_fee_cache: Option<Arc<PrioritizationFeeCache>>,
        filter_keys: Arc<HashSet<Pubkey>>,
        priority_floor: Arc<SchedulerPriorityFloor>,
    ) -> BankingStageHandle {
        let committer = Committer::new(transaction_status_sender, prioritization_fee_cache);

        // Setup the manager thread state.
        let banking_shutdown_signal = CancellationToken::new();
        let manager = BankingStage {
            banking_shutdown_signal: banking_shutdown_signal.clone(),
            worker_exit_signal: Arc::new(AtomicBool::new(false)),
            worker_thread_arenas,
            banking_control_receiver,
            non_vote_receiver,
            transaction_recorder,
            poh_recorder,
            bank_forks,
            alpenglow_slot_clock,
            committer,
            log_messages_bytes_limit,
            filter_keys,
            priority_floor,
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
        self.spawn_scheduler(initial_args).unwrap();

        loop {
            tokio::select! {
                biased;

                _ = self.banking_shutdown_signal.cancelled() => break,
                Some(args) = self.banking_control_receiver.recv() => {
                    if self.cycle_threads(args).await.is_err() {
                        self.cycle_threads_fallback().await.unwrap();
                    }
                },
                Some((name, res)) = self.threads.next() => {
                    match res.unwrap() {
                        Ok(()) => error!("Banking worker exited unexpectedly; name={name}"),
                        Err(err) => error!("Banking worker exited with error; name={name}; err={err:?}"),
                    };
                    self.cycle_threads_fallback().await.unwrap();
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

    async fn cycle_threads(&mut self, args: BankingControlMsg) -> Result<(), ()> {
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
        self.spawn_scheduler(args)
    }

    async fn cycle_threads_fallback(&mut self) -> Result<(), ()> {
        error!("Spawning the default block production method as a fallback...");

        self.cycle_threads(BankingControlMsg::Internal {
            block_production_method: BlockProductionMethod::default(),
            num_workers: BankingStage::default_num_workers(),
            config: SchedulerConfig::default(),
        })
        .await
    }

    fn spawn_scheduler(&mut self, args: BankingControlMsg) -> Result<(), ()> {
        let threads = (match args {
            BankingControlMsg::Internal {
                block_production_method,
                num_workers,
                config,
            } => match block_production_method {
                BlockProductionMethod::CentralScheduler
                | BlockProductionMethod::CentralSchedulerGreedy => {
                    self.spawn_internal_central(num_workers, config)
                }
            },
            #[cfg(unix)]
            BankingControlMsg::External { session } => self.spawn_external(session),
        })?;

        self.threads.extend(threads.into_iter().map(|handle| {
            let name = handle.thread().name().unwrap().to_string();

            NamedTask::new(tokio::task::spawn_blocking(|| handle.join()), name)
        }));

        info!("Scheduler spawned");
        Ok(())
    }

    fn spawn_internal_central(
        &self,
        num_workers: NonZeroUsize,
        scheduler_config: SchedulerConfig,
    ) -> Result<Vec<JoinHandle<()>>, ()> {
        info!("Spawning internal central scheduler");

        assert!(num_workers <= BankingStage::max_num_workers());
        let num_workers = num_workers.get();
        const NUM_CHECK_WORKERS: NonZeroUsize = NonZeroUsize::new(8).unwrap();
        const CHANNEL_CAPACITY: usize = 10_000;

        let exit = self.worker_exit_signal.clone();

        // Setup receive & buffer.
        let sharable_banks = self.bank_forks.read().unwrap().sharable_banks();
        let priority_floor = self.priority_floor.clone();
        let (check_work_sender, check_work_receiver) = bounded(CHANNEL_CAPACITY);
        let (check_result_sender, check_result_receiver) = bounded(CHANNEL_CAPACITY);
        let check_worker_handles = spawn_check_workers(
            NUM_CHECK_WORKERS,
            check_work_receiver,
            check_result_sender,
            sharable_banks.clone(),
            self.poh_recorder.read().unwrap().shared_leader_state(),
            self.filter_keys.clone(),
        );
        let receive_and_buffer = TransactionViewReceiveAndBuffer::new(
            self.non_vote_receiver.clone(),
            check_work_sender,
            check_result_receiver,
        );

        let mut threads = Vec::with_capacity(num_workers + NUM_CHECK_WORKERS.get() + 1);
        threads.extend(check_worker_handles);

        // Create channels for communication between scheduler and workers
        let (work_senders, work_receivers): (Vec<Sender<_>>, Vec<Receiver<_>>) =
            (0..num_workers).map(|_| bounded(CHANNEL_CAPACITY)).unzip();
        let (finished_work_sender, finished_work_receiver) =
            bounded(num_workers.saturating_mul(CHANNEL_CAPACITY));

        // Spawn the worker threads
        let migration_status = self.bank_forks.read().unwrap().migration_status();
        let decision_maker = DecisionMaker::new(
            self.poh_recorder.read().unwrap().shared_leader_state(),
            migration_status,
            self.alpenglow_slot_clock.clone(),
        );
        let mut worker_metrics = Vec::with_capacity(num_workers);
        for (index, work_receiver) in work_receivers.into_iter().enumerate() {
            let id = index as u32;
            let assigned_arena = self
                .worker_thread_arenas
                .as_ref()
                .map(|arenas| arena_for_worker(arenas, index));
            let consume_worker = ConsumeWorker::new(
                exit.clone(),
                work_receiver,
                Consumer::new(
                    self.committer.clone(),
                    self.transaction_recorder.clone(),
                    self.log_messages_bytes_limit,
                ),
                finished_work_sender.clone(),
                self.poh_recorder.read().unwrap().shared_leader_state(),
            );

            worker_metrics.push(consume_worker.metrics_handle());
            threads.push(
                Builder::new()
                    .name(format!("solCoWorker{id:02}"))
                    .spawn(move || {
                        let _disable_tcache_on_exit = bind_arena(assigned_arena);

                        if let Err(err) = consume_worker.run() {
                            error!("Internal consume worker error; err={err}");
                        }
                    })
                    .unwrap(),
            )
        }

        // Both block production methods currently route to the greedy scheduler.
        let scheduler = GreedyScheduler::new(
            work_senders,
            finished_work_receiver,
            GreedySchedulerConfig::default(),
        );
        let exit = exit.clone();
        let shutdown_signal = self.banking_shutdown_signal.clone();
        threads.push(
            Builder::new()
                .name("solBnkTxSched".to_string())
                .spawn(move || {
                    let mut scheduler_controller = SchedulerController::new(
                        exit,
                        scheduler_config,
                        decision_maker,
                        receive_and_buffer,
                        sharable_banks,
                        scheduler,
                        worker_metrics,
                        priority_floor,
                    );

                    match scheduler_controller.run() {
                        Ok(_) => info!("Scheduler exiting without error"),
                        Err(SchedulerError::DisconnectedRecvChannel(_)) => {
                            info!("Upstream disconnected, shutting down banking");

                            // NB: We must signal shutdown before dropping the scheduler
                            //     controller, else, the workers may exit with an error and
                            //     trigger a new spawn before we have a chance to issue the
                            //     cancel.
                            shutdown_signal.cancel();
                            drop(scheduler_controller);
                        }
                        Err(SchedulerError::DisconnectedSendChannel(_)) => {
                            warn!("Unexpected worker disconnect from scheduler")
                        }
                    }
                })
                .unwrap(),
        );

        Ok(threads)
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
        crate::banking_stage::{
            consume_worker::external::ExternalWorker,
            transaction_scheduler::check_worker::external::ExternalCheckWorker,
        },
        agave_scheduler_handshake::{
            AgaveCheckWorkerSession, AgaveSession, AgaveSimulateWorkerSession, AgaveWorkerSession,
        },
        tpu_to_pack::BankingPacketReceivers,
    };

    impl BankingStage {
        pub(super) fn spawn_external(
            &self,
            AgaveSession {
                tpu_to_pack,
                progress_tracker,
                check_workers,
                workers,
                simulate_workers,
            }: AgaveSession,
        ) -> Result<Vec<JoinHandle<()>>, ()> {
            info!("Spawning external scheduler");

            static_assertions::const_assert!(
                agave_scheduler_handshake::MAX_WORKERS == BankingStage::max_num_workers().get()
            );
            assert!(workers.len() <= BankingStage::max_num_workers().get());

            // Spawn the external consumer workers.
            let mut threads = Vec::with_capacity(
                workers.len() + simulate_workers.len() + check_workers.len() + 2,
            );
            let mut worker_metrics = Vec::with_capacity(workers.len() + simulate_workers.len());
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
                let assigned_arena = self
                    .worker_thread_arenas
                    .as_ref()
                    .map(|arenas| arena_for_worker(arenas, index));
                let consume_worker = ExternalWorker::new(
                    self.worker_exit_signal.clone(),
                    Consumer::new(
                        self.committer.clone(),
                        self.transaction_recorder.clone(),
                        self.log_messages_bytes_limit,
                    ),
                    allocator,
                    self.poh_recorder.read().unwrap().shared_leader_state(),
                    self.bank_forks.read().unwrap().sharable_banks(),
                );

                worker_metrics.push(consume_worker.metrics_handle());
                threads.push(
                    Builder::new()
                        .name(format!("solECoWorker{id:02}"))
                        .spawn(move || {
                            let _disable_tcache_on_exit = bind_arena(assigned_arena);

                            if let Err(err) =
                                consume_worker.run_execution_worker(pack_to_worker, worker_to_pack)
                            {
                                error!("External consume worker error; err={err}");
                            }
                        })
                        .unwrap(),
                );
            }

            for (
                index,
                AgaveSimulateWorkerSession {
                    allocator,
                    pack_to_simulate_worker,
                    simulate_worker_to_pack,
                },
            ) in simulate_workers.into_iter().enumerate()
            {
                let id = index as u32;
                let consume_worker = ExternalWorker::new(
                    self.worker_exit_signal.clone(),
                    Consumer::new(
                        self.committer.clone(),
                        self.transaction_recorder.clone(),
                        self.log_messages_bytes_limit,
                    ),
                    allocator,
                    self.poh_recorder.read().unwrap().shared_leader_state(),
                    self.bank_forks.read().unwrap().sharable_banks(),
                );

                worker_metrics.push(consume_worker.metrics_handle());
                threads.push(
                    Builder::new()
                        .name(format!("solESimWorker{id:02}"))
                        .spawn(move || {
                            if let Err(err) = consume_worker.run_simulation_worker(
                                pack_to_simulate_worker,
                                simulate_worker_to_pack,
                            ) {
                                error!("External simulation worker error; err={err}");
                            }
                        })
                        .unwrap(),
                );
            }

            for (
                index,
                AgaveCheckWorkerSession {
                    allocator,
                    pack_to_check_worker,
                    check_worker_to_pack,
                },
            ) in check_workers.into_iter().enumerate()
            {
                let id = index as u32;
                let check_worker = ExternalCheckWorker::new(
                    self.worker_exit_signal.clone(),
                    pack_to_check_worker,
                    check_worker_to_pack,
                    allocator,
                    self.poh_recorder.read().unwrap().shared_leader_state(),
                    self.bank_forks.read().unwrap().sharable_banks(),
                );

                threads.push(
                    Builder::new()
                        .name(format!("solEChWorker{id:02}"))
                        .spawn(move || {
                            if let Err(err) = check_worker.run() {
                                error!("External check worker error; err={err}");
                            }
                        })
                        .unwrap(),
                );
            }

            // Spawn tpu to pack.
            let tpu_to_pack_receivers = BankingPacketReceivers {
                packet_receiver: self.non_vote_receiver.clone(),
            };
            threads.push(tpu_to_pack::spawn(
                self.worker_exit_signal.clone(),
                self.banking_shutdown_signal.clone(),
                tpu_to_pack_receivers,
                tpu_to_pack,
            ));

            // Spawn progress tracker.
            let (shared_leader_state, ticks_per_slot) = {
                let poh = self.poh_recorder.read().unwrap();

                (poh.shared_leader_state(), poh.ticks_per_slot())
            };
            let migration_status = self.bank_forks.read().unwrap().migration_status();
            threads.push(progress_tracker::spawn(
                self.worker_exit_signal.clone(),
                progress_tracker,
                shared_leader_state,
                worker_metrics,
                ticks_per_slot,
                migration_status,
                self.alpenglow_slot_clock.clone(),
            ));

            Ok(threads)
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
        session: agave_scheduler_handshake::AgaveSession,
    },
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
        crate::{banking_channels::Channels, validator::SchedulerPacing},
        agave_banking_stage_ingress_types::{
            to_banking_packet_batch, to_single_banking_packet_batch,
        },
        solana_entry::entry::{self, EntrySlice},
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::Blockstore,
            genesis_utils::{
                GenesisConfigInfo, create_genesis_config, create_genesis_config_with_leader,
            },
            get_tmp_ledger_path_auto_delete,
            leader_schedule_cache::LeaderScheduleCache,
        },
        solana_poh::{
            poh_recorder::{PohRecorderError, WorkingBankMessage},
            record_channels::{RecordReceiver, record_channels},
            transaction_recorder::RecordTransactionsSummary,
        },
        solana_poh_config::PohConfig,
        solana_pubkey::Pubkey,
        solana_runtime::{bank::Bank, genesis_utils::bootstrap_validator_stake_lamports},
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_system_transaction as system_transaction,
        solana_transaction::{Transaction, sanitized::SanitizedTransaction},
        std::{
            assert_matches,
            thread::sleep,
            time::{Duration, Instant},
        },
    };

    pub(crate) fn sanitize_transactions(
        txs: Vec<Transaction>,
    ) -> Vec<RuntimeTransaction<SanitizedTransaction>> {
        txs.into_iter()
            .map(RuntimeTransaction::from_transaction_for_tests)
            .collect()
    }

    #[allow(clippy::type_complexity)]
    fn create_test_recorder_without_service(
        bank: Arc<Bank>,
        blockstore: Arc<Blockstore>,
        poh_config: &PohConfig,
    ) -> (
        Arc<RwLock<PohRecorder>>,
        TransactionRecorder,
        RecordReceiver,
        Receiver<WorkingBankMessage>,
    ) {
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let (mut poh_recorder, entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            blockstore,
            &leader_schedule_cache,
            poh_config,
            Arc::default(),
        );
        poh_recorder.set_bank_for_test(bank.clone());

        let (record_sender, mut record_receiver) = record_channels(false);
        record_receiver.restart(bank.bank_id());

        (
            Arc::new(RwLock::new(poh_recorder)),
            TransactionRecorder::new(record_sender),
            record_receiver,
            entry_receiver,
        )
    }

    fn wait_for_condition(mut condition: impl FnMut() -> bool, message: &str) {
        const TIMEOUT: Duration = Duration::from_secs(10);
        let start = Instant::now();
        while !condition() {
            assert!(start.elapsed() <= TIMEOUT, "{message}");
            sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn test_banking_stage_shutdown1() {
        let genesis_config = create_genesis_config(2).genesis_config;
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            ..
        } = Channels::new();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger"),
        );
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let (poh_recorder, _entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            blockstore,
            &leader_schedule_cache,
            &genesis_config.poh_config,
            Arc::default(),
        );
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));
        let (record_sender, _record_receiver) = record_channels(false);
        let transaction_recorder = TransactionRecorder::new(record_sender);

        let banking_stage = BankingStage::new_num_threads(
            BlockProductionMethod::CentralSchedulerGreedy,
            poh_recorder,
            transaction_recorder,
            non_vote_receiver,
            mpsc::channel(1).1,
            DEFAULT_NUM_WORKERS,
            SchedulerConfig {
                scheduler_pacing: SchedulerPacing::Disabled,
            },
            None,
            None,
            None,
            bank_forks,
            SharedAlpenglowSlotClock::default(),
            None,
            Arc::default(),
            Arc::new(SchedulerPriorityFloor::new()),
        );
        drop(non_vote_sender);
        banking_stage.join().unwrap();
    }

    #[test]
    fn test_banking_stage_entries_only_central_scheduler() {
        agave_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10);
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let start_hash = bank.last_blockhash();
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            ..
        } = Channels::new();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger"),
        );
        let (poh_recorder, transaction_recorder, mut record_receiver, entry_receiver) =
            create_test_recorder_without_service(
                bank.clone(),
                blockstore,
                &genesis_config.poh_config,
            );

        let banking_stage = BankingStage::new_num_threads(
            BlockProductionMethod::CentralSchedulerGreedy,
            poh_recorder.clone(),
            transaction_recorder,
            non_vote_receiver,
            mpsc::channel(1).1,
            DEFAULT_NUM_WORKERS,
            SchedulerConfig {
                scheduler_pacing: SchedulerPacing::Disabled,
            },
            None,
            None,
            None,
            bank_forks,
            SharedAlpenglowSlotClock::default(),
            None,
            Arc::default(),
            Arc::new(SchedulerPriorityFloor::new()),
        );

        // This packet is valid, but explicitly discarded before BankingStage sees it.
        let discarded_recipient = Pubkey::new_unique();
        let discarded_tx =
            system_transaction::transfer(&mint_keypair, &discarded_recipient, 2, start_hash);

        // This is the only valid transaction that should be recorded.
        let valid_recipient = Pubkey::new_unique();
        let valid_tx = system_transaction::transfer(&mint_keypair, &valid_recipient, 1, start_hash);

        // This transaction fails account loading because its source account does not exist.
        let missing_source = Keypair::new();
        let missing_recipient = Pubkey::new_unique();
        let account_not_found_tx =
            system_transaction::transfer(&missing_source, &missing_recipient, 1, start_hash);

        let mut packet_batch =
            to_banking_packet_batch(&[discarded_tx, account_not_found_tx, valid_tx]);
        Arc::make_mut(&mut packet_batch)
            .first_mut()
            .unwrap()
            .meta_mut()
            .set_discard(true);
        non_vote_sender.send(packet_batch).unwrap();

        wait_for_condition(
            || bank.get_balance(&valid_recipient) == 1,
            "banking stage took too long to process the valid transaction",
        );

        drop(non_vote_sender);
        banking_stage.join().unwrap();

        // BlockCreationLoop owns this handoff in production. Drain it synchronously here so the
        // test exercises the same Record -> Entry conversion without recreating PohService.
        record_receiver.shutdown();
        let records: Vec<_> = record_receiver.drain_after_shutdown().collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].transactions.len(), 1);
        for record in records {
            poh_recorder
                .write()
                .unwrap()
                .record(record.bank_id, record.mixin, record.transactions)
                .unwrap();
        }
        drop(poh_recorder);

        let entries: Vec<_> = entry_receiver
            .iter()
            .map(|(_bank, (message, _tick_height))| message.unwrap_entry())
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(
            entries
                .verify(&start_hash, &entry::thread_pool_for_tests())
                .status()
        );

        let replay_bank = Bank::new_for_tests(&genesis_config);
        for entry in entries {
            replay_bank
                .process_entry_transactions(entry.transactions)
                .iter()
                .for_each(|result| assert_eq!(*result, Ok(())));
        }
        assert_eq!(replay_bank.get_balance(&valid_recipient), 1);
        assert_eq!(replay_bank.get_balance(&discarded_recipient), 0);
    }

    #[test]
    fn test_banking_stage_entryfication() {
        agave_logger::setup();
        // Conflicting transactions must be placed in separate entries so replay observes the
        // same account state transitions as the producer.
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(2);
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            ..
        } = Channels::new();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger"),
        );
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let start_hash = bank.last_blockhash();
        let (poh_recorder, transaction_recorder, mut record_receiver, entry_receiver) =
            create_test_recorder_without_service(
                bank.clone(),
                blockstore,
                &genesis_config.poh_config,
            );
        let banking_stage = BankingStage::new_num_threads(
            BlockProductionMethod::CentralSchedulerGreedy,
            poh_recorder.clone(),
            transaction_recorder,
            non_vote_receiver,
            mpsc::channel(1).1,
            DEFAULT_NUM_WORKERS,
            SchedulerConfig {
                scheduler_pacing: SchedulerPacing::Disabled,
            },
            None,
            None,
            None,
            bank_forks,
            SharedAlpenglowSlotClock::default(),
            None,
            Arc::default(),
            Arc::new(SchedulerPriorityFloor::new()),
        );

        let alice = Keypair::new();
        let fund_alice =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 2, genesis_config.hash());
        non_vote_sender
            .send(to_single_banking_packet_batch(&fund_alice))
            .unwrap();
        wait_for_condition(
            || bank.get_balance(&alice.pubkey()) == 2,
            "banking stage took too long to fund the dependent account",
        );

        // The second transaction depends on the first transaction creating and funding Alice's
        // account. If both transactions were placed in one entry, parallel replay could evaluate
        // this spend before the funding transaction's state was visible.
        let spend_from_alice =
            system_transaction::transfer(&alice, &mint_keypair.pubkey(), 1, genesis_config.hash());
        non_vote_sender
            .send(to_single_banking_packet_batch(&spend_from_alice))
            .unwrap();
        wait_for_condition(
            || bank.get_balance(&alice.pubkey()) == 1,
            "banking stage took too long to process the dependent transaction",
        );
        let producer_balance = bank.get_balance(&alice.pubkey());
        assert_eq!(producer_balance, 1);

        drop(non_vote_sender);
        banking_stage.join().unwrap();

        record_receiver.shutdown();
        let records: Vec<_> = record_receiver.drain_after_shutdown().collect();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| record.transactions.len() == 1));
        for record in records {
            poh_recorder
                .write()
                .unwrap()
                .record(record.bank_id, record.mixin, record.transactions)
                .unwrap();
        }
        drop(poh_recorder);

        let entries: Vec<_> = entry_receiver
            .iter()
            .map(|(_bank, (message, _tick_height))| message.unwrap_entry())
            .collect();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.transactions.len() == 1));
        assert!(
            entries
                .verify(&start_hash, &entry::thread_pool_for_tests())
                .status()
        );

        let replay_bank = Bank::new_for_tests(&genesis_config);
        for entry in entries {
            let _ = replay_bank
                .try_process_entry_transactions(entry.transactions)
                .expect("all transactions should be processed during replay");
        }
        assert_eq!(replay_bank.get_balance(&alice.pubkey()), producer_balance);
    }

    #[test]
    fn test_bank_record_transactions() {
        agave_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

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
            record_receiver.try_recv().unwrap().transactions,
            txs.clone()
        );
        assert!(record_receiver.try_recv().is_err());

        // Once bank is set to a new bank (next bank id in record_transactions),
        // record_transactions should throw MaxHeightReached
        let next_bank_id = bank.bank_id_generator().next();
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
}
