#![cfg(feature = "dev-context-only-utils")]
use {
    crate::{
        banking_stage::{
            transaction_scheduler::scheduler_controller::SchedulerConfig,
            unified_scheduler::ensure_banking_stage_setup,
            update_bank_forks_and_poh_recorder_for_new_tpu_bank, BankingStage, BankingStageHandle,
            LikeClusterInfo,
        },
        banking_trace::{
            BankingTracer, ChannelLabel, Channels, TimedTracedEvent, TracedEvent, TracedSender,
            TracerThread, BANKING_TRACE_DIR_DEFAULT_BYTE_LIMIT, BASENAME,
        },
        validator::BlockProductionMethod,
    },
    agave_banking_stage_ingress_types::BankingPacketBatch,
    assert_matches::assert_matches,
    bincode::deserialize_from,
    crossbeam_channel::{unbounded, Sender},
    itertools::Itertools,
    log::*,
    solana_clock::{Slot, DEFAULT_MS_PER_SLOT, HOLD_TRANSACTIONS_SLOT_OFFSET},
    solana_genesis_config::GenesisConfig,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfoQuery, node::Node},
    solana_keypair::Keypair,
    solana_ledger::{
        blockstore::{Blockstore, PurgeType},
        leader_schedule_cache::LeaderScheduleCache,
    },
    solana_net_utils::{
        sockets::{bind_in_range_with_config, SocketConfiguration},
        SocketAddrSpace,
    },
    solana_poh::{
        poh_controller::PohController,
        poh_recorder::{PohRecorder, GRACE_TICKS_FACTOR, MAX_GRACE_SLOTS},
        poh_service::{PohService, DEFAULT_HASHES_PER_BATCH, DEFAULT_PINNED_CPU_CORE},
        record_channels::record_channels,
        transaction_recorder::TransactionRecorder,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, HashOverrides},
        bank_forks::BankForks,
        installed_scheduler_pool::BankWithScheduler,
    },
    solana_shred_version::compute_shred_version,
    solana_signer::Signer,
    solana_turbine::broadcast_stage::{BroadcastStage, BroadcastStageType},
    solana_unified_scheduler_pool::DefaultSchedulerPool,
    std::{
        collections::BTreeMap,
        fmt::Display,
        fs::File,
        io::{self, BufRead, BufReader},
        net::{IpAddr, Ipv4Addr},
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{self, sleep, JoinHandle},
        time::{Duration, Instant, SystemTime},
    },
    thiserror::Error,
    tokio::sync::mpsc,
};

/// This creates a simulated environment around `BankingStage` to produce leader's blocks based on
/// recorded banking trace events (`TimedTracedEvent`).
///
/// At a high level, the task of `BankingStage` is to pack transactions into assigned,
/// fixed-duration, leader blocks. So, there are 3 abstract inputs to simulate: blocks, time, and
/// transactions.
///
/// In the context of simulation, the first two are simple; both are well defined.
///
/// For ancestor blocks, we first replay a certain number of blocks immediately up to target
/// simulation leader's slot with `halt_at_slot` mechanism. Ultimately freezing the ancestor block
/// with expected and deterministic hashes. This has the added possible benefit of warming caches
/// that may be used during simulation.
///
/// After replay, a minor tweak is applied during simulation: we forcibly override leader's hashes
/// as the simulated `BankingStage` creates them, using recorded `BlockAndBankHash` events. This is
/// to provide indistinguishable sysvars to TX execution and identical TX age resolution as the
/// simulation goes on. Otherwise, the vast majority of TX processing would differ because the
/// simulated block's hashes would differ than the recorded ones as block composition difference is
/// inevitable.
///
/// As in the real environment, for PoH time we use the `PohRecorder`. This is simply a 400ms
/// timer, external to `BankingStage` and thus mostly irrelevant to `BankingStage` performance. For
/// wall time, we use the first `BankStatus::BlockAndBankHash` and `SystemTime::now()` to define
/// T=0 for simulation. Then, simulation progress is timed accordingly. For context, this syncing
/// is necessary because all trace events are recorded in UTC, not relative to poh nor to leader
/// schedule for simplicity at recording.
///
/// Lastly, the last and most complicated input to simulate: transactions.
///
/// A closer look of the transaction load profile is below, regardless of internal banking
/// implementation and simulation:
///
/// Due to solana's general tx broadcast strategy of client's submission and optional node
/// forwarding, many transactions often arrive before the first leader slot begins. Thus, the
/// initial leader block creation typically starts with rather large number of schedule-able
/// transactions. Also, note that additional transactions arrive during the 4 leader slot window
/// (roughly ~1.6 seconds).
///
/// Simulation must mimic this load pattern while being agnostic to internal banking impl as much
/// as possible. For that agnostic objective, `TracedSender`s were introduced into the `SigVerify`
/// stage and gossip subsystem by `BankingTracer` to trace **all** `BankingPacketBatch`s' exact
/// payload and _sender_'s timing with `SystemTime::now()` for all `ChannelLabel`s. This deliberate
/// tracing placement is not to be affected by any `BankingStage`'s internal capacity (if any) nor
/// by its channel consumption pattern.
///
/// BankingSimulator consists of 2 phases chronologically: warm-up and on-the-fly. The 2 phases are
/// segregated by the aforementioned T=0.
///
/// Both phases just send `BankingPacketBatch` in the same fashion, pretending to be
/// `SigVerifyStage`/gossip from a single thread to busy loop for precise T=N at ~1us granularity.
///
/// Warm-up starts at T=-WARMUP_DURATION (~ 13 secs). As soon as warm up is initiated, we invoke
/// `BankingStage::new_num_threads()` as well to simulate the pre-leader slot's tx-buffering time.
pub struct BankingSimulator {
    banking_trace_events: BankingTraceEvents,
    first_simulated_slot: Slot,
}

#[derive(Error, Debug)]
pub enum SimulateError {
    #[error("IO Error: {0}")]
    IoError(#[from] io::Error),

    #[error("Deserialization Error: {0}")]
    DeserializeError(#[from] bincode::Error),
}

// Defined to be enough to cover the holding phase prior to leader slots with some idling (+5 secs)
const WARMUP_DURATION: Duration =
    Duration::from_millis(HOLD_TRANSACTIONS_SLOT_OFFSET * DEFAULT_MS_PER_SLOT + 5000);

/// BTreeMap is intentional because events could be unordered slightly due to tracing jitter.
type PacketBatchesByTime = BTreeMap<SystemTime, (ChannelLabel, BankingPacketBatch)>;

type FreezeTimeBySlot = BTreeMap<Slot, SystemTime>;

type TimedBatchesToSend = Vec<(
    (Duration, (ChannelLabel, BankingPacketBatch)),
    (usize, usize),
)>;

type EventSenderThread = JoinHandle<(TracedSender, TracedSender, TracedSender)>;

#[derive(Default)]
pub struct BankingTraceEvents {
    packet_batches_by_time: PacketBatchesByTime,
    freeze_time_by_slot: FreezeTimeBySlot,
    hash_overrides: HashOverrides,
}

impl BankingTraceEvents {
    fn read_event_file(
        event_file_path: &PathBuf,
        mut callback: impl FnMut(TimedTracedEvent),
    ) -> Result<(), SimulateError> {
        let mut reader = BufReader::new(File::open(event_file_path)?);

        // EOF is reached at a correct deserialization boundary or just the file is just empty.
        // We want to look-ahead the buf, so NOT calling reader.consume(..) is correct.
        while !reader.fill_buf()?.is_empty() {
            callback(deserialize_from(&mut reader)?);
        }

        Ok(())
    }

    pub fn load(event_file_paths: &[PathBuf]) -> Result<Self, SimulateError> {
        let mut event_count = 0;
        let mut events = Self::default();
        for event_file_path in event_file_paths {
            let old_event_count = event_count;
            let read_result = Self::read_event_file(event_file_path, |event| {
                event_count += 1;
                events.load_event(event);
            });
            info!(
                "Read {} events from {:?}",
                event_count - old_event_count,
                event_file_path,
            );

            if matches!(
                read_result,
                Err(SimulateError::DeserializeError(ref deser_err))
                    if matches!(
                        &**deser_err,
                        bincode::ErrorKind::Io(io_err)
                            if io_err.kind() == std::io::ErrorKind::UnexpectedEof
                    )
            ) {
                // Silence errors here as this can happen under normal operation...
                warn!(
                    "Reading {event_file_path:?} failed {read_result:?} due to file corruption or \
                     unclean validator shutdown",
                );
            } else {
                read_result?
            }
        }

        Ok(events)
    }

    fn load_event(&mut self, TimedTracedEvent(event_time, event): TimedTracedEvent) {
        match event {
            TracedEvent::PacketBatch(label, batch) => {
                // Deserialized PacketBatches will mostly be ordered by event_time, but this
                // isn't guaranteed when traced, because time are measured by multiple _sender_
                // threads without synchronization among them to avoid overhead.
                //
                // Also, there's a possibility of system clock change. In this case,
                // the simulation is meaningless, though...
                //
                // Somewhat naively assume that event_times (nanosecond resolution) won't
                // collide.
                let is_new = self
                    .packet_batches_by_time
                    .insert(event_time, (label, batch))
                    .is_none();
                assert!(is_new);
            }
            TracedEvent::BlockAndBankHash(slot, blockhash, bank_hash) => {
                let is_new = self.freeze_time_by_slot.insert(slot, event_time).is_none();
                self.hash_overrides.add_override(slot, blockhash, bank_hash);
                assert!(is_new);
            }
        }
    }

    pub fn hash_overrides(&self) -> &HashOverrides {
        &self.hash_overrides
    }
}

struct DummyClusterInfo {
    // Artificially wrap Pubkey with RwLock to induce lock contention if any to mimic the real
    // ClusterInfo
    id: RwLock<Pubkey>,
}

impl LikeClusterInfo for Arc<DummyClusterInfo> {
    fn id(&self) -> Pubkey {
        *self.id.read().unwrap()
    }

    fn lookup_contact_info<R>(&self, _: &Pubkey, _: impl ContactInfoQuery<R>) -> Option<R> {
        None
    }
}

struct SimulatorLoopLogger {
    simulated_leader: Pubkey,
    freeze_time_by_slot: FreezeTimeBySlot,
    base_event_time: SystemTime,
    base_simulation_time: SystemTime,
}

impl SimulatorLoopLogger {
    fn bank_costs(bank: &Bank) -> (u64, u64) {
        bank.read_cost_tracker()
            .map(|t| (t.block_cost(), t.vote_cost()))
            .unwrap()
    }

    fn log_frozen_bank_cost(&self, bank: &Bank, bank_elapsed: Duration) {
        info!(
            "simulated bank slot+delta: {}+{}ms costs: {:?} fees: {} txs: {} (frozen)",
            bank.slot(),
            bank_elapsed.as_millis(),
            Self::bank_costs(bank),
            bank.collector_fees(),
            bank.executed_transaction_count(),
        );
    }

    fn log_ongoing_bank_cost(&self, bank: &Bank, bank_elapsed: Duration) {
        info!(
            "simulated bank slot+delta: {}+{}ms costs: {:?} fees: {} txs: {} (ongoing)",
            bank.slot(),
            bank_elapsed.as_millis(),
            Self::bank_costs(bank),
            bank.collector_fees(),
            bank.executed_transaction_count(),
        );
    }

    fn log_jitter(&self, bank: &Bank) {
        let old_slot = bank.slot();
        if let Some(event_time) = self.freeze_time_by_slot.get(&old_slot) {
            if log_enabled!(log::Level::Info) {
                let current_simulation_time = SystemTime::now();
                let elapsed_simulation_time = current_simulation_time
                    .duration_since(self.base_simulation_time)
                    .unwrap();
                let elapsed_event_time = event_time.duration_since(self.base_event_time).unwrap();
                info!(
                    "jitter(parent_slot: {}): {}{:?} (sim: {:?} event: {:?})",
                    old_slot,
                    if elapsed_simulation_time > elapsed_event_time {
                        "+"
                    } else {
                        "-"
                    },
                    elapsed_simulation_time.abs_diff(elapsed_event_time),
                    elapsed_simulation_time,
                    elapsed_event_time,
                );
            }
        }
    }

    fn on_new_leader(
        &self,
        bank: &Bank,
        bank_elapsed: Duration,
        new_slot: Slot,
        new_leader: Pubkey,
    ) {
        self.log_frozen_bank_cost(bank, bank_elapsed);
        info!(
            "{} isn't leader anymore at slot {}; new leader: {}",
            self.simulated_leader, new_slot, new_leader
        );
    }
}

struct SenderLoop {
    parent_slot: Slot,
    first_simulated_slot: Slot,
    non_vote_sender: TracedSender,
    tpu_vote_sender: TracedSender,
    gossip_vote_sender: TracedSender,
    exit: Arc<AtomicBool>,
    raw_base_event_time: SystemTime,
    total_batch_count: usize,
    timed_batches_to_send: TimedBatchesToSend,
}

impl SenderLoop {
    fn log_starting(&self) {
        info!(
            "simulating events: {} (out of {}), starting at slot {} (based on {} from traced \
             event slot: {}) (warmup: -{:?})",
            self.timed_batches_to_send.len(),
            self.total_batch_count,
            self.first_simulated_slot,
            SenderLoopLogger::format_as_timestamp(self.raw_base_event_time),
            self.parent_slot,
            WARMUP_DURATION,
        );
    }

    fn spawn(self, base_simulation_time: SystemTime) -> Result<EventSenderThread, SimulateError> {
        let handle = thread::Builder::new()
            .name("solSimSender".into())
            .spawn(move || self.start(base_simulation_time))?;
        Ok(handle)
    }

    fn start(
        mut self,
        base_simulation_time: SystemTime,
    ) -> (TracedSender, TracedSender, TracedSender) {
        let mut logger = SenderLoopLogger::new(
            &self.non_vote_sender,
            &self.tpu_vote_sender,
            &self.gossip_vote_sender,
        );
        let mut simulation_duration = Duration::default();
        for ((required_duration, (label, batches_with_stats)), (batch_count, tx_count)) in
            self.timed_batches_to_send.drain(..)
        {
            // Busy loop for most accurate sending timings
            while simulation_duration < required_duration {
                let current_simulation_time = SystemTime::now();
                simulation_duration = current_simulation_time
                    .duration_since(base_simulation_time)
                    .unwrap();
            }

            let sender = match label {
                ChannelLabel::NonVote => &self.non_vote_sender,
                ChannelLabel::TpuVote => &self.tpu_vote_sender,
                ChannelLabel::GossipVote => &self.gossip_vote_sender,
                ChannelLabel::Dummy => unreachable!(),
            };
            sender.send(batches_with_stats).unwrap();

            logger.on_sending_batches(&simulation_duration, label, batch_count, tx_count);
            if self.exit.load(Ordering::Relaxed) {
                break;
            }
        }
        logger.on_terminating();
        drop(self.timed_batches_to_send);
        // hold these senders in join_handle to control banking stage termination!
        (
            self.non_vote_sender,
            self.tpu_vote_sender,
            self.gossip_vote_sender,
        )
    }
}

struct SimulatorLoop {
    bank: BankWithScheduler,
    parent_slot: Slot,
    first_simulated_slot: Slot,
    freeze_time_by_slot: FreezeTimeBySlot,
    base_event_time: SystemTime,
    poh_recorder: Arc<RwLock<PohRecorder>>,
    poh_controller: PohController,
    simulated_leader: Pubkey,
    bank_forks: Arc<RwLock<BankForks>>,
    blockstore: Arc<Blockstore>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    retransmit_slots_sender: Sender<Slot>,
    retracer: Arc<BankingTracer>,
}

impl SimulatorLoop {
    fn enter(
        self,
        base_simulation_time: SystemTime,
        sender_thread: EventSenderThread,
    ) -> (EventSenderThread, Sender<Slot>) {
        sleep(WARMUP_DURATION);
        info!("warmup done!");
        self.start(base_simulation_time, sender_thread)
    }

    fn start(
        mut self,
        base_simulation_time: SystemTime,
        sender_thread: EventSenderThread,
    ) -> (EventSenderThread, Sender<Slot>) {
        let logger = SimulatorLoopLogger {
            simulated_leader: self.simulated_leader,
            base_event_time: self.base_event_time,
            base_simulation_time,
            freeze_time_by_slot: self.freeze_time_by_slot,
        };
        let (mut bank, mut bank_created) = (self.bank, Instant::now());
        loop {
            if self.poh_recorder.read().unwrap().bank().is_none() {
                let next_leader_slot = self.leader_schedule_cache.next_leader_slot(
                    &self.simulated_leader,
                    bank.slot(),
                    &bank,
                    Some(&self.blockstore),
                    GRACE_TICKS_FACTOR * MAX_GRACE_SLOTS,
                );
                debug!("{next_leader_slot:?}");
                self.poh_controller
                    .reset_sync(bank.clone_without_scheduler(), next_leader_slot)
                    .unwrap();
                info!("Bank::new_from_parent()!");

                logger.log_jitter(&bank);
                if let Some((result, _execute_timings)) = bank.wait_for_completed_scheduler() {
                    assert_matches!(result, Ok(()));
                }
                bank.freeze();
                let new_slot = if bank.slot() == self.parent_slot {
                    info!("initial leader block!");
                    self.first_simulated_slot
                } else {
                    info!("next leader block!");
                    bank.slot() + 1
                };
                let new_leader = self
                    .leader_schedule_cache
                    .slot_leader_at(new_slot, None)
                    .unwrap();
                if new_leader != self.simulated_leader {
                    logger.on_new_leader(&bank, bank_created.elapsed(), new_slot, new_leader);
                    break;
                } else if sender_thread.is_finished() {
                    warn!("sender thread existed maybe due to completion of sending traced events");
                    break;
                } else {
                    info!("new leader bank slot: {new_slot}");
                }
                let new_bank = Bank::new_from_parent(
                    bank.clone_without_scheduler(),
                    &self.simulated_leader,
                    new_slot,
                );
                // make sure parent is frozen for finalized hashes via the above
                // new()-ing of its child bank
                self.retracer
                    .hash_event(bank.slot(), &bank.last_blockhash(), &bank.hash());
                if *bank.leader_id() == self.simulated_leader {
                    logger.log_frozen_bank_cost(&bank, bank_created.elapsed());
                }
                self.retransmit_slots_sender.send(bank.slot()).unwrap();
                update_bank_forks_and_poh_recorder_for_new_tpu_bank(
                    &self.bank_forks,
                    &mut self.poh_controller,
                    new_bank,
                );
                // Wait for the controller message to be processed.
                // This loop assumes that
                // `update_bank_forks_and_poh_recorder_for_new_tpu_bank`
                // takes immediate effect in PohRecorder's working bank.
                while self.poh_controller.has_pending_message() {}

                (bank, bank_created) = (
                    self.bank_forks
                        .read()
                        .unwrap()
                        .working_bank_with_scheduler(),
                    Instant::now(),
                );
                logger.log_ongoing_bank_cost(&bank, bank_created.elapsed());
            } else {
                logger.log_ongoing_bank_cost(&bank, bank_created.elapsed());
            }

            sleep(Duration::from_millis(10));
        }

        (sender_thread, self.retransmit_slots_sender)
    }
}

struct SimulatorThreads {
    poh_service: PohService,
    banking_stage: BankingStageHandle,
    broadcast_stage: BroadcastStage,
    retracer_thread: TracerThread,
    exit: Arc<AtomicBool>,
}

impl SimulatorThreads {
    fn finish(self, sender_thread: EventSenderThread, retransmit_slots_sender: Sender<Slot>) {
        info!("Sleeping a bit before signaling exit");
        sleep(Duration::from_millis(100));
        self.exit.store(true, Ordering::Relaxed);

        // The order is important. Consuming sender_thread by joining will drop some channels. That
        // triggers termination of banking_stage, in turn retracer thread will be terminated.
        sender_thread.join().unwrap();
        self.banking_stage.join().unwrap();
        self.poh_service.join().unwrap();
        if let Some(retracer_thread) = self.retracer_thread {
            retracer_thread.join().unwrap().unwrap();
        }

        info!("Joining broadcast stage...");
        drop(retransmit_slots_sender);
        self.broadcast_stage.join().unwrap();
    }
}

struct SenderLoopLogger<'a> {
    non_vote_sender: &'a TracedSender,
    tpu_vote_sender: &'a TracedSender,
    gossip_vote_sender: &'a TracedSender,
    last_log_duration: Duration,
    last_tx_count: usize,
    last_non_vote_batch_count: usize,
    last_tpu_vote_tx_count: usize,
    last_gossip_vote_tx_count: usize,
    non_vote_batch_count: usize,
    non_vote_tx_count: usize,
    tpu_vote_batch_count: usize,
    tpu_vote_tx_count: usize,
    gossip_vote_batch_count: usize,
    gossip_vote_tx_count: usize,
}

impl<'a> SenderLoopLogger<'a> {
    fn new(
        non_vote_sender: &'a TracedSender,
        tpu_vote_sender: &'a TracedSender,
        gossip_vote_sender: &'a TracedSender,
    ) -> Self {
        Self {
            non_vote_sender,
            tpu_vote_sender,
            gossip_vote_sender,
            last_log_duration: Duration::default(),
            last_tx_count: 0,
            last_non_vote_batch_count: 0,
            last_tpu_vote_tx_count: 0,
            last_gossip_vote_tx_count: 0,
            non_vote_batch_count: 0,
            non_vote_tx_count: 0,
            tpu_vote_batch_count: 0,
            tpu_vote_tx_count: 0,
            gossip_vote_batch_count: 0,
            gossip_vote_tx_count: 0,
        }
    }

    fn on_sending_batches(
        &mut self,
        &simulation_duration: &Duration,
        label: ChannelLabel,
        batch_count: usize,
        tx_count: usize,
    ) {
        debug!("sent {label:?} {batch_count} batches ({tx_count} txes)");

        use ChannelLabel::*;
        let (total_batch_count, total_tx_count) = match label {
            NonVote => (&mut self.non_vote_batch_count, &mut self.non_vote_tx_count),
            TpuVote => (&mut self.tpu_vote_batch_count, &mut self.tpu_vote_tx_count),
            GossipVote => (
                &mut self.gossip_vote_batch_count,
                &mut self.gossip_vote_tx_count,
            ),
            Dummy => unreachable!(),
        };
        *total_batch_count += batch_count;
        *total_tx_count += tx_count;

        let log_interval = simulation_duration - self.last_log_duration;
        if log_interval > Duration::from_millis(100) {
            let current_tx_count =
                self.non_vote_tx_count + self.tpu_vote_tx_count + self.gossip_vote_tx_count;
            let duration = log_interval.as_secs_f64();
            let tps = (current_tx_count - self.last_tx_count) as f64 / duration;
            let non_vote_tps =
                (self.non_vote_tx_count - self.last_non_vote_batch_count) as f64 / duration;
            let tpu_vote_tps =
                (self.tpu_vote_tx_count - self.last_tpu_vote_tx_count) as f64 / duration;
            let gossip_vote_tps =
                (self.gossip_vote_tx_count - self.last_gossip_vote_tx_count) as f64 / duration;
            info!(
                "senders(non-,tpu-,gossip-vote): tps: {:.0} (={:.0}+{:.0}+{:.0}) over {:?} \
                 not-recved: ({}+{}+{})",
                tps,
                non_vote_tps,
                tpu_vote_tps,
                gossip_vote_tps,
                log_interval,
                self.non_vote_sender.len(),
                self.tpu_vote_sender.len(),
                self.gossip_vote_sender.len(),
            );
            self.last_log_duration = simulation_duration;
            self.last_tx_count = current_tx_count;
            (
                self.last_non_vote_batch_count,
                self.last_tpu_vote_tx_count,
                self.last_gossip_vote_tx_count,
            ) = (
                self.non_vote_tx_count,
                self.tpu_vote_tx_count,
                self.gossip_vote_batch_count,
            );
        }
    }

    fn on_terminating(self) {
        info!(
            "terminating to send...: non_vote: {} ({}), tpu_vote: {} ({}), gossip_vote: {} ({})",
            self.non_vote_batch_count,
            self.non_vote_tx_count,
            self.tpu_vote_batch_count,
            self.tpu_vote_tx_count,
            self.gossip_vote_batch_count,
            self.gossip_vote_tx_count,
        );
    }

    fn format_as_timestamp(time: SystemTime) -> impl Display + use<> {
        let time: chrono::DateTime<chrono::Utc> = time.into();
        time.format("%Y-%m-%d %H:%M:%S.%f")
    }
}

impl BankingSimulator {
    pub fn new(banking_trace_events: BankingTraceEvents, first_simulated_slot: Slot) -> Self {
        Self {
            banking_trace_events,
            first_simulated_slot,
        }
    }

    pub fn parent_slot(&self) -> Option<Slot> {
        self.banking_trace_events
            .freeze_time_by_slot
            .range(..self.first_simulated_slot)
            .last()
            .map(|(slot, _time)| slot)
            .copied()
    }

    fn prepare_simulation(
        self,
        genesis_config: GenesisConfig,
        bank_forks: Arc<RwLock<BankForks>>,
        blockstore: Arc<Blockstore>,
        block_production_method: BlockProductionMethod,
        unified_scheduler_pool: Option<Arc<DefaultSchedulerPool>>,
    ) -> (SenderLoop, SimulatorLoop, SimulatorThreads) {
        let parent_slot = self.parent_slot().unwrap();
        let mut packet_batches_by_time = self.banking_trace_events.packet_batches_by_time;
        let freeze_time_by_slot = self.banking_trace_events.freeze_time_by_slot;
        let bank = bank_forks.read().unwrap().working_bank_with_scheduler();

        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        assert_eq!(parent_slot, bank.slot());

        let simulated_leader = leader_schedule_cache
            .slot_leader_at(self.first_simulated_slot, None)
            .unwrap();
        info!(
            "Simulated leader and slot: {}, {}",
            simulated_leader, self.first_simulated_slot,
        );

        let exit = Arc::new(AtomicBool::default());

        if let Some(end_slot) = blockstore
            .slot_meta_iterator(self.first_simulated_slot)
            .unwrap()
            .map(|(s, _)| s)
            .last()
        {
            info!("purging slots {}, {}", self.first_simulated_slot, end_slot);
            blockstore.purge_from_next_slots(self.first_simulated_slot, end_slot);
            blockstore.purge_slots(self.first_simulated_slot, end_slot, PurgeType::Exact);
            info!("done: purging");
        } else {
            info!("skipping purging...");
        }

        info!("Poh is starting!");

        let (poh_recorder, entry_receiver) = PohRecorder::new_with_clear_signal(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            None,
            bank.ticks_per_slot(),
            false,
            blockstore.clone(),
            blockstore.get_new_shred_signal(0),
            &leader_schedule_cache,
            &genesis_config.poh_config,
            exit.clone(),
        );
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));
        let (record_sender, record_receiver) = record_channels(false);
        let transaction_recorder = TransactionRecorder::new(record_sender);
        let (poh_controller, poh_service_message_receiver) = PohController::new();
        let poh_service = PohService::new(
            poh_recorder.clone(),
            &genesis_config.poh_config,
            exit.clone(),
            bank.ticks_per_slot(),
            DEFAULT_PINNED_CPU_CORE,
            DEFAULT_HASHES_PER_BATCH,
            record_receiver,
            poh_service_message_receiver,
        );

        // Enable BankingTracer to approximate the real environment as close as possible because
        // it's not expected to disable BankingTracer on production environments.
        //
        // It's not likely for it to affect the banking stage performance noticeably. So, make sure
        // that assumption is held here. That said, it incurs additional channel sending,
        // SystemTime::now() and buffered seq IO, and indirectly functions as a background dropper
        // of `BankingPacketBatch`.
        //
        // Lastly, the actual retraced events can be used to evaluate simulation timing accuracy in
        // the future.
        let (retracer, retracer_thread) = BankingTracer::new(Some((
            &blockstore.banking_retracer_path(),
            exit.clone(),
            BANKING_TRACE_DIR_DEFAULT_BYTE_LIMIT,
        )))
        .unwrap();
        assert!(retracer.is_enabled());
        info!("Enabled banking retracer (dir_byte_limit: {BANKING_TRACE_DIR_DEFAULT_BYTE_LIMIT})",);

        let num_workers = BankingStage::default_num_workers();
        let banking_tracer_channels = retracer.create_channels();
        if let Some(pool) = unified_scheduler_pool {
            ensure_banking_stage_setup(
                &pool,
                &bank_forks,
                &banking_tracer_channels,
                &poh_recorder,
                transaction_recorder.clone(),
                num_workers,
            );
        };
        let Channels {
            non_vote_sender,
            non_vote_receiver,
            tpu_vote_sender,
            tpu_vote_receiver,
            gossip_vote_sender,
            gossip_vote_receiver,
        } = banking_tracer_channels;

        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let (retransmit_slots_sender, retransmit_slots_receiver) = unbounded();
        let shred_version = compute_shred_version(
            &genesis_config.hash(),
            Some(&bank_forks.read().unwrap().root_bank().hard_forks()),
        );
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);

        // Create a completely-dummy ClusterInfo for the broadcast stage.
        // We only need it to write shreds into the blockstore and it seems given ClusterInfo is
        // irrelevant for the necessary minimum work for this simulation.
        let random_keypair = Arc::new(Keypair::new());
        let cluster_info_for_broadcast = Arc::new(ClusterInfo::new(
            Node::new_localhost_with_pubkey(&random_keypair.pubkey()).info,
            random_keypair,
            SocketAddrSpace::Unspecified,
        ));
        // Broadcast stage is needed to save the simulated blocks for post-run analysis by
        // inserting produced shreds into the blockstore.
        let (_, socket) = bind_in_range_with_config(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            (1024, u16::MAX),
            SocketConfiguration::default(),
        )
        .expect("should bind");
        let broadcast_stage = BroadcastStageType::Standard.new_broadcast_stage(
            vec![socket],
            cluster_info_for_broadcast.clone(),
            entry_receiver,
            retransmit_slots_receiver,
            exit.clone(),
            blockstore.clone(),
            bank_forks.clone(),
            shred_version,
            sender,
            None,
        );

        info!("Start banking stage!...");
        let banking_stage = BankingStage::new_num_threads(
            block_production_method.clone(),
            poh_recorder.clone(),
            transaction_recorder,
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_vote_receiver,
            mpsc::channel(1).1,
            num_workers,
            SchedulerConfig::default(),
            None,
            replay_vote_sender,
            None,
            bank_forks.clone(),
            None,
        );

        let (&_slot, &raw_base_event_time) = freeze_time_by_slot
            .range(parent_slot..)
            .next()
            .expect("timed hashes");
        let base_event_time = raw_base_event_time - WARMUP_DURATION;

        let total_batch_count = packet_batches_by_time.len();
        let timed_batches_to_send = packet_batches_by_time.split_off(&base_event_time);
        let batch_and_tx_counts = timed_batches_to_send
            .values()
            .map(|(_label, batches)| {
                (
                    batches.len(),
                    batches.iter().map(|batch| batch.len()).sum::<usize>(),
                )
            })
            .collect::<Vec<_>>();
        // Convert to a large plain old Vec and drain on it, finally dropping it outside
        // the simulation loop to avoid jitter due to interleaved deallocs of BTreeMap.
        let timed_batches_to_send = timed_batches_to_send
            .into_iter()
            .map(|(event_time, batches)| {
                (event_time.duration_since(base_event_time).unwrap(), batches)
            })
            .zip_eq(batch_and_tx_counts)
            .collect::<Vec<_>>();

        let sender_loop = SenderLoop {
            parent_slot,
            first_simulated_slot: self.first_simulated_slot,
            non_vote_sender,
            tpu_vote_sender,
            gossip_vote_sender,
            exit: exit.clone(),
            raw_base_event_time,
            total_batch_count,
            timed_batches_to_send,
        };

        let simulator_loop = SimulatorLoop {
            bank,
            parent_slot,
            first_simulated_slot: self.first_simulated_slot,
            freeze_time_by_slot,
            base_event_time,
            poh_recorder,
            poh_controller,
            simulated_leader,
            bank_forks,
            blockstore,
            leader_schedule_cache,
            retransmit_slots_sender,
            retracer,
        };

        let simulator_threads = SimulatorThreads {
            poh_service,
            banking_stage,
            broadcast_stage,
            retracer_thread,
            exit,
        };

        (sender_loop, simulator_loop, simulator_threads)
    }

    pub fn start(
        self,
        genesis_config: GenesisConfig,
        bank_forks: Arc<RwLock<BankForks>>,
        blockstore: Arc<Blockstore>,
        block_production_method: BlockProductionMethod,
        unified_scheduler_pool: Option<Arc<DefaultSchedulerPool>>,
    ) -> Result<(), SimulateError> {
        let (sender_loop, simulator_loop, simulator_threads) = self.prepare_simulation(
            genesis_config,
            bank_forks,
            blockstore,
            block_production_method,
            unified_scheduler_pool,
        );

        sender_loop.log_starting();
        let base_simulation_time = SystemTime::now();
        // Spawning and entering these two loops must be done at the same time as they're timed.
        // So, all the mundane setup must be done in advance.
        let sender_thread = sender_loop.spawn(base_simulation_time)?;
        let (sender_thread, retransmit_slots_sender) =
            simulator_loop.enter(base_simulation_time, sender_thread);

        simulator_threads.finish(sender_thread, retransmit_slots_sender);

        Ok(())
    }

    pub fn event_file_name(index: usize) -> String {
        if index == 0 {
            BASENAME.to_string()
        } else {
            format!("{BASENAME}.{index}")
        }
    }
}
