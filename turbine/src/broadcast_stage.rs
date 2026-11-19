//! A stage to broadcast data from a leader node to validators
#![allow(clippy::rc_buffer)]
use {
    self::{
        broadcast_duplicates_run::{BroadcastDuplicatesConfig, BroadcastDuplicatesRun},
        broadcast_metrics::*,
        standard_broadcast_run::StandardBroadcastRun,
    },
    crate::{
        ShredReceiverAddresses, XdpSender,
        cluster_nodes::{ClusterNodes, ClusterNodesCache},
    },
    agave_votor::event::VotorEventSender,
    crossbeam_channel::{
        Receiver, RecvError, RecvTimeoutError, SendError, Sender, TrySendError, bounded,
    },
    itertools::Itertools,
    solana_clock::Slot,
    solana_gossip::{
        cluster_info::{ClusterInfo, ClusterInfoError},
        contact_info::Protocol,
    },
    solana_keypair::Keypair,
    solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
    solana_ledger::{
        blockstore::Blockstore,
        blockstore_db::{DBPinnableSlice, WriteBatch},
        leader_schedule_cache::LeaderScheduleCache,
        shred::{MAX_FEC_SETS_PER_SLOT, Shred},
    },
    solana_measure::measure::Measure,
    solana_metrics::inc_new_counter_error,
    solana_net_utils::{SocketAddrSpace, bind_to_unspecified},
    solana_poh::poh_recorder::WorkingBankMessage,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::MAX_LEADER_SCHEDULE_STAKES, bank_forks::BankForks},
    solana_streamer::sendmmsg::{SendPktsError, batch_send},
    solana_time_utils::{AtomicInterval, timestamp},
    std::{
        collections::HashMap,
        net::{SocketAddr, UdpSocket},
        sync::{
            Arc, Mutex, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    thiserror::Error,
};

pub mod broadcast_duplicates_run;
pub mod broadcast_metrics;
pub(crate) mod broadcast_utils;
pub(crate) mod standard_broadcast_run;

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

// Capacity in batches. One batch typically packs 1-2 FEC sets worth of bytes
// (see get_target_batch_bytes_default), so this is at least 4 slots' worth of
// broadcast traffic. Filling the channel means the consumer (blockstore record
// or socket transmit) has fallen at least 4 slots behind, which means we've
// been skipped already.
const BROADCAST_CHANNEL_CAPACITY: usize = MAX_FEC_SETS_PER_SLOT as usize * 4;

pub(crate) type RecordReceiver = Receiver<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>;
pub(crate) type TransmitReceiver = Receiver<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>;

/// Try to send into a bounded channel. If the channel is full,
/// log an error and fall back to a blocking send so shreds are not dropped.
/// A full channel means there is something very wrong with our node,
/// so we want a loud signal but must preserve shreds so the validator
/// does not start repairing its own blocks.
fn try_send_or_block<T>(
    sender: &Sender<T>,
    msg: T,
    channel_name: &str,
) -> std::result::Result<(), SendError<T>> {
    match sender.try_send(msg) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(msg)) => {
            error!("The channel to {channel_name} is full, node is resource starved!");
            sender.send(msg)
        }
        Err(TrySendError::Disconnected(msg)) => Err(SendError(msg)),
    }
}

/// Fan a batch of shreds out to both the blockstore record path and the
/// network transmit path. Each leg uses [`try_send_or_block`] so any full
/// channel is logged loudly but never drops shreds.
#[allow(clippy::type_complexity)]
pub(crate) fn dispatch_shreds(
    blockstore_sender: &Sender<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>,
    socket_sender: &Sender<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>,
    shreds: Arc<Vec<Shred>>,
    batch_info: Option<BroadcastShredBatchInfo>,
) -> std::result::Result<(), SendError<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>> {
    try_send_or_block(
        blockstore_sender,
        (shreds.clone(), batch_info.clone()),
        "blockstore",
    )?;
    try_send_or_block(socket_sender, (shreds, batch_info), "network egress")
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Blockstore(#[from] solana_ledger::blockstore::BlockstoreError),
    #[error(transparent)]
    ClusterInfo(#[from] solana_gossip::cluster_info::ClusterInfoError),
    #[error("Duplicate slot broadcast: {0}")]
    DuplicateSlotBroadcast(Slot),
    #[error("Invalid Merkle root, slot: {slot}, index: {index}")]
    InvalidMerkleRoot { slot: Slot, index: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Recv(#[from] crossbeam_channel::RecvError),
    #[error(transparent)]
    RecvTimeout(#[from] crossbeam_channel::RecvTimeoutError),
    #[error("Xdp channel full")]
    XdpChannelFull,
    #[error("Send")]
    Send,
    #[error(transparent)]
    Serialize(#[from] wincode::WriteError),
    #[error("Shred not found, slot: {slot}, index: {index}")]
    ShredNotFound { slot: Slot, index: u64 },
    #[error(transparent)]
    TransportError(#[from] solana_transaction_error::TransportError),
    #[error("Unknown last index, slot: {0}")]
    UnknownLastIndex(Slot),
    #[error("Unknown slot meta, slot: {0}")]
    UnknownSlotMeta(Slot),
    #[error("Window for slot {0} has already been skipped - newer window finalized")]
    WindowSkipped(Slot),
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
pub enum BroadcastStageType {
    Standard,
    BroadcastDuplicates(BroadcastDuplicatesConfig),
}

impl BroadcastStageType {
    #[allow(clippy::too_many_arguments)]
    pub fn new_broadcast_stage(
        &self,
        sock: Vec<UdpSocket>,
        cluster_info: Arc<ClusterInfo>,
        receiver: Receiver<WorkingBankMessage>,
        exit_sender: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        bank_forks: Arc<RwLock<BankForks>>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        shred_version: u16,
        xdp_sender: Option<XdpSender>,
        votor_event_sender: VotorEventSender,
        shred_receivers: Option<ShredReceivers>,
    ) -> BroadcastStage {
        let migration_status = bank_forks.read().unwrap().migration_status();
        match self {
            BroadcastStageType::Standard => BroadcastStage::new(
                sock,
                cluster_info,
                receiver,
                exit_sender,
                blockstore,
                bank_forks,
                StandardBroadcastRun::new(
                    shred_version,
                    migration_status,
                    votor_event_sender,
                    leader_schedule_cache,
                    shred_receivers,
                ),
                xdp_sender,
            ),

            BroadcastStageType::BroadcastDuplicates(config) => BroadcastStage::new(
                sock,
                cluster_info,
                receiver,
                exit_sender,
                blockstore,
                bank_forks,
                BroadcastDuplicatesRun::new(
                    shred_version,
                    config.clone(),
                    migration_status,
                    votor_event_sender,
                ),
                xdp_sender,
            ),
        }
    }
}

trait BroadcastRun {
    fn run<'db>(
        &mut self,
        keypair: &Keypair,
        blockstore: &'db Blockstore,
        pinnable_slice: &mut DBPinnableSlice<'db>,
        write_batch: &mut WriteBatch,
        receiver: &Receiver<WorkingBankMessage>,
        socket_sender: &Sender<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>,
        blockstore_sender: &Sender<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>,
    ) -> Result<()>;
    fn transmit(
        &mut self,
        receiver: &TransmitReceiver,
        cluster_info: &ClusterInfo,
        sock: BroadcastSocket,
        bank_forks: &RwLock<BankForks>,
    ) -> Result<()>;
    fn record<'db>(
        &mut self,
        receiver: &RecordReceiver,
        blockstore: &'db Blockstore,
        pinnable_slice: &mut DBPinnableSlice<'db>,
        write_batch: &mut WriteBatch,
    ) -> Result<()>;
}

// Implement a destructor for the BroadcastStage thread to signal it exited
// even on panics
struct Finalizer {
    exit_sender: Arc<AtomicBool>,
}

impl Finalizer {
    fn new(exit_sender: Arc<AtomicBool>) -> Self {
        Finalizer { exit_sender }
    }
}
// Implement a destructor for Finalizer.
impl Drop for Finalizer {
    fn drop(&mut self) {
        self.exit_sender.clone().store(true, Ordering::Relaxed);
    }
}

pub struct BroadcastStage {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl BroadcastStage {
    #[allow(clippy::too_many_arguments)]
    fn run(
        cluster_info: Arc<ClusterInfo>,
        blockstore: &Blockstore,
        receiver: &Receiver<WorkingBankMessage>,
        socket_sender: &Sender<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>,
        blockstore_sender: &Sender<(Arc<Vec<Shred>>, Option<BroadcastShredBatchInfo>)>,
        mut broadcast_stage_run: impl BroadcastRun,
    ) {
        let mut pinnable_slice = blockstore.new_pinnable_slice();
        let mut write_batch = blockstore.get_write_batch();
        loop {
            let res = broadcast_stage_run.run(
                &cluster_info.keypair(),
                blockstore,
                &mut pinnable_slice,
                &mut write_batch,
                receiver,
                socket_sender,
                blockstore_sender,
            );

            if Self::should_exit_on_error(res, "run") {
                break;
            }
        }
    }

    fn should_exit_on_error(result: Result<()>, name: &str) -> bool {
        let Err(err) = result else { return false };

        match err {
            Error::RecvTimeout(RecvTimeoutError::Disconnected)
            | Error::Send
            | Error::Recv(RecvError) => true,
            Error::RecvTimeout(RecvTimeoutError::Timeout)
            | Error::ClusterInfo(ClusterInfoError::NoPeers) => {
                // TODO: Why are the unit-tests throwing hundreds of these?
                false
            }
            _ => {
                inc_new_counter_error!("streamer-broadcaster-error", 1, 1);
                error!("{name} broadcaster error: {err:?}");
                false
            }
        }
    }

    /// Service to broadcast messages from the leader to layer 1 nodes.
    /// See `cluster_info` for network layer definitions.
    /// # Arguments
    /// * `sock` - Socket to send from.
    /// * `exit` - Boolean to signal system exit.
    /// * `cluster_info` - ClusterInfo structure
    /// * `window` - Cache of Shreds that we have broadcast
    /// * `receiver` - Receive channel for Shreds to be retransmitted to all the layer 1 nodes.
    /// * `exit_sender` - Set to true when this service exits, allows rest of Tpu to exit cleanly.
    ///
    /// Otherwise, when a Tpu closes, it only closes the stages that come after it. The stages
    /// that come before could be blocked on a receive, and never notice that they need to
    /// exit. Now, if any stage of the Tpu closes, it will lead to closing the WriteStage (b/c
    /// WriteStage is the last stage in the pipeline), which will then close Broadcast service,
    /// which will then close FetchStage in the Tpu, and then the rest of the Tpu,
    /// completing the cycle.
    #[allow(clippy::too_many_arguments)]
    fn new(
        socks: Vec<UdpSocket>,
        cluster_info: Arc<ClusterInfo>,
        receiver: Receiver<WorkingBankMessage>,
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        bank_forks: Arc<RwLock<BankForks>>,
        mut broadcast_stage_run: impl BroadcastRun + Send + 'static + Clone,
        xdp_sender: Option<XdpSender>,
    ) -> Self {
        let (socket_sender, socket_receiver) = bounded(BROADCAST_CHANNEL_CAPACITY);
        let (blockstore_sender, blockstore_receiver) = bounded(BROADCAST_CHANNEL_CAPACITY);
        let bs_run = broadcast_stage_run.clone();

        let thread_hdl = {
            let blockstore = blockstore.clone();
            let cluster_info = cluster_info.clone();
            Builder::new()
                .name("solBroadcast".to_string())
                .spawn(move || {
                    let _finalizer = Finalizer::new(exit);
                    Self::run(
                        cluster_info,
                        &blockstore,
                        &receiver,
                        &socket_sender,
                        &blockstore_sender,
                        bs_run,
                    )
                })
                .unwrap()
        };
        let mut thread_hdls = vec![thread_hdl];
        let num_broadcast_sockets_per_interface = socks.len() / cluster_info.bind_ip_addrs().len();
        let num_interfaces: usize = cluster_info.bind_ip_addrs().len();

        // Partition by interface
        // With 2 interfaces and the default of 4 sockets per interface, `sockets_by_interface` is:
        // sockets_by_interface = [[s0, s1, s2, s3], [s4, s5, s6, s7]]
        let mut it = socks.into_iter();
        let sockets_by_interface: Vec<Vec<UdpSocket>> = (0..num_interfaces)
            .map(|_| {
                it.by_ref()
                    .take(num_broadcast_sockets_per_interface)
                    .collect()
            })
            .collect();

        let mut iters: Vec<_> = sockets_by_interface
            .into_iter()
            .map(|sockets| sockets.into_iter())
            .collect();

        // Spawn `num_broadcast_sockets_per_interface` threads
        // Each thread gets a socket from each interface (i.e. 2 sockets per thread if multihomed w/ 2 interfaces)
        thread_hdls.extend((0..num_broadcast_sockets_per_interface).map(|_| {
            let mut group = Vec::with_capacity(num_interfaces);
            for it in &mut iters {
                group.push(it.next().expect("aligned lengths"));
            }

            let socket_receiver = socket_receiver.clone();
            let mut bs_transmit = broadcast_stage_run.clone();
            let cluster_info = cluster_info.clone();
            let bank_forks = bank_forks.clone();
            let xdp_sender = xdp_sender.clone();
            let run_transmit = move || loop {
                let sock_variant = match xdp_sender.as_ref() {
                    Some(xdp) => BroadcastSocket::Xdp(xdp),
                    None => {
                        let active_index = cluster_info.bind_ip_addrs().active_index();
                        let active_socket = &group[active_index];
                        BroadcastSocket::Udp(active_socket)
                    }
                };
                let res = bs_transmit.transmit(
                    &socket_receiver,
                    &cluster_info,
                    sock_variant,
                    &bank_forks,
                );

                if Self::should_exit_on_error(res, "solana-broadcaster-transmit") {
                    break;
                }
            };
            Builder::new()
                .name("solBroadcastTx".to_string())
                .spawn(run_transmit)
                .unwrap()
        }));

        // Blockstore::insert_threads() obtains and holds a write lock for the entire function.
        // Until this changes, only a single inserter thread is necessary.
        thread_hdls.push({
            let run_record = move || {
                let mut pinnable_slice = blockstore.new_pinnable_slice();
                let mut write_batch = blockstore.get_write_batch();
                loop {
                    let res = broadcast_stage_run.record(
                        &blockstore_receiver,
                        &blockstore,
                        &mut pinnable_slice,
                        &mut write_batch,
                    );

                    if Self::should_exit_on_error(res, "solana-broadcaster-record") {
                        break;
                    }
                }
            };
            Builder::new()
                .name("solBroadcastRec".to_string())
                .spawn(run_record)
                .unwrap()
        });
        Self { thread_hdls }
    }

    pub fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls.into_iter() {
            let _ = thread_hdl.join();
        }
        Ok(())
    }
}

fn update_peer_stats(
    cluster_nodes: &ClusterNodes<BroadcastStage>,
    last_datapoint_submit: &AtomicInterval,
) {
    if last_datapoint_submit.should_update(1000) {
        cluster_nodes.submit_metrics("cluster_nodes_broadcast", timestamp());
    }
}

#[derive(Clone, Copy)]
pub enum BroadcastSocket<'a> {
    Udp(&'a UdpSocket),
    Xdp(&'a XdpSender),
}

/// The XDP arm hands the sender one item per shred for up to this many
/// receivers. A transmit queue sends an item whole, so this bounds what one
/// item of copies can put ahead of a turbine packet. The top-level
/// README.md quotes this number.
const SHRED_RECEIVERS_PER_XDP_ITEM: usize = 32;

/// Extra destinations for this node's own leader shreds
/// (`--shred-receiver-address`): a copy of every shred goes to each address,
/// after the turbine packets. On the XDP arm the copies are best effort, see
/// [`broadcast_shreds`].
#[derive(Clone)]
pub struct ShredReceivers {
    addrs: Arc<ShredReceiverAddresses>,
    /// `addrs` in groups of [`SHRED_RECEIVERS_PER_XDP_ITEM`].
    xdp_items: Arc<[Arc<[SocketAddr]>]>,
    /// Bound to the unspecified address and an ephemeral port rather than
    /// `--bind-address`, so on the UDP arm the kernel routing table picks the
    /// outbound interface per receiver.
    socket: Arc<UdpSocket>,
}

impl ShredReceivers {
    /// `None` when no address is configured: nothing is bound and
    /// `broadcast_shreds` sends nothing extra.
    pub fn new(addrs: ShredReceiverAddresses) -> Option<Self> {
        if addrs.is_empty() {
            return None;
        }
        let socket = bind_to_unspecified().expect("bind the shred receiver socket");
        // Let copies to a multicast group cross routers, like the other
        // clients with this flag.
        socket
            .set_multicast_ttl_v4(64)
            .expect("set the multicast TTL of the shred receiver socket");
        Some(Self {
            xdp_items: addrs
                .chunks(SHRED_RECEIVERS_PER_XDP_ITEM)
                .map(Arc::from)
                .collect(),
            addrs: Arc::new(addrs),
            socket: Arc::new(socket),
        })
    }
}

/// Returns the pubkey of the next leader after `slot`, or None if it is us.
fn next_broadcast_leader_pubkey(
    leader_schedule_cache: &LeaderScheduleCache,
    working_bank: &solana_runtime::bank::Bank,
    my_pubkey: &Pubkey,
    slot: Slot,
) -> Option<Pubkey> {
    let next_leader_slot = slot.saturating_add(NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot);
    leader_schedule_cache
        .slot_leader_at(next_leader_slot, Some(working_bank))
        .map(|next_leader| next_leader.id)
        .filter(|next_leader_id| next_leader_id != my_pubkey)
}

/// Broadcasts shreds from the leader (i.e. this node) to the root of the
/// turbine retransmit tree for each shred, then sends one copy of every
/// shred to each configured `shred_receivers` address.
///
/// The turbine packets leave through `socket`. On the UDP arm the copies
/// leave through the receivers' own socket; on the XDP arm the sender routes
/// them like the turbine packets, so they must be reachable through the XDP
/// interface. There the copies are best effort: a transmit queue sends them
/// only when it has nothing else to send, a bounded number of frames at a
/// time, so the turbine packets wait behind a bounded number of copy frames
/// whatever the number of receivers, and the copies are dropped when the
/// queue's low priority channel is full. The copies of data shreds go ahead
/// of those of coding shreds and wait apart from them, so a queue that
/// cannot send every copy drops copies of coding shreds first and sends the
/// rest of them late. The copies are counted apart from the turbine packets,
/// and a receiver that cannot be reached never fails the turbine broadcast.
#[allow(clippy::too_many_arguments)]
pub fn broadcast_shreds(
    socket: BroadcastSocket,
    shreds: &[Shred],
    cluster_nodes_cache: &ClusterNodesCache<BroadcastStage>,
    last_datapoint_submit: &AtomicInterval,
    transmit_stats: &mut TransmitShredsStats,
    cluster_info: &ClusterInfo,
    bank_forks: &RwLock<BankForks>,
    leader_schedule_cache: &LeaderScheduleCache,
    socket_addr_space: &SocketAddrSpace,
    shred_receivers: Option<&ShredReceivers>,
) -> Result<()> {
    let mut result = Ok(());
    // Compute destinations for each of the shreds to be sent
    let (root_bank, working_bank) = {
        let bank_forks = bank_forks.read().unwrap();
        (bank_forks.root_bank(), bank_forks.working_bank())
    };
    let my_pubkey = cluster_info.id();
    // Helper to find the next leader's pubkey (None if it is us)
    let find_next_leader = |slot: Slot| -> Option<Pubkey> {
        next_broadcast_leader_pubkey(leader_schedule_cache, &working_bank, &my_pubkey, slot)
    };

    let grouped_shreds = shreds.iter().chunk_by(|shred| shred.slot());
    let packets = grouped_shreds.into_iter().flat_map(|(slot, shreds)| {
        let cluster_nodes = cluster_nodes_cache.get(slot, &root_bank, &working_bank, cluster_info);
        update_peer_stats(&cluster_nodes, last_datapoint_submit);
        let maybe_next_leader_udp = find_next_leader(slot).and_then(|leader| {
            cluster_info
                .lookup_contact_info(&leader, |node| {
                    node.tvu(Protocol::UDP)
                        .filter(|addr| !addr.is_ipv6() && socket_addr_space.check(addr))
                })
                .flatten()
        });
        shreds.flat_map(move |shred| {
            let key = shred.id();
            let maybe_standard_broadcast_peer = cluster_nodes
                .get_broadcast_peer(&key)
                .and_then(|ci| ci.tvu(Protocol::UDP))
                .filter(|addr| !addr.is_ipv6() && socket_addr_space.check(addr));
            // only send to next leader if not standard broadcast peer
            let maybe_next_leader =
                maybe_next_leader_udp.filter(|addr| Some(*addr) != maybe_standard_broadcast_peer);
            [maybe_next_leader, maybe_standard_broadcast_peer]
                .into_iter()
                .filter_map(move |tvu_addr: Option<SocketAddr>| {
                    tvu_addr.map(|addr| (shred.payload(), addr))
                })
        })
    });
    let mut num_packets = 0;
    match socket {
        BroadcastSocket::Udp(s) => {
            let packets: Vec<_> = packets.collect();
            let batch_len = packets.len();
            num_packets += batch_len;
            let mut send_mmsg_time = Measure::start("send_mmsg");
            match batch_send(s, packets) {
                Ok(num_sent) => transmit_stats.dropped_packets_udp += batch_len - num_sent,
                Err(SendPktsError::IoError(ioerr)) => result = Err(Error::Io(ioerr)),
            }
            send_mmsg_time.stop();
            transmit_stats.send_mmsg_elapsed += send_mmsg_time.as_us();
            // One copy of every shred per configured receiver, after the
            // turbine packets.
            if let Some(receivers) = shred_receivers {
                let mut mirrored = Vec::with_capacity(receivers.addrs.len() * shreds.len());
                mirrored.extend(
                    receivers
                        .addrs
                        .iter()
                        .flat_map(|addr| shreds.iter().map(move |shred| (shred.payload(), *addr))),
                );
                let mirrored_len = mirrored.len();
                transmit_stats.mirrored_packets += mirrored_len;
                match batch_send(&receivers.socket, mirrored) {
                    Ok(num_sent) => {
                        transmit_stats.dropped_packets_mirrored += mirrored_len - num_sent
                    }
                    Err(SendPktsError::IoError(ioerr)) => {
                        log::warn!("shred receiver send failed: {ioerr:?}");
                        transmit_stats.dropped_packets_mirrored += mirrored_len;
                    }
                }
            }
        }
        BroadcastSocket::Xdp(s) => {
            for (idx, (payload, addr)) in packets.enumerate() {
                num_packets += 1;
                if let Err(e) = s.try_send(idx, addr, payload.bytes.clone()) {
                    log::warn!("xdp channel full: {e:?}");
                    transmit_stats.dropped_packets_xdp += 1;
                    result = Err(Error::XdpChannelFull);
                }
            }
            // The groups of receivers are the outer loop and `idx` keeps
            // counting, so the copies for each group spread over the TX
            // queues like the turbine packets, and a low priority channel
            // that fills up refuses the copies for the last groups first.
            let copies = shred_receivers.into_iter().flat_map(|receivers| {
                receivers
                    .xdp_items
                    .iter()
                    .flat_map(|addrs| shreds.iter().map(move |shred| (shred, addrs)))
            });
            for (idx, (shred, addrs)) in copies.enumerate() {
                transmit_stats.mirrored_packets += addrs.len();
                let (idx, payload) = (num_packets + idx, shred.payload().bytes.clone());
                // A receiver needs no coding shred of a set whose data shreds
                // all arrive, so the copies of coding shreds give way.
                let sent = if shred.is_data() {
                    s.try_send_low_priority(idx, Arc::clone(addrs), payload)
                } else {
                    s.try_send_lowest_priority(idx, Arc::clone(addrs), payload)
                };
                if sent.is_err() {
                    transmit_stats.dropped_packets_mirrored += addrs.len();
                    if shred.is_data() {
                        transmit_stats.dropped_packets_mirrored_data += addrs.len();
                    }
                }
            }
        }
    }

    transmit_stats.total_packets += num_packets;
    result
}

impl<T> From<crossbeam_channel::SendError<T>> for Error {
    fn from(_: crossbeam_channel::SendError<T>) -> Error {
        Error::Send
    }
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        agave_votor_messages::migration::MigrationStatus,
        crossbeam_channel::bounded,
        solana_entry::{entry::create_ticks, recorder_message::RecorderMessage},
        solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo, node::Node},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_leader_schedule::{FixedSchedule, LeaderSchedule, SlotLeader},
        solana_ledger::{
            blockstore::Blockstore,
            genesis_utils::{GenesisConfigInfo, create_genesis_config},
            get_tmp_ledger_path_auto_delete,
            leader_schedule_cache::LeaderScheduleCache,
            shred::{ProcessShredsStats, Shredder},
        },
        solana_net_utils::sockets::bind_to_localhost_unique,
        solana_runtime::bank::Bank,
        solana_signer::Signer,
        std::{
            collections::HashSet,
            num::NonZeroUsize,
            path::Path,
            sync::{Arc, atomic::AtomicBool},
            thread::sleep,
        },
    };

    fn fixed_leader_schedule_cache(
        slot_leaders: Vec<SlotLeader>,
        repeat: usize,
        bank: &Bank,
    ) -> LeaderScheduleCache {
        let mut leader_schedule_cache = LeaderScheduleCache::new_from_bank(bank);
        leader_schedule_cache.set_fixed_leader_schedule(Some(FixedSchedule {
            leader_schedule: Arc::new(LeaderSchedule::new_from_schedule(
                slot_leaders,
                NonZeroUsize::new(repeat).unwrap(),
            )),
        }));
        leader_schedule_cache
    }

    #[test]
    fn test_next_broadcast_leader_pubkey() {
        let leader_a = Keypair::new();
        let leader_b = Keypair::new();
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let leader_schedule_cache = fixed_leader_schedule_cache(
            vec![
                SlotLeader {
                    id: leader_a.pubkey(),
                    vote_address: Pubkey::new_unique(),
                },
                SlotLeader {
                    id: leader_b.pubkey(),
                    vote_address: Pubkey::new_unique(),
                },
            ],
            NUM_CONSECUTIVE_LEADER_SLOTS.get(),
            &bank,
        );

        assert_eq!(
            next_broadcast_leader_pubkey(&leader_schedule_cache, &bank, &leader_a.pubkey(), 0,),
            Some(leader_b.pubkey())
        );
        assert_eq!(
            next_broadcast_leader_pubkey(
                &leader_schedule_cache,
                &bank,
                &leader_b.pubkey(),
                NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot,
            ),
            Some(leader_a.pubkey())
        );
        assert_eq!(
            next_broadcast_leader_pubkey(
                &leader_schedule_cache,
                &bank,
                &leader_a.pubkey(),
                NUM_CONSECUTIVE_LEADER_SLOTS.get() as Slot,
            ),
            None,
        );
    }

    struct MockBroadcastStage {
        blockstore: Arc<Blockstore>,
        broadcast_service: BroadcastStage,
        bank: Arc<Bank>,
    }

    fn setup_dummy_broadcast_service(
        leader_keypair: Arc<Keypair>,
        ledger_path: &Path,
        entry_receiver: Receiver<WorkingBankMessage>,
    ) -> MockBroadcastStage {
        // Make the database ledger
        let blockstore = Arc::new(Blockstore::open(ledger_path).unwrap());

        // Make the leader node and scheduler
        let leader_info = Node::new_localhost_with_pubkey(&leader_keypair.pubkey());

        // Make a node to broadcast to
        let buddy_keypair = Keypair::new();
        let broadcast_buddy = Node::new_localhost_with_pubkey(&buddy_keypair.pubkey());

        // Fill the cluster_info with the buddy's info
        let cluster_info = ClusterInfo::new(
            leader_info.info.clone(),
            leader_keypair,
            SocketAddrSpace::Unspecified,
        );
        cluster_info.insert_info(broadcast_buddy.info);
        let cluster_info = Arc::new(cluster_info);

        let exit_sender = Arc::new(AtomicBool::new(false));

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let root_bank = Bank::new_for_tests(&genesis_config);
        root_bank.set_tick_height(root_bank.max_tick_height());
        Bank::calculate_and_set_block_id_for_dcou(&root_bank);
        let bank_forks = BankForks::new_rw_arc(root_bank);
        let root_bank = bank_forks.read().unwrap().root_bank();
        let child_bank =
            Bank::new_from_parent(root_bank.clone(), *root_bank.leader(), root_bank.slot() + 1);
        let bank = bank_forks
            .write()
            .unwrap()
            .insert(child_bank)
            .clone_without_scheduler();

        // Create votor event channel for test
        let (votor_event_sender, _votor_event_receiver) = bounded(100);
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(
            &bank_forks.read().unwrap().root_bank(),
        ));

        // Start up the broadcast stage
        let broadcast_service = BroadcastStage::new(
            leader_info.sockets.broadcast,
            cluster_info,
            entry_receiver,
            exit_sender,
            blockstore.clone(),
            bank_forks,
            StandardBroadcastRun::new(
                0,
                Arc::new(MigrationStatus::default()),
                votor_event_sender,
                leader_schedule_cache,
                None,
            ),
            None,
        );

        MockBroadcastStage {
            blockstore,
            broadcast_service,
            bank,
        }
    }

    #[test]
    fn test_shred_receivers_none_when_unconfigured() {
        assert!(ShredReceivers::new(ShredReceiverAddresses::default()).is_none());
    }

    /// Returns a `broadcast_shreds` call for the leader of a cluster whose
    /// only other node, if any, is staked and has `turbine_peer` as its TVU
    /// address, and the payloads of the shreds the call broadcasts.
    fn leader_broadcast(
        turbine_peer: Option<SocketAddr>,
    ) -> (
        impl Fn(BroadcastSocket, &ShredReceivers) -> Result<TransmitShredsStats>,
        HashSet<Vec<u8>>,
    ) {
        let leader_keypair = Arc::new(Keypair::new());
        let leader_info = Node::new_localhost_with_pubkey(&leader_keypair.pubkey());
        let cluster_info = ClusterInfo::new(
            leader_info.info,
            leader_keypair.clone(),
            SocketAddrSpace::Unspecified,
        );
        let GenesisConfigInfo {
            genesis_config,
            validator_pubkey,
            ..
        } = create_genesis_config(10_000);
        if let Some(turbine_peer) = turbine_peer {
            let mut peer_info = ContactInfo::new_localhost(&validator_pubkey, timestamp());
            peer_info.set_tvu(Protocol::UDP, turbine_peer).unwrap();
            cluster_info.insert_info(peer_info);
        }
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let root_bank = bank_forks.read().unwrap().root_bank();
        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&root_bank);
        let cluster_nodes_cache =
            ClusterNodesCache::<BroadcastStage>::new(1, Duration::from_secs(60));

        let shredder = Shredder::new(root_bank.slot(), root_bank.parent_slot(), 0, 0).unwrap();
        let entries = create_ticks(1, 0, Hash::default());
        let (mut shreds, mut coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
            &leader_keypair,
            &entries,
            true,
            Hash::default(),
            0,
            0,
            &mut ProcessShredsStats::default(),
        );
        // Four shreds of each kind, data first as a leader broadcasts them:
        // few enough to keep the copies well inside a default socket receive
        // buffer, so the tests do not depend on the host's rmem_default.
        assert!(shreds.len() > 4 && coding_shreds.len() > 4);
        shreds.truncate(4);
        coding_shreds.truncate(4);
        shreds.append(&mut coding_shreds);
        let payloads = shreds
            .iter()
            .map(|shred| shred.payload().to_vec())
            .collect();

        let broadcast = move |socket: BroadcastSocket, receivers: &ShredReceivers| {
            let mut transmit_stats = TransmitShredsStats::default();
            broadcast_shreds(
                socket,
                &shreds,
                &cluster_nodes_cache,
                &AtomicInterval::default(),
                &mut transmit_stats,
                &cluster_info,
                &bank_forks,
                &leader_schedule_cache,
                &SocketAddrSpace::Unspecified,
                Some(receivers),
            )
            .map(|()| transmit_stats)
        };
        (broadcast, payloads)
    }

    #[test]
    fn test_shred_receiver_addresses_get_every_shred_once_on_udp() {
        let bind_receiver = || {
            let socket = bind_to_localhost_unique().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            socket
        };
        let receiver_sockets = [bind_receiver(), bind_receiver()];
        let receivers = ShredReceivers::new(
            ShredReceiverAddresses::new(
                receiver_sockets
                    .iter()
                    .map(|socket| socket.local_addr().unwrap()),
            )
            .unwrap(),
        )
        .unwrap();
        let turbine_socket = bind_to_localhost_unique().unwrap();

        // Only this node is in the cluster, so turbine has no destinations.
        let (broadcast, payloads) = leader_broadcast(None);
        let transmit_stats = broadcast(BroadcastSocket::Udp(&turbine_socket), &receivers).unwrap();
        assert_eq!(transmit_stats.total_packets, 0);
        assert_eq!(
            transmit_stats.mirrored_packets,
            payloads.len() * receiver_sockets.len()
        );
        assert_eq!(transmit_stats.dropped_packets_mirrored, 0);

        // The socket is bound to the unspecified address, so the kernel picks
        // the source IP; the port identifies it.
        let source_port = receivers.socket.local_addr().unwrap().port();
        for receiver in &receiver_sockets {
            let mut buf = [0u8; 2048];
            let mut got = HashSet::new();
            for _ in 0..payloads.len() {
                let (len, from) = receiver.recv_from(&mut buf).unwrap();
                assert_eq!(from.port(), source_port);
                assert!(got.insert(buf[..len].to_vec()));
            }
            assert_eq!(got, payloads);
            // Exactly once: nothing more arrives.
            assert!(receiver.recv_from(&mut buf).is_err());
        }
    }

    /// On the XDP arm the turbine packets and the copies wait apart on a TX
    /// queue: the copies are low priority, one item per shred for up to
    /// SHRED_RECEIVERS_PER_XDP_ITEM receivers, those of data shreds ahead of
    /// those of coding shreds, and a queue with no room left drops copies,
    /// counted by kind, without failing the turbine broadcast.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_shred_receiver_copies_are_low_priority_on_xdp() {
        use {
            agave_xdp::{
                transmitter::XdpSender as QueuesSender,
                tx_loop::{Receiver as _, TxPacket as _},
            },
            solana_ledger::shred::{ShredType, wire::get_shred_type},
            std::net::{Ipv4Addr, SocketAddrV4},
        };
        const QUEUES: usize = 2;
        // Two items per shred: a full one and one more receiver.
        const RECEIVERS: usize = SHRED_RECEIVERS_PER_XDP_ITEM + 1;

        let localhost = |port| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let receiver_addrs: Vec<_> = (0..RECEIVERS).map(|i| localhost(9001 + i as u16)).collect();
        let receivers =
            ShredReceivers::new(ShredReceiverAddresses::new(receiver_addrs.clone()).unwrap())
                .unwrap();
        // The cluster's one staked node is the turbine destination of every
        // shred.
        let turbine_peer = localhost(9000);
        let (broadcast, payloads) = leader_broadcast(Some(turbine_peer));
        let copies_per_call = payloads.len() * RECEIVERS;
        let is_coding = |payload: &[u8]| get_shred_type(payload).unwrap() == ShredType::Code;
        // Half the shreds of a call are data shreds.
        assert_eq!(
            payloads.iter().filter(|payload| is_coding(payload)).count() * 2,
            payloads.len()
        );
        // Each channel holds one call's share of the copies of each kind and
        // no more.
        let items_per_call = payloads.len() * RECEIVERS.div_ceil(SHRED_RECEIVERS_PER_XDP_ITEM);
        let items_of_a_kind_per_queue = items_per_call / 2 / QUEUES;
        let (sender, queues) = QueuesSender::with_receivers(QUEUES, items_of_a_kind_per_queue);
        let sender = XdpSender::new(sender, SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8001));
        // What the TX loops would send first: only turbine packets.
        let turbine_packets = || {
            let mut sent = HashSet::new();
            for queue in &queues {
                while let Ok(packet) = queue.try_recv() {
                    assert_eq!(packet.dst_addrs().as_ref(), [turbine_peer]);
                    sent.insert(packet.payload().to_vec());
                }
            }
            sent
        };

        // The copies of the first call fill the low priority channels, so
        // those of the second are dropped, and counted for every receiver,
        // those of data shreds apart. The turbine packets go out both times.
        for dropped in [0, copies_per_call] {
            let transmit_stats = broadcast(BroadcastSocket::Xdp(&sender), &receivers).unwrap();
            assert_eq!(transmit_stats.total_packets, payloads.len());
            assert_eq!(transmit_stats.dropped_packets_xdp, 0);
            assert_eq!(transmit_stats.mirrored_packets, copies_per_call);
            assert_eq!(transmit_stats.dropped_packets_mirrored, dropped);
            assert_eq!(transmit_stats.dropped_packets_mirrored_data, dropped / 2);
            assert_eq!(turbine_packets(), payloads);
        }

        // The copies of the first call: every shred once for every receiver.
        let mut copies = HashSet::new();
        for queue in &queues {
            let mut queue_copies = 0;
            let mut items = Vec::new();
            while let Some(packet) = queue.try_recv_low_priority(|| true) {
                let addrs = packet.dst_addrs().as_ref();
                assert!(addrs.len() <= SHRED_RECEIVERS_PER_XDP_ITEM);
                queue_copies += addrs.len();
                items.push((is_coding(packet.payload()), addrs[0]));
                for addr in addrs {
                    assert!(copies.insert((packet.payload().to_vec(), *addr)));
                }
            }
            // Like the turbine packets, the copies spread evenly over the TX
            // queues.
            assert_eq!(queue_copies, copies_per_call / QUEUES);
            // A queue sends the copies of data shreds before those of coding
            // shreds. Within each kind the receiver addresses ascend, so this
            // is the order given: the groups listed last are queued last, and
            // a channel that fills up refuses them first.
            assert!(items.is_sorted());
        }
        let expected: HashSet<_> = payloads
            .iter()
            .flat_map(|payload| {
                receiver_addrs
                    .iter()
                    .map(move |addr| (payload.clone(), *addr))
            })
            .collect();
        assert_eq!(copies, expected);

        // A queue that has sent the copies of data shreds has room for those
        // of the next call, whatever copies of coding shreds still wait on
        // it: only the copies of coding shreds are dropped.
        broadcast(BroadcastSocket::Xdp(&sender), &receivers).unwrap();
        assert_eq!(turbine_packets(), payloads);
        for queue in &queues {
            for _ in 0..items_of_a_kind_per_queue {
                let packet = queue.try_recv_low_priority(|| true).unwrap();
                assert!(!is_coding(packet.payload()));
            }
        }
        let transmit_stats = broadcast(BroadcastSocket::Xdp(&sender), &receivers).unwrap();
        assert_eq!(transmit_stats.mirrored_packets, copies_per_call);
        assert_eq!(transmit_stats.dropped_packets_mirrored, copies_per_call / 2);
        assert_eq!(transmit_stats.dropped_packets_mirrored_data, 0);
    }

    #[test]
    fn test_broadcast_ledger() {
        agave_logger::setup();
        let ledger_path = get_tmp_ledger_path_auto_delete!();

        // Create the leader scheduler
        let leader_keypair = Arc::new(Keypair::new());

        let (entry_sender, entry_receiver) = bounded(1024);
        let broadcast_service =
            setup_dummy_broadcast_service(leader_keypair, ledger_path.path(), entry_receiver);
        let start_tick_height;
        let max_tick_height;
        let ticks_per_slot;
        let num_expected_entries;
        let slot;
        {
            let bank = broadcast_service.bank;
            start_tick_height = bank.tick_height();
            max_tick_height = bank.max_tick_height();
            ticks_per_slot = bank.ticks_per_slot();
            num_expected_entries = (max_tick_height - start_tick_height) as usize;
            slot = bank.slot();
            let ticks = create_ticks(max_tick_height - start_tick_height, 0, Hash::default());
            for (i, tick) in ticks.into_iter().enumerate() {
                entry_sender
                    .send((
                        bank.clone(),
                        (
                            RecorderMessage::Entry(tick),
                            start_tick_height + i as u64 + 1,
                        ),
                    ))
                    .expect("Expect successful send to broadcast service");
            }
        }

        trace!(
            "[broadcast_ledger] max_tick_height: {max_tick_height}, start_tick_height: \
             {start_tick_height}, ticks_per_slot: {ticks_per_slot}",
        );

        let mut entries = vec![];
        for _ in 0..10 {
            entries = broadcast_service
                .blockstore
                .get_slot_entries(slot, 0)
                .expect("Expect entries to be present");
            if entries.len() >= num_expected_entries {
                break;
            }
            sleep(Duration::from_millis(1000));
        }

        drop(entry_sender);
        broadcast_service
            .broadcast_service
            .join()
            .expect("Expect successful join of broadcast service");
        assert_eq!(entries.len(), num_expected_entries);
    }
}
