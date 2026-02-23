use {
    crate::welford_stats::WelfordStats,
    agave_votor_messages::vote::Vote,
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender},
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_metrics::datapoint_info,
    solana_pubkey::Pubkey,
    std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

/// Even at 10 events per slot, this supports 1000 slots in flight
/// With 2000 active validators, we can't have more than:
/// - 1 Notarize vote
/// - 3 Notarize-fallback votes
/// - 1 Skip-fallback vote
/// - 1 Finalize vote
///
/// Per validator, resulting in 12k vote events.
/// We overprovision this channel at 15k total events.
pub const MAX_IN_FLIGHT_CONSENSUS_EVENTS: usize = 15_000;

/// Number of epochs to retain metrics for (current + previous).
const EPOCHS_TO_RETAIN: u64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsensusMetricsEvent {
    /// A vote was received from the node with `id`.
    Vote { id: Pubkey, vote: Vote },
    /// A block hash was seen for `slot` and the `leader` is responsible for producing it.
    BlockHashSeen { leader: Pubkey, slot: Slot },
    /// Start of slot
    StartOfSlot { slot: Slot },
    /// A slot was finalized
    SlotFinalized { slot: Slot },
}

pub type ConsensusMetricsEventSender = Sender<(Instant, Vec<ConsensusMetricsEvent>)>;
pub type ConsensusMetricsEventReceiver = Receiver<(Instant, Vec<ConsensusMetricsEvent>)>;

/// Tracks all [`Vote`] metrics for a given node.
#[derive(Debug, Default)]
struct NodeVoteMetrics {
    notar: WelfordStats,
    notar_fallback: WelfordStats,
    skip: WelfordStats,
    skip_fallback: WelfordStats,
    final_: WelfordStats,
}

impl NodeVoteMetrics {
    /// Records metrics for when `vote` was received after `elapsed` time has passed since the start of the slot.
    fn record_vote(&mut self, vote: &Vote, elapsed: Duration) {
        let elapsed = elapsed.as_micros();
        let elapsed = match elapsed.try_into() {
            Ok(e) => e,
            Err(err) => {
                warn!(
                    "recording duration {elapsed} for vote {vote:?}: conversion to u64 failed \
                     with {err}"
                );
                return;
            }
        };
        match vote {
            Vote::Notarize(_) => self.notar.add_sample(elapsed),
            Vote::NotarizeFallback(_) => self.notar_fallback.add_sample(elapsed),
            Vote::Skip(_) => self.skip.add_sample(elapsed),
            Vote::SkipFallback(_) => self.skip_fallback.add_sample(elapsed),
            Vote::Finalize(_) => self.final_.add_sample(elapsed),
            Vote::Genesis(_) => (), // Only for migration, tracked elsewhere
        }
    }
}

/// Errors returned from [`ConsensusMetrics::record_vote`].
#[derive(Debug)]
pub enum RecordVoteError {
    /// Could not find start of slot entry.
    SlotNotFound,
}

/// Errors returned from [`ConsensusMetrics::record_block_hash_seen`].
#[derive(Debug)]
pub enum RecordBlockHashError {
    /// Could not find start of slot entry.
    SlotNotFound,
}

/// Per-epoch metrics container.
#[derive(Debug, Default)]
struct EpochMetrics {
    /// Used to track this node's view of how the other nodes on the network are voting.
    node_metrics: BTreeMap<Pubkey, NodeVoteMetrics>,

    /// Used to track when this node received blocks from different leaders in the network.
    leader_metrics: BTreeMap<Pubkey, WelfordStats>,

    /// Counts number of times metrics recording failed.
    metrics_recording_failed: usize,

    /// Tracks when individual slots began.
    ///
    /// Relies on [`TimerManager`] to notify of start of slots.
    /// The manager uses parent ready event and timeouts as per the Alpenglow protocol to determine start of slots.
    start_of_slot: BTreeMap<Slot, Instant>,
}

/// Tracks various Consensus related metrics.
pub struct ConsensusMetrics {
    /// Per-epoch metrics storage.
    epoch_metrics: BTreeMap<Epoch, EpochMetrics>,

    /// Epochs that have already been emitted (to prevent duplicate emissions).
    emitted_epochs: BTreeSet<Epoch>,

    /// The highest finalized slot we've seen.
    highest_finalized_slot: Option<Slot>,

    /// Epoch schedule for computing epoch boundaries.
    epoch_schedule: EpochSchedule,

    /// Receiver for events.
    receiver: ConsensusMetricsEventReceiver,
}

impl ConsensusMetrics {
    fn new(epoch_schedule: EpochSchedule, receiver: ConsensusMetricsEventReceiver) -> Self {
        Self {
            epoch_metrics: BTreeMap::default(),
            emitted_epochs: BTreeSet::default(),
            highest_finalized_slot: None,
            epoch_schedule,
            receiver,
        }
    }

    pub fn start_metrics_loop(
        epoch_schedule: EpochSchedule,
        receiver: ConsensusMetricsEventReceiver,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solConsMetrics".into())
            .spawn(move || {
                let mut metrics = Self::new(epoch_schedule, receiver);
                metrics.run(exit);
            })
            .expect("Failed to start consensus metrics thread")
    }

    fn run(&mut self, exit: Arc<AtomicBool>) {
        while !exit.load(Ordering::Relaxed) {
            match self.receiver.recv_timeout(Duration::from_secs(1)) {
                Ok((received, events)) => {
                    for event in events {
                        match event {
                            ConsensusMetricsEvent::Vote { id, vote } => {
                                self.record_vote(id, &vote, received);
                            }
                            ConsensusMetricsEvent::BlockHashSeen { leader, slot } => {
                                self.record_block_hash_seen(leader, slot, received);
                            }
                            ConsensusMetricsEvent::StartOfSlot { slot } => {
                                self.record_start_of_slot(slot, received);
                            }
                            ConsensusMetricsEvent::SlotFinalized { slot } => {
                                self.handle_slot_finalized(slot);
                            }
                        }
                    }
                }
                Err(err) => match err {
                    RecvTimeoutError::Timeout => trace!("ConsensusMetricsEventReceiver timeout"),
                    RecvTimeoutError::Disconnected => {
                        warn!("ConsensusMetricsEventReceiver disconnected, exiting loop");
                        return;
                    }
                },
            }
        }
    }

    fn epoch_metrics_for_slot(&mut self, slot: Slot) -> &mut EpochMetrics {
        let epoch = self.epoch_schedule.get_epoch(slot);
        self.epoch_metrics.entry(epoch).or_default()
    }

    /// Records a `vote` from the node with `id`.
    fn record_vote(&mut self, id: Pubkey, vote: &Vote, received: Instant) {
        let slot = vote.slot();
        let epoch_metrics = self.epoch_metrics_for_slot(slot);

        let Some(start) = epoch_metrics.start_of_slot.get(&slot) else {
            epoch_metrics.metrics_recording_failed =
                epoch_metrics.metrics_recording_failed.saturating_add(1);
            return;
        };
        let node = epoch_metrics.node_metrics.entry(id).or_default();
        let elapsed = received.duration_since(*start);
        node.record_vote(vote, elapsed);
    }

    /// Records when a block for `slot` was seen and the `leader` is responsible for producing it.
    fn record_block_hash_seen(&mut self, leader: Pubkey, slot: Slot, received: Instant) {
        let epoch_metrics = self.epoch_metrics_for_slot(slot);

        let Some(start) = epoch_metrics.start_of_slot.get(&slot) else {
            epoch_metrics.metrics_recording_failed =
                epoch_metrics.metrics_recording_failed.saturating_add(1);
            return;
        };
        let elapsed = received.duration_since(*start).as_micros();
        let elapsed = match elapsed.try_into() {
            Ok(e) => e,
            Err(err) => {
                warn!(
                    "recording duration {elapsed} for block hash for slot {slot}: conversion to \
                     u64 failed with {err}"
                );
                return;
            }
        };
        epoch_metrics
            .leader_metrics
            .entry(leader)
            .or_default()
            .add_sample(elapsed);
    }

    /// Records when a given slot started.
    fn record_start_of_slot(&mut self, slot: Slot, received: Instant) {
        self.epoch_metrics_for_slot(slot)
            .start_of_slot
            .entry(slot)
            .or_insert(received);
    }

    /// Handles a slot finalization event.
    fn handle_slot_finalized(&mut self, finalized_slot: Slot) {
        let current = self.highest_finalized_slot.unwrap_or(0);
        self.highest_finalized_slot = Some(current.max(finalized_slot));
        self.maybe_emit_completed_epochs();
    }

    /// Checks if any epochs are ready to be emitted and emits them.
    fn maybe_emit_completed_epochs(&mut self) {
        let Some(highest_finalized) = self.highest_finalized_slot else {
            return;
        };
        let finalized_epoch = self.epoch_schedule.get_epoch(highest_finalized);

        for (&epoch, epoch_metrics) in &self.epoch_metrics {
            if !self.emitted_epochs.contains(&epoch) && finalized_epoch > epoch {
                Self::emit_epoch_metrics(epoch, epoch_metrics);
                self.emitted_epochs.insert(epoch);
            }
        }

        self.cleanup_old_epochs(finalized_epoch);
    }

    /// Emits metrics for the given epoch.
    fn emit_epoch_metrics(epoch: Epoch, epoch_metrics: &EpochMetrics) {
        for (addr, metrics) in &epoch_metrics.node_metrics {
            let addr = addr.to_string();
            datapoint_info!("consensus_vote_metrics",
                "address" => addr,
                ("epoch", epoch, i64),
                ("notar_vote_count", metrics.notar.count(), i64),
                ("notar_vote_us_mean", metrics.notar.mean::<i64>(), Option<i64>),
                ("notar_vote_us_stddev", metrics.notar.stddev::<i64>(), Option<i64>),
                ("notar_vote_us_maximum", metrics.notar.maximum::<i64>(), Option<i64>),

                ("notar_fallback_vote_count", metrics.notar_fallback.count(), i64),
                ("notar_fallback_vote_us_mean", metrics.notar_fallback.mean::<i64>(), Option<i64>),
                ("notar_fallback_vote_us_stddev", metrics.notar_fallback.stddev::<i64>(), Option<i64>),
                ("notar_fallback_vote_us_maximum", metrics.notar_fallback.maximum::<i64>(), Option<i64>),

                ("skip_vote_count", metrics.skip.count(), i64),
                ("skip_vote_us_mean", metrics.skip.mean::<i64>(), Option<i64>),
                ("skip_vote_us_stddev", metrics.skip.stddev::<i64>(), Option<i64>),
                ("skip_vote_us_maximum", metrics.skip.maximum::<i64>(), Option<i64>),

                ("skip_fallback_vote_count", metrics.skip_fallback.count(), i64),
                ("skip_fallback_vote_us_mean", metrics.skip_fallback.mean::<i64>(), Option<i64>),
                ("skip_fallback_vote_us_stddev", metrics.skip_fallback.stddev::<i64>(), Option<i64>),
                ("skip_fallback_vote_us_maximum", metrics.skip_fallback.maximum::<i64>(), Option<i64>),

                ("finalize_vote_count", metrics.final_.count(), i64),
                ("finalize_vote_us_mean", metrics.final_.mean::<i64>(), Option<i64>),
                ("finalize_vote_us_stddev", metrics.final_.stddev::<i64>(), Option<i64>),
                ("finalize_vote_us_maximum", metrics.final_.maximum::<i64>(), Option<i64>),
            );
        }

        for (addr, stats) in &epoch_metrics.leader_metrics {
            let addr = addr.to_string();
            datapoint_info!("consensus_block_hash_seen_metrics",
                "address" => addr,
                ("epoch", epoch, i64),
                ("block_hash_seen_count", stats.count(), i64),
                ("block_hash_seen_us_mean", stats.mean::<i64>(), Option<i64>),
                ("block_hash_seen_us_stddev", stats.stddev::<i64>(), Option<i64>),
                ("block_hash_seen_us_maximum", stats.maximum::<i64>(), Option<i64>),
            );
        }

        datapoint_info!(
            "consensus_metrics_internals",
            ("epoch", epoch, i64),
            (
                "start_of_slot_count",
                epoch_metrics.start_of_slot.len(),
                i64
            ),
            (
                "metrics_recording_failed",
                epoch_metrics.metrics_recording_failed,
                i64
            ),
        );
    }

    /// Cleans up old epoch data to prevent unbounded memory growth.
    fn cleanup_old_epochs(&mut self, finalized_epoch: Epoch) {
        let cutoff_epoch = finalized_epoch.saturating_sub(EPOCHS_TO_RETAIN);
        self.epoch_metrics = self.epoch_metrics.split_off(&cutoff_epoch);
        self.emitted_epochs = self.emitted_epochs.split_off(&cutoff_epoch);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_votor_messages::vote::{SkipVote, Vote},
        crossbeam_channel::unbounded,
        solana_keypair::Keypair,
        solana_signer::Signer,
        std::thread::sleep,
    };

    fn new_metrics() -> ConsensusMetrics {
        let (_, rx) = unbounded();
        ConsensusMetrics::new(EpochSchedule::custom(100, 100, false), rx) // 100 slots/epoch
    }

    #[test]
    fn test_vote_before_slot_start() {
        let mut metrics = new_metrics();

        metrics.record_vote(
            Keypair::new().pubkey(),
            &Vote::Skip(SkipVote { slot: 42 }),
            Instant::now(),
        );

        assert_eq!(metrics.epoch_metrics[&0].metrics_recording_failed, 1);
    }

    #[test]
    fn test_vote_after_slot_start() {
        let mut metrics = new_metrics();
        let pubkey = Keypair::new().pubkey();

        metrics.record_start_of_slot(42, Instant::now());
        sleep(Duration::from_millis(1));
        metrics.record_vote(pubkey, &Vote::Skip(SkipVote { slot: 42 }), Instant::now());

        let node = &metrics.epoch_metrics[&0].node_metrics[&pubkey];
        assert_eq!(node.skip.count(), 1);
        assert!(node.skip.mean::<i64>().unwrap() > 0);
    }

    #[test]
    fn test_out_of_order_epoch_replay() {
        let mut metrics = new_metrics();
        let t = Instant::now();

        metrics.record_start_of_slot(200, t);
        metrics.record_start_of_slot(100, t);

        assert!(metrics.epoch_metrics.contains_key(&1));
        assert!(metrics.epoch_metrics.contains_key(&2));
    }

    #[test]
    fn test_emit_on_next_epoch() {
        let mut metrics = new_metrics();

        metrics.record_start_of_slot(50, Instant::now());
        metrics.handle_slot_finalized(100);

        assert!(metrics.emitted_epochs.contains(&0));
    }

    #[test]
    fn test_no_emit_on_last_slot_of_same_epoch() {
        let mut metrics = new_metrics();

        metrics.record_start_of_slot(50, Instant::now());
        metrics.handle_slot_finalized(99);
        assert!(!metrics.emitted_epochs.contains(&0));

        metrics.handle_slot_finalized(100);
        assert!(metrics.emitted_epochs.contains(&0));
    }

    #[test]
    fn test_no_double_emit() {
        let mut metrics = new_metrics();

        metrics.record_start_of_slot(50, Instant::now());
        metrics.handle_slot_finalized(100);
        let count_before = metrics.emitted_epochs.iter().filter(|&&e| e == 0).count();

        metrics.handle_slot_finalized(101);
        metrics.handle_slot_finalized(102);

        assert_eq!(
            metrics.emitted_epochs.iter().filter(|&&e| e == 0).count(),
            count_before
        );
    }

    #[test]
    fn test_cleanup_old_epochs() {
        let mut metrics = new_metrics();

        for ix in 0u64..5 {
            metrics.record_start_of_slot(ix * 100, Instant::now());
        }
        metrics.handle_slot_finalized(400);

        assert!(!metrics.epoch_metrics.contains_key(&0));
        assert!(!metrics.epoch_metrics.contains_key(&1));
        assert!(metrics.epoch_metrics.contains_key(&2));
    }

    #[test]
    fn test_finalize_keeps_max() {
        let mut metrics = new_metrics();

        metrics.handle_slot_finalized(200);
        metrics.handle_slot_finalized(50);

        assert_eq!(metrics.highest_finalized_slot, Some(200));
    }

    #[test]
    fn test_block_hash_seen() {
        let mut metrics = new_metrics();
        let leader = Keypair::new().pubkey();

        metrics.record_start_of_slot(42, Instant::now());
        metrics.record_block_hash_seen(leader, 42, Instant::now());

        assert_eq!(metrics.epoch_metrics[&0].leader_metrics[&leader].count(), 1);
    }
}
