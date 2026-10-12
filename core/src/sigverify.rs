//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.

use {
    crate::{
        banking_trace::BankingPacketSender, transaction_priority::calculate_priority_from_bytes,
    },
    agave_banking_stage_ingress_types::{BankingPacketBatch, SchedulerPriorityFloor},
    crossbeam_channel::{Receiver, Sender, TrySendError},
    solana_measure::measure_us,
    solana_perf::{
        deduper::{self, Deduper},
        packet::PacketBatch,
        sigverify::{self},
    },
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    std::{
        num::NonZeroUsize,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread::JoinHandle,
        time::Duration,
    },
};

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

#[derive(Clone)]
struct WorkerPoolChannels {
    packet_receiver: Receiver<PacketBatch>,
    forward_stage_sender: Sender<BankingPacketBatch>,
    sharable_banks: SharableBanks,
    worker_state: SigVerifyWorkerState,
}

pub(crate) struct SigVerifyWorkerPool {
    exit: Arc<AtomicBool>,
    worker_hdls: Vec<JoinHandle<()>>,
}

impl Drop for SigVerifyWorkerPool {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
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
        packet_receiver: Receiver<PacketBatch>,
        forward_stage_sender: Sender<BankingPacketBatch>,
        forward_transactions: bool,
        sharable_banks: SharableBanks,
        worker_state: SigVerifyWorkerState,
    ) -> Self {
        let channels = WorkerPoolChannels {
            packet_receiver,
            forward_stage_sender,
            sharable_banks,
            worker_state,
        };
        let exit = Arc::new(AtomicBool::new(false));
        let worker_hdls = (0..num_workers.get())
            .map(|idx| {
                let exit = exit.clone();
                let channels = channels.clone();

                std::thread::Builder::new()
                    .name(format!("solSigVerify{idx:02}"))
                    .spawn(move || Self::worker(exit, channels, forward_transactions))
                    .expect("failed to spawn sigverify worker thread")
            })
            .collect();
        Self { exit, worker_hdls }
    }

    fn worker(exit: Arc<AtomicBool>, channels: WorkerPoolChannels, forward_transactions: bool) {
        while !exit.load(Ordering::Relaxed) {
            if !Self::worker_iteration(&channels, forward_transactions) {
                break;
            }
        }
    }

    /// Returns false if some channel connection is disconnected.
    fn worker_iteration(channels: &WorkerPoolChannels, forward_transactions: bool) -> bool {
        crossbeam_channel::select! {
            recv(&channels.packet_receiver) -> maybe_work => {
                match maybe_work {
                    Ok(batch) => Self::run_transaction_task(
                        batch,
                        &channels.forward_stage_sender,
                        forward_transactions,
                        &channels.sharable_banks,
                        &channels.worker_state,
                    ),
                    Err(_) => false,
                }
            }
            default(Duration::from_millis(10)) => { true }
        }
    }

    fn run_transaction_task(
        mut batch: PacketBatch,
        forward_stage_sender: &Sender<BankingPacketBatch>,
        should_forward: bool,
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

        let (_, verify_time_us) = measure_us!(sigverify::ed25519_verify_serial(&mut batch));
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
            Self::try_forward(forward_stage_sender, banking_packet_batch);
        }

        true
    }

    fn try_forward(
        forward_stage_sender: &Sender<BankingPacketBatch>,
        banking_packet_batch: BankingPacketBatch,
    ) {
        if let Err(TrySendError::Full(_)) = forward_stage_sender.try_send(banking_packet_batch) {
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
    for packet in batch.iter_mut() {
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
