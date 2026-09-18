//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.

use {
    crate::{
        banking_trace::BankingPacketSender, sigverify_stage::SigVerifyServiceError,
        transaction_priority::calculate_priority_from_bytes,
    },
    agave_banking_stage_ingress_types::{BankingPacketBatch, SchedulerPriorityFloor},
    agave_wake_channel::{
        Receiver as LaneReceiver, RecvError, Sender as LaneSender, TryRecvError, WakeGroup,
    },
    crossbeam_channel::{Sender, TrySendError},
    solana_measure::measure_us,
    solana_perf::{
        deduper::{self, Deduper},
        packet::PacketBatch,
        sigverify::{self},
    },
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    solana_transaction::Transaction,
    std::{
        cell::Cell,
        num::NonZeroUsize,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread::JoinHandle,
    },
};

pub(crate) struct GossipVerifyTask {
    batch: PacketBatch,
    transaction: Transaction,
}

pub(crate) struct GossipVerifiedVoteBatch {
    pub(crate) transaction: Transaction,
    pub(crate) packet_batch: PacketBatch,
}

#[derive(Clone)]
pub(crate) struct SigVerifyWorkerStats {
    pub(crate) total_batches: Arc<AtomicUsize>,
    pub(crate) total_packets: Arc<AtomicUsize>,
    pub(crate) total_dedup: Arc<AtomicUsize>,
    pub(crate) total_dedup_time_us: Arc<AtomicUsize>,
    pub(crate) total_valid_packets: Arc<AtomicUsize>,
    pub(crate) total_verify_time_us: Arc<AtomicUsize>,
    /// Max occupancy of the banking_stage channel sampled immediately before each send.
    pub(crate) max_pre_send_len: Arc<AtomicUsize>,
    /// Count of sends where the EvictingSender had to drop a batch to make room.
    pub(crate) eviction_drops: Arc<AtomicUsize>,
    pub(crate) total_dropped_below_priority_floor: Arc<AtomicUsize>,
    pub(crate) total_priority_floor_time_us: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub(crate) struct SigVerifyWorkerState {
    banking_stage_sender: BankingPacketSender,
    deduper: Arc<Deduper<2, [u8]>>,
    stats: SigVerifyWorkerStats,
    /// Scheduler-published priority floor: when saturated, the scheduler publishes
    /// the queue-min transaction's priority and workers drop at-or-below-floor
    /// arrivals here, ahead of signature verification. `None` disables the
    /// check (e.g. for the vote worker, which is governed by a separate
    /// priority policy in banking stage).
    priority_floor: Option<Arc<SchedulerPriorityFloor>>,
}

impl SigVerifyWorkerState {
    pub(crate) fn new(
        banking_stage_sender: BankingPacketSender,
        deduper: Arc<Deduper<2, [u8]>>,
        stats: SigVerifyWorkerStats,
        priority_floor: Option<Arc<SchedulerPriorityFloor>>,
    ) -> Self {
        Self {
            banking_stage_sender,
            deduper,
            stats,
            priority_floor,
        }
    }
}

pub(crate) struct GossipSigVerifier {
    worker_sender: LaneSender<GossipVerifyTask>,
}

impl GossipSigVerifier {
    #[cfg(test)]
    pub(crate) fn new_for_tests(worker_sender: LaneSender<GossipVerifyTask>) -> Self {
        Self { worker_sender }
    }

    pub(crate) fn send_votes_to_worker_pool(
        &self,
        votes: Vec<Transaction>,
        packet_batches: Vec<PacketBatch>,
    ) -> Result<usize, SigVerifyServiceError> {
        assert_eq!(votes.len(), packet_batches.len());

        let num_votes = votes.len();
        let mut num_sent = 0;
        for (transaction, batch) in votes.into_iter().zip(packet_batches) {
            match self
                .worker_sender
                .try_send(GossipVerifyTask { batch, transaction })
            {
                Ok(()) => {
                    num_sent += 1;
                }
                Err(TrySendError::Full(_)) => {
                    warn!(
                        "gossip sigverify worker queue is full, dropping {} votes.",
                        num_votes.saturating_sub(num_sent)
                    );
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(SigVerifyServiceError::WorkerQueueClosed);
                }
            }
        }

        Ok(num_sent)
    }
}

/// Gossip votes use a bounded queue into the worker pool.
const SIGVERIFY_GOSSIP_VOTE_WORK_CHANNEL_SIZE: usize = 50_000;

pub(crate) struct SigVerifyWorkerSenders {
    pub(crate) gossip_verified_vote_sender: Sender<GossipVerifiedVoteBatch>,
    pub(crate) forward_stage_sender: Sender<(BankingPacketBatch, bool)>,
}

/// The lanes a worker drains, in base order. Each worker rotates its starting lane after every
/// receive so that a busy lane cannot monopolise it; see [`WorkerPoolChannels::poll`].
#[derive(Clone, Copy)]
enum Lane {
    TpuVote,
    Gossip,
    NonVote,
}

const LANES: [Lane; 3] = [Lane::TpuVote, Lane::Gossip, Lane::NonVote];

/// One unit of work, tagged with the lane it came from.
enum SigVerifyWork {
    TpuVote(PacketBatch),
    Gossip(GossipVerifyTask),
    NonVote(PacketBatch),
}

#[derive(Clone)]
struct WorkerPoolChannels {
    /// Shared by all three lanes; producers wake sleeping workers through it.
    wake_group: Arc<WakeGroup>,
    /// Index into `LANES` where the next poll starts, advanced past the lane that last yielded so
    /// a busy lane cannot monopolise the worker. `Cell` because `poll` runs inside the closure
    /// passed to `recv_with` and therefore only has `&self`.
    next_lane: Cell<usize>,
    non_vote_receiver: LaneReceiver<PacketBatch>,
    tpu_vote_receiver: LaneReceiver<PacketBatch>,
    gossip_receiver: LaneReceiver<GossipVerifyTask>,
    gossip_verified_vote_sender: Sender<GossipVerifiedVoteBatch>,
    forward_stage_sender: Sender<(BankingPacketBatch, bool)>,
    sharable_banks: SharableBanks,
    non_vote_state: SigVerifyWorkerState,
    tpu_vote_state: SigVerifyWorkerState,
}

impl WorkerPoolChannels {
    fn try_recv_lane(&self, lane: Lane) -> Result<SigVerifyWork, TryRecvError> {
        match lane {
            Lane::TpuVote => self
                .tpu_vote_receiver
                .try_recv()
                .map(SigVerifyWork::TpuVote),
            Lane::Gossip => self.gossip_receiver.try_recv().map(SigVerifyWork::Gossip),
            Lane::NonVote => self
                .non_vote_receiver
                .try_recv()
                .map(SigVerifyWork::NonVote),
        }
    }

    /// Polls every lane once, starting at `LANES[self.next_lane]`, and moves the start past the
    /// lane that yielded. A disconnected lane, or the pool's exit flag, ends the worker, as it did
    /// under `select!`.
    ///
    /// This runs inside `WakeGroup::recv_with`, so it is also what the worker re-checks right after
    /// registering as a waiter. Everything read here (lane contents, disconnects, `exit`) is
    /// therefore covered by the wake protocol, provided the writer calls `wake_one`/`wake_all` on
    /// the group after making its change.
    fn poll(&self, exit: &AtomicBool) -> Result<SigVerifyWork, TryRecvError> {
        if exit.load(Ordering::Relaxed) {
            return Err(TryRecvError::Disconnected);
        }
        for (index, lane) in LANES
            .iter()
            .enumerate()
            .cycle()
            .skip(self.next_lane.get())
            .take(LANES.len())
        {
            match self.try_recv_lane(*lane) {
                Ok(work) => {
                    self.next_lane.set((index + 1) % LANES.len());
                    return Ok(work);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return Err(TryRecvError::Disconnected),
            }
        }
        Err(TryRecvError::Empty)
    }

    fn recv(&self, exit: &AtomicBool) -> Result<SigVerifyWork, RecvError> {
        self.wake_group.recv_with(|| self.poll(exit))
    }
}

pub(crate) struct SigVerifyWorkerPool {
    exit: Arc<AtomicBool>,
    wake_group: Arc<WakeGroup>,
    gossip_sender: LaneSender<GossipVerifyTask>,
    worker_hdls: Vec<JoinHandle<()>>,
}

impl Drop for SigVerifyWorkerPool {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        // Wake every worker so the join below cannot hang. Workers re-read `exit` right after
        // registering as waiters (see `WorkerPoolChannels::poll`), so the wake cannot be missed.
        // Disconnect would not do it: `Tpu::join` runs this drop before joining the streamer
        // threads that own the lane senders, so the lanes are still connected here.
        self.wake_group.wake_all();
        self.worker_hdls.drain(..).for_each(|hdl| {
            if let Err(err) = hdl.join() {
                error!("sigverify worker encountered unexpected error: {err:?}");
            }
        });
    }
}

impl SigVerifyWorkerPool {
    pub(crate) fn new(
        num_workers: NonZeroUsize,
        non_vote_receiver: LaneReceiver<PacketBatch>,
        tpu_vote_receiver: LaneReceiver<PacketBatch>,
        senders: SigVerifyWorkerSenders,
        forward_non_votes: bool,
        sharable_banks: SharableBanks,
        non_vote_state: SigVerifyWorkerState,
        tpu_vote_state: SigVerifyWorkerState,
    ) -> Self {
        let wake_group = Arc::clone(non_vote_receiver.wake_group());
        // A lane on another group would fill up without ever waking a sleeping worker.
        assert!(
            Arc::ptr_eq(&wake_group, tpu_vote_receiver.wake_group()),
            "sigverify lanes must share one wake group"
        );
        let (gossip_sender, gossip_receiver) = agave_wake_channel::bounded(
            SIGVERIFY_GOSSIP_VOTE_WORK_CHANNEL_SIZE,
            Arc::clone(&wake_group),
        );
        let channels = WorkerPoolChannels {
            wake_group: Arc::clone(&wake_group),
            next_lane: Cell::new(0),
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_receiver,
            gossip_verified_vote_sender: senders.gossip_verified_vote_sender,
            forward_stage_sender: senders.forward_stage_sender,
            sharable_banks,
            non_vote_state,
            tpu_vote_state,
        };
        let exit = Arc::new(AtomicBool::new(false));
        let worker_hdls = (0..num_workers.get())
            .map(|idx| {
                let exit = exit.clone();
                let channels = channels.clone();

                std::thread::Builder::new()
                    .name(format!("solSigVerify{idx:02}"))
                    .spawn(move || Self::worker(exit, channels, forward_non_votes))
                    .expect("failed to spawn sigverify worker thread")
            })
            .collect();
        Self {
            exit,
            wake_group,
            gossip_sender,
            worker_hdls,
        }
    }

    pub(crate) fn gossip_verifier(&self) -> GossipSigVerifier {
        GossipSigVerifier {
            worker_sender: self.gossip_sender.clone(),
        }
    }

    fn worker(exit: Arc<AtomicBool>, channels: WorkerPoolChannels, forward_non_votes: bool) {
        while let Ok(work) = channels.recv(&exit) {
            let keep_going = match work {
                SigVerifyWork::NonVote(batch) => Self::run_transaction_task(
                    batch,
                    false,
                    &channels.forward_stage_sender,
                    forward_non_votes,
                    false,
                    &channels.sharable_banks,
                    &channels.non_vote_state,
                ),
                SigVerifyWork::TpuVote(batch) => Self::run_transaction_task(
                    batch,
                    true,
                    &channels.forward_stage_sender,
                    true,
                    true,
                    &channels.sharable_banks,
                    &channels.tpu_vote_state,
                ),
                SigVerifyWork::Gossip(task) => {
                    Self::run_gossip_task(task, &channels.gossip_verified_vote_sender)
                }
            };
            if !keep_going {
                break;
            }
        }
    }

    fn run_transaction_task(
        mut batch: PacketBatch,
        reject_non_vote: bool,
        forward_stage_sender: &Sender<(BankingPacketBatch, bool)>,
        should_forward: bool,
        is_tpu_vote: bool,
        sharable_banks: &SharableBanks,
        state: &SigVerifyWorkerState,
    ) -> bool {
        let batch_len = batch.len();
        state.stats.total_batches.fetch_add(1, Ordering::Relaxed);
        state
            .stats
            .total_packets
            .fetch_add(batch_len, Ordering::Relaxed);

        let (discard_or_dedup_fail, dedup_time_us) =
            measure_us!(deduper::dedup_packets_and_count_discards(
                &state.deduper,
                std::slice::from_mut(&mut batch)
            ));
        state
            .stats
            .total_dedup
            .fetch_add(discard_or_dedup_fail as usize, Ordering::Relaxed);
        state
            .stats
            .total_dedup_time_us
            .fetch_add(dedup_time_us as usize, Ordering::Relaxed);

        if discard_or_dedup_fail as usize == batch_len {
            return true;
        }

        let working_bank = sharable_banks.working();

        if let Some(floor) = state.priority_floor.as_ref() {
            let floor = floor.get();
            if floor > 0 {
                let ((dropped, all_below), priority_floor_time_us) = measure_us!(
                    apply_priority_floor_to_batch(&mut batch, floor, &working_bank)
                );
                state
                    .stats
                    .total_priority_floor_time_us
                    .fetch_add(priority_floor_time_us as usize, Ordering::Relaxed);
                if dropped > 0 {
                    state
                        .stats
                        .total_dropped_below_priority_floor
                        .fetch_add(dropped, Ordering::Relaxed);
                }
                if all_below {
                    // Entire batch went below-floor: nothing left to verify or
                    // forward.
                    return true;
                }
            }
        }

        let (_, verify_time_us) = measure_us!(sigverify::ed25519_verify_serial(
            &mut batch,
            reject_non_vote,
        ));
        let num_valid_packets = sigverify::count_valid_packets(std::iter::once(&batch));
        state
            .stats
            .total_valid_packets
            .fetch_add(num_valid_packets, Ordering::Relaxed);
        state
            .stats
            .total_verify_time_us
            .fetch_add(verify_time_us as usize, Ordering::Relaxed);

        if num_valid_packets == 0 {
            return true;
        }

        let banking_packet_batch = BankingPacketBatch::new(batch);
        // Sample backlog before the push: measures consumer health without
        // including this batch's own contribution.
        state
            .stats
            .max_pre_send_len
            .fetch_max(state.banking_stage_sender.len(), Ordering::Relaxed);
        match state
            .banking_stage_sender
            .send(banking_packet_batch.clone())
        {
            Ok(0) => {} // avoid poking atomics if nothing was evicted (typical case)
            Ok(evicted) => {
                // record evicted amount into metrics
                state
                    .stats
                    .eviction_drops
                    .fetch_add(evicted, Ordering::Relaxed);
            }
            Err(err) => {
                error!("sigverify send to banking failed: {err:?}");
                return false;
            }
        }
        if should_forward {
            Self::try_forward(forward_stage_sender, banking_packet_batch, is_tpu_vote);
        }

        true
    }

    fn run_gossip_task(
        mut work: GossipVerifyTask,
        verified_vote_sender: &Sender<GossipVerifiedVoteBatch>,
    ) -> bool {
        // Gossip votes are legacy Transaction values, not tx-v1 packets.
        sigverify::ed25519_verify_serial(&mut work.batch, true);

        if let Err(err) = verified_vote_sender.send(GossipVerifiedVoteBatch {
            transaction: work.transaction,
            packet_batch: work.batch,
        }) {
            debug!("gossip sigverify response send failed: {err:?}");
        }

        true
    }

    fn try_forward(
        forward_stage_sender: &Sender<(BankingPacketBatch, bool)>,
        banking_packet_batch: BankingPacketBatch,
        is_tpu_vote: bool,
    ) {
        if let Err(TrySendError::Full(_)) =
            forward_stage_sender.try_send((banking_packet_batch, is_tpu_vote))
        {
            warn!("forwarding stage channel is full, dropping packets.");
        }
    }
}

/// Apply the scheduler-published priority floor to a single batch in place.
///
/// Below-floor packets are marked `discard`. Returns `(dropped, all_below)`,
/// where `dropped` is the number of packets newly marked and `all_below` is
/// true iff no useful packets remain in the batch (so the caller can skip
/// downstream work for this batch entirely).
fn apply_priority_floor_to_batch(
    batch: &mut PacketBatch,
    floor: u64,
    bank: &Bank,
) -> (usize, bool) {
    let mut dropped: usize = 0;
    let mut any_kept = false;
    for mut packet in batch.iter_mut() {
        if packet.meta().discard() {
            continue;
        }
        let Some(data) = packet.data(..) else {
            // Zero-length or otherwise unreadable: leave to downstream
            // stages to reject.
            any_kept = true;
            continue;
        };
        // Unparseable packets are kept and left for downstream rejection.
        match calculate_priority_from_bytes(bank, data) {
            Some(priority) if priority <= floor => {
                packet.meta_mut().set_discard(true);
                dropped = dropped.saturating_add(1);
            }
            _ => any_kept = true,
        }
    }
    (dropped, !any_kept)
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::banking_trace::BankingTracer, solana_perf::packet::BytesPacketBatch,
        solana_runtime::genesis_utils::create_genesis_config,
    };

    fn test_channels() -> (
        WorkerPoolChannels,
        LaneSender<PacketBatch>,
        LaneSender<PacketBatch>,
        LaneSender<GossipVerifyTask>,
    ) {
        let wake_group = Arc::new(WakeGroup::default());
        let (non_vote_sender, non_vote_receiver) =
            agave_wake_channel::bounded(4, wake_group.clone());
        let (tpu_vote_sender, tpu_vote_receiver) =
            agave_wake_channel::bounded(4, wake_group.clone());
        let (gossip_sender, gossip_receiver) = agave_wake_channel::bounded(4, wake_group.clone());
        let (_, bank_forks) =
            Bank::new_with_bank_forks_for_tests(&create_genesis_config(1).genesis_config);
        let state = SigVerifyWorkerState::new(
            BankingTracer::channel_for_test().0,
            Arc::new(Deduper::new(&mut rand::rng(), 1024)),
            SigVerifyWorkerStats {
                total_batches: Arc::default(),
                total_packets: Arc::default(),
                total_dedup: Arc::default(),
                total_dedup_time_us: Arc::default(),
                total_valid_packets: Arc::default(),
                total_verify_time_us: Arc::default(),
                max_pre_send_len: Arc::default(),
                eviction_drops: Arc::default(),
                total_dropped_below_priority_floor: Arc::default(),
                total_priority_floor_time_us: Arc::default(),
            },
            None,
        );
        let channels = WorkerPoolChannels {
            wake_group,
            next_lane: Cell::new(0),
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_receiver,
            gossip_verified_vote_sender: crossbeam_channel::unbounded().0,
            forward_stage_sender: crossbeam_channel::unbounded().0,
            sharable_banks: bank_forks.read().unwrap().sharable_banks(),
            non_vote_state: state.clone(),
            tpu_vote_state: state,
        };
        (channels, non_vote_sender, tpu_vote_sender, gossip_sender)
    }

    fn empty_batch() -> PacketBatch {
        PacketBatch::Bytes(BytesPacketBatch::default())
    }

    fn enqueue_lanes(
        non_vote_sender: &LaneSender<PacketBatch>,
        tpu_vote_sender: &LaneSender<PacketBatch>,
        gossip_sender: &LaneSender<GossipVerifyTask>,
    ) {
        non_vote_sender.try_send(empty_batch()).unwrap();
        tpu_vote_sender.try_send(empty_batch()).unwrap();
        assert!(
            gossip_sender
                .try_send(GossipVerifyTask {
                    batch: empty_batch(),
                    transaction: Transaction::default(),
                })
                .is_ok()
        );
    }

    #[test]
    fn test_poll_each_lane() {
        let (channels, non_vote_sender, tpu_vote_sender, gossip_sender) = test_channels();
        let exit = AtomicBool::new(false);
        // One item per lane: successive polls drain them in lane order.
        enqueue_lanes(&non_vote_sender, &tpu_vote_sender, &gossip_sender);
        assert!(matches!(
            channels.poll(&exit),
            Ok(SigVerifyWork::TpuVote(_))
        ));
        assert!(matches!(channels.poll(&exit), Ok(SigVerifyWork::Gossip(_))));
        assert!(matches!(
            channels.poll(&exit),
            Ok(SigVerifyWork::NonVote(_))
        ));
        assert!(matches!(channels.poll(&exit), Err(TryRecvError::Empty)));

        // The rotation now starts at the vote lane again; a poll must skip the two empty lanes
        // ahead of the only one holding data.
        assert_eq!(channels.next_lane.get(), 0);
        non_vote_sender.try_send(empty_batch()).unwrap();
        assert!(matches!(
            channels.poll(&exit),
            Ok(SigVerifyWork::NonVote(_))
        ));
        assert!(matches!(channels.poll(&exit), Err(TryRecvError::Empty)));
    }

    #[test]
    fn test_poll_rotates_across_backlogged_lanes() {
        let (channels, non_vote_sender, tpu_vote_sender, gossip_sender) = test_channels();
        let exit = AtomicBool::new(false);
        for _ in 0..3 {
            enqueue_lanes(&non_vote_sender, &tpu_vote_sender, &gossip_sender);
        }
        // With every lane backlogged, the rotation must visit them round-robin, wrapping from the
        // last lane back to the first, rather than draining one lane first.
        for _ in 0..3 {
            assert!(matches!(
                channels.poll(&exit),
                Ok(SigVerifyWork::TpuVote(_))
            ));
            assert!(matches!(channels.poll(&exit), Ok(SigVerifyWork::Gossip(_))));
            assert!(matches!(
                channels.poll(&exit),
                Ok(SigVerifyWork::NonVote(_))
            ));
        }
        assert!(matches!(channels.poll(&exit), Err(TryRecvError::Empty)));

        // Shutdown takes precedence even while work remains queued.
        enqueue_lanes(&non_vote_sender, &tpu_vote_sender, &gossip_sender);
        exit.store(true, Ordering::Relaxed);
        assert!(matches!(
            channels.poll(&exit),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn test_poll_disconnected_lane() {
        for disconnected_lane in LANES {
            let (channels, non_vote_sender, tpu_vote_sender, gossip_sender) = test_channels();
            match disconnected_lane {
                Lane::NonVote => drop(non_vote_sender),
                Lane::TpuVote => drop(tpu_vote_sender),
                Lane::Gossip => drop(gossip_sender),
            }
            // Whichever lane the rotation starts at, the disconnect must be reported.
            for first_lane in 0..LANES.len() {
                channels.next_lane.set(first_lane);
                assert!(matches!(
                    channels.poll(&AtomicBool::new(false)),
                    Err(TryRecvError::Disconnected)
                ));
            }
        }
    }
}
