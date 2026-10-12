//! The `banking_stage` processes Transaction messages. It is intended to be used
//! to construct a software pipeline.

#[cfg(test)]
use std::time::Duration;
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
mod execute_and_commit_timings;
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

pub struct BankingStage {
    banking_shutdown_signal: CancellationToken,
    worker_exit_signal: Arc<AtomicBool>,
    banking_control_receiver: mpsc::Receiver<BankingControlMsg>,
    transaction_receiver: BankingPacketReceiver,
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
        transaction_receiver: BankingPacketReceiver,
        banking_control_receiver: mpsc::Receiver<BankingControlMsg>,
        num_workers: NonZeroUsize,
        scheduler_config: SchedulerConfig,
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
            banking_control_receiver,
            transaction_receiver,
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
            self.filter_keys.clone(),
        );
        let receive_and_buffer = TransactionViewReceiveAndBuffer::new(
            self.transaction_receiver.clone(),
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
        let decision_maker = DecisionMaker::new(
            self.poh_recorder.read().unwrap().shared_leader_state(),
            self.alpenglow_slot_clock.clone(),
        );
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
        agave_scheduler_handshake::{AgaveCheckWorkerSession, AgaveSession, AgaveWorkerSession},
        tpu_to_pack::BankingPacketReceivers,
    };

    impl BankingStage {
        pub(super) fn spawn_external(
            &self,
            AgaveSession {
                flags: _,
                tpu_to_pack,
                progress_tracker,
                check_workers,
                workers,
            }: AgaveSession,
        ) -> Result<Vec<JoinHandle<()>>, ()> {
            info!("Spawning external scheduler");

            static_assertions::const_assert!(
                agave_scheduler_handshake::MAX_WORKERS == BankingStage::max_num_workers().get()
            );
            assert!(workers.len() <= BankingStage::max_num_workers().get());

            // Spawn the external consumer workers.
            let mut threads = Vec::with_capacity(workers.len() + check_workers.len() + 2);
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
                        self.log_messages_bytes_limit,
                    ),
                    worker_to_pack,
                    allocator,
                    self.poh_recorder.read().unwrap().shared_leader_state(),
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
                packet_receiver: self.transaction_receiver.clone(),
            };
            threads.push(tpu_to_pack::spawn(
                self.worker_exit_signal.clone(),
                self.banking_shutdown_signal.clone(),
                tpu_to_pack_receivers,
                tpu_to_pack,
            ));

            // Spawn progress tracker.
            let shared_leader_state = {
                let poh = self.poh_recorder.read().unwrap();
                poh.shared_leader_state()
            };
            threads.push(progress_tracker::spawn(
                self.worker_exit_signal.clone(),
                progress_tracker,
                shared_leader_state,
                worker_metrics,
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
        crate::{
            banking_trace::{BankingTracer, Channels},
            validator::SchedulerPacing,
        },
        agave_banking_stage_ingress_types::{
            to_banking_packet_batch, to_single_banking_packet_batch,
        },
        crossbeam_channel::Receiver,
        solana_entry::{
            entry::{self, EntrySlice},
            recorder_message::RecorderMessage,
        },
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
            poh_recorder::{PohRecorder, PohRecorderError, WorkingBankMessage},
            record_channels::record_channels,
            transaction_recorder::{RecordTransactionsSummary, TransactionRecorder},
        },
        solana_poh_config::PohConfig,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::Bank, genesis_utils::bootstrap_validator_stake_lamports,
            installed_scheduler_pool::BankWithScheduler,
        },
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_system_transaction as system_transaction,
        solana_transaction::{Transaction, sanitized::SanitizedTransaction},
        std::{
            sync::atomic::Ordering,
            thread::{JoinHandle, sleep},
            time::Instant,
        },
    };

    #[allow(clippy::type_complexity)]
    fn create_test_recorder(
        bank: Arc<Bank>,
        blockstore: Arc<Blockstore>,
    ) -> (
        Arc<AtomicBool>,
        Arc<RwLock<PohRecorder>>,
        TransactionRecorder,
        JoinHandle<()>,
        Receiver<WorkingBankMessage>,
    ) {
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let exit = Arc::new(AtomicBool::new(false));
        let (mut poh_recorder, entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            blockstore,
            &leader_schedule_cache,
            &PohConfig::default(),
            exit.clone(),
        );
        poh_recorder.set_bank(BankWithScheduler::new_without_scheduler(bank.clone()));

        let (record_sender, mut record_receiver) = record_channels(false);
        record_receiver.restart(bank.bank_id());
        let transaction_recorder = TransactionRecorder::new(record_sender);
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));

        let service_exit = exit.clone();
        let service_poh_recorder = poh_recorder.clone();
        let record_service = Builder::new()
            .name("solTestRecord".to_owned())
            .spawn(move || {
                loop {
                    match record_receiver.recv_timeout(Duration::from_millis(10)) {
                        Ok(record) => service_poh_recorder
                            .write()
                            .unwrap()
                            .record(record.bank_id, record.mixin, record.transactions)
                            .unwrap(),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            if service_exit.load(Ordering::Relaxed)
                                && record_receiver.is_safe_to_restart()
                            {
                                break;
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    };
                }
            })
            .unwrap();

        (
            exit,
            poh_recorder,
            transaction_recorder,
            record_service,
            entry_receiver,
        )
    }

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
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            transaction_sender,
            transaction_receiver,
        } = banking_tracer.create_channels();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger"),
        );
        let (exit, poh_recorder, transaction_recorder, record_service, _entry_receiver) =
            create_test_recorder(bank, blockstore);

        let banking_stage = BankingStage::new_num_threads(
            BlockProductionMethod::CentralSchedulerGreedy,
            poh_recorder,
            transaction_recorder,
            transaction_receiver,
            mpsc::channel(1).1,
            DEFAULT_NUM_WORKERS,
            SchedulerConfig {
                scheduler_pacing: SchedulerPacing::Disabled,
            },
            None,
            None,
            bank_forks,
            SharedAlpenglowSlotClock::default(),
            None,
            Arc::default(),
            Arc::new(SchedulerPriorityFloor::new()),
        );
        drop(transaction_sender);
        banking_stage.join().unwrap();
        exit.store(true, Ordering::Relaxed);
        record_service.join().unwrap();
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
        let banking_tracer = BankingTracer::new_disabled();
        let Channels {
            transaction_sender,
            transaction_receiver,
        } = banking_tracer.create_channels();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger"),
        );
        let (exit, poh_recorder, transaction_recorder, record_service, entry_receiver) =
            create_test_recorder(bank, blockstore);

        let banking_stage = BankingStage::new_num_threads(
            BlockProductionMethod::CentralSchedulerGreedy,
            poh_recorder.clone(),
            transaction_recorder,
            transaction_receiver,
            mpsc::channel(1).1,
            DEFAULT_NUM_WORKERS,
            SchedulerConfig {
                scheduler_pacing: SchedulerPacing::Disabled,
            },
            None,
            None,
            bank_forks, // keep a local-copy of bank-forks so worker threads do not lose weak access to bank-forks
            SharedAlpenglowSlotClock::default(),
            None,
            Arc::default(),
            Arc::new(SchedulerPriorityFloor::new()),
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
        let mut packet_batch = to_banking_packet_batch(&[tx_no_ver, tx_anf, tx]);
        Arc::make_mut(&mut packet_batch)
            .first_mut()
            .unwrap()
            .meta_mut()
            .set_discard(true); // set discard on `tx_no_ver`

        // glad they all fit
        assert_eq!(packet_batch.len(), 3);

        transaction_sender // no_ver, anf, tx
            .send(packet_batch)
            .unwrap();

        // capture the entry receiver until we've received all our entries.
        let mut entries = Vec::with_capacity(100);
        loop {
            if let Ok((_bank, (RecorderMessage::Entry(entry), _))) = entry_receiver.try_recv() {
                let tx_entry = !entry.transactions.is_empty();
                entries.push(entry);
                if tx_entry {
                    break; // once we have the entry break. don't expect more than one.
                }
            }
            sleep(Duration::from_millis(10));
        }

        drop(transaction_sender);
        banking_stage.join().unwrap();

        exit.store(true, Ordering::Relaxed);
        record_service.join().unwrap();
        drop(poh_recorder);

        let blockhash = start_hash;
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        // receive entries + ticks. The sender has been dropped, so there
        // are no more entries that will ever come in after the `iter` here.
        entries.extend(
            entry_receiver
                .iter()
                .map(|(_bank, (message, _tick_height))| message.unwrap_entry()),
        );

        assert!(
            entries
                .verify(&blockhash, &entry::thread_pool_for_tests())
                .status()
        );
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
            transaction_sender,
            transaction_receiver,
        } = banking_tracer.create_channels();

        // Process a batch that includes a transaction that receives two lamports.
        let alice = Keypair::new();
        let tx =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 2, genesis_config.hash());

        let packet_batches = to_single_banking_packet_batch(&tx);
        transaction_sender.send(packet_batches).unwrap();

        // Process a second batch that uses the same from account, so conflicts with above TX
        let tx =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 1, genesis_config.hash());
        let packet_batches = to_single_banking_packet_batch(&tx);
        transaction_sender.send(packet_batches).unwrap();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(
            Blockstore::open(ledger_path.path())
                .expect("Expected to be able to open database ledger"),
        );

        let entry_receiver = {
            // start a banking_stage to eat verified receiver
            let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
            let (exit, poh_recorder, transaction_recorder, record_service, entry_receiver) =
                create_test_recorder(bank.clone(), blockstore);
            let banking_stage = BankingStage::new_num_threads(
                BlockProductionMethod::CentralSchedulerGreedy,
                poh_recorder,
                transaction_recorder,
                transaction_receiver,
                mpsc::channel(1).1,
                DEFAULT_NUM_WORKERS,
                SchedulerConfig {
                    scheduler_pacing: SchedulerPacing::Disabled,
                },
                None,
                None,
                bank_forks,
                SharedAlpenglowSlotClock::default(),
                None,
                Arc::default(),
                Arc::new(SchedulerPriorityFloor::new()),
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
            banking_stage.join().unwrap();
            exit.store(true, Ordering::Relaxed);
            record_service.join().unwrap();
            entry_receiver
        };
        drop(transaction_sender);

        // consume the entire entry_receiver, feed it into a new bank
        // check that the balance is what we expect.
        let entries: Vec<_> = entry_receiver
            .iter()
            .map(|(_bank, (message, _tick_height))| message.unwrap_entry())
            .collect();

        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
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
