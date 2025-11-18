//! The `retransmit_stage` retransmits shreds between validators

use {
    crate::{
        addr_cache::AddrCache,
        cluster_nodes::{self, ClusterNodes, ClusterNodesCache, Error, MAX_NUM_TURBINE_HOPS},
        xdp::XdpSender,
    },
    agave_votor::event::VotorEvent,
    bytes::Bytes,
    crossbeam_channel::{Receiver, RecvError, Sender, TryRecvError},
    lru::LruCache,
    rand::Rng,
    rayon::{prelude::*, ThreadPool, ThreadPoolBuilder},
    solana_clock::Slot,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::Protocol},
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
    solana_streamer::sendmmsg::{multi_target_send, SendPktsError},
    solana_time_utils::timestamp,
    std::{
        borrow::Cow,
        collections::{HashMap, HashSet},
        net::{SocketAddr, UdpSocket},
        ops::AddAssign,
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Arc, RwLock,
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::sync::mpsc::Sender as AsyncSender,
};

const MAX_DUPLICATE_COUNT: usize = 2;
const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
const DEDUPER_NUM_BITS: u64 = 637_534_199; // 76MB
const DEDUPER_RESET_CYCLE: Duration = Duration::from_secs(5 * 60);
// Minimum number of shreds to use rayon parallel iterators.
const PAR_ITER_MIN_NUM_SHREDS: usize = 2;

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
    // Addresses the shred was sent to if there was a cache miss.
    addrs: Option<Box<[SocketAddr]>>,
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
    // Root distance and socket-addresses the shreds were sent to if there was
    // a cache miss.
    pub(crate) addrs: Vec<(ShredId, /*root_distance:*/ u8, Box<[SocketAddr]>)>,
}

struct RetransmitStats {
    since: Instant,
    addr_cache_hit: AtomicUsize,
    addr_cache_miss: AtomicUsize,
    num_nodes: AtomicUsize,
    num_addrs_failed: AtomicUsize,
    num_shreds_dropped_xdp_full: AtomicUsize,
    num_loopback_errs: AtomicUsize,
    num_shreds: usize,
    num_shreds_skipped: AtomicUsize,
    num_small_batches: usize,
    total_batches: usize,
    total_time: u64,
    epoch_fetch: u64,
    epoch_cache_update: u64,
    retransmit_total: AtomicU64,
    compute_turbine_peers_total: AtomicU64,
    slot_stats: LruCache<Slot, RetransmitSlotStats>,
    unknown_shred_slot_leader: usize,
}

impl RetransmitStats {
    fn maybe_submit(
        &mut self,
        root_bank: &Bank,
        working_bank: &Bank,
        cluster_info: &ClusterInfo,
        cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
        is_xdp: bool,
    ) {
        const SUBMIT_CADENCE: Duration = Duration::from_secs(2);
        if self.since.elapsed() < SUBMIT_CADENCE {
            return;
        }
        cluster_nodes_cache
            .get(root_bank.slot(), root_bank, working_bank, cluster_info)
            .submit_metrics("cluster_nodes_retransmit", timestamp());
        datapoint_info!(
            "retransmit-stage",
            "is_xdp" => is_xdp.to_string(),
            ("total_time", self.total_time, i64),
            ("epoch_fetch", self.epoch_fetch, i64),
            ("epoch_cache_update", self.epoch_cache_update, i64),
            ("total_batches", self.total_batches, i64),
            ("num_small_batches", self.num_small_batches, i64),
            ("num_nodes", *self.num_nodes.get_mut(), i64),
            ("num_addrs_failed", *self.num_addrs_failed.get_mut(), i64),
            (
                "num_shreds_dropped_xdp_full",
                *self.num_shreds_dropped_xdp_full.get_mut(),
                i64
            ),
            ("num_loopback_errs", *self.num_loopback_errs.get_mut(), i64),
            ("num_shreds", self.num_shreds, i64),
            (
                "num_shreds_skipped",
                *self.num_shreds_skipped.get_mut(),
                i64
            ),
            ("retransmit_total", *self.retransmit_total.get_mut(), i64),
            ("addr_cache_hit", *self.addr_cache_hit.get_mut(), i64),
            ("addr_cache_miss", *self.addr_cache_miss.get_mut(), i64),
            (
                "compute_turbine",
                *self.compute_turbine_peers_total.get_mut(),
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

    fn maybe_reset<R: Rng>(
        &mut self,
        rng: &mut R,
        false_positive_rate: f64,
        reset_cycle: Duration,
    ) {
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
        if let Some(xdp_sender) = xdp_sender {
            RetransmitSocket::Xdp(xdp_sender)
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

/// The number of shreds to pull from the retransmit_receiver at a time.
const RETRANSMIT_BATCH_SIZE: usize = 4096;

// pull the shreds from the shreds_receiver until empty, then retransmit them.
// uses a thread_pool to parallelize work if there are enough shreds to justify that
#[allow(clippy::too_many_arguments)]
fn retransmit(
    thread_pool: &ThreadPool,
    bank_forks: &RwLock<BankForks>,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_info: &ClusterInfo,
    retransmit_receiver: &Receiver<Vec<shred::Payload>>,
    retransmit_sockets: &[UdpSocket],
    quic_endpoint_sender: &AsyncSender<(SocketAddr, Bytes)>,
    xdp_sender: Option<&XdpSender>,
    stats: &mut RetransmitStats,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
    addr_cache: &mut AddrCache,
    shred_deduper: &mut ShredDeduper,
    max_slots: &MaxSlots,
    rpc_subscriptions: Option<&RpcSubscriptions>,
    slot_status_notifier: Option<&SlotStatusNotifier>,
    shred_buf: &mut Vec<Vec<shred::Payload>>,
    votor_event_sender: Option<&Sender<VotorEvent>>,
) -> Result<(), RecvError> {
    // Try to receive shreds from the channel without blocking. If the channel
    // is empty precompute turbine trees speculatively. If no cache updates are
    // made then block on the channel until some shreds are received.
    match retransmit_receiver.try_recv() {
        Ok(shreds) => {
            shred_buf.push(shreds);
        }
        Err(TryRecvError::Disconnected) => return Err(RecvError),
        Err(TryRecvError::Empty) => {
            if cache_retransmit_addrs(
                thread_pool,
                addr_cache,
                bank_forks,
                leader_schedule_cache,
                cluster_info,
                cluster_nodes_cache,
            ) {
                return Ok(());
            }
            shred_buf.push(retransmit_receiver.recv()?);
        }
    };
    // now the batch has started
    let mut timer_start = Measure::start("retransmit");
    let mut num_shreds = shred_buf[0].len();
    // Create a RETRANSMIT_BATCH_SIZE sized batch from the channel
    for shreds in retransmit_receiver
        .try_iter()
        // We already pulled 1 batch
        .take(RETRANSMIT_BATCH_SIZE - 1)
    {
        num_shreds += shreds.len();
        shred_buf.push(shreds);
    }

    stats.num_shreds += num_shreds;
    stats.total_batches += 1;

    let mut epoch_fetch = Measure::start("retransmit_epoch_fetch");
    let (working_bank, root_bank) = {
        let bank_forks = bank_forks.read().unwrap();
        (bank_forks.working_bank(), bank_forks.root_bank())
    };
    epoch_fetch.stop();
    stats.epoch_fetch += epoch_fetch.as_us();

    let mut epoch_cache_update = Measure::start("retransmit_epoch_cache_update");
    shred_deduper.maybe_reset(
        &mut rand::thread_rng(),
        DEDUPER_FALSE_POSITIVE_RATE,
        DEDUPER_RESET_CYCLE,
    );
    epoch_cache_update.stop();
    stats.epoch_cache_update += epoch_cache_update.as_us();
    // Lookup slot leader and cluster nodes for each slot.
    let cache: HashMap<Slot, _> = shred_buf
        .iter()
        .flatten()
        .filter_map(|shred| shred::layout::get_slot(shred))
        .collect::<HashSet<Slot>>()
        .into_iter()
        .filter_map(|slot: Slot| {
            max_slots.retransmit.fetch_max(slot, Ordering::Relaxed);
            // TODO: consider using root-bank here for leader lookup!
            // Shreds' signatures should be verified before they reach here,
            // and if the leader is unknown they should fail signature check.
            // So here we should expect to know the slot leader and otherwise
            // skip the shred.
            let Some(slot_leader) = leader_schedule_cache.slot_leader_at(slot, Some(&working_bank))
            else {
                stats.unknown_shred_slot_leader += num_shreds;
                return None;
            };
            let cluster_nodes =
                cluster_nodes_cache.get(slot, &root_bank, &working_bank, cluster_info);
            Some((slot, (slot_leader, cluster_nodes)))
        })
        .collect();
    let socket_addr_space = cluster_info.socket_addr_space();
    let record = |mut stats: HashMap<Slot, RetransmitSlotStats>, out: RetransmitShredOutput| {
        let now = timestamp();
        let entry = stats.entry(out.shred.slot()).or_default();
        entry.record(now, out);
        stats
    };
    let retransmit_shred = |shred, socket, stats| {
        retransmit_shred(
            shred,
            &root_bank,
            shred_deduper,
            &cache,
            addr_cache,
            socket_addr_space,
            socket,
            quic_endpoint_sender,
            stats,
        )
    };

    let retransmit_socket =
        |index: usize| RetransmitSocket::new(index, retransmit_sockets, xdp_sender, cluster_info);

    let slot_stats = if num_shreds < PAR_ITER_MIN_NUM_SHREDS {
        stats.num_small_batches += 1;
        shred_buf
            .drain(..)
            .flatten()
            .enumerate()
            .filter_map(|(index, shred)| retransmit_shred(shred, retransmit_socket(index), stats))
            .fold(HashMap::new(), record)
    } else {
        thread_pool.install(|| {
            shred_buf
                .par_drain(..)
                .flatten()
                .filter_map(|shred| {
                    retransmit_shred(
                        shred,
                        retransmit_socket(thread_pool.current_thread_index().unwrap()),
                        stats,
                    )
                })
                .fold(HashMap::new, record)
                .reduce(HashMap::new, RetransmitSlotStats::merge)
        })
    };

    stats.upsert_slot_stats(
        slot_stats,
        root_bank.slot(),
        addr_cache,
        rpc_subscriptions,
        slot_status_notifier,
        votor_event_sender,
    );
    timer_start.stop();
    stats.total_time += timer_start.as_us();
    stats.maybe_submit(
        &root_bank,
        &working_bank,
        cluster_info,
        cluster_nodes_cache,
        xdp_sender.is_some(),
    );
    Ok(())
}

// Retransmit a single shred to all downstream nodes
fn retransmit_shred(
    shred: shred::Payload,
    root_bank: &Bank,
    shred_deduper: &ShredDeduper,
    cache: &HashMap<Slot, (/*leader:*/ Pubkey, Arc<ClusterNodes<RetransmitStage>>)>,
    addr_cache: &AddrCache,
    socket_addr_space: &SocketAddrSpace,
    socket: RetransmitSocket<'_>,
    quic_endpoint_sender: &AsyncSender<(SocketAddr, Bytes)>,
    stats: &RetransmitStats,
) -> Option<RetransmitShredOutput> {
    let key = shred::layout::get_shred_id(shred.as_ref())?;
    if key.slot() < root_bank.slot()
        || shred_deduper.dedup(key, shred.as_ref(), MAX_DUPLICATE_COUNT)
    {
        stats.num_shreds_skipped.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut compute_turbine_peers = Measure::start("turbine_start");
    let (root_distance, addrs) =
        get_retransmit_addrs(&key, root_bank, cache, addr_cache, socket_addr_space, stats)?;
    compute_turbine_peers.stop();
    stats
        .compute_turbine_peers_total
        .fetch_add(compute_turbine_peers.as_us(), Ordering::Relaxed);
    let last_shred_in_slot = shred::wire::get_flags(shred.as_ref())
        .map(|flags| flags.contains(ShredFlags::LAST_SHRED_IN_SLOT))
        .unwrap_or_default();
    let mut retransmit_time = Measure::start("retransmit_to");
    let num_addrs = addrs.len();
    let num_nodes = match cluster_nodes::get_broadcast_protocol(&key) {
        Protocol::QUIC => {
            let shred = shred.bytes;
            addrs
                .iter()
                .filter_map(|&addr| quic_endpoint_sender.try_send((addr, shred.clone())).ok())
                .count()
        }
        Protocol::UDP => match socket {
            RetransmitSocket::Xdp(sender) => {
                let mut sent = num_addrs;
                if num_addrs > 0 {
                    if let Err(e) = sender.try_send(key.index() as usize, addrs.to_vec(), shred) {
                        log::warn!("xdp channel full: {e:?}");
                        stats
                            .num_shreds_dropped_xdp_full
                            .fetch_add(num_addrs, Ordering::Relaxed);
                        sent = 0;
                    }
                }
                sent
            }
            RetransmitSocket::Socket(_) | RetransmitSocket::Multihomed { .. } => {
                let socket = socket.get_socket();
                match multi_target_send(socket, shred, &addrs) {
                    Ok(()) => num_addrs,
                    Err(SendPktsError::IoError(ioerr, num_failed)) => {
                        error!(
                            "retransmit_to multi_target_send error: {ioerr:?}, \
                             {num_failed}/{num_addrs} packets failed"
                        );
                        num_addrs - num_failed
                    }
                }
            }
        },
    };
    retransmit_time.stop();
    stats
        .num_addrs_failed
        .fetch_add(num_addrs - num_nodes, Ordering::Relaxed);
    stats.num_nodes.fetch_add(num_nodes, Ordering::Relaxed);
    stats
        .retransmit_total
        .fetch_add(retransmit_time.as_us(), Ordering::Relaxed);
    Some(RetransmitShredOutput {
        shred: key,
        last_shred_in_slot,
        root_distance,
        num_nodes,
        addrs: match addrs {
            Cow::Owned(addrs) => Some(addrs.into_boxed_slice()),
            Cow::Borrowed(_) => None,
        },
    })
}

fn get_retransmit_addrs<'a>(
    shred: &ShredId,
    root_bank: &Bank,
    cache: &HashMap<Slot, (/*leader:*/ Pubkey, Arc<ClusterNodes<RetransmitStage>>)>,
    addr_cache: &'a AddrCache,
    socket_addr_space: &SocketAddrSpace,
    stats: &RetransmitStats,
) -> Option<(/*root_distance:*/ u8, Cow<'a, [SocketAddr]>)> {
    if let Some((root_distance, addrs)) = addr_cache.get(shred) {
        stats.addr_cache_hit.fetch_add(1, Ordering::Relaxed);
        return Some((root_distance, Cow::Borrowed(addrs)));
    }
    let (slot_leader, cluster_nodes) = cache.get(&shred.slot())?;
    let data_plane_fanout = cluster_nodes::get_data_plane_fanout(shred.slot(), root_bank);
    let (root_distance, addrs) = cluster_nodes
        .get_retransmit_addrs(slot_leader, shred, data_plane_fanout, socket_addr_space)
        .inspect_err(|err| match err {
            Error::Loopback { .. } => {
                stats.num_loopback_errs.fetch_add(1, Ordering::Relaxed);
            }
        })
        .ok()?;
    stats.addr_cache_miss.fetch_add(1, Ordering::Relaxed);
    Some((root_distance, Cow::Owned(addrs)))
}

// Speculatively precomputes turbine tree and caches retranmsit addresses.
// Returns false if no new addresses were cached.
#[must_use]
fn cache_retransmit_addrs(
    thread_pool: &ThreadPool,
    addr_cache: &mut AddrCache,
    bank_forks: &RwLock<BankForks>,
    leader_schedule_cache: &LeaderScheduleCache,
    cluster_info: &ClusterInfo,
    cluster_nodes_cache: &ClusterNodesCache<RetransmitStage>,
) -> bool {
    let shreds = addr_cache.get_shreds(thread_pool.current_num_threads() * 4);
    if shreds.is_empty() {
        return false;
    }
    let (working_bank, root_bank) = {
        let bank_forks = bank_forks.read().unwrap();
        (bank_forks.working_bank(), bank_forks.root_bank())
    };
    let cache: HashMap<Slot, _> = shreds
        .iter()
        .map(ShredId::slot)
        .collect::<HashSet<Slot>>()
        .into_iter()
        .filter_map(|slot: Slot| {
            let slot_leader = leader_schedule_cache.slot_leader_at(slot, Some(&working_bank))?;
            let cluster_nodes =
                cluster_nodes_cache.get(slot, &root_bank, &working_bank, cluster_info);
            Some((slot, (slot_leader, cluster_nodes)))
        })
        .collect();
    if cache.is_empty() {
        return false;
    }
    let socket_addr_space = cluster_info.socket_addr_space();
    let get_retransmit_addrs = |shred: ShredId| {
        let data_plane_fanout = cluster_nodes::get_data_plane_fanout(shred.slot(), &root_bank);
        let (slot_leader, cluster_nodes) = cache.get(&shred.slot())?;
        let (root_distance, addrs) = cluster_nodes
            .get_retransmit_addrs(slot_leader, &shred, data_plane_fanout, socket_addr_space)
            .ok()?;
        Some((shred, (root_distance, addrs.into_boxed_slice())))
    };
    let mut out = false;
    if shreds.len() < PAR_ITER_MIN_NUM_SHREDS {
        for (shred, entry) in shreds.into_iter().filter_map(get_retransmit_addrs) {
            addr_cache.put(&shred, entry);
            out = true;
        }
    } else {
        let entries: Vec<_> = thread_pool.install(|| {
            shreds
                .into_par_iter()
                .filter_map(get_retransmit_addrs)
                .collect()
        });
        for (shred, entry) in entries {
            addr_cache.put(&shred, entry);
            out = true;
        }
    }
    out
}

/// Service to retransmit messages received from other peers in turbine.
pub struct RetransmitStage {
    retransmit_thread_handle: JoinHandle<()>,
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
        quic_endpoint_sender: AsyncSender<(SocketAddr, Bytes)>,
        retransmit_receiver: Receiver<Vec<shred::Payload>>,
        max_slots: Arc<MaxSlots>,
        rpc_subscriptions: Option<Arc<RpcSubscriptions>>,
        slot_status_notifier: Option<SlotStatusNotifier>,
        xdp_sender: Option<XdpSender>,
        votor_event_sender: Option<Sender<VotorEvent>>,
    ) -> Self {
        let cluster_nodes_cache = ClusterNodesCache::<RetransmitStage>::new(
            CLUSTER_NODES_CACHE_NUM_EPOCH_CAP,
            CLUSTER_NODES_CACHE_TTL,
        );
        let mut rng = rand::thread_rng();
        let mut stats = RetransmitStats::new(Instant::now());
        let mut addr_cache = AddrCache::with_capacity(/*capacity:*/ 4);
        let mut shred_deduper = ShredDeduper::new(&mut rng, DEDUPER_NUM_BITS);

        let thread_pool = {
            let num_threads = retransmit_sockets.len();
            ThreadPoolBuilder::new()
                .num_threads(num_threads)
                .thread_name(|i| format!("solRetransmit{i:02}"))
                .build()
                .unwrap()
        };

        let retransmit_thread_handle = Builder::new()
            .name("solRetransmittr".to_string())
            .spawn({
                move || {
                    let mut shred_buf = Vec::with_capacity(RETRANSMIT_BATCH_SIZE);
                    while retransmit(
                        &thread_pool,
                        &bank_forks,
                        &leader_schedule_cache,
                        &cluster_info,
                        &retransmit_receiver,
                        &retransmit_sockets,
                        &quic_endpoint_sender,
                        xdp_sender.as_ref(),
                        &mut stats,
                        &cluster_nodes_cache,
                        &mut addr_cache,
                        &mut shred_deduper,
                        &max_slots,
                        rpc_subscriptions.as_deref(),
                        slot_status_notifier.as_ref(),
                        &mut shred_buf,
                        votor_event_sender.as_ref(),
                    )
                    .is_ok()
                    {}
                }
            })
            .unwrap();

        Self {
            retransmit_thread_handle,
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.retransmit_thread_handle.join()
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
            mut addrs,
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
        if self.addrs.len() < addrs.len() {
            std::mem::swap(&mut self.addrs, &mut addrs);
        }
        self.addrs.append(&mut addrs);
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
            addr_cache_hit: AtomicUsize::default(),
            addr_cache_miss: AtomicUsize::default(),
            num_nodes: AtomicUsize::default(),
            num_addrs_failed: AtomicUsize::default(),
            num_shreds_dropped_xdp_full: AtomicUsize::default(),
            num_loopback_errs: AtomicUsize::default(),
            num_shreds: 0usize,
            num_shreds_skipped: AtomicUsize::default(),
            total_batches: 0usize,
            num_small_batches: 0usize,
            total_time: 0u64,
            epoch_fetch: 0u64,
            epoch_cache_update: 0u64,
            retransmit_total: AtomicU64::default(),
            compute_turbine_peers_total: AtomicU64::default(),
            // Cache capacity is manually enforced by `SLOT_STATS_CACHE_CAPACITY`
            slot_stats: LruCache::<Slot, RetransmitSlotStats>::unbounded(),
            unknown_shred_slot_leader: 0usize,
        }
    }

    fn upsert_slot_stats(
        &mut self,
        feed: impl IntoIterator<Item = (Slot, RetransmitSlotStats)>,
        root: Slot,
        addr_cache: &mut AddrCache,
        rpc_subscriptions: Option<&RpcSubscriptions>,
        slot_status_notifier: Option<&SlotStatusNotifier>,
        votor_event_sender: Option<&Sender<VotorEvent>>,
    ) {
        for (slot, mut slot_stats) in feed {
            addr_cache.record(slot, &mut slot_stats);
            match self.slot_stats.get_mut(&slot) {
                None => {
                    if slot > root {
                        notify_subscribers(
                            slot,
                            slot_stats.outset,
                            rpc_subscriptions,
                            slot_status_notifier,
                            votor_event_sender,
                        );
                    }
                    self.slot_stats.put(slot, slot_stats);
                }
                Some(entry) => {
                    *entry += slot_stats;
                }
            }
        }
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
        if let Some(addrs) = out.addrs {
            self.addrs.push((out.shred, out.root_distance, addrs));
        }
    }

    fn merge(mut acc: HashMap<Slot, Self>, other: HashMap<Slot, Self>) -> HashMap<Slot, Self> {
        if acc.len() < other.len() {
            return Self::merge(other, acc);
        }
        for (key, value) in other {
            *acc.entry(key).or_default() += value;
        }
        acc
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
    rpc_subscriptions: Option<&RpcSubscriptions>,
    slot_status_notifier: Option<&SlotStatusNotifier>,
    votor_event_sender: Option<&Sender<VotorEvent>>,
) {
    if let Some(rpc_subscriptions) = rpc_subscriptions {
        let slot_update = SlotUpdate::FirstShredReceived { slot, timestamp };
        rpc_subscriptions.notify_slot_update(slot_update);
        datapoint_info!("retransmit-first-shred", ("slot", slot, i64));
    }
    if let Some(slot_status_notifier) = slot_status_notifier {
        slot_status_notifier
            .read()
            .unwrap()
            .notify_first_shred_received(slot);
    }
    if let Some(votor_event_sender) = votor_event_sender {
        if let Err(err) = votor_event_sender.send(VotorEvent::FirstShred(slot)) {
            warn!(
                "Sending {:?} failed as channel became disconnected.  Ignoring.",
                err.into_inner()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        rand::SeedableRng,
        rand_chacha::ChaChaRng,
        solana_entry::entry::create_ticks,
        solana_hash::Hash,
        solana_keypair::Keypair,
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
                Hash::new_from_array(rand::thread_rng().gen()),
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
}
