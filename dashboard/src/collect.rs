//! Samples validator state five times a second and publishes what changed.
//!
//! Everything reads through handles the validator already holds, and no lock
//! is held longer than it takes to clone an `Arc` out. This is the slot half
//! of the sampling, the half that reads the blockstore and the accounts
//! database; the once-a-second readings are in [`crate::meters`], on their own
//! thread, so a slow read here does not stall every panel.

use {
    crate::{
        context::{DashboardContext, StartProgress},
        history::SlotHistory,
        metrics_tap::{MetricsTap, ShredFill},
        produced::{ProducedBlock, ProducedRing},
        proto::{Debounced, Publisher, TOPIC_EPOCH, TOPIC_PEERS, TOPIC_SLOT, TOPIC_SUMMARY},
        slots::{BlockDetail, ShredArrival, SlotEntry, SlotLevel, SlotRing},
        startup::StartupPublisher,
        tips::{TipMeter, TipRates},
        validator_info::{self, ValidatorInfoCache},
    },
    serde::Serialize,
    solana_clock::{Clock, Epoch, Slot},
    solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    std::{
        collections::{HashMap, HashSet, VecDeque},
        sync::{Arc, Mutex, RwLock},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    },
};

/// A validator whose last vote is further behind than this is reported as
/// delinquent, matching the threshold the RPC layer uses.
const MAX_DELINQUENT_SLOT_DISTANCE: u64 = 128;

/// How often the expensive samples (the full validator set, the program cache)
/// are taken, regardless of the poll interval.
const SLOW_TICK: Duration = Duration::from_secs(5);

/// Slots to include in the strip and sidebar snapshot sent on connect.
const SLOT_OVERVIEW_LEN: usize = 512;

/// Recent slots kept in memory. Deeper than the overview a client is sent, so
/// a slot arriving late can still be matched.
const SLOT_HISTORY: usize = 4096;

/// Distinct client versions reported before the tail is folded into one row.
const MAX_VERSIONS_REPORTED: usize = 5;

/// How far ahead to look for this validator's next leader slot.
const NEXT_LEADER_LOOKAHEAD: u64 = 20_000;

/// Slots of the leader schedule published ahead of the tip: eight turns, of
/// which the page shows two. On the slow tier because the list shifts by a
/// couple of slots a second.
const UPCOMING_SLOTS: u64 = 32;

/// Produced blocks kept for the block detail panel, matched to the own-slot
/// retention in the slot ring: about eleven hours for a validator leading four
/// slots in eight hundred.
const PRODUCED_BLOCKS: usize = 500;

/// Slots of arrival times kept, about five minutes. Counted in slots rather
/// than time so the window does not thin out during a stall, which is when it
/// gets read. Both the strip's readout and the epoch countdown read spans of
/// it.
const SLOT_TIME_WINDOW_SLOTS: usize = 750;

/// Span the slot strip's readout averages over. Short enough to follow the
/// cluster, long enough that a single slow slot does not move it.
const SLOT_READOUT_SPAN_MS: u64 = 60_000;

/// How near the highest slot held replay must come before this validator is
/// following the cluster rather than replaying towards it.
const CAUGHT_UP_SLOT_DISTANCE: u64 = 4;

/// Samples the window must hold before the distance is believed. A validator
/// that has just loaded a snapshot sits at zero distance before replaying
/// anything.
const CAUGHT_UP_MIN_SAMPLES: usize = 64;

/// Slots skipped past when the marker is set, so that the interval straddling
/// the transition — part replay burst, part cluster — is not measured.
const CAUGHT_UP_MARGIN_SLOTS: u64 = 4;

/// Slots an epoch must have run before its own rate is believed. Below this
/// the cluster clock's whole-second quantisation is noisier than the sliding
/// window; above it the epoch's rate is the quieter of the two.
const EPOCH_RATE_MIN_ELAPSED_SLOTS: u64 = 4_000;

/// The countdown follows its estimate once it has moved further than this
/// fraction of the time left. Proportional because a fixed allowance is four
/// hundred times looser at the end of an epoch than at the start.
const EPOCH_END_DRIFT_DIVISOR: u32 = 64;

/// Slots timed in one tick. Bounds the blockstore lookups after a stall, when
/// the cursor could otherwise be thousands of slots behind.
const MAX_SLOTS_TIMED_PER_TICK: u64 = 512;

/// Above this rate of slots replayed per second the validator is catching up
/// rather than following the cluster, so throughput samples are discarded. A
/// healthy cluster produces about two and a half slots a second.
pub(crate) const CATCH_UP_SLOTS_PER_SECOND: f64 = 6.0;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StakeSummary {
    /// Stake delegated to this validator's vote account, in lamports.
    pub activated_stake: u64,
    /// Total stake across all vote accounts, in lamports.
    pub total_stake: u64,
    /// This validator's share of total stake, in `[0, 1]`.
    pub share: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValidatorCounts {
    /// Distinct staked identities this epoch. Unstaked vote accounts are excluded,
    /// and identities rather than vote accounts are counted so one validator counts
    /// once.
    pub total: usize,
    /// Staked validators whose last vote is too far behind the chain tip.
    pub delinquent: usize,
    pub rpc_nodes: usize,
    pub non_delinquent_stake: u64,
    pub delinquent_stake: u64,
}

/// How the cluster's stake is spread across client versions.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VersionShare {
    /// Semver as gossip reports it, or `null` for peers reporting none.
    pub version: Option<String>,
    pub validators: usize,
    pub stake: u64,
    /// True for the row the tail was folded into, which sorts beside a genuine
    /// no-version group.
    pub other: bool,
}

/// Stake, version and address for the leaders on screen only, so the table is
/// bounded by the page rather than the validator set. Name and icon are on the
/// slot rows already. The gossip address is public within the cluster, but
/// this publishes it to anyone who can reach the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Peer {
    pub identity: String,
    /// Client version as gossip reports it, absent for a node not being heard
    /// from.
    pub version: Option<String>,
    /// Active stake this epoch, in lamports. Zero for an unstaked node.
    pub stake: u64,
    /// Host of the gossip address, without the port.
    pub ip: Option<String>,
    /// Display name from on-chain validator info, held once per leader rather than
    /// on every slot it leads.
    pub name: Option<String>,
    /// The validator's on-chain icon URL, when it published one.
    pub icon: Option<String>,
}

/// A scheduled slot that has not happened yet. Leaner than [`SlotEntry`]: no
/// level, block or duration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpcomingSlot {
    pub slot: Slot,
    pub leader: String,
    pub leader_name: Option<String>,
    pub leader_icon: Option<String>,
    /// True when this validator is the scheduled leader.
    pub mine: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EpochInfo {
    pub epoch: Epoch,
    pub start_slot: Slot,
    pub end_slot: Slot,
    pub slots_in_epoch: u64,
    /// Slots in this epoch where this validator is the leader.
    pub my_leader_slots: Vec<Slot>,

    /// Every leader of this epoch, in the order they first take a turn. Sent once
    /// rather than on every slot.
    pub leaders: Vec<String>,
    /// One index into `leaders` per run of consecutive slots, so a slot's leader is
    /// `leaders[turns[(slot - start_slot) / NUM_CONSECUTIVE_LEADER_SLOTS]]`. Empty
    /// until the schedule is derived, and empty rather than partial where it could
    /// not be read as whole turns.
    pub turns: Vec<u16>,

    /// Consensus limits every block of this epoch is measured against. Here rather
    /// than per slot because they only move at epoch boundaries.
    pub block_cost_limit: u64,
    pub account_cost_limit: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Health {
    pub replay: &'static str,
    pub vote: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkipRate {
    pub epoch: Epoch,
    /// Fraction of this validator's leader slots that produced no block, over the
    /// part of the epoch the blockstore covers. `None` until the root has passed
    /// one.
    pub rate: Option<f64>,
}

#[derive(Default)]
struct Debounces {
    identity_key: Debounced<String>,
    identity_name: Debounced<Option<String>>,
    identity_icon: Debounced<Option<String>>,
    vote_key: Debounced<String>,
    root_slot: Debounced<Slot>,
    optimistically_confirmed_slot: Debounced<Slot>,
    finalized_slot: Debounced<Slot>,
    completed_slot: Debounced<Slot>,
    estimated_slot: Debounced<Slot>,
    vote_slot: Debounced<Option<Slot>>,
    behind_cluster: Debounced<Option<u64>>,
    identity_balance: Debounced<u64>,
    vote_balance: Debounced<u64>,
    vote_commission: Debounced<Option<u8>>,
    stake: Debounced<StakeSummary>,
    validator_counts: Debounced<ValidatorCounts>,
    versions: Debounced<Vec<VersionShare>>,
    block_height: Debounced<u64>,
    slot_duration_nanos: Debounced<u64>,
    observed_slot_duration_nanos: Debounced<Option<u64>>,
    next_leader_slot: Debounced<Option<Slot>>,
    skip_rate: Debounced<SkipRate>,
    health: Debounced<Health>,
    epoch: Debounced<EpochInfo>,
    epoch_remaining_nanos: Debounced<u64>,
    upcoming: Debounced<Vec<UpcomingSlot>>,
    peers: Debounced<Vec<Peer>>,
}

pub struct Collector {
    ctx: DashboardContext,
    publisher: Arc<Publisher>,
    /// Supplied by the service rather than the context, since the boot
    /// thread reports progress long before a context can be built.
    startup_progress: StartProgress,

    debounces: Debounces,
    slots: SlotRing,
    info_cache: Arc<RwLock<ValidatorInfoCache>>,
    /// The boot thread's own publisher, handed over rather than replaced: it
    /// holds what each phase took, and a fresh one here published `running`
    /// with no phases at all.
    startup: Arc<Mutex<StartupPublisher>>,
    /// For what replay spent on each slot, asked by slot as the bank freezes.
    metrics_tap: Arc<MetricsTap>,

    /// Highest slot for which leaders have been resolved, so the schedule is
    /// only walked forwards.
    leaders_resolved_to: Slot,
    /// Highest slot already swept for validator-info writes.
    info_scanned_to: Slot,
    /// Tip at the moment the collector started. Slots below it were never
    /// watched, so they are neither tracked nor counted as skipped.
    first_observed_slot: Option<Slot>,
    /// Detail for blocks this validator produced, captured as they froze.
    produced: ProducedRing,
    /// The same slots packed to a schedule row's columns and kept far deeper.
    /// Shared with the server, which answers range queries out of it.
    history: Arc<RwLock<SlotHistory>>,
    /// This epoch and the one before it, shared with the server. Only the current
    /// one is published; the previous is kept for pages reading back across the
    /// boundary.
    epochs: Arc<RwLock<Vec<EpochInfo>>>,
    /// The epoch, identity and whether the schedule was known when the epoch
    /// message was last built. It is a hundred thousand turns on mainnet, so it is
    /// built once per epoch rather than per poll. The schedule flag lets a late
    /// schedule still be published; the identity lets a validator that boots on a
    /// dummy identity and swaps get its real leader slots.
    epoch_published: Option<(Epoch, Pubkey, bool)>,
    /// This validator's leader slots for the epoch the root is in, walked by the
    /// skip rate as the root passes each. Rebuilt with `skip_next_index` when the
    /// root crosses an epoch, and on an identity change.
    skip_leader_slots: Vec<Slot>,
    skip_epoch: Option<(Epoch, Pubkey)>,
    skip_next_index: usize,
    skip_produced: usize,
    skip_elapsed: usize,
    last_completed_slot: Slot,
    last_completed_at: Instant,
    /// Highest slot examined for a shred timestamp, whether or not it had one.
    /// Skipped slots never do, so this advances past them independently.
    slot_timed_to: Option<Slot>,
    /// The last slot that did carry a timestamp, and that timestamp in
    /// milliseconds. The next slot's duration is measured from here.
    last_shred_time: Option<(Slot, u64)>,
    /// `(slot, arrival)` pairs spanning the averaging window, oldest first.
    /// Bounded by [`SLOT_TIME_WINDOW_SLOTS`].
    slot_time_window: VecDeque<(Slot, u64)>,
    /// First slot whose timing describes the cluster rather than a replay
    /// burst. Set once, never cleared. See [`Collector::mark_caught_up`].
    caught_up_at: Option<Slot>,
    /// Whether replay was seen trailing the tip before the marker was set, which
    /// decides whether there is anything to discard.
    replayed_behind: bool,
    /// Whether a tip ahead of replay has been seen since startup, and whether
    /// replay has since drawn level with it. The blockstore keeps optimistic slots
    /// from before a restart, so the first tip read is stale and below the
    /// snapshot; level with that is not caught up.
    trailed_cluster: bool,
    caught_cluster: bool,
    /// The epoch end being counted down to, held so the readout does not chase its
    /// own estimate. See [`EPOCH_END_DRIFT_DIVISOR`].
    epoch_end: Option<(Epoch, SystemTime)>,
    last_vote_advance: Instant,
    /// Whether this process is the identity allowed to vote with the configured
    /// vote account. False on a backup identity.
    voting: bool,
    last_slow_tick: Instant,
    /// Viewers attached as of the last tick, kept only so that pausing and
    /// resuming are logged once rather than on every tick.
    subscribers: usize,

    /// Reads what each slot paid in jito tips. `None` where no tip payment
    /// program is configured, which is every plain agave validator.
    tips: Option<TipMeter>,
    /// Our own commission on tips, sent to the page so it can work out what our
    /// blocks earned. Never applied to another validator's turn.
    commission_bps: Option<u16>,
    /// The meter's last reported residual, so a change is logged once rather
    /// than every slow tick.
    tips_residual: Option<u64>,
}

impl Collector {
    /// Every argument is a handle the service already holds, passed once at
    /// attach; a struct to carry them would exist for this call alone.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ctx: DashboardContext,
        publisher: Arc<Publisher>,
        info_cache: Arc<RwLock<ValidatorInfoCache>>,
        history: Arc<RwLock<SlotHistory>>,
        epochs: Arc<RwLock<Vec<EpochInfo>>>,
        startup_progress: StartProgress,
        startup: Arc<Mutex<StartupPublisher>>,
        metrics_tap: Arc<MetricsTap>,
        tips: Option<TipMeter>,
        commission_bps: Option<u16>,
    ) -> Self {
        let now = Instant::now();
        Self {
            slots: SlotRing::new(SLOT_HISTORY),
            ctx,
            publisher,
            startup_progress,
            debounces: Debounces::default(),
            info_cache,
            startup,
            metrics_tap,
            leaders_resolved_to: 0,
            info_scanned_to: 0,
            first_observed_slot: None,
            produced: ProducedRing::new(PRODUCED_BLOCKS),
            history,
            epochs,
            skip_leader_slots: Vec::new(),
            epoch_published: None,
            skip_epoch: None,
            skip_next_index: 0,
            skip_produced: 0,
            skip_elapsed: 0,
            last_completed_slot: 0,
            last_completed_at: now,
            slot_timed_to: None,
            last_shred_time: None,
            slot_time_window: VecDeque::new(),
            caught_up_at: None,
            replayed_behind: false,
            trailed_cluster: false,
            caught_cluster: false,
            epoch_end: None,
            last_vote_advance: now,
            // Nothing is known until the first bank is read, and claiming to be
            // voting before then would flash the wrong status on startup.
            voting: false,
            last_slow_tick: now.checked_sub(SLOW_TICK).unwrap_or(now),
            subscribers: 0,
            tips,
            commission_bps,
            tips_residual: None,
        }
    }

    /// Publishes the values that never change for the lifetime of the process.
    pub fn publish_static(&self) {
        let version = solana_version::Version::this_build();
        self.publisher
            .publish(TOPIC_SUMMARY, "version", &version.as_semver_string());
        // Published apart from the version, which does not carry it: forks ship the
        // version of the release they follow, and `4.2.1` alone does not say whether
        // this is Agave or Jito.
        self.publisher
            .publish(TOPIC_SUMMARY, "client", &version.client().to_string());
        self.publisher.publish(
            TOPIC_SUMMARY,
            "commit_hash",
            &format!("{:08x}", version.commit()),
        );
        self.publisher
            .publish(TOPIC_SUMMARY, "cluster", &self.ctx.cluster_name());
        // The rates the page derives its tip figures with. Sent rather than applied so
        // a corrected rate repairs the whole history. Absent where no tip program is
        // configured.
        if let Some(meter) = &self.tips {
            log::info!(
                "dashboard: reading jito tips from {} accounts, {:?}",
                meter.accounts().len(),
                meter.accounts()
            );
            self.publisher.publish(
                TOPIC_SUMMARY,
                "tip_rates",
                &TipRates {
                    jito_cut_bps: crate::tips::JITO_CUT_BPS,
                    commission_bps: self.commission_bps,
                },
            );
        }
        // Fixed once the node has joined, and the first thing to check when a
        // validator will not gossip.
        self.publisher.publish(
            TOPIC_SUMMARY,
            "shred_version",
            &self.ctx.cluster_info.my_shred_version(),
        );
    }

    pub fn tick(&mut self) {
        let now = Instant::now();

        // One acquisition for the whole tick, held only long enough to clone the
        // handles out: bank forks is the lock replay holds to advance.
        let (root_bank, working_bank, highest_slot, mut frozen) = {
            let bank_forks = self.ctx.bank_forks.read().unwrap();
            (
                bank_forks.root_bank(),
                bank_forks.working_bank(),
                bank_forks.highest_slot(),
                bank_forks.frozen_banks().collect::<Vec<_>>(),
            )
        };
        // Slot order, for the tip meter's running total. Everything else is a
        // difference against a parent and would be right in any order.
        frozen.sort_by_key(|(slot, _)| *slot);
        // The highest slot this validator has replayed, as opposed to the
        // highest it holds a bank for.
        let completed = frozen
            .iter()
            .map(|(slot, _)| *slot)
            .max()
            .unwrap_or_default();

        self.collect_slot_positions(&root_bank, highest_slot, completed);
        self.mark_caught_cluster();
        self.collect_leaders(&root_bank, highest_slot);
        self.collect_slot_levels(&root_bank, &frozen);
        // From the working bank: the root trails the tip by the thirty-two slots it
        // takes to root.
        self.collect_identity_and_vote(&working_bank);
        self.collect_epoch(&working_bank);
        self.collect_startup_progress();

        // The five-second tier is where the cost is, walking every vote account and
        // each newly frozen slot's write set, and is skipped while nobody is
        // connected. Only this tier: the tiers above feed the slot ring, and a gap
        // there would have the collector walking over slots it never watched.
        let subscribers = self.publisher.subscriber_count();
        if subscribers != self.subscribers {
            log::debug!(
                "dashboard: {subscribers} viewers attached, cluster sampling {}",
                if subscribers == 0 {
                    "paused"
                } else {
                    "running"
                }
            );
            self.subscribers = subscribers;
        }

        // `last_slow_tick` only moves when the tier runs, so the first tick after
        // someone connects is already due.
        if subscribers > 0 && now.duration_since(self.last_slow_tick) >= SLOW_TICK {
            self.last_slow_tick = now;
            self.collect_validator_info(&frozen);
            self.collect_peers(&working_bank);
            self.collect_health();
            self.collect_skip_rate(&root_bank);
            // Ahead of the peer table, which covers the leaders of both the
            // slots already sent and the ones about to be.
            let ahead = self.collect_upcoming(&root_bank, highest_slot);
            self.collect_peer_table(&working_bank, ahead);
            self.report_tip_residual();
        }
    }

    /// Logs what the last sweep says the tip readings missed, once per change. The
    /// only check the measurement gets; logged rather than published because it is
    /// about our arithmetic, not the cluster.
    fn report_tip_residual(&mut self) {
        let residual = self.tips.as_ref().and_then(TipMeter::residual);
        if residual == self.tips_residual {
            return;
        }
        self.tips_residual = residual;
        if let Some(lamports) = residual {
            log::info!(
                "dashboard: {lamports} lamports of tips were paid before the receiver changed and \
                 are counted against no turn"
            );
        }
    }

    // ---- slot positions -------------------------------------------------

    fn collect_slot_positions(&mut self, root_bank: &Bank, highest_slot: Slot, completed: Slot) {
        let commitment = self.ctx.block_commitment_cache.read().unwrap();
        let (root, confirmed, finalized) = (
            commitment.root(),
            commitment.highest_confirmed_slot(),
            commitment.highest_super_majority_root(),
        );
        drop(commitment);

        // `root_bank` is the authority on the root. The commitment cache can
        // briefly lag it during startup.
        let root = root.max(root_bank.slot());

        self.debounces
            .root_slot
            .publish(&self.publisher, TOPIC_SUMMARY, "root_slot", root);
        self.debounces.optimistically_confirmed_slot.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "optimistically_confirmed_slot",
            confirmed,
        );
        self.debounces.finalized_slot.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "finalized_slot",
            finalized,
        );
        self.debounces.estimated_slot.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "estimated_slot",
            highest_slot,
        );

        self.debounces.completed_slot.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "completed_slot",
            completed,
        );
        self.collect_slot_durations(completed);
        self.mark_caught_up(highest_slot, completed);
        self.observe_slot_duration(root_bank, completed);
    }

    /// Publishes both slot durations: what the cluster is configured for, which the
    /// strip's bars are drawn against, and what it is doing, which is the strip's
    /// readout.
    fn observe_slot_duration(&mut self, root_bank: &Bank, completed: Slot) {
        if completed > self.last_completed_slot {
            self.last_completed_slot = completed;
            self.last_completed_at = Instant::now();
        }

        // Constant between epoch boundaries, which is what makes it usable as
        // the scale the bars are drawn against.
        let ns_per_slot = root_bank.ns_per_slot_at_slot(completed) as u64;
        self.debounces.slot_duration_nanos.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "estimated_slot_duration_nanos",
            ns_per_slot,
        );

        self.debounces.observed_slot_duration_nanos.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "observed_slot_duration_nanos",
            self.windowed_slot_nanos(),
        );
    }

    /// Times each newly arrived slot from the blockstore's record of when its first
    /// shred landed, rather than from a 200ms poll that could be half a slot out.
    fn collect_slot_durations(&mut self, up_to: Slot) {
        // Nothing before the collector started was watched, so the first tick
        // establishes a baseline rather than walking the whole ledger.
        let from = match self.slot_timed_to {
            None => up_to,
            Some(timed_to) => timed_to.saturating_add(1),
        };
        let from = from.max(up_to.saturating_sub(MAX_SLOTS_TIMED_PER_TICK));

        let mut changed = Vec::new();
        for slot in from..=up_to {
            self.slot_timed_to = Some(slot);
            // A skipped slot has no shreds and no timestamp, so the next slot that does is
            // measured from the last that did.
            let Some(arrived) = self.first_shred_time(slot) else {
                continue;
            };

            let elapsed = self
                .last_shred_time
                .filter(|(previous_slot, _)| slot > *previous_slot)
                .map(|(_, previous_arrival)| arrived.saturating_sub(previous_arrival));
            // Reported as the slot fills, which is before replay can freeze it.
            let shreds = self.metrics_tap.shred_fill(slot).map(arrival);
            if let Some(entry) = self.slots.update(slot, |entry| {
                entry.time_millis = Some(arrived);
                if let Some(elapsed) = elapsed {
                    entry.duration_nanos = Some(elapsed.saturating_mul(1_000_000));
                }
                if entry.shreds.is_none() {
                    entry.shreds = shreds;
                }
            }) {
                changed.push(entry);
            }

            // The slot's own wall clock, kept by the packed history; the entries above
            // carry only the difference between slots.
            self.history.write().unwrap().record_time(slot, arrived);
            self.last_shred_time = Some((slot, arrived));
            self.slot_time_window.push_back((slot, arrived));
            // Skipped slots never enter the window. The mean divides by slot span, not
            // sample count, so they are still accounted for.
            while self.slot_time_window.len() > SLOT_TIME_WINDOW_SLOTS {
                self.slot_time_window.pop_front();
            }
        }

        for entry in &changed {
            self.publish_slot(entry);
        }
    }

    /// Records, once, the point from which slot timings describe the cluster. A
    /// validator replaying towards the tip runs through slots faster than they were
    /// produced, and those intervals would drag the epoch countdown down. The
    /// marker is set when the highest slot held comes within a few slots of what
    /// has been replayed, and never cleared: a validator that later falls behind
    /// has genuinely slow slots.
    fn mark_caught_up(&mut self, highest_slot: Slot, completed: Slot) {
        if self.caught_up_at.is_some() {
            return;
        }
        if highest_slot.saturating_sub(completed) > CAUGHT_UP_SLOT_DISTANCE {
            self.replayed_behind = true;
            return;
        }
        if self.slot_time_window.len() < CAUGHT_UP_MIN_SAMPLES {
            return;
        }

        let from = completed.saturating_add(CAUGHT_UP_MARGIN_SLOTS);
        self.caught_up_at = Some(from);
        if self.replayed_behind {
            // Everything held was measured while trailing the tip, so it goes and the
            // readout is blank until the window refills. A validator that started level
            // keeps its samples.
            self.slot_time_window.retain(|(slot, _)| *slot >= from);
        }
        log::info!("dashboard: caught up with the cluster, timing slots from {from}");
    }

    fn first_shred_time(&self, slot: Slot) -> Option<u64> {
        match self.ctx.blockstore.meta(slot) {
            Ok(Some(meta)) if meta.first_shred_timestamp > 0 => Some(meta.first_shred_timestamp),
            _ => None,
        }
    }

    /// Mean milliseconds per slot over the last minute, in nanoseconds.
    fn windowed_slot_nanos(&self) -> Option<u64> {
        windowed_mean_nanos(&self.slot_time_window, SLOT_READOUT_SPAN_MS)
    }

    fn collect_leaders(&mut self, root_bank: &Bank, highest_slot: Slot) {
        let me = self.ctx.identity();
        // The ring starts at the current tip. Filling it with earlier slots would
        // report every one as skipped, since no bank for them will appear.
        let from = match self.first_observed_slot {
            None => {
                self.first_observed_slot = Some(highest_slot);
                highest_slot
            }
            Some(_) => self
                .leaders_resolved_to
                .max(highest_slot.saturating_sub(SLOT_HISTORY as u64)),
        };

        for slot in from..=highest_slot {
            let Some(leader) = self
                .ctx
                .leader_schedule_cache
                .slot_leader_at(slot, Some(root_bank))
            else {
                // The schedule for this epoch is not known yet; stop here and
                // pick up where we left off next tick.
                self.leaders_resolved_to = slot;
                return;
            };
            // Only whether it is ours. The leader comes off the epoch's turn array in the
            // browser.
            if let Some(entry) = self.slots.set_mine(slot, leader.id == me) {
                self.publish_slot(&entry);
            }
            self.leaders_resolved_to = slot.saturating_add(1);
        }

        // The cache's own lookahead skips slots we already have shreds for. A
        // plain walk of the schedule would report those as still upcoming.
        let next_mine = self.ctx.leader_schedule_cache.next_leader_slot(
            &me,
            highest_slot,
            root_bank,
            Some(self.ctx.blockstore.as_ref()),
            NEXT_LEADER_LOOKAHEAD,
        );
        self.debounces.next_leader_slot.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "next_leader_slot",
            next_mine.map(|(first, _last)| first),
        );
    }

    /// Publishes who leads the slots that have not happened yet, as far as the
    /// schedule is known. Anchored on the highest slot bank forks holds so the list
    /// starts past the slot being worked on. Returns the leaders published, for the
    /// peer table.
    fn collect_upcoming(&mut self, root_bank: &Bank, highest_slot: Slot) -> HashSet<String> {
        let me = self.ctx.identity();
        let first = highest_slot.saturating_add(1);
        let last = highest_slot.saturating_add(UPCOMING_SLOTS);

        let mut upcoming = Vec::new();
        for slot in first..=last {
            let Some(leader) = self
                .ctx
                .leader_schedule_cache
                .slot_leader_at(slot, Some(root_bank))
            else {
                break;
            };
            let (leader_name, leader_icon) = self.peer_display(&leader.id);
            upcoming.push(UpcomingSlot {
                slot,
                leader: leader.id.to_string(),
                leader_name,
                leader_icon,
                mine: leader.id == me,
            });
        }

        let leaders = upcoming
            .iter()
            .map(|slot| slot.leader.clone())
            .collect::<HashSet<_>>();
        self.debounces
            .upcoming
            .publish(&self.publisher, TOPIC_SLOT, "upcoming", upcoming);
        leaders
    }

    /// Publishes stake, client version and address for the leaders on screen, and
    /// no more: a table of every node would be the largest message the dashboard
    /// sends. Sorted by identity so the debounce has a stable value.
    fn collect_peer_table(&mut self, bank: &Bank, mut leaders: HashSet<String>) {
        // The leaders of the window a client holds, from the schedule: a leader takes
        // four slots at a time, so a quarter as many lookups as slots.
        let highest = self.last_completed_slot;
        let first = highest.saturating_sub(SLOT_OVERVIEW_LEN as u64);
        let stride = NUM_CONSECUTIVE_LEADER_SLOTS.get() as u64;
        let mut slot = first.saturating_sub(first.checked_rem(stride).unwrap_or(0));
        while slot <= highest {
            if let Some(leader) = self
                .ctx
                .leader_schedule_cache
                .slot_leader_at(slot, Some(bank))
            {
                leaders.insert(leader.id.to_string());
            }
            slot = slot.saturating_add(stride);
        }

        let mut stakes: HashMap<String, u64> = HashMap::new();
        for (stake, account) in bank.vote_accounts().values() {
            if *stake == 0 {
                continue;
            }
            let identity = account.node_pubkey().to_string();
            if leaders.contains(&identity) {
                let total = stakes.entry(identity).or_insert(0);
                *total = total.saturating_add(*stake);
            }
        }

        let mut gossip: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
        for (contact_info, _) in self.ctx.cluster_info.all_peers() {
            let identity = contact_info.pubkey().to_string();
            if !leaders.contains(&identity) {
                continue;
            }
            gossip.insert(
                identity,
                (
                    Some(contact_info.version().to_string()),
                    contact_info.gossip().map(|addr| addr.ip().to_string()),
                ),
            );
        }

        let mut peers: Vec<Peer> = leaders
            .into_iter()
            .map(|identity| {
                let (version, ip) = gossip.get(&identity).cloned().unwrap_or_default();
                let (name, icon) = identity
                    .parse()
                    .map(|key| self.peer_display(&key))
                    .unwrap_or_default();
                Peer {
                    stake: stakes.get(&identity).copied().unwrap_or(0),
                    version,
                    ip,
                    name,
                    icon,
                    identity,
                }
            })
            .collect();
        peers.sort_by(|a, b| a.identity.cmp(&b.identity));

        self.debounces
            .peers
            .publish(&self.publisher, TOPIC_PEERS, "all", peers);
    }

    fn collect_slot_levels(&mut self, root_bank: &Bank, frozen: &[(Slot, Arc<Bank>)]) {
        let commitment = self.ctx.block_commitment_cache.read().unwrap();
        let (confirmed, finalized) = (
            commitment.highest_confirmed_slot(),
            commitment.highest_super_majority_root(),
        );
        drop(commitment);
        let root = root_bank.slot();

        let mut changed = Vec::new();
        let mut captured = false;
        for (slot, bank) in frozen {
            let slot = *slot;
            // Transaction counts are cumulative along a fork, so a block's own work is its
            // difference from its parent. Not `Bank::executed_transaction_count`, which
            // treats a missing parent as zero and yields the running total again. A pruned
            // parent leaves the figure alone; by then the bank was frozen many ticks ago
            // and carries a correct count.
            let parent = bank.parent();
            let counts = parent.as_ref().map(|parent| {
                (
                    bank.transaction_count()
                        .saturating_sub(parent.transaction_count()),
                    bank.non_vote_transaction_count_since_restart()
                        .saturating_sub(parent.non_vote_transaction_count_since_restart()),
                )
            });
            // Read once, at a frozen bank's first sighting, for every block: the cost
            // tracker and fees go with the bank when it is dropped after rooting.
            let fresh = self
                .slots
                .get(slot)
                .is_none_or(|entry| entry.block.is_none());
            // Once per slot, and only with a parent to difference against.
            let tips = if fresh {
                parent
                    .as_ref()
                    .zip(self.tips.as_mut())
                    .map(|(parent, meter)| meter.measure(bank, parent))
            } else {
                None
            };
            // Replay reports a point for a bank this validator built too, at nought:
            // it finds the block already executed and has nothing to do. Left
            // absent rather than shown as replayed in no time.
            let mine = self.slots.get(slot).is_some_and(|entry| entry.mine);
            let replayed = (fresh && !mine)
                .then(|| self.metrics_tap.replayed(slot))
                .flatten();
            let replay = replayed.map(|times| times.serial());
            // From the slot's first shred, on the blockstore's clock. The entry has
            // it once the timing walk has passed; before that, the blockstore does.
            let replayed_millis = replayed.and_then(|times| {
                let first = self
                    .slots
                    .get(slot)
                    .and_then(|entry| entry.time_millis)
                    .or_else(|| self.first_shred_time(slot))?;
                Some(times.observed_millis.saturating_sub(first))
            });
            // Also here, for a slot frozen before the timing walk reached it.
            let shreds = fresh
                .then(|| self.metrics_tap.shred_fill(slot).map(arrival))
                .flatten();
            let detail = counts
                .filter(|_| fresh)
                .map(|(total, non_vote)| block_detail(bank, total, non_vote, tips, replay));

            // Our own blocks carry a blockhash and a start time on top of that,
            // which the block panel shows and nothing else needs.
            if let Some(detail) = &detail
                && mine
            {
                let block = self.capture_block(slot, bank, detail);
                if self.produced.insert(block) {
                    captured = true;
                }
            }

            let level = level_for(slot, root, confirmed, finalized);
            if let Some(entry) = self.slots.update(slot, |entry| {
                entry.level = level;
                if let Some(detail) = &detail {
                    entry.block = Some(detail.clone());
                }
                if replayed_millis.is_some() {
                    entry.replayed_millis = replayed_millis;
                }
                if entry.shreds.is_none() {
                    entry.shreds = shreds;
                }
            }) {
                changed.push(entry);
            }
        }

        // The loop above no longer covers slots that have fallen out of bank
        // forks, so their levels advance from the roots directly.
        changed.extend(self.slots.promote(finalized, SlotLevel::Finalized));
        changed.extend(self.slots.promote(root, SlotLevel::Rooted));
        changed.extend(
            self.slots
                .promote(confirmed, SlotLevel::OptimisticallyConfirmed),
        );

        // Anything still unstarted below the root will never be produced.
        changed.extend(self.slots.mark_skipped_below(root));

        for entry in &changed {
            self.publish_slot(entry);
        }
        if !changed.is_empty() {
            self.retain_slot_overview();
        }
        if captured {
            self.publisher
                .publish(TOPIC_SUMMARY, "produced_blocks", &self.produced.blocks());
        }
    }

    /// Adds what only our own blocks report: the blockhash and start time, which
    /// only the block panel reads and would be forty-four characters on every slot
    /// otherwise.
    fn capture_block(&self, slot: Slot, bank: &Bank, detail: &BlockDetail) -> ProducedBlock {
        ProducedBlock {
            slot,
            slot_time_millis: self.first_shred_time(slot),
            blockhash: bank.last_blockhash().to_string(),
            duration_nanos: self.slots.get(slot).and_then(|entry| entry.duration_nanos),
            transactions: detail.transactions,
            non_vote_transactions: detail.non_vote_transactions,
            failed_transactions: detail.failed_transactions,
            entries: detail.entries,
            block_cost: detail.block_cost,
            block_cost_limit: detail.block_cost_limit,
            account_cost_limit: detail.account_cost_limit,
            total_fees: detail.total_fees,
            priority_fees: detail.priority_fees,
            tips: detail.tips,
        }
    }

    /// Sends one changed slot to live clients and keeps it in the packed history.
    /// Here rather than at the four places that change a slot, since no single
    /// moment finishes an entry.
    fn publish_slot(&mut self, entry: &SlotEntry) {
        self.history.write().unwrap().record(entry);
        self.publisher
            .publish_ephemeral(TOPIC_SLOT, "update", entry);
    }

    /// Refreshes the snapshot a newly connected client receives, without
    /// broadcasting it to clients that are already following the live updates.
    fn retain_slot_overview(&self) {
        self.publisher.retain_only(
            TOPIC_SLOT,
            "overview",
            &self.slots.overview(SLOT_OVERVIEW_LEN),
        );
    }

    // ---- identity, vote account, stake ----------------------------------

    fn collect_identity_and_vote(&mut self, bank: &Bank) {
        let identity = self.ctx.identity();
        self.debounces.identity_key.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "identity_key",
            identity.to_string(),
        );
        self.debounces.vote_key.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "vote_key",
            self.ctx.vote_account.to_string(),
        );
        let (my_name, my_icon) = self.peer_display(&identity);
        self.debounces.identity_name.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "identity_name",
            my_name,
        );
        self.debounces.identity_icon.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "identity_icon",
            my_icon,
        );
        self.debounces.identity_balance.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "identity_balance",
            bank.get_balance(&identity),
        );
        self.debounces.vote_balance.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "vote_balance",
            bank.get_balance(&self.ctx.vote_account),
        );

        let vote_accounts = bank.vote_accounts();
        let mine = vote_accounts.get(&self.ctx.vote_account);
        let total_stake: u64 = vote_accounts.values().map(|(stake, _)| *stake).sum();

        // Whether this process is the one allowed to vote with that account. The
        // identity changes under `set-identity`; the vote account is fixed at startup.
        // After a failover the account is voted from elsewhere, and reading its last
        // vote would report another machine's health.
        let identity = self.ctx.identity();
        let voting = mine.is_some_and(|(_, account)| *account.node_pubkey() == identity);
        self.voting = voting;

        let (activated_stake, commission, voter_vote) = match mine {
            Some((stake, account)) => {
                let view = account.vote_state_view();
                (*stake, Some(view.commission()), view.last_voted_slot())
            }
            None => (0, None, None),
        };

        // Published only while this process is the voter; otherwise it is another
        // node's progress.
        let last_vote = if voting { voter_vote } else { None };

        self.debounces.vote_commission.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "vote_commission",
            commission,
        );
        self.debounces.stake.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "stake",
            StakeSummary {
                activated_stake,
                total_stake,
                share: if total_stake == 0 {
                    0.0
                } else {
                    activated_stake as f64 / total_stake as f64
                },
            },
        );

        if self.debounces.vote_slot.last() != Some(&last_vote) {
            self.last_vote_advance = Instant::now();
        }
        self.debounces
            .vote_slot
            .publish(&self.publisher, TOPIC_SUMMARY, "vote_slot", last_vote);

        // How far this node's replay trails the cluster. The one figure not taken from
        // this validator's own view of the chain, which lags when replay lags: a node
        // hundreds of slots back votes promptly on a stale tip and passes every other
        // check. `collect_slot_positions` ran earlier this tick, so the completed slot
        // is current.
        let behind_cluster = self
            .ctx
            .cluster_tip()
            .map(|tip| tip.saturating_sub(self.last_completed_slot));
        self.debounces.behind_cluster.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "behind_cluster",
            behind_cluster,
        );
    }

    // ---- epoch ----------------------------------------------------------

    fn collect_epoch(&mut self, bank: &Bank) {
        // Blocks, not slots. A skipped slot advances one and not the other, so
        // the gap between the two is how much the cluster has dropped.
        self.debounces.block_height.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "block_height",
            bank.block_height(),
        );

        let epoch_schedule = bank.epoch_schedule();
        let slot = bank.slot();
        let epoch = epoch_schedule.get_epoch(slot);
        let start_slot = epoch_schedule.get_first_slot_in_epoch(epoch);
        let slots_in_epoch = epoch_schedule.get_slots_in_epoch(epoch);
        let end_slot = start_slot.saturating_add(slots_in_epoch.saturating_sub(1));

        // The countdown is published separately below. It changes constantly, and
        // this message carries every leader slot in the epoch.

        // Built when the epoch turns, not at every poll. See `epoch_published`.
        let me = self.ctx.identity();
        if self.epoch_published != Some((epoch, me, true)) {
            // An unknown schedule is published as no leader slots. The panel
            // counts them, and a count is better absent-as-zero than withheld.
            let my_leader_slots = self.leader_slots_in_epoch(bank, epoch).unwrap_or_default();
            let (leaders, turns) = self.epoch_turns(epoch, slots_in_epoch);
            let known = !turns.is_empty();

            // Poisoned only if a replay thread panicked while holding it.
            let (block_cost_limit, account_cost_limit) = match bank.read_cost_tracker() {
                Ok(tracker) => (tracker.get_block_limit(), tracker.get_account_limit()),
                Err(_) => (0, 0),
            };

            let current = EpochInfo {
                epoch,
                start_slot,
                end_slot,
                slots_in_epoch,
                my_leader_slots,
                leaders,
                turns,
                block_cost_limit,
                account_cost_limit,
            };

            // The epoch before this one, built alongside and kept rather than sent. A page
            // reading back through the history crosses into it about a quarter of the
            // time, and it is half a megabyte, so it is asked for rather than pushed.
            let previous = epoch
                .checked_sub(1)
                .map(|before| self.epoch_record(bank, before));

            let mut archive = self.epochs.write().unwrap();
            archive.clear();
            archive.extend(previous.into_iter().flatten());
            archive.push(current.clone());
            drop(archive);

            self.debounces
                .epoch
                .publish(&self.publisher, TOPIC_EPOCH, "new", current);
            self.epoch_published = Some((epoch, me, known));
        }

        self.collect_epoch_countdown(bank, epoch, start_slot, end_slot);
    }

    /// Publishes how much of the epoch is left as a duration, which needs no
    /// agreement about the clock between this validator and the viewer. Rounded to
    /// the second so the debounce has something to suppress.
    fn collect_epoch_countdown(
        &mut self,
        bank: &Bank,
        epoch: Epoch,
        start_slot: Slot,
        end_slot: Slot,
    ) {
        // The slot the panel's progress bar is drawn from, so the two halves of the
        // card agree. The working bank's slot, because nothing has frozen yet just
        // after startup, and a completed slot of zero would put the epoch end years
        // out.
        let completed = self.last_completed_slot.max(bank.slot());
        let remaining_slots = end_slot.saturating_sub(completed);
        let ahead = Duration::from_nanos(
            remaining_slots.saturating_mul(self.cluster_slot_nanos(bank, start_slot, completed)),
        );

        let now = SystemTime::now();
        let Some(estimate) = now.checked_add(ahead) else {
            return;
        };
        let held = self
            .epoch_end
            .filter(|(held_epoch, _)| *held_epoch == epoch)
            .map(|(_, end)| end);
        // Measured against what is left, so the countdown is as steady an hour
        // from the boundary as it is six hours out.
        let allowance = held
            .and_then(|end| end.duration_since(now).ok())
            .unwrap_or_default()
            .checked_div(EPOCH_END_DRIFT_DIVISOR)
            .unwrap_or_default();
        let end = steady_epoch_end(held, estimate, allowance);
        self.epoch_end = Some((epoch, end));

        let remaining = end.duration_since(now).unwrap_or_default();
        self.debounces.epoch_remaining_nanos.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "epoch_remaining_nanos",
            remaining.as_secs().saturating_mul(1_000_000_000),
        );
    }

    /// How long a slot is taking, on the best evidence available: the epoch's own
    /// rate, measured over every slot since it began and from the cluster's clock,
    /// once enough of it has run; the sliding window before that; and the
    /// configured duration before this validator has caught up, when replayed slots
    /// record the download rather than the cluster.
    fn cluster_slot_nanos(&self, bank: &Bank, start_slot: Slot, completed: Slot) -> u64 {
        epoch_anchored_nanos(&bank.clock(), start_slot, completed)
            .or_else(|| {
                self.caught_up_at
                    .and_then(|_| windowed_mean_nanos(&self.slot_time_window, u64::MAX))
            })
            .unwrap_or_else(|| bank.ns_per_slot_at_slot(completed) as u64)
    }

    /// Everything the page needs to name the leaders of an epoch that is not the
    /// current one. `my_leader_slots` is left empty; nothing asks about a past
    /// epoch's. `None` where the schedule is no longer cached.
    fn epoch_record(&self, bank: &Bank, epoch: Epoch) -> Option<EpochInfo> {
        let schedule = bank.epoch_schedule();
        let start_slot = schedule.get_first_slot_in_epoch(epoch);
        let slots_in_epoch = schedule.get_slots_in_epoch(epoch);
        let (leaders, turns) = self.epoch_turns(epoch, slots_in_epoch);
        if turns.is_empty() {
            return None;
        }

        let (block_cost_limit, account_cost_limit) = match bank.read_cost_tracker() {
            Ok(tracker) => (tracker.get_block_limit(), tracker.get_account_limit()),
            Err(_) => (0, 0),
        };
        Some(EpochInfo {
            epoch,
            start_slot,
            end_slot: start_slot.saturating_add(slots_in_epoch.saturating_sub(1)),
            slots_in_epoch,
            my_leader_slots: Vec::new(),
            leaders,
            turns,
            block_cost_limit,
            account_cost_limit,
        })
    }

    /// The epoch's leader schedule as a table of leaders and one index per turn,
    /// recovered by stepping over `get_slot_leaders` at the schedule's own stride.
    /// Empty, never partial, if the length does not come out as whole turns: a
    /// schedule off by a factor would name the wrong leader for every slot.
    fn epoch_turns(&self, epoch: Epoch, slots_in_epoch: u64) -> (Vec<String>, Vec<u16>) {
        let Some(schedule) = self
            .ctx
            .leader_schedule_cache
            .get_epoch_leader_schedule(epoch)
        else {
            return (Vec::new(), Vec::new());
        };
        let stride = NUM_CONSECUTIVE_LEADER_SLOTS.get();

        let mut leaders: Vec<String> = Vec::new();
        let mut seen: HashMap<Pubkey, u16> = HashMap::new();
        let mut turns: Vec<u16> = Vec::new();
        for leader in schedule.get_slot_leaders().step_by(stride) {
            let index = match seen.get(&leader.id) {
                Some(index) => *index,
                None => {
                    // More than sixty-five thousand distinct leaders cannot be indexed by this
                    // array; mainnet has about thirteen hundred.
                    let Ok(index) = u16::try_from(leaders.len()) else {
                        return (Vec::new(), Vec::new());
                    };
                    leaders.push(leader.id.to_string());
                    seen.insert(leader.id, index);
                    index
                }
            };
            turns.push(index);
        }

        let covered = (turns.len() as u64).saturating_mul(stride as u64);
        if covered != slots_in_epoch {
            log::warn!(
                "dashboard: epoch {epoch} leader schedule read as {} turns covering {covered} \
                 slots, not {slots_in_epoch}; publishing no schedule for it",
                turns.len()
            );
            return (Vec::new(), Vec::new());
        }
        (leaders, turns)
    }

    /// This validator's leader slots in `epoch`, ascending. The epoch is passed
    /// in because the panel wants the cluster's epoch and the skip rate wants the
    /// root's, and they differ after a rollover. `None` means the schedule is not
    /// known yet, which is not the same as an empty list.
    fn leader_slots_in_epoch(&self, bank: &Bank, epoch: Epoch) -> Option<Vec<Slot>> {
        let epoch_schedule = bank.epoch_schedule();
        let start_slot = epoch_schedule.get_first_slot_in_epoch(epoch);
        let end_slot =
            start_slot.saturating_add(epoch_schedule.get_slots_in_epoch(epoch).saturating_sub(1));

        // `get_leader_upcoming_slots` yields an endlessly repeating schedule, so
        // the `take_while` is what bounds it to the epoch asked for.
        let me = self.ctx.identity();
        self.ctx
            .leader_schedule_cache
            .get_epoch_leader_schedule(epoch)
            .map(|leaders| {
                leaders
                    .get_leader_upcoming_slots(&me, 0)
                    .map(|index| start_slot.saturating_add(index as Slot))
                    .take_while(|slot| *slot <= end_slot)
                    .collect()
            })
    }

    // ---- clock, TPS -----------------------------------------------------

    // ---- peers ----------------------------------------------------------

    /// Counts the cluster: who holds stake, who is behind, and what they run.
    /// Accumulated straight into the counters rather than building a record per
    /// validator first.
    fn collect_peers(&mut self, bank: &Bank) {
        let vote_accounts = bank.vote_accounts();
        let tip = bank.slot();

        // Gossip reports a client version; vote accounts report stake. A
        // validator can appear in one and not the other, so both are walked.
        let versions: HashMap<Pubkey, String> = self
            .ctx
            .cluster_info
            .all_peers()
            .into_iter()
            .map(|(contact_info, _)| (*contact_info.pubkey(), contact_info.version().to_string()))
            .collect();

        let rpc_nodes = self
            .ctx
            .cluster_info
            .rpc_peers()
            .iter()
            .map(|contact_info| *contact_info.pubkey())
            .collect::<HashSet<_>>()
            .len();

        let tally = tally_stake(
            vote_accounts
                .iter()
                .map(|(_vote_pubkey, (stake, account))| {
                    (
                        *account.node_pubkey(),
                        *stake,
                        account.vote_state_view().last_voted_slot(),
                    )
                }),
            tip,
        );

        self.debounces.validator_counts.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "validator_counts",
            ValidatorCounts {
                total: tally.staked.len(),
                delinquent: tally.delinquent.len(),
                rpc_nodes,
                non_delinquent_stake: tally.non_delinquent_stake,
                delinquent_stake: tally.delinquent_stake,
            },
        );

        self.debounces.versions.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "versions",
            version_shares(&tally.staked, &versions),
        );
    }

    /// Display name and icon URL from on-chain validator info. The icon is a
    /// third-party URL the client fetches itself.
    fn peer_display(&self, identity: &Pubkey) -> (Option<String>, Option<String>) {
        match self.info_cache.read().unwrap().get(identity) {
            None => (None, None),
            Some(info) => (info.name.clone(), info.icon_url.clone()),
        }
    }

    /// Picks up validator names published since the last sweep. Each bank is asked
    /// only for the config accounts written in its own slot, and every bank frozen
    /// since the last tick is swept, since bank forks drops banks once rooted. The
    /// cache lock is taken only to merge, and only when a scan found something.
    fn collect_validator_info(&mut self, frozen: &[(Slot, Arc<Bank>)]) {
        let mut found = Vec::new();
        for (slot, bank) in frozen {
            if *slot <= self.info_scanned_to {
                continue;
            }
            self.info_scanned_to = self.info_scanned_to.max(*slot);
            found.extend(validator_info::scan_slot(bank));
        }
        if found.is_empty() {
            return;
        }

        let changed = self.info_cache.write().unwrap().merge(found);
        if changed > 0 {
            log::debug!("dashboard: {changed} validator info entries updated");
        }
    }

    // ---- health, skip rate ----------------------------------------------

    /// Stamps, once, the moment replay draws level with a cluster tip it was
    /// seen trailing: caught up as an operator means it. On the fast path, since
    /// the health figures only run while someone is watching.
    fn mark_caught_cluster(&mut self) {
        if self.caught_cluster {
            return;
        }
        let Some(tip) = self.ctx.cluster_tip() else {
            return;
        };
        if tip > self.last_completed_slot {
            self.trailed_cluster = true;
            return;
        }
        if !self.trailed_cluster {
            return;
        }
        self.caught_cluster = true;
        // Wall clock rather than an offset, since the collector is built after
        // boot and the page has the uptime to take it from.
        self.publisher.publish(
            TOPIC_SUMMARY,
            "caught_up_time_nanos",
            &system_time_nanos(SystemTime::now()),
        );
    }

    fn collect_health(&mut self) {
        let health = health_of(
            self.last_completed_at.elapsed(),
            self.last_completed_slot,
            self.voting,
            self.debounces.vote_slot.last().copied().flatten(),
            // Still the vote's own distance, which is what the delinquency rules are
            // about.
            self.debounces
                .vote_slot
                .last()
                .copied()
                .flatten()
                .map(|vote| self.last_completed_slot.saturating_sub(vote)),
            self.last_vote_advance.elapsed(),
        );
        self.debounces
            .health
            .publish(&self.publisher, TOPIC_SUMMARY, "health", health);
    }

    /// Skip rate across this validator's leader slots for the epoch, from the
    /// blockstore rather than the slot ring, on the same basis as `solana
    /// block-production`. Each leader slot is checked once, as the root passes it.
    fn collect_skip_rate(&mut self, root_bank: &Bank) {
        // The root's epoch, not the working bank's: they differ for half a minute
        // after a rollover.
        let epoch = root_bank.epoch();
        // Keyed on the identity too, so a validator that boots on a dummy one and
        // swaps counts its real slots.
        let me = self.ctx.identity();
        if self.skip_epoch != Some((epoch, me)) {
            // Latched only once the schedule is in hand. Taking an unknown schedule as
            // empty would record a permanent zero.
            let Some(leader_slots) = self.leader_slots_in_epoch(root_bank, epoch) else {
                return;
            };
            self.skip_epoch = Some((epoch, me));
            self.skip_leader_slots = leader_slots;
            self.skip_next_index = 0;
            self.skip_produced = 0;
            self.skip_elapsed = 0;
        }

        // Only slots the root has passed have a settled outcome, and only slots the
        // blockstore covers say anything about production. After a restart from a
        // snapshot the ledger begins partway through the epoch.
        let root = root_bank.slot();
        let floor = self.ctx.blockstore.lowest_slot();
        while let Some(slot) = self.skip_leader_slots.get(self.skip_next_index).copied() {
            if slot > root {
                break;
            }
            self.skip_next_index = self.skip_next_index.saturating_add(1);
            if slot < floor {
                continue;
            }
            if self.ctx.blockstore.is_full(slot) {
                self.skip_produced = self.skip_produced.saturating_add(1);
            }
            self.skip_elapsed = self.skip_elapsed.saturating_add(1);
        }

        let rate = (self.skip_elapsed > 0).then(|| {
            self.skip_elapsed.saturating_sub(self.skip_produced) as f64 / self.skip_elapsed as f64
        });
        self.debounces.skip_rate.publish(
            &self.publisher,
            TOPIC_SUMMARY,
            "skip_rate",
            SkipRate { epoch, rate },
        );
    }

    fn collect_startup_progress(&mut self) {
        let progress = *self.startup_progress.read().unwrap();
        self.startup
            .lock()
            .unwrap()
            .publish(&self.publisher, progress);
    }
}

/// How settled a frozen slot is. Tested most-settled first, because the
/// thresholds cross during startup when the commitment cache lags the root
/// bank.
fn level_for(slot: Slot, root: Slot, confirmed: Slot, finalized: Slot) -> SlotLevel {
    if slot <= finalized {
        SlotLevel::Finalized
    } else if slot <= root {
        SlotLevel::Rooted
    } else if slot <= confirmed {
        SlotLevel::OptimisticallyConfirmed
    } else {
        SlotLevel::Completed
    }
}

/// Who holds stake and who is behind, keyed by identity: a validator running
/// several staked vote accounts is one validator. Counted per vote account,
/// active validators went negative.
#[derive(Debug, Default, PartialEq, Eq)]
struct StakeTally {
    /// Stake per identity, summed across that identity's vote accounts.
    staked: HashMap<Pubkey, u64>,
    delinquent: HashSet<Pubkey>,
    delinquent_stake: u64,
    non_delinquent_stake: u64,
}

/// Folds vote accounts into [`StakeTally`]. Takes the fields rather than the
/// accounts so it can be tested without a bank.
fn tally_stake(
    accounts: impl Iterator<Item = (Pubkey, u64, Option<Slot>)>,
    tip: Slot,
) -> StakeTally {
    let mut tally = StakeTally::default();
    for (identity, stake, last_vote) in accounts {
        // The bank holds every vote account ever created, most with no stake. A
        // validator is one with stake this epoch.
        if stake == 0 {
            continue;
        }
        let is_delinquent = last_vote
            .map(|vote| tip.saturating_sub(vote) > MAX_DELINQUENT_SLOT_DISTANCE)
            .unwrap_or(true);
        if is_delinquent {
            tally.delinquent.insert(identity);
            tally.delinquent_stake = tally.delinquent_stake.saturating_add(stake);
        } else {
            tally.non_delinquent_stake = tally.non_delinquent_stake.saturating_add(stake);
        }
        let total = tally.staked.entry(identity).or_insert(0);
        *total = total.saturating_add(stake);
    }
    tally
}

/// A vote further behind than this is reported as delinquent on the status
/// card. Looser than the cluster-wide threshold, since a brief lag on our own
/// node is normal.
const VOTE_BEHIND_LIMIT: u64 = 150;

/// How long replay may go without completing a slot before it reads as stalled.
const REPLAY_STALL_AFTER: Duration = Duration::from_secs(12);

/// How long the last vote may stand still before the node reads as delinquent
/// however close to the tip that vote was.
const VOTE_STALL_AFTER: Duration = Duration::from_secs(60);

/// This validator's own replay and vote health. The two durations sit at
/// either end of the argument list because swapping them compiles.
fn health_of(
    since_completed: Duration,
    completed_slot: Slot,
    voting: bool,
    vote_slot: Option<Slot>,
    behind: Option<u64>,
    since_vote_advance: Duration,
) -> Health {
    let replay = if since_completed > REPLAY_STALL_AFTER {
        "stalled"
    } else if completed_slot == 0 {
        "not_started"
    } else {
        "running"
    };

    // Checked first: a node that is not the voter has no votes of its own, and
    // every rule below would read the other machine's health. Not a fault; an
    // operator who has just failed over wants to see that it took.
    let vote = if !voting {
        "not_voting"
    } else {
        // A vote can be delinquent two ways: far behind, or not moving at all.
        match (vote_slot, behind) {
            (None, _) => "not_started",
            (Some(_), Some(behind)) if behind > VOTE_BEHIND_LIMIT => "delinquent",
            _ if since_vote_advance > VOTE_STALL_AFTER => "delinquent",
            _ => "voting",
        }
    };

    Health { replay, vote }
}

/// How the cluster's stake divides across client versions, counted over staked
/// identities so the two cards add up to each other. A staked identity gossip
/// is not hearing from has no version and is not the folded tail. Releases are
/// borrowed from the gossip strings; only the surviving rows allocate.
fn version_shares(
    staked: &HashMap<Pubkey, u64>,
    versions: &HashMap<Pubkey, String>,
) -> Vec<VersionShare> {
    let mut totals: HashMap<Option<&str>, (usize, u64)> = HashMap::new();
    for (identity, stake) in staked {
        let release = versions.get(identity).map(|version| release_of(version));
        let entry = totals.entry(release).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = entry.1.saturating_add(*stake);
    }

    let mut shares: Vec<VersionShare> = totals
        .into_iter()
        .map(|(version, (validators, stake))| VersionShare {
            version: version.map(str::to_string),
            validators,
            stake,
            other: false,
        })
        .collect();
    // Stake first: a version running on a crowd of unstaked nodes matters far
    // less than one carrying a slice of the vote.
    shares.sort_by(|a, b| {
        b.stake
            .cmp(&a.stake)
            .then_with(|| b.validators.cmp(&a.validators))
    });

    // Version strings arrive over gossip, so the tail is folded into one row to
    // keep the message a fixed size.
    if shares.len() > MAX_VERSIONS_REPORTED {
        let tail = shares.split_off(MAX_VERSIONS_REPORTED);
        shares.push(VersionShare {
            version: None,
            validators: tail.iter().map(|share| share.validators).sum(),
            stake: tail.iter().map(|share| share.stake).sum(),
            other: true,
        });
    }
    shares
}

/// Mean milliseconds per slot across a window of arrival times, in
/// nanoseconds. A true mean between the ends of the span, so it does not drift
/// and needs no seeding. `span_ms` bounds how far back from the newest sample
/// to reach; `u64::MAX` reads the whole window.
fn windowed_mean_nanos(window: &VecDeque<(Slot, u64)>, span_ms: u64) -> Option<u64> {
    let (last_slot, last_arrival) = window.back().copied()?;
    // The oldest sample inside the span. A span holding only the newest is
    // rejected below for spanning no slots.
    let (first_slot, first_arrival) = window
        .iter()
        .rev()
        .take_while(|(_, arrival)| last_arrival.saturating_sub(*arrival) <= span_ms)
        .last()
        .copied()?;
    let slots = last_slot
        .checked_sub(first_slot)
        .filter(|slots| *slots > 0)?;
    let millis = last_arrival.checked_sub(first_arrival)?;

    // Repair delivers shreds for many old slots at once, so their arrival times
    // bunch up. That is the download, not the cluster.
    let per_second = slots as f64 / (millis as f64 / 1_000.0).max(f64::MIN_POSITIVE);
    if per_second > CATCH_UP_SLOTS_PER_SECOND {
        return None;
    }

    let nanos = (millis as f64 / slots as f64) * 1_000_000.0;
    Some(nanos as u64)
}

/// Reads a frozen bank's own figures for one block. `transactions` and
/// `non_vote` are already differenced by the caller; the error and entry
/// counters are reset per bank, so differencing them would be wrong.
fn block_detail(
    bank: &Bank,
    transactions: u64,
    non_vote: u64,
    tips: Option<u64>,
    replay_micros: Option<u64>,
) -> BlockDetail {
    // Poisoned only if a replay thread panicked while holding it, in which case
    // the validator has more pressing problems than a missing bar.
    let (block_cost, block_cost_limit, account_cost_limit) = match bank.read_cost_tracker() {
        Ok(tracker) => (
            tracker.block_cost(),
            tracker.get_block_limit(),
            tracker.get_account_limit(),
        ),
        Err(_) => (0, 0, 0),
    };
    let fees = bank.get_collector_fee_details();

    BlockDetail {
        transactions,
        non_vote_transactions: non_vote,
        failed_transactions: bank.transaction_error_count(),
        entries: bank.transaction_entries_count(),
        block_cost,
        block_cost_limit,
        account_cost_limit,
        total_fees: fees.total_transaction_fee(),
        priority_fees: fees.total_priority_fee(),
        tips,
        replay_micros,
    }
}

/// What goes on the wire from a fill record: the slot is the entry's own.
fn arrival(fill: ShredFill) -> ShredArrival {
    ShredArrival {
        count: fill.shreds,
        repaired: fill.repaired,
        full_millis: fill.full_millis,
    }
}

/// The rate this epoch has actually run at, from the cluster's own clock: both
/// timestamps are stake-weighted medians agreed on chain, and the base grows
/// all epoch, which is what makes it settle. `None` until the clock's
/// whole-second granularity matters less than the answer.
fn epoch_anchored_nanos(clock: &Clock, start_slot: Slot, completed: Slot) -> Option<u64> {
    let slots = completed.saturating_sub(start_slot);
    if slots < EPOCH_RATE_MIN_ELAPSED_SLOTS {
        return None;
    }
    // A cluster whose clock has not advanced past the epoch's first slot says
    // nothing about the rate, and a negative span says the two disagree.
    let elapsed = clock
        .unix_timestamp
        .checked_sub(clock.epoch_start_timestamp)?;
    let elapsed = u64::try_from(elapsed).ok().filter(|secs| *secs > 0)?;
    elapsed.checked_mul(1_000_000_000)?.checked_div(slots)
}

/// The epoch end to count down to, holding the previous answer unless the new
/// one has moved further than `allowance`. The estimate underneath is a slot
/// duration multiplied by hundreds of thousands of slots, so it swings by
/// minutes on nothing. The allowance scales with the time left; see
/// [`EPOCH_END_DRIFT_DIVISOR`].
fn steady_epoch_end(
    held: Option<SystemTime>,
    estimate: SystemTime,
    allowance: Duration,
) -> SystemTime {
    let Some(held) = held else {
        return estimate;
    };
    // Either order, whichever way the estimate moved.
    let drift = held
        .duration_since(estimate)
        .or_else(|_| estimate.duration_since(held))
        .unwrap_or_default();
    if drift > allowance { estimate } else { held }
}

/// Folds a gossip version string to its release. A cluster mid-upgrade reports
/// `4.2.0`, `4.2.0-rc.0` and `4.2.0-rc.1`, which are one release.
fn release_of(version: &str) -> &str {
    match version.find(['-', '+']) {
        Some(at) => &version[..at],
        None => version,
    }
}

pub(crate) fn system_time_nanos(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            fixture::{Fixture, fixture},
            startup::StartupPublisher,
        },
        solana_core::validator::ValidatorStartProgress,
        solana_keypair::Keypair,
    };

    /// The arrival window as the collector holds it.
    fn window(samples: &[(Slot, u64)]) -> VecDeque<(Slot, u64)> {
        samples.iter().copied().collect()
    }

    // ---- slot levels ----------------------------------------------------

    #[test]
    fn test_level_reads_the_thresholds_most_settled_first() {
        // root 100, confirmed 110, finalized 90 — the ordinary arrangement.
        assert_eq!(level_for(80, 100, 110, 90), SlotLevel::Finalized);
        assert_eq!(level_for(95, 100, 110, 90), SlotLevel::Rooted);
        assert_eq!(
            level_for(105, 100, 110, 90),
            SlotLevel::OptimisticallyConfirmed
        );
        assert_eq!(level_for(120, 100, 110, 90), SlotLevel::Completed);
    }

    #[test]
    fn test_a_rooted_slot_is_not_demoted_when_confirmed_lags_the_root() {
        // During startup the commitment cache trails the root bank, so `confirmed`
        // can sit below a rooted slot.
        assert_eq!(level_for(100, 100, 50, 0), SlotLevel::Rooted);
    }

    #[test]
    fn test_the_boundaries_are_inclusive() {
        // A slot exactly at a threshold has reached it.
        assert_eq!(level_for(90, 100, 110, 90), SlotLevel::Finalized);
        assert_eq!(level_for(100, 100, 110, 90), SlotLevel::Rooted);
        assert_eq!(
            level_for(110, 100, 110, 90),
            SlotLevel::OptimisticallyConfirmed
        );
    }

    // ---- stake tally ----------------------------------------------------

    fn identity(seed: u8) -> Pubkey {
        Pubkey::new_from_array([seed; 32])
    }

    const TIP: Slot = 1_000;

    #[test]
    fn test_unstaked_vote_accounts_are_not_counted() {
        // The bank keeps every vote account ever created, tens of thousands with no
        // stake.
        let tally = tally_stake(
            [(identity(1), 0, Some(TIP)), (identity(2), 100, Some(TIP))].into_iter(),
            TIP,
        );
        assert_eq!(tally.staked.len(), 1);
        assert_eq!(tally.non_delinquent_stake, 100);
    }

    #[test]
    fn test_two_vote_accounts_on_one_identity_are_one_validator() {
        let tally = tally_stake(
            [(identity(1), 100, Some(TIP)), (identity(1), 250, Some(TIP))].into_iter(),
            TIP,
        );
        assert_eq!(tally.staked.len(), 1, "one identity is one validator");
        assert_eq!(tally.staked[&identity(1)], 350, "its stake is the sum");
    }

    #[test]
    fn test_active_validators_cannot_go_negative() {
        // Counted per vote account, an identity with two delinquent accounts
        // gave total 1 and delinquent 2, and the page renders the difference.
        let tally = tally_stake(
            [(identity(1), 100, Some(0)), (identity(1), 100, Some(0))].into_iter(),
            TIP,
        );
        assert_eq!(tally.staked.len(), 1);
        assert_eq!(tally.delinquent.len(), 1);
        assert_eq!(tally.staked.len() - tally.delinquent.len(), 0);
    }

    #[test]
    fn test_a_vote_account_that_never_voted_is_delinquent() {
        let tally = tally_stake([(identity(1), 100, None)].into_iter(), TIP);
        assert_eq!(tally.delinquent.len(), 1);
        assert_eq!(tally.delinquent_stake, 100);
        assert_eq!(tally.non_delinquent_stake, 0);
    }

    #[test]
    fn test_delinquency_is_decided_at_the_threshold() {
        let at = TIP - MAX_DELINQUENT_SLOT_DISTANCE;
        assert!(
            tally_stake([(identity(1), 1, Some(at))].into_iter(), TIP)
                .delinquent
                .is_empty(),
            "exactly at the limit is still voting"
        );
        assert_eq!(
            tally_stake([(identity(1), 1, Some(at - 1))].into_iter(), TIP)
                .delinquent
                .len(),
            1,
            "one slot past it is not"
        );
    }

    #[test]
    fn test_stake_splits_by_delinquency() {
        let tally = tally_stake(
            [
                (identity(1), 100, Some(TIP)),
                (identity(2), 30, Some(0)),
                (identity(3), 7, Some(TIP)),
            ]
            .into_iter(),
            TIP,
        );
        assert_eq!(tally.non_delinquent_stake, 107);
        assert_eq!(tally.delinquent_stake, 30);
    }

    // ---- epoch schedule ---------------------------------------------------

    #[test]
    fn test_the_turn_array_names_the_leader_the_schedule_names() {
        // Every slot must resolve through the two arrays to the key the schedule
        // gives it. An off-by-one in the stride names the wrong validator throughout.
        let harness = fixture();
        let collector = harness.collector();
        let bank = harness.working_bank();
        let schedule = bank.epoch_schedule();
        let epoch = schedule.get_epoch(bank.slot());
        let start = schedule.get_first_slot_in_epoch(epoch);
        let slots_in_epoch = schedule.get_slots_in_epoch(epoch);

        let (leaders, turns) = collector.epoch_turns(epoch, slots_in_epoch);
        assert!(!turns.is_empty(), "the fixture's own schedule is derivable");

        let stride = NUM_CONSECUTIVE_LEADER_SLOTS.get() as u64;
        assert_eq!((turns.len() as u64).saturating_mul(stride), slots_in_epoch);

        // Walked by turn rather than by slot, which keeps a runtime division the lint
        // would reject out of it.
        for (turn, index) in turns.iter().enumerate().take(16) {
            let slot = start.saturating_add((turn as u64).saturating_mul(stride));
            let expected = harness
                .ctx
                .leader_schedule_cache
                .slot_leader_at(slot, Some(&bank))
                .expect("the fixture leads its own schedule");
            assert_eq!(
                leaders[*index as usize],
                expected.id.to_string(),
                "slot {slot}"
            );
        }
    }

    #[test]
    fn test_an_epoch_with_no_derived_schedule_carries_no_turns() {
        // Empty, never partial. A short array is indistinguishable from a short
        // epoch, so a schedule that cannot be read has to say nothing at all.
        let harness = fixture();
        let collector = harness.collector();
        let bank = harness.working_bank();
        let epoch = bank.epoch_schedule().get_epoch(bank.slot());

        let (leaders, turns) = collector.epoch_turns(epoch.saturating_add(500), 432_000);
        assert!(leaders.is_empty());
        assert!(turns.is_empty());
    }

    #[test]
    fn test_a_mismatched_stride_publishes_nothing_rather_than_a_wrong_schedule() {
        // Asking for the real epoch against the wrong length is the same failure as
        // the schedule's repeat drifting from the constant.
        let harness = fixture();
        let collector = harness.collector();
        let bank = harness.working_bank();
        let epoch = bank.epoch_schedule().get_epoch(bank.slot());
        let slots_in_epoch = bank.epoch_schedule().get_slots_in_epoch(epoch);

        let (_, turns) = collector.epoch_turns(epoch, slots_in_epoch.saturating_add(4));
        assert!(turns.is_empty());
    }

    #[test]
    fn test_the_epoch_before_this_one_is_kept_rather_than_sent() {
        // Half a megabyte, wanted only by a page that has read back past a boundary.
        let harness = fixture();
        let mut collector = harness.collector();
        collector.tick();

        let held = harness.epochs.read().unwrap();
        let bank = harness.working_bank();
        let epoch = bank.epoch_schedule().get_epoch(bank.slot());
        assert!(
            held.iter().any(|record| record.epoch == epoch),
            "the current epoch is always among them"
        );
        // The previous one only where its schedule is still cached, which for a
        // fixture starting at slot nought it is not. Absent, not wrong.
        assert!(held.iter().all(|record| record.epoch <= epoch));
    }

    #[test]
    fn test_a_kept_past_epoch_carries_no_leader_slots_of_ours() {
        // They feed the countdown and the list for the epoch being led now.
        // Twenty kilobytes of slot numbers nothing asks a past epoch for.
        let harness = fixture();
        let collector = harness.collector();
        let bank = harness.working_bank();
        let epoch = bank.epoch_schedule().get_epoch(bank.slot());

        if let Some(record) = collector.epoch_record(&bank, epoch) {
            assert!(record.my_leader_slots.is_empty());
            assert!(!record.turns.is_empty(), "and it is otherwise whole");
        }
    }

    #[test]
    fn test_the_epoch_message_is_built_once_and_not_at_every_poll() {
        // A hundred thousand turns on mainnet; rebuilt at the poll rate it is two
        // hundred kilobytes five times a second.
        let harness = fixture();
        let mut collector = harness.collector();
        collector.tick();
        let first = collector.epoch_published;
        assert!(first.is_some());

        harness.advance_to(8);
        collector.tick();
        assert_eq!(collector.epoch_published, first);
    }

    #[test]
    fn test_a_swapped_identity_rebuilds_the_epoch_rather_than_keeping_the_old_answer() {
        // A validator that boots on a dummy identity and swaps had the dummy's leader
        // slots, none, latched for the epoch while the countdown beside it kept
        // working.
        let harness = fixture();
        let mut collector = harness.collector();
        collector.tick();
        let first = collector.epoch_published;
        assert!(first.is_some());

        harness
            .ctx
            .cluster_info
            .set_keypair(Arc::new(Keypair::new()));
        harness.advance_to(8);
        collector.tick();

        assert_ne!(
            collector.epoch_published, first,
            "the epoch must be rebuilt for whoever this validator is now"
        );
    }

    #[test]
    fn test_the_skip_rate_starts_again_for_a_swapped_identity() {
        // Same latch, same reason. The slot list it walks belongs to one
        // validator, and after a swap it belongs to somebody else.
        let harness = fixture();
        let mut collector = harness.collector();
        collector.tick();

        // Called directly: the skip rate rides the slow tier, and this is about the
        // latch.
        let stranger = Pubkey::new_unique();
        collector.skip_epoch = Some((0, stranger));
        collector.skip_next_index = 99;
        collector.collect_skip_rate(&harness.working_bank());

        assert_ne!(
            collector.skip_epoch,
            Some((0, stranger)),
            "the latch must not still belong to the identity that has gone"
        );
        // Not nought: the reset is followed by the walk over every passed leader slot.
        // Compared against a collector meeting the epoch fresh rather than a constant.
        let restarted = {
            let mut control = harness.collector();
            control.collect_skip_rate(&harness.working_bank());
            control.skip_next_index
        };
        assert_eq!(
            collector.skip_next_index, restarted,
            "the walk restarts rather than carrying an index into another schedule"
        );
    }

    // ---- what this build is ---------------------------------------------

    #[test]
    fn test_the_build_reports_its_client_beside_its_version() {
        let harness = fixture();
        harness.collector().publish_static();
        let client = solana_version::Version::this_build().client().to_string();

        // Asserting a name would only assert which fork the tests ran from. What can
        // break is the header disagreeing with the validator's own startup line.
        let published = harness.published_key("summary", "client").unwrap();
        assert!(
            published.contains(&format!(r#""value":"{client}""#)),
            "published {published}, which does not carry the client {client}"
        );
        assert!(
            solana_version::Version::this_build()
                .as_detailed_string()
                .contains(&format!("client:{client}")),
            "the header and the startup log would name the client differently"
        );

        // And the reason it is published at all: the version it sits beside
        // does not carry it, which is why every fork's header read the same.
        let version = harness.published_key("summary", "version").unwrap();
        assert!(
            !version.contains(&client),
            "{version} already names the client, so publishing it apart is dead weight"
        );
    }

    // ---- version shares -------------------------------------------------

    fn staked(entries: &[(u8, u64)]) -> HashMap<Pubkey, u64> {
        entries
            .iter()
            .map(|&(seed, stake)| (identity(seed), stake))
            .collect()
    }

    fn gossiped(entries: &[(u8, &str)]) -> HashMap<Pubkey, String> {
        entries
            .iter()
            .map(|&(seed, version)| (identity(seed), version.to_string()))
            .collect()
    }

    #[test]
    fn test_prerelease_tags_fold_into_one_release_row() {
        // A cluster mid-upgrade reports 4.2.0, 4.2.0-rc.0 and 4.2.0-rc.1.
        // Counted apart they understate how much stake has actually moved.
        let shares = version_shares(
            &staked(&[(1, 10), (2, 10), (3, 10)]),
            &gossiped(&[(1, "4.2.0"), (2, "4.2.0-rc.0"), (3, "4.2.0-rc.1")]),
        );
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].version.as_deref(), Some("4.2.0"));
        assert_eq!(shares[0].validators, 3);
        assert_eq!(shares[0].stake, 30);
    }

    #[test]
    fn test_rows_are_ordered_by_stake_not_by_node_count() {
        // The whole point of the panel: a version on a crowd of lightly staked
        // nodes matters less than one carrying a slice of the vote.
        let shares = version_shares(
            &staked(&[(1, 1_000), (2, 1), (3, 1), (4, 1)]),
            &gossiped(&[(1, "4.2.0"), (2, "4.1.0"), (3, "4.1.0"), (4, "4.1.0")]),
        );
        assert_eq!(shares[0].version.as_deref(), Some("4.2.0"));
        assert_eq!(shares[0].stake, 1_000);
        assert_eq!(shares[1].validators, 3, "more nodes, less stake, second");
    }

    #[test]
    fn test_an_unstaked_gossip_node_is_not_counted() {
        // The counts are read beside the validator card, which counts staked
        // identities. Counting gossip peers here summed past that total.
        let shares = version_shares(&staked(&[]), &gossiped(&[(1, "4.2.0")]));
        assert!(shares.is_empty());
    }

    #[test]
    fn test_the_counts_sum_to_the_staked_validator_total() {
        let staked = staked(&[(1, 10), (2, 10), (3, 10)]);
        // Two unstaked peers gossip a version alongside them.
        let shares = version_shares(
            &staked,
            &gossiped(&[(1, "4.2.0"), (2, "4.3.0"), (7, "4.3.0"), (8, "4.3.0")]),
        );
        let counted: usize = shares.iter().map(|share| share.validators).sum();
        assert_eq!(counted, staked.len());
    }

    #[test]
    fn test_a_staked_validator_gossip_cannot_see_reports_no_version() {
        let shares = version_shares(&staked(&[(9, 500)]), &gossiped(&[]));
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].version, None);
        assert_eq!(shares[0].stake, 500);
        assert!(
            !shares[0].other,
            "no version reported is not the folded tail"
        );
    }

    #[test]
    fn test_the_tail_folds_into_one_flagged_row() {
        // Version strings arrive over gossip, so the number of distinct values
        // is not ours to bound; the message has to stay a fixed size.
        let peers: Vec<(u8, String)> = (1..=9).map(|seed| (seed, format!("4.{seed}.0"))).collect();
        let versions = gossiped(
            &peers
                .iter()
                .map(|(seed, version)| (*seed, version.as_str()))
                .collect::<Vec<_>>(),
        );
        // Distinct stakes, so which rows survive the fold does not come down to
        // the order a hash map happened to iterate in.
        let stakes: Vec<(u8, u64)> = (1..=9).map(|seed| (seed, u64::from(seed))).collect();
        let shares = version_shares(&staked(&stakes), &versions);

        assert_eq!(shares.len(), MAX_VERSIONS_REPORTED + 1);
        let last = shares.last().unwrap();
        assert!(last.other, "the fold is flagged rather than inferred");
        assert_eq!(last.validators, 9 - MAX_VERSIONS_REPORTED);
        assert!(
            shares[..MAX_VERSIONS_REPORTED].iter().all(|s| !s.other),
            "only the tail row is flagged"
        );
    }

    // ---- health ---------------------------------------------------------

    const FRESH: Duration = Duration::from_secs(1);

    #[test]
    fn test_replay_is_stalled_when_no_slot_completes() {
        assert_eq!(
            health_of(Duration::from_secs(13), 100, true, Some(99), Some(1), FRESH).replay,
            "stalled"
        );
        assert_eq!(
            health_of(FRESH, 100, true, Some(99), Some(1), FRESH).replay,
            "running"
        );
    }

    #[test]
    fn test_replay_has_not_started_before_the_first_slot() {
        // Slot zero means nothing has completed yet, which is not a stall.
        assert_eq!(
            health_of(FRESH, 0, true, None, None, FRESH).replay,
            "not_started"
        );
    }

    /// The integer a summary key was published with, if it carried one.
    fn published_number(harness: &Fixture, key: &str) -> Option<u64> {
        let message = harness.published_key("summary", key)?;
        let after = message.rsplit_once(r#""value":"#)?.1;
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    }

    #[test]
    fn test_the_cluster_distance_is_measured_against_the_cluster() {
        // The figure this replaced was replay against our own vote, which reads
        // nought however far back the node is.
        let harness = fixture();
        harness.advance_to(64);
        harness.set_cluster_tip(10_000);
        harness.collector().tick();

        let completed = published_number(&harness, "completed_slot").unwrap();
        assert_eq!(
            published_number(&harness, "behind_cluster"),
            Some(10_000 - completed),
            "the distance is the cluster's tip less what this node has replayed"
        );
    }

    #[test]
    fn test_a_node_the_cluster_has_not_outrun_is_not_behind() {
        // Saturating rather than negative. Being ahead of the last certificate
        // seen is ordinary, and is not a distance.
        let harness = fixture();
        harness.advance_to(64);
        harness.set_cluster_tip(1);
        harness.collector().tick();

        assert_eq!(published_number(&harness, "behind_cluster"), Some(0));
    }

    #[test]
    fn test_nothing_is_claimed_before_a_certificate_arrives() {
        // On a fresh start nought would say this node was in step with a cluster it
        // has not heard from.
        let harness = fixture();
        harness.advance_to(64);
        harness.collector().tick();

        let published = harness.published_key("summary", "behind_cluster").unwrap();
        assert!(published.contains(r#""value":null"#), "got {published}");
    }

    #[test]
    fn test_a_validator_on_its_backup_identity_is_not_voting() {
        // The vote account is voted from wherever the voting identity runs, so its
        // last vote looks healthy.
        assert_eq!(
            health_of(FRESH, 100, false, Some(99), Some(1), FRESH).vote,
            "not_voting"
        );
    }

    #[test]
    fn test_not_voting_outranks_every_other_reading() {
        // Including the two that would otherwise call it delinquent: a node that is
        // not the voter has no votes to be late with.
        assert_eq!(
            health_of(
                FRESH,
                100,
                false,
                Some(50),
                Some(VOTE_BEHIND_LIMIT + 1),
                FRESH
            )
            .vote,
            "not_voting"
        );
        assert_eq!(
            health_of(FRESH, 100, false, None, None, Duration::from_secs(3_600)).vote,
            "not_voting"
        );
    }

    #[test]
    fn test_replay_is_reported_whether_or_not_this_node_votes() {
        // A backup still replays, and an operator watching a failover wants to
        // know it is keeping up before handing the identity back.
        assert_eq!(
            health_of(FRESH, 100, false, None, None, FRESH).replay,
            "running"
        );
    }

    #[test]
    fn test_a_vote_far_behind_the_tip_is_delinquent() {
        assert_eq!(
            health_of(
                FRESH,
                100,
                true,
                Some(50),
                Some(VOTE_BEHIND_LIMIT + 1),
                FRESH
            )
            .vote,
            "delinquent"
        );
        assert_eq!(
            health_of(FRESH, 100, true, Some(50), Some(VOTE_BEHIND_LIMIT), FRESH).vote,
            "voting"
        );
    }

    #[test]
    fn test_a_vote_that_is_close_but_frozen_is_delinquent() {
        // The case the distance alone misses: near the tip and not moving.
        assert_eq!(
            health_of(FRESH, 100, true, Some(99), Some(1), Duration::from_secs(61)).vote,
            "delinquent"
        );
    }

    #[test]
    fn test_a_node_that_has_never_voted_is_not_delinquent() {
        // An unstaked node is not a failing one, however long it sits there.
        assert_eq!(
            health_of(FRESH, 100, true, None, None, Duration::from_secs(3_600)).vote,
            "not_started"
        );
    }

    /// `count` samples `slot_ms` apart from `from`, newest last.
    fn steady_window(from: (Slot, u64), count: u64, slot_ms: u64) -> VecDeque<(Slot, u64)> {
        let (slot, arrival) = from;
        (0..count)
            .map(|index| {
                (
                    slot.saturating_add(index),
                    arrival.saturating_add(index.saturating_mul(slot_ms)),
                )
            })
            .collect()
    }

    #[test]
    fn test_a_full_window_averages_the_whole_of_it() {
        // Five minutes of slots at 420ms, read whole, as the epoch countdown
        // will read them.
        let samples = steady_window((100, 1_000), SLOT_TIME_WINDOW_SLOTS as u64, 420);
        assert_eq!(windowed_mean_nanos(&samples, u64::MAX), Some(420_000_000));
    }

    #[test]
    fn test_a_full_window_of_replay_is_still_rejected() {
        // Widening the window made the catch-up guard less twitchy, which is
        // the point, but it must not have made it blind.
        let samples = steady_window((100, 1_000), SLOT_TIME_WINDOW_SLOTS as u64, 10);
        assert_eq!(windowed_mean_nanos(&samples, u64::MAX), None);
    }

    #[test]
    fn test_the_readout_span_ignores_samples_older_than_itself() {
        // Five minutes at 400ms, then a minute at 500ms. Read whole the slowdown is
        // diluted; read over the readout's span it is the whole answer.
        let mut samples = steady_window((100, 1_000), SLOT_TIME_WINDOW_SLOTS as u64, 400);
        let (last_slot, last_arrival) = *samples.back().unwrap();
        samples.extend(steady_window(
            (
                last_slot.saturating_add(1),
                last_arrival.saturating_add(500),
            ),
            120,
            500,
        ));
        assert_eq!(
            windowed_mean_nanos(&samples, SLOT_READOUT_SPAN_MS),
            Some(500_000_000)
        );
        assert!(windowed_mean_nanos(&samples, u64::MAX).unwrap() < 420_000_000);
    }

    // ---- upcoming leaders -----------------------------------------------

    /// The slot numbers of the published upcoming list, in order.
    fn upcoming_slots(harness: &crate::fixture::Fixture) -> Vec<u64> {
        let published = harness
            .published_key("slot", "upcoming")
            .expect("upcoming is published");
        let envelope: serde_json::Value = serde_json::from_str(&published).unwrap();
        envelope["value"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["slot"].as_u64().unwrap())
            .collect()
    }

    #[test]
    fn test_upcoming_starts_past_the_tip_and_runs_contiguously() {
        let harness = fixture();
        harness.advance_to(64);
        let mut collector = harness.collector();
        let root_bank = harness.bank_forks.read().unwrap().root_bank();

        collector.collect_upcoming(&root_bank, 64);
        let slots = upcoming_slots(&harness);

        assert!(!slots.is_empty(), "the schedule for this epoch is known");
        assert_eq!(slots[0], 65, "starts past the slot being worked on");
        let contiguous: Vec<u64> = (0..slots.len() as u64)
            .map(|index| index.saturating_add(65))
            .collect();
        assert_eq!(slots, contiguous, "no gaps");
        assert!(
            slots.len() as u64 <= UPCOMING_SLOTS,
            "bounded at {UPCOMING_SLOTS}"
        );
    }

    #[test]
    fn test_upcoming_marks_our_own_slots() {
        // The fixture stakes this validator alone, so it leads every slot.
        let harness = fixture();
        harness.advance_to(8);
        let mut collector = harness.collector();
        let root_bank = harness.bank_forks.read().unwrap().root_bank();

        collector.collect_upcoming(&root_bank, 8);
        let published = harness.published_key("slot", "upcoming").unwrap();
        assert!(
            published.contains(r#""mine":true"#),
            "the only staked leader should be marked as ours"
        );
    }

    // ---- epoch countdown ------------------------------------------------

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH
            .checked_add(Duration::from_secs(seconds))
            .unwrap()
    }

    const ALLOWANCE: Duration = Duration::from_secs(60);

    /// A clock `elapsed` seconds into its epoch.
    fn clock_at(elapsed: i64) -> Clock {
        Clock {
            epoch_start_timestamp: 1_700_000_000,
            unix_timestamp: 1_700_000_000_i64.saturating_add(elapsed),
            ..Clock::default()
        }
    }

    #[test]
    fn test_the_epoch_rate_is_its_own_elapsed_time_over_its_own_slots() {
        // Six hours across sixty thousand slots is 360ms a slot.
        let nanos = epoch_anchored_nanos(&clock_at(21_600), 100, 60_100);
        assert_eq!(nanos, Some(360_000_000));
    }

    #[test]
    fn test_the_epoch_rate_waits_for_the_epoch_to_get_going() {
        // The cluster clock moves in whole seconds, so early on the error in
        // that second is worth more than the answer.
        assert_eq!(epoch_anchored_nanos(&clock_at(400), 100, 1_100), None);
    }

    #[test]
    fn test_a_clock_that_has_not_moved_yields_no_rate() {
        // Nothing to divide, and a negative span means the two disagree.
        assert_eq!(epoch_anchored_nanos(&clock_at(0), 100, 60_100), None);
        assert_eq!(epoch_anchored_nanos(&clock_at(-10), 100, 60_100), None);
    }

    #[test]
    fn test_the_first_estimate_is_adopted_as_it_stands() {
        assert_eq!(steady_epoch_end(None, at(10_000), ALLOWANCE), at(10_000));
    }

    #[test]
    fn test_an_estimate_that_barely_moved_does_not_move_the_countdown() {
        // Half a minute either way, on a figure hours out. Following this is
        // what made the readout restless while telling nobody anything.
        let held = at(10_000);
        for estimate in [at(10_030), at(9_970)] {
            assert_eq!(steady_epoch_end(Some(held), estimate, ALLOWANCE), held);
        }
    }

    #[test]
    fn test_real_drift_is_followed_in_one_step() {
        let held = at(10_000);
        assert_eq!(
            steady_epoch_end(Some(held), at(10_600), ALLOWANCE),
            at(10_600),
            "ten minutes is the estimate genuinely changing, not noise"
        );
    }

    #[test]
    fn test_drift_exactly_at_the_allowance_is_still_held() {
        // The boundary belongs to the quiet side: a countdown that steps for a
        // difference this small is a countdown that steps constantly.
        let held = at(10_000);
        assert_eq!(steady_epoch_end(Some(held), at(10_060), ALLOWANCE), held);
    }

    #[test]
    fn test_the_allowance_scales_with_what_is_left() {
        // A fixed sixty seconds is a seventh of a millisecond of slot time across a
        // fresh epoch, which every sample turnover clears.
        let six_hours = Duration::from_secs(21_600);
        let allowance = six_hours.checked_div(EPOCH_END_DRIFT_DIVISOR).unwrap();
        assert!(allowance > Duration::from_secs(300), "{allowance:?}");

        let one_hour = Duration::from_secs(3_600);
        let allowance = one_hour.checked_div(EPOCH_END_DRIFT_DIVISOR).unwrap();
        assert!(allowance < Duration::from_secs(60), "{allowance:?}");
    }

    // ---- catching up ----------------------------------------------------

    /// A collector holding `count` samples ending at `last`, 400ms apart.
    fn collector_following(last: Slot, count: u64) -> Collector {
        let mut collector = fixture().collector();
        let first = last.saturating_sub(count.saturating_sub(1));
        collector.slot_time_window = steady_window((first, 1_000), count, 400);
        collector
    }

    #[test]
    fn test_the_marker_waits_for_the_window_to_fill() {
        // A validator that has loaded a snapshot and received nothing sits at zero
        // distance without having caught up.
        let mut collector = collector_following(300_000_000, 4);
        collector.mark_caught_up(300_000_000, 300_000_000);
        assert_eq!(collector.caught_up_at, None);
        assert_eq!(collector.slot_time_window.len(), 4, "nothing discarded");
    }

    #[test]
    fn test_the_marker_waits_for_replay_to_reach_the_tip() {
        let mut collector = collector_following(300_000_000, CAUGHT_UP_MIN_SAMPLES as u64);
        // Replaying, and still a thousand slots behind what it holds.
        collector.mark_caught_up(300_001_000, 300_000_000);
        assert_eq!(collector.caught_up_at, None);
    }

    #[test]
    fn test_catching_up_discards_everything_measured_while_behind() {
        let mut collector = collector_following(300_000_000, CAUGHT_UP_MIN_SAMPLES as u64);
        // Trailing the tip by a thousand slots, then level.
        collector.mark_caught_up(300_001_000, 300_000_000);
        collector.mark_caught_up(300_000_002, 300_000_000);

        assert_eq!(
            collector.caught_up_at,
            Some(300_000_000_u64.saturating_add(CAUGHT_UP_MARGIN_SLOTS))
        );
        assert!(
            collector.slot_time_window.is_empty(),
            "every sample was taken while behind, so none of it describes the cluster"
        );
    }

    #[test]
    fn test_starting_level_keeps_the_samples_it_already_has() {
        // A restart that never trailed the tip has measured nothing but the cluster.
        let mut collector = collector_following(300_000_000, CAUGHT_UP_MIN_SAMPLES as u64);
        collector.mark_caught_up(300_000_000, 300_000_000);

        assert!(collector.caught_up_at.is_some());
        assert_eq!(
            collector.slot_time_window.len(),
            CAUGHT_UP_MIN_SAMPLES,
            "nothing was measured while behind, so nothing is thrown away"
        );
    }

    #[test]
    fn test_the_collector_keeps_the_phases_the_boot_thread_timed() {
        // Each side had a publisher of its own, and the collector's, starting
        // empty, published `running` with no phases at all. The page then read
        // startup as nought and put the whole boot into catching up.
        let harness = fixture();
        let shared = Arc::new(Mutex::new(StartupPublisher::default()));
        shared
            .lock()
            .unwrap()
            .publish(&harness.publisher, ValidatorStartProgress::CleaningAccounts);
        shared
            .lock()
            .unwrap()
            .publish(&harness.publisher, ValidatorStartProgress::Running);

        let mut collector = harness.collector_with_startup(shared);
        collector.tick();

        let last = harness
            .published()
            .into_iter()
            .rfind(|message| message.contains(r#""key":"startup_progress""#))
            .expect("the collector publishes startup progress");
        assert!(
            last.contains(r#""phase":"cleaning_accounts""#),
            "the collector's own message must carry what the boot thread timed: {last}"
        );
    }

    #[test]
    fn test_a_stale_tip_below_the_snapshot_does_not_count_as_caught_up() {
        // The blockstore keeps optimistic slots from before a restart, so the
        // first tip read is below what replay started from. That stamped catch-up
        // at nought seconds on a node still twenty-five slots behind.
        let harness = fixture();
        harness.advance_to(64);
        let mut collector = harness.collector();

        harness.set_cluster_tip(1);
        collector.tick();
        assert!(
            harness
                .published_key("summary", "caught_up_time_nanos")
                .is_none()
        );
    }

    #[test]
    fn test_catching_up_is_stamped_once_a_tip_seen_ahead_is_reached() {
        let harness = fixture();
        harness.advance_to(64);
        let mut collector = harness.collector();

        // Behind: a tip ahead of replay is seen, and nothing is stamped.
        harness.set_cluster_tip(100);
        collector.tick();
        assert!(
            harness
                .published_key("summary", "caught_up_time_nanos")
                .is_none()
        );

        // Level with it.
        harness.advance_to(101);
        collector.tick();
        assert!(
            harness
                .published_key("summary", "caught_up_time_nanos")
                .is_some()
        );
    }

    #[test]
    fn test_the_marker_is_set_once_and_falling_behind_does_not_move_it() {
        // Re-testing the rate continuously and clearing the window fed back on
        // itself: a shorter window trips more easily.
        let mut collector = collector_following(300_000_000, CAUGHT_UP_MIN_SAMPLES as u64);
        collector.mark_caught_up(300_000_000, 300_000_000);
        let marked = collector.caught_up_at;

        collector.slot_time_window = steady_window((300_001_000, 1_000), 200, 400);
        collector.mark_caught_up(300_099_000, 300_001_000);

        assert_eq!(collector.caught_up_at, marked, "the marker never moves");
        assert_eq!(
            collector.slot_time_window.len(),
            200,
            "and nothing is discarded a second time"
        );
    }

    #[test]
    fn test_mean_spans_the_ends_of_the_window() {
        // Ten slots over four seconds is 400ms each, however the middle fell.
        let samples = window(&[(100, 1_000), (105, 3_100), (110, 5_000)]);
        assert_eq!(windowed_mean_nanos(&samples, u64::MAX), Some(400_000_000));
    }

    #[test]
    fn test_one_slow_slot_barely_moves_the_mean() {
        // 150 slots at 400ms with a single two-second slot among them.
        let steady = 150_u64 * 400;
        assert_eq!(
            windowed_mean_nanos(&window(&[(0, 0), (150, steady + 1_600)]), u64::MAX),
            Some(410_666_666)
        );
    }

    #[test]
    fn test_repair_burst_is_not_reported_as_the_cluster_rate() {
        // A thousand slots arriving in two seconds is a download, not a cluster.
        assert_eq!(
            windowed_mean_nanos(&window(&[(0, 0), (1_000, 2_000)]), u64::MAX),
            None
        );
    }

    #[test]
    fn test_window_that_cannot_span_two_slots_reports_nothing() {
        assert_eq!(windowed_mean_nanos(&window(&[]), u64::MAX), None);
        assert_eq!(
            windowed_mean_nanos(&window(&[(100, 1_000)]), u64::MAX),
            None
        );
    }

    #[test]
    fn test_releases_fold_their_prerelease_tags() {
        assert_eq!(release_of("4.2.0-rc.1"), "4.2.0");
        assert_eq!(release_of("0.1102.0-beta.40201"), "0.1102.0");
        assert_eq!(release_of("4.2.0"), "4.2.0");
        assert_eq!(release_of("1.18.23+build7"), "1.18.23");
    }

    #[test]
    fn test_folding_leaves_strings_that_are_not_semver_alone() {
        // Gossip is not obliged to send semver, and a version that cannot be
        // parsed is better reported verbatim than dropped.
        assert_eq!(release_of(""), "");
        assert_eq!(release_of("unknown"), "unknown");
        assert_eq!(release_of("-leading"), "");
    }
}
