use {
    crate::{event::VotorEvent, voting_service::BLSOp},
    agave_votor_messages::{consensus_message::ConsensusMessage, vote::VoteType},
    solana_clock::Slot,
    solana_metrics::datapoint_info,
    std::{
        collections::{BTreeMap, HashMap},
        time::{Duration, Instant},
    },
};

const STATS_REPORT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
struct SlotTracking {
    /// The time when the slot tracking started
    start: Instant,
    /// The time when the first shred for this slot was received
    first_shred: Option<Instant>,
    /// The time when the parent block for this slot was ready
    parent_ready: Option<Instant>,
    /// The time when the notarization vote for this slot was sent
    vote_notarize: Option<Instant>,
    /// The time when the skip vote for this slot was sent
    vote_skip: Option<Instant>,
    /// If the slot was finalized, this is the time when it was finalized,
    /// the bool indicates if it was fast finalized
    finalized: Option<(Instant, bool)>,
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

#[derive(Debug, Default)]
struct EventCountAndTime {
    count: u16,
    time_us: u32,
}

#[derive(Debug)]
pub(crate) struct EventHandlerStats {
    // Number of events that were ignored. This includes events that were
    // received but not processed due to various reasons (e.g., outdated,
    // irrelevant).
    pub(crate) ignored: u16,

    // Number of times where we are attempting to start a leader window but
    // there is already a pending window to produce. The older window is
    // discarded in favor of the newer one.
    pub(crate) leader_window_replaced: u16,

    // Number of times we updated the root.
    pub(crate) set_root_count: u16,

    // Number of times we setup timeouts for a new leader window.
    pub(crate) timeout_set: u16,

    // Amount of time spent receiving events. Includes waiting for events.
    pub(crate) receive_event_time_us: u32,

    // Amount of time spent sending votes.
    pub(crate) send_votes_batch_time_us: u32,

    // Number of times we saw each event and time spent processing the event.
    received_events_count_and_timing: HashMap<StatsEvent, EventCountAndTime>,

    // Number of votes sent for each vote type.
    sent_votes: HashMap<VoteType, u16>,

    // Timing information for major events for each slot.
    slot_tracking_map: BTreeMap<Slot, SlotTracking>,

    root_slot: Slot,
    last_report_time: Instant,
}

impl Default for EventHandlerStats {
    fn default() -> Self {
        Self::new()
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
            received_events_count_and_timing: HashMap::new(),
            sent_votes: HashMap::new(),
            slot_tracking_map: BTreeMap::new(),
            root_slot: 0,
            last_report_time: Instant::now(),
        }
    }

    pub fn handle_event_arrival(&mut self, event: &VotorEvent) -> StatsEvent {
        match event {
            VotorEvent::FirstShred(slot) => {
                let entry = self.slot_tracking_map.entry(*slot).or_default();
                entry.first_shred = Some(Instant::now());
            }
            VotorEvent::ParentReady { slot, .. } => {
                let entry = self.slot_tracking_map.entry(*slot).or_default();
                entry.parent_ready = Some(Instant::now());
            }
            VotorEvent::Finalized((slot, _), is_fast_finalization) => {
                let entry = self.slot_tracking_map.entry(*slot).or_default();
                if entry.finalized.is_none() {
                    entry.finalized = Some((Instant::now(), *is_fast_finalization));
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
        let entry = self
            .received_events_count_and_timing
            .entry(stats_event)
            .or_default();
        entry.count = entry.count.saturating_add(1);
        entry.time_us = entry.time_us.saturating_add(time_us as u32);
    }

    pub fn incr_vote(&mut self, bls_op: &BLSOp) {
        if let BLSOp::PushVote { message, .. } = bls_op {
            let ConsensusMessage::Vote(vote) = **message else {
                warn!("Unexpected BLS message type: {message:?}");
                return;
            };
            let vote_type = vote.vote.get_type();
            let entry = self.sent_votes.entry(vote_type).or_insert(0);
            *entry = entry.saturating_add(1);
            if vote_type == VoteType::Notarize {
                let entry = self.slot_tracking_map.entry(vote.vote.slot()).or_default();
                entry.vote_notarize = Some(Instant::now());
            } else if vote_type == VoteType::Skip {
                let entry = self.slot_tracking_map.entry(vote.vote.slot()).or_default();
                entry.vote_skip = Some(Instant::now());
            }
        } else {
            warn!("Unexpected BLS operation: {bls_op:?}");
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
        );
        for (event, EventCountAndTime { count, time_us }) in &self.received_events_count_and_timing
        {
            datapoint_info!(
                "event_handler_received_event_count_and_timing",
                ("event", format!("{:?}", event), String),
                ("count", *count as i64, i64),
                ("elapsed_us", *time_us as i64, i64)
            );
        }
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
        for (vote_type, count) in &self.sent_votes {
            datapoint_info!(
                "event_handler_sent_vote_count",
                ("vote", format!("{:?}", vote_type), String),
                ("count", *count as i64, i64)
            );
        }
        // Only report if the slot is lower than root_slot
        let split_off_map = self.slot_tracking_map.split_off(&self.root_slot);
        for (slot, tracking) in &self.slot_tracking_map {
            let start = tracking.start;
            datapoint_info!(
                "event_handler_slot_tracking",
                ("slot", *slot as i64, i64),
                (
                    "first_shred",
                    tracking.first_shred.map(|t| {
                        t.saturating_duration_since(start)
                            .as_micros()
                            .min(i64::MAX as u128) as i64
                    }),
                    Option<i64>
                ),
                (
                    "parent_ready",
                    tracking.parent_ready.map(|t| {
                        t.saturating_duration_since(start)
                            .as_micros()
                            .min(i64::MAX as u128) as i64
                    }),
                    Option<i64>
                ),
                (
                    "vote_notarize",
                    tracking.vote_notarize.map(|t| {
                        t.saturating_duration_since(start)
                            .as_micros()
                            .min(i64::MAX as u128) as i64
                    }),
                    Option<i64>
                ),
                (
                    "vote_skip",
                    tracking.vote_skip.map(|t| {
                        t.saturating_duration_since(start)
                            .as_micros()
                            .min(i64::MAX as u128) as i64
                    }),
                    Option<i64>
                ),
                (
                    "finalized",
                    tracking.finalized.map(|t| {
                        t.0.saturating_duration_since(start)
                            .as_micros()
                            .min(i64::MAX as u128) as i64
                    }),
                    Option<i64>
                ),
                ("is_fast_finalization", tracking.finalized.map(|t| t.1), Option<bool>)
            );
        }
        self.last_report_time = now;
        let root_slot = self.root_slot;
        *self = EventHandlerStats::new();
        self.root_slot = root_slot;
        self.slot_tracking_map = split_off_map;
    }
}
