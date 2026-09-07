//! Counters lifted from the metrics points the validator submits about itself.
//!
//! The counters behind most panels are private to the module keeping them and
//! reset as they are reported, so this watches the points on their way out
//! instead. The observer runs on the submitting thread: a name comparison, and
//! for the few points wanted a scan of fields into atomics. Only the per-slot
//! points take a lock, and those arrive once per leader slot.
//!
//! The points carry deltas, so accumulating them gives totals that only climb
//! and can be differenced between readings like every other rate here.

use {
    serde::Serialize,
    solana_clock::Slot,
    solana_metrics::datapoint::DataPoint,
    std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    },
};

/// The point carrying the accounts read cache figures, submitted once a second
/// by the accounts database with the counters reset as it reads them.
const ACCOUNTS_DB_TIMINGS: &str = "accounts_db_store_timings";

/// The two shred receivers, one per socket: turbine, and repair for what this
/// validator had to ask another node for.
const SHREDS_TURBINE: &str = "shred_fetch_receiver";
const SHREDS_REPAIR: &str = "shred_fetch_repair_receiver";

/// The receivers on the other two UDP ports the socket panel lists. TPU and TPU
/// forwards speak QUIC, whose counters are transactions rather than datagrams,
/// and serve repair's receiver is never reported.
const GOSSIP_RECEIVER: &str = "gossip_receiver";

/// The two UDP senders that report what they sent, under the name each was
/// given. Turbine goes out over XDP and reports shred counts only, so egress
/// can be split this far and no further.
const GOSSIP_SENDER: &str = "Gossip";
const REPAIR_SENDER: &str = "Repair";
const SENT_BYTES: &str = "streamer-send-bytes_total";
const SENT_MILLIS: &str = "streamer-send-sample_duration_ms";
const TPU_VOTE_RECEIVER: &str = "tpu_vote_receiver";

/// Packets seen, which for the shred receivers is shreds.
const PACKETS_COUNT: &str = "packets_count";

/// The scheduler's account of everything handed to it, reported once a second
/// with its counters reset as it reports. Submitted only when there is something
/// to report, and through `solana_metrics::submit` directly, so it arrives at
/// any log level.
const SCHEDULER_COUNTS: &str = "banking_stage_scheduler_counts";
/// Why a worker's transaction never reached the block. Same worker, tick and
/// `id` as the counts point, so the two are read into one set of counters.
const WORKER_ERROR_METRICS: &str = "banking_stage_worker_error_metrics";

/// Where the accounts database served reads from and what it wrote, across the
/// three points it reports on.
const ACCOUNTS_LOADS: &str = "accounts_db_load_accounts";
const ACCOUNTS_STORES: &str = "accounts_db-stores";
const ACCOUNTS_FLUSH: &str = "accounts_db-flush_accounts_cache";

/// The program cache's counters, reported once per bank and reset as they are,
/// so each point is one slot's work. The cache object itself is behind
/// `dev-context-only-utils`, and polling it would miss most of each slot.
const PROGRAM_CACHE: &str = "loaded-programs-cache-stats";

/// The QUIC listeners, one point per port. Only the TPU port feeds the stages
/// below, but the connection and stream figures are worth having for all three.
const QUIC_TPU: &str = "quic_streamer_tpu";
const QUIC_TPU_FORWARDS: &str = "quic_streamer_tpu_forwards";
const QUIC_TPU_VOTE: &str = "quic_streamer_tpu_vote";

/// Signature verification for everything that is not a vote. `tpu-vote-verifier`
/// is left alone: votes never reach the scheduler below.
const TPU_VERIFIER: &str = "tpu-verifier";

/// The bundle stage's loop, on builds that have one. Bundles arrive over gRPC
/// and skip the QUIC ports and sigverify, so this annotates the executed stage
/// rather than adding one. Silent where there is no bundle stage, where it is
/// idle, and under BAM, which supersedes it.
const BUNDLE_STAGE: &str = "bundle_stage-loop_stats";

/// The worker threads, one point each under an `id` tag that is not read:
/// summing them gives the stage's total. Submitted at trace level, which does
/// not matter here because the observer runs before the level is consulted.
const WORKER_COUNTS: &str = "banking_stage_worker_counts";

/// The same counters again per leader slot, with the slot as a field. Submitted
/// only while this validator is producing, so it exists for exactly the slots
/// this node led.
const SCHEDULER_SLOT_COUNTS: &str = "banking_stage_scheduler_slot_counts";

/// How the XDP transmit path is set up, on a validator running one. Submitted
/// on an interval only where an XDP config was given, so its absence is what
/// says XDP is off. One transmitter serves turbine, repair and gossip alike.
const XDP_NETWORK_CONFIG: &str = "xdp-network-config";

/// Every slot's replay, timed. Sent with `datapoint_info!`, so it arrives
/// unless the validator logs below `solana=info`.
const REPLAY_SLOT_STATS: &str = "replay-slot-stats";

/// Every slot the blockstore fills, timed from its first shred, with how many
/// of its shreds had to be repaired. Sent with `datapoint_info!`.
const SHRED_FULL: &str = "shred_insert_is_full";

/// What the cost tracker made of a block. Reported for every slot and tagged
/// with whether it was ours; only ours are kept.
const COST_TRACKER: &str = "cost_tracker_stats";

/// The tag saying whether the reporting node produced the block.
const IS_LEADER: &str = "is_leader";

/// The field naming the slot a point covers.
const SLOT: &str = "slot";

/// Replayed slots kept, about a minute and a half. Shorter samples missed the
/// program cache's mean by a third, because compilation arrives in bursts.
const REPLAY_SLOTS: usize = 256;

/// Filled slots kept. A slot's record is read when replay freezes it, and
/// during a catch-up the blockstore fills a long way ahead of replay.
const SHRED_FILLS: usize = 4096;

/// Leader slots kept, matched to the produced block panel's retention so every
/// block it shows still has its waterfall and costs.
const SLOT_WATERFALLS: usize = 500;

/// The tag naming which scheduler reported a point. Absent on a stock
/// validator; jito tags both of its schedulers.
const SCHEDULER_ID: &str = "id";

/// The id the validator's own scheduler reports under.
const OWN_SCHEDULER_ID: &str = "0";

/// What the accounts database read, wrote, and is holding. Reads are counted in
/// accounts rather than bytes, because nothing on the load path counts bytes.
#[derive(Debug, Default)]
pub struct AccountsCounters {
    /// Accounts served from each of the three places a read can be answered
    /// from. The last is the one that touches a file.
    pub loaded_from_write_cache: AtomicU64,
    pub loaded_from_read_cache: AtomicU64,
    pub loaded_from_storage: AtomicU64,

    /// Accounts written out of the cache to storage, and their size.
    pub stored_accounts: AtomicU64,
    pub stored_bytes: AtomicU64,

    /// Levels, not counts: storage that exists and storage still live. The
    /// difference is what shrink reclaims.
    pub storage_bytes: AtomicU64,
    pub storage_alive_bytes: AtomicU64,
    pub storage_count: AtomicU64,
    /// Bytes held by the read cache, and entries in it.
    pub cache_bytes: AtomicU64,
    pub cache_entries: AtomicU64,
}

/// How the program cache is faring: what replay asked of it, and what it lost.
#[derive(Debug, Default)]
pub struct ProgramCacheCounters {
    /// Programs replay wanted that were already compiled, and that were not.
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    /// Compiled programs dropped to keep the cache within its entry limit,
    /// which is the usual reason a hit rate falls.
    pub evictions: AtomicU64,
    /// An entry that had been unloaded being compiled again — the cost of an
    /// eviction, paid later.
    pub reloads: AtomicU64,
    /// Programs added to the cache, and additions that were thrown away
    /// because the fork they belonged to was gone by the time they finished.
    pub insertions: AtomicU64,
    pub lost_insertions: AtomicU64,
    /// An entry already present being compiled a second time by mistake.
    pub replacements: AtomicU64,
    /// Programs used once and then evicted, which is cache space spent for
    /// nothing.
    pub one_hit_wonders: AtomicU64,
    /// Entries dropped because their fork was abandoned, and because they were
    /// not recompiled for the incoming epoch.
    pub prunes_orphan: AtomicU64,
    pub prunes_environment: AtomicU64,
    /// Keys left holding no versions at all once pruning had finished.
    pub empty_entries: AtomicU64,

    /// Entries loaded when an eviction last ran. A level, written only when an
    /// eviction happens and reset with each bank, so the panel takes the peak across
    /// its window rather than the latest reading.
    pub water_level: AtomicU64,
}

/// One QUIC port: who was let in, what they sent, and what got through.
///
/// Most fields are reported with `swap(0)` and accumulate here.
/// `total_incoming_connection_attempts` is reported with `load`, already
/// cumulative, and is stored instead; the last two are levels. Accumulating the
/// cumulative one would square it within a minute.
#[derive(Debug, Default)]
pub struct QuicCounters {
    /// Connections offered, cumulative on the wire. The denominator for
    /// everything else in this group.
    pub offered: AtomicU64,
    /// Shed before the handshake because the port was over its overall rate.
    pub shed_all: AtomicU64,
    /// Shed before the handshake because one address was over its rate.
    pub shed_address: AtomicU64,
    /// Refused because the endpoint already held all the connections it may.
    pub refused_full: AtomicU64,
    /// Reached the handshake and ran out of time.
    pub handshake_timeout: AtomicU64,
    /// Reached the handshake and failed it.
    pub handshake_error: AtomicU64,
    /// Completed the handshake. The one checkpoint between the offer and the
    /// connection table; see `QuicTotals` for why it matters.
    pub handshook: AtomicU64,
    /// Handshook and then refused a place in the table. Four overlapping counters
    /// for one event, never summed; see `refusedTable` in `tpuPath.ts`.
    pub add_failed: AtomicU64,
    /// Refused by the stake-weighted listener with the staked table full.
    pub add_failed_staked: AtomicU64,
    /// Refused by the stake-weighted listener with the unstaked table full.
    pub add_failed_unstaked: AtomicU64,
    /// Refused because the peer's identity is on the vote listener's banlist.
    pub add_failed_banned: AtomicU64,
    /// Admitted, from a peer with stake.
    pub admitted_staked: AtomicU64,
    /// Admitted, from a peer without.
    pub admitted_unstaked: AtomicU64,

    /// Streams opened on connections that were admitted.
    pub streams: AtomicU64,
    /// Held back by the per-stake stream limiter.
    pub throttled_staked: AtomicU64,
    pub throttled_unstaked: AtomicU64,
    /// Opened and then never finished arriving.
    pub read_timeouts: AtomicU64,
    pub read_errors: AtomicU64,
    /// Refused for describing a transaction that could not be one.
    pub invalid_size: AtomicU64,

    /// Handed on towards verification.
    pub handed_on: AtomicU64,
    pub bytes_handed_on: AtomicU64,
    /// Thrown away because the queue towards verification was full. The one
    /// row here that means this validator could not keep up.
    pub queue_full: AtomicU64,
    /// Thrown away because that queue had gone.
    pub disconnected: AtomicU64,

    /// Connections open at the moment of the last point. A level.
    pub open: AtomicU64,
    /// Streams in flight at that same moment. A level.
    pub active_streams: AtomicU64,
}

/// What signature verification and deduplication did with what QUIC handed on.
#[derive(Debug, Default)]
pub struct VerifyCounters {
    /// Everything that arrived, votes excluded.
    pub received: AtomicU64,
    /// Seen before. The network sends the same transaction more than once as a
    /// matter of course, so this is ordinary rather than a fault.
    pub duplicate: AtomicU64,
    /// Dropped for paying too little, when a priority floor is configured.
    pub below_floor: AtomicU64,
    /// Passed verification.
    pub verified: AtomicU64,
    /// Batches, not transactions, dropped because the queue to the scheduler was
    /// full. Kept apart because it cannot be added to a packet count.
    pub evicted_batches: AtomicU64,
}

/// What the worker threads did with what the scheduler gave them.
#[derive(Debug, Default)]
pub struct ExecutedCounters {
    /// Transactions the workers took up.
    pub attempted: AtomicU64,
    /// Held back by the cost model rather than executed: the block had no room
    /// left for them.
    pub cost_throttled: AtomicU64,
    /// Handed back to be tried again.
    pub retryable: AtomicU64,
    /// Handed back because the bank they were for had gone.
    pub expired_bank: AtomicU64,
    /// Executed and committed to a block.
    pub processed: AtomicU64,
    /// Of those, the ones whose result was success. The rest landed in the
    /// block having failed, which still costs their fee.
    pub succeeded: AtomicU64,

    // Why a transaction the worker took up never reached the block. Only the
    // terminal reasons: retries are drawn as retries, and `instruction_error`
    // reached the block.
    pub too_many_locks: AtomicU64,
    pub account_missing: AtomicU64,
    pub fee_payer_broke: AtomicU64,
    pub fee_payer_invalid: AtomicU64,
    pub blockhash_missing: AtomicU64,
    pub blockhash_old: AtomicU64,
    pub already_processed: AtomicU64,
    pub bad_compute_budget: AtomicU64,
    pub account_data_too_large: AtomicU64,
    pub program_not_executable: AtomicU64,
    pub program_restricted: AtomicU64,
}

/// Running totals of the counters worth watching.
#[derive(Debug, Default)]
pub struct MetricsTap {
    /// Reads of an account that were already cached, and those that were not.
    pub accounts_cache_hits: AtomicU64,
    pub accounts_cache_misses: AtomicU64,
    /// Accounts dropped from the cache, which is the usual reason a hit rate
    /// falls.
    pub accounts_cache_evicts: AtomicU64,

    /// Shreds that arrived on their own, and shreds this validator had to ask for.
    /// The first is also the turbine port's received count: everything on the TVU
    /// port is a shred.
    pub shreds_turbine: AtomicU64,
    pub shreds_repair: AtomicU64,

    /// Packets delivered on the gossip and TPU vote ports, which is the denominator
    /// `/proc/net/udp` will not give for its drop counts.
    pub packets_gossip: AtomicU64,
    pub packets_tpu_vote: AtomicU64,

    /// Bytes each named sender put on the wire, with the milliseconds each
    /// sample covered, so a rate is bytes over the window reported rather than
    /// over the interval the points happened to arrive at.
    pub gossip_sent_bytes: AtomicU64,
    pub gossip_sent_millis: AtomicU64,
    pub repair_sent_bytes: AtomicU64,
    pub repair_sent_millis: AtomicU64,

    /// What the accounts database read, wrote, and is holding.
    pub accounts: AccountsCounters,

    /// How the program cache is faring.
    pub program_cache: ProgramCacheCounters,

    /// The stages either side of the scheduler. Kept as separate sets and drawn as
    /// separate sections, because they do not reconcile into one flow.
    pub quic: QuicCounters,
    /// The other two QUIC ports, read for the same connection and stream figures.
    pub quic_forwards: QuicCounters,
    pub quic_vote: QuicCounters,
    pub verify: VerifyCounters,
    pub executed: ExecutedCounters,
    pub bundles: BundleCounters,

    /// Where the transactions handed to the banking stage ended up.
    pub scheduler: SchedulerCounters,

    /// The same per leader slot. The one thing here behind a lock, taken once per
    /// slot this node leads.
    slot_waterfalls: Mutex<VecDeque<SlotWaterfall>>,

    /// Which scheduler sent the interval counts, from the same tag the per-slot
    /// points carry. It decides what `received` counts.
    scheduler_is_bam: AtomicBool,

    /// What each block this validator produced cost, newest last. Bounded like the
    /// waterfalls.
    slot_costs: Mutex<VecDeque<SlotCost>>,

    /// The last few hundred replayed slots, kept as arrivals rather than totals
    /// because the panel wants the worst slot as well as the mean.
    replay_slots: Mutex<VecDeque<ReplaySlotTimes>>,

    /// How each recent slot's shreds arrived, oldest first.
    shred_fills: Mutex<VecDeque<ShredFill>>,

    /// How the XDP transmit path is configured. Latched: it cannot change while the
    /// process runs, and a config that stops being reported has not been turned
    /// off.
    xdp: Mutex<Option<XdpConfig>>,
}

/// How the XDP transmit path is configured, as the validator resolved it.
/// Vendor and model may arrive as "unknown" on a host without the PCI
/// database, and are passed through as sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct XdpConfig {
    /// Whether the socket bound with `XDP_ZEROCOPY`. The flag is passed straight to
    /// `bind`, which fails rather than falling back, so true means zero-copy.
    pub zero_copy: bool,
    pub driver: String,
    pub vendor: String,
    pub model: String,
    pub kernel_version: String,
}

/// One leader slot's waterfall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SlotWaterfall {
    pub slot: Slot,
    pub source: SchedulerSource,
    #[serde(flatten)]
    pub counts: SchedulerTotals,
}

/// What one block cost, and which account took the most of it. The per-account
/// ceiling is a consensus limit read from the bank rather than held here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlotCost {
    pub slot: Slot,
    /// The account that consumed the most compute in this block.
    pub costliest_account: String,
    pub costliest_cost: u64,
    /// The block's total, so the costliest account can be read as a share of it.
    pub block_cost: u64,
    pub accounts: u64,
    /// Accounts more than one transaction wanted to write. The cost tracker's
    /// own definition: within five percent of the per-account ceiling.
    pub contended: u64,
    pub new_account_data: u64,
    pub in_flight: u64,
}

/// How one slot's shreds arrived, reported by the blockstore as the slot
/// filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShredFill {
    pub slot: Slot,
    /// Data shreds in the block: the last index plus one.
    pub shreds: u64,
    /// Of those, the ones this validator had to ask for.
    pub repaired: u64,
    /// Milliseconds from the first shred to the last.
    pub full_millis: u64,
}

/// One replayed slot's timings, in microseconds, of three kinds:
///
/// - `fetch`, `confirming` and `completing` are disjoint spans on replay's own
///   thread and add up to a real duration;
/// - `poh_verify`, `tx_verify` and `dispatch` are sums of overlapping
///   asynchronous jobs, worth only relative to one another;
/// - everything from `execute` down is thread time across the workers, which
///   partitions cleanly into the CPU one slot costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplaySlotTimes {
    /// The slot the point described, so a row can be asked for its own figure.
    pub slot: Slot,
    // Replay's own thread, sequential.
    pub fetch: u64,
    pub confirming: u64,
    pub completing: u64,

    // Verification jobs, concurrent. Relative only.
    pub poh_verify: u64,
    pub tx_verify: u64,
    pub dispatch: u64,

    // Thread time across the workers.
    pub execute: u64,
    pub bytecode: u64,
    pub serialising: u64,
    pub deserialising: u64,
    pub creating_vms: u64,
    pub load: u64,
    pub store: u64,
    pub program_cache: u64,
    pub compiling: u64,
    pub checking: u64,
    pub other: u64,

    pub transactions: u64,

    /// When the point arrived, in milliseconds since the epoch: replay reports as
    /// it finishes a slot, on the clock the blockstore stamps shreds with.
    pub observed_millis: u64,
}

impl ReplaySlotTimes {
    /// What replay's own thread spent on this slot: three disjoint spans, so a
    /// real duration to hold against the slot time.
    pub fn serial(&self) -> u64 {
        self.fetch
            .saturating_add(self.confirming)
            .saturating_add(self.completing)
    }

    /// Thread time the slot cost across every worker.
    pub fn cpu(&self) -> u64 {
        self.execute
            .saturating_add(self.load)
            .saturating_add(self.store)
            .saturating_add(self.program_cache)
            .saturating_add(self.checking)
            .saturating_add(self.other)
    }
}

/// Which of the process's schedulers built a slot, and therefore what its
/// counts are counting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerSource {
    /// The validator's own scheduler, and the only one a stock build runs.
    #[default]
    Scheduler,
    /// A second scheduler running beside it, which on jito is BAM. It receives
    /// batches rather than packets.
    Bam,
}

/// Reads the tag, treating an untagged point as the validator's own. Any other
/// id is taken as BAM, the only second scheduler that exists.
fn scheduler_source(point: &DataPoint) -> SchedulerSource {
    match point.tags.iter().find(|(name, _)| *name == SCHEDULER_ID) {
        None => SchedulerSource::Scheduler,
        Some((_, id)) if id == OWN_SCHEDULER_ID => SchedulerSource::Scheduler,
        Some(_) => SchedulerSource::Bam,
    }
}

/// Whether a newly arrived report describes more of a slot's work than the one
/// held. `scheduled` decides it, since only the enabled scheduler placed work;
/// `finished` and `buffered` break ties on an empty slot. `received` is not
/// consulted because the two schedulers count it in different units.
fn describes_more_work(new: &SchedulerTotals, held: &SchedulerTotals) -> bool {
    (new.scheduled, new.finished, new.buffered) > (held.scheduled, held.finished, held.buffered)
}

/// The scheduler's counters, in the order a transaction meets them: received,
/// buffered, scheduled, finished, with the reasons the count falls between
/// each. The first stretch is an identity the validator's own tests assert; the
/// later ones are not, since the container holds a standing population across
/// seconds.
#[derive(Debug, Default)]
pub struct SchedulerCounters {
    /// Everything sigverify handed the scheduler.
    pub received: AtomicU64,

    // Lost at the door, before ever being buffered.
    // Not held because the validator was forwarding rather than buffering, which
    // on most nodes most of the time is nearly all the traffic.
    pub not_held: AtomicU64,
    /// The queue feeding the checks was full.
    pub check_queue_full: AtomicU64,
    /// Would not parse, or would not sanitize.
    pub unparsable: AtomicU64,
    /// Asked for locks it could not have.
    pub bad_locks: AtomicU64,
    /// Its compute budget instructions did not add up.
    pub compute_budget: AtomicU64,
    /// Its blockhash was too old, or its nonce did not hold.
    pub too_old: AtomicU64,
    /// Already in the ledger.
    pub already_processed: AtomicU64,
    /// The fee payer could not pay.
    pub fee_payer: AtomicU64,
    /// Excluded by the account key filter.
    pub filtered: AtomicU64,
    /// A nonce transaction already queued at the same or higher priority.
    pub nonce_conflict: AtomicU64,

    /// Made it into the container.
    pub buffered: AtomicU64,

    // Lost from the container, after being buffered.
    /// Pushed out by something of higher priority when the queue was full.
    pub queue_full: AtomicU64,
    /// Evicted by a validated nonce transaction that outranked it.
    pub nonce_evicted: AtomicU64,
    /// Thrown away when the container was cleared.
    pub cleared: AtomicU64,
    /// Thrown away as stale when the container was cleaned.
    pub cleaned: AtomicU64,

    /// Handed to a worker.
    pub scheduled: AtomicU64,
    /// Held back this pass for account conflicts or busy workers. Pressure, not
    /// losses.
    pub blocked_conflicts: AtomicU64,
    pub blocked_threads: AtomicU64,

    /// Came back from a worker done, and came back to be tried again.
    pub finished: AtomicU64,
    pub retried: AtomicU64,
}

/// A snapshot of [`SchedulerCounters`], sent to the browser as it stands: the
/// field names are already the panel's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct AccountsTotals {
    pub loaded_from_write_cache: u64,
    pub loaded_from_read_cache: u64,
    pub loaded_from_storage: u64,
    pub stored_accounts: u64,
    pub stored_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ProgramCacheTotals {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub reloads: u64,
    pub insertions: u64,
    pub lost_insertions: u64,
    pub replacements: u64,
    pub one_hit_wonders: u64,
    pub prunes_orphan: u64,
    pub prunes_environment: u64,
    pub empty_entries: u64,
}

/// One window of a QUIC port's counters.
///
/// These do not partition the offer. The listener drops a connection without
/// counting it in two places, either side of the handshake, and `handshook` is
/// what tells the two silences apart:
///
/// ```text
/// before = offered - (shed_all + shed_address + refused_full
///                     + handshake_timeout + handshake_error + handshook)
/// after  = handshook - (refused a table place + admitted)
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct QuicTotals {
    pub offered: u64,
    pub shed_all: u64,
    pub shed_address: u64,
    pub refused_full: u64,
    pub handshake_timeout: u64,
    pub handshake_error: u64,
    pub handshook: u64,
    pub add_failed: u64,
    pub add_failed_staked: u64,
    pub add_failed_unstaked: u64,
    pub add_failed_banned: u64,
    pub admitted_staked: u64,
    pub admitted_unstaked: u64,
    pub streams: u64,
    pub throttled_staked: u64,
    pub throttled_unstaked: u64,
    pub read_timeouts: u64,
    pub read_errors: u64,
    pub invalid_size: u64,
    pub handed_on: u64,
    pub bytes_handed_on: u64,
    pub queue_full: u64,
    pub disconnected: u64,
}

/// How one QUIC port stands at this instant. Levels, kept apart from the
/// counters because a window of them can be neither summed nor differenced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct QuicLevels {
    pub open: u64,
    pub active_streams: u64,
}

/// Bundles the block engine sent, and the transactions inside them. The point's
/// other sixteen fields describe a funnel nothing here draws.
#[derive(Debug, Default)]
pub struct BundleCounters {
    /// Bundles handed to the stage over the interval.
    pub received: AtomicU64,
    /// Transactions carried in them, in the same unit as the section this
    /// annotates.
    pub packets: AtomicU64,
}

/// One epoch's worth of what the block engine sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct BundleTotals {
    pub received: u64,
    pub packets: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct VerifyTotals {
    pub received: u64,
    pub duplicate: u64,
    pub below_floor: u64,
    pub verified: u64,
    pub evicted_batches: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ExecutedTotals {
    pub attempted: u64,
    pub cost_throttled: u64,
    pub retryable: u64,
    pub expired_bank: u64,
    pub processed: u64,
    pub succeeded: u64,
    pub too_many_locks: u64,
    pub account_missing: u64,
    pub fee_payer_broke: u64,
    pub fee_payer_invalid: u64,
    pub blockhash_missing: u64,
    pub blockhash_old: u64,
    pub already_processed: u64,
    pub bad_compute_budget: u64,
    pub account_data_too_large: u64,
    pub program_not_executable: u64,
    pub program_restricted: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SchedulerTotals {
    pub received: u64,
    pub not_held: u64,
    pub check_queue_full: u64,
    pub unparsable: u64,
    pub bad_locks: u64,
    pub compute_budget: u64,
    pub too_old: u64,
    pub already_processed: u64,
    pub fee_payer: u64,
    pub filtered: u64,
    pub nonce_conflict: u64,
    pub buffered: u64,
    pub queue_full: u64,
    pub nonce_evicted: u64,
    pub cleared: u64,
    pub cleaned: u64,
    pub scheduled: u64,
    pub blocked_conflicts: u64,
    pub blocked_threads: u64,
    pub finished: u64,
    pub retried: u64,
}

/// A snapshot of the totals, for differencing between readings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TapCounters {
    pub accounts_cache_hits: u64,
    pub accounts_cache_misses: u64,
    pub accounts_cache_evicts: u64,
    pub shreds_turbine: u64,
    pub shreds_repair: u64,
    pub packets_gossip: u64,
    pub packets_tpu_vote: u64,
    pub gossip_sent_bytes: u64,
    pub gossip_sent_millis: u64,
    pub repair_sent_bytes: u64,
    pub repair_sent_millis: u64,
    pub scheduler: SchedulerTotals,
    pub accounts: AccountsTotals,
    /// Levels, read as they stand rather than differenced.
    pub accounts_storage_bytes: u64,
    pub accounts_storage_alive_bytes: u64,
    pub accounts_storage_count: u64,
    pub accounts_cache_bytes: u64,
    pub accounts_cache_entries: u64,
    pub program_cache: ProgramCacheTotals,
    /// Entries loaded when an eviction last ran. A level, so it is read as it
    /// stands rather than differenced.
    pub program_cache_water_level: u64,
    pub quic: QuicTotals,
    pub quic_forwards: QuicTotals,
    pub quic_vote: QuicTotals,
    /// Levels rather than counts, one set per port, in the same order the
    /// totals above are named.
    pub quic_levels: QuicLevels,
    pub quic_forwards_levels: QuicLevels,
    pub quic_vote_levels: QuicLevels,
    pub verify: VerifyTotals,
    pub executed: ExecutedTotals,
    pub bundles: BundleTotals,
}

impl MetricsTap {
    /// Starts watching, if nothing else already is. The first observer keeps the
    /// process's one slot; a second gets a tap that stays at zero.
    pub fn install() -> Arc<Self> {
        let tap = Arc::new(Self::default());
        let observer = tap.clone();
        if !solana_metrics::set_datapoint_observer(Box::new(move |point| {
            observer.observe(point);
        })) {
            log::warn!("dashboard: something else is already watching metrics points");
        }
        tap
    }

    /// Feeds one point in, for tests in other modules. `observe` itself stays
    /// private.
    #[cfg(test)]
    pub(crate) fn observe_point(&self, point: &DataPoint) {
        self.observe(point);
    }

    /// Adds what one point carries, if it is one of the few wanted. Every point the
    /// validator submits passes through here, so an unwanted one costs the match
    /// and nothing else.
    fn observe(&self, point: &DataPoint) {
        match point.name {
            ACCOUNTS_DB_TIMINGS => {
                for (name, value) in &point.fields {
                    // The same point carries the cache's size and entry
                    // count, which are levels rather than counts.
                    self.accounts.add_point(point);
                    let counter = match *name {
                        "read_only_accounts_cache_hits" => &self.accounts_cache_hits,
                        "read_only_accounts_cache_misses" => &self.accounts_cache_misses,
                        "read_only_accounts_cache_evicts" => &self.accounts_cache_evicts,
                        _ => continue,
                    };
                    add_field(counter, value);
                }
            }
            SHREDS_TURBINE => self.add_packets(&self.shreds_turbine, point),
            SHREDS_REPAIR => self.add_packets(&self.shreds_repair, point),
            GOSSIP_RECEIVER => self.add_packets(&self.packets_gossip, point),
            TPU_VOTE_RECEIVER => self.add_packets(&self.packets_tpu_vote, point),
            GOSSIP_SENDER => {
                self.add_sent(&self.gossip_sent_bytes, &self.gossip_sent_millis, point)
            }
            REPAIR_SENDER => {
                self.add_sent(&self.repair_sent_bytes, &self.repair_sent_millis, point)
            }
            SCHEDULER_COUNTS => {
                self.scheduler_is_bam.store(
                    scheduler_source(point) == SchedulerSource::Bam,
                    Ordering::Relaxed,
                );
                self.scheduler.add_point(point)
            }
            SCHEDULER_SLOT_COUNTS => self.remember_slot(point),
            REPLAY_SLOT_STATS => self.remember_replay(point),
            SHRED_FULL => self.remember_fill(point),
            XDP_NETWORK_CONFIG => self.remember_xdp(point),
            COST_TRACKER => self.remember_cost(point),
            ACCOUNTS_LOADS | ACCOUNTS_STORES | ACCOUNTS_FLUSH => self.accounts.add_point(point),
            PROGRAM_CACHE => self.program_cache.add_point(point),
            QUIC_TPU => self.quic.add_point(point),
            QUIC_TPU_FORWARDS => self.quic_forwards.add_point(point),
            QUIC_TPU_VOTE => self.quic_vote.add_point(point),
            TPU_VERIFIER => self.verify.add_point(point),
            BUNDLE_STAGE => self.bundles.add_point(point),
            WORKER_COUNTS | WORKER_ERROR_METRICS => self.executed.add_point(point),
            _ => (),
        }
    }

    /// Adds a socket receiver's packet count, the whole of what is wanted from
    /// any of those four points.
    fn add_packets(&self, counter: &AtomicU64, point: &DataPoint) {
        for (name, value) in &point.fields {
            if *name == PACKETS_COUNT {
                add_field(counter, value);
                return;
            }
        }
    }

    fn add_sent(&self, bytes: &AtomicU64, millis: &AtomicU64, point: &DataPoint) {
        for (name, value) in &point.fields {
            match *name {
                SENT_BYTES => add_field(bytes, value),
                SENT_MILLIS => add_field(millis, value),
                _ => (),
            }
        }
    }

    /// Records one leader slot's waterfall. These counts are already one slot's
    /// own, so they are kept as they arrive. A point without a readable slot has
    /// nowhere to be shown and is dropped.
    fn remember_slot(&self, point: &DataPoint) {
        let Some(slot) = point
            .fields
            .iter()
            .find(|(name, _)| *name == SLOT)
            .and_then(|(_, value)| field_u64(value))
        else {
            return;
        };

        let counters = SchedulerCounters::default();
        counters.add_point(point);
        let waterfall = SlotWaterfall {
            slot,
            source: scheduler_source(point),
            counts: counters.totals(),
        };

        let Ok(mut slots) = self.slot_waterfalls.lock() else {
            // A panicking observer would have poisoned this. The dashboard
            // losing a panel is not worth taking the validator down over.
            return;
        };
        // One row per slot, keeping whichever report describes the block. A build
        // running two schedulers reports every leader slot twice, and only the enabled
        // one did the work; keeping the last to arrive emptied the panel on half of
        // all slots.
        if let Some(held) = slots.iter_mut().find(|held| held.slot == slot) {
            if describes_more_work(&waterfall.counts, &held.counts) {
                *held = waterfall;
            }
            return;
        }
        slots.push_back(waterfall);
        while slots.len() > SLOT_WATERFALLS {
            slots.pop_front();
        }
    }

    /// Latches how the XDP transmit path is set up. Read from tags as well as
    /// fields: `driver` and `zero_copy` are tags, the rest fields. An unparseable
    /// `zero_copy` is taken as false rather than dropping the report.
    fn remember_xdp(&self, point: &DataPoint) {
        let tag = |wanted: &str| {
            point
                .tags
                .iter()
                .find(|(name, _)| *name == wanted)
                .map(|(_, value)| value.as_str())
        };
        let field = |wanted: &str| {
            point
                .fields
                .iter()
                .find(|(name, _)| *name == wanted)
                .map(|(_, value)| field_str(value))
                .unwrap_or_default()
        };

        let config = XdpConfig {
            zero_copy: tag("zero_copy") == Some("true"),
            driver: tag("driver").unwrap_or_default().to_string(),
            vendor: field("vendor"),
            model: field("model"),
            kernel_version: field("kernel_version"),
        };
        if let Ok(mut held) = self.xdp.lock() {
            *held = Some(config);
        }
    }

    /// Records one replayed slot's timings. The confirm and dispatch spans arrive
    /// as `confirmation_without_replay_us` and `task_submission_us`, the unified
    /// scheduler's names, and `update_transaction_statuses` has no `_us` suffix.
    fn remember_replay(&self, point: &DataPoint) {
        let mut slot = ReplaySlotTimes::default();
        let mut seen = false;
        for (name, value) in &point.fields {
            let Some(micros) = field_u64(value) else {
                continue;
            };
            if *name == SLOT {
                slot.slot = micros;
                continue;
            }
            let field = match *name {
                "fetch_entries_time" => &mut slot.fetch,
                "confirmation_without_replay_us" => &mut slot.confirming,
                "bank_complete_time_us" => &mut slot.completing,

                "entry_poh_verification_time" => &mut slot.poh_verify,
                "entry_transaction_verification_time" => &mut slot.tx_verify,
                "task_submission_us" => &mut slot.dispatch,

                "execute_us" => &mut slot.execute,
                "execute_details_execute_inner_us" => &mut slot.bytecode,
                "execute_details_serialize_us" => &mut slot.serialising,
                "execute_details_deserialize_us" => &mut slot.deserialising,
                "execute_details_create_vm_us" => &mut slot.creating_vms,
                "load_us" => &mut slot.load,
                "store_us" => &mut slot.store,
                "program_cache_us" => &mut slot.program_cache,
                "total_transactions" => &mut slot.transactions,

                // Compiling a program that was not in the cache, summed: what a miss cost,
                // not which third of the compiler it went to.
                "execute_details_create_executor_load_elf_us"
                | "execute_details_create_executor_verify_code_us"
                | "execute_details_create_executor_jit_compile_us" => &mut slot.compiling,

                // The checks at the door, before a transaction is handed to a
                // worker at all.
                "validate_transactions_us" | "validate_fees_us" | "filter_executable_us" => {
                    &mut slot.checking
                }

                // Bookkeeping around execution, small on any validator.
                "collect_balances_us"
                | "collect_logs_us"
                | "update_stakes_cache_us"
                | "update_transaction_statuses"
                | "check_block_limits_us" => &mut slot.other,

                _ => continue,
            };
            *field = field.saturating_add(micros);
            seen = true;
        }

        // A point naming nothing this reads is not a slot that took no time. Keeping
        // it would drag every mean towards nought.
        if !seen {
            return;
        }

        slot.observed_millis = now_millis();
        let Ok(mut slots) = self.replay_slots.lock() else {
            return;
        };
        slots.push_back(slot);
        while slots.len() > REPLAY_SLOTS {
            slots.pop_front();
        }
    }

    /// Records how a slot's shreds arrived. The count is one more than the last
    /// index, and the -1 the blockstore sends for an unknown one drops the point.
    fn remember_fill(&self, point: &DataPoint) {
        let mut fill = ShredFill::default();
        let mut slot = None;
        let mut last_index = None;
        for (name, value) in &point.fields {
            let Some(number) = field_u64(value) else {
                continue;
            };
            match *name {
                SLOT => slot = Some(number),
                "last_index" => last_index = Some(number),
                "num_repaired" => fill.repaired = number,
                "total_time_ms" => fill.full_millis = number,
                _ => (),
            }
        }
        let (Some(slot), Some(last_index)) = (slot, last_index) else {
            return;
        };
        fill.slot = slot;
        fill.shreds = last_index.saturating_add(1);

        let Ok(mut fills) = self.shred_fills.lock() else {
            return;
        };
        fills.push_back(fill);
        while fills.len() > SHRED_FILLS {
            fills.pop_front();
        }
    }

    /// Records what one of this validator's own blocks cost. Other validators'
    /// blocks are dropped on the `is_leader` tag.
    fn remember_cost(&self, point: &DataPoint) {
        let is_leader = point
            .tags
            .iter()
            .any(|(name, value)| *name == IS_LEADER && value == "true");
        if !is_leader {
            return;
        }

        let mut slot = None;
        let mut cost = SlotCost {
            slot: 0,
            costliest_account: String::new(),
            costliest_cost: 0,
            block_cost: 0,
            accounts: 0,
            contended: 0,
            new_account_data: 0,
            in_flight: 0,
        };
        for (name, value) in &point.fields {
            match *name {
                // A pubkey, and the only field here that is not a number.
                "costliest_account" => cost.costliest_account = value.trim_matches('"').to_string(),
                "bank_slot" => slot = field_u64(value),
                "costliest_account_cost" => cost.costliest_cost = field_u64(value).unwrap_or(0),
                "block_cost" => cost.block_cost = field_u64(value).unwrap_or(0),
                "number_of_accounts" => cost.accounts = field_u64(value).unwrap_or(0),
                "number_of_contended_accounts" => cost.contended = field_u64(value).unwrap_or(0),
                "allocated_accounts_data_size" => {
                    cost.new_account_data = field_u64(value).unwrap_or(0)
                }
                "inflight_transaction_count" => cost.in_flight = field_u64(value).unwrap_or(0),
                _ => continue,
            }
        }

        // Without a slot it cannot be joined to the block it describes, which
        // is the only place it is shown.
        let Some(slot_number) = slot else {
            return;
        };
        cost.slot = slot_number;

        let Ok(mut costs) = self.slot_costs.lock() else {
            return;
        };
        // Replaced rather than appended if the slot is already held, so a repeat
        // cannot push a real row off the end.
        if let Some(held) = costs.iter_mut().find(|held| held.slot == slot_number) {
            *held = cost;
            return;
        }
        costs.push_back(cost);
        while costs.len() > SLOT_WATERFALLS {
            costs.pop_front();
        }
    }

    /// What this validator's recent blocks cost, oldest first.
    pub fn slot_costs(&self) -> Vec<SlotCost> {
        self.slot_costs
            .lock()
            .map(|costs| costs.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// How the XDP transmit path is configured, or nothing where it is not.
    pub fn xdp(&self) -> Option<XdpConfig> {
        self.xdp.lock().ok().and_then(|held| held.clone())
    }

    /// The replay record for `slot`, while it is still held. Newest first, in
    /// case a slot was replayed twice.
    pub fn replayed(&self, slot: Slot) -> Option<ReplaySlotTimes> {
        let slots = self.replay_slots.lock().ok()?;
        slots.iter().rev().find(|times| times.slot == slot).copied()
    }

    /// How `slot`'s shreds arrived, while the record is still held.
    pub fn shred_fill(&self, slot: Slot) -> Option<ShredFill> {
        let fills = self.shred_fills.lock().ok()?;
        fills.iter().rev().find(|fill| fill.slot == slot).copied()
    }

    /// The replayed slots held, oldest first.
    pub fn replay_slots(&self) -> Vec<ReplaySlotTimes> {
        self.replay_slots
            .lock()
            .map(|slots| slots.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Which scheduler the interval counts last came from.
    pub fn scheduler_source(&self) -> SchedulerSource {
        if self.scheduler_is_bam.load(Ordering::Relaxed) {
            SchedulerSource::Bam
        } else {
            SchedulerSource::Scheduler
        }
    }

    /// The leader slots held, oldest first.
    pub fn slot_waterfalls(&self) -> Vec<SlotWaterfall> {
        self.slot_waterfalls
            .lock()
            .map(|slots| slots.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The totals as they stand, read together so a reading is coherent enough
    /// to difference.
    pub fn counters(&self) -> TapCounters {
        TapCounters {
            accounts_cache_hits: self.accounts_cache_hits.load(Ordering::Relaxed),
            accounts_cache_misses: self.accounts_cache_misses.load(Ordering::Relaxed),
            accounts_cache_evicts: self.accounts_cache_evicts.load(Ordering::Relaxed),
            shreds_turbine: self.shreds_turbine.load(Ordering::Relaxed),
            shreds_repair: self.shreds_repair.load(Ordering::Relaxed),
            packets_gossip: self.packets_gossip.load(Ordering::Relaxed),
            packets_tpu_vote: self.packets_tpu_vote.load(Ordering::Relaxed),
            gossip_sent_bytes: self.gossip_sent_bytes.load(Ordering::Relaxed),
            gossip_sent_millis: self.gossip_sent_millis.load(Ordering::Relaxed),
            repair_sent_bytes: self.repair_sent_bytes.load(Ordering::Relaxed),
            repair_sent_millis: self.repair_sent_millis.load(Ordering::Relaxed),
            scheduler: self.scheduler.totals(),
            accounts: self.accounts.totals(),
            accounts_storage_bytes: self.accounts.storage_bytes.load(Ordering::Relaxed),
            accounts_storage_alive_bytes: self.accounts.storage_alive_bytes.load(Ordering::Relaxed),
            accounts_storage_count: self.accounts.storage_count.load(Ordering::Relaxed),
            accounts_cache_bytes: self.accounts.cache_bytes.load(Ordering::Relaxed),
            accounts_cache_entries: self.accounts.cache_entries.load(Ordering::Relaxed),
            program_cache: self.program_cache.totals(),
            program_cache_water_level: self.program_cache.water_level.load(Ordering::Relaxed),
            quic: self.quic.totals(),
            quic_forwards: self.quic_forwards.totals(),
            quic_vote: self.quic_vote.totals(),
            quic_levels: self.quic.levels(),
            quic_forwards_levels: self.quic_forwards.levels(),
            quic_vote_levels: self.quic_vote.levels(),
            verify: self.verify.totals(),
            bundles: self.bundles.totals(),
            executed: self.executed.totals(),
        }
    }
}

impl AccountsCounters {
    /// Adds one of the three accounts points, matched on field name across all
    /// three since the names do not collide.
    fn add_point(&self, point: &DataPoint) {
        for (name, value) in &point.fields {
            // Levels first. These say how things stand rather than what has
            // happened since the last point, so they are replaced, not summed.
            let gauge = match *name {
                "total_bytes" => Some(&self.storage_bytes),
                "total_alive_bytes" => Some(&self.storage_alive_bytes),
                "total_count" => Some(&self.storage_count),
                "read_only_accounts_cache_data_size" => Some(&self.cache_bytes),
                "read_only_accounts_cache_entries" => Some(&self.cache_entries),
                _ => None,
            };
            if let Some(gauge) = gauge {
                set_field(gauge, value);
                continue;
            }

            let counter = match *name {
                "num_loaded_from_write_cache" => &self.loaded_from_write_cache,
                "num_loaded_from_read_cache" => &self.loaded_from_read_cache,
                "num_loaded_from_index_storage" => &self.loaded_from_storage,
                // Two spellings each: 4.3 calls stored what 4.2 called flushed, and this file
                // is carried across both branches.
                "num_accounts_stored" | "num_accounts_flushed" => &self.stored_accounts,
                "account_bytes_stored" | "account_bytes_flushed" => &self.stored_bytes,
                _ => continue,
            };
            add_field(counter, value);
        }
    }

    fn totals(&self) -> AccountsTotals {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        AccountsTotals {
            loaded_from_write_cache: read(&self.loaded_from_write_cache),
            loaded_from_read_cache: read(&self.loaded_from_read_cache),
            loaded_from_storage: read(&self.loaded_from_storage),
            stored_accounts: read(&self.stored_accounts),
            stored_bytes: read(&self.stored_bytes),
        }
    }
}

impl ProgramCacheCounters {
    fn add_point(&self, point: &DataPoint) {
        for (name, value) in &point.fields {
            // `water_level` is a level and `slot` names the point; neither is
            // something to add to a running total.
            if *name == "water_level" {
                set_field(&self.water_level, value);
                continue;
            }
            let counter = match *name {
                "hits" => &self.hits,
                "misses" => &self.misses,
                "evictions" => &self.evictions,
                "reloads" => &self.reloads,
                "insertions" => &self.insertions,
                "lost_insertions" => &self.lost_insertions,
                // The field is not named after the counter behind it.
                "replace_entry" => &self.replacements,
                "one_hit_wonders" => &self.one_hit_wonders,
                "prunes_orphan" => &self.prunes_orphan,
                "prunes_environment" => &self.prunes_environment,
                "empty_entries" => &self.empty_entries,
                _ => continue,
            };
            add_field(counter, value);
        }
    }

    fn totals(&self) -> ProgramCacheTotals {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        ProgramCacheTotals {
            hits: read(&self.hits),
            misses: read(&self.misses),
            evictions: read(&self.evictions),
            reloads: read(&self.reloads),
            insertions: read(&self.insertions),
            lost_insertions: read(&self.lost_insertions),
            replacements: read(&self.replacements),
            one_hit_wonders: read(&self.one_hit_wonders),
            prunes_orphan: read(&self.prunes_orphan),
            prunes_environment: read(&self.prunes_environment),
            empty_entries: read(&self.empty_entries),
        }
    }
}

impl QuicCounters {
    fn add_point(&self, point: &DataPoint) {
        for (name, value) in &point.fields {
            // Cumulative on the wire rather than one interval's worth, unlike
            // every counter beside it. Stored, not added.
            if *name == "total_incoming_connection_attempts" {
                set_field(&self.offered, value);
                continue;
            }
            // Levels, and the only two read here. `peak_open_staked_connections` is a
            // peak reset as it is reported, which is neither a level nor a count.
            if *name == "open_connections" {
                set_field(&self.open, value);
                continue;
            }
            if *name == "active_streams" {
                set_field(&self.active_streams, value);
                continue;
            }
            let counter = match *name {
                "connection_rate_limited_across_all" => &self.shed_all,
                "connection_rate_limited_per_ipaddr" => &self.shed_address,
                "refused_connections_too_many_open_connections" => &self.refused_full,
                "connection_setup_timeout" => &self.handshake_timeout,
                "connection_setup_error" => &self.handshake_error,
                "new_connections" => &self.handshook,
                "connection_add_failed" => &self.add_failed,
                "connection_add_failed_staked_node" => &self.add_failed_staked,
                "connection_add_failed_unstaked_node" => &self.add_failed_unstaked,
                "connection_add_failed_banned" => &self.add_failed_banned,
                // `connection_add_failed_on_pruning` is raised on the same refusal as
                // `..._staked_node` and would count one event twice.
                "connection_added_from_staked_peer" => &self.admitted_staked,
                "connection_added_from_unstaked_peer" => &self.admitted_unstaked,
                "new_streams" => &self.streams,
                "throttled_staked_streams" => &self.throttled_staked,
                "throttled_unstaked_streams" => &self.throttled_unstaked,
                "stream_read_timeouts" => &self.read_timeouts,
                "stream_read_errors" => &self.read_errors,
                "invalid_stream_size" => &self.invalid_size,
                // Named for the point's field, not the struct's
                // `total_packets_sent_to_consumer`.
                "packets_sent_to_consumer" => &self.handed_on,
                "bytes_sent_to_consumer" => &self.bytes_handed_on,
                "total_handle_chunk_to_packet_send_full_err" => &self.queue_full,
                "total_handle_chunk_to_packet_send_disconnected_err" => &self.disconnected,
                _ => continue,
            };
            add_field(counter, value);
        }
    }

    fn totals(&self) -> QuicTotals {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        QuicTotals {
            offered: read(&self.offered),
            shed_all: read(&self.shed_all),
            shed_address: read(&self.shed_address),
            refused_full: read(&self.refused_full),
            handshake_timeout: read(&self.handshake_timeout),
            handshake_error: read(&self.handshake_error),
            handshook: read(&self.handshook),
            add_failed: read(&self.add_failed),
            add_failed_staked: read(&self.add_failed_staked),
            add_failed_unstaked: read(&self.add_failed_unstaked),
            add_failed_banned: read(&self.add_failed_banned),
            admitted_staked: read(&self.admitted_staked),
            admitted_unstaked: read(&self.admitted_unstaked),
            streams: read(&self.streams),
            throttled_staked: read(&self.throttled_staked),
            throttled_unstaked: read(&self.throttled_unstaked),
            read_timeouts: read(&self.read_timeouts),
            read_errors: read(&self.read_errors),
            invalid_size: read(&self.invalid_size),
            handed_on: read(&self.handed_on),
            bytes_handed_on: read(&self.bytes_handed_on),
            queue_full: read(&self.queue_full),
            disconnected: read(&self.disconnected),
        }
    }

    /// The two levels, which are read as they stand and never windowed.
    fn levels(&self) -> QuicLevels {
        QuicLevels {
            open: self.open.load(Ordering::Relaxed),
            active_streams: self.active_streams.load(Ordering::Relaxed),
        }
    }
}

impl BundleCounters {
    fn add_point(&self, point: &DataPoint) {
        for (name, value) in &point.fields {
            let counter = match *name {
                "num_bundles_received" => &self.received,
                "num_packets_received" => &self.packets,
                // The drop reasons, buffer levels and timings. All reset by the reporter, so
                // any could be added here except the `current_buffered_*` levels.
                _ => continue,
            };
            add_field(counter, value);
        }
    }

    fn totals(&self) -> BundleTotals {
        BundleTotals {
            received: self.received.load(Ordering::Relaxed),
            packets: self.packets.load(Ordering::Relaxed),
        }
    }
}

impl VerifyCounters {
    fn add_point(&self, point: &DataPoint) {
        for (name, value) in &point.fields {
            let counter = match *name {
                "total_packets" => &self.received,
                "total_dedup" => &self.duplicate,
                "total_dropped_below_priority_floor" => &self.below_floor,
                "total_valid_packets" => &self.verified,
                "eviction_drops" => &self.evicted_batches,
                // Timings, batch counts, and the deduper's saturation flag.
                _ => continue,
            };
            add_field(counter, value);
        }
    }

    fn totals(&self) -> VerifyTotals {
        VerifyTotals {
            received: self.received.load(Ordering::Relaxed),
            duplicate: self.duplicate.load(Ordering::Relaxed),
            below_floor: self.below_floor.load(Ordering::Relaxed),
            verified: self.verified.load(Ordering::Relaxed),
            evicted_batches: self.evicted_batches.load(Ordering::Relaxed),
        }
    }
}

impl ExecutedCounters {
    fn add_point(&self, point: &DataPoint) {
        for (name, value) in &point.fields {
            let counter = match *name {
                "transactions_attempted_processing_count" => &self.attempted,
                "cost_model_throttled_transactions_count" => &self.cost_throttled,
                "retryable_transaction_count" => &self.retryable,
                "retryable_expired_bank_count" => &self.expired_bank,
                "processed_transactions_count" => &self.processed,
                "processed_with_successful_result_count" => &self.succeeded,
                // And from the error point beside it. No name is shared with
                // the counts point, so both are read here.
                "too_many_account_locks" => &self.too_many_locks,
                "account_not_found" => &self.account_missing,
                "insufficient_funds" => &self.fee_payer_broke,
                "invalid_account_for_fee" => &self.fee_payer_invalid,
                "blockhash_not_found" => &self.blockhash_missing,
                "blockhash_too_old" => &self.blockhash_old,
                "already_processed" => &self.already_processed,
                "invalid_compute_budget" => &self.bad_compute_budget,
                "max_loaded_accounts_data_size_exceeded" => &self.account_data_too_large,
                "invalid_program_for_execution" => &self.program_not_executable,
                "program_execution_temporarily_restricted" => &self.program_restricted,
                // `max_queue_len` is a gauge, `num_messages_processed` counts batches, and
                // `total` sums errors drawn elsewhere.
                _ => continue,
            };
            add_field(counter, value);
        }
    }

    fn totals(&self) -> ExecutedTotals {
        ExecutedTotals {
            attempted: self.attempted.load(Ordering::Relaxed),
            cost_throttled: self.cost_throttled.load(Ordering::Relaxed),
            retryable: self.retryable.load(Ordering::Relaxed),
            expired_bank: self.expired_bank.load(Ordering::Relaxed),
            processed: self.processed.load(Ordering::Relaxed),
            succeeded: self.succeeded.load(Ordering::Relaxed),
            too_many_locks: self.too_many_locks.load(Ordering::Relaxed),
            account_missing: self.account_missing.load(Ordering::Relaxed),
            fee_payer_broke: self.fee_payer_broke.load(Ordering::Relaxed),
            fee_payer_invalid: self.fee_payer_invalid.load(Ordering::Relaxed),
            blockhash_missing: self.blockhash_missing.load(Ordering::Relaxed),
            blockhash_old: self.blockhash_old.load(Ordering::Relaxed),
            already_processed: self.already_processed.load(Ordering::Relaxed),
            bad_compute_budget: self.bad_compute_budget.load(Ordering::Relaxed),
            account_data_too_large: self.account_data_too_large.load(Ordering::Relaxed),
            program_not_executable: self.program_not_executable.load(Ordering::Relaxed),
            program_restricted: self.program_restricted.load(Ordering::Relaxed),
        }
    }
}

impl SchedulerCounters {
    /// Adds the counts one scheduler point carries. Both scheduler points carry the
    /// same fields under the same names, so the mapping lives here once.
    fn add_point(&self, point: &DataPoint) {
        for (name, value) in &point.fields {
            let counter = match *name {
                "num_received" => &self.received,
                "num_dropped_on_receive" => &self.not_held,
                "num_dropped_on_check_work_queue_full" => &self.check_queue_full,
                "num_dropped_on_parsing_and_sanitization" => &self.unparsable,
                "num_dropped_on_validate_locks" => &self.bad_locks,
                "num_dropped_on_receive_compute_budget" => &self.compute_budget,
                "num_dropped_on_receive_age" => &self.too_old,
                "num_dropped_on_receive_already_processed" => &self.already_processed,
                "num_dropped_on_receive_fee_payer" => &self.fee_payer,
                "num_dropped_on_filter_key" => &self.filtered,
                "num_dropped_on_nonce_dedup" => &self.nonce_conflict,
                "num_buffered" => &self.buffered,
                "num_dropped_on_capacity" => &self.queue_full,
                "num_evicted_on_nonce_dedup" => &self.nonce_evicted,
                "num_dropped_on_clear" => &self.cleared,
                "num_dropped_on_clean" => &self.cleaned,
                "num_scheduled" => &self.scheduled,
                "num_unschedulable_conflicts" => &self.blocked_conflicts,
                "num_unschedulable_threads" => &self.blocked_threads,
                "num_finished" => &self.finished,
                "num_retryable" => &self.retried,
                // `min_priority` and `max_priority` are gauges and `slot` names the point.
                _ => continue,
            };
            add_field(counter, value);
        }
    }

    /// The counters as they stand.
    fn totals(&self) -> SchedulerTotals {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        SchedulerTotals {
            received: read(&self.received),
            not_held: read(&self.not_held),
            check_queue_full: read(&self.check_queue_full),
            unparsable: read(&self.unparsable),
            bad_locks: read(&self.bad_locks),
            compute_budget: read(&self.compute_budget),
            too_old: read(&self.too_old),
            already_processed: read(&self.already_processed),
            fee_payer: read(&self.fee_payer),
            filtered: read(&self.filtered),
            nonce_conflict: read(&self.nonce_conflict),
            buffered: read(&self.buffered),
            queue_full: read(&self.queue_full),
            nonce_evicted: read(&self.nonce_evicted),
            cleared: read(&self.cleared),
            cleaned: read(&self.cleaned),
            scheduled: read(&self.scheduled),
            blocked_conflicts: read(&self.blocked_conflicts),
            blocked_threads: read(&self.blocked_threads),
            finished: read(&self.finished),
            retried: read(&self.retried),
        }
    }
}

/// Counters that only ever climb, so a window of them is a difference summed.
/// A trait so the windowing is written once, in the meters.
pub trait WindowedCounters: Copy + Default {
    /// This reading less the one before it, which is one interval's work.
    fn since(&self, previous: &Self) -> Self;
    /// This reading added to another, for summing a window of them.
    fn plus(&self, other: &Self) -> Self;
}

/// Differencing and summing over every field, listed once: a field left out of
/// either would read nought for ever without failing.
macro_rules! counter_arithmetic {
    ($totals:ident { $($field:ident),* $(,)? }) => {
        impl WindowedCounters for $totals {
            /// Saturating: a lower reading than the last means the tap was installed
            /// mid-flight or a counter was reset, and nought is the right answer to that.
            fn since(&self, previous: &Self) -> Self {
                Self {
                    $($field: self.$field.saturating_sub(previous.$field),)*
                }
            }

            fn plus(&self, other: &Self) -> Self {
                Self {
                    $($field: self.$field.saturating_add(other.$field),)*
                }
            }
        }
    };
}

counter_arithmetic!(AccountsTotals {
    loaded_from_write_cache,
    loaded_from_read_cache,
    loaded_from_storage,
    stored_accounts,
    stored_bytes,
});

counter_arithmetic!(ProgramCacheTotals {
    hits,
    misses,
    evictions,
    reloads,
    insertions,
    lost_insertions,
    replacements,
    one_hit_wonders,
    prunes_orphan,
    prunes_environment,
    empty_entries,
});

counter_arithmetic!(QuicTotals {
    offered,
    shed_all,
    shed_address,
    refused_full,
    handshake_timeout,
    handshake_error,
    handshook,
    add_failed,
    add_failed_staked,
    add_failed_unstaked,
    add_failed_banned,
    admitted_staked,
    admitted_unstaked,
    streams,
    throttled_staked,
    throttled_unstaked,
    read_timeouts,
    read_errors,
    invalid_size,
    handed_on,
    bytes_handed_on,
    queue_full,
    disconnected,
});

counter_arithmetic!(BundleTotals { received, packets });

counter_arithmetic!(VerifyTotals {
    received,
    duplicate,
    below_floor,
    verified,
    evicted_batches,
});

counter_arithmetic!(ExecutedTotals {
    attempted,
    cost_throttled,
    retryable,
    expired_bank,
    processed,
    succeeded,
    too_many_locks,
    account_missing,
    fee_payer_broke,
    fee_payer_invalid,
    blockhash_missing,
    blockhash_old,
    already_processed,
    bad_compute_budget,
    account_data_too_large,
    program_not_executable,
    program_restricted,
});

counter_arithmetic!(SchedulerTotals {
    received,
    not_held,
    check_queue_full,
    unparsable,
    bad_locks,
    compute_budget,
    too_old,
    already_processed,
    fee_payer,
    filtered,
    nonce_conflict,
    buffered,
    queue_full,
    nonce_evicted,
    cleared,
    cleaned,
    scheduled,
    blocked_conflicts,
    blocked_threads,
    finished,
    retried,
});

/// Adds a field's value to a counter, if it reads as an integer.
fn add_field(counter: &AtomicU64, value: &str) {
    if let Some(delta) = field_u64(value) {
        counter.fetch_add(delta, Ordering::Relaxed);
    }
}

/// Replaces a gauge with the reading a point carries. A gauge only means
/// anything as the latest value.
fn set_field(gauge: &AtomicU64, value: &str) {
    if let Some(latest) = field_u64(value) {
        gauge.store(latest, Ordering::Relaxed);
    }
}

/// Reads a field written as an integer, which the line protocol marks with a
/// trailing `i`. Anything without one is not a counter.
fn field_u64(value: &str) -> Option<u64> {
    value.strip_suffix('i')?.parse().ok()
}

/// Milliseconds since the epoch, the clock the blockstore stamps shreds with.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A string field without the line-protocol quotes `add_field_str` put round
/// it. One pair is taken off, so a value that started with a quote keeps it.
fn field_str(value: &str) -> String {
    let inner = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    inner.replace("\\\"", "\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &'static str, fields: &[(&'static str, &str)]) -> DataPoint {
        let mut point = DataPoint::new(name);
        for (field, value) in fields {
            point.fields.push((field, (*value).to_string()));
        }
        point
    }

    /// A slot point tagged as one scheduler or the other, as a build running
    /// two of them sends it.
    fn tagged_slot_point(id: &str, slot: u64, fields: &[(&'static str, &str)]) -> DataPoint {
        let mut point = slot_point(slot, fields);
        point.tags.push((SCHEDULER_ID, id.to_string()));
        point
    }

    fn point(fields: &[(&'static str, &str)]) -> DataPoint {
        named(ACCOUNTS_DB_TIMINGS, fields)
    }

    #[test]
    fn test_integer_fields_carry_the_line_protocol_suffix() {
        // What `add_field_i64` writes, which is not what a reader expects.
        assert_eq!(field_u64("42i"), Some(42));
        assert_eq!(field_u64("0i"), Some(0));
    }

    #[test]
    fn test_anything_that_is_not_an_integer_field_is_left_alone() {
        // A float, a boolean and a quoted string all appear in the same points.
        assert_eq!(field_u64("42"), None);
        assert_eq!(field_u64("1.5"), None);
        assert_eq!(field_u64("true"), None);
        assert_eq!(field_u64("\"words\"i"), None);
    }

    #[test]
    fn test_the_totals_accumulate_across_points() {
        // Each point carries what happened since the last, the counters behind
        // them being reset as they are read, so the totals are the sum.
        let tap = MetricsTap::default();
        tap.observe(&point(&[
            ("read_only_accounts_cache_hits", "10i"),
            ("read_only_accounts_cache_misses", "2i"),
            ("read_only_accounts_cache_evicts", "1i"),
        ]));
        tap.observe(&point(&[
            ("read_only_accounts_cache_hits", "5i"),
            ("read_only_accounts_cache_misses", "1i"),
        ]));

        assert_eq!(
            tap.counters(),
            TapCounters {
                accounts_cache_hits: 15,
                accounts_cache_misses: 3,
                accounts_cache_evicts: 1,
                // Every other counter stays at nought: a point from one source must not move
                // another's figures.
                ..TapCounters::default()
            }
        );
    }

    #[test]
    fn test_other_points_are_ignored() {
        // The observer sees everything the process submits, most of which is
        // nothing to do with this.
        let tap = MetricsTap::default();
        let mut other = DataPoint::new("banking_stage-loop-stats");
        other
            .fields
            .push(("read_only_accounts_cache_hits", "99i".to_string()));
        tap.observe(&other);
        assert_eq!(tap.counters(), TapCounters::default());
    }

    #[test]
    fn test_shreds_are_counted_by_the_socket_they_arrived_on() {
        // The whole of the repair signal: one receiver per socket, and the
        // repair one carries only what this validator had to ask for.
        let tap = MetricsTap::default();
        tap.observe(&named(SHREDS_TURBINE, &[("packets_count", "900i")]));
        tap.observe(&named(SHREDS_REPAIR, &[("packets_count", "12i")]));
        tap.observe(&named(SHREDS_TURBINE, &[("packets_count", "100i")]));

        let counters = tap.counters();
        assert_eq!(counters.shreds_turbine, 1_000);
        assert_eq!(counters.shreds_repair, 12);
    }

    #[test]
    fn test_each_socket_receiver_counts_into_its_own_port() {
        // Each receiver's packets must land against the port it was read from, or a
        // row's share of traffic lost is wrong.
        let tap = MetricsTap::default();
        tap.observe(&named(SHREDS_TURBINE, &[("packets_count", "900i")]));
        tap.observe(&named(GOSSIP_RECEIVER, &[("packets_count", "40i")]));
        tap.observe(&named(TPU_VOTE_RECEIVER, &[("packets_count", "70i")]));
        tap.observe(&named(GOSSIP_RECEIVER, &[("packets_count", "2i")]));

        let counters = tap.counters();
        assert_eq!(counters.shreds_turbine, 900);
        assert_eq!(counters.packets_gossip, 42);
        assert_eq!(counters.packets_tpu_vote, 70);
    }

    #[test]
    fn test_a_replayed_slot_can_be_asked_for_its_own_wall_time() {
        // The three spans on replay's own thread, summed. The schedule page draws
        // this beside the slot's duration, so it has to be reachable by slot rather
        // than only as a window mean.
        let tap = MetricsTap::default();
        tap.observe(&named(
            REPLAY_SLOT_STATS,
            &[
                ("slot", "443895975i"),
                ("fetch_entries_time", "4300i"),
                ("confirmation_without_replay_us", "33500i"),
                ("bank_complete_time_us", "9400i"),
                ("execute_us", "231000i"),
            ],
        ));

        assert_eq!(
            tap.replayed(443_895_975).map(|times| times.serial()),
            Some(47_200)
        );
        assert!(tap.replayed(443_895_974).is_none());
    }

    #[test]
    fn test_a_replay_record_says_when_it_arrived() {
        // Replay reports as it finishes a slot, so the arrival is when replay
        // finished it, and the page measures that from the slot's first shred.
        let tap = MetricsTap::default();
        let before = now_millis();
        tap.observe(&named(
            REPLAY_SLOT_STATS,
            &[("slot", "443895975i"), ("execute_us", "231000i")],
        ));
        let observed = tap.replayed(443_895_975).unwrap().observed_millis;
        assert!(observed >= before && observed <= now_millis());
    }

    #[test]
    fn test_a_filled_slot_is_read_as_a_shred_count_and_a_repair_count() {
        // Names from `ledger/src/slot_stats.rs`. The last index is the highest
        // shred, so the count is one more.
        let tap = MetricsTap::default();
        tap.observe(&named(
            SHRED_FULL,
            &[
                ("slot", "444422652i"),
                ("total_time_ms", "921i"),
                ("last_index", "927i"),
                ("num_repaired", "63i"),
                ("num_recovered", "12i"),
            ],
        ));

        let fill = tap.shred_fill(444_422_652).unwrap();
        assert_eq!(fill.shreds, 928);
        assert_eq!(fill.repaired, 63);
        assert_eq!(fill.full_millis, 921);
        assert!(tap.shred_fill(444_422_651).is_none());
    }

    #[test]
    fn test_a_fill_with_no_last_index_is_dropped() {
        // The blockstore writes -1 where it has not seen the last shred, which
        // is not a count and must not become one.
        let tap = MetricsTap::default();
        tap.observe(&named(
            SHRED_FULL,
            &[
                ("slot", "10i"),
                ("total_time_ms", "400i"),
                ("last_index", "-1i"),
            ],
        ));
        assert!(tap.shred_fill(10).is_none());
    }

    #[test]
    fn test_only_the_newest_filled_slots_are_kept() {
        let tap = MetricsTap::default();
        for slot in 0..SHRED_FILLS.saturating_add(10) {
            tap.observe(&named(
                SHRED_FULL,
                &[("slot", &format!("{slot}i")), ("last_index", "99i")],
            ));
        }
        assert!(tap.shred_fill(9).is_none(), "the first ten dropped");
        assert!(tap.shred_fill(10).is_some());
        assert!(
            tap.shred_fill((SHRED_FILLS as u64).saturating_add(9))
                .is_some()
        );
    }

    #[test]
    fn test_each_named_sender_keeps_its_bytes_and_its_window_apart() {
        // Gossip reports every five seconds or so and repair every ten, so the
        // window has to travel with the bytes or a rate would be off by the
        // ratio of the two.
        let tap = MetricsTap::default();
        fn sent<'a>(bytes: &'a str, millis: &'a str) -> [(&'static str, &'a str); 3] {
            [
                (SENT_BYTES, bytes),
                (SENT_MILLIS, millis),
                ("streamer-send-host_count", "1000i"),
            ]
        }
        tap.observe(&named(GOSSIP_SENDER, &sent("22786715i", "5503i")));
        tap.observe(&named(REPAIR_SENDER, &sent("2138992i", "10004i")));
        tap.observe(&named(GOSSIP_SENDER, &sent("21012098i", "5383i")));

        let counters = tap.counters();
        assert_eq!(counters.gossip_sent_bytes, 43_798_813);
        assert_eq!(counters.gossip_sent_millis, 10_886);
        assert_eq!(counters.repair_sent_bytes, 2_138_992);
        assert_eq!(counters.repair_sent_millis, 10_004);
    }

    /// The waterfall point as the scheduler sends it, one field per counter.
    fn scheduler(fields: &[(&'static str, &str)]) -> DataPoint {
        named(SCHEDULER_COUNTS, fields)
    }

    #[test]
    fn test_the_waterfall_counters_land_where_they_belong() {
        // Several of these names differ only in their tail; a transposed pair would
        // put the wrong reason under a label.
        let tap = MetricsTap::default();
        tap.observe(&scheduler(&[
            ("num_received", "1000i"),
            ("num_dropped_on_receive", "900i"),
            ("num_dropped_on_check_work_queue_full", "1i"),
            ("num_dropped_on_parsing_and_sanitization", "2i"),
            ("num_dropped_on_validate_locks", "3i"),
            ("num_dropped_on_receive_compute_budget", "4i"),
            ("num_dropped_on_receive_age", "5i"),
            ("num_dropped_on_receive_already_processed", "6i"),
            ("num_dropped_on_receive_fee_payer", "7i"),
            ("num_dropped_on_filter_key", "8i"),
            ("num_dropped_on_nonce_dedup", "9i"),
            ("num_buffered", "55i"),
            ("num_dropped_on_capacity", "10i"),
            ("num_evicted_on_nonce_dedup", "11i"),
            ("num_dropped_on_clear", "12i"),
            ("num_dropped_on_clean", "13i"),
            ("num_scheduled", "40i"),
            ("num_unschedulable_conflicts", "14i"),
            ("num_unschedulable_threads", "15i"),
            ("num_finished", "38i"),
            ("num_retryable", "16i"),
        ]));

        let counters = tap.counters().scheduler;
        assert_eq!(counters.received, 1_000);
        assert_eq!(counters.not_held, 900);
        assert_eq!(counters.check_queue_full, 1);
        assert_eq!(counters.unparsable, 2);
        assert_eq!(counters.bad_locks, 3);
        assert_eq!(counters.compute_budget, 4);
        assert_eq!(counters.too_old, 5);
        assert_eq!(counters.already_processed, 6);
        assert_eq!(counters.fee_payer, 7);
        assert_eq!(counters.filtered, 8);
        assert_eq!(counters.nonce_conflict, 9);
        assert_eq!(counters.buffered, 55);
        assert_eq!(counters.queue_full, 10);
        assert_eq!(counters.nonce_evicted, 11);
        assert_eq!(counters.cleared, 12);
        assert_eq!(counters.cleaned, 13);
        assert_eq!(counters.scheduled, 40);
        assert_eq!(counters.blocked_conflicts, 14);
        assert_eq!(counters.blocked_threads, 15);
        assert_eq!(counters.finished, 38);
        assert_eq!(counters.retried, 16);
    }

    #[test]
    fn test_the_receive_stretch_of_that_point_balances() {
        // The identity the validator's own tests assert: everything received either
        // got in or has a reason it did not.
        let counters = MetricsTap::default();
        counters.observe(&scheduler(&[
            ("num_received", "1000i"),
            ("num_dropped_on_receive", "900i"),
            ("num_dropped_on_check_work_queue_full", "1i"),
            ("num_dropped_on_parsing_and_sanitization", "2i"),
            ("num_dropped_on_validate_locks", "3i"),
            ("num_dropped_on_receive_compute_budget", "4i"),
            ("num_dropped_on_receive_age", "5i"),
            ("num_dropped_on_receive_already_processed", "6i"),
            ("num_dropped_on_receive_fee_payer", "7i"),
            ("num_dropped_on_filter_key", "8i"),
            ("num_dropped_on_nonce_dedup", "9i"),
            ("num_buffered", "55i"),
        ]));

        let totals = counters.counters().scheduler;
        let accounted = [
            totals.not_held,
            totals.check_queue_full,
            totals.unparsable,
            totals.bad_locks,
            totals.compute_budget,
            totals.too_old,
            totals.already_processed,
            totals.fee_payer,
            totals.filtered,
            totals.nonce_conflict,
            totals.buffered,
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add);
        assert_eq!(accounted, totals.received);
    }

    #[test]
    fn test_a_window_of_readings_differences_and_sums() {
        // Differenced against the last reading, then summed across the window,
        // saturating at both steps.
        let first = SchedulerTotals {
            received: 100,
            buffered: 10,
            ..SchedulerTotals::default()
        };
        let second = SchedulerTotals {
            received: 250,
            buffered: 25,
            ..SchedulerTotals::default()
        };

        let step = second.since(&first);
        assert_eq!(step.received, 150);
        assert_eq!(step.buffered, 15);

        assert_eq!(step.plus(&step).received, 300);
        // Backwards, which only happens if a counter was reset under us.
        assert_eq!(first.since(&second).received, 0);
    }

    /// The per-slot point. The slot carries the trailing `i` that `add_field_i64`
    /// writes, formatted here so a fixture cannot leave it off.
    fn slot_point(slot: u64, fields: &[(&'static str, &str)]) -> DataPoint {
        let slot = format!("{slot}i");
        let mut all = vec![("slot", slot.as_str())];
        all.extend_from_slice(fields);
        named(SCHEDULER_SLOT_COUNTS, &all)
    }

    // ---- what a block cost ----------------------------------------------

    /// A cost tracker point as the validator sends it, tagged with whether this
    /// node produced the block.
    fn cost_point(is_leader: bool, fields: &[(&'static str, &str)]) -> DataPoint {
        let mut point = named(COST_TRACKER, fields);
        point.tags.push((IS_LEADER, is_leader.to_string()));
        point
    }

    #[test]
    fn test_a_block_this_validator_produced_is_kept() {
        let tap = MetricsTap::default();
        tap.observe(&cost_point(
            true,
            &[
                ("bank_slot", "441034909i"),
                ("block_cost", "42574937i"),
                (
                    "costliest_account",
                    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
                ),
                ("costliest_account_cost", "11842006i"),
                ("number_of_accounts", "3847i"),
                ("number_of_contended_accounts", "412i"),
                ("allocated_accounts_data_size", "421888i"),
                ("inflight_transaction_count", "0i"),
            ],
        ));

        let held = tap.slot_costs();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].slot, 441_034_909);
        assert_eq!(
            held[0].costliest_account,
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        );
        assert_eq!(held[0].costliest_cost, 11_842_006);
        assert_eq!(held[0].contended, 412);
    }

    #[test]
    fn test_other_validators_blocks_are_dropped() {
        // This point arrives for every slot replayed; only the blocks we built are
        // kept.
        let tap = MetricsTap::default();
        tap.observe(&cost_point(
            false,
            &[("bank_slot", "441034910i"), ("block_cost", "1i")],
        ));
        assert!(tap.slot_costs().is_empty());
    }

    #[test]
    fn test_a_cost_point_with_no_slot_is_dropped() {
        // It has nowhere to be shown: the panel joins these to blocks by slot.
        let tap = MetricsTap::default();
        tap.observe(&cost_point(true, &[("block_cost", "42574937i")]));
        assert!(tap.slot_costs().is_empty());
    }

    #[test]
    fn test_the_leader_tag_is_read_as_a_tag_not_a_field() {
        // `is_leader` is a tag, not a field.
        let tap = MetricsTap::default();
        let mut point = named(COST_TRACKER, &[("bank_slot", "441034909i")]);
        point.fields.push((IS_LEADER, "true".to_string()));
        tap.observe(&point);
        assert!(
            tap.slot_costs().is_empty(),
            "the validator does not send it as a field"
        );
    }

    /// One replay point, abridged to the fields the tap reads. Names taken from a
    /// real mainnet line.
    fn replay_point(fields: &[(&'static str, &str)]) -> DataPoint {
        named(REPLAY_SLOT_STATS, fields)
    }

    #[test]
    fn test_a_replayed_slot_is_read_field_by_field() {
        let tap = MetricsTap::default();
        tap.observe(&replay_point(&[
            ("fetch_entries_time", "2034i"),
            ("confirmation_without_replay_us", "17288i"),
            ("bank_complete_time_us", "443i"),
            ("entry_poh_verification_time", "28601i"),
            ("entry_transaction_verification_time", "13644i"),
            ("task_submission_us", "9828i"),
            ("execute_us", "176771i"),
            ("load_us", "26210i"),
            ("store_us", "10150i"),
            ("program_cache_us", "22954i"),
            ("total_transactions", "1232i"),
        ]));

        let held = tap.replay_slots();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].confirming, 17_288);
        assert_eq!(held[0].execute, 176_771);
        assert_eq!(held[0].transactions, 1_232);
        // Replay's own thread: the three sequential spans and nothing else.
        assert_eq!(held[0].serial(), 2_034 + 17_288 + 443);
        assert_eq!(held[0].cpu(), 176_771 + 26_210 + 10_150 + 22_954);
    }

    #[test]
    fn test_the_names_the_unified_scheduler_reports_under() {
        // The only spellings the unified scheduler sends; the older names would read
        // nought for ever.
        let tap = MetricsTap::default();
        tap.observe(&replay_point(&[
            ("confirmation_time_us", "17288i"),
            ("replay_time", "9828i"),
            ("execute_batches_us", "50000i"),
        ]));
        assert!(
            tap.replay_slots().is_empty(),
            "none of those names are sent by this validator"
        );
    }

    #[test]
    fn test_the_status_field_carries_no_micros_suffix() {
        // Every figure around it ends `_us` and this one does not, which is
        // exactly the sort of thing that gets tidied into a permanent nought.
        let tap = MetricsTap::default();
        tap.observe(&replay_point(&[("update_transaction_statuses", "1212i")]));
        assert_eq!(tap.replay_slots()[0].other, 1_212);
    }

    #[test]
    fn test_the_costs_of_a_program_cache_miss_are_summed() {
        // What an operator wants from these is what a miss cost, not which
        // third of the compiler it went to.
        let tap = MetricsTap::default();
        tap.observe(&replay_point(&[
            ("execute_details_create_executor_load_elf_us", "12506i"),
            ("execute_details_create_executor_verify_code_us", "1701i"),
            ("execute_details_create_executor_jit_compile_us", "7486i"),
        ]));
        assert_eq!(tap.replay_slots()[0].compiling, 12_506 + 1_701 + 7_486);
    }

    #[test]
    fn test_a_point_naming_nothing_this_reads_is_dropped() {
        // Not a slot that took no time: a point this does not understand.
        // Keeping it would drag every mean down and report the node as faster.
        let tap = MetricsTap::default();
        tap.observe(&replay_point(&[("some_field_from_a_later_release", "9i")]));
        assert!(tap.replay_slots().is_empty());
    }

    #[test]
    fn test_only_the_newest_replayed_slots_are_kept() {
        let tap = MetricsTap::default();
        for micros in 0..REPLAY_SLOTS.saturating_add(10) {
            tap.observe(&replay_point(&[("execute_us", &format!("{micros}i"))]));
        }

        let held = tap.replay_slots();
        assert_eq!(held.len(), REPLAY_SLOTS);
        assert_eq!(held[0].execute, 10, "oldest first, the first ten dropped");
    }

    #[test]
    fn test_a_leader_slot_is_kept_whole_rather_than_accumulated() {
        // Already one slot's own counts, so held as they arrived rather than added to
        // the running totals.
        let tap = MetricsTap::default();
        tap.observe(&slot_point(
            430_789_128,
            &[("num_received", "500i"), ("num_buffered", "80i")],
        ));
        tap.observe(&slot_point(
            430_789_129,
            &[("num_received", "600i"), ("num_buffered", "90i")],
        ));

        let held = tap.slot_waterfalls();
        assert_eq!(held.len(), 2);
        assert_eq!(held[0].slot, 430_789_128);
        assert_eq!(held[0].counts.received, 500);
        assert_eq!(held[1].counts.buffered, 90);
        // And the interval totals are untouched: this point is not one of them.
        assert_eq!(tap.counters().scheduler.received, 0);
    }

    #[test]
    fn test_the_slot_is_read_by_the_same_rule_as_every_other_integer() {
        // `add_field_i64` writes "430789128i", not "430789128". A change upstream
        // fails here rather than quietly emptying the panel.
        let tap = MetricsTap::default();
        let mut bare = DataPoint::new(SCHEDULER_SLOT_COUNTS);
        bare.fields.push(("slot", "430789128".to_string()));
        bare.fields.push(("num_received", "500i".to_string()));
        tap.observe(&bare);
        assert!(tap.slot_waterfalls().is_empty());

        // The same point written as the validator writes it.
        tap.observe(&slot_point(430_789_128, &[("num_received", "500i")]));
        assert_eq!(tap.slot_waterfalls().len(), 1);
    }

    #[test]
    fn test_a_waterfall_with_no_slot_is_dropped() {
        // It has nowhere to go. The panel joins these to blocks by slot number,
        // so one that cannot say which slot it describes belongs to none.
        let tap = MetricsTap::default();
        tap.observe(&named(SCHEDULER_SLOT_COUNTS, &[("num_received", "500i")]));
        assert!(tap.slot_waterfalls().is_empty());
    }

    #[test]
    fn test_a_slot_reported_twice_keeps_one_row() {
        // Appending would leave two rows describing one slot and push a real
        // one off the end of the queue.
        let tap = MetricsTap::default();
        tap.observe(&slot_point(100, &[("num_scheduled", "5i")]));
        tap.observe(&slot_point(100, &[("num_scheduled", "9i")]));

        let held = tap.slot_waterfalls();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].counts.scheduled, 9);
    }

    #[test]
    fn test_an_idle_scheduler_does_not_empty_a_slot_it_did_not_build() {
        // Two schedulers report every leader slot, and only the enabled one did the
        // work. Keeping the last to arrive emptied the panel on half of all slots.
        for order in [["10000", "0"], ["0", "10000"]] {
            let tap = MetricsTap::default();
            for id in order {
                // BAM built this one; the validator's own scheduler sat out and
                // has nothing but the packets it went on buffering.
                let fields: &[(&'static str, &str)] = if id == "10000" {
                    &[
                        ("num_received", "40i"),
                        ("num_buffered", "700i"),
                        ("num_scheduled", "738i"),
                        ("num_finished", "735i"),
                    ]
                } else {
                    &[("num_received", "710i"), ("num_buffered", "717i")]
                };
                tap.observe(&tagged_slot_point(id, 100, fields));
            }

            let held = tap.slot_waterfalls();
            assert_eq!(held.len(), 1, "one row per slot, whatever the order");
            assert_eq!(held[0].counts.finished, 735, "arrival order {order:?}");
            assert_eq!(held[0].source, SchedulerSource::Bam);
        }
    }

    #[test]
    fn test_the_interval_counts_say_which_scheduler_sent_them() {
        // Which scheduler reported decides whether `received` is packets or batches.
        let tap = MetricsTap::default();
        assert_eq!(
            tap.scheduler_source(),
            SchedulerSource::Scheduler,
            "a validator running one scheduler, before any point arrives"
        );

        let mut bam = named(SCHEDULER_COUNTS, &[("num_received", "5i")]);
        bam.tags.push((SCHEDULER_ID, "10000".to_string()));
        tap.observe(&bam);
        assert_eq!(tap.scheduler_source(), SchedulerSource::Bam);

        // And back, as it goes when BAM drops and the validator takes over.
        let mut own = named(SCHEDULER_COUNTS, &[("num_received", "5i")]);
        own.tags.push((SCHEDULER_ID, "0".to_string()));
        tap.observe(&own);
        assert_eq!(tap.scheduler_source(), SchedulerSource::Scheduler);

        // Untagged, as every stock validator sends it.
        tap.observe(&named(SCHEDULER_COUNTS, &[("num_received", "5i")]));
        assert_eq!(tap.scheduler_source(), SchedulerSource::Scheduler);
    }

    #[test]
    fn test_the_scheduler_that_built_the_slot_is_named() {
        // The two count arrivals in different units, so the panel needs to know which
        // built the block.
        let tap = MetricsTap::default();
        tap.observe(&tagged_slot_point("0", 1, &[("num_scheduled", "5i")]));
        tap.observe(&tagged_slot_point("10000", 2, &[("num_scheduled", "5i")]));
        // Untagged, as every stock validator sends it.
        tap.observe(&slot_point(3, &[("num_scheduled", "5i")]));

        let held = tap.slot_waterfalls();
        let sources: Vec<SchedulerSource> = held.iter().map(|slot| slot.source).collect();
        assert_eq!(
            sources,
            [
                SchedulerSource::Scheduler,
                SchedulerSource::Bam,
                SchedulerSource::Scheduler
            ]
        );
    }

    #[test]
    fn test_an_empty_leader_slot_keeps_the_report_that_saw_the_most() {
        // Nothing scheduled by either, so the tie falls to what was buffered.
        let tap = MetricsTap::default();
        tap.observe(&tagged_slot_point("10000", 100, &[("num_received", "2i")]));
        tap.observe(&tagged_slot_point("0", 100, &[("num_buffered", "31i")]));

        let held = tap.slot_waterfalls();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].counts.buffered, 31);
        assert_eq!(held[0].source, SchedulerSource::Scheduler);
    }

    #[test]
    fn test_only_the_newest_leader_slots_are_kept() {
        // Matched to what the produced block panel retains.
        let tap = MetricsTap::default();
        for slot in 0..u64::try_from(SLOT_WATERFALLS).unwrap().saturating_add(10) {
            tap.observe(&slot_point(slot, &[("num_received", "1i")]));
        }

        let held = tap.slot_waterfalls();
        assert_eq!(held.len(), SLOT_WATERFALLS);
        assert_eq!(held[0].slot, 10);
    }

    #[test]
    fn test_the_flush_point_is_read_under_either_spelling() {
        // The field was renamed between validator versions and this crate is carried
        // across both.
        for (accounts, bytes) in [
            ("num_accounts_stored", "account_bytes_stored"),
            ("num_accounts_flushed", "account_bytes_flushed"),
        ] {
            let tap = MetricsTap::default();
            tap.observe(&named(
                ACCOUNTS_FLUSH,
                &[(accounts, "500i"), (bytes, "64000i")],
            ));

            let read = tap.counters().accounts;
            assert_eq!(read.stored_accounts, 500, "{accounts}");
            assert_eq!(read.stored_bytes, 64_000, "{bytes}");
        }
    }

    /// Every field the QUIC panel reads, spelled as the point spells it.
    const QUIC_POINT: &[(&str, &str)] = &[
        ("total_incoming_connection_attempts", "18420i"),
        ("connection_rate_limited_across_all", "2140i"),
        ("connection_rate_limited_per_ipaddr", "6880i"),
        ("refused_connections_too_many_open_connections", "412i"),
        ("connection_setup_timeout", "1205i"),
        ("connection_setup_error", "338i"),
        ("new_connections", "7445i"),
        ("connection_add_failed", "7i"),
        ("connection_added_from_staked_peer", "1890i"),
        ("connection_added_from_unstaked_peer", "5548i"),
        ("new_streams", "42880i"),
        ("throttled_staked_streams", "0i"),
        ("throttled_unstaked_streams", "3412i"),
        ("stream_read_timeouts", "288i"),
        ("stream_read_errors", "41i"),
        ("invalid_stream_size", "12i"),
        ("packets_sent_to_consumer", "900i"),
        ("bytes_sent_to_consumer", "64000i"),
        ("total_handle_chunk_to_packet_send_full_err", "8i"),
        ("total_handle_chunk_to_packet_send_disconnected_err", "1i"),
        ("open_connections", "1284i"),
        ("active_streams", "46i"),
    ];

    #[test]
    fn test_the_quic_fields_are_the_ones_the_point_carries() {
        // Several of these are named after the counter behind them and several are
        // not. A name from the struct rather than the wire reads nought for ever.
        let tap = MetricsTap::default();
        tap.observe(&named(QUIC_TPU, QUIC_POINT));

        let read = tap.counters();
        let quic = read.quic;
        assert_eq!(quic.offered, 18_420);
        assert_eq!(quic.shed_all, 2_140);
        assert_eq!(quic.shed_address, 6_880);
        assert_eq!(quic.refused_full, 412);
        assert_eq!(quic.handshake_timeout, 1_205);
        assert_eq!(quic.handshake_error, 338);
        assert_eq!(quic.add_failed, 7);
        assert_eq!(quic.admitted_staked, 1_890);
        assert_eq!(quic.admitted_unstaked, 5_548);
        assert_eq!(quic.streams, 42_880);
        assert_eq!(quic.throttled_staked, 0);
        assert_eq!(quic.throttled_unstaked, 3_412);
        assert_eq!(quic.read_timeouts, 288);
        assert_eq!(quic.read_errors, 41);
        assert_eq!(quic.invalid_size, 12);
        assert_eq!(quic.handed_on, 900);
        assert_eq!(quic.bytes_handed_on, 64_000);
        assert_eq!(quic.queue_full, 8);
        assert_eq!(quic.disconnected, 1);
        assert_eq!(read.quic_levels.open, 1_284);
        assert_eq!(read.quic_levels.active_streams, 46);
    }

    #[test]
    fn test_the_shed_connections_account_for_the_offer() {
        // The listener sheds in order, so every attempt is shed at one gate, fails the
        // handshake, or is admitted, and the section is drawn against its own total.
        // Not quite a partition: a second rate-limit after the handshake and a failed
        // `accept()` are counted nowhere. This sample is built so neither happens;
        // `handshook` measures them when they do.
        let tap = MetricsTap::default();
        tap.observe(&named(QUIC_TPU, QUIC_POINT));

        let quic = tap.counters().quic;
        // Saturating: the workspace denies `arithmetic_side_effects` and these are
        // runtime values.
        let accounted = [
            quic.shed_all,
            quic.shed_address,
            quic.refused_full,
            quic.handshake_timeout,
            quic.handshake_error,
            quic.add_failed,
            quic.admitted_staked,
            quic.admitted_unstaked,
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add);
        assert_eq!(accounted, quic.offered);
    }

    #[test]
    fn test_the_handshake_checkpoint_is_read() {
        // The one figure that tells the two uncounted branches apart, both nought in
        // this clean sample.
        let tap = MetricsTap::default();
        tap.observe(&named(QUIC_TPU, QUIC_POINT));

        let quic = tap.counters().quic;
        assert_eq!(quic.handshook, 7_445);
        let after = [
            quic.add_failed,
            quic.admitted_staked,
            quic.admitted_unstaked,
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add);
        assert_eq!(after, quic.handshook);
    }

    #[test]
    fn test_the_refusal_counters_are_kept_apart_rather_than_summed() {
        // Four overlapping names for one refusal. The tap reads them apart; the panel
        // takes the larger rather than the sum.
        let tap = MetricsTap::default();
        tap.observe(&named(
            QUIC_TPU,
            &[
                ("connection_add_failed", "40i"),
                ("connection_add_failed_staked_node", "3i"),
                ("connection_add_failed_unstaked_node", "40i"),
                ("connection_add_failed_banned", "2i"),
            ],
        ));

        let quic = tap.counters().quic;
        assert_eq!(quic.add_failed, 40);
        assert_eq!(quic.add_failed_staked, 3);
        assert_eq!(quic.add_failed_unstaked, 40);
        assert_eq!(quic.add_failed_banned, 2);
    }

    #[test]
    fn test_the_pruning_alias_is_left_out_of_the_refusals() {
        // `connection_add_failed_on_pruning` is raised on the same refusal as
        // `..._staked_node` and means the same thing.
        let tap = MetricsTap::default();
        tap.observe(&named(
            QUIC_TPU,
            &[
                ("connection_add_failed_on_pruning", "9i"),
                ("connection_add_failed_staked_node", "9i"),
            ],
        ));

        let quic = tap.counters().quic;
        assert_eq!(quic.add_failed_staked, 9);
        assert_eq!(quic.add_failed, 0);
        assert_eq!(quic.add_failed_unstaked, 0);
        assert_eq!(quic.add_failed_banned, 0);
    }

    #[test]
    fn test_the_cumulative_offer_is_stored_rather_than_added() {
        // Every counter here is reported with `swap` except the offer, which arrives
        // cumulative. Added like its neighbours it would square inside a minute.
        let tap = MetricsTap::default();
        tap.observe(&named(
            QUIC_TPU,
            &[
                ("total_incoming_connection_attempts", "1000i"),
                ("new_streams", "10i"),
            ],
        ));
        tap.observe(&named(
            QUIC_TPU,
            &[
                ("total_incoming_connection_attempts", "1600i"),
                ("new_streams", "10i"),
            ],
        ));

        let quic = tap.counters().quic;
        assert_eq!(quic.offered, 1_600);
        assert_eq!(quic.streams, 20);
    }

    #[test]
    fn test_the_levels_are_the_latest_reading_rather_than_a_sum() {
        let tap = MetricsTap::default();
        tap.observe(&named(
            QUIC_TPU,
            &[("open_connections", "900i"), ("active_streams", "12i")],
        ));
        tap.observe(&named(
            QUIC_TPU,
            &[("open_connections", "870i"), ("active_streams", "9i")],
        ));

        let levels = tap.counters().quic_levels;
        assert_eq!(levels.open, 870);
        assert_eq!(levels.active_streams, 9);
    }

    #[test]
    fn test_each_quic_port_counts_into_its_own_set() {
        // Three listeners report the same field names under three point names;
        // summed, the busiest would hide the other two.
        let tap = MetricsTap::default();
        tap.observe(&named(QUIC_TPU, &[("new_streams", "900i")]));
        tap.observe(&named(QUIC_TPU_FORWARDS, &[("new_streams", "40i")]));
        tap.observe(&named(QUIC_TPU_VOTE, &[("new_streams", "70i")]));

        let read = tap.counters();
        assert_eq!(read.quic.streams, 900);
        assert_eq!(read.quic_forwards.streams, 40);
        assert_eq!(read.quic_vote.streams, 70);
    }

    #[test]
    fn test_the_accounts_points_add_into_one_set_of_figures() {
        // Three points from three parts of the accounts database, matched on field
        // name in one place.
        let tap = MetricsTap::default();
        tap.observe(&named(
            ACCOUNTS_LOADS,
            &[
                ("num_loaded_from_write_cache", "10i"),
                ("num_loaded_from_read_cache", "900i"),
                ("num_loaded_from_index_storage", "40i"),
            ],
        ));
        tap.observe(&named(
            ACCOUNTS_FLUSH,
            &[
                ("num_accounts_stored", "500i"),
                ("account_bytes_stored", "64000i"),
            ],
        ));
        tap.observe(&named(
            ACCOUNTS_STORES,
            &[
                ("total_bytes", "137400000000i"),
                ("total_alive_bytes", "27700000000i"),
                ("total_count", "812i"),
            ],
        ));

        let counters = tap.counters();
        assert_eq!(counters.accounts.loaded_from_storage, 40);
        assert_eq!(counters.accounts.stored_bytes, 64_000);
        assert_eq!(counters.accounts_storage_alive_bytes, 27_700_000_000);
        assert_eq!(counters.accounts_storage_count, 812);
    }

    #[test]
    fn test_the_storage_levels_are_replaced_rather_than_summed() {
        // A level, not a count: summed, a steady hundred gigabytes would read as
        // terabytes within a minute.
        let tap = MetricsTap::default();
        tap.observe(&named(ACCOUNTS_STORES, &[("total_bytes", "100i")]));
        tap.observe(&named(ACCOUNTS_STORES, &[("total_bytes", "104i")]));
        assert_eq!(tap.counters().accounts_storage_bytes, 104);
    }

    #[test]
    fn test_a_level_is_replaced_rather_than_accumulated() {
        // `water_level` is where the cache stood, not what happened since the last
        // point.
        let tap = MetricsTap::default();
        tap.observe(&named(PROGRAM_CACHE, &[("water_level", "400i")]));
        tap.observe(&named(PROGRAM_CACHE, &[("water_level", "412i")]));
        assert_eq!(tap.counters().program_cache_water_level, 412);
    }

    #[test]
    fn test_the_cache_counter_named_differently_from_its_field_still_lands() {
        // The point calls it `replace_entry` where the counter is `replacements`.
        let tap = MetricsTap::default();
        tap.observe(&named(
            PROGRAM_CACHE,
            &[("replace_entry", "3i"), ("hits", "90i"), ("misses", "10i")],
        ));

        let cache = tap.counters().program_cache;
        assert_eq!(cache.replacements, 3);
        assert_eq!(cache.hits, 90);
        assert_eq!(cache.misses, 10);
    }

    #[test]
    fn test_the_verify_stage_accounts_for_every_packet_it_was_given() {
        // There is no counter for a failed signature. It is what is left once
        // duplicates, underpaying and verified are taken off, which is exact because
        // sigverify discards at one step.
        let tap = MetricsTap::default();
        tap.observe(&named(
            TPU_VERIFIER,
            &[
                ("total_packets", "1000i"),
                ("total_dedup", "300i"),
                ("total_dropped_below_priority_floor", "50i"),
                ("total_valid_packets", "620i"),
                ("total_verify_time_us", "4200i"),
            ],
        ));

        let verify = tap.counters().verify;
        assert_eq!(verify.received, 1_000);
        let accounted = verify
            .duplicate
            .saturating_add(verify.below_floor)
            .saturating_add(verify.verified);
        assert_eq!(verify.received.saturating_sub(accounted), 30);
    }

    #[test]
    fn test_every_worker_adds_into_the_same_execution_totals() {
        // One point per worker, told apart by a tag nothing reads. Summing them gives
        // the stage's figure.
        let tap = MetricsTap::default();
        for _ in 0..4 {
            tap.observe(&named(
                WORKER_COUNTS,
                &[
                    ("transactions_attempted_processing_count", "100i"),
                    ("processed_transactions_count", "90i"),
                    ("processed_with_successful_result_count", "80i"),
                ],
            ));
        }

        let executed = tap.counters().executed;
        assert_eq!(executed.attempted, 400);
        assert_eq!(executed.processed, 360);
        assert_eq!(executed.succeeded, 320);
    }

    #[test]
    fn test_the_reasons_a_worker_dropped_a_transaction_join_its_counts() {
        // Two points from the same worker on the same tick: what became of the work
        // and why. Read into one set of counters.
        let tap = MetricsTap::default();
        tap.observe(&named(
            WORKER_COUNTS,
            &[
                ("transactions_attempted_processing_count", "101i"),
                ("retryable_transaction_count", "13i"),
                ("processed_transactions_count", "63i"),
                ("processed_with_successful_result_count", "63i"),
            ],
        ));
        tap.observe(&named(
            WORKER_ERROR_METRICS,
            &[
                ("blockhash_not_found", "12i"),
                ("insufficient_funds", "8i"),
                ("already_processed", "4i"),
                // Counted, but drawn as a retry rather than as a loss, so it
                // must not land in one of the reasons above.
                ("account_in_use", "13i"),
                // The sum of every error including the ones drawn elsewhere.
                // Reading it as a figure of its own would double the section.
                ("total", "37i"),
            ],
        ));

        let executed = tap.counters().executed;
        assert_eq!(executed.attempted, 101);
        assert_eq!(executed.retryable, 13);
        assert_eq!(executed.processed, 63);
        assert_eq!(executed.blockhash_missing, 12);
        assert_eq!(executed.fee_payer_broke, 8);
        assert_eq!(executed.already_processed, 4);
        // Neither of the two fields that would double-count reached a counter.
        assert_eq!(
            executed.attempted.saturating_sub(executed.processed),
            38,
            "nothing from the error point was added to the outcomes"
        );
    }

    #[test]
    fn test_the_vote_verifier_is_not_counted_with_the_rest() {
        // Votes never reach the scheduler, so counting them at the top would inflate a
        // total the stages below cannot account for.
        let tap = MetricsTap::default();
        tap.observe(&named("tpu-vote-verifier", &[("total_packets", "5000i")]));
        assert_eq!(tap.counters().verify.received, 0);
    }

    #[test]
    fn test_the_priority_gauges_are_left_out_of_the_waterfall() {
        // The only two fields on the point that are not counts.
        let tap = MetricsTap::default();
        tap.observe(&scheduler(&[
            ("min_priority", "5i"),
            ("max_priority", "900000i"),
            ("num_received", "7i"),
        ]));
        assert_eq!(tap.counters().scheduler.received, 7);
    }

    #[test]
    fn test_only_the_packet_count_is_taken_from_a_receiver() {
        // Those points carry timings and channel depths as well, and adding
        // those to a shred count would be nonsense rather than merely wrong.
        let tap = MetricsTap::default();
        tap.observe(&named(
            SHREDS_TURBINE,
            &[
                ("packet_batches_count", "7i"),
                ("packets_count", "900i"),
                ("channel_len", "3i"),
            ],
        ));
        assert_eq!(tap.counters().shreds_turbine, 900);
    }

    /// An XDP point built through the same calls the validator's macro makes, so
    /// the encodings are the real ones.
    fn xdp_point(
        zero_copy: bool,
        driver: &str,
        vendor: &str,
        model: &str,
        kernel: &str,
    ) -> DataPoint {
        let mut point = DataPoint::new(XDP_NETWORK_CONFIG);
        point.add_tag("driver", driver);
        point.add_tag("zero_copy", &zero_copy.to_string());
        point.add_field_str("kernel_version", kernel);
        point.add_field_str("vendor", vendor);
        point.add_field_str("model", model);
        point
    }

    #[test]
    fn test_the_xdp_config_is_read_from_the_tags_and_the_fields_alike() {
        // A tag keeps its value; a string field arrives wrapped in quotes for the line
        // protocol.
        let tap = MetricsTap::default();
        tap.observe(&xdp_point(
            true,
            "ice",
            "Intel Corporation",
            "Ethernet Controller E810-C for QSFP",
            "6.8.0-45-generic",
        ));

        let xdp = tap.xdp().expect("a reported config is held");
        assert!(xdp.zero_copy);
        assert_eq!(xdp.driver, "ice");
        assert_eq!(xdp.vendor, "Intel Corporation");
        assert_eq!(xdp.model, "Ethernet Controller E810-C for QSFP");
        assert_eq!(xdp.kernel_version, "6.8.0-45-generic");
    }

    #[test]
    fn test_there_is_no_xdp_config_where_none_was_ever_reported() {
        // The point is only submitted where an XDP config was given, so its absence
        // is the answer.
        let tap = MetricsTap::default();
        tap.observe(&named(SHREDS_TURBINE, &[("packets_count", "1i")]));
        assert!(tap.xdp().is_none());
    }

    #[test]
    fn test_a_missing_zero_copy_tag_reads_as_copy_rather_than_dropping_the_report() {
        // Everything else on the point is still worth having, and copy is the
        // reading that claims least.
        let mut point = DataPoint::new(XDP_NETWORK_CONFIG);
        point.add_tag("driver", "mlx5_core");
        point.add_field_str("model", "MT2892 Family");
        let tap = MetricsTap::default();
        tap.observe(&point);

        let xdp = tap
            .xdp()
            .expect("a config with no zero_copy tag is still a config");
        assert!(!xdp.zero_copy);
        assert_eq!(xdp.driver, "mlx5_core");
        assert_eq!(xdp.kernel_version, "");
    }

    #[test]
    fn test_the_latest_xdp_report_stands() {
        // Submitted on an interval and unchanging, so overwriting means a config read
        // late, once the PCI database was available, wins over an early read.
        let tap = MetricsTap::default();
        tap.observe(&xdp_point(false, "ice", "unknown", "unknown", "6.8.0"));
        tap.observe(&xdp_point(
            false,
            "ice",
            "Intel Corporation",
            "Ethernet Controller E810-C for QSFP",
            "6.8.0",
        ));
        assert_eq!(tap.xdp().unwrap().vendor, "Intel Corporation");
    }

    #[test]
    fn test_a_string_field_keeps_a_quote_that_was_part_of_the_value() {
        // The wrapper is one pair, not every quote at either end, and a quote
        // inside the value arrives escaped.
        assert_eq!(field_str(r#""6.8.0""#), "6.8.0");
        assert_eq!(field_str(r#""a \"b\" c""#), r#"a "b" c"#);
        assert_eq!(field_str("unquoted"), "unquoted");
    }
}
