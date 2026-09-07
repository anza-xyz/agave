//! The once-a-second readings: throughput, host, network, sockets, caches and
//! the TPU path.
//!
//! On their own thread, apart from the slot sampling in [`crate::collect`], so
//! a slow blockstore read does not stall every panel. Bank forks is taken with
//! `try_read`: if replay holds it the sample is skipped. A gap in a chart is
//! honest; a stalled heartbeat is not.

use {
    crate::{
        collect::{CATCH_UP_SLOTS_PER_SECOND, system_time_nanos},
        context::{DashboardContext, StartProgress},
        host_stats::{self, HostSnapshot},
        metrics_tap::{
            AccountsTotals, BundleTotals, ExecutedTotals, MetricsTap, ProgramCacheTotals,
            QuicLevels, QuicTotals, ReplaySlotTimes, SchedulerSource, SchedulerTotals, SlotCost,
            SlotWaterfall, TapCounters, VerifyTotals, WindowedCounters, XdpConfig,
        },
        net_stats::{self, NetCounters},
        proto::{Debounced, Publisher, TOPIC_SUMMARY},
        thread_stats::{self, ThreadGroup, ThreadReading},
        udp_drops::{self, PortCounters, PortWindow},
    },
    serde::Serialize,
    solana_clock::{Epoch, Slot},
    solana_core::validator::ValidatorStartProgress,
    solana_gossip::contact_info::Protocol,
    solana_program_runtime::loaded_programs::MAX_LOADED_ENTRY_COUNT,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    std::{
        collections::{BTreeMap, HashMap, HashSet, VecDeque},
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant, SystemTime},
    },
};

/// How often these readings are taken.
pub const METER_INTERVAL: Duration = Duration::from_secs(1);

/// Samples retained for the transaction and network charts: five minutes at
/// one a second, which is what the client keeps.
const CHART_HISTORY: usize = 300;

/// Samples the thread panel keeps: the minute it draws. Shorter than the
/// charts because each sample is nine rows rather than two figures.
const THREADS_HISTORY: usize = 60;

/// Thread groups given a row of their own; the rest share one.
const THREAD_ROWS: usize = 8;

/// Window the reported socket drops accumulate over. Long enough that a burst
/// stays visible for a while after it stops, short enough that it clears.
const DROPS_WINDOW: Duration = Duration::from_secs(60);

/// Samples the accounts cache hit rate is taken over, a minute of them, to
/// match the program cache beside it.
const ACCOUNTS_CACHE_WINDOW: usize = 60;

/// Samples the shred figures span. Five minutes: repair is bursty, and a
/// minute says more about the last burst than about the connection.
const SHREDS_WINDOW: usize = 300;

/// Samples the waterfall is summed over. Five minutes, because the leader half
/// only moves for four slots every couple of minutes.
const WATERFALL_WINDOW: usize = 300;

/// Samples the program cache rate spans. One sample is one slot's handful of
/// loads, so a rate from it alone swings between nothing and everything.
const PROGRAM_CACHE_WINDOW: usize = 60;

/// Where this validator's shreds came from over the window. A rising repair
/// share means the cluster is not reaching this node.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shreds {
    /// Shreds received over the window, however they arrived.
    pub received: u64,
    /// Of those, the ones this validator had to ask another node for.
    pub repaired: u64,
    /// The share it had to ask for, in `[0, 1]`.
    pub repair_rate: f64,
}

/// How often an account replay needed was already in memory, over the last
/// minute of the accounts database's own once-a-second points.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountsCache {
    /// Reads in the window, hits and misses together.
    pub read: u64,
    /// Of those, the ones already cached, in `[0, 1]`.
    pub hit_rate: f64,
    /// Accounts dropped from the cache over the window.
    pub evictions: u64,
    /// What the cache is holding right now.
    pub cache_bytes: u64,
    pub cache_entries: u64,

    /// Where reads were answered from. `from_storage` is the only one that touches
    /// a file, counted in accounts because nothing counts bytes.
    pub from_write_cache: u64,
    pub from_read_cache: u64,
    pub from_storage: u64,

    /// Accounts written out to storage over the window, and their size. Unlike
    /// the read side this does have a byte figure.
    pub stored_accounts: u64,
    pub stored_bytes: u64,
    /// Seconds the window actually covers, so the panel can turn the totals
    /// above into rates without assuming it is full.
    pub window_seconds: f64,

    /// How much storage exists, how much is live, and how many files. `None` until
    /// the accounts database has reported once, on a clean cycle rather than a
    /// timer.
    pub disk: Option<AccountsDisk>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountsDisk {
    /// Bytes still referenced by a live account.
    pub used: u64,
    /// Bytes the storage files occupy.
    pub allocated: u64,
    /// Dead account data still on disk, which is what shrink reclaims.
    pub fragmented: u64,
    pub storages: u64,
}

/// How often replay found a program already compiled. The counters reset per
/// bank, so `looked_up` is the window's own total, not since startup.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProgramCache {
    /// Loads seen in the window, hits and misses together.
    pub looked_up: u64,
    pub hits: u64,
    pub misses: u64,
    /// Of those, the ones already compiled and in the cache, in `[0, 1]`.
    pub hit_rate: f64,
    /// Compiled programs dropped from the cache over the window, which is the
    /// usual reason a hit rate falls.
    pub evictions: u64,
    /// An evicted program being compiled again: the cost of an eviction, paid
    /// on the next block that wants it.
    pub reloads: u64,
    /// Programs added, and additions thrown away because the fork they were
    /// for had gone by the time they finished compiling.
    pub insertions: u64,
    pub lost_insertions: u64,
    /// Something already cached compiled a second time by mistake.
    pub replacements: u64,
    /// Compiled, used once, and evicted — cache space spent for nothing.
    pub one_hit_wonders: u64,
    /// Dropped for an abandoned fork, or not recompiled for the incoming epoch.
    /// Neither is a fault.
    pub prunes_orphan: u64,
    pub prunes_environment: u64,
    /// The most entries seen loaded at any eviction in the window, against the
    /// limit. `None` until an eviction has happened at all.
    pub peak_entries: Option<u64>,
    pub entry_limit: u64,
}

/// A window of `(hits, misses, evictions)` as `(asked, rate, evicted)`, or
/// `None` while nothing has been asked: a hit rate of nought reads as a cache
/// that is failing.
fn cache_rate(window: &VecDeque<(u64, u64, u64)>) -> Option<(u64, f64, u64)> {
    let mut hits = 0u64;
    let mut misses = 0u64;
    let mut evictions = 0u64;
    for (sample_hits, sample_misses, sample_evictions) in window {
        hits = hits.saturating_add(*sample_hits);
        misses = misses.saturating_add(*sample_misses);
        evictions = evictions.saturating_add(*sample_evictions);
    }

    let asked = hits.saturating_add(misses);
    (asked > 0).then(|| (asked, hits as f64 / asked as f64, evictions))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tps {
    pub total: f64,
    pub vote: f64,
    pub non_vote_success: f64,
    pub non_vote_failed: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TpsSample {
    pub slot: Slot,
    pub timestamp_nanos: u64,
    #[serde(flatten)]
    pub tps: Tps,
}

/// Host interface throughput, in bytes per second.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Network {
    pub received_per_second: u64,
    pub sent_per_second: u64,
}

/// The host the validator runs on. Load and memory are what the process has to
/// work with, `filesystems` what will run out of room, `devices` what will run
/// out of throughput.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Host {
    pub cores: usize,
    pub load_one: f64,
    pub load_five: f64,
    pub load_fifteen: f64,
    pub threads: u64,
    pub running: u64,

    pub memory_total: u64,
    pub memory_available: u64,
    /// What the kernel will hand back on demand. "Used" counts it, and sixty
    /// gigabytes of page cache is not a shortage.
    pub memory_reclaimable: u64,
    /// Untouched. With `memory_reclaimable` this gives what is genuinely spoken
    /// for.
    pub memory_free: u64,
    /// Absent where `SwapTotal` is nought. A machine with no swap has nothing
    /// to report and nothing to warn about.
    pub swap: Option<Swap>,

    pub filesystems: Vec<FilesystemUsage>,
    pub devices: Vec<DeviceLoad>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Swap {
    pub total: u64,
    pub used: u64,
}

/// How full one filesystem is. A level, so nothing here is a rate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FilesystemUsage {
    /// What the validator uses it for, which is what an operator recognises it
    /// by. One filesystem holding several accounts directories is named once.
    pub name: String,
    pub path: String,
    pub total: u64,
    pub available: u64,
}

/// How hard one block device is being worked, over the last sample.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceLoad {
    pub device: String,
    /// The roles whose paths resolve to this device, joined for the label. Two
    /// mounts on one disk share its queue, so they share one row.
    pub roles: Vec<String>,
    /// Share of the sample the device had a request in flight, in `[0, 1]`.
    pub busy: f64,
    /// Mean milliseconds a request waited, absent where none did.
    pub wait_ms: Option<f64>,
    pub operations_per_second: u64,
    pub read_per_second: u64,
    pub write_per_second: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NetworkSample {
    pub timestamp_nanos: u64,
    #[serde(flatten)]
    pub rates: Network,
}

/// Where the validator's threads spent one second: the busiest groups by their
/// minute's mean, and one row for the rest.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ThreadsSample {
    pub timestamp_nanos: u64,
    /// Threads in the process, every group included.
    pub threads: usize,
    pub groups: Vec<ThreadGroup>,
}

/// Kernel-side receive health for one of this validator's UDP ports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IngestPath {
    /// Which service the port belongs to, used as the row's label and key.
    pub name: &'static str,
    pub port: u16,
    /// Drops over the trailing window. Always sent, including as zero, so a
    /// healthy row and an unmeasured one look different.
    pub drops_recent: u64,
    /// Drops since the validator finished starting.
    pub drops_total: u64,
    /// Bytes waiting unread at the instant of the sample.
    pub queued_bytes: u64,

    /// Packets the port handed over across the same window, so the panel can show
    /// a share lost. Missing for a port with no receiver reporting one, which is
    /// four of the seven; nought received alongside drops would read as total
    /// loss. `Some(0)` is possible where the validator logs below info and the
    /// counting points never fire, so the panel shows a share only where something
    /// was counted.
    pub received_recent: Option<u64>,
    pub received_total: Option<u64>,

    /// Whether this port speaks QUIC, which decides whether the socket panel or the
    /// TPU path panel draws it.
    pub quic: bool,
}

/// One QUIC port's account of what was offered and what got through. Per port,
/// since the three are separate listeners with separate limits.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuicPort {
    /// The same name the socket list uses for this port.
    pub name: &'static str,
    #[serde(flatten)]
    pub counts: QuicTotals,
    #[serde(flatten)]
    pub levels: QuicLevels,
    /// Datagrams the kernel discarded on this port over the same span as the counts
    /// beside them. Whole datagrams where everything else is connections or
    /// transactions, so drawn without a bar and never added.
    pub kernel_drops: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuicPaths {
    pub window_seconds: f64,
    pub ports: Vec<QuicPort>,
    /// Whether the TPU address advertised in gossip is a socket on this host. It is
    /// not behind a relayer or block-assembly proxy, and the listener then reports
    /// almost nothing, which reads as a fault without this. Says only that the
    /// address is answered elsewhere, never by what.
    pub tpu_offhost: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IngestSummary {
    /// What `drops_recent` actually spans, short until the window has filled.
    pub window_seconds: f64,
    pub paths: Vec<IngestPath>,
}

/// The live waterfall, and which scheduler produced its counts, which decides
/// what `received` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WaterfallWindow {
    #[serde(flatten)]
    pub counts: SchedulerTotals,
    pub source: SchedulerSource,
}

/// What the two per-epoch sections of the TPU path card cover, in slots. Two
/// figures because a restart part way through an epoch leaves totals honest
/// about a shorter span than the heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EpochSpan {
    pub epoch: Epoch,
    /// Slots of this epoch that have happened.
    pub elapsed_slots: u64,
    /// Slots of this epoch the totals were actually summed over, which is
    /// fewer than `elapsed_slots` where counting began part way in.
    pub counted_slots: u64,
    pub slots_in_epoch: u64,
}

/// Where the chain is in its epoch, taken from a bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EpochPosition {
    epoch: Epoch,
    slot: Slot,
    start_slot: Slot,
    slots_in_epoch: u64,
}

/// Verify, Executed and the bundles, summed from the start of the epoch. One
/// structure because they share an epoch and starting slot; the bundles are
/// printed on Executed's line and must cover the same span.
#[derive(Debug, Clone, Copy, Default)]
struct LeaderTotals {
    /// The epoch these cover. `None` until the first sample lands.
    epoch: Option<Epoch>,
    /// The slot the summing began at: the epoch's first slot where the
    /// validator has been up for all of it, and later where it has not.
    from_slot: Slot,
    verify: VerifyTotals,
    executed: ExecutedTotals,
    bundles: BundleTotals,
}

impl LeaderTotals {
    /// Adds one interval's work, starting over where the epoch has turned.
    fn add(
        &mut self,
        at: EpochPosition,
        verify: VerifyTotals,
        executed: ExecutedTotals,
        bundles: BundleTotals,
    ) {
        if self.epoch != Some(at.epoch) {
            self.epoch = Some(at.epoch);
            self.from_slot = at.slot;
            self.verify = VerifyTotals::default();
            self.executed = ExecutedTotals::default();
            self.bundles = BundleTotals::default();
        }
        self.verify = self.verify.plus(&verify);
        self.executed = self.executed.plus(&executed);
        self.bundles = self.bundles.plus(&bundles);
    }

    /// What the totals cover as of the last position added at, inclusive at both
    /// ends. `from_slot` is clamped to the epoch's first slot, since a bank read
    /// can land before the one that turned the epoch.
    fn span(&self, at: EpochPosition) -> EpochSpan {
        let from_slot = self.from_slot.max(at.start_slot);
        EpochSpan {
            epoch: at.epoch,
            elapsed_slots: at
                .slot
                .saturating_sub(at.start_slot)
                .saturating_add(1)
                .min(at.slots_in_epoch),
            counted_slots: at
                .slot
                .saturating_sub(from_slot)
                .saturating_add(1)
                .min(at.slots_in_epoch),
            slots_in_epoch: at.slots_in_epoch,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
/// What replay did with the last few hundred slots, in microseconds. Every
/// figure but the two peaks is a mean per slot; the peaks are the worst single
/// slot's sums, not the maximum of each field, which land on different slots.
pub struct ReplayWindow {
    /// Slots behind the figures, so the panel can name what it is showing
    /// rather than claim a window it has not yet filled.
    pub slots: usize,
    pub transactions: u64,

    // Replay's own thread. Disjoint spans; these add up.
    pub fetch: u64,
    pub confirming: u64,
    pub completing: u64,
    pub serial_peak: u64,

    // Verification jobs. Concurrent and each parallel inside, so these are
    // comparable to one another and to nothing else.
    pub poh_verify: u64,
    pub tx_verify: u64,
    pub dispatch: u64,

    // Thread time across the workers. These partition.
    pub execute: u64,
    pub bytecode: u64,
    pub serialising: u64,
    pub deserialising: u64,
    pub load: u64,
    pub store: u64,
    pub program_cache: u64,
    pub compiling: u64,
    pub program_cache_peak: u64,
    pub checking: u64,
    pub other: u64,
    pub cpu_peak: u64,
}

/// Averages a window of replayed slots and finds its worst. `None` until a
/// slot has been replayed, so the panel stays absent rather than showing
/// noughts.
fn replay_window(slots: &[ReplaySlotTimes]) -> Option<ReplayWindow> {
    let count = u64::try_from(slots.len()).ok().filter(|n| *n > 0)?;
    // Checked: the workspace denies bare arithmetic and cannot see the guard
    // above.
    let mean = |total: u64| total.checked_div(count).unwrap_or_default();
    let sum = |pick: fn(&ReplaySlotTimes) -> u64| {
        slots
            .iter()
            .fold(0u64, |total, slot| total.saturating_add(pick(slot)))
    };
    let peak = |pick: fn(&ReplaySlotTimes) -> u64| slots.iter().map(pick).max().unwrap_or_default();

    Some(ReplayWindow {
        slots: slots.len(),
        transactions: mean(sum(|s| s.transactions)),

        fetch: mean(sum(|s| s.fetch)),
        confirming: mean(sum(|s| s.confirming)),
        completing: mean(sum(|s| s.completing)),
        serial_peak: peak(ReplaySlotTimes::serial),

        poh_verify: mean(sum(|s| s.poh_verify)),
        tx_verify: mean(sum(|s| s.tx_verify)),
        dispatch: mean(sum(|s| s.dispatch)),

        execute: mean(sum(|s| s.execute)),
        bytecode: mean(sum(|s| s.bytecode)),
        serialising: mean(sum(|s| s.serialising)),
        deserialising: mean(sum(|s| s.deserialising)),
        load: mean(sum(|s| s.load)),
        store: mean(sum(|s| s.store)),
        program_cache: mean(sum(|s| s.program_cache)),
        compiling: mean(sum(|s| s.compiling)),
        program_cache_peak: peak(|s| s.program_cache),
        checking: mean(sum(|s| s.checking)),
        other: mean(sum(|s| s.other)),
        cpu_peak: peak(ReplaySlotTimes::cpu),
    })
}

/// One row's two identities: the port the kernel counts drops against, and the
/// validator's own count of what it delivered, where there is one.
struct IngestPort {
    name: &'static str,
    port: u16,
    /// Running total of packets delivered, or `None` for a port whose traffic
    /// nothing counts in datagrams.
    received: Option<u64>,
    quic: bool,
}

/// Cumulative transaction counters, differenced between samples for a rate.
/// Total and non-vote are inherited from the parent bank; the error count
/// resets per bank, so `errors` is a running sum the caller adds to as banks
/// freeze.
#[derive(Clone, Copy)]
struct TxnCounters {
    slot: Slot,
    total: u64,
    non_vote: u64,
    errors: u64,
    sampled_at: Instant,
}

impl TxnCounters {
    fn read(bank: &Bank, errors: u64) -> Self {
        Self {
            slot: bank.slot(),
            total: bank.transaction_count(),
            non_vote: bank.non_vote_transaction_count_since_restart(),
            errors,
            sampled_at: Instant::now(),
        }
    }
}

/// Failed transactions in frozen banks newer than `counted_to`, and the newest
/// frozen slot. With no `counted_to` the newest slot becomes the baseline.
fn frozen_errors(bank_forks: &BankForks, counted_to: Option<Slot>) -> (u64, Option<Slot>) {
    let mut errors = 0u64;
    let mut newest = counted_to;
    for (slot, bank) in bank_forks.frozen_banks() {
        if counted_to.is_some_and(|counted_to| slot > counted_to) {
            errors = errors.saturating_add(bank.transaction_error_count());
        }
        if newest.is_none_or(|newest| slot > newest) {
            newest = Some(slot);
        }
    }
    (errors, newest)
}

/// One path the validator writes to, and what it is for.
#[derive(Debug, Clone, PartialEq)]
struct HostPath {
    name: String,
    path: PathBuf,
}

/// The paths worth reporting, one row per filesystem: several accounts
/// directories on one mount are one lot of free space, and reported per path
/// would read as four disks filling.
fn resolve_host_paths(ctx: &DashboardContext) -> Vec<HostPath> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    let ledger = ctx.blockstore.ledger_path().clone();
    if let Ok(id) = host_stats::filesystem_id(&ledger) {
        seen.insert(id);
        paths.push(HostPath {
            name: "ledger".to_owned(),
            path: ledger,
        });
    }

    let mut accounts = Vec::new();
    for path in &ctx.account_paths {
        let Ok(id) = host_stats::filesystem_id(path) else {
            continue;
        };
        if seen.insert(id) {
            accounts.push(path.clone());
        }
    }
    // Numbered only when there is more than one to tell apart. A single
    // "accounts 1" reads as though something is missing.
    let numbered = accounts.len() > 1;
    for (index, path) in accounts.into_iter().enumerate() {
        let ordinal = index.saturating_add(1);
        paths.push(HostPath {
            name: if numbered {
                format!("accounts {ordinal}")
            } else {
                "accounts".to_owned()
            },
            path,
        });
    }

    paths
}

/// What each device behind those paths did over the sample, grouped by device
/// since two mounts on one disk share a queue.
fn device_loads(
    paths: &[HostPath],
    current: &HostSnapshot,
    previous: &HostSnapshot,
    interval_ms: f64,
    seconds: f64,
) -> Vec<DeviceLoad> {
    let mut roles: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in paths {
        // `Ok(None)` is a filesystem with no block device under it, such as tmpfs.
        if let Ok(Some(device)) = host_stats::device_for(&path.path) {
            roles.entry(device).or_default().push(path.name.clone());
        }
    }

    roles
        .into_iter()
        .filter_map(|(device, roles)| {
            let now = current.disks.get(&device)?;
            let before = previous.disks.get(&device)?;
            let delta = now.since(before)?;
            Some(DeviceLoad {
                device,
                roles,
                busy: delta.busy(interval_ms).unwrap_or_default(),
                wait_ms: delta.wait_ms(),
                operations_per_second: (delta.operations() as f64 / seconds) as u64,
                read_per_second: (delta.read_bytes() as f64 / seconds) as u64,
                write_per_second: (delta.write_bytes() as f64 / seconds) as u64,
            })
        })
        .collect()
}

/// Cores the machine has, for reading load average against. Context, not a
/// denominator.
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

pub struct Meters {
    ctx: DashboardContext,
    publisher: Arc<Publisher>,
    /// Read directly rather than shared with the collector, so the two threads
    /// need nothing between them.
    startup_progress: StartProgress,
    /// When the dashboard came up, for the uptime readout.
    started: SystemTime,
    metrics_tap: Arc<MetricsTap>,
    /// Running totals as of the last reading, so each sample is the difference.
    last_tap: Option<TapCounters>,

    throughput: Throughput,
    network: NetworkMeter,
    host: HostMeter,
    sockets: SocketMeter,
    shreds: ShredMeter,
    egress: EgressMeter,
    accounts: AccountsMeter,
    program_cache: ProgramCacheMeter,
    tpu: TpuMeter,
    threads: ThreadMeter,
}

impl Meters {
    pub fn new(
        ctx: DashboardContext,
        publisher: Arc<Publisher>,
        startup_progress: StartProgress,
        started: SystemTime,
        metrics_tap: Arc<MetricsTap>,
    ) -> Self {
        Self {
            ctx,
            publisher,
            startup_progress,
            started,
            metrics_tap,
            last_tap: None,
            throughput: Throughput::new(),
            network: NetworkMeter::default(),
            host: HostMeter::default(),
            sockets: SocketMeter::new(),
            shreds: ShredMeter::new(),
            egress: EgressMeter::default(),
            accounts: AccountsMeter::new(),
            program_cache: ProgramCacheMeter::new(),
            tpu: TpuMeter::new(),
            threads: ThreadMeter::default(),
        }
    }

    pub fn tick(&mut self) {
        self.collect_clock();

        // Taken without waiting: replay holds bank forks to advance, and this thread
        // exists so the readings survive a busy validator.
        let working_bank = match self.ctx.bank_forks.try_read() {
            Ok(bank_forks) => {
                self.throughput.count_frozen(&bank_forks);
                Some(bank_forks.working_bank())
            }
            Err(_) => None,
        };
        if let Some(working_bank) = working_bank {
            self.throughput.tick(&working_bank, &self.publisher);
            self.note_epoch(&working_bank);
        }

        self.network.tick(&self.publisher);
        self.collect_xdp();
        self.host.tick(&self.ctx, &self.publisher);
        self.threads.tick(&self.publisher);
        self.collect_ingest_paths();
        self.collect_from_metrics();
    }

    fn collect_clock(&self) {
        let now = SystemTime::now();
        self.publisher
            .publish(TOPIC_SUMMARY, "server_time_nanos", &system_time_nanos(now));
        let uptime = now
            .duration_since(self.started)
            .unwrap_or_default()
            .as_nanos() as u64;
        self.publisher
            .publish(TOPIC_SUMMARY, "uptime_nanos", &uptime);
    }

    fn note_epoch(&mut self, working_bank: &Bank) {
        self.tpu.note_epoch(working_bank);
    }

    fn collect_xdp(&mut self) {
        self.tpu.collect_xdp(&self.metrics_tap, &self.publisher);
    }

    fn collect_ingest_paths(&mut self) {
        let running = matches!(
            *self.startup_progress.read().unwrap(),
            ValidatorStartProgress::Running
        );
        self.sockets.tick(
            &self.ctx,
            &self.metrics_tap.counters(),
            running,
            &self.publisher,
        );
    }

    /// The readings from the metrics tap, each the difference against the last.
    /// The first reading sets the baseline: from zero it would report everything
    /// since startup as one second's work.
    fn collect_from_metrics(&mut self) {
        let current = self.metrics_tap.counters();
        let Some(previous) = self.last_tap.replace(current) else {
            return;
        };
        self.shreds.tick(&previous, &current, &self.publisher);
        self.egress.tick(&previous, &current, &self.publisher);
        self.collect_waterfall(&previous, &current);
        self.program_cache
            .tick(&previous, &current, &self.publisher);
        self.accounts.tick(&previous, &current, &self.publisher);
    }

    fn collect_waterfall(&mut self, previous: &TapCounters, current: &TapCounters) {
        // Whether the advertised TPU port is bound here: a port missing from the
        // kernel's table is one this host is not listening on. Only answerable while
        // that table can be read.
        let tpu_offhost =
            !self.sockets.unavailable && !self.sockets.kernel_drops.contains_key("tpu");
        self.tpu.tick(
            &self.metrics_tap,
            previous,
            current,
            &self.sockets.kernel_drops,
            tpu_offhost,
            &self.publisher,
        );
    }
}

/// Transactions per second, from the working bank's counters.
struct Throughput {
    last_counters: Option<TxnCounters>,
    /// Failed transactions summed over frozen banks, and the slot summed to.
    errors_total: u64,
    errors_counted_to: Option<Slot>,
    history: Vec<TpsSample>,
}

impl Throughput {
    fn new() -> Self {
        Self {
            last_counters: None,
            errors_total: 0,
            errors_counted_to: None,
            history: Vec::with_capacity(CHART_HISTORY),
        }
    }

    /// Adds the failures in the banks frozen since the last call.
    fn count_frozen(&mut self, bank_forks: &BankForks) {
        let (errors, counted_to) = frozen_errors(bank_forks, self.errors_counted_to);
        self.errors_total = self.errors_total.saturating_add(errors);
        self.errors_counted_to = counted_to;
    }

    fn tick(&mut self, working_bank: &Bank, publisher: &Publisher) {
        let current = TxnCounters::read(working_bank, self.errors_total);
        let Some(previous) = self.last_counters.replace(current) else {
            return;
        };
        // A fork switch or a restart makes the counters incomparable.
        if current.slot <= previous.slot || current.total < previous.total {
            return;
        }
        let seconds = current
            .sampled_at
            .duration_since(previous.sampled_at)
            .as_secs_f64();
        if seconds <= 0.0 {
            return;
        }

        // While catching up, replay chews through slots far faster than the cluster
        // produces them, and one such sample would pin the chart's scale.
        let slots_per_second = current.slot.saturating_sub(previous.slot) as f64 / seconds;
        if slots_per_second > CATCH_UP_SLOTS_PER_SECOND {
            return;
        }

        let total = current.total.saturating_sub(previous.total) as f64 / seconds;
        let non_vote = current.non_vote.saturating_sub(previous.non_vote) as f64 / seconds;
        // Errors are not split by vote and non-vote. Failed votes are rare
        // enough that attributing every failure to non-vote traffic is close.
        let failed = current.errors.saturating_sub(previous.errors) as f64 / seconds;
        let tps = Tps {
            total,
            vote: (total - non_vote).max(0.0),
            non_vote_success: (non_vote - failed).max(0.0),
            non_vote_failed: failed.min(non_vote),
        };

        publisher.publish(TOPIC_SUMMARY, "estimated_tps", &tps);

        let sample = TpsSample {
            slot: current.slot,
            timestamp_nanos: system_time_nanos(SystemTime::now()),
            tps,
        };
        publisher.publish_ephemeral(TOPIC_SUMMARY, "tps_sample", &sample);

        push_history(&mut self.history, sample, publisher, "tps_history");
    }
}

/// Host interface throughput, derived from cumulative counters.
#[derive(Default)]
struct NetworkMeter {
    last: Option<(NetCounters, Instant)>,
    history: Vec<NetworkSample>,
    /// Set once the counters prove unreadable, so the failure is logged once
    /// rather than every second.
    unavailable: bool,
}

impl NetworkMeter {
    /// Publishes nothing when the counters cannot be read, so the panel is
    /// absent rather than showing zeros that look like an idle network.
    fn tick(&mut self, publisher: &Publisher) {
        if self.unavailable {
            return;
        }
        let current = match net_stats::read() {
            Ok(counters) => counters,
            Err(err) => {
                self.unavailable = true;
                log::info!("dashboard: network counters unavailable, panel disabled: {err}");
                return;
            }
        };
        let now = Instant::now();
        let Some((previous, sampled_at)) = self.last.replace((current, now)) else {
            return;
        };

        let seconds = now.duration_since(sampled_at).as_secs_f64();
        if seconds <= 0.0 {
            return;
        }
        // Counters are unsigned and wrap or reset when an interface goes down,
        // so a decrease is discarded rather than read as negative throughput.
        let (Some(received), Some(sent)) = (
            current.received.checked_sub(previous.received),
            current.sent.checked_sub(previous.sent),
        ) else {
            return;
        };

        let rates = Network {
            received_per_second: (received as f64 / seconds) as u64,
            sent_per_second: (sent as f64 / seconds) as u64,
        };
        publisher.publish(TOPIC_SUMMARY, "network", &rates);

        let sample = NetworkSample {
            timestamp_nanos: system_time_nanos(SystemTime::now()),
            rates,
        };
        publisher.publish_ephemeral(TOPIC_SUMMARY, "network_sample", &sample);

        push_history(&mut self.history, sample, publisher, "network_history");
    }
}

/// Load, memory, filesystem capacity and disk saturation.
#[derive(Default)]
struct HostMeter {
    last: Option<(HostSnapshot, Instant)>,
    /// Set once `/proc` proves unreadable, so the failure is logged once rather
    /// than every second.
    unavailable: bool,
    /// Resolved once at the first sample rather than every second: a mount does
    /// not move, and `statvfs` on a hung filesystem would block the meter.
    paths: Option<Vec<HostPath>>,
}

impl HostMeter {
    /// Publishes nothing when `/proc` cannot be read, so the panel is absent
    /// rather than showing a healthy-looking idle machine.
    fn tick(&mut self, ctx: &DashboardContext, publisher: &Publisher) {
        if self.unavailable {
            return;
        }
        let current = match host_stats::read() {
            Ok(snapshot) => snapshot,
            Err(err) => {
                self.unavailable = true;
                log::info!("dashboard: host counters unavailable, panel disabled: {err}");
                return;
            }
        };
        let now = Instant::now();
        let paths = self
            .paths
            .get_or_insert_with(|| resolve_host_paths(ctx))
            .clone();

        let Some((previous, sampled_at)) = self.last.replace((current.clone(), now)) else {
            return;
        };
        let interval_ms = now.duration_since(sampled_at).as_secs_f64() * 1000.0;
        if interval_ms <= 0.0 {
            return;
        }
        let seconds = interval_ms / 1000.0;

        let swap_used = current
            .memory
            .swap_total
            .saturating_sub(current.memory.swap_free);
        let host = Host {
            cores: num_cpus(),
            load_one: current.load.one,
            load_five: current.load.five,
            load_fifteen: current.load.fifteen,
            threads: current.load.threads,
            running: current.load.running,
            memory_total: current.memory.total,
            memory_available: current.memory.available,
            memory_reclaimable: current.memory.reclaimable,
            memory_free: current.memory.free,
            // Absent rather than a permanent nought where none is configured.
            swap: (current.memory.swap_total > 0).then_some(Swap {
                total: current.memory.swap_total,
                used: swap_used,
            }),
            filesystems: paths
                .iter()
                .filter_map(|path| {
                    let usage = host_stats::filesystem(&path.path).ok()?;
                    Some(FilesystemUsage {
                        name: path.name.clone(),
                        path: path.path.to_string_lossy().into_owned(),
                        total: usage.total,
                        available: usage.available,
                    })
                })
                .collect(),
            devices: device_loads(&paths, &current, &previous, interval_ms, seconds),
        };
        publisher.publish(TOPIC_SUMMARY, "host", &host);
    }
}

/// Where the validator's threads spend each second, grouped by pool.
#[derive(Default)]
struct ThreadMeter {
    last: Option<(HashMap<u64, ThreadReading>, Instant)>,
    /// Each thread's pinning, read once when it is first seen: `Some` where it
    /// may run on fewer cores than the machine has.
    pinning: HashMap<u64, Option<String>>,
    /// Each group's share per tick over the window, for ranking the rows.
    recent: VecDeque<Vec<(String, f64)>>,
    history: Vec<ThreadsSample>,
    /// Set once the task directory proves unreadable, so the failure is logged
    /// once rather than every second.
    unavailable: bool,
}

impl ThreadMeter {
    /// Publishes nothing where the counters cannot be read, so the group is
    /// absent rather than a list of idle threads.
    fn tick(&mut self, publisher: &Publisher) {
        if self.unavailable {
            return;
        }
        let current = match thread_stats::read() {
            Ok(threads) => threads,
            Err(err) => {
                self.unavailable = true;
                log::info!("dashboard: thread counters unavailable, panel disabled: {err}");
                return;
            }
        };
        let cores = num_cpus();
        for thread in &current {
            // Read once: affinity is set at start and does not move.
            self.pinning.entry(thread.tid).or_insert_with(|| {
                thread_stats::cores_allowed(thread.tid)
                    .filter(|list| thread_stats::cores_in(list) < cores)
            });
        }
        let live: HashSet<u64> = current.iter().map(|thread| thread.tid).collect();
        self.pinning.retain(|tid, _| live.contains(tid));

        let now = Instant::now();
        let by_tid: HashMap<u64, ThreadReading> = current
            .iter()
            .cloned()
            .map(|thread| (thread.tid, thread))
            .collect();
        let Some((previous, sampled_at)) = self.last.replace((by_tid, now)) else {
            return;
        };
        let interval = now.duration_since(sampled_at).as_nanos() as u64;
        if interval == 0 {
            return;
        }

        let groups = thread_stats::group_shares(&previous, &current, &self.pinning, interval);
        self.recent.push_back(
            groups
                .iter()
                .map(|group| (group.name.clone(), group.on_cpu))
                .collect(),
        );
        while self.recent.len() > THREADS_HISTORY {
            self.recent.pop_front();
        }
        let means = thread_stats::window_means(&self.recent);
        let sample = ThreadsSample {
            timestamp_nanos: system_time_nanos(SystemTime::now()),
            threads: current.len(),
            groups: thread_stats::select_rows(groups, &means, THREAD_ROWS),
        };
        publisher.publish_ephemeral(TOPIC_SUMMARY, "threads_sample", &sample);
        push_history_of(
            &mut self.history,
            sample,
            THREADS_HISTORY,
            publisher,
            "threads_history",
        );
    }
}

/// Packets lost in the kernel before the validator could read them, per
/// advertised port. These never reached userspace, and are the usual way shreds
/// go missing.
struct SocketMeter {
    /// Trailing history of per-port drop totals, so a startup burst ages out.
    drops_window: PortWindow,
    /// The same drops over the longer window the QUIC counters use, so the TPU
    /// path card compares like spans.
    quic_drops_window: PortWindow,
    /// What that window last worked out, per port name, for the TPU meter to
    /// pick up.
    kernel_drops: HashMap<&'static str, u64>,
    /// Per-port drops when the validator finished starting. Most drops happen
    /// during startup, and carrying that burst for the life of the process said
    /// nothing about now.
    drops_baseline: Option<HashMap<u16, u64>>,
    /// The same window and baseline for what each port delivered, the other term
    /// in the share lost. Kept apart because only three of the seven ports have a
    /// count.
    received_window: PortWindow,
    received_baseline: Option<HashMap<u16, u64>>,
    /// Last counters seen per reported port. `/proc/net/udp` is not read
    /// atomically, so a bound socket can drop out of one snapshot; taking the
    /// snapshot at its word made rows vanish for a tick.
    known_sockets: HashMap<u16, PortCounters>,
    /// Set once `/proc/net/udp` proves unreadable. It fails independently of
    /// the other `/proc` files: a container can expose one and not another.
    unavailable: bool,
    published: Debounced<IngestSummary>,
}

impl SocketMeter {
    fn new() -> Self {
        Self {
            drops_window: PortWindow::new(DROPS_WINDOW),
            quic_drops_window: PortWindow::new(Duration::from_secs(WATERFALL_WINDOW as u64)),
            kernel_drops: HashMap::new(),
            drops_baseline: None,
            received_window: PortWindow::new(DROPS_WINDOW),
            received_baseline: None,
            known_sockets: HashMap::new(),
            unavailable: false,
            published: Debounced::default(),
        }
    }

    fn tick(
        &mut self,
        ctx: &DashboardContext,
        tap: &TapCounters,
        running: bool,
        publisher: &Publisher,
    ) {
        if self.unavailable {
            return;
        }
        let current = match udp_drops::read() {
            Ok(ports) => ports,
            Err(err) => {
                self.unavailable = true;
                // Emptied: this latches, and a reading kept here would show on the TPU path
                // card as current for the rest of the process.
                self.kernel_drops.clear();
                log::info!("dashboard: socket counters unavailable, panel disabled: {err}");
                return;
            }
        };
        let now = Instant::now();
        let ports = ingest_ports(ctx, tap);

        // A port absent from this snapshot keeps the counters it had, which is a
        // sample stale rather than a vanished row. Only the reported ports are
        // remembered.
        for port in &ports {
            if let Some(counters) = current.get(&port.port) {
                self.known_sockets.insert(port.port, *counters);
            }
        }

        let drops: HashMap<u16, u64> = ports
            .iter()
            .filter_map(|port| Some((port.port, self.known_sockets.get(&port.port)?.drops)))
            .collect();
        // Only the ports that have a count, so none acquires a received figure by
        // accident.
        let received: HashMap<u16, u64> = ports
            .iter()
            .filter_map(|port| Some((port.port, port.received?)))
            .collect();

        // Taken the first tick the validator reports itself running, which is where
        // the startup burst ends; before that the raw counters stand so the burst is
        // visible. Both baselines at the same instant, so the two totals can be
        // divided.
        if self.drops_baseline.is_none() && running {
            self.drops_baseline = Some(drops.clone());
            self.received_baseline = Some(received.clone());
        }
        self.drops_window.push(now, drops.clone());
        self.received_window.push(now, received);
        self.quic_drops_window.push(now, drops);
        // Into a local first: the expression reads two other fields of `self`.
        let kernel_drops: HashMap<&'static str, u64> = ports
            .iter()
            .filter(|port| port.quic)
            .filter_map(|port| {
                let counters = self.known_sockets.get(&port.port)?;
                Some((
                    port.name,
                    self.quic_drops_window.since(port.port, counters.drops),
                ))
            })
            .collect();
        self.kernel_drops = kernel_drops;

        let paths: Vec<IngestPath> = ports
            .iter()
            .filter_map(|port| {
                let counters = self.known_sockets.get(&port.port)?;
                let dropped_by = at_baseline(self.drops_baseline.as_ref(), port.port);
                let received_by = at_baseline(self.received_baseline.as_ref(), port.port);
                Some(IngestPath {
                    name: port.name,
                    port: port.port,
                    drops_recent: self.drops_window.since(port.port, counters.drops),
                    // Saturating rather than wrapping: a socket rebound after
                    // the baseline was taken restarts below it.
                    drops_total: counters.drops.saturating_sub(dropped_by),
                    queued_bytes: counters.queued,
                    received_recent: port
                        .received
                        .map(|total| self.received_window.since(port.port, total)),
                    received_total: port.received.map(|total| total.saturating_sub(received_by)),
                    quic: port.quic,
                })
            })
            .collect();

        // Empty means none of the advertised ports is bound here, as behind a port
        // forward. Zeroed rows would report healthy on a failed lookup.
        if paths.is_empty() {
            return;
        }
        let summary = IngestSummary {
            // Capped at the span. Coverage runs a tick over it by design.
            window_seconds: self
                .drops_window
                .covers(now)
                .min(DROPS_WINDOW)
                .as_secs_f64(),
            paths,
        };
        self.published
            .publish(publisher, TOPIC_SUMMARY, "ingest_paths", summary);
    }
}

/// This validator's own UDP ports, in the order the panel lists them, from what
/// the node advertises in gossip: behind a port forward the match finds
/// nothing. Each is paired with the validator's own count of what it delivered,
/// where anything counts it, because this is the one place that knows which
/// socket is which.
fn ingest_ports(ctx: &DashboardContext, tap: &TapCounters) -> Vec<IngestPort> {
    let info = ctx.cluster_info.my_contact_info();
    [
        // Everything arriving on the TVU port is a shred, so the count the
        // shred receiver keeps is the count of what the port delivered.
        (
            "turbine",
            info.tvu(Protocol::UDP),
            Some(tap.shreds_turbine),
            false,
        ),
        // QUIC. The stream layer counts transactions, not datagrams, so no share can
        // be worked out against a datagram drop count; the TPU path panel draws these
        // instead.
        ("tpu", info.tpu(Protocol::QUIC), None, true),
        (
            "tpu forwards",
            info.tpu_forwards(Protocol::QUIC),
            None,
            true,
        ),
        // The UDP vote port, which has a receiver counting datagrams. The QUIC vote
        // port below is a different socket.
        (
            "tpu vote",
            info.tpu_vote(Protocol::UDP),
            Some(tap.packets_tpu_vote),
            false,
        ),
        ("tpu vote quic", info.tpu_vote(Protocol::QUIC), None, true),
        ("gossip", info.gossip(), Some(tap.packets_gossip), false),
        // The one port that could have a count and does not: its receiver keeps
        // counters nothing reports. Reaching them means a change to `core`.
        (
            "serve repair",
            info.serve_repair(Protocol::UDP),
            None,
            false,
        ),
    ]
    .into_iter()
    .filter_map(|(name, addr, received, quic)| {
        Some(IngestPort {
            name,
            port: addr?.port(),
            received,
            quic,
        })
    })
    .collect()
}

/// How much of what arrived had to be asked for.
/// What the two UDP senders that count their bytes are putting out, in bytes
/// per second. `None` until a sender has reported once since startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
struct EgressSplit {
    gossip_per_second: Option<u64>,
    repair_per_second: Option<u64>,
}

/// The gossip and repair share of egress, from the senders' own reports. Each
/// point carries the window it covers, so a rate is bytes over that window and
/// holds its last value between points rather than dropping to nought.
#[derive(Default)]
struct EgressMeter {
    split: EgressSplit,
    published: Debounced<EgressSplit>,
}

impl EgressMeter {
    fn tick(&mut self, previous: &TapCounters, current: &TapCounters, publisher: &Publisher) {
        let rate = |bytes: u64, millis: u64, was: Option<u64>| {
            (millis > 0)
                .then(|| bytes.saturating_mul(1000).checked_div(millis))
                .flatten()
                .or(was)
        };
        self.split = EgressSplit {
            gossip_per_second: rate(
                current
                    .gossip_sent_bytes
                    .saturating_sub(previous.gossip_sent_bytes),
                current
                    .gossip_sent_millis
                    .saturating_sub(previous.gossip_sent_millis),
                self.split.gossip_per_second,
            ),
            repair_per_second: rate(
                current
                    .repair_sent_bytes
                    .saturating_sub(previous.repair_sent_bytes),
                current
                    .repair_sent_millis
                    .saturating_sub(previous.repair_sent_millis),
                self.split.repair_per_second,
            ),
        };
        self.published
            .publish(publisher, TOPIC_SUMMARY, "network_egress", self.split);
    }
}

struct ShredMeter {
    /// `(turbine, repair)` shreds per sample.
    window: VecDeque<(u64, u64)>,
    published: Debounced<Option<Shreds>>,
}

impl ShredMeter {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(SHREDS_WINDOW),
            published: Debounced::default(),
        }
    }

    fn tick(&mut self, previous: &TapCounters, current: &TapCounters, publisher: &Publisher) {
        self.window.push_back((
            current
                .shreds_turbine
                .saturating_sub(previous.shreds_turbine),
            current.shreds_repair.saturating_sub(previous.shreds_repair),
        ));
        while self.window.len() > SHREDS_WINDOW {
            self.window.pop_front();
        }

        let mut turbine = 0u64;
        let mut repaired = 0u64;
        for (sample_turbine, sample_repair) in &self.window {
            turbine = turbine.saturating_add(*sample_turbine);
            repaired = repaired.saturating_add(*sample_repair);
        }

        let received = turbine.saturating_add(repaired);
        // Nothing rather than nought while no shreds have arrived: a validator not
        // receiving, not one receiving perfectly.
        let shreds = (received > 0).then(|| Shreds {
            received,
            repaired,
            repair_rate: repaired as f64 / received as f64,
        });
        self.published
            .publish(publisher, TOPIC_SUMMARY, "shreds", shreds);
    }
}

/// How often an account replay needed was already in memory, and what the
/// accounts database read, wrote and is holding.
struct AccountsMeter {
    /// `(hits, misses, evictions)` of the read cache per sample.
    cache_window: VecDeque<(u64, u64, u64)>,
    /// One interval's worth of the accounts database's own counters per sample.
    window: VecDeque<AccountsTotals>,
    published: Debounced<Option<AccountsCache>>,
}

impl AccountsMeter {
    fn new() -> Self {
        Self {
            cache_window: VecDeque::with_capacity(ACCOUNTS_CACHE_WINDOW),
            window: VecDeque::with_capacity(ACCOUNTS_CACHE_WINDOW),
            published: Debounced::default(),
        }
    }

    fn tick(&mut self, previous: &TapCounters, current: &TapCounters, publisher: &Publisher) {
        self.cache_window.push_back((
            current
                .accounts_cache_hits
                .saturating_sub(previous.accounts_cache_hits),
            current
                .accounts_cache_misses
                .saturating_sub(previous.accounts_cache_misses),
            current
                .accounts_cache_evicts
                .saturating_sub(previous.accounts_cache_evicts),
        ));
        while self.cache_window.len() > ACCOUNTS_CACHE_WINDOW {
            self.cache_window.pop_front();
        }

        let totals = windowed(
            &mut self.window,
            current.accounts.since(&previous.accounts),
            ACCOUNTS_CACHE_WINDOW,
        );
        // What the window actually spans, not what it will span once full. A
        // rate taken against the full minute would read low for the first one.
        let window_seconds = self.cache_window.len() as f64 * METER_INTERVAL.as_secs_f64();

        // Absent until the accounts database has reported its storage once. Nought
        // allocated would read as a validator holding no accounts.
        let disk = (current.accounts_storage_bytes > 0).then(|| AccountsDisk {
            used: current.accounts_storage_alive_bytes,
            allocated: current.accounts_storage_bytes,
            fragmented: current
                .accounts_storage_bytes
                .saturating_sub(current.accounts_storage_alive_bytes),
            storages: current.accounts_storage_count,
        });

        let rate =
            cache_rate(&self.cache_window).map(|(read, hit_rate, evictions)| AccountsCache {
                read,
                hit_rate,
                evictions,
                cache_bytes: current.accounts_cache_bytes,
                cache_entries: current.accounts_cache_entries,
                from_write_cache: totals.loaded_from_write_cache,
                from_read_cache: totals.loaded_from_read_cache,
                from_storage: totals.loaded_from_storage,
                stored_accounts: totals.stored_accounts,
                stored_bytes: totals.stored_bytes,
                window_seconds,
                disk,
            });
        self.published
            .publish(publisher, TOPIC_SUMMARY, "accounts_cache", rate);
    }
}

/// How often replay found a program already compiled. The counters reset per
/// bank, so a reading alone is one slot's handful of lookups; summed over a
/// minute the rate is steady, and nothing is counted twice.
struct ProgramCacheMeter {
    /// One interval's worth of the cache's counters per sample, and beside it
    /// the level readings, which are peaked rather than summed.
    window: VecDeque<ProgramCacheTotals>,
    levels: VecDeque<u64>,
    published: Debounced<Option<ProgramCache>>,
}

impl ProgramCacheMeter {
    fn new() -> Self {
        Self {
            window: VecDeque::with_capacity(PROGRAM_CACHE_WINDOW),
            levels: VecDeque::with_capacity(PROGRAM_CACHE_WINDOW),
            published: Debounced::default(),
        }
    }

    fn tick(&mut self, previous: &TapCounters, current: &TapCounters, publisher: &Publisher) {
        let sample = current.program_cache.since(&previous.program_cache);
        let totals = windowed(&mut self.window, sample, PROGRAM_CACHE_WINDOW);

        // The level is not differenced — it is where the cache stood, not what
        // happened — so it is kept as its own window and read as a peak.
        self.levels.push_back(current.program_cache_water_level);
        while self.levels.len() > PROGRAM_CACHE_WINDOW {
            self.levels.pop_front();
        }
        let peak_entries = self.levels.iter().copied().max().filter(|peak| *peak > 0);

        let looked_up = totals.hits.saturating_add(totals.misses);
        // Nothing rather than nought: a hit rate of zero reads as a cache that is
        // failing rather than one not asked.
        let cache = (looked_up > 0).then(|| ProgramCache {
            looked_up,
            hits: totals.hits,
            misses: totals.misses,
            hit_rate: totals.hits as f64 / looked_up as f64,
            evictions: totals.evictions,
            reloads: totals.reloads,
            insertions: totals.insertions,
            lost_insertions: totals.lost_insertions,
            replacements: totals.replacements,
            one_hit_wonders: totals.one_hit_wonders,
            prunes_orphan: totals.prunes_orphan,
            prunes_environment: totals.prunes_environment,
            peak_entries,
            entry_limit: MAX_LOADED_ENTRY_COUNT as u64,
        });
        self.published
            .publish(publisher, TOPIC_SUMMARY, "program_cache", cache);
    }
}

/// The path a transaction takes through this validator, the two stages that
/// only run while it is leader, and the per-slot lists the block pages join on.
struct TpuMeter {
    /// One interval of the scheduler's counters per sample. Its own window and
    /// key, apart from the QUIC and leader stages, because the stages do not
    /// reconcile into one flow.
    waterfall_window: VecDeque<SchedulerTotals>,
    waterfall: Debounced<Option<WaterfallWindow>>,
    /// Which scheduler the samples in that window came from, so that a
    /// changeover can be noticed rather than summed through.
    waterfall_source: SchedulerSource,
    quic_window: VecDeque<QuicTotals>,
    quic_forwards_window: VecDeque<QuicTotals>,
    quic_vote_window: VecDeque<QuicTotals>,
    quic_paths: Debounced<Option<QuicPaths>>,
    /// How the XDP transmit path is configured, or nothing where the
    /// validator is not running one.
    xdp: Debounced<Option<XdpConfig>>,
    /// Where the chain had got to in its epoch as of the last tick that could take
    /// bank forks. Held because the totals are summed in the metrics pass, which
    /// has no bank; kept across a missed read, which is a missing sample and not a
    /// new epoch.
    epoch_now: Option<EpochPosition>,
    /// The two stages that only run while this validator is leader, gathered
    /// over the epoch rather than over the window the sections above use.
    leader_totals: LeaderTotals,
    epoch_span: Debounced<Option<EpochSpan>>,
    verify: Debounced<Option<VerifyTotals>>,
    executed: Debounced<Option<ExecutedTotals>>,
    bundles: Debounced<Option<BundleTotals>>,
    slot_waterfalls: Debounced<Vec<SlotWaterfall>>,
    slot_costs: Debounced<Vec<SlotCost>>,
    replay: Debounced<Option<ReplayWindow>>,
}

impl TpuMeter {
    fn new() -> Self {
        Self {
            waterfall_window: VecDeque::with_capacity(WATERFALL_WINDOW),
            waterfall: Debounced::default(),
            waterfall_source: SchedulerSource::default(),
            quic_window: VecDeque::with_capacity(WATERFALL_WINDOW),
            quic_forwards_window: VecDeque::with_capacity(WATERFALL_WINDOW),
            quic_vote_window: VecDeque::with_capacity(WATERFALL_WINDOW),
            quic_paths: Debounced::default(),
            xdp: Debounced::default(),
            epoch_now: None,
            leader_totals: LeaderTotals::default(),
            epoch_span: Debounced::default(),
            verify: Debounced::default(),
            executed: Debounced::default(),
            bundles: Debounced::default(),
            slot_waterfalls: Debounced::default(),
            slot_costs: Debounced::default(),
            replay: Debounced::default(),
        }
    }

    /// Remembers where the chain is in its epoch, from the working bank rather
    /// than the root: the counters are reported as the work happens, not as it is
    /// finalised.
    fn note_epoch(&mut self, working_bank: &Bank) {
        let schedule = working_bank.epoch_schedule();
        let slot = working_bank.slot();
        let epoch = schedule.get_epoch(slot);
        self.epoch_now = Some(EpochPosition {
            epoch,
            slot,
            start_slot: schedule.get_first_slot_in_epoch(epoch),
            slots_in_epoch: schedule.get_slots_in_epoch(epoch),
        });
    }

    /// Publishes how the XDP transmit path is set up. Debounced, and the tap
    /// latches it, so the wire carries it once.
    fn collect_xdp(&mut self, tap: &MetricsTap, publisher: &Publisher) {
        self.xdp.publish(publisher, TOPIC_SUMMARY, "xdp", tap.xdp());
    }

    /// Publishes where the transactions handed to the banking stage went. The
    /// scheduler reports once a second with its counters reset, so the window is a
    /// sum of seconds' work, not a queue depth.
    fn tick(
        &mut self,
        tap: &MetricsTap,
        previous: &TapCounters,
        current: &TapCounters,
        kernel_drops: &HashMap<&'static str, u64>,
        tpu_offhost: bool,
        publisher: &Publisher,
    ) {
        // Nothing rather than noughts for a stage with an empty window: its point is
        // only submitted when it had something to say. Started over when the
        // scheduler changes, since the two count `received` in different units.
        let source = tap.scheduler_source();
        if source != self.waterfall_source {
            self.waterfall_window.clear();
            self.waterfall_source = source;
        }
        let scheduler = windowed(
            &mut self.waterfall_window,
            current.scheduler.since(&previous.scheduler),
            WATERFALL_WINDOW,
        );
        self.waterfall.publish(
            publisher,
            TOPIC_SUMMARY,
            "waterfall",
            (scheduler.received > 0).then_some(WaterfallWindow {
                counts: scheduler,
                source,
            }),
        );

        // One row per QUIC port, sent as a list so the panel draws the ports it was
        // given. Present once any port has ever taken a connection, rather than within
        // the window: behind a proxy the only inbound QUIC is vote traffic during
        // leader slots, and a windowed test left the card off the grid for the half
        // hour between. A lifetime at nought is a port nothing has used, which below
        // `solana=info` is every port.
        let ports: Vec<QuicPort> = [
            (
                "tpu",
                &mut self.quic_window,
                current.quic.since(&previous.quic),
                current.quic_levels,
            ),
            (
                "tpu forwards",
                &mut self.quic_forwards_window,
                current.quic_forwards.since(&previous.quic_forwards),
                current.quic_forwards_levels,
            ),
            (
                "tpu vote quic",
                &mut self.quic_vote_window,
                current.quic_vote.since(&previous.quic_vote),
                current.quic_vote_levels,
            ),
        ]
        .into_iter()
        .map(|(name, window, sample, levels)| QuicPort {
            name,
            counts: windowed(window, sample, WATERFALL_WINDOW),
            levels,
            // Absent where the socket read found no such port, which is not a port that
            // dropped nothing.
            kernel_drops: kernel_drops.get(name).copied(),
        })
        .collect();
        // The cumulative figures: `offered` is stored as the listener reports it, so
        // this is the count since the port opened.
        let ever_offered = current.quic.offered > 0
            || current.quic_forwards.offered > 0
            || current.quic_vote.offered > 0;
        // Every port's window is pushed on the same tick and trimmed to the
        // same length, so any one of them says how much time the figures cover.
        let window_seconds = (self.quic_window.len() as f64) * METER_INTERVAL.as_secs_f64();
        self.quic_paths.publish(
            publisher,
            TOPIC_SUMMARY,
            "quic_paths",
            ever_offered.then_some(QuicPaths {
                window_seconds,
                ports,
                tpu_offhost,
            }),
        );

        // The last two stages are counted over the epoch rather than the window. Both
        // only run while this validator is leader, and a five-minute window of a stage
        // that fires for a few slots every few hours is not a rate. Safe over an epoch
        // because every field is a difference the reporter itself resets. Nothing
        // published until a bank has said which epoch, since an unlabelled total is
        // worse than none.
        if let Some(at) = self.epoch_now {
            self.leader_totals.add(
                at,
                current.verify.since(&previous.verify),
                current.executed.since(&previous.executed),
                current.bundles.since(&previous.bundles),
            );
            let LeaderTotals {
                verify,
                executed,
                bundles,
                ..
            } = self.leader_totals;
            let span = self.leader_totals.span(at);

            // One span for both sections: summed and started over on the same tick, so
            // two copies would be two chances to disagree.
            self.epoch_span.publish(
                publisher,
                TOPIC_SUMMARY,
                "epoch_span",
                (verify.received > 0 || executed.attempted > 0 || bundles.received > 0)
                    .then_some(span),
            );
            self.verify.publish(
                publisher,
                TOPIC_SUMMARY,
                "verify",
                (verify.received > 0).then_some(verify),
            );
            self.executed.publish(
                publisher,
                TOPIC_SUMMARY,
                "executed",
                (executed.attempted > 0).then_some(executed),
            );
            // A note on what Executed is made of rather than a stage of its own. Absent
            // where no bundle arrived, which without a block engine or under BAM is
            // always.
            self.bundles.publish(
                publisher,
                TOPIC_SUMMARY,
                "bundles",
                (bundles.received > 0).then_some(bundles),
            );
        }

        // The per-slot points are sent as their own list and joined by slot in the
        // browser, since the produced block is captured on the other thread and either
        // can arrive first. Debounced, so a tick between leader slots sends nothing.
        self.slot_waterfalls.publish(
            publisher,
            TOPIC_SUMMARY,
            "slot_waterfalls",
            tap.slot_waterfalls(),
        );

        // Sent as its own list and joined by slot in the browser, for the same reason
        // as the waterfalls.
        self.slot_costs
            .publish(publisher, TOPIC_SUMMARY, "slot_costs", tap.slot_costs());

        // Averaged over slots held rather than seconds, so the window holds the same
        // number of samples whatever the cluster's pace.
        self.replay.publish(
            publisher,
            TOPIC_SUMMARY,
            "replay",
            replay_window(&tap.replay_slots()),
        );
    }
}

/// Pushes one interval onto a window, forgets what fell out, and sums the
/// rest.
fn windowed<T: WindowedCounters>(window: &mut VecDeque<T>, sample: T, span: usize) -> T {
    window.push_back(sample);
    while window.len() > span {
        window.pop_front();
    }
    window
        .iter()
        .fold(T::default(), |total, sample| total.plus(sample))
}

/// What a port's counter read when the baseline was taken, or nought before
/// there is one, so the first minute reports totals from when each counter
/// started rather than a blank panel over the startup burst.
fn at_baseline(baseline: Option<&HashMap<u16, u64>>, port: u16) -> u64 {
    baseline
        .and_then(|baseline| baseline.get(&port))
        .copied()
        .unwrap_or(0)
}

/// Appends a chart sample and republishes the retained series, which a
/// connecting client needs whole.
fn push_history<T: Serialize>(history: &mut Vec<T>, sample: T, publisher: &Publisher, key: &str) {
    push_history_of(history, sample, CHART_HISTORY, publisher, key);
}

/// The same, keeping `keep` samples.
fn push_history_of<T: Serialize>(
    history: &mut Vec<T>,
    sample: T,
    keep: usize,
    publisher: &Publisher,
    key: &str,
) {
    history.push(sample);
    if history.len() > keep {
        let excess = history.len().saturating_sub(keep);
        history.drain(..excess);
    }
    publisher.retain_only(TOPIC_SUMMARY, key, history);
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::fixture::fixture, solana_metrics::datapoint::DataPoint, std::thread::sleep,
    };

    /// A tap reading carrying only the scheduler, so each test moves one thing.
    fn tap(scheduler: SchedulerTotals) -> TapCounters {
        TapCounters {
            scheduler,
            ..TapCounters::default()
        }
    }

    /// A tap reading where the TPU port has taken `offered` connections since it
    /// opened, the one QUIC counter stored as reported.
    fn quic_tap(offered: u64) -> TapCounters {
        TapCounters {
            quic: QuicTotals {
                offered,
                ..QuicTotals::default()
            },
            ..TapCounters::default()
        }
    }

    /// A window of `(hits, misses, evictions)` samples.
    fn window(samples: &[(u64, u64, u64)]) -> VecDeque<(u64, u64, u64)> {
        samples.iter().copied().collect()
    }

    /// A replayed slot with only the fields a test cares about set.
    fn replayed(set: impl Fn(&mut ReplaySlotTimes)) -> ReplaySlotTimes {
        let mut slot = ReplaySlotTimes::default();
        set(&mut slot);
        slot
    }

    #[test]
    fn test_a_replay_window_reports_the_mean_slot() {
        // What one slot costs, which is the figure that compares against how
        // long a slot lasts. A total over the window would not.
        let window = replay_window(&[
            replayed(|s| {
                s.confirming = 10;
                s.execute = 100;
            }),
            replayed(|s| {
                s.confirming = 30;
                s.execute = 300;
            }),
        ])
        .unwrap();

        assert_eq!(window.slots, 2);
        assert_eq!(window.confirming, 20);
        assert_eq!(window.execute, 200);
    }

    #[test]
    fn test_a_peak_is_the_worst_slot_not_the_worst_of_each_field() {
        // The maxima land on different slots; adding them would describe a slot that
        // never happened.
        let window = replay_window(&[
            replayed(|s| {
                s.fetch = 50;
                s.confirming = 1;
            }),
            replayed(|s| {
                s.fetch = 1;
                s.confirming = 40;
            }),
        ])
        .unwrap();

        assert_eq!(window.serial_peak, 51, "the worse of the two slots");
        assert_ne!(window.serial_peak, 90, "not the two maxima added");
    }

    #[test]
    fn test_the_program_cache_carries_its_own_peak() {
        // Compilation arrives in bursts, so the mean alone hides the thing worth
        // seeing.
        let mut slots = vec![replayed(|s| s.program_cache = 1_000); 9];
        slots.push(replayed(|s| s.program_cache = 45_000));
        let window = replay_window(&slots).unwrap();

        assert_eq!(window.program_cache, 5_400);
        assert_eq!(window.program_cache_peak, 45_000);
    }

    #[test]
    fn test_no_replayed_slots_is_no_panel() {
        // Absent rather than a card of noughts, which a quiet log filter would
        // otherwise show for ever.
        assert!(replay_window(&[]).is_none());
    }

    #[test]
    fn test_the_rate_is_taken_over_the_whole_window() {
        // Taken one at a time the rate is 100%, 50%, 0; over the window it is the six
        // in nine it was.
        let (asked, rate, _) = cache_rate(&window(&[(4, 0, 0), (1, 1, 0), (1, 2, 0)])).unwrap();
        assert_eq!(asked, 9);
        assert!((rate - 6.0 / 9.0).abs() < f64::EPSILON, "{rate}");
    }

    #[test]
    fn test_evictions_are_summed_alongside() {
        // The usual reason a hit rate falls, so it is reported beside it rather
        // than left to be inferred from the rate dropping.
        let (_, _, evictions) = cache_rate(&window(&[(10, 1, 2), (10, 1, 3)])).unwrap();
        assert_eq!(evictions, 5);
    }

    #[test]
    fn test_nothing_asked_reports_nothing() {
        // Not zero: a cache that has not been asked is not a cache that is failing.
        assert!(cache_rate(&window(&[])).is_none());
        assert!(cache_rate(&window(&[(0, 0, 0), (0, 0, 4)])).is_none());
    }

    #[test]
    fn test_only_the_ports_counted_in_datagrams_carry_a_received_figure() {
        // The kernel keys drops by port and the validator keys packets by thread
        // name, and only this join knows `shred_fetch_receiver` is the socket gossip
        // advertises as `tvu`.
        let harness = fixture();
        let counted = ingest_ports(
            &harness.ctx,
            &TapCounters {
                shreds_turbine: 900,
                packets_gossip: 40,
                packets_tpu_vote: 70,
                ..TapCounters::default()
            },
        );
        let by_name: HashMap<&str, Option<u64>> = counted
            .iter()
            .map(|port| (port.name, port.received))
            .collect();

        assert_eq!(by_name["turbine"], Some(900));
        assert_eq!(by_name["gossip"], Some(40));
        assert_eq!(by_name["tpu vote"], Some(70));

        // Nothing rather than nought: no packets received alongside any drops works
        // out to every packet lost.
        assert_eq!(
            by_name["tpu"], None,
            "QUIC counts transactions, not packets"
        );
        assert_eq!(by_name["tpu forwards"], None);
        assert_eq!(
            by_name["serve repair"], None,
            "its receiver keeps counters that nothing reports"
        );
    }

    #[test]
    fn test_the_quic_ports_are_flagged_and_the_udp_vote_port_is_not() {
        // Two panels split this list on the flag. The vote ports are the pair worth
        // pinning: `tpu_vote` is UDP, the QUIC vote endpoint another socket.
        let harness = fixture();
        let ports = ingest_ports(&harness.ctx, &TapCounters::default());
        let quic: HashMap<&str, bool> = ports.iter().map(|port| (port.name, port.quic)).collect();

        assert_eq!(quic.get("tpu"), Some(&true));
        assert_eq!(quic.get("tpu forwards"), Some(&true));
        assert_eq!(quic.get("tpu vote"), Some(&false));
        assert_eq!(quic.get("turbine"), Some(&false));
        assert_eq!(quic.get("gossip"), Some(&false));
        assert_eq!(quic.get("serve repair"), Some(&false));
        // Present only where the node advertises one, which is why this is
        // checked for absence rather than asserted into existence.
        assert_ne!(quic.get("tpu vote quic"), Some(&false));
    }

    #[test]
    fn test_a_port_with_no_baseline_yet_counts_from_its_own_start() {
        // What the panel shows over the startup burst, before the baselines are
        // taken.
        assert_eq!(at_baseline(None, 8001), 0);
        assert_eq!(at_baseline(Some(&HashMap::new()), 8001), 0);
        assert_eq!(at_baseline(Some(&HashMap::from([(8001, 42)])), 8001), 42);
    }

    #[test]
    fn test_the_waterfall_reports_the_window_and_not_the_running_total() {
        // The tap's counters only climb; published as they stand they would present
        // every transaction since startup as the last five minutes.
        let harness = fixture();
        let mut meters = harness.meters();

        // A counter already well into its life, as it is by the time anyone
        // opens the page.
        let start = SchedulerTotals {
            received: 1_000,
            buffered: 40,
            ..SchedulerTotals::default()
        };
        let next = SchedulerTotals {
            received: 1_100,
            buffered: 50,
            ..SchedulerTotals::default()
        };
        let last = SchedulerTotals {
            received: 1_250,
            buffered: 65,
            ..SchedulerTotals::default()
        };
        meters.collect_waterfall(&tap(start), &tap(next));
        meters.collect_waterfall(&tap(next), &tap(last));

        let published = harness.published_key("summary", "waterfall").unwrap();
        // A hundred then a hundred and fifty, against a counter reading 1,250.
        assert!(published.contains(r#""received":250"#), "{published}");
        assert!(published.contains(r#""buffered":25"#), "{published}");
    }

    #[test]
    fn test_a_scheduler_with_no_traffic_reports_nothing_rather_than_noughts() {
        // An empty window is a validator nothing was sent, not one throwing
        // everything away.
        let harness = fixture();
        let mut meters = harness.meters();
        meters.collect_waterfall(&TapCounters::default(), &TapCounters::default());

        let published = harness.published_key("summary", "waterfall").unwrap();
        assert!(published.contains(r#""value":null"#), "{published}");
    }

    #[test]
    fn test_a_changeover_restarts_the_waterfall_window() {
        // The two schedulers count `received` in different units, so a window
        // spanning a handover would add two units.
        let harness = fixture();
        let mut meters = harness.meters();

        // Three samples under the scheduler the fixture's tap reports.
        let mut previous = SchedulerTotals::default();
        for step in 1..=3u64 {
            let current = SchedulerTotals {
                received: step.saturating_mul(10),
                ..SchedulerTotals::default()
            };
            meters.collect_waterfall(&tap(previous), &tap(current));
            previous = current;
        }
        assert_eq!(meters.tpu.waterfall_window.len(), 3);

        // Now say those three were BAM's. The tap still reports the validator's own
        // scheduler, so the next tick is a handover.
        meters.tpu.waterfall_source = SchedulerSource::Bam;
        let current = SchedulerTotals {
            received: 145,
            ..SchedulerTotals::default()
        };
        meters.collect_waterfall(&tap(previous), &tap(current));

        assert_eq!(meters.tpu.waterfall_window.len(), 1);
        assert_eq!(meters.tpu.waterfall_source, SchedulerSource::Scheduler);
        let published = harness.published_key("summary", "waterfall").unwrap();
        assert!(published.contains(r#""source":"scheduler""#), "{published}");
        // 145 against a previous reading of 30: the one sample, not the four.
        assert!(published.contains(r#""received":115"#), "{published}");
    }

    /// Where the chain is, a given number of slots into a 432,000-slot epoch.
    fn at(epoch: Epoch, slots_in: u64) -> EpochPosition {
        let start_slot = epoch.saturating_mul(432_000);
        EpochPosition {
            epoch,
            slot: start_slot.saturating_add(slots_in),
            start_slot,
            slots_in_epoch: 432_000,
        }
    }

    fn verified(received: u64) -> VerifyTotals {
        VerifyTotals {
            received,
            verified: received,
            ..VerifyTotals::default()
        }
    }

    fn attempted(attempted: u64) -> ExecutedTotals {
        ExecutedTotals {
            attempted,
            succeeded: attempted,
            ..ExecutedTotals::default()
        }
    }

    fn bundled(received: u64, packets: u64) -> BundleTotals {
        BundleTotals { received, packets }
    }

    #[test]
    fn test_the_bundles_are_summed_over_the_epoch_beside_the_stage_they_annotate() {
        // Printed on Executed's heading, so they have to cover what Executed covers.
        let mut totals = LeaderTotals::default();
        totals.add(at(842, 10), verified(100), attempted(40), bundled(6, 21));
        totals.add(at(842, 20), verified(0), attempted(0), bundled(0, 0));
        totals.add(at(842, 30), verified(70), attempted(25), bundled(4, 9));

        assert_eq!(totals.bundles, bundled(10, 30));
        assert_eq!(totals.executed.attempted, 65);
    }

    #[test]
    fn test_the_bundles_start_over_with_the_stage_they_are_printed_against() {
        // Reset on the same tick as the stages they annotate.
        let mut totals = LeaderTotals::default();
        totals.add(
            at(842, 400_000),
            verified(900),
            attempted(300),
            bundled(80, 240),
        );
        totals.add(at(843, 3), verified(11), attempted(4), bundled(1, 2));

        assert_eq!(totals.bundles, bundled(1, 2));
        assert_eq!(totals.epoch, Some(843));
    }

    #[test]
    fn test_the_leader_totals_add_every_sample_of_the_epoch_rather_than_a_window() {
        // A stage that fires for a few slots every few hours has nothing to say about
        // the last five minutes.
        let mut totals = LeaderTotals::default();
        totals.add(
            at(842, 10),
            verified(100),
            attempted(40),
            BundleTotals::default(),
        );
        totals.add(
            at(842, 20),
            verified(0),
            attempted(0),
            BundleTotals::default(),
        );
        totals.add(
            at(842, 30),
            verified(70),
            attempted(25),
            BundleTotals::default(),
        );

        assert_eq!(totals.verify.received, 170);
        assert_eq!(totals.executed.attempted, 65);
        // The quiet sample in the middle neither cleared anything nor aged the
        // first one out, which is what a window would have done to it.
        assert_eq!(totals.executed.succeeded, 65);
    }

    #[test]
    fn test_the_leader_totals_start_over_when_the_epoch_turns() {
        // The leader schedule and the stake behind it are drawn per epoch, so a total
        // spanning two is a total of two schedules.
        let mut totals = LeaderTotals::default();
        totals.add(
            at(842, 400_000),
            verified(900),
            attempted(300),
            BundleTotals::default(),
        );
        totals.add(
            at(843, 3),
            verified(11),
            attempted(4),
            BundleTotals::default(),
        );

        assert_eq!(totals.epoch, Some(843));
        assert_eq!(totals.verify.received, 11);
        assert_eq!(totals.executed.attempted, 4);
        // And the span starts again with it, rather than reporting the new
        // epoch as covered from wherever the last one began.
        assert_eq!(totals.span(at(843, 3)).counted_slots, 1);
    }

    #[test]
    fn test_the_span_says_how_much_of_the_epoch_was_actually_counted() {
        // A validator restarted part way through an epoch has totals honest about a
        // shorter span than the heading.
        let mut totals = LeaderTotals::default();
        totals.add(
            at(842, 300_000),
            verified(5),
            attempted(2),
            BundleTotals::default(),
        );
        let span = totals.span(at(842, 320_000));

        assert_eq!(span.elapsed_slots, 320_001);
        assert_eq!(span.counted_slots, 20_001);
        assert_eq!(span.slots_in_epoch, 432_000);
    }

    #[test]
    fn test_the_counted_span_never_runs_past_the_epoch_it_is_counted_against() {
        // A bank read can land either side of the one that turned the epoch over.
        let totals = LeaderTotals {
            epoch: Some(842),
            from_slot: 842u64.saturating_mul(432_000).saturating_sub(50),
            ..LeaderTotals::default()
        };
        let span = totals.span(at(842, 10));

        assert_eq!(span.counted_slots, 11);
        assert!(span.counted_slots <= span.elapsed_slots);
    }

    #[test]
    fn test_the_two_leader_stages_are_published_against_the_epoch_they_ran_in() {
        let harness = fixture();
        let mut meters = harness.meters();
        meters.tpu.epoch_now = Some(at(842, 216_000));

        let previous = TapCounters::default();
        let current = TapCounters {
            verify: verified(4_000),
            executed: attempted(1_500),
            ..TapCounters::default()
        };
        meters.collect_waterfall(&previous, &current);

        let span = harness.published_key("summary", "epoch_span").unwrap();
        assert!(span.contains(r#""epoch":842"#), "{span}");
        assert!(span.contains(r#""slots_in_epoch":432000"#), "{span}");
        let verify = harness.published_key("summary", "verify").unwrap();
        assert!(verify.contains(r#""received":4000"#), "{verify}");
        let executed = harness.published_key("summary", "executed").unwrap();
        assert!(executed.contains(r#""attempted":1500"#), "{executed}");
    }

    #[test]
    fn test_nothing_is_published_for_a_stage_until_a_bank_has_said_which_epoch() {
        // A total with no epoch against it cannot be labelled, and one drawn
        // under a heading it was not counted for is worse than none at all.
        let harness = fixture();
        let mut meters = harness.meters();
        assert!(meters.tpu.epoch_now.is_none());

        let current = TapCounters {
            verify: verified(900),
            ..TapCounters::default()
        };
        meters.collect_waterfall(&TapCounters::default(), &current);

        assert!(harness.published_key("summary", "verify").is_none());
        assert!(harness.published_key("summary", "epoch_span").is_none());
    }

    #[test]
    fn test_the_epoch_position_is_read_from_the_bank_the_validator_is_building_on() {
        // The working bank, not the root: the counters are reported as the work
        // happens.
        let harness = fixture();
        let mut meters = harness.meters();
        let bank = harness.advance_to(64);
        meters.note_epoch(&bank);

        let position = meters.tpu.epoch_now.unwrap();
        assert_eq!(position.slot, 64);
        assert_eq!(position.epoch, bank.epoch_schedule().get_epoch(64));
        assert!(position.start_slot <= 64);
    }

    #[test]
    fn test_no_xdp_is_published_where_the_validator_reported_none() {
        // The key is sent as null so the panel can tell a validator that
        // reported no config apart from one whose config has not arrived yet.
        let harness = fixture();
        let mut meters = harness.meters();
        meters.collect_xdp();

        let published = harness.published_key("summary", "xdp").unwrap();
        assert!(published.contains(r#""value":null"#), "{published}");
    }

    #[test]
    fn test_a_reported_xdp_config_reaches_the_wire_whole() {
        // Tags and fields alike, which is what makes this point different from
        // every other one the tap reads.
        let harness = fixture();
        let mut meters = harness.meters();
        let mut point = DataPoint::new("xdp-network-config");
        point.add_tag("driver", "ice");
        point.add_tag("zero_copy", "true");
        point.add_field_str("model", "Ethernet Controller E810-C for QSFP");
        meters.metrics_tap.observe_point(&point);
        meters.collect_xdp();

        let published = harness.published_key("summary", "xdp").unwrap();
        assert!(published.contains(r#""zero_copy":true"#), "{published}");
        assert!(published.contains(r#""driver":"ice""#), "{published}");
        // Unwrapped on the way through: the point stores a string field with
        // the line protocol's quotes still round it.
        assert!(
            published.contains(r#""model":"Ethernet Controller E810-C for QSFP""#),
            "{published}"
        );
    }

    #[test]
    fn test_the_path_card_stays_once_a_port_has_ever_been_used() {
        // The same reading twice, so every windowed figure is nought while the
        // cumulative offer stands: the quiet half hour between leader groups behind a
        // proxy.
        let harness = fixture();
        let mut meters = harness.meters();
        let used = quic_tap(40);
        meters.collect_waterfall(&used, &used);

        let published = harness.published_key("summary", "quic_paths").unwrap();
        assert!(!published.contains(r#""value":null"#), "{published}");
        assert!(published.contains(r#""offered":0"#), "{published}");
    }

    #[test]
    fn test_no_path_card_where_no_port_has_ever_been_offered_anything() {
        // Also what a validator logging below `solana=info` looks like: the tap sees
        // nothing.
        let harness = fixture();
        let mut meters = harness.meters();
        meters.collect_waterfall(&TapCounters::default(), &TapCounters::default());

        let published = harness.published_key("summary", "quic_paths").unwrap();
        assert!(published.contains(r#""value":null"#), "{published}");
    }

    #[test]
    fn test_an_advertised_tpu_bound_elsewhere_is_said_to_be() {
        // A relayer or proxy overwrites the advertised address, so the socket join
        // finds no port here and the listener reports next to nothing.
        let harness = fixture();
        let mut meters = harness.meters();
        let used = quic_tap(1);
        meters.collect_waterfall(&used, &used);
        let published = harness.published_key("summary", "quic_paths").unwrap();
        assert!(published.contains(r#""tpu_offhost":true"#), "{published}");

        // Found among this host's own sockets, and the claim is dropped.
        meters.sockets.kernel_drops.insert("tpu", 0);
        meters.collect_waterfall(&used, &used);
        let published = harness.published_key("summary", "quic_paths").unwrap();
        assert!(published.contains(r#""tpu_offhost":false"#), "{published}");
    }

    #[test]
    fn test_a_host_whose_sockets_cannot_be_read_is_not_told_its_tpu_moved() {
        // Once the socket table is unreadable every port looks absent, and the honest
        // answer is that we cannot tell.
        let harness = fixture();
        let mut meters = harness.meters();
        meters.sockets.unavailable = true;
        let used = quic_tap(1);
        meters.collect_waterfall(&used, &used);

        let published = harness.published_key("summary", "quic_paths").unwrap();
        assert!(published.contains(r#""tpu_offhost":false"#), "{published}");
    }

    #[test]
    fn test_the_waterfall_window_forgets_what_falls_out_of_it() {
        // Otherwise the busiest five minutes the validator ever had would stay
        // on the page for the life of the process.
        let harness = fixture();
        let mut meters = harness.meters();

        let mut previous = SchedulerTotals::default();
        for step in 1..=WATERFALL_WINDOW.saturating_add(10) {
            let current = SchedulerTotals {
                received: (step as u64).saturating_mul(10),
                ..SchedulerTotals::default()
            };
            meters.collect_waterfall(&tap(previous), &tap(current));
            previous = current;
        }

        assert_eq!(meters.tpu.waterfall_window.len(), WATERFALL_WINDOW);
        let published = harness.published_key("summary", "waterfall").unwrap();
        // Ten a sample across the window, not across every sample ever taken.
        let expected = (WATERFALL_WINDOW as u64).saturating_mul(10);
        assert!(
            published.contains(&format!(r#""received":{expected}"#)),
            "{published}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_the_threads_are_published_from_the_second_tick() {
        // The first reading is the baseline; the panel wants a difference.
        let harness = fixture();
        let mut meters = harness.meters();
        meters.tick();
        assert!(
            harness
                .published_key("summary", "threads_history")
                .is_none()
        );
        sleep(Duration::from_millis(20));
        meters.tick();
        let published = harness.published_key("summary", "threads_history").unwrap();
        assert!(published.contains(r#""groups":["#), "{published}");
    }

    #[test]
    fn test_a_tick_always_publishes_the_clock() {
        // The heartbeat. Everything else can report nothing; the clock says the feed
        // is alive.
        let harness = fixture();
        let mut meters = harness.meters();
        meters.tick();

        assert!(
            harness
                .published_key("summary", "server_time_nanos")
                .is_some()
        );
        assert!(harness.published_key("summary", "uptime_nanos").is_some());
    }

    #[test]
    fn test_a_busy_bank_forks_costs_a_sample_and_not_the_heartbeat() {
        // The reason this thread takes bank forks with `try_read`: waiting would stop
        // the clock, which looks like a dead feed.
        let harness = fixture();
        let mut meters = harness.meters();
        let held = harness.bank_forks.write().unwrap();

        meters.tick();
        drop(held);

        assert!(
            harness
                .published_key("summary", "server_time_nanos")
                .is_some(),
            "the heartbeat stopped while replay held the lock"
        );
        assert!(
            harness.published_key("summary", "estimated_tps").is_none(),
            "the sample should have been skipped, not waited for"
        );
    }

    #[test]
    fn test_throughput_needs_two_samples_with_a_slot_between_them() {
        // Rates are differences, so the first tick can only establish a baseline.
        let harness = fixture();
        let mut meters = harness.meters();

        meters.tick();
        assert!(
            harness.published_key("summary", "estimated_tps").is_none(),
            "a rate was reported from one reading"
        );

        // Slow enough that one slot does not look like a catch-up burst. Sleeping
        // longer only lowers the rate, so a loaded machine cannot flake this.
        sleep(Duration::from_millis(250));
        harness.advance_to(1);
        meters.tick();

        assert!(
            harness.published_key("summary", "estimated_tps").is_some(),
            "two readings a slot apart should have produced a rate"
        );
        assert!(
            harness.published_key("summary", "tps_history").is_some(),
            "the chart series is retained for a client connecting later"
        );
    }

    #[test]
    fn test_failures_are_summed_as_banks_freeze() {
        // The error counter resets per bank. Differenced like the total, it read
        // nought whenever a bank froze with fewer failures than the one before.
        let harness = fixture();
        let mut meters = harness.meters();

        meters.tick();
        sleep(Duration::from_millis(250));
        harness.advance_with_failures(1, 3);
        meters.tick();
        sleep(Duration::from_millis(250));
        harness.advance_with_failures(2, 1);
        meters.tick();

        let message = harness.published_key("summary", "estimated_tps").unwrap();
        let tps: serde_json::Value = serde_json::from_str(&message).unwrap();
        let failed = tps["value"]["non_vote_failed"].as_f64().unwrap();
        let non_vote = failed + tps["value"]["non_vote_success"].as_f64().unwrap();
        assert!(
            failed > 0.0,
            "one failure in the last slot was reported as none"
        );
        assert!(
            (failed - non_vote).abs() < 1e-9,
            "every non-vote transaction failed, so the two rates should agree: {message}"
        );
    }

    #[test]
    fn test_a_replayed_burst_is_not_reported_as_throughput() {
        // One catch-up sample pins the chart's scale for as long as it stays in view.
        let harness = fixture();
        let mut meters = harness.meters();

        meters.tick();
        // No sleep: two ticks in immediate succession put the rate orders of
        // magnitude above the guard.
        harness.advance_to(1);
        meters.tick();

        assert!(
            harness.published_key("summary", "estimated_tps").is_none(),
            "replay throughput was reported as cluster throughput"
        );
    }
}
