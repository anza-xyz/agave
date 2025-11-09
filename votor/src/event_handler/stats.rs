use {
    crate::{common::VoteType, event::VotorEvent, voting_service::BLSOp},
    agave_votor_messages::consensus_message::ConsensusMessage,
    solana_clock::Slot,
    solana_metrics::datapoint_info,
    std::{
        collections::BTreeMap,
        time::{Duration, Instant},
    },
};

const STATS_REPORT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct SlotTracking {
    /// The time when the slot tracking started
    start: Instant,
    /// Duration in microseconds from start to when the first shred for this
    /// slot was received
    first_shred: Option<i64>,
    /// Duration in microseconds from start to when the parent block for this
    /// slot was ready
    parent_ready: Option<i64>,
    /// Duration in microseconds from start to when the notarization vote for
    /// this slot was sent
    vote_notarize: Option<i64>,
    /// Duration in microseconds from start to when the skip vote for this slot
    /// was sent
    vote_skip: Option<i64>,
    /// If the slot was finalized, this is the duration in microseconds from
    /// start to when it was finalized, and the bool indicates if it was fast
    /// finalized
    finalized: Option<(i64, bool)>,
}

impl Default for SlotTracking {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            first_shred: None,
            parent_ready: None,
            vote_notarize: None,
            vote_skip: None,
            finalized: None,
        }
    }
}

impl SlotTracking {
    fn finalized_elapsed_micros(&self) -> Option<i64> {
        self.finalized.map(|(t, _fast_finalize)| t)
    }

    fn is_fast_finalized(&self) -> Option<bool> {
        self.finalized.map(|(_t, fast_finalize)| fast_finalize)
    }

    fn report(&self, slot: Slot) {
        datapoint_info!(
            "event_handler_slot_tracking",
            ("slot", slot as i64, i64),
            (
                "first_shred",
                self.first_shred,
                Option<i64>
            ),
            (
                "parent_ready",
                self.parent_ready,
                Option<i64>
            ),
            (
                "vote_notarize",
                self.vote_notarize,
                Option<i64>
            ),
            (
                "vote_skip",
                self.vote_skip,
                Option<i64>
            ),
            (
                "finalized",
                self.finalized_elapsed_micros(),
                Option<i64>
            ),
            ("is_fast_finalization", self.is_fast_finalized(), Option<bool>)
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatsEvent {
    Block,
    BlockNotarized,
    FirstShred,
    ParentReady,
    TimeoutCrashedLeader,
    Timeout,
    SafeToNotar,
    SafeToSkip,
    ProduceWindow,
    Finalized,
    Standstill,
    SetIdentity,
}

#[derive(Debug, Default)]
struct EventCountAndTime {
    count: usize,
    time_us: u64,
}

#[derive(Debug, Default)]
struct ReceivedEventsStats {
    block: EventCountAndTime,
    block_notarized: EventCountAndTime,
    first_shred: EventCountAndTime,
    parent_ready: EventCountAndTime,
    timeout_crashed_leader: EventCountAndTime,
    timeout: EventCountAndTime,
    safe_to_notar: EventCountAndTime,
    safe_to_skip: EventCountAndTime,
    produce_window: EventCountAndTime,
    finalized: EventCountAndTime,
    standstill: EventCountAndTime,
    set_identity: EventCountAndTime,
}

impl ReceivedEventsStats {
    fn incr_event_with_timing(&mut self, stats_event: StatsEvent, time_us: u64) {
        match stats_event {
            StatsEvent::Block => {
                self.block.count = self.block.count.saturating_add(1);
                self.block.time_us = self.block.time_us.saturating_add(time_us);
            }
            StatsEvent::BlockNotarized => {
                self.block_notarized.count = self.block_notarized.count.saturating_add(1);
                self.block_notarized.time_us = self.block_notarized.time_us.saturating_add(time_us);
            }
            StatsEvent::FirstShred => {
                self.first_shred.count = self.first_shred.count.saturating_add(1);
                self.first_shred.time_us = self.first_shred.time_us.saturating_add(time_us);
            }
            StatsEvent::ParentReady => {
                self.parent_ready.count = self.parent_ready.count.saturating_add(1);
                self.parent_ready.time_us = self.parent_ready.time_us.saturating_add(time_us);
            }
            StatsEvent::TimeoutCrashedLeader => {
                self.timeout_crashed_leader.count =
                    self.timeout_crashed_leader.count.saturating_add(1);
                self.timeout_crashed_leader.time_us =
                    self.timeout_crashed_leader.time_us.saturating_add(time_us);
            }
            StatsEvent::Timeout => {
                self.timeout.count = self.timeout.count.saturating_add(1);
                self.timeout.time_us = self.timeout.time_us.saturating_add(time_us);
            }
            StatsEvent::SafeToNotar => {
                self.safe_to_notar.count = self.safe_to_notar.count.saturating_add(1);
                self.safe_to_notar.time_us = self.safe_to_notar.time_us.saturating_add(time_us);
            }
            StatsEvent::SafeToSkip => {
                self.safe_to_skip.count = self.safe_to_skip.count.saturating_add(1);
                self.safe_to_skip.time_us = self.safe_to_skip.time_us.saturating_add(time_us);
            }
            StatsEvent::ProduceWindow => {
                self.produce_window.count = self.produce_window.count.saturating_add(1);
                self.produce_window.time_us = self.produce_window.time_us.saturating_add(time_us);
            }
            StatsEvent::Finalized => {
                self.finalized.count = self.finalized.count.saturating_add(1);
                self.finalized.time_us = self.finalized.time_us.saturating_add(time_us);
            }
            StatsEvent::Standstill => {
                self.standstill.count = self.standstill.count.saturating_add(1);
                self.standstill.time_us = self.standstill.time_us.saturating_add(time_us);
            }
            StatsEvent::SetIdentity => {
                self.set_identity.count = self.set_identity.count.saturating_add(1);
                self.set_identity.time_us = self.set_identity.time_us.saturating_add(time_us);
            }
        }
    }

    fn report(&self) {
        datapoint_info!(
            "event_handler_received_event_count_and_timing",
            ("block_count", self.block.count as i64, i64),
            ("block_elapsed_us", self.block.time_us as i64, i64),
            (
                "block_notarized_count",
                self.block_notarized.count as i64,
                i64
            ),
            (
                "block_notarized_elapsed_us",
                self.block_notarized.time_us as i64,
                i64
            ),
            ("first_shred_count", self.first_shred.count as i64, i64),
            (
                "first_shred_elapsed_us",
                self.first_shred.time_us as i64,
                i64
            ),
            ("parent_ready_count", self.parent_ready.count as i64, i64),
            (
                "parent_ready_elapsed_us",
                self.parent_ready.time_us as i64,
                i64
            ),
            (
                "timeout_crashed_leader_count",
                self.timeout_crashed_leader.count as i64,
                i64
            ),
            (
                "timeout_crashed_leader_elapsed_us",
                self.timeout_crashed_leader.time_us as i64,
                i64
            ),
            ("timeout_count", self.timeout.count as i64, i64),
            ("timeout_elapsed_us", self.timeout.time_us as i64, i64),
            ("safe_to_notar_count", self.safe_to_notar.count as i64, i64),
            (
                "safe_to_notar_elapsed_us",
                self.safe_to_notar.time_us as i64,
                i64
            ),
            ("safe_to_skip_count", self.safe_to_skip.count as i64, i64),
            (
                "safe_to_skip_elapsed_us",
                self.safe_to_skip.time_us as i64,
                i64
            ),
            (
                "produce_window_count",
                self.produce_window.count as i64,
                i64
            ),
            (
                "produce_window_elapsed_us",
                self.produce_window.time_us as i64,
                i64
            ),
            ("finalized_count", self.finalized.count as i64, i64),
            ("finalized_elapsed_us", self.finalized.time_us as i64, i64),
            ("standstill_count", self.standstill.count as i64, i64),
            ("standstill_elapsed_us", self.standstill.time_us as i64, i64),
            ("set_identity_count", self.set_identity.count as i64, i64),
            (
                "set_identity_elapsed_us",
                self.set_identity.time_us as i64,
                i64
            ),
        );
    }
}

#[derive(Debug, Default)]
struct SentVoteStats {
    finalize: usize,
    notarize: usize,
    notarize_fallback: usize,
    skip: usize,
    skip_fallback: usize,
}

impl SentVoteStats {
    fn incr_vote(&mut self, vote_type: VoteType) {
        match vote_type {
            VoteType::Finalize => self.finalize = self.finalize.saturating_add(1),
            VoteType::Notarize => self.notarize = self.notarize.saturating_add(1),
            VoteType::NotarizeFallback => {
                self.notarize_fallback = self.notarize_fallback.saturating_add(1)
            }
            VoteType::Skip => self.skip = self.skip.saturating_add(1),
            VoteType::SkipFallback => self.skip_fallback = self.skip_fallback.saturating_add(1),
            VoteType::Genesis => (),
        }
    }

    fn report(&self) {
        datapoint_info!(
            "event_handler_sent_vote_count",
            ("finalize", self.finalize as i64, i64),
            ("notarize", self.notarize as i64, i64),
            ("notarize_fallback", self.notarize_fallback as i64, i64),
            ("skip", self.skip as i64, i64),
            ("skip_fallback", self.skip_fallback as i64, i64),
        );
    }
}

#[derive(Debug)]
pub(crate) struct EventHandlerStats {
    /// Number of events that were ignored. This includes events that were
    /// received but not processed due to various reasons (e.g., outdated,
    /// irrelevant).
    pub(crate) ignored: usize,

    /// Number of times where we are attempting to start a leader window but
    /// there is already a pending window to produce. The older window is
    /// discarded in favor of the newer one.
    pub(crate) leader_window_replaced: usize,

    /// Number of times we updated the root.
    pub(crate) set_root_count: usize,

    /// Number of times we setup timeouts for a new leader window.
    pub(crate) timeout_set: usize,

    /// Amount of time spent receiving events. Includes waiting for events.
    pub(crate) receive_event_time_us: u64,

    /// Amount of time spent sending votes.
    pub(crate) send_votes_batch_time_us: u64,

    /// Number of times we saw each event and time spent processing the event.
    received_events_stats: ReceivedEventsStats,

    /// Number of votes sent for each vote type.
    sent_votes: SentVoteStats,

    /// Timing information for major events for each slot.
    slot_tracking_map: BTreeMap<Slot, SlotTracking>,

    /// Whether the send metrics queue has been full.
    pub(super) metrics_queue_became_full: bool,

    root_slot: Slot,
    last_report_time: Instant,
}

impl Default for EventHandlerStats {
    fn default() -> Self {
        Self::new()
    }
}

impl StatsEvent {
    pub fn new(event: &VotorEvent) -> Self {
        match event {
            VotorEvent::Block(_) => StatsEvent::Block,
            VotorEvent::BlockNotarized(_) => StatsEvent::BlockNotarized,
            VotorEvent::FirstShred(_) => StatsEvent::FirstShred,
            VotorEvent::ParentReady { .. } => StatsEvent::ParentReady,
            VotorEvent::TimeoutCrashedLeader(_) => StatsEvent::TimeoutCrashedLeader,
            VotorEvent::Timeout(_) => StatsEvent::Timeout,
            VotorEvent::SafeToNotar(_) => StatsEvent::SafeToNotar,
            VotorEvent::SafeToSkip(_) => StatsEvent::SafeToSkip,
            VotorEvent::ProduceWindow(_) => StatsEvent::ProduceWindow,
            VotorEvent::Finalized(..) => StatsEvent::Finalized,
            VotorEvent::Standstill(_) => StatsEvent::Standstill,
            VotorEvent::SetIdentity => StatsEvent::SetIdentity,
        }
    }
}

impl EventHandlerStats {
    pub fn new() -> Self {
        Self {
            ignored: 0,
            leader_window_replaced: 0,
            set_root_count: 0,
            timeout_set: 0,
            receive_event_time_us: 0,
            send_votes_batch_time_us: 0,
            received_events_stats: ReceivedEventsStats::default(),
            sent_votes: SentVoteStats::default(),
            slot_tracking_map: BTreeMap::new(),
            root_slot: 0,
            last_report_time: Instant::now(),
            metrics_queue_became_full: false,
        }
    }

    fn reset(&mut self, root_slot: Slot, slot_tracking_map: BTreeMap<Slot, SlotTracking>) {
        *self = EventHandlerStats::new();
        self.root_slot = root_slot;
        self.slot_tracking_map = slot_tracking_map;
    }

    pub fn handle_event_arrival(&mut self, event: &VotorEvent) -> StatsEvent {
        match event {
            VotorEvent::FirstShred(slot) => {
                let entry = self.slot_tracking_map.entry(*slot).or_default();
                entry.first_shred = Some(
                    Instant::now()
                        .saturating_duration_since(entry.start)
                        .as_micros() as i64,
                );
            }
            VotorEvent::ParentReady { slot, .. } => {
                let entry = self.slot_tracking_map.entry(*slot).or_default();
                entry.parent_ready = Some(
                    Instant::now()
                        .saturating_duration_since(entry.start)
                        .as_micros() as i64,
                );
            }
            VotorEvent::Finalized((slot, _), is_fast_finalization) => {
                let entry = self.slot_tracking_map.entry(*slot).or_default();
                if entry.finalized.is_none() {
                    entry.finalized = Some((
                        Instant::now()
                            .saturating_duration_since(entry.start)
                            .as_micros() as i64,
                        *is_fast_finalization,
                    ));
                } else if *is_fast_finalization {
                    // We can accept Notarize and FastFinalization, never set the flag from true to false
                    if let Some((instant, false)) = entry.finalized {
                        entry.finalized = Some((instant, true));
                    }
                }
            }
            _ => (),
        }
        StatsEvent::new(event)
    }

    pub fn set_root(&mut self, new_root: Slot) {
        self.root_slot = new_root;
        self.set_root_count = self.set_root_count.saturating_add(1);
    }

    pub fn incr_event_with_timing(&mut self, stats_event: StatsEvent, time_us: u64) {
        self.received_events_stats
            .incr_event_with_timing(stats_event, time_us);
    }

    pub fn incr_vote(&mut self, bls_op: &BLSOp) {
        let BLSOp::PushVote { message, .. } = bls_op else {
            warn!("Unexpected BLS operation: {bls_op:?}");
            return;
        };
        let ConsensusMessage::Vote(ref vote) = **message else {
            warn!("Unexpected BLS message type: {message:?}");
            return;
        };

        // Increment vote type counters
        let vote_type = VoteType::get_type(&vote.vote);
        self.sent_votes.incr_vote(vote_type);

        // Increment slot based counters
        if vote_type == VoteType::Notarize {
            let entry = self.slot_tracking_map.entry(vote.vote.slot()).or_default();
            entry.vote_notarize = Some(
                Instant::now()
                    .saturating_duration_since(entry.start)
                    .as_micros() as i64,
            );
        } else if vote_type == VoteType::Skip {
            let entry = self.slot_tracking_map.entry(vote.vote.slot()).or_default();
            entry.vote_skip = Some(
                Instant::now()
                    .saturating_duration_since(entry.start)
                    .as_micros() as i64,
            );
        }
    }

    pub fn maybe_report(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_report_time) < STATS_REPORT_INTERVAL {
            return;
        }
        datapoint_info!(
            "event_handler_stats",
            ("ignored", self.ignored as i64, i64),
            (
                "leader_window_replaced",
                self.leader_window_replaced as i64,
                i64
            ),
            ("set_root_count", self.set_root_count as i64, i64),
            ("timeout_set", self.timeout_set as i64, i64),
            (
                "metrics_queue_became_full",
                self.metrics_queue_became_full,
                bool
            )
        );
        datapoint_info!(
            "event_handler_timing",
            (
                "receive_event_time_us",
                self.receive_event_time_us as i64,
                i64
            ),
            (
                "send_votes_batch_time_us",
                self.send_votes_batch_time_us as i64,
                i64
            ),
        );

        self.received_events_stats.report();
        self.sent_votes.report();

        // Only report slots lower than the `root_slot`
        let split_off_map = self.slot_tracking_map.split_off(&self.root_slot);
        for (slot, tracking) in &self.slot_tracking_map {
            tracking.report(*slot);
        }

        self.reset(self.root_slot, split_off_map);
    }
}
