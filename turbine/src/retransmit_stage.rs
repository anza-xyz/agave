//! The `retransmit_stage` retransmits shreds between validators

use {
    crate::{
        XdpSender,
        cluster_nodes::{
            ClusterNodes, ClusterNodesCache, DATA_PLANE_FANOUT, Error, MAX_NUM_TURBINE_HOPS,
        },
    },
    agave_votor::event::VotorEvent,
    agave_votor_messages::migration::MigrationStatus,
    crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender, TrySendError},
    lru::LruCache,
    rand::Rng,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::{
        leader_schedule_cache::LeaderScheduleCache,
        shred::{self, ShredFlags, ShredId, ShredType},
    },
    solana_measure::measure::Measure,
    solana_net_utils::SocketAddrSpace,
    solana_perf::deduper::Deduper,
    solana_pubkey::Pubkey,
    solana_rpc::{
        max_slots::MaxSlots, rpc_subscriptions::RpcSubscriptions,
        slot_status_notifier::SlotStatusNotifier,
    },
    solana_rpc_client_api::response::SlotUpdate,
    solana_runtime::{
        bank::{Bank, MAX_LEADER_SCHEDULE_STAKES},
        bank_forks::BankForks,
    },
    solana_streamer::sendmmsg::{SendPktsError, multi_target_send},
    solana_time_utils::timestamp,
    std::{
        collections::{HashMap, HashSet},
        net::{SocketAddr, UdpSocket},
        ops::AddAssign,
        sync::{Arc, RwLock, atomic::Ordering},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

mod worker_pool;
use worker_pool::WorkerPool as RetransmitWorkerPool;
pub use worker_pool::{RetransmitSender, ShredBatch};

const MAX_DUPLICATE_COUNT: usize = 2;
const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
const DEDUPER_NUM_BITS: u64 = 637_534_199; // 76MB
const DEDUPER_RESET_CYCLE: Duration = Duration::from_secs(5 * 60);

/// Sets the upper bound on the number of batches stored in the retransmit
/// stage ingress channel.
/// Allows for a max of 16k batches of up to 64 packets each
/// (PACKETS_PER_BATCH).
/// This translates to about 1 GB of RAM for packet storage in the worst case.
/// In reality this means about 200K shreds since most batches are not full.
const CHANNEL_SIZE_RETRANSMIT_INGRESS: usize = 16 * 1024;

const _: () = const {
    // From https://github.com/anza-xyz/agave/pull/1735#discussion_r1644899183:
    // 1. There must be at least two epochs because near an epoch boundary you might receive
    //    shreds from the other side of the epoch boundary.
    // 2. It does not make sense to have capacity more than the number of epoch-stakes in Bank.
    assert!(CLUSTER_NODES_CACHE_NUM_EPOCH_CAP >= 2);
    assert!(CLUSTER_NODES_CACHE_NUM_EPOCH_CAP <= MAX_LEADER_SCHEDULE_STAKES as usize);
};
const CLUSTER_NODES_CACHE_NUM_EPOCH_CAP: usize = MAX_LEADER_SCHEDULE_STAKES as usize;
const CLUSTER_NODES_CACHE_TTL: Duration = Duration::from_secs(5);

// Output of fn retransmit_shred(...).
struct RetransmitShredOutput {
    shred: ShredId,
    // If the shred has ShredFlags::LAST_SHRED_IN_SLOT.
    last_shred_in_slot: bool,
    // This node's distance from the turbine root.
    root_distance: u8,
    // Number of nodes the shred was retransmitted to.
    num_nodes: usize,
}

#[derive(Default)]
pub(crate) struct RetransmitSlotStats {
    asof: u64,   // Latest timestamp struct was updated.
    outset: u64, // 1st shred retransmit timestamp.
    // Maximum code and data indices observed.
    pub(crate) max_index_code: u32,
    pub(crate) max_index_data: u32,
    // If any of the shreds had ShredFlags::LAST_SHRED_IN_SLOT.
    pub(crate) last_shred_in_slot: bool,
    // Number of shreds sent and received at different
    // distances from the turbine broadcast root.
    pub(crate) num_shreds_received: [usize; MAX_NUM_TURBINE_HOPS],
    num_shreds_sent: [usize; MAX_NUM_TURBINE_HOPS],
}

struct RetransmitStats {
    since: Instant,
    num_iters: usize,
    num_nodes: usize,
    num_addrs_failed: usize,
    num_shreds_dropped_xdp_full: usize,
    num_loopback_errs: usize,
    num_shreds: usize,
    num_shreds_skipped: usize,
    num_small_batches: usize,
    total_batches: usize,
    total_time: u64,
    epoch_fetch: u64,
    epoch_cache_update: u64,
    retransmit_total: u64,
    compute_turbine_peers_total: u64,
    slot_stats: LruCache<Slot, RetransmitSlotStats>,
    unknown_shred_slot_leader: usize,
}

struct RetransmitNotifiers {
    rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
    slot_status_notifier: Option<SlotStatusNotifier>,
    migration_status: Arc<MigrationStatus>,
    votor_event_sender: CrossbeamSender<VotorEvent>,
}

struct WorkerContext {
    bank_forks: Arc<RwLock<BankForks>>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    cluster_info: Arc<ClusterInfo>,
    retransmit_sockets: Arc<Vec<UdpSocket>>,
    xdp_sender: Option<XdpSender>,
    cluster_nodes_cache: Arc<ClusterNodesCache<RetransmitStage>>,
    shred_deduper: Arc<ShredDeduper>,
    max_slots: Arc<MaxSlots>,
    stats_sender: CrossbeamSender<BatchStats>,
}

impl RetransmitStats {
    fn maybe_submit(
        &mut self,
        bank_forks: &RwLock<BankForks>,
        cluster_info: &ClusterInfo,
        cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
        is_xdp: bool,
    ) {
        const SUBMIT_CADENCE: Duration = Duration::from_secs(2);
        if self.since.elapsed() < SUBMIT_CADENCE {
            return;
        }
        let (working_bank, root_bank) = {
            let bank_forks = bank_forks.read().unwrap();
            (bank_forks.working_bank(), bank_forks.root_bank())
        };
        cluster_nodes_cache
            .get(root_bank.slot(), &root_bank, &working_bank, cluster_info)
            .submit_metrics("cluster_nodes_retransmit", timestamp());
        datapoint_info!(
            "retransmit-stage",
            "is_xdp" => is_xdp.to_string(),
            ("total_time", self.total_time, i64),
            ("epoch_fetch", self.epoch_fetch, i64),
            ("epoch_cache_update", self.epoch_cache_update, i64),
            ("num_iters", self.num_iters, i64),
            ("total_batches", self.total_batches, i64),
            ("num_small_batches", self.num_small_batches, i64),
            ("num_nodes", self.num_nodes, i64),
            ("num_addrs_failed", self.num_addrs_failed, i64),
            (
                "num_shreds_dropped_xdp_full",
                self.num_shreds_dropped_xdp_full,
                i64
            ),
            ("num_loopback_errs", self.num_loopback_errs, i64),
            ("num_shreds", self.num_shreds, i64),
            ("num_shreds_skipped", self.num_shreds_skipped, i64),
            ("retransmit_total", self.retransmit_total, i64),
            (
                "compute_turbine",
                self.compute_turbine_peers_total,
                i64
            ),
            (
                "unknown_shred_slot_leader",
                self.unknown_shred_slot_leader,
                i64
            ),
        );
        // slot_stats are submitted at a different cadence.
        let old = std::mem::replace(self, Self::new(Instant::now()));
        self.slot_stats = old.slot_stats;
    }
}

struct ShredDeduper<const K: usize = 2> {
    deduper: Deduper<K, /*shred:*/ [u8]>,
    shred_id_filter: Deduper<K, (ShredId, /*0..MAX_DUPLICATE_COUNT:*/ usize)>,
}

impl<const K: usize> ShredDeduper<K> {
    fn new<R: Rng>(rng: &mut R, num_bits: u64) -> Self {
        Self {
            deduper: Deduper::new(rng, num_bits),
            shred_id_filter: Deduper::new(rng, num_bits),
        }
    }

    fn maybe_reset<R: Rng>(&self, rng: &mut R, false_positive_rate: f64, reset_cycle: Duration) {
        self.deduper
            .maybe_reset(rng, false_positive_rate, reset_cycle);
        self.shred_id_filter
            .maybe_reset(rng, false_positive_rate, reset_cycle);
    }

    // Returns true if the shred is duplicate and should be discarded.
    #[must_use]
    fn dedup(&self, key: ShredId, shred: &[u8], max_duplicate_count: usize) -> bool {
        // Shreds in the retransmit stage:
        //   * don't have repair nonce (repaired shreds are not retransmitted).
        //   * are already resigned by this node as the retransmitter.
        //   * have their leader's signature verified.
        // Therefore in order to dedup shreds, it suffices to compare:
        //    (signature, slot, shred-index, shred-type)
        // Because ShredCommonHeader already includes all of the above tuple,
        // the rest of the payload can be skipped.
        // In order to detect duplicate blocks across cluster, we retransmit
        // max_duplicate_count different shreds for each ShredId.
        shred::layout::get_common_header_bytes(shred)
            .map(|header| self.deduper.dedup(header))
            .unwrap_or(true)
            || (0..max_duplicate_count).all(|i| self.shred_id_filter.dedup(&(key, i)))
    }
}

type RetransmitAddrCache = HashMap<Slot, (Pubkey, Arc<ClusterNodes<RetransmitStage>>)>;

#[derive(Default)]
struct BatchStats {
    root: Slot,
    slot_stats: Vec<(Slot, RetransmitSlotStats)>,
    num_shreds: usize,
    num_small_batches: usize,
    total_batches: usize,
    total_time: u64,
    epoch_fetch: u64,
    unknown_shred_slot_leader: usize,
    num_nodes: usize,
    num_addrs_failed: usize,
    num_shreds_dropped_xdp_full: usize,
    num_loopback_errs: usize,
    num_shreds_skipped: usize,
    retransmit_total: u64,
    compute_turbine_peers_total: u64,
}

impl BatchStats {
    fn record_slot_stats(&mut self, shred_stats: RetransmitShredOutput) {
        let now = timestamp();
        let slot = shred_stats.shred.slot();

        if let Some((_, stats)) = self.slot_stats.iter_mut().find(|(s, _)| *s == slot) {
            stats.record(now, shred_stats);
        } else {
            let mut stats = RetransmitSlotStats::default();
            stats.record(now, shred_stats);
            self.slot_stats.push((slot, stats));
        }
    }
}

fn retransmit_batch(mut shreds: ShredBatch, worker_index: usize, context: &WorkerContext) {
    let mut total_timer = Measure::start("retransmit");

    let mut stats = BatchStats {
        num_shreds: shreds.len(),
        num_small_batches: usize::from(shreds.len() < 2),
        total_batches: 1,
        ..BatchStats::default()
    };

    let mut epoch_fetch = Measure::start("retransmit_epoch_fetch");
    let (working_bank, root_bank) = {
        let bank_forks = context.bank_forks.read().unwrap();
        (bank_forks.working_bank(), bank_forks.root_bank())
    };
    epoch_fetch.stop();
    stats.epoch_fetch = epoch_fetch.as_us();
    stats.root = root_bank.slot();

    //TODO(klykov): the size of batch is typically 64 shreds, does it make any sense to make all
    //this machinery with HashSet/Table? Use something simpler?
    // Resolve the leader and retransmit tree once for each distinct slot in this capped batch.
    let cache: RetransmitAddrCache = shreds
        .iter()
        .filter_map(|shred| shred::layout::get_slot(shred))
        .collect::<HashSet<Slot>>()
        .into_iter()
        .filter_map(|slot| {
            context
                .max_slots
                .retransmit
                .fetch_max(slot, Ordering::Relaxed);
            // Shreds have already passed signature verification, so the leader should be known.
            let Some(slot_leader) = context
                .leader_schedule_cache
                .slot_leader_at(slot, Some(&working_bank))
            else {
                stats.unknown_shred_slot_leader += shreds
                    .iter()
                    .filter(|shred| shred::layout::get_slot(shred) == Some(slot))
                    .count();
                return None;
            };
            let cluster_nodes = context.cluster_nodes_cache.get(
                slot,
                &root_bank,
                &working_bank,
                &context.cluster_info,
            );
            Some((slot, (slot_leader.id, cluster_nodes)))
        })
        .collect();

    let socket_addr_space = context.cluster_info.socket_addr_space();
    for shred in shreds.drain(..) {
        let socket = RetransmitSocket::new(
            worker_index,
            &context.retransmit_sockets,
            context.xdp_sender.as_ref(),
            &context.cluster_info,
        );
        let shred_stats = retransmit_shred(
            shred,
            &root_bank,
            &context.shred_deduper,
            &cache,
            socket_addr_space,
            socket,
            &mut stats,
        );
        if let Some(shred_stats) = shred_stats {
            stats.record_slot_stats(shred_stats);
        }
    }
    total_timer.stop();
    stats.total_time = total_timer.as_us();
    // If receiver has been dropped, retransmission has already completed and only these per-job
    // stats are lost. The worker has no recovery to perform.
    let _ = context.stats_sender.send(stats);
}

enum RetransmitSocket<'a> {
    Socket(&'a UdpSocket),
    Xdp(&'a XdpSender),
    Multihomed {
        sockets: &'a [UdpSocket],
        interface_offset: usize,
        sockets_per_interface: usize,
        thread_index: usize,
    },
}

impl<'a> RetransmitSocket<'a> {
    pub fn new(
        thread_index: usize,
        retransmit_sockets: &'a [UdpSocket],
        xdp_sender: Option<&'a XdpSender>,
        cluster_info: &'a ClusterInfo,
    ) -> Self {
        if let Some(sender) = xdp_sender {
            RetransmitSocket::Xdp(sender)
        } else if cluster_info.bind_ip_addrs().multihoming_enabled() {
            let sockets_per_interface =
                retransmit_sockets.len() / cluster_info.bind_ip_addrs().len();
            let active_index = cluster_info.bind_ip_addrs().active_index();
            let interface_offset = sockets_per_interface.saturating_mul(active_index);

            RetransmitSocket::Multihomed {
                sockets: retransmit_sockets,
                interface_offset,
                sockets_per_interface,
                thread_index,
            }
        } else {
            let socket: &UdpSocket = &retransmit_sockets[thread_index % retransmit_sockets.len()];
            RetransmitSocket::Socket(socket)
        }
    }

    pub fn get_socket(&self) -> &'a UdpSocket {
        match self {
            RetransmitSocket::Socket(socket) => socket,
            RetransmitSocket::Multihomed {
                sockets,
                interface_offset,
                sockets_per_interface,
                thread_index,
            } => {
                let socket_index = interface_offset + (thread_index % sockets_per_interface);
                &sockets[socket_index]
            }
            RetransmitSocket::Xdp(_) => {
                unreachable!("get_socket() should not be called for XDP variants")
            }
        }
    }
}

// Retransmit a single shred to all downstream nodes
fn retransmit_shred(
    shred: shred::Payload,
    root_bank: &Bank,
    shred_deduper: &ShredDeduper,
    cache: &RetransmitAddrCache,
    socket_addr_space: &SocketAddrSpace,
    socket: RetransmitSocket<'_>,
    stats: &mut BatchStats,
) -> Option<RetransmitShredOutput> {
    let key = shred::layout::get_shred_id(shred.as_ref())?;
    if key.slot() < root_bank.slot()
        || shred_deduper.dedup(key, shred.as_ref(), MAX_DUPLICATE_COUNT)
    {
        stats.num_shreds_skipped += 1;
        return None;
    }
    let mut compute_turbine_peers = Measure::start("turbine_start");
    let (root_distance, addrs) = get_retransmit_addrs(&key, cache, socket_addr_space, stats)?;
    compute_turbine_peers.stop();
    stats.compute_turbine_peers_total += compute_turbine_peers.as_us();
    let last_shred_in_slot = shred::wire::get_flags(shred.as_ref())
        .map(|flags| flags.contains(ShredFlags::LAST_SHRED_IN_SLOT))
        .unwrap_or_default();
    let mut retransmit_time = Measure::start("retransmit_to");
    let num_addrs = addrs.len();
    let num_nodes = match socket {
        RetransmitSocket::Xdp(sender) => {
            let mut sent = num_addrs;
            if num_addrs > 0
                && let Err(e) = sender.try_send(key.index() as usize, addrs, shred.bytes)
            {
                log::warn!("xdp channel full: {e:?}");
                stats.num_shreds_dropped_xdp_full += num_addrs;
                sent = 0;
            }
            sent
        }
        RetransmitSocket::Socket(_) | RetransmitSocket::Multihomed { .. } => {
            let socket = socket.get_socket();
            match multi_target_send(socket, shred, addrs.as_ref()) {
                Ok(num_sent) => num_sent,
                Err(SendPktsError::IoError(ioerr)) => {
                    error!(
                        "retransmit_to multi_target_send error: {ioerr:?}, {num_addrs} \
                         destinations"
                    );
                    0
                }
            }
        }
    };
    retransmit_time.stop();
    stats.num_addrs_failed += num_addrs - num_nodes;
    stats.num_nodes += num_nodes;
    stats.retransmit_total += retransmit_time.as_us();
    Some(RetransmitShredOutput {
        shred: key,
        last_shred_in_slot,
        root_distance,
        num_nodes,
    })
}

fn get_retransmit_addrs(
    shred: &ShredId,
    cache: &RetransmitAddrCache,
    socket_addr_space: &SocketAddrSpace,
    stats: &mut BatchStats,
) -> Option<(/*root_distance:*/ u8, Vec<SocketAddr>)> {
    let (slot_leader, cluster_nodes) = cache.get(&shred.slot())?;
    let (root_distance, addrs) = cluster_nodes
        .get_retransmit_addrs(slot_leader, shred, DATA_PLANE_FANOUT, socket_addr_space)
        .inspect_err(|err| match err {
            Error::Loopback { .. } => {
                stats.num_loopback_errs += 1;
            }
        })
        .ok()?;
    Some((root_distance, addrs))
}

/// Service to retransmit messages received from other peers in turbine.
pub struct RetransmitStage {
    stats_handle: JoinHandle<()>,
    thread_pool: RetransmitWorkerPool,
}

impl RetransmitStage {
    /// Construct the RetransmitStage.
    ///
    /// Key arguments:
    /// * `retransmit_sockets` - Sockets to use for transmission of shreds
    /// * `max_slots` - Structure to keep track of the Turbine progress
    /// * `bank_forks` - Reference to the BankForks structure
    /// * `leader_schedule_cache` - The leader schedule to verify shreds
    /// * `cluster_info` - This structure needs to be updated and populated by the bank and via gossip.
    /// * `retransmit_receiver` - Receive channel for batches of shreds to be retransmitted.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bank_forks: Arc<RwLock<BankForks>>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        cluster_info: Arc<ClusterInfo>,
        retransmit_sockets: Arc<Vec<UdpSocket>>,
        max_slots: Arc<MaxSlots>,
        rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
        slot_status_notifier: Option<SlotStatusNotifier>,
        xdp_sender: Option<XdpSender>,
        votor_event_sender: CrossbeamSender<VotorEvent>,
    ) -> (RetransmitSender, Self) {
        let migration_status = bank_forks.read().unwrap().migration_status();
        let cluster_nodes_cache = Arc::new(ClusterNodesCache::<RetransmitStage>::new(
            CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
            CLUSTER_NODES_CACHE_TTL,
        ));
        let mut rng = rand::rng();
        let shred_deduper = Arc::new(ShredDeduper::new(&mut rng, DEDUPER_NUM_BITS));

        let num_workers = retransmit_sockets.len();
        let (stats_sender, stats_receiver) = crossbeam_channel::bounded(num_workers);
        let is_xdp = xdp_sender.is_some();
        let context = Arc::new(WorkerContext {
            bank_forks: bank_forks.clone(),
            leader_schedule_cache,
            cluster_info: cluster_info.clone(),
            retransmit_sockets,
            xdp_sender,
            cluster_nodes_cache: cluster_nodes_cache.clone(),
            shred_deduper: shred_deduper.clone(),
            max_slots,
            stats_sender,
        });
        let run_batch = {
            let context = Arc::clone(&context);
            move |shreds, worker_index| retransmit_batch(shreds, worker_index, context.as_ref())
        };
        // Match workers to retransmit sockets to preserve socket affinity.
        let (retransmit_sender, thread_pool) = RetransmitWorkerPool::build(
            "solRetransmit",
            num_workers,
            CHANNEL_SIZE_RETRANSMIT_INGRESS,
            run_batch,
        );

        let stats_thread_context = StatsThreadContext {
            bank_forks,
            cluster_info,
            cluster_nodes_cache,
            shred_deduper,
            is_xdp,
            notifiers: RetransmitNotifiers {
                rpc_subscriptions,
                slot_status_notifier,
                migration_status,
                votor_event_sender,
            },
        };

        let stats_handle = Builder::new()
            .name("solRetransmStats".to_string())
            .spawn(move || process_stats_update(stats_thread_context, stats_receiver))
            .unwrap();

        (
            retransmit_sender,
            Self {
                stats_handle,
                thread_pool,
            },
        )
    }

    pub fn join(self) -> thread::Result<()> {
        let Self {
            stats_handle,
            thread_pool,
        } = self;

        let stats_result = stats_handle.join();
        let worker_pool_result = thread_pool.join();

        stats_result?;
        worker_pool_result
    }
}

struct StatsThreadContext {
    bank_forks: Arc<RwLock<BankForks>>,
    cluster_info: Arc<ClusterInfo>,
    cluster_nodes_cache: Arc<ClusterNodesCache<RetransmitStage>>,
    shred_deduper: Arc<ShredDeduper>,
    is_xdp: bool,
    notifiers: RetransmitNotifiers,
}

fn process_stats_update(
    StatsThreadContext {
        bank_forks,
        cluster_info,
        cluster_nodes_cache,
        shred_deduper,
        is_xdp,
        notifiers,
    }: StatsThreadContext,
    receiver: CrossbeamReceiver<BatchStats>,
) {
    let mut stats = RetransmitStats::new(Instant::now());
    let mut pending_first_shred_event = None;
    for batch_stats in receiver.iter() {
        // Attempt to resend a pending first shred event to votor
        if let Some(event) = pending_first_shred_event.take()
            && let Err(TrySendError::Full(event)) = notifiers.votor_event_sender.try_send(event)
        {
            // Failed again, requeue
            pending_first_shred_event = Some(event);
        }
        let mut epoch_cache_update = Measure::start("retransmit_epoch_cache_update");
        shred_deduper.maybe_reset(
            &mut rand::rng(),
            DEDUPER_FALSE_POSITIVE_RATE,
            DEDUPER_RESET_CYCLE,
        );
        epoch_cache_update.stop();
        stats.epoch_cache_update += epoch_cache_update.as_us();

        stats.accumulate_job_counters(&batch_stats);
        stats.insert_slot_stats(
            batch_stats.slot_stats,
            batch_stats.root,
            &notifiers,
            &mut pending_first_shred_event,
        );
        stats.submit_slot_stats();

        stats.maybe_submit(
            &bank_forks,
            cluster_info.as_ref(),
            &cluster_nodes_cache,
            is_xdp,
        );
    }
}

impl AddAssign for RetransmitSlotStats {
    fn add_assign(&mut self, other: Self) {
        let Self {
            asof,
            outset,
            max_index_code,
            max_index_data,
            last_shred_in_slot,
            num_shreds_received,
            num_shreds_sent,
        } = other;
        self.asof = self.asof.max(asof);
        self.max_index_code = self.max_index_code.max(max_index_code);
        self.max_index_data = self.max_index_data.max(max_index_data);
        self.last_shred_in_slot |= last_shred_in_slot;
        self.outset = if self.outset == 0 {
            outset
        } else {
            self.outset.min(outset)
        };
        for k in 0..MAX_NUM_TURBINE_HOPS {
            self.num_shreds_received[k] += num_shreds_received[k];
            self.num_shreds_sent[k] += num_shreds_sent[k];
        }
    }
}

impl RetransmitStats {
    const SLOT_STATS_CACHE_CAPACITY: usize = 750;

    fn new(now: Instant) -> Self {
        Self {
            since: now,
            num_iters: 0usize,
            num_nodes: 0usize,
            num_addrs_failed: 0usize,
            num_shreds_dropped_xdp_full: 0usize,
            num_loopback_errs: 0usize,
            num_shreds: 0usize,
            num_shreds_skipped: 0usize,
            total_batches: 0usize,
            num_small_batches: 0usize,
            total_time: 0u64,
            epoch_fetch: 0u64,
            epoch_cache_update: 0u64,
            retransmit_total: 0u64,
            compute_turbine_peers_total: 0u64,
            // Cache capacity is manually enforced by `SLOT_STATS_CACHE_CAPACITY`
            slot_stats: LruCache::<Slot, RetransmitSlotStats>::unbounded(),
            unknown_shred_slot_leader: 0usize,
        }
    }

    fn insert_slot_stats(
        &mut self,
        feed: impl IntoIterator<Item = (Slot, RetransmitSlotStats)>,
        root: Slot,
        notifiers: &RetransmitNotifiers,
        pending_first_shred_event: &mut Option<VotorEvent>,
    ) {
        for (slot, slot_stats) in feed {
            match self.slot_stats.get_mut(&slot) {
                None => {
                    if slot > root {
                        notify_subscribers(
                            slot,
                            slot_stats.outset,
                            notifiers,
                            pending_first_shred_event,
                        );
                    }
                    self.slot_stats.put(slot, slot_stats);
                }
                Some(entry) => {
                    *entry += slot_stats;
                }
            }
        }
    }

    fn submit_slot_stats(&mut self) {
        while self.slot_stats.len() > Self::SLOT_STATS_CACHE_CAPACITY {
            // Pop and submit metrics for the slot which was updated least
            // recently. At this point the node most likely will not receive
            // and retransmit any more shreds for this slot.
            match self.slot_stats.pop_lru() {
                Some((slot, stats)) => stats.submit(slot),
                None => break,
            }
        }
    }

    fn accumulate_job_counters(&mut self, job_stats: &BatchStats) {
        self.num_iters += 1;
        self.num_shreds += job_stats.num_shreds;
        self.num_small_batches += job_stats.num_small_batches;
        self.total_batches += job_stats.total_batches;
        self.num_nodes += job_stats.num_nodes;
        self.num_addrs_failed += job_stats.num_addrs_failed;
        self.num_shreds_dropped_xdp_full += job_stats.num_shreds_dropped_xdp_full;
        self.num_loopback_errs += job_stats.num_loopback_errs;
        self.num_shreds_skipped += job_stats.num_shreds_skipped;
        self.total_time += job_stats.total_time;
        self.epoch_fetch += job_stats.epoch_fetch;
        self.retransmit_total += job_stats.retransmit_total;
        self.compute_turbine_peers_total += job_stats.compute_turbine_peers_total;
        self.unknown_shred_slot_leader += job_stats.unknown_shred_slot_leader;
    }
}

impl RetransmitSlotStats {
    fn record(&mut self, now: u64, out: RetransmitShredOutput) {
        self.outset = if self.outset == 0 {
            now
        } else {
            self.outset.min(now)
        };
        self.asof = self.asof.max(now);
        let max_index = match out.shred.shred_type() {
            ShredType::Code => &mut self.max_index_code,
            ShredType::Data => &mut self.max_index_data,
        };
        *max_index = (*max_index).max(out.shred.index());
        self.last_shred_in_slot |= out.last_shred_in_slot;
        self.num_shreds_received[usize::from(out.root_distance)] += 1;
        self.num_shreds_sent[usize::from(out.root_distance)] += out.num_nodes;
    }

    fn submit(&self, slot: Slot) {
        let num_shreds: usize = self.num_shreds_received.iter().sum();
        let num_nodes: usize = self.num_shreds_sent.iter().sum();
        let elapsed_millis = self.asof.saturating_sub(self.outset);
        datapoint_info!(
            "retransmit-stage-slot-stats",
            ("slot", slot, i64),
            ("outset_timestamp", self.outset, i64),
            ("elapsed_millis", elapsed_millis, i64),
            ("num_shreds", num_shreds, i64),
            ("num_nodes", num_nodes, i64),
            ("num_shreds_received_root", self.num_shreds_received[0], i64),
            (
                "num_shreds_received_1st_layer",
                self.num_shreds_received[1],
                i64
            ),
            (
                "num_shreds_received_2nd_layer",
                self.num_shreds_received[2],
                i64
            ),
            (
                "num_shreds_received_3rd_layer",
                self.num_shreds_received[3],
                i64
            ),
            ("num_shreds_sent_root", self.num_shreds_sent[0], i64),
            ("num_shreds_sent_1st_layer", self.num_shreds_sent[1], i64),
            ("num_shreds_sent_2nd_layer", self.num_shreds_sent[2], i64),
            ("num_shreds_sent_3rd_layer", self.num_shreds_sent[3], i64),
        );
    }
}

// Notifies subscribers of shreds received from a new slot.
fn notify_subscribers(
    slot: Slot,
    timestamp: u64, // When the first shred in the slot was received.
    notifiers: &RetransmitNotifiers,
    pending_first_shred_event: &mut Option<VotorEvent>,
) {
    if let Some(rpc_subscriptions) = notifiers.rpc_subscriptions.as_ref() {
        let slot_update = SlotUpdate::FirstShredReceived { slot, timestamp };
        rpc_subscriptions.notify_slot_update(slot_update);
        datapoint_info!("retransmit-first-shred", ("slot", slot, i64));
    }
    if let Some(slot_status_notifier) = notifiers.slot_status_notifier.as_ref() {
        slot_status_notifier
            .read()
            .unwrap()
            .notify_first_shred_received(slot);
    }

    if notifiers.migration_status.should_send_first_shred(slot) {
        match notifiers
            .votor_event_sender
            .try_send(VotorEvent::FirstShred(slot))
        {
            Ok(()) => (),
            Err(TrySendError::Full(event)) => {
                error!(
                    "Votor event channel is backed up len {}, something is wrong",
                    notifiers.votor_event_sender.len(),
                );
                // Only the latest first shred notification matters, requeue
                pending_first_shred_event.replace(event);
            }
            Err(TrySendError::Disconnected(_)) => {
                info!("Votor event channel disconnected, we are shutting down")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::TryRecvError,
        rand::SeedableRng,
        rand_chacha::ChaChaRng,
        solana_entry::entry::create_ticks,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
        solana_ledger::shred::{ProcessShredsStats, ReedSolomonCache, Shredder},
    };

    fn get_keypair() -> Keypair {
        const KEYPAIR: &str = "Fcc2HUvRC7Dv4GgehTziAremzRvwDw5miYu8Ahuu1rsGjA\
            5eCn55pXiSkEPcuqviV41rJxrFpZDmHmQkZWfoYYS";
        bs58::decode(KEYPAIR)
            .into_vec()
            .as_deref()
            .map(Keypair::try_from)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn test_shred_deduper() {
        let keypair = get_keypair();
        let entries = create_ticks(10, 1, Hash::new_unique());
        let rsc = ReedSolomonCache::default();
        let make_shreds_for_slot = |slot, parent, code_index| {
            let shredder = Shredder::new(slot, parent, 1, 0).unwrap();
            shredder.entries_to_merkle_shreds_for_tests(
                &keypair,
                &entries,
                true,
                // chained_merkle_root
                Hash::new_from_array(rand::rng().random()),
                0,
                code_index,
                &rsc,
                &mut ProcessShredsStats::default(),
            )
        };

        let mut rng = ChaChaRng::from_seed([0xa5; 32]);
        let shred_deduper = ShredDeduper::<2>::new(&mut rng, /*num_bits:*/ 640_007);

        // make a set of shreds for slot 5 with parent slot 4
        let (shreds_data_5_4, shreds_code_5_4) = make_shreds_for_slot(5, 4, 0);
        // make a set of shreds for slot 5 with parent slot 3
        let (shreds_data_5_3, _shreds_code_5_3) = make_shreds_for_slot(5, 3, 0);
        // make a set of shreds for slot 5 with parent slot 2
        let (shreds_data_5_2, _shreds_code_5_2) = make_shreds_for_slot(5, 2, 0);
        // pick a shred for tests
        let shred = shreds_data_5_4.last().unwrap().clone();
        // unique shred should pass
        assert!(
            !shred_deduper.dedup(shred.id(), shred.payload(), MAX_DUPLICATE_COUNT),
            "First time shred X => Not dup because it is the only shred"
        );
        // duplicate shred blocked
        assert!(
            shred_deduper.dedup(shred.id(), shred.payload(), MAX_DUPLICATE_COUNT),
            "
            Second time shred X => Dup because common header is duplicate
            "
        );
        // Pick a shred with same index as `shred` but different parent offset
        let shred_dup = shreds_data_5_3.last().unwrap().clone();
        // first shred passed through
        assert!(
            !shred_deduper.dedup(shred_dup.id(), shred_dup.payload(), MAX_DUPLICATE_COUNT),
            "First time seeing shred X with different parent slot (3 instead of 4) => Not dup \
             because common header is unique & shred ID only seen once"
        );
        // then blocked
        assert!(
            shred_deduper.dedup(shred_dup.id(), shred_dup.payload(), MAX_DUPLICATE_COUNT),
            "Second time seeing shred X with parent slot 3 => Dup because common header is not \
             unique & shred ID seen twice"
        );

        let shred_dup2 = shreds_data_5_2.last().unwrap().clone();

        assert!(
            shred_deduper.dedup(shred_dup2.id(), shred_dup2.payload(), MAX_DUPLICATE_COUNT),
            "First time seeing shred X with parent slot 2 => Dup because common header is unique \
             but shred ID seen twice already"
        );

        /* Coding shreds */

        // Pick a coding shred at index 4 based off FEC set index 0
        let shred = shreds_code_5_4[4].clone();
        // Coding passes
        assert!(
            !shred_deduper.dedup(shred.id(), shred.payload(), MAX_DUPLICATE_COUNT),
            "
           First time seeing coding shred Y => Not dup because common header & shred ID are unique"
        );
        // then blocked
        assert!(
            shred_deduper.dedup(shred.id(), shred.payload(), MAX_DUPLICATE_COUNT),
            "
            Second time seeing coding shred Y => Dup because common header is dup
            "
        );

        // Make a coding shred at index 4 based off FEC set index 2
        let (_, shreds_code_invalid) = make_shreds_for_slot(5, 4, 2);

        let shred_inv_code_1 = shreds_code_invalid[2].clone();
        assert_eq!(
            shred.index(),
            shred_inv_code_1.index(),
            "we want a shred with same index but different FEC set index"
        );
        // 2nd unique coding passes
        assert!(
            !shred_deduper.dedup(
                shred_inv_code_1.id(),
                shred_inv_code_1.payload(),
                MAX_DUPLICATE_COUNT
            ),
            "First time seeing shred Y w/ changed header (FEC Set index 2) => Not dup because \
             common header is unique & shred ID only seen once"
        );
        // same again is blocked
        assert!(
            shred_deduper.dedup(
                shred_inv_code_1.id(),
                shred_inv_code_1.payload(),
                MAX_DUPLICATE_COUNT
            ),
            "
           Second time seeing shred Y w/ changed header (FEC Set index 2) => Dup because common \
             header is not unique & shred ID seen twice "
        );
        // Make a coding shred at index 4 based off FEC set index 3
        let (_, shreds_code_invalid) = make_shreds_for_slot(5, 4, 3);

        let shred_inv_code_2 = shreds_code_invalid[1].clone();
        assert_eq!(
            shred.index(),
            shred_inv_code_2.index(),
            "we want a shred with same index but different FEC set index"
        );
        assert!(
            shred_deduper.dedup(
                shred_inv_code_2.id(),
                shred_inv_code_2.payload(),
                MAX_DUPLICATE_COUNT
            ),
            "
           First time seeing shred Y w/ changed header (FEC Set index 3)=>Dup because common \
             header is unique but shred ID seen twice already"
        );
    }

    #[test]
    fn test_notify_subscribers_sends_first_shred_after_genesis() {
        let (votor_event_sender, votor_event_receiver) = crossbeam_channel::bounded(1);
        let migration_status = Arc::new(MigrationStatus::post_migration_status());
        let genesis_slot = migration_status.genesis_block().unwrap().slot;
        let notifiers = RetransmitNotifiers {
            rpc_subscriptions: None,
            slot_status_notifier: None,
            migration_status,
            votor_event_sender,
        };
        let mut pending_first_shred_event = None;

        let slot = genesis_slot.checked_add(1).unwrap();
        assert!(!slot.is_multiple_of(NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot));
        notify_subscribers(
            slot,
            /*timestamp:*/ 0,
            &notifiers,
            &mut pending_first_shred_event,
        );

        assert!(matches!(
            votor_event_receiver.try_recv(),
            Ok(VotorEvent::FirstShred(received_slot)) if received_slot == slot
        ));
        assert!(pending_first_shred_event.is_none());

        // Later unaligned slots do not need a FirstShred event.
        let slot = slot.checked_add(1).unwrap();
        assert!(!slot.is_multiple_of(NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot));
        notify_subscribers(
            slot,
            /*timestamp:*/ 0,
            &notifiers,
            &mut pending_first_shred_event,
        );
        assert!(matches!(
            votor_event_receiver.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }
}
