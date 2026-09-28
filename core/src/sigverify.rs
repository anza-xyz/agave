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
        Receiver as WakeReceiver, RecvError, Sender as WakeSender, TryRecvError, WakeEvent,
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
    worker_sender: WakeSender<GossipVerifyTask>,
}

impl GossipSigVerifier {
    #[cfg(test)]
    pub(crate) fn new_for_tests(worker_sender: WakeSender<GossipVerifyTask>) -> Self {
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

/// The input channels a worker drains. A worker polls them in `INPUTS` order, starting one past
/// the input that last yielded, so a busy input cannot monopolise it; see
/// [`WorkerPoolChannels::recv`].
#[derive(Clone, Copy)]
enum Input {
    TpuVote,
    Gossip,
    NonVote,
}

const INPUTS: [Input; 3] = [Input::TpuVote, Input::Gossip, Input::NonVote];

/// One unit of work, tagged with the input it came from.
enum SigVerifyWork {
    TpuVote(PacketBatch),
    Gossip(GossipVerifyTask),
    NonVote(PacketBatch),
}

#[derive(Clone)]
struct WorkerPoolChannels {
    /// Shared by all three input channels; producers wake sleeping workers through it.
    wake_event: Arc<WakeEvent>,
    /// Index into `INPUTS` where the next poll starts.
    next_input: usize,
    non_vote_receiver: WakeReceiver<PacketBatch>,
    tpu_vote_receiver: WakeReceiver<PacketBatch>,
    gossip_receiver: WakeReceiver<GossipVerifyTask>,
    gossip_verified_vote_sender: Sender<GossipVerifiedVoteBatch>,
    forward_stage_sender: Sender<(BankingPacketBatch, bool)>,
    sharable_banks: SharableBanks,
    non_vote_state: SigVerifyWorkerState,
    tpu_vote_state: SigVerifyWorkerState,
}

impl WorkerPoolChannels {
    /// Blocks until an input yields work. Each poll tries every input once, starting at
    /// `INPUTS[self.next_input]`, and moves the start past the input that yielded. A disconnected
    /// input, or the pool's exit flag, ends the worker.
    fn recv(&mut self, exit: &AtomicBool) -> Result<SigVerifyWork, RecvError> {
        // The closure uses fields, not `&self` methods, so it does not borrow `self.wake_event`.
        self.wake_event.recv_with(|| {
            if exit.load(Ordering::Relaxed) {
                return Err(TryRecvError::Disconnected);
            }
            let start = self.next_input;
            for offset in 0..INPUTS.len() {
                let index = (start + offset) % INPUTS.len();
                let received = match INPUTS[index] {
                    Input::TpuVote => self
                        .tpu_vote_receiver
                        .try_recv()
                        .map(SigVerifyWork::TpuVote),
                    Input::Gossip => self.gossip_receiver.try_recv().map(SigVerifyWork::Gossip),
                    Input::NonVote => self
                        .non_vote_receiver
                        .try_recv()
                        .map(SigVerifyWork::NonVote),
                };
                match received {
                    Ok(work) => {
                        self.next_input = (index + 1) % INPUTS.len();
                        return Ok(work);
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => return Err(TryRecvError::Disconnected),
                }
            }
            Err(TryRecvError::Empty)
        })
    }
}

pub(crate) struct SigVerifyWorkerPool {
    exit: Arc<AtomicBool>,
    wake_event: Arc<WakeEvent>,
    gossip_sender: WakeSender<GossipVerifyTask>,
    worker_hdls: Vec<JoinHandle<()>>,
}

impl Drop for SigVerifyWorkerPool {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        // Signal the workers to exit.
        self.wake_event.wake_all();
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
        non_vote_receiver: WakeReceiver<PacketBatch>,
        tpu_vote_receiver: WakeReceiver<PacketBatch>,
        senders: SigVerifyWorkerSenders,
        forward_non_votes: bool,
        sharable_banks: SharableBanks,
        non_vote_state: SigVerifyWorkerState,
        tpu_vote_state: SigVerifyWorkerState,
    ) -> Self {
        let wake_event = Arc::clone(non_vote_receiver.wake_event());
        // A channel on another event would fill up without ever waking a sleeping worker.
        assert!(
            Arc::ptr_eq(&wake_event, tpu_vote_receiver.wake_event()),
            "sigverify input channels must share one wake event"
        );
        let (gossip_sender, gossip_receiver) = agave_wake_channel::bounded_with_wake_event(
            SIGVERIFY_GOSSIP_VOTE_WORK_CHANNEL_SIZE,
            Arc::clone(&wake_event),
        );
        let channels = WorkerPoolChannels {
            wake_event: Arc::clone(&wake_event),
            next_input: 0,
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
            wake_event,
            gossip_sender,
            worker_hdls,
        }
    }

    pub(crate) fn gossip_verifier(&self) -> GossipSigVerifier {
        GossipSigVerifier {
            worker_sender: self.gossip_sender.clone(),
        }
    }

    fn worker(exit: Arc<AtomicBool>, mut channels: WorkerPoolChannels, forward_non_votes: bool) {
        while !exit.load(Ordering::Relaxed) {
            if !Self::worker_iteration(&mut channels, &exit, forward_non_votes) {
                break;
            }
        }
    }

    /// Returns false if the pool is exiting or some channel connection is disconnected.
    fn worker_iteration(
        channels: &mut WorkerPoolChannels,
        exit: &AtomicBool,
        forward_non_votes: bool,
    ) -> bool {
        match channels.recv(exit) {
            Ok(SigVerifyWork::NonVote(batch)) => Self::run_transaction_task(
                batch,
                false,
                &channels.forward_stage_sender,
                forward_non_votes,
                false,
                &channels.sharable_banks,
                &channels.non_vote_state,
            ),
            Ok(SigVerifyWork::TpuVote(batch)) => Self::run_transaction_task(
                batch,
                true,
                &channels.forward_stage_sender,
                true,
                true,
                &channels.sharable_banks,
                &channels.tpu_vote_state,
            ),
            Ok(SigVerifyWork::Gossip(work)) => {
                Self::run_gossip_task(work, &channels.gossip_verified_vote_sender)
            }
            Err(RecvError) => false,
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
        super::*, crate::banking_trace::BankingTracer, crossbeam_channel::Receiver,
        solana_perf::packet::BytesPacketBatch,
        solana_runtime::genesis_utils::create_genesis_config,
    };

    fn test_state() -> SigVerifyWorkerState {
        SigVerifyWorkerState::new(
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
        )
    }

    fn test_channels() -> (
        WorkerPoolChannels,
        WakeSender<PacketBatch>,
        WakeSender<PacketBatch>,
        WakeSender<GossipVerifyTask>,
        Receiver<GossipVerifiedVoteBatch>,
    ) {
        let wake_event = Arc::new(WakeEvent::default());
        let (non_vote_sender, non_vote_receiver) =
            agave_wake_channel::bounded_with_wake_event(4, wake_event.clone());
        let (tpu_vote_sender, tpu_vote_receiver) =
            agave_wake_channel::bounded_with_wake_event(4, wake_event.clone());
        let (gossip_sender, gossip_receiver) =
            agave_wake_channel::bounded_with_wake_event(4, wake_event.clone());
        let (gossip_verified_vote_sender, gossip_verified_vote_receiver) =
            crossbeam_channel::unbounded();
        let (_, bank_forks) =
            Bank::new_with_bank_forks_for_tests(&create_genesis_config(1).genesis_config);
        let channels = WorkerPoolChannels {
            wake_event,
            next_input: 0,
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_receiver,
            gossip_verified_vote_sender,
            forward_stage_sender: crossbeam_channel::unbounded().0,
            sharable_banks: bank_forks.read().unwrap().sharable_banks(),
            non_vote_state: test_state(),
            tpu_vote_state: test_state(),
        };
        (
            channels,
            non_vote_sender,
            tpu_vote_sender,
            gossip_sender,
            gossip_verified_vote_receiver,
        )
    }

    fn empty_batch() -> PacketBatch {
        PacketBatch::Bytes(BytesPacketBatch::default())
    }

    fn enqueue_inputs(
        non_vote_sender: &WakeSender<PacketBatch>,
        tpu_vote_sender: &WakeSender<PacketBatch>,
        gossip_sender: &WakeSender<GossipVerifyTask>,
    ) {
        non_vote_sender.try_send(empty_batch()).unwrap();
        tpu_vote_sender.try_send(empty_batch()).unwrap();
        gossip_sender
            .try_send(GossipVerifyTask {
                batch: empty_batch(),
                transaction: Transaction::default(),
            })
            .unwrap();
    }

    /// Items each input's task has processed so far, in `INPUTS` order.
    fn processed(
        channels: &WorkerPoolChannels,
        gossip_verified_vote_receiver: &Receiver<GossipVerifiedVoteBatch>,
    ) -> [usize; 3] {
        let batches =
            |state: &SigVerifyWorkerState| state.stats.total_batches.load(Ordering::Relaxed);
        [
            batches(&channels.tpu_vote_state),
            gossip_verified_vote_receiver.len(),
            batches(&channels.non_vote_state),
        ]
    }

    #[test]
    fn test_worker_iteration_each_input() {
        let (mut channels, non_vote_sender, tpu_vote_sender, gossip_sender, verified_receiver) =
            test_channels();
        let exit = AtomicBool::new(false);
        // One item per input: successive iterations process them in input order.
        enqueue_inputs(&non_vote_sender, &tpu_vote_sender, &gossip_sender);
        for expected in [[1, 0, 0], [1, 1, 0], [1, 1, 1]] {
            assert!(SigVerifyWorkerPool::worker_iteration(
                &mut channels,
                &exit,
                false
            ));
            assert_eq!(processed(&channels, &verified_receiver), expected);
        }

        // The rotation now starts at the vote input again; an iteration must skip the two empty
        // inputs ahead of the only one holding data.
        assert_eq!(channels.next_input, 0);
        non_vote_sender.try_send(empty_batch()).unwrap();
        assert!(SigVerifyWorkerPool::worker_iteration(
            &mut channels,
            &exit,
            false
        ));
        assert_eq!(processed(&channels, &verified_receiver), [1, 1, 2]);
    }

    #[test]
    fn test_worker_iteration_rotates_across_backlogged_inputs() {
        let (mut channels, non_vote_sender, tpu_vote_sender, gossip_sender, verified_receiver) =
            test_channels();
        let exit = AtomicBool::new(false);
        for _ in 0..3 {
            enqueue_inputs(&non_vote_sender, &tpu_vote_sender, &gossip_sender);
        }
        // With every input backlogged, each round of iterations must take one item from every
        // input rather than draining one input first.
        for round in 1..=3 {
            for _ in 0..INPUTS.len() {
                assert!(SigVerifyWorkerPool::worker_iteration(
                    &mut channels,
                    &exit,
                    false
                ));
            }
            assert_eq!(processed(&channels, &verified_receiver), [round; 3]);
        }

        // Shutdown takes precedence even while work remains queued.
        enqueue_inputs(&non_vote_sender, &tpu_vote_sender, &gossip_sender);
        exit.store(true, Ordering::Relaxed);
        assert!(!SigVerifyWorkerPool::worker_iteration(
            &mut channels,
            &exit,
            false
        ));
        assert_eq!(processed(&channels, &verified_receiver), [3; 3]);
    }

    #[test]
    fn test_worker_iteration_disconnected_input() {
        for disconnected_input in INPUTS {
            let (mut channels, non_vote_sender, tpu_vote_sender, gossip_sender, _) =
                test_channels();
            match disconnected_input {
                Input::NonVote => drop(non_vote_sender),
                Input::TpuVote => drop(tpu_vote_sender),
                Input::Gossip => drop(gossip_sender),
            }
            // Whichever input the rotation starts at, the disconnect must end the worker.
            for first_input in 0..INPUTS.len() {
                channels.next_input = first_input;
                assert!(!SigVerifyWorkerPool::worker_iteration(
                    &mut channels,
                    &AtomicBool::new(false),
                    false
                ));
            }
        }
    }
}
