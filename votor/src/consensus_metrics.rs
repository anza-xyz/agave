use {
    agave_math_utils::welford_stats::WelfordStats,
    agave_votor_messages::{
        VoteAccountPubkeys,
        metric_types::{ConsensusMetricsEvent, ConsensusMetricsEventReceiver},
        vote::Vote,
    },
    crossbeam_channel::RecvTimeoutError,
    solana_clock::{Epoch, Slot},
    solana_metrics::datapoint_info,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank_forks::SharableBanks, leader_schedule_utils::first_of_consecutive_leader_slots,
    },
    std::{
        collections::{BTreeMap, BTreeSet, HashMap},
        num::Saturating,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

/// Number of epochs to retain metrics for (current + previous).
const EPOCHS_TO_RETAIN: u64 = 2;

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

/// Per-epoch metrics container.
#[derive(Debug, Default)]
struct EpochMetrics {
    /// Used to track this node's view of how the other nodes on the network are voting.
    node_metrics: HashMap<Pubkey, NodeVoteMetrics>,

    /// Tracks when replay completed when the given leader produced the block.
    leader_metrics: HashMap<Pubkey, WelfordStats>,

    /// Tracks when an event was received before start-of-window event.
    missing_start_of_window: Saturating<usize>,

    /// Tracks when parent ready event for the given `slot` was seen.
    parent_ready_seen: HashMap<Slot, Instant>,
}

/// Tracks various Consensus related metrics.
pub struct ConsensusMetrics {
    sharable_banks: SharableBanks,

    /// Per-epoch metrics storage.
    epoch_metrics: BTreeMap<Epoch, EpochMetrics>,

    /// Epochs that have already been emitted (to prevent duplicate emissions).
    emitted_epochs: BTreeSet<Epoch>,

    /// The highest finalized slot we've seen.
    highest_finalized_slot: Option<Slot>,

    /// Receiver for events.
    receiver: ConsensusMetricsEventReceiver,
}

impl ConsensusMetrics {
    fn new(sharable_banks: SharableBanks, receiver: ConsensusMetricsEventReceiver) -> Self {
        Self {
            epoch_metrics: BTreeMap::default(),
            emitted_epochs: BTreeSet::default(),
            highest_finalized_slot: None,
            sharable_banks,
            receiver,
        }
    }

    pub fn start_metrics_loop(
        sharable_banks: SharableBanks,
        receiver: ConsensusMetricsEventReceiver,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solVotorMetrics".into())
            .spawn(move || {
                let mut metrics = Self::new(sharable_banks, receiver);
                metrics.run(exit);
            })
            .expect("Failed to start consensus metrics thread")
    }

    fn run(&mut self, exit: Arc<AtomicBool>) {
        while !exit.load(Ordering::Relaxed) {
            match self.receiver.recv_timeout(Duration::from_secs(1)) {
                Ok((received, event)) => match event {
                    ConsensusMetricsEvent::Vote { ids, vote } => {
                        self.record_vote(ids, &vote, received);
                    }
                    ConsensusMetricsEvent::ReplayCompleted { leader, slot } => {
                        self.record_replay_completed(leader, slot, received);
                    }
                    ConsensusMetricsEvent::ParentReadySeen { slot } => {
                        self.record_parent_ready_seen(slot, received);
                    }
                    ConsensusMetricsEvent::SlotFinalized { slot } => {
                        self.handle_slot_finalized(slot);
                    }
                },
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
        let epoch = self.sharable_banks.root().epoch_schedule().get_epoch(slot);
        self.epoch_metrics.entry(epoch).or_default()
    }

    /// Computes start of slot based on when parent ready event was seen.
    fn compute_start_of_slot(&self, slot: Slot) -> Option<Instant> {
        let delta_block =
            Duration::from_nanos_u128(self.sharable_banks.root().ns_per_slot_at_slot(slot));
        let first_slot_in_window = first_of_consecutive_leader_slots(slot);
        let epoch = self.sharable_banks.root().epoch_schedule().get_epoch(slot);
        let start_of_window = self
            .epoch_metrics
            .get(&epoch)?
            .parent_ready_seen
            .get(&first_slot_in_window)?;
        Some(
            start_of_window
                .checked_add(
                    delta_block.saturating_mul(
                        u32::try_from(slot.saturating_sub(first_slot_in_window))
                            .expect("leader window must fit in u32"),
                    ),
                )
                .expect("leader window duration must fit"),
        )
    }

    /// Records a `vote` from the node with `id`.
    fn record_vote(&mut self, ids: VoteAccountPubkeys, vote: &Vote, received: Instant) {
        let vote_slot = vote.slot();
        let maybe_start_of_slot = self.compute_start_of_slot(vote_slot);
        let epoch_metrics = self.epoch_metrics_for_slot(vote_slot);
        let Some(start_of_slot) = maybe_start_of_slot else {
            epoch_metrics.missing_start_of_window += ids.as_slice().len();
            return;
        };
        let elapsed = received.duration_since(start_of_slot);
        let mut record_vote = |id| {
            let node = epoch_metrics.node_metrics.entry(id).or_default();
            node.record_vote(vote, elapsed);
        };
        match ids {
            VoteAccountPubkeys::Owned(ids) => ids.into_iter().for_each(&mut record_vote),
            VoteAccountPubkeys::Shared(ids) => {
                ids.iter().copied().for_each(&mut record_vote);
            }
        }
    }

    /// Records when a block for `slot` was seen and the `leader` is responsible for producing it.
    fn record_replay_completed(&mut self, leader: Pubkey, slot: Slot, received: Instant) {
        let maybe_start = self.compute_start_of_slot(slot);
        let epoch_metrics = self.epoch_metrics_for_slot(slot);
        let Some(start) = maybe_start else {
            epoch_metrics.missing_start_of_window += 1;
            return;
        };
        let elapsed = received.duration_since(start).as_micros();
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

    /// Records that a parent ready was seen.
    fn record_parent_ready_seen(&mut self, slot: Slot, received: Instant) {
        self.epoch_metrics_for_slot(slot)
            .parent_ready_seen
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
        let finalized_epoch = self
            .sharable_banks
            .root()
            .epoch_schedule()
            .get_epoch(highest_finalized);

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
            datapoint_info!("consensus_replay_completed_metrics",
                "address" => addr,
                ("epoch", epoch, i64),
                ("replay_completed_count", stats.count(), i64),
                ("replay_completed_us_mean", stats.mean::<i64>(), Option<i64>),
                ("replay_completed_us_stddev", stats.stddev::<i64>(), Option<i64>),
                ("replay_completed_us_maximum", stats.maximum::<i64>(), Option<i64>),
            );
        }

        datapoint_info!(
            "consensus_metrics_internals",
            ("epoch", epoch, i64),
            (
                "parent_ready_seen",
                epoch_metrics.parent_ready_seen.len(),
                i64
            ),
            (
                "missing_start_of_window",
                epoch_metrics.missing_start_of_window.0,
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
        crossbeam_channel::bounded,
        solana_epoch_schedule::EpochSchedule,
        solana_keypair::Keypair,
        solana_runtime::{bank::Bank, bank_forks::BankForks, genesis_utils::create_genesis_config},
        solana_signer::Signer,
    };

    fn new_metrics() -> ConsensusMetrics {
        let (_, rx) = bounded(1024);
        let mut genesis_config = create_genesis_config(10_000).genesis_config;
        // 100 slots/epoch
        genesis_config.epoch_schedule = EpochSchedule::custom(100, 100, false);
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        ConsensusMetrics::new(sharable_banks, rx)
    }

    #[test]
    fn test_vote_before_slot_start() {
        let mut metrics = new_metrics();

        metrics.record_vote(
            VoteAccountPubkeys::Owned(vec![Keypair::new().pubkey()]),
            &Vote::Skip(SkipVote { slot: 42 }),
            Instant::now(),
        );

        assert_eq!(metrics.epoch_metrics[&0].missing_start_of_window.0, 1);
    }

    #[test]
    fn test_vote_after_slot_start() {
        let mut metrics = new_metrics();
        let slot = 42;
        let first_slot_in_window = first_of_consecutive_leader_slots(slot);
        let pubkey = Keypair::new().pubkey();

        let start = Instant::now();
        let slot_duration =
            Duration::from_nanos_u128(metrics.sharable_banks.root().ns_per_slot_at_slot(slot));
        metrics.record_parent_ready_seen(first_slot_in_window, start);
        metrics.record_vote(
            VoteAccountPubkeys::Owned(vec![pubkey]),
            &Vote::Skip(SkipVote { slot }),
            start + slot_duration * 2 + Duration::from_millis(1),
        );

        let node = &metrics.epoch_metrics[&0].node_metrics[&pubkey];
        assert_eq!(node.skip.count(), 1);
        assert_eq!(node.skip.mean::<i64>(), Some(1_000));
        assert_eq!(metrics.epoch_metrics[&0].missing_start_of_window.0, 0);
    }

    #[test]
    fn test_out_of_order_epoch_replay() {
        let mut metrics = new_metrics();
        let t = Instant::now();

        metrics.record_parent_ready_seen(200, t);
        metrics.record_parent_ready_seen(100, t);

        assert!(metrics.epoch_metrics.contains_key(&1));
        assert!(metrics.epoch_metrics.contains_key(&2));
    }

    #[test]
    fn test_emit_on_next_epoch() {
        let mut metrics = new_metrics();

        metrics.record_parent_ready_seen(50, Instant::now());
        metrics.handle_slot_finalized(100);

        assert!(metrics.emitted_epochs.contains(&0));
    }

    #[test]
    fn test_no_emit_on_last_slot_of_same_epoch() {
        let mut metrics = new_metrics();

        metrics.record_parent_ready_seen(50, Instant::now());
        metrics.handle_slot_finalized(99);
        assert!(!metrics.emitted_epochs.contains(&0));

        metrics.handle_slot_finalized(100);
        assert!(metrics.emitted_epochs.contains(&0));
    }

    #[test]
    fn test_no_double_emit() {
        let mut metrics = new_metrics();

        metrics.record_parent_ready_seen(50, Instant::now());
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
            metrics.record_parent_ready_seen(ix * 100, Instant::now());
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
        let slot = 42;
        let first_slot_in_window = first_of_consecutive_leader_slots(slot);
        let leader = Keypair::new().pubkey();

        let start = Instant::now();
        let slot_duration =
            Duration::from_nanos_u128(metrics.sharable_banks.root().ns_per_slot_at_slot(slot));
        metrics.record_parent_ready_seen(first_slot_in_window, start);
        metrics.record_replay_completed(
            leader,
            slot,
            start + slot_duration * 2 + Duration::from_millis(1),
        );

        assert_eq!(metrics.epoch_metrics[&0].leader_metrics[&leader].count(), 1);
        assert_eq!(
            metrics.epoch_metrics[&0].leader_metrics[&leader].mean::<i64>(),
            Some(1_000)
        );
        assert_eq!(metrics.epoch_metrics[&0].missing_start_of_window.0, 0);
    }
}
