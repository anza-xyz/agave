//! The `cluster_info` module defines a data structure that is shared by all the nodes in the network over
//! a gossip control plane.  The goal is to share small bits of off-chain information and detect and
//! repair partitions.
//!
//! This CRDT only supports a very limited set of types.  A map of Pubkey -> Versioned Struct.
//! The last version is always picked during an update.
//!
//! The network is arranged in layers:
//!
//! * layer 0 - Leader.
//! * layer 1 - As many nodes as we can fit
//! * layer 2 - Everyone else, if layer 1 is `2^10`, layer 2 should be able to fit `2^20` number of nodes.
//!
//! Bank needs to provide an interface for us to query the stake weight

use {
    crate::{
        cluster_info_metrics::{Counter, GossipStats, ScopedTimer, TimedGuard},
        contact_info::{self, ContactInfo, ContactInfoQuery, Error as ContactInfoError},
        crds::{self, Crds, Cursor, GossipRoute},
        crds_data::{self, CrdsData, EpochSlotsIndex, LowestSlot, MAX_VOTES, SnapshotHashes, Vote},
        crds_filter::{GossipFilterDirection, should_retain_crds_value},
        crds_gossip::CrdsGossip,
        crds_gossip_error::CrdsGossipError,
        crds_gossip_pull::{
            self, CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS, CRDS_GOSSIP_PURGE_DURATION, CrdsFilter,
            CrdsTimeouts, ProcessPullStats, PullRequest, get_max_bloom_filter_bytes,
        },
        crds_value::{CrdsValue, CrdsValueLabel},
        duplicate_shred::{self, DuplicateShred},
        epoch_slots::EpochSlots,
        gossip_command::GossipCommand,
        gossip_error::GossipError,
        gossip_identity::{ContactInfoChanged, GossipIdentity},
        gossip_ingress::ValidatedGossipMessage,
        gossip_policy::GossipPolicy,
        ping_pong::Pong,
        protocol::{
            DUPLICATE_SHRED_MAX_PAYLOAD_SIZE, MAX_INCREMENTAL_SNAPSHOT_HASHES,
            MAX_PRUNE_DATA_NODES, PULL_RESPONSE_MAX_PAYLOAD_SIZE,
            PULL_RESPONSE_MIN_SERIALIZED_SIZE, PUSH_MESSAGE_MAX_PAYLOAD_SIZE, Ping, PingCache,
            Protocol, PruneData, deserialize_protocol, split_gossip_messages,
        },
        sigverify_cache::SigVerifyCache,
        weighted_shuffle::WeightedShuffle,
    },
    agave_votor_messages::migration::MigrationStatus,
    arc_swap::ArcSwapOption,
    crossbeam_channel::{Sender, TrySendError, bounded},
    itertools::{Either, Itertools},
    rand::{CryptoRng, Rng, prelude::IndexedMutRandom},
    rayon::{ThreadPool, prelude::*},
    solana_clock::{DEFAULT_SLOTS_PER_EPOCH, Slot},
    solana_hash::Hash,
    solana_keypair::{Keypair, signable::Signable},
    solana_ledger::shred::Shred,
    solana_net_utils::{
        PortRange, SocketAddrSpace, VALIDATOR_PORT_RANGE, bind_in_range,
        multihomed_sockets::BindIpAddrs,
        sockets::{bind_gossip_port_in_range, bind_to_localhost_unique},
        token_bucket::{KeyedRateLimiter, TokenBucket},
    },
    solana_perf::{
        data_budget::DataBudget,
        packet::{Packet, PacketBatch, PacketBatchRecycler, PacketRef, RecycledPacketBatch},
    },
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_signer::Signer,
    solana_streamer::{
        packet,
        streamer::{ChannelSend, PacketBatchReceiver},
    },
    solana_time_utils::timestamp,
    solana_transaction::Transaction,
    solana_vote::vote_parser,
    std::{
        borrow::Borrow,
        collections::{HashMap, HashSet},
        fmt::{Debug, Write as _},
        fs::{self, File},
        io::{BufReader, BufWriter, Write},
        iter::repeat,
        net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket},
        num::NonZeroUsize,
        ops::{Div, Range},
        path::{Path, PathBuf},
        rc::Rc,
        result::Result,
        sync::{
            Arc, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
    thiserror::Error,
};

/// milliseconds we sleep for between gossip rounds
pub const GOSSIP_SLEEP_MILLIS: u64 = 100;
/// Maximum UDP packet batches validated in one pass.
const MAX_PACKET_BATCHES_PER_VALIDATION: usize = 1024;
const MIN_PARALLEL_GOSSIP_PACKETS: usize = 16;
/// Channel capacity for gossip channels.
///
/// A hard limit on incoming gossip messages.
///
/// 262,144 packets with saturated packet batches (64 packets).
///
/// 114,688 packets with observed average packet batch size (28 packets),
/// putting this within reasonable range of previous hard limit
/// of `MAX_GOSSIP_TRAFFIC` (103,896).
pub(crate) const GOSSIP_CHANNEL_CAPACITY: usize = 4096; // 2^12
const GOSSIP_PING_CACHE_CAPACITY: usize = 126976;
pub(crate) const GOSSIP_PING_CACHE_TTL: Duration = Duration::from_secs(1280);
/// Per-entry Pong wait timeout is drawn uniformly from this range (in milliseconds).
pub(crate) const GOSSIP_PING_CACHE_OUTSTANDING_PING_TIMEOUT_MS: Range<u64> = 1000..2000;
// Per-IP scan budget for incoming pull requests; validators are assumed not
// to share IPs. Mirrors the ping-pong cache capacity.
const GOSSIP_PULL_SCAN_BUDGET_CACHE_CAPACITY: usize = GOSSIP_PING_CACHE_CAPACITY;
const GOSSIP_PULL_SCAN_BUDGET_CAPACITY: u64 = 16 * crds_gossip_pull::MIN_NUM_BLOOM_ITEMS as u64;
const GOSSIP_PULL_SCAN_BUDGET_REFILL_PER_SEC: u64 =
    4 * crds_gossip_pull::MIN_NUM_BLOOM_ITEMS as u64;
const GOSSIP_PULL_SCAN_BUDGET_SHARD_COUNT: usize = 64;

#[inline]
fn pull_request_scan_cost(scan_entries: usize, bloom_hash_count: usize) -> u64 {
    let scan_entries = u64::try_from(scan_entries).unwrap_or(u64::MAX).max(1);
    let bloom_hash_count = u64::try_from(bloom_hash_count)
        .unwrap_or(u64::MAX)
        .max(crds_gossip_pull::KEYS as u64);
    scan_entries.saturating_mul(bloom_hash_count)
}

pub const DEFAULT_CONTACT_DEBUG_INTERVAL_MILLIS: u64 = 10_000;
pub const DEFAULT_CONTACT_SAVE_INTERVAL_MILLIS: u64 = 60_000;
// Limit number of unique pubkeys in the crds table.
pub(crate) const CRDS_UNIQUE_PUBKEY_CAPACITY: usize = 8192;

// Must have at least one socket to monitor the TVU port
pub const MINIMUM_NUM_TVU_RECEIVE_SOCKETS: NonZeroUsize = NonZeroUsize::new(1).unwrap();
pub const DEFAULT_NUM_TVU_RECEIVE_SOCKETS: NonZeroUsize = MINIMUM_NUM_TVU_RECEIVE_SOCKETS;
pub const MINIMUM_NUM_TVU_RETRANSMIT_SOCKETS: NonZeroUsize = NonZeroUsize::new(1).unwrap();
pub const DEFAULT_NUM_TVU_RETRANSMIT_SOCKETS: NonZeroUsize = NonZeroUsize::new(12).unwrap();

/// Number of QUIC endpoints for the votor datagram transport.
pub const DEFAULT_NUM_VOTOR_QUIC_ENDPOINTS: NonZeroUsize = NonZeroUsize::new(1).unwrap();

// Contact-info save/restore handles trusted local data, so disable wincode's
// default 4 MiB preallocation limit (which defends untrusted wire input).
type ContactInfoFileConfig =
    wincode::config::Configuration<true, { wincode::config::PREALLOCATION_SIZE_LIMIT_DISABLED }>;

#[derive(Debug, PartialEq, Eq, Error)]
pub enum ClusterInfoError {
    #[error("NoPeers")]
    NoPeers,
    #[error("NoLeader")]
    NoLeader,
    #[error("BadContactInfo")]
    BadContactInfo,
    #[error("BadGossipAddress")]
    BadGossipAddress,
    #[error("TooManyIncrementalSnapshotHashes")]
    TooManyIncrementalSnapshotHashes,
}

pub struct ClusterInfo {
    /// The network
    gossip: CrdsGossip,
    identity: GossipIdentity,
    /// Network entrypoints
    entrypoints: RwLock<Vec<ContactInfo>>,
    /// Additional pubkeys to preserve during CRDS table trimming.
    known_validators: OnceLock<HashSet<Pubkey>>,
    outbound_budget: DataBudget,
    ping_cache: Mutex<PingCache>,
    pull_request_budget: KeyedRateLimiter<IpAddr>,
    pub(crate) stats: GossipStats,
    contact_debug_interval: u64, // milliseconds, 0 = disabled
    contact_save_interval: u64,  // milliseconds, 0 = disabled
    contact_info_path: PathBuf,
    socket_addr_space: SocketAddrSpace,
    bind_ip_addrs: Arc<BindIpAddrs>,
    sigverify_cache: SigVerifyCache,
    /// Alpenglow migration status
    migration_status: OnceLock<Arc<MigrationStatus>>,
    command_endpoint: ArcSwapOption<Sender<GossipCommand>>,
    /// Prevents inline fallback from racing the engine.
    writer_lease: Mutex<()>,
}

impl ClusterInfo {
    pub(crate) fn submit_stats(&self, stakes: &HashMap<Pubkey, u64>) {
        crate::cluster_info_metrics::submit_gossip_stats(&self.stats, &self.gossip, stakes);
    }

    pub(crate) fn attach_command_endpoint(
        &self,
        sender: Sender<GossipCommand>,
    ) -> Arc<Sender<GossipCommand>> {
        let sender = Arc::new(sender);
        let previous = self
            .command_endpoint
            .compare_and_swap(&None::<Arc<_>>, Some(Arc::clone(&sender)));
        assert!(previous.is_none(), "gossip engine already attached");
        sender
    }

    pub(crate) fn detach_command_endpoint(&self, sender: &Arc<Sender<GossipCommand>>) {
        let _ = self.command_endpoint.compare_and_swap(sender, None);
    }

    pub(crate) fn acquire_writer_lease(&self) -> MutexGuard<'_, ()> {
        self.writer_lease.lock().unwrap()
    }

    /// Sends to the engine, or applies inline when no engine is attached.
    fn submit_command(&self, command: GossipCommand) {
        let command = match self.command_endpoint.load_full() {
            Some(sender) => match sender.send(command) {
                Ok(()) => return,
                Err(err) => err.0,
            },
            None => command,
        };
        let _writer_lease = self.acquire_writer_lease();
        self.apply_command(command);
    }

    fn submit_and_wait<R>(&self, make_command: impl FnOnce(Sender<R>) -> GossipCommand) -> R {
        let (completed, receiver) = bounded(1);
        self.submit_command(make_command(completed));
        receiver
            .recv()
            .expect("gossip command processor stopped without replying")
    }

    pub(crate) fn apply_command(&self, command: GossipCommand) {
        match command {
            GossipCommand::PublishValue(value) => {
                self.insert_local_route_value("publish_value", *value)
            }
            GossipCommand::PublishLowestSlot(slot) => self.apply_lowest_slot(slot),
            GossipCommand::PublishEpochSlots(slots) => self.apply_epoch_slots(&slots),
            GossipCommand::PublishContactInfo(completed) => {
                self.refresh_my_gossip_contact_info();
                let _ = completed.send(());
            }
            GossipCommand::PublishVote {
                slot,
                transaction,
                completed,
            } => {
                let result = self.apply_vote(slot, transaction);
                let _ = completed.send(result);
            }
            GossipCommand::RefreshVote { transaction, slot } => {
                self.apply_vote_refresh(transaction, slot)
            }
            GossipCommand::PublishDuplicateShred { keypair, chunks } => {
                self.gossip.insert_duplicate_shred(&keypair, chunks)
            }
            GossipCommand::Barrier(completed) => {
                let _ = completed.send(());
            }
        }
    }

    pub fn new(
        contact_info: ContactInfo,
        keypair: Arc<Keypair>,
        socket_addr_space: SocketAddrSpace,
    ) -> Self {
        let me = Self {
            gossip: CrdsGossip::default(),
            identity: GossipIdentity::new(contact_info, keypair),
            entrypoints: RwLock::default(),
            known_validators: OnceLock::new(),
            outbound_budget: DataBudget::default(),
            ping_cache: Mutex::new(PingCache::new(
                GOSSIP_PING_CACHE_TTL,
                GOSSIP_PING_CACHE_OUTSTANDING_PING_TIMEOUT_MS,
                GOSSIP_PING_CACHE_CAPACITY,
            )),
            pull_request_budget: KeyedRateLimiter::new(
                GOSSIP_PULL_SCAN_BUDGET_CACHE_CAPACITY,
                TokenBucket::new(
                    GOSSIP_PULL_SCAN_BUDGET_CAPACITY,
                    GOSSIP_PULL_SCAN_BUDGET_CAPACITY,
                    GOSSIP_PULL_SCAN_BUDGET_REFILL_PER_SEC as f64,
                ),
                GOSSIP_PULL_SCAN_BUDGET_SHARD_COUNT,
            ),
            stats: GossipStats::default(),
            contact_debug_interval: DEFAULT_CONTACT_DEBUG_INTERVAL_MILLIS,
            contact_info_path: PathBuf::default(),
            contact_save_interval: 0, // disabled
            socket_addr_space,
            bind_ip_addrs: Arc::new(BindIpAddrs::default()),
            sigverify_cache: SigVerifyCache::new(),
            migration_status: OnceLock::new(),
            command_endpoint: ArcSwapOption::empty(),
            writer_lease: Mutex::new(()),
        };
        me.refresh_my_gossip_contact_info();
        me
    }

    /// Wires in the Alpenglow MigrationStatus.
    pub fn set_migration_status(&self, migration_status: Arc<MigrationStatus>) {
        let _ = self.migration_status.set(migration_status);
    }

    /// Returns `true` iff the migration status has been wired AND the cluster
    /// has completed the full Alpenglow epoch transition.
    pub(crate) fn is_full_alpenglow_epoch(&self) -> bool {
        self.migration_status
            .get()
            .is_some_and(|m| m.is_full_alpenglow_epoch())
    }

    pub fn set_contact_debug_interval(&mut self, new: u64) {
        self.contact_debug_interval = new;
    }

    pub(crate) fn contact_debug_interval(&self) -> Option<Duration> {
        (self.contact_debug_interval != 0)
            .then(|| Duration::from_millis(self.contact_debug_interval))
    }

    pub(crate) fn contact_save_interval(&self) -> Option<Duration> {
        (self.contact_save_interval != 0).then(|| Duration::from_millis(self.contact_save_interval))
    }

    pub fn socket_addr_space(&self) -> &SocketAddrSpace {
        &self.socket_addr_space
    }

    pub fn set_bind_ip_addrs(&mut self, ip_addrs: Arc<BindIpAddrs>) {
        self.bind_ip_addrs = ip_addrs;
    }

    pub fn bind_ip_addrs(&self) -> Arc<BindIpAddrs> {
        self.bind_ip_addrs.clone()
    }

    pub(crate) fn refresh_push_active_set(
        &self,
        recycler: &PacketBatchRecycler,
        stakes: &HashMap<Pubkey, u64>,
        gossip_validators: Option<&HashSet<Pubkey>>,
        sender: &impl ChannelSend<PacketBatch>,
    ) {
        let shred_version = self.identity.shred_version();
        let mut pings = Vec::new();
        self.gossip.refresh_push_active_set(
            &self.keypair(),
            shred_version,
            stakes,
            gossip_validators,
            &self.ping_cache,
            &mut pings,
            &self.socket_addr_space,
        );
        let pings = pings
            .into_iter()
            .map(|(addr, ping)| (addr, Protocol::PingMessage(ping)));
        send_gossip_packets(pings, recycler, sender, &self.stats);
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    pub fn insert_info(&self, node: ContactInfo) {
        let entry = CrdsValue::new(CrdsData::ContactInfo(node), &self.keypair());
        if let Err(err) = {
            let mut gossip_crds = self.gossip.crds.write().unwrap();
            gossip_crds.insert(entry, timestamp(), GossipRoute::LocalMessage)
        } {
            error!("ClusterInfo.insert_info: {err:?}");
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn insert_crds_value_for_tests(
        &self,
        value: CrdsValue,
    ) -> Result<(), crate::crds::CrdsError> {
        self.gossip
            .crds
            .write()
            .unwrap()
            .insert(value, timestamp(), GossipRoute::LocalMessage)
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn lowest_slot_for_tests(&self, pubkey: Pubkey) -> Option<LowestSlot> {
        self.gossip
            .crds
            .read()
            .unwrap()
            .get::<&LowestSlot>(pubkey)
            .cloned()
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn gossip_counters_for_tests(&self) -> (usize, usize, usize, usize) {
        (
            self.gossip.push.num_old.load(Ordering::Relaxed),
            self.gossip.push.num_total.load(Ordering::Relaxed),
            self.gossip.push.num_pushes.load(Ordering::Relaxed),
            self.gossip.pull.num_pulls.load(Ordering::Relaxed),
        )
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn reset_gossip_counters_for_tests(&self) {
        self.gossip.push.num_old.store(0, Ordering::Relaxed);
        self.gossip.push.num_total.store(0, Ordering::Relaxed);
        self.gossip.push.num_pushes.store(0, Ordering::Relaxed);
        self.gossip.pull.num_pulls.store(0, Ordering::Relaxed);
    }

    pub fn set_entrypoint(&self, entrypoint: ContactInfo) {
        self.set_entrypoints(vec![entrypoint]);
    }

    pub fn set_entrypoints(&self, entrypoints: Vec<ContactInfo>) {
        *self.entrypoints.write().unwrap() = entrypoints;
    }

    /// Pubkeys that should be preserved during CRDS trim.
    /// Returns `Err` when called more than once.
    pub fn set_trim_keep_pubkeys(
        &self,
        pubkeys: impl IntoIterator<Item = Pubkey>,
    ) -> Result<(), HashSet<Pubkey>> {
        let pubkeys = pubkeys.into_iter().collect();
        self.known_validators.set(pubkeys)
    }

    /// Attach a channel that receives an owned `ContactInfoSnapshot` for
    /// every contact info update accepted into the underlying CRDS table.
    /// See [`crate::contact_info_notifier`] for the snapshot type and
    /// channel construction. Intended to be called once at startup when
    /// at least one Geyser plugin has opted into contact info
    /// notifications; leaving it unset is the zero-cost default.
    pub fn set_contact_info_sender(&self, sender: crate::contact_info_notifier::ContactInfoSender) {
        self.gossip
            .crds
            .write()
            .unwrap()
            .set_contact_info_sender(sender);
    }

    pub fn save_contact_info(&self) {
        let _st = ScopedTimer::from(&self.stats.save_contact_info_time);
        let nodes = self.contact_infos_to_save();
        if nodes.is_empty() {
            return;
        }
        let filename = self.contact_info_path.join("contact-info.bin");
        if write_contact_info_file(&filename, &nodes) {
            info!(
                "Saved contact info for {} nodes into {}",
                nodes.len(),
                filename.display()
            );
        }
    }

    /// Every peer worth persisting: not ourselves, and not an entrypoint.
    fn contact_infos_to_save(&self) -> Vec<CrdsValue> {
        let entrypoint_gossip_addrs = self
            .entrypoints
            .read()
            .unwrap()
            .iter()
            .filter_map(ContactInfo::gossip)
            .collect::<HashSet<_>>();
        let self_pubkey = self.id();
        let gossip_crds = self.gossip.crds.read().unwrap();
        gossip_crds
            .get_nodes()
            .filter(|v| {
                // Don't save:
                // 1. Our ContactInfo. No point
                // 2. Entrypoint ContactInfo. This will avoid adopting the incorrect shred
                //    version on restart if the entrypoint shred version changes.  Also
                //    there's not much point in saving entrypoint ContactInfo since by
                //    definition that information is already available
                let contact_info = v.value.contact_info().unwrap();
                contact_info.pubkey() != &self_pubkey
                    && contact_info
                        .gossip()
                        .is_some_and(|addr| !entrypoint_gossip_addrs.contains(&addr))
            })
            .map(|v| v.value.clone())
            .collect()
    }

    pub fn restore_contact_info(&mut self, contact_info_path: &Path, contact_save_interval: u64) {
        self.contact_info_path = contact_info_path.into();
        self.contact_save_interval = contact_save_interval;

        let filename = contact_info_path.join("contact-info.bin");
        if !filename.exists() {
            return;
        }

        let nodes: Vec<CrdsValue> = match File::open(&filename) {
            Ok(file) => wincode::config::deserialize_from(
                &mut BufReader::new(file),
                ContactInfoFileConfig::new(),
            )
            .unwrap_or_else(|err| {
                warn!("Failed to deserialize {}: {}", filename.display(), err);
                vec![]
            }),
            Err(err) => {
                warn!("Failed to open {}: {}", filename.display(), err);
                vec![]
            }
        };

        info!(
            "Loaded contact info for {} nodes from {}",
            nodes.len(),
            filename.display()
        );
        let self_shred_version = self.my_shred_version();
        self.insert_local_route_values(
            "restore_contact_info",
            nodes.into_iter().filter(|node| {
                node.contact_info()
                    .is_some_and(|contact_info| contact_info.shred_version() == self_shred_version)
            }),
        );
    }

    pub fn id(&self) -> Pubkey {
        self.identity.id()
    }

    pub fn keypair(&self) -> Arc<Keypair> {
        self.identity.keypair()
    }

    pub fn set_keypair(&self, new_keypair: Arc<Keypair>) {
        let changed = self.identity.set_keypair(new_keypair);
        self.publish_contact_info(changed);
    }

    /// Mutates the advertised contact record and republishes it. Every
    /// contact-info mutation must go through here so the CRDS table never
    /// keeps a stale copy.
    fn update_and_publish_contact_info(
        &self,
        update: impl FnOnce(&mut ContactInfo) -> Result<(), ContactInfoError>,
    ) -> Result<(), ContactInfoError> {
        let (result, changed) = self.identity.update_contact_info(update);
        // A failed setter leaves the record untouched, so there is nothing to
        // publish and the change token is dropped deliberately.
        result?;
        self.publish_contact_info(changed);
        Ok(())
    }

    pub fn set_gossip_socket(&self, gossip_addr: SocketAddr) -> Result<(), ContactInfoError> {
        self.update_and_publish_contact_info(|contact_info| contact_info.set_gossip(gossip_addr))
    }

    pub fn set_tvu_socket(&self, tvu_addr: SocketAddr) -> Result<(), ContactInfoError> {
        self.update_and_publish_contact_info(|contact_info| {
            contact_info.set_tvu(contact_info::Protocol::UDP, tvu_addr)
        })
    }

    pub fn set_tpu_quic(&self, tpu_addr: SocketAddr) -> Result<(), ContactInfoError> {
        self.update_and_publish_contact_info(|contact_info| {
            contact_info.set_tpu(contact_info::Protocol::QUIC, tpu_addr)
        })
    }

    pub fn set_tpu_forwards_quic(
        &self,
        tpu_forwards_addr: SocketAddr,
    ) -> Result<(), ContactInfoError> {
        self.update_and_publish_contact_info(|contact_info| {
            contact_info.set_tpu_forwards(contact_info::Protocol::QUIC, tpu_forwards_addr)
        })
    }

    pub fn set_tpu_vote(
        &self,
        protocol: contact_info::Protocol,
        tpu_vote_addr: SocketAddr,
    ) -> Result<(), ContactInfoError> {
        self.update_and_publish_contact_info(|contact_info| {
            contact_info.set_tpu_vote(protocol, tpu_vote_addr)
        })
    }

    pub fn lookup_contact_info<R>(
        &self,
        id: &Pubkey,
        query: impl ContactInfoQuery<R>,
    ) -> Option<R> {
        let gossip_crds = self.gossip.crds.read().unwrap();
        gossip_crds.get(*id).map(query)
    }

    /// Resolve `query` on ContactInfo for `peers`.
    /// Yields one entry per pubkey in `peers`, or `None` when peer is absent.
    pub fn query_contact_infos<'a, R>(
        &self,
        peers: impl IntoIterator<Item = &'a Pubkey>,
        query: impl ContactInfoQuery<R>,
    ) -> Vec<(Pubkey, Option<R>)> {
        let read_guard = self.gossip.crds.read().unwrap();
        peers
            .into_iter()
            .map(|id| (*id, read_guard.get(*id).map(|ci: &ContactInfo| query(ci))))
            .collect()
    }

    pub fn lookup_contact_info_by_gossip_addr(
        &self,
        gossip_addr: &SocketAddr,
    ) -> Option<ContactInfo> {
        let gossip_crds = self.gossip.crds.read().unwrap();
        let mut nodes = gossip_crds.get_nodes_contact_info();
        nodes
            .find(|node| node.gossip() == Some(*gossip_addr))
            .cloned()
    }

    pub fn my_contact_info(&self) -> ContactInfo {
        self.identity.contact_info()
    }

    pub fn my_shred_version(&self) -> u16 {
        self.identity.shred_version()
    }

    fn publish_contact_info(&self, _changed: ContactInfoChanged) {
        self.submit_and_wait(GossipCommand::PublishContactInfo);
    }

    fn lookup_epoch_slots(&self, ix: EpochSlotsIndex) -> EpochSlots {
        let self_pubkey = self.id();
        let label = CrdsValueLabel::EpochSlots(ix, self_pubkey);
        let gossip_crds = self.gossip.crds.read().unwrap();
        gossip_crds
            .get::<&CrdsValue>(&label)
            .and_then(|v| v.epoch_slots())
            .cloned()
            .unwrap_or_else(|| EpochSlots::new(self_pubkey, timestamp()))
    }

    fn addr_to_string(&self, default_ip: &Option<IpAddr>, addr: &Option<SocketAddr>) -> String {
        addr.filter(|addr| self.socket_addr_space.check(addr))
            .map(|addr| {
                if &Some(addr.ip()) == default_ip {
                    addr.port().to_string()
                } else {
                    addr.to_string()
                }
            })
            .unwrap_or_else(|| String::from("none"))
    }

    pub fn rpc_info_trace(&self) -> String {
        self.rpc_info_trace_from(&self.all_peers())
    }

    fn rpc_info_trace_from(&self, peers: &[(ContactInfo, u64)]) -> String {
        macro_rules! format_string {
            () => {
                "{:15} {:2}|{:^7}| {:^44} |{:^21}|{:^6}|{:^6}|{:^8}\n"
            };
        }
        let mut output = format!(
            format_string!(),
            "RPC Address", "", "Age(ms)", "Node identifier", "Version", "RPC", "PubSub", "ShredVer"
        );
        push_header_rule(&mut output);
        let now = timestamp();
        let my_pubkey = self.id();
        let mut num_nodes = 0usize;
        for (node, last_updated) in peers {
            let Some(node_rpc) = node.rpc().filter(|addr| self.socket_addr_space.check(addr))
            else {
                continue;
            };
            num_nodes = num_nodes.saturating_add(1);
            let rpc_addr = node_rpc.ip();
            write!(
                &mut output,
                format_string!(),
                rpc_addr.to_string(),
                if node.pubkey() == &my_pubkey {
                    "me"
                } else {
                    ""
                },
                now.saturating_sub(*last_updated),
                node.pubkey().to_string(),
                node.version().to_string(),
                self.addr_to_string(&Some(rpc_addr), &node.rpc()),
                self.addr_to_string(&Some(rpc_addr), &node.rpc_pubsub()),
                node.shred_version(),
            )
            .unwrap();
        }
        writeln!(&mut output, "\nRPC Enabled Nodes: {num_nodes}").unwrap();
        output
    }

    pub fn contact_info_trace(&self) -> String {
        self.contact_info_trace_from(&self.all_peers())
    }

    pub(crate) fn contact_info_traces(&self) -> (String, String) {
        let peers = self.all_peers();
        (
            self.contact_info_trace_from(&peers),
            self.rpc_info_trace_from(&peers),
        )
    }

    fn contact_info_trace_from(&self, peers: &[(ContactInfo, u64)]) -> String {
        macro_rules! format_string {
            () => {
                "{:15} {:2}|{:^7}| {:^44} |{:^21}|{:^6}|{:^7}|{:^5}|{:^6}|{:^5}|{:^6}|{:^6}|{:^8}\n"
            };
        }
        let mut output = format!(
            format_string!(),
            "IP Address",
            "",
            "Age(ms)",
            "Node identifier",
            "Version",
            "Gossip",
            "TPUvote",
            "TPU",
            "TPUfwd",
            "TVU",
            "ServeR",
            "Alpeng",
            "ShredVer"
        );
        push_header_rule(&mut output);

        let now = timestamp();
        let mut total_spy_nodes = 0usize;
        let my_pubkey = self.id();
        for (node, last_updated) in peers {
            if Self::is_spy_node(node, &self.socket_addr_space) {
                total_spy_nodes = total_spy_nodes.saturating_add(1);
            }
            let gossip_addr = node.gossip();
            let ip_addr = gossip_addr.map(|addr| addr.ip());
            write!(
                &mut output,
                format_string!(),
                gossip_addr
                    .filter(|addr| self.socket_addr_space.check(addr))
                    .map(|addr| addr.ip().to_string())
                    .unwrap_or_else(|| String::from("none")),
                if node.pubkey() == &my_pubkey {
                    "me"
                } else {
                    ""
                },
                now.saturating_sub(*last_updated),
                node.pubkey().to_string(),
                node.version().to_string(),
                self.addr_to_string(&ip_addr, &gossip_addr),
                self.addr_to_string(&ip_addr, &node.tpu_vote(contact_info::Protocol::UDP)),
                self.addr_to_string(&ip_addr, &node.tpu(contact_info::Protocol::QUIC)),
                self.addr_to_string(&ip_addr, &node.tpu_forwards(contact_info::Protocol::QUIC)),
                self.addr_to_string(&ip_addr, &node.tvu(contact_info::Protocol::UDP)),
                self.addr_to_string(&ip_addr, &node.serve_repair(contact_info::Protocol::UDP)),
                self.addr_to_string(&ip_addr, &node.alpenglow()),
                node.shred_version(),
            )
            .unwrap();
        }
        write!(
            &mut output,
            "Nodes: {}",
            peers.len().saturating_sub(total_spy_nodes)
        )
        .unwrap();
        if total_spy_nodes > 0 {
            write!(&mut output, "\nSpies: {total_spy_nodes}").unwrap();
        }
        output
    }

    pub fn push_lowest_slot(&self, lowest_slot: Slot) {
        self.submit_command(GossipCommand::PublishLowestSlot(lowest_slot));
    }

    fn apply_lowest_slot(&self, lowest_slot: Slot) {
        let self_keypair = self.keypair();
        let self_pubkey = self_keypair.pubkey();
        let current_lowest_slot = {
            let gossip_crds = self.gossip.crds.read().unwrap();
            gossip_crds
                .get::<&LowestSlot>(self_pubkey)
                .map(|x| x.lowest)
                .unwrap_or_default()
        };
        if lowest_slot > current_lowest_slot {
            let now = timestamp();
            let entry = CrdsValue::new(
                CrdsData::LowestSlot(0, LowestSlot::new(self_pubkey, lowest_slot, now)),
                &self_keypair,
            );
            self.insert_local_route_value("publish_lowest_slot", entry);
        }
    }

    fn insert_local_route_value(&self, operation: &str, value: CrdsValue) {
        self.insert_local_route_values(operation, [value]);
    }

    fn insert_local_route_values(
        &self,
        operation: &str,
        values: impl IntoIterator<Item = CrdsValue>,
    ) {
        crds::insert_local_route_values(&mut self.gossip.crds.write().unwrap(), operation, values);
    }

    pub fn push_epoch_slots(&self, new_slots: &[Slot]) {
        self.submit_command(GossipCommand::PublishEpochSlots(new_slots.to_vec()));
    }

    fn apply_epoch_slots(&self, new_slots: &[Slot]) {
        let self_keypair = self.keypair();
        let record_metadata = self.epoch_slots_record_metadata(&self_keypair.pubkey());
        self.warn_if_epoch_slots_filling(new_slots, &record_metadata);
        let entries = self.build_epoch_slots_entries(new_slots, &record_metadata, &self_keypair);
        self.insert_local_route_values("publish_epoch_slots", entries);
    }

    /// The `(wallclock, first_slot, index)` of every epoch-slots record this
    /// node currently advertises.
    fn epoch_slots_record_metadata(
        &self,
        self_pubkey: &Pubkey,
    ) -> Vec<(u64, Slot, EpochSlotsIndex)> {
        let gossip_crds =
            self.time_gossip_read_lock("lookup_epoch_slots", &self.stats.epoch_slots_lookup);
        (0..crds_data::MAX_EPOCH_SLOTS)
            .filter_map(|ix| {
                let label = CrdsValueLabel::EpochSlots(ix, *self_pubkey);
                let crds_value = gossip_crds.get::<&CrdsValue>(&label)?;
                let epoch_slots = crds_value.epoch_slots()?;
                let first_slot = epoch_slots.first_slot()?;
                Some((epoch_slots.wallclock, first_slot, ix))
            })
            .collect()
    }

    /// Warns when the epoch-slots ring is full and no longer spans a full epoch.
    fn warn_if_epoch_slots_filling(
        &self,
        new_slots: &[Slot],
        record_metadata: &[(u64, Slot, EpochSlotsIndex)],
    ) {
        let min_slot: Slot = record_metadata
            .iter()
            .map(|(_wallclock, slot, _index)| *slot)
            .min()
            .unwrap_or_default();
        let max_slot: Slot = new_slots.iter().max().copied().unwrap_or(0);
        let slot_span = max_slot as isize - min_slot as isize;
        if DEFAULT_SLOTS_PER_EPOCH as isize > slot_span
            && crds_data::MAX_EPOCH_SLOTS as usize <= record_metadata.len()
        {
            self.stats.epoch_slots_filled.add_relaxed(1);
            warn!(
                "EPOCH_SLOTS are filling up FAST {}/{}",
                slot_span,
                record_metadata.len()
            );
        }
    }

    /// Packs `update` into epoch-slots records, continuing from the newest
    /// existing index and wrapping around the ring as it fills.
    fn build_epoch_slots_entries(
        &self,
        mut new_slots: &[Slot],
        record_metadata: &[(u64, Slot, EpochSlotsIndex)],
        self_keypair: &Keypair,
    ) -> Vec<CrdsValue> {
        let self_pubkey = self_keypair.pubkey();
        let mut epoch_slots_index = match record_metadata.iter().max() {
            Some((_wallclock, _slot, index)) => *index,
            None => 0,
        };
        // The first record extends the newest existing one; the rest start empty.
        let mut start_new_record = false;
        let mut entries = Vec::default();
        while !new_slots.is_empty() {
            let now = timestamp();
            let mut epoch_slots = if start_new_record {
                EpochSlots::new(self_pubkey, now)
            } else {
                self.lookup_epoch_slots(epoch_slots_index)
            };
            let num_filled = epoch_slots.fill(new_slots, now);
            new_slots = &new_slots[num_filled..];
            if num_filled > 0 {
                let data = CrdsData::EpochSlots(epoch_slots_index, epoch_slots);
                entries.push(CrdsValue::new(data, self_keypair));
            }
            epoch_slots_index = (epoch_slots_index + 1) % crds_data::MAX_EPOCH_SLOTS;
            start_new_record = true;
        }
        entries
    }

    fn time_gossip_read_lock<'a>(
        &'a self,
        label: &'static str,
        counter: &'a Counter,
    ) -> TimedGuard<'a, RwLockReadGuard<'a, Crds>> {
        TimedGuard::new(self.gossip.crds.read().unwrap(), label, counter)
    }

    fn publish_value(&self, value: CrdsValue) {
        self.submit_command(GossipCommand::PublishValue(Box::new(value)));
    }

    pub fn push_snapshot_hashes(
        &self,
        full: (Slot, Hash),
        incremental: Vec<(Slot, Hash)>,
    ) -> Result<(), ClusterInfoError> {
        if incremental.len() > MAX_INCREMENTAL_SNAPSHOT_HASHES {
            return Err(ClusterInfoError::TooManyIncrementalSnapshotHashes);
        }

        let self_keypair = self.keypair();

        let message = CrdsData::SnapshotHashes(SnapshotHashes {
            from: self_keypair.pubkey(),
            full,
            incremental,
            wallclock: timestamp(),
        });
        self.publish_value(CrdsValue::new(message, &self_keypair));

        Ok(())
    }

    pub fn publish_vote_at_index(&self, vote: Transaction, vote_index: u8, self_keypair: &Keypair) {
        assert!(vote_index < MAX_VOTES);
        let self_pubkey = self_keypair.pubkey();
        let now = timestamp();
        let vote = Vote::new(self_pubkey, vote, now).unwrap();
        let vote = CrdsData::Vote(vote_index, vote);
        let vote = CrdsValue::new(vote, self_keypair);
        self.insert_local_route_value("publish_vote", vote);
    }

    /// If there are less than `MAX_LOCKOUT_HISTORY` votes present, returns the next index
    /// without a vote. If there are `MAX_LOCKOUT_HISTORY` votes:
    /// - Finds the oldest wallclock vote and returns its index
    /// - Otherwise returns the total amount of observed votes
    ///
    /// If there exists a newer vote in gossip than `new_vote_slot` return `None` as this indicates
    /// that we might be submitting slashable votes after an improper restart
    fn find_vote_index_to_evict(&self, new_vote_slot: Slot) -> Option<u8> {
        let self_pubkey = self.id();
        let mut num_crds_votes = 0;
        let mut exists_newer_vote = false;
        let vote_index = {
            let gossip_crds =
                self.time_gossip_read_lock("gossip_read_push_vote", &self.stats.push_vote_read);
            (0..MAX_VOTES)
                .filter_map(|ix| {
                    let vote = CrdsValueLabel::Vote(ix, self_pubkey);
                    let vote: &CrdsData = gossip_crds.get(&vote)?;
                    num_crds_votes += 1;
                    match &vote {
                        CrdsData::Vote(_, vote) if vote.slot() < Some(new_vote_slot) => {
                            Some((vote.wallclock, ix))
                        }
                        CrdsData::Vote(_, _) => {
                            exists_newer_vote = true;
                            None
                        }
                        _ => panic!("this should not happen!"),
                    }
                })
                .min() // Boot the oldest evicted vote by wallclock.
                .map(|(_ /*wallclock*/, ix)| ix)
        };
        if exists_newer_vote {
            return None;
        }
        if num_crds_votes < MAX_VOTES {
            // Do not evict if there is space in crds
            Some(num_crds_votes)
        } else {
            vote_index
        }
    }

    pub fn push_vote(&self, tower: &[Slot], vote: Transaction) {
        debug_assert!(tower.iter().tuple_windows().all(|(a, b)| a < b));
        let slot = tower.last().copied().expect("Cannot push empty vote");
        let result = self.submit_and_wait(|completed| GossipCommand::PublishVote {
            slot,
            transaction: vote,
            completed,
        });
        let Err(vote) = result else {
            return;
        };
        // In this case we have restarted with a mangled/missing tower and are attempting
        // to push an old vote. This could be a slashable offense so better to panic here.
        let (_, vote, hash, _) = vote_parser::parse_vote_transaction(&vote).unwrap();
        panic!(
            "Submitting old vote, switch: {}, vote slots: {:?}, tower: {:?}. The local tower.bin \
             was out of date or missing, and we are attempting to submit slashable votes. Another \
             possibility is that the node was not correctly started with wait for supermajority \
             during a cluster restart, and then later started with wait for supermajority, \
             causing the tower.bin to be pruned. To progress, either download a newer snapshot or \
             set --wait-to-vote-slot higher than the last vote present in gossip",
            hash.is_some(),
            vote.slots(),
            tower
        );
    }

    fn apply_vote(&self, new_vote_slot: Slot, vote: Transaction) -> Result<(), Transaction> {
        let self_keypair = self.keypair();
        // Find the oldest crds vote by wallclock that has a lower slot than the new vote
        // and recycle its vote-index. If the crds buffer is not full we instead add a new vote-index.
        let Some(vote_index) = self.find_vote_index_to_evict(new_vote_slot) else {
            return Err(vote);
        };
        debug_assert!(vote_index < MAX_VOTES);
        self.publish_vote_at_index(vote, vote_index, &self_keypair);
        Ok(())
    }

    pub fn refresh_vote(&self, refresh_vote: Transaction, refresh_vote_slot: Slot) {
        self.submit_command(GossipCommand::RefreshVote {
            transaction: refresh_vote,
            slot: refresh_vote_slot,
        });
    }

    fn apply_vote_refresh(&self, refresh_vote: Transaction, refresh_vote_slot: Slot) {
        let self_keypair = self.keypair();
        let vote_index = {
            let gossip_crds =
                self.time_gossip_read_lock("gossip_read_push_vote", &self.stats.push_vote_read);
            (0..MAX_VOTES).find(|ix| {
                let vote = CrdsValueLabel::Vote(*ix, self_keypair.pubkey());
                let Some(vote) = gossip_crds.get::<&CrdsData>(&vote) else {
                    return false;
                };
                let CrdsData::Vote(_, prev_vote) = &vote else {
                    panic!("this should not happen!");
                };
                match prev_vote.slot() {
                    Some(prev_vote_slot) => prev_vote_slot == refresh_vote_slot,
                    None => {
                        error!("crds vote with no slots!");
                        false
                    }
                }
            })
        };

        // We don't write to an arbitrary index, because it may replace one of this validator's
        // existing votes on the network.
        if let Some(vote_index) = vote_index {
            self.publish_vote_at_index(refresh_vote, vote_index, &self_keypair);
        } else {
            // If you don't see a vote with the same slot yet, this means you probably
            // restarted, and need to repush and evict the oldest vote
            let Some(vote_index) = self.find_vote_index_to_evict(refresh_vote_slot) else {
                warn!(
                    "trying to refresh slot {refresh_vote_slot} but all votes in gossip table are \
                     for newer slots",
                );
                return;
            };
            debug_assert!(vote_index < MAX_VOTES);
            self.publish_vote_at_index(refresh_vote, vote_index, &self_keypair);
        }
    }

    /// Returns votes inserted since the given cursor.
    pub fn get_votes(&self, cursor: &mut Cursor) -> Vec<Transaction> {
        let txs: Vec<Transaction> = self
            .time_gossip_read_lock("get_votes", &self.stats.get_votes)
            .get_votes(cursor)
            .map(|vote| {
                let CrdsData::Vote(_, vote) = vote.value.data() else {
                    panic!("this should not happen!");
                };
                vote.transaction().clone()
            })
            .collect();
        self.stats.get_votes_count.add_relaxed(txs.len() as u64);
        txs
    }

    /// Returns votes and the associated labels inserted since the given cursor.
    pub fn get_votes_with_labels(
        &self,
        cursor: &mut Cursor,
    ) -> (Vec<CrdsValueLabel>, Vec<Transaction>) {
        let (labels, txs): (_, Vec<_>) = self
            .time_gossip_read_lock("get_votes", &self.stats.get_votes)
            .get_votes(cursor)
            .map(|vote| {
                let label = vote.value.label();
                let CrdsData::Vote(_, vote) = vote.value.data() else {
                    panic!("this should not happen!");
                };
                (label, vote.transaction().clone())
            })
            .unzip();
        self.stats.get_votes_count.add_relaxed(txs.len() as u64);
        (labels, txs)
    }

    pub fn push_duplicate_shred(
        &self,
        shred: &Shred,
        other_payload: &[u8],
    ) -> Result<(), GossipError> {
        if self.is_full_alpenglow_epoch() {
            // Alpenglow does not propagate duplicate-shred proofs via gossip.
            return Ok(());
        }
        let keypair = self.keypair();
        if self
            .gossip
            .contains_duplicate_shred(&keypair.pubkey(), shred.slot())
        {
            return Ok(());
        }
        // Keep expensive preparation outside the CRDS write lock.
        let chunks = duplicate_shred::from_shred(
            shred.clone(),
            keypair.pubkey(),
            Vec::from(other_payload),
            None::<fn(Slot) -> Option<Pubkey>>, // Leader schedule
            timestamp(),
            DUPLICATE_SHRED_MAX_PAYLOAD_SIZE,
            self.my_shred_version(),
        )?
        .collect();
        self.submit_command(GossipCommand::PublishDuplicateShred { keypair, chunks });
        Ok(())
    }

    pub fn get_snapshot_hashes_for_node(&self, pubkey: &Pubkey) -> Option<SnapshotHashes> {
        self.gossip
            .crds
            .read()
            .unwrap()
            .get::<&SnapshotHashes>(*pubkey)
            .cloned()
    }

    /// Returns epoch-slots inserted since the given cursor.
    pub fn get_epoch_slots(&self, cursor: &mut Cursor) -> Vec<EpochSlots> {
        let gossip_crds = self.gossip.crds.read().unwrap();
        gossip_crds
            .get_epoch_slots(cursor)
            .map(|entry| match entry.value.data() {
                CrdsData::EpochSlots(_, slots) => slots.clone(),
                _ => panic!("this should not happen!"),
            })
            .collect()
    }

    /// Returns duplicate-shreds inserted since the given cursor.
    pub(crate) fn get_duplicate_shreds(&self, cursor: &mut Cursor) -> Vec<DuplicateShred> {
        let gossip_crds = self.gossip.crds.read().unwrap();
        gossip_crds
            .get_duplicate_shreds(cursor)
            .map(|entry| match entry.value.data() {
                CrdsData::DuplicateShred(_, dup) => dup.clone(),
                _ => panic!("this should not happen!"),
            })
            .collect()
    }

    pub fn get_node_version(&self, pubkey: &Pubkey) -> Option<solana_version::Version> {
        let gossip_crds = self.gossip.crds.read().unwrap();
        gossip_crds
            .get::<&ContactInfo>(*pubkey)
            .map(ContactInfo::version)
            .cloned()
    }

    fn check_socket_addr_space(&self, addr: &Option<SocketAddr>) -> bool {
        addr.as_ref()
            .map(|addr| self.socket_addr_space.check(addr))
            .unwrap_or_default()
    }

    /// all validators that have a valid rpc port.
    pub fn rpc_peers(&self) -> Vec<ContactInfo> {
        let self_pubkey = self.id();
        let gossip_crds = self.gossip.crds.read().unwrap();
        gossip_crds
            .get_nodes_contact_info()
            .filter(|node| {
                node.pubkey() != &self_pubkey && self.check_socket_addr_space(&node.rpc())
            })
            .cloned()
            .collect()
    }

    // All nodes in gossip (including spy nodes) and the last time we heard about them
    pub fn all_peers(&self) -> Vec<(ContactInfo, u64)> {
        let gossip_crds = self.gossip.crds.read().unwrap();
        gossip_crds
            .get_nodes()
            .filter_map(|node| {
                let contact_info = node.value.contact_info()?;
                Some((contact_info.clone(), node.local_timestamp))
            })
            .collect()
    }

    pub fn gossip_peers(&self) -> Vec<ContactInfo> {
        let me = self.id();
        let gossip_crds = self.gossip.crds.read().unwrap();
        gossip_crds
            .get_nodes_contact_info()
            .filter(|node| node.pubkey() != &me && self.check_socket_addr_space(&node.gossip()))
            .cloned()
            .collect()
    }

    /// all validators that have a valid tvu port.
    pub fn tvu_peers<R>(&self, query: impl ContactInfoQuery<R>) -> Vec<R> {
        let self_pubkey = self.id();
        self.time_gossip_read_lock("tvu_peers", &self.stats.tvu_peers)
            .get_nodes_contact_info()
            .filter(|node| {
                node.pubkey() != &self_pubkey
                    && self.check_socket_addr_space(&node.tvu(contact_info::Protocol::UDP))
            })
            .map(query)
            .collect()
    }

    /// all tvu peers with valid gossip addrs that likely have the slot being requested
    pub fn repair_peers(&self, slot: Slot) -> Vec<ContactInfo> {
        let _st = ScopedTimer::from(&self.stats.repair_peers);
        let self_pubkey = self.id();
        let gossip_crds = self.gossip.crds.read().unwrap();
        gossip_crds
            .get_nodes_contact_info()
            .filter(|node| {
                node.pubkey() != &self_pubkey
                    && self.check_socket_addr_space(&node.tvu(contact_info::Protocol::UDP))
                    && self.check_socket_addr_space(&node.serve_repair(contact_info::Protocol::UDP))
                    && match gossip_crds.get::<&LowestSlot>(*node.pubkey()) {
                        None => true, // fallback to legacy behavior
                        Some(lowest_slot) => lowest_slot.lowest <= slot,
                    }
            })
            .cloned()
            .collect()
    }

    fn is_spy_node(node: &ContactInfo, socket_addr_space: &SocketAddrSpace) -> bool {
        ![
            node.tpu(contact_info::Protocol::QUIC),
            node.gossip(),
            node.tvu(contact_info::Protocol::UDP),
        ]
        .into_iter()
        .all(|addr| {
            addr.map(|addr| socket_addr_space.check(&addr))
                .unwrap_or_default()
        })
    }

    /// compute broadcast table
    pub fn tpu_peers(&self) -> Vec<ContactInfo> {
        let self_pubkey = self.id();
        let gossip_crds = self.gossip.crds.read().unwrap();
        gossip_crds
            .get_nodes_contact_info()
            .filter(|node| {
                node.pubkey() != &self_pubkey
                    && self.check_socket_addr_space(&node.tpu(contact_info::Protocol::QUIC))
            })
            .cloned()
            .collect()
    }

    pub(crate) fn refresh_my_gossip_contact_info(&self) {
        let node = self.identity.refreshed_crds_value(timestamp());
        self.insert_local_route_value("publish_contact_info", node);
    }

    // If the network entrypoint hasn't been discovered yet, add it to the crds table
    fn append_entrypoint_to_pulls<T: Iterator<Item = (SocketAddr, CrdsFilter)> + Clone>(
        &self,
        thread_pool: &ThreadPool,
        max_bloom_filter_bytes: usize,
        pulls: T,
    ) -> impl Iterator<Item = (SocketAddr, CrdsFilter)> + use<T> {
        const THROTTLE_DELAY: u64 = CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS / 2;
        let mut pulls = pulls.peekable();
        let entrypoint = {
            let mut entrypoints = self.entrypoints.write().unwrap();
            let Some(entrypoint) = entrypoints.choose_mut(&mut rand::rng()) else {
                return Either::Left(pulls);
            };
            if pulls.peek().is_some() {
                let now = timestamp();
                if now <= entrypoint.wallclock().saturating_add(THROTTLE_DELAY) {
                    return Either::Left(pulls);
                }
                entrypoint.set_wallclock(now);
                if let Some(entrypoint_gossip) = entrypoint.gossip()
                    && self
                        .time_gossip_read_lock("entrypoint", &self.stats.entrypoint)
                        .get_nodes_contact_info()
                        .any(|node| node.gossip() == Some(entrypoint_gossip))
                {
                    // Found the entrypoint, no need to pull from it.
                    return Either::Left(pulls);
                }
            }
            let Some(entrypoint) = entrypoint.gossip() else {
                return Either::Left(pulls);
            };
            entrypoint
        };
        let filters = if pulls.peek().is_none() {
            let _st = ScopedTimer::from(&self.stats.entrypoint2);
            Either::Left(
                self.gossip
                    .pull
                    .build_crds_filters(thread_pool, &self.gossip.crds, max_bloom_filter_bytes)
                    .into_iter(),
            )
        } else {
            Either::Right(pulls.clone().map(|(_, filter)| filter))
        };
        self.stats.pull_from_entrypoint_count.add_relaxed(1);
        Either::Right(pulls.chain(repeat(entrypoint).zip(filters)))
    }

    fn new_pull_requests(
        &self,
        thread_pool: &ThreadPool,
        gossip_validators: Option<&HashSet<Pubkey>>,
        stakes: &HashMap<Pubkey, u64>,
    ) -> impl Iterator<Item = (SocketAddr, Protocol)> + use<> {
        let now = timestamp();
        let keypair = self.keypair();
        let mut contact_info = self.my_contact_info();
        contact_info.set_wallclock(now);
        let self_info = CrdsValue::new(CrdsData::from(contact_info), &keypair);
        let max_bloom_filter_bytes = get_max_bloom_filter_bytes(&self_info);
        let mut pings = Vec::new();
        let pulls = {
            let _st = ScopedTimer::from(&self.stats.new_pull_requests);
            self.gossip
                .new_pull_request(
                    thread_pool,
                    &keypair,
                    self.my_shred_version(),
                    now,
                    gossip_validators,
                    stakes,
                    max_bloom_filter_bytes,
                    &self.ping_cache,
                    &mut pings,
                    &self.socket_addr_space,
                )
                .into_iter()
                .flatten()
        };
        let pings = pings
            .into_iter()
            .map(|(addr, ping)| (addr, Protocol::PingMessage(ping)));
        self.append_entrypoint_to_pulls(thread_pool, max_bloom_filter_bytes, pulls)
            .map(move |(gossip_addr, filter)| {
                let request = Protocol::PullRequest(filter, self_info.clone());
                (gossip_addr, request)
            })
            .chain(pings)
    }

    /// Blocks until every command submitted before this call has been applied.
    pub fn flush_gossip_commands(&self) {
        self.submit_and_wait(GossipCommand::Barrier);
    }

    fn new_push_requests(
        &self,
        stakes: &HashMap<Pubkey, u64>,
    ) -> impl Iterator<Item = (SocketAddr, Protocol)> + use<> {
        let self_id = self.id();
        let is_full_alpenglow_epoch = self.is_full_alpenglow_epoch();
        let (entries, push_messages, num_pushes) = {
            let _st = ScopedTimer::from(&self.stats.new_push_requests);
            self.gossip
                .new_push_messages(&self_id, timestamp(), stakes, |value| {
                    should_retain_crds_value(
                        value,
                        stakes,
                        GossipFilterDirection::EgressPush,
                        is_full_alpenglow_epoch,
                    )
                })
        };
        self.stats
            .push_fanout_num_entries
            .add_relaxed(entries.len() as u64);
        self.stats
            .push_fanout_num_nodes
            .add_relaxed(num_pushes as u64);
        let push_messages: Vec<_> = {
            let gossip_crds =
                self.time_gossip_read_lock("push_req_lookup", &self.stats.new_push_requests2);
            push_messages
                .into_iter()
                .filter_map(|(pubkey, messages)| {
                    let addr = get_node_addr(
                        pubkey,
                        ContactInfo::gossip,
                        &gossip_crds,
                        &self.socket_addr_space,
                    )?;
                    Some((addr, messages))
                })
                .collect()
        };
        let entries = Rc::new(entries);
        push_messages
            .into_iter()
            .flat_map(move |(peer, msgs): (SocketAddr, Vec<usize>)| {
                let entries = Rc::clone(&entries);
                let msgs = msgs.into_iter().map(move |k| entries[k].clone());
                let msgs = split_gossip_messages(PUSH_MESSAGE_MAX_PAYLOAD_SIZE, msgs)
                    .map(move |msgs| Protocol::PushMessage(self_id, msgs));
                repeat(peer).zip(msgs)
            })
    }

    // Builds new push and pull requests.
    fn build_gossip_requests(
        &self,
        thread_pool: &ThreadPool,
        gossip_validators: Option<&HashSet<Pubkey>>,
        stakes: &HashMap<Pubkey, u64>,
        generate_pull_requests: bool,
    ) -> impl Iterator<Item = (SocketAddr, Protocol)> + use<> {
        self.trim_crds_table(CRDS_UNIQUE_PUBKEY_CAPACITY, stakes);
        let push_requests = self.new_push_requests(stakes);
        if generate_pull_requests {
            let pull_requests = self.new_pull_requests(thread_pool, gossip_validators, stakes);
            Either::Right(push_requests.chain(pull_requests))
        } else {
            Either::Left(push_requests)
        }
    }

    /// Sends push messages and, when due, pull requests to selected peers.
    pub(crate) fn send_gossip_requests(
        &self,
        thread_pool: &ThreadPool,
        gossip_validators: Option<&HashSet<Pubkey>>,
        recycler: &PacketBatchRecycler,
        stakes: &HashMap<Pubkey, u64>,
        outbound_sender: &impl ChannelSend<PacketBatch>,
        generate_pull_requests: bool,
    ) -> Result<(), GossipError> {
        let _st = ScopedTimer::from(&self.stats.gossip_transmit_loop_time);
        let mut packet_batch =
            RecycledPacketBatch::new_with_recycler(recycler, 0, "send_gossip_requests");
        self.build_gossip_requests(
            thread_pool,
            gossip_validators,
            stakes,
            generate_pull_requests,
        )
        .filter_map(|(addr, data)| make_gossip_packet(addr, &data, &self.stats))
        .for_each(|pkt| packet_batch.push(pkt));
        if !packet_batch.is_empty()
            && let Err(TrySendError::Full(packet_batch)) =
                outbound_sender.try_send(packet_batch.into())
        {
            self.stats
                .gossip_transmit_packets_dropped_count
                .add_relaxed(packet_batch.len() as u64);
        }
        self.stats
            .gossip_transmit_loop_iterations_since_last_report
            .add_relaxed(1);
        Ok(())
    }

    pub(crate) fn process_entrypoints(&self) -> bool {
        let mut entrypoints = self.entrypoints.write().unwrap();
        if entrypoints.is_empty() {
            // No entrypoint specified.  Nothing more to process
            return true;
        }
        for entrypoint in entrypoints.iter_mut() {
            if entrypoint.pubkey() == &Pubkey::default() {
                // If a pull from the entrypoint was successful it should exist in the CRDS table
                if let Some(entrypoint_from_gossip) = entrypoint
                    .gossip()
                    .and_then(|addr| self.lookup_contact_info_by_gossip_addr(&addr))
                {
                    // Update the entrypoint's id so future entrypoint pulls correctly reference it
                    *entrypoint = entrypoint_from_gossip;
                }
            }
        }
        entrypoints
            .iter()
            .all(|entrypoint| entrypoint.pubkey() != &Pubkey::default())
    }

    pub(crate) fn purge_expired_crds(
        &self,
        thread_pool: &ThreadPool,
        stakes: &HashMap<Pubkey, u64>,
    ) {
        let self_pubkey = self.id();
        let timeouts = self
            .gossip
            .make_timeouts(self_pubkey, stakes, CRDS_GOSSIP_PURGE_DURATION);
        let num_purged = {
            let _st = ScopedTimer::from(&self.stats.purge);
            self.gossip
                .purge(&self_pubkey, thread_pool, timestamp(), &timeouts)
        };
        self.stats.purge_count.add_relaxed(num_purged as u64);
    }

    // Trims the CRDS table by dropping all values associated with the pubkeys
    // with the lowest stake, so that the number of unique pubkeys are bounded.
    fn trim_crds_table(&self, cap: usize, stakes: &HashMap<Pubkey, u64>) {
        if !self.gossip.crds.read().unwrap().should_trim(cap) {
            return;
        }
        let keep: HashSet<_> = self
            .entrypoints
            .read()
            .unwrap()
            .iter()
            .map(ContactInfo::pubkey)
            .copied()
            .chain(std::iter::once(self.id()))
            .chain(self.known_validators.get().into_iter().flatten().copied())
            .collect();
        self.stats.trim_crds_table.add_relaxed(1);
        let mut gossip_crds = self.gossip.crds.write().unwrap();
        let num_purged = gossip_crds.trim(cap, &keep, stakes, timestamp());
        self.stats
            .trim_crds_table_purged_values_count
            .add_relaxed(num_purged as u64);
    }

    fn handle_batch_prune_messages(&self, messages: Vec<PruneData>, stakes: &HashMap<Pubkey, u64>) {
        let _st = ScopedTimer::from(&self.stats.handle_batch_prune_messages_time);
        if messages.is_empty() {
            return;
        }
        self.stats
            .prune_message_len
            .add_relaxed(messages.iter().map(|data| data.prunes.len() as u64).sum());
        let mut prune_message_timeout = 0;
        let mut bad_prune_destination = 0;
        let self_pubkey = self.id();
        {
            let _st = ScopedTimer::from(&self.stats.process_prune);
            let now = timestamp();
            for data in messages {
                match self.gossip.process_prune_msg(
                    &self_pubkey,
                    &data.pubkey,
                    &data.destination,
                    &data.prunes,
                    data.wallclock,
                    now,
                    stakes,
                ) {
                    Err(CrdsGossipError::PruneMessageTimeout) => {
                        prune_message_timeout += 1;
                    }
                    Err(CrdsGossipError::BadPruneDestination) => {
                        bad_prune_destination += 1;
                    }
                    _ => (),
                }
            }
        }
        if prune_message_timeout != 0 {
            self.stats
                .prune_message_timeout
                .add_relaxed(prune_message_timeout);
        }
        if bad_prune_destination != 0 {
            self.stats
                .bad_prune_destination
                .add_relaxed(bad_prune_destination);
        }
    }

    fn handle_batch_pull_requests(
        &self,
        requests: Vec<PullRequest>,
        recycler: &PacketBatchRecycler,
        stakes: &HashMap<Pubkey, u64>,
        response_sender: &impl ChannelSend<PacketBatch>,
    ) {
        let _st = ScopedTimer::from(&self.stats.handle_batch_pull_requests_time);
        if !requests.is_empty() {
            let response = self.handle_pull_requests(recycler, requests, stakes);
            if !response.is_empty()
                && let Err(TrySendError::Full(response)) = response_sender.try_send(response.into())
            {
                self.stats
                    .gossip_packets_dropped_count
                    .add_relaxed(response.len() as u64);
            }
        }
    }

    fn update_data_budget(&self, num_staked: usize) -> usize {
        const INTERVAL_MS: u64 = 100;
        // epoch slots + votes ~= 1.5kB/slot ~= 4kB/s
        // Allow 10kB/s per staked validator.
        const BYTES_PER_INTERVAL: usize = 1024;
        const MAX_BUDGET_MULTIPLE: usize = 5; // allow budget build-up to 5x the interval default
        let num_staked = num_staked.max(2);
        self.outbound_budget.update(INTERVAL_MS, |bytes| {
            std::cmp::min(
                bytes + num_staked * BYTES_PER_INTERVAL,
                MAX_BUDGET_MULTIPLE * num_staked * BYTES_PER_INTERVAL,
            )
        })
    }

    // Returns a predicate checking if the pull request is from a valid
    // address, and if the address have responded to a ping request. Also
    // appends ping packets for the addresses which need to be (re)verified.
    fn check_pull_request<'a, R>(
        &'a self,
        now: Instant,
        rng: &'a mut R,
        packet_batch: &'a mut RecycledPacketBatch,
    ) -> impl FnMut(&PullRequest) -> bool + 'a
    where
        R: Rng + CryptoRng,
    {
        let mut cache = HashMap::<(Pubkey, SocketAddr), bool>::new();
        let mut ping_cache = self.ping_cache.lock().unwrap();
        let mut hard_check = move |node| {
            let (check, ping) = ping_cache.check(rng, &self.keypair(), now, node);
            if let Some(ping) = ping {
                let ping = Protocol::PingMessage(ping);
                if let Some(pkt) = make_gossip_packet(node.1, &ping, &self.stats) {
                    packet_batch.push(pkt);
                }
            }
            if !check {
                self.stats
                    .pull_request_ping_pong_check_failed_count
                    .add_relaxed(1)
            }
            check
        };
        // Because pull-responses are sent back to packet.meta().socket_addr() of
        // incoming pull-requests, pings are also sent to request.from_addr (as
        // opposed to caller.gossip address).
        move |request| {
            ContactInfo::is_valid_address(&request.addr, &self.socket_addr_space) && {
                let node = (request.pubkey, request.addr);
                *cache.entry(node).or_insert_with(|| hard_check(node))
            }
        }
    }

    fn try_consume_pull_request_scan_budget(
        &self,
        request: &PullRequest,
        scan_entries: usize,
    ) -> bool {
        let cost = pull_request_scan_cost(scan_entries, request.filter.bloom_hash_count());
        if self
            .pull_request_budget
            .consume_tokens(request.addr.ip(), cost)
            .is_ok()
        {
            true
        } else {
            self.stats.pull_request_scan_budget_exhausted.add_relaxed(1);
            false
        }
    }

    // Pull requests take an incoming bloom filter of contained entries from a node
    // and tries to send back to them the values it detects are missing.
    fn handle_pull_requests(
        &self,
        recycler: &PacketBatchRecycler,
        mut requests: Vec<PullRequest>,
        stakes: &HashMap<Pubkey, u64>,
    ) -> RecycledPacketBatch {
        let output_size_limit =
            self.update_data_budget(stakes.len()) / PULL_RESPONSE_MIN_SERIALIZED_SIZE;
        let mut packet_batch =
            RecycledPacketBatch::new_with_recycler(recycler, 64, "handle_pull_requests");
        let mut rng = rand::rng();
        requests.retain({
            let now = Instant::now();
            self.check_pull_request(now, &mut rng, &mut packet_batch)
        });
        let now = timestamp();
        let self_id = self.id();
        let is_full_alpenglow_epoch = self.is_full_alpenglow_epoch();
        let pull_responses = {
            let _st = ScopedTimer::from(&self.stats.generate_pull_responses);
            self.gossip.generate_pull_responses(
                &requests,
                output_size_limit,
                now,
                |value| {
                    should_retain_crds_value(
                        value,
                        stakes,
                        GossipFilterDirection::EgressPullResponse,
                        is_full_alpenglow_epoch,
                    )
                },
                |request, scan_entries| {
                    self.try_consume_pull_request_scan_budget(request, scan_entries)
                },
                &self.stats,
            )
        };
        // Prioritize more recent values, staked values and ContactInfos.
        let get_score = |value: &CrdsValue| -> u64 {
            let age = now.saturating_sub(value.wallclock());
            // score CrdsValue: 2x score if staked; 2x score if ContactInfo
            let score = (CRDS_GOSSIP_PURGE_DURATION.as_millis() as u64)
                .saturating_sub(age)
                .div(CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS)
                .max(1);
            let score = if stakes.contains_key(&value.pubkey()) {
                2 * score
            } else {
                score
            };
            match value.data() {
                CrdsData::ContactInfo(_) => 2 * score,
                _ => score,
            }
        };
        let mut num_crds_values = 0;
        let (scores, mut pull_responses): (Vec<_>, Vec<_>) = requests
            .iter()
            .zip(pull_responses)
            .flat_map(|(PullRequest { addr, .. }, values)| {
                num_crds_values += values.len();
                split_gossip_messages(PULL_RESPONSE_MAX_PAYLOAD_SIZE, values).map(move |values| {
                    let score = values.iter().map(get_score).max().unwrap_or_default();
                    (score, (addr, values))
                })
            })
            .collect();
        let (total_bytes, sent_crds_values) = WeightedShuffle::new("handle-pull-requests", scores)
            .shuffle(&mut rng)
            .filter_map(|k| {
                let (addr, values) = &mut pull_responses[k];
                let num_values = values.len();
                let response = Protocol::PullResponse(self_id, std::mem::take(values));
                let packet = make_gossip_packet(*addr, &response, &self.stats)?;
                Some((packet, num_values))
            })
            .take_while(|(packet, _)| {
                if self.outbound_budget.take(packet.meta().size) {
                    true
                } else {
                    self.stats.gossip_pull_request_no_budget.add_relaxed(1);
                    false
                }
            })
            .map(|(packet, num_values)| {
                let num_bytes = packet.meta().size;
                packet_batch.push(packet);
                (num_bytes, num_values)
            })
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        let dropped_responses = num_crds_values.saturating_sub(sent_crds_values);
        self.stats
            .gossip_pull_request_dropped_requests
            .add_relaxed(dropped_responses as u64);
        self.stats
            .gossip_pull_request_sent_bytes
            .add_relaxed(total_bytes as u64);
        packet_batch
    }

    fn handle_batch_pull_responses(
        &self,
        responses: Vec<CrdsValue>,
        stakes: &HashMap<Pubkey, u64>,
    ) {
        let _st = ScopedTimer::from(&self.stats.handle_batch_pull_responses_time);
        if !responses.is_empty() {
            let self_pubkey = self.id();
            let timeouts =
                self.gossip
                    .make_timeouts(self_pubkey, stakes, CRDS_GOSSIP_PURGE_DURATION);
            self.handle_pull_response(responses, &timeouts);
        }
    }

    // Returns (failed, timeout, success)
    fn handle_pull_response(
        &self,
        crds_values: Vec<CrdsValue>,
        timeouts: &CrdsTimeouts,
    ) -> (usize, usize, usize) {
        let len = crds_values.len();
        let mut pull_stats = ProcessPullStats::default();
        let (filtered_pulls, filtered_pulls_expired_timeout, failed_inserts) = {
            let _st = ScopedTimer::from(&self.stats.filter_pull_response);
            self.gossip
                .filter_pull_responses(timeouts, crds_values, timestamp(), &mut pull_stats)
        };
        if !filtered_pulls.is_empty()
            || !filtered_pulls_expired_timeout.is_empty()
            || !failed_inserts.is_empty()
        {
            let _st = ScopedTimer::from(&self.stats.process_pull_response);
            self.gossip.process_pull_responses(
                filtered_pulls,
                filtered_pulls_expired_timeout,
                failed_inserts,
                timestamp(),
                &mut pull_stats,
            );
        }
        self.stats.process_pull_response_count.add_relaxed(1);
        self.stats.process_pull_response_len.add_relaxed(len as u64);
        self.stats
            .process_pull_response_fail_insert
            .add_relaxed(pull_stats.failed_insert as u64);
        self.stats
            .process_pull_response_fail_timeout
            .add_relaxed(pull_stats.failed_timeout as u64);
        self.stats
            .process_pull_response_success
            .add_relaxed(pull_stats.success as u64);

        (
            pull_stats.failed_insert,
            pull_stats.failed_timeout,
            pull_stats.success,
        )
    }

    fn handle_batch_ping_messages<S: Borrow<SocketAddr>>(
        &self,
        pings: impl IntoIterator<Item = (S, Ping), IntoIter: ExactSizeIterator>,
        recycler: &PacketBatchRecycler,
        response_sender: &impl ChannelSend<PacketBatch>,
    ) {
        let _st = ScopedTimer::from(&self.stats.handle_batch_ping_messages_time);
        let keypair = self.keypair();
        let pongs = pings.into_iter().map(|(addr, ping)| {
            let pong = Pong::new(&ping, &keypair);
            (addr, Protocol::PongMessage(pong))
        });
        send_gossip_packets(pongs, recycler, response_sender, &self.stats);
    }

    fn handle_batch_pong_messages<I>(&self, pongs: I, now: Instant)
    where
        I: IntoIterator<Item = (SocketAddr, Pong)>,
    {
        let _st = ScopedTimer::from(&self.stats.handle_batch_pong_messages_time);
        let mut pongs = pongs.into_iter().peekable();
        if pongs.peek().is_some() {
            let mut ping_cache = self.ping_cache.lock().unwrap();
            for (addr, pong) in pongs {
                ping_cache.add(&pong, addr, now);
            }
        }
    }

    fn handle_batch_push_messages(
        &self,
        messages: Vec<(Pubkey, Vec<CrdsValue>)>,
        thread_pool: &ThreadPool,
        recycler: &PacketBatchRecycler,
        stakes: &HashMap<Pubkey, u64>,
        response_sender: &impl ChannelSend<PacketBatch>,
    ) {
        let _st = ScopedTimer::from(&self.stats.handle_batch_push_messages_time);
        if messages.is_empty() {
            return;
        }
        // Origins' pubkeys of upserted crds values.
        let origins: HashSet<_> = {
            let _st = ScopedTimer::from(&self.stats.process_push_message);
            let now = timestamp();
            self.gossip.process_push_message(messages, now)
        };
        // Generate prune messages.
        let prune_messages = self.generate_prune_messages(thread_pool, origins, stakes);
        let mut packet_batch = make_gossip_packet_batch(prune_messages, recycler, &self.stats);
        self.new_push_requests(stakes)
            .filter_map(|(addr, data)| make_gossip_packet(addr, &data, &self.stats))
            .for_each(|pkt| packet_batch.push(pkt));
        if !packet_batch.is_empty()
            && let Err(TrySendError::Full(packet_batch)) =
                response_sender.try_send(packet_batch.into())
        {
            self.stats
                .gossip_packets_dropped_count
                .add_relaxed(packet_batch.len() as u64);
        }
    }

    fn generate_prune_messages(
        &self,
        thread_pool: &ThreadPool,
        // Unique origin pubkeys of upserted CRDS values from push messages.
        origins: impl IntoIterator<Item = Pubkey>,
        stakes: &HashMap<Pubkey, u64>,
    ) -> Vec<(SocketAddr, Protocol /*::PruneMessage*/)> {
        let _st = ScopedTimer::from(&self.stats.generate_prune_messages);
        let self_keypair = self.keypair();
        let self_pubkey = self_keypair.pubkey();
        // Obtain redundant gossip links which can be pruned.
        let prunes: HashMap</*gossip peer:*/ Pubkey, /*origins:*/ Vec<Pubkey>> = {
            let _st = ScopedTimer::from(&self.stats.prune_received_cache);
            self.gossip
                .prune_received_cache(&self_pubkey, origins, stakes)
        };
        // Look up gossip addresses of destination nodes.
        let prunes: Vec<(
            Pubkey,      // gossip peer to be pruned
            SocketAddr,  // gossip socket-addr of peer
            Vec<Pubkey>, // CRDS value origins
        )> = {
            let gossip_crds = self.gossip.crds.read().unwrap();
            thread_pool.install(|| {
                prunes
                    .into_par_iter()
                    .filter_map(|(pubkey, prunes)| {
                        let addr = get_node_addr(
                            pubkey,
                            ContactInfo::gossip,
                            &gossip_crds,
                            &self.socket_addr_space,
                        )?;
                        Some((pubkey, addr, prunes))
                    })
                    .collect()
            })
        };
        // Create and sign Protocol::PruneMessages.
        thread_pool.install(|| {
            let wallclock = timestamp();
            prunes
                .into_par_iter()
                .flat_map(|(destination, addr, prunes)| {
                    // Chunk up origins so that each chunk fits into a packet.
                    let prunes = prunes.into_par_iter().chunks(MAX_PRUNE_DATA_NODES);
                    rayon::iter::repeat((destination, addr)).zip(prunes)
                })
                .map(|((destination, addr), prunes)| {
                    let mut prune_data = PruneData {
                        pubkey: self_pubkey,
                        prunes,
                        signature: Signature::default(),
                        destination,
                        wallclock,
                    };
                    prune_data.sign(&self_keypair);
                    let prune_message = Protocol::PruneMessage(self_pubkey, prune_data);
                    (addr, prune_message)
                })
                .collect()
        })
    }

    pub(crate) fn handle_validated_messages(
        &self,
        message_batches: &mut Vec<Vec<ValidatedGossipMessage>>,
        thread_pool: &ThreadPool,
        recycler: &PacketBatchRecycler,
        outbound_sender: &impl ChannelSend<PacketBatch>,
        stakes: &HashMap<Pubkey, u64>,
        should_check_duplicate_instance: bool,
    ) -> Result<(), GossipError> {
        let _st = ScopedTimer::from(&self.stats.process_gossip_packets_time);
        let self_keypair = self.keypair();
        let self_pubkey = self_keypair.pubkey();
        // Filter out values if the shred-versions are different.
        let self_shred_version = self.my_shred_version();
        {
            let gossip_crds = self.gossip.crds.read().unwrap();
            let discard_different_shred_version = |message| {
                discard_different_shred_version(
                    message,
                    self_shred_version,
                    &gossip_crds,
                    &self.stats,
                )
            };
            if message_batches.len() < 4 && message_batches.iter().map(Vec::len).sum::<usize>() < 16
            {
                for message in message_batches.iter_mut().flatten() {
                    discard_different_shred_version(message);
                }
            } else {
                thread_pool.install(|| {
                    message_batches
                        .par_iter_mut()
                        .flatten()
                        .for_each(discard_different_shred_version)
                })
            }
        }
        // Check if there is a duplicate instance of
        // this node with more recent timestamp.
        let check_duplicate_instance = {
            let my_contact_info = self.my_contact_info();
            move |values: &[CrdsValue]| {
                let mut nodes = values.iter().filter_map(CrdsValue::contact_info);
                if nodes.any(|other| my_contact_info.check_duplicate(other)) {
                    Err(GossipError::DuplicateNodeInstance)
                } else {
                    Ok(())
                }
            }
        };
        let mut pings = Vec::new();
        let mut rng = rand::rng();
        let mut verify_gossip_addr = |value: &CrdsValue| {
            if verify_gossip_addr(
                &mut rng,
                &self_keypair,
                value,
                &self.socket_addr_space,
                &self.ping_cache,
                &mut pings,
            ) {
                true
            } else {
                self.stats.num_unverifed_gossip_addrs.add_relaxed(1);
                false
            }
        };
        // Split messages based on their protocol variants.
        let mut pull_requests = vec![];
        let mut pull_responses = vec![];
        let mut push_messages = vec![];
        let mut prune_messages = vec![];
        let mut ping_messages = vec![];
        let mut pong_messages = vec![];
        for message in message_batches.drain(..).flatten() {
            let (from_addr, protocol) = message.into_parts();
            match protocol {
                Protocol::PullRequest(filter, caller) => {
                    if !check_pull_request_shred_version(self_shred_version, &caller) {
                        self.stats.skip_pull_shred_version.add_relaxed(1);
                        continue;
                    }
                    let request = PullRequest {
                        pubkey: caller.pubkey(),
                        addr: from_addr,
                        wallclock: caller.wallclock(),
                        filter,
                    };
                    if request.pubkey == self_pubkey {
                        self.stats.window_request_loopback.add_relaxed(1);
                    } else {
                        pull_requests.push(request);
                    }
                }
                Protocol::PullResponse(_, mut data) => {
                    if should_check_duplicate_instance {
                        check_duplicate_instance(&data)?;
                    }
                    data.retain(&mut verify_gossip_addr);
                    if !data.is_empty() {
                        pull_responses.append(&mut data);
                    }
                }
                Protocol::PushMessage(from, mut data) => {
                    if should_check_duplicate_instance {
                        check_duplicate_instance(&data)?;
                    }
                    data.retain(&mut verify_gossip_addr);
                    if !data.is_empty() {
                        self.stats
                            .push_message_value_count
                            .add_relaxed(data.len() as u64);
                        push_messages.push((from, data));
                    }
                }
                Protocol::PruneMessage(_from, data) => prune_messages.push(data),
                Protocol::PingMessage(ping) => ping_messages.push((from_addr, ping)),
                Protocol::PongMessage(pong) => pong_messages.push((from_addr, pong)),
            }
        }
        let pings = pings
            .into_iter()
            .map(|(addr, ping)| (addr, Protocol::PingMessage(ping)));
        send_gossip_packets(pings, recycler, outbound_sender, &self.stats);
        self.handle_batch_ping_messages(ping_messages, recycler, outbound_sender);
        self.handle_batch_prune_messages(prune_messages, stakes);
        self.handle_batch_push_messages(
            push_messages,
            thread_pool,
            recycler,
            stakes,
            outbound_sender,
        );
        self.handle_batch_pull_responses(pull_responses, stakes);
        self.trim_crds_table(CRDS_UNIQUE_PUBKEY_CAPACITY, stakes);
        self.handle_batch_pong_messages(pong_messages, Instant::now());
        self.handle_batch_pull_requests(pull_requests, recycler, stakes, outbound_sender);
        Ok(())
    }

    // Validates packets received from the socket and forwards decoded messages.
    fn validate_ingress(
        &self,
        thread_pool: &ThreadPool,
        policy: &GossipPolicy,
        packet_receiver: &PacketBatchReceiver,
        validated_sender: &impl ChannelSend<Vec<ValidatedGossipMessage>>,
        packet_batches: &mut Vec<PacketBatch>,
    ) -> Result<(), GossipError> {
        let mut num_packets = 0;
        for packet_batch in packet_receiver
            .recv()
            .map(std::iter::once)?
            .chain(packet_receiver.try_iter())
        {
            num_packets += packet_batch.len();
            packet_batches.push(packet_batch);
            if packet_batches.len() == MAX_PACKET_BATCHES_PER_VALIDATION {
                break;
            }
        }
        self.stats
            .packets_received_count
            .add_relaxed(num_packets as u64);
        fn validate_packet(
            packet: PacketRef,
            stakes: &HashMap<Pubkey, u64>,
            stats: &GossipStats,
            sigverify_cache: &SigVerifyCache,
            is_full_alpenglow_epoch: bool,
        ) -> Option<ValidatedGossipMessage> {
            let result: wincode::ReadResult<Protocol> = packet
                .data(..)
                .ok_or(wincode::ReadError::Custom("packet discarded"))
                .and_then(deserialize_protocol);
            let protocol: Protocol = stats.record_received_packet(result)?;
            ValidatedGossipMessage::new(
                packet.meta().socket_addr(),
                protocol,
                stakes,
                is_full_alpenglow_epoch,
                sigverify_cache,
            )
            .inspect(|_| {
                stats.packets_received_verified_count.add_relaxed(1);
            })
        }
        let snapshot = policy.load();
        let stakes = snapshot.stakes.as_ref();
        let is_full_alpenglow_epoch = snapshot.is_full_alpenglow_epoch;
        let validate_packet = |packet| {
            validate_packet(
                packet,
                stakes,
                &self.stats,
                &self.sigverify_cache,
                is_full_alpenglow_epoch,
            )
        };
        let validated_messages: Vec<_> = {
            let _st = ScopedTimer::from(&self.stats.verify_gossip_packets_time);
            if num_packets < MIN_PARALLEL_GOSSIP_PACKETS {
                packet_batches
                    .iter()
                    .flatten()
                    .filter_map(&validate_packet)
                    .collect()
            } else {
                thread_pool.install(|| {
                    if packet_batches.len() == 1 {
                        packet_batches[0]
                            .par_iter()
                            .filter_map(&validate_packet)
                            .collect()
                    } else {
                        packet_batches
                            .par_iter()
                            .flatten()
                            .filter_map(&validate_packet)
                            .collect()
                    }
                })
            }
        };
        if let Err(TrySendError::Full(_)) = validated_sender.try_send(validated_messages) {
            self.stats.gossip_packets_dropped_count.add_relaxed(
                packet_batches
                    .iter()
                    .fold(0, |acc, packet_batch| acc + packet_batch.len()) as u64,
            );
        }
        packet_batches.clear();
        Ok(())
    }

    pub(crate) fn start_ingress_validation_thread(
        self: Arc<Self>,
        thread_pool: Arc<ThreadPool>,
        policy: Arc<GossipPolicy>,
        packet_receiver: PacketBatchReceiver,
        validated_sender: impl ChannelSend<Vec<ValidatedGossipMessage>>,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        let mut packet_batches = Vec::with_capacity(MAX_PACKET_BATCHES_PER_VALIDATION);
        let validate_ingress = move || {
            while !exit.load(Ordering::Relaxed) {
                let result = self.validate_ingress(
                    &thread_pool,
                    &policy,
                    &packet_receiver,
                    &validated_sender,
                    &mut packet_batches,
                );
                match result {
                    // A recv operation can only fail if the sending end of a
                    // channel is disconnected.
                    Err(GossipError::RecvError(_)) => break,
                    // A send operation can only fail if the receiving end of a
                    // channel is disconnected.
                    Err(GossipError::SendError) => break,
                    Err(err) => error!("gossip consume: {err}"),
                    Ok(()) => (),
                }
            }
        };
        let thread_name = String::from("solGossipValid");
        Builder::new()
            .name(thread_name)
            .spawn(validate_ingress)
            .unwrap()
    }

    pub fn gossip_contact_info(id: Pubkey, gossip: SocketAddr, shred_version: u16) -> ContactInfo {
        let mut node = ContactInfo::new(id, /*wallclock:*/ timestamp(), shred_version);
        let _ = node.set_gossip(gossip);
        node
    }

    /// An alternative to Spy Node that has a valid gossip address and fully participate in Gossip.
    pub fn gossip_node(
        id: Pubkey,
        gossip_addr: &SocketAddr,
        shred_version: u16,
    ) -> (ContactInfo, UdpSocket, Option<TcpListener>) {
        let bind_ip_addr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let (port, (gossip_socket, ip_echo)) =
            bind_gossip_port_in_range(gossip_addr, VALIDATOR_PORT_RANGE, bind_ip_addr);
        let contact_info =
            Self::gossip_contact_info(id, SocketAddr::new(gossip_addr.ip(), port), shred_version);

        (contact_info, gossip_socket, Some(ip_echo))
    }

    /// A Node with dummy ports to spy on gossip via pull requests
    pub fn spy_node(
        id: Pubkey,
        shred_version: u16,
    ) -> (ContactInfo, UdpSocket, Option<TcpListener>) {
        let bind_ip_addr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let (port, gossip_socket) = bind_in_range(bind_ip_addr, VALIDATOR_PORT_RANGE).unwrap();
        let contact_info = Self::gossip_contact_info(
            id,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            shred_version,
        );
        (contact_info, gossip_socket, None)
    }
}

#[derive(Debug)]
pub struct Sockets {
    pub gossip: Arc<[UdpSocket]>,     // udp read/write
    pub ip_echo: Option<TcpListener>, // read/write (tcp)
    pub tvu: Vec<UdpSocket>,          // udp read only
    pub tpu_vote: Vec<UdpSocket>,     // udp read only
    pub broadcast: Vec<UdpSocket>,    // udp write only
    // Socket sending out local repair requests,
    // and receiving repair responses from the cluster.
    pub repair: UdpSocket,                  // udp read/write
    pub retransmit_sockets: Vec<UdpSocket>, // udp write only
    // Socket receiving remote repair requests from the cluster,
    // and sending back repair responses.
    pub serve_repair: UdpSocket, // udp read/write
    // Socket sending out local RepairProtocol::AncestorHashes,
    // and receiving AncestorHashesResponse from the cluster.
    pub ancestor_hashes_requests: UdpSocket, // udp read/write
    pub tpu_quic: Vec<UdpSocket>,            // quic read only
    pub tpu_forwards_quic: Vec<UdpSocket>,   // quic read only
    pub tpu_vote_quic: Vec<UdpSocket>,       // quic read only
    // Socket sending out BlockIdRepairType requests,
    // and receiving BlockIdRepairResponse from the cluster.
    pub block_id_repair: UdpSocket,

    /// Client-side socket for ForwardingStage vote transactions
    pub tpu_vote_forwarding_client: UdpSocket, // udp write only
    /// Client-side socket for ForwardingStage non-vote transactions
    pub tpu_transaction_forwarding_clients: Box<[UdpSocket]>, // quic write only
    /// UDP sockets for the alpenglow votor QUIC server endpoints.
    pub votor_server: Vec<UdpSocket>, // quic read only
    /// Connection cache endpoint for QUIC-based Vote
    pub quic_vote_client: UdpSocket, // quic write only
    /// Client-side socket for the alpenglow votor QUIC endpoint.
    pub quic_votor_client: UdpSocket, // quic write only
    /// Client-side socket for RPC/SendTransactionService.
    pub rpc_sts_client: UdpSocket, // quic write only
}

pub struct NodeConfig {
    /// The IP address advertised to the cluster in gossip
    pub advertised_ip: IpAddr,
    /// The gossip port advertised to the cluster
    pub gossip_port: u16,
    pub port_range: PortRange,
    /// Multihoming: The IP addresses the node can bind to
    pub bind_ip_addrs: BindIpAddrs,
    pub public_tpu_addr: Option<SocketAddr>,
    pub public_tpu_forwards_addr: Option<SocketAddr>,
    pub public_tvu_addr: Option<SocketAddr>,

    /// The number of TVU receive sockets to create
    pub num_tvu_receive_sockets: NonZeroUsize,
    /// The number of TVU retransmit sockets to create
    pub num_tvu_retransmit_sockets: NonZeroUsize,
    /// The number of QUIC tpu endpoints
    pub num_quic_endpoints: NonZeroUsize,
    /// The number of QUIC endpoints for the votor transport.
    pub num_votor_quic_endpoints: NonZeroUsize,
}

pub fn push_messages_to_peer_for_tests(
    messages: Vec<CrdsValue>,
    self_id: Pubkey,
    peer_gossip: SocketAddr,
    socket_addr_space: &SocketAddrSpace,
) -> Result<(), GossipError> {
    let requests: Vec<_> = split_gossip_messages(PUSH_MESSAGE_MAX_PAYLOAD_SIZE, messages)
        .map(move |payload| (peer_gossip, Protocol::PushMessage(self_id, payload)))
        .collect();
    let packet_batch = make_gossip_packet_batch(
        requests,
        &PacketBatchRecycler::default(),
        &GossipStats::default(),
    );
    let sock = bind_to_localhost_unique().expect("should bind");
    packet::send_to(&packet_batch, &sock, socket_addr_space)?;
    Ok(())
}

/// Serializes `nodes` to a sibling temp file and renames it over `filename`.
/// Returns whether the file was replaced; failures are logged, not propagated.
fn write_contact_info_file(filename: &Path, nodes: &[CrdsValue]) -> bool {
    let tmp_filename = &filename.with_extension("tmp");
    match File::create(tmp_filename) {
        Ok(file) => {
            let mut writer = BufWriter::new(file);
            if let Err(err) =
                wincode::config::serialize_into(&mut writer, &nodes, ContactInfoFileConfig::new())
            {
                warn!(
                    "Failed to serialize contact info info {}: {}",
                    tmp_filename.display(),
                    err
                );
                return false;
            }
            if let Err(err) = writer.flush() {
                warn!("Failed to save contact info: {err}");
            }
        }
        Err(err) => {
            warn!("Failed to create {}: {}", tmp_filename.display(), err);
            return false;
        }
    }
    if let Err(err) = fs::rename(tmp_filename, filename) {
        warn!(
            "Failed to rename {} to {}: {}",
            tmp_filename.display(),
            filename.display(),
            err
        );
        return false;
    }
    true
}

/// Appends a `---+---` rule matching the column layout of the header already
/// in `output`.
fn push_header_rule(output: &mut String) {
    let rule: String = output
        .chars()
        .map(|c| match c {
            '|' => '+',
            '\n' => '\n',
            _ => '-',
        })
        .collect();
    output.push_str(&rule);
}

// Checks shred-version of a pull-request caller and returns false if the
// pull-request should be ignored and discarded.
#[inline]
#[must_use]
fn check_pull_request_shred_version(self_shred_version: u16, caller: &CrdsValue) -> bool {
    let shred_version = match caller.data() {
        CrdsData::ContactInfo(node) => node.shred_version(),
        _ => return false,
    };
    shred_version == self_shred_version
}

// Discards CrdsValues in PushMessages and PullResponses from nodes with
// different shred-version.
fn discard_different_shred_version(
    message: &mut ValidatedGossipMessage,
    self_shred_version: u16,
    crds: &Crds,
    stats: &GossipStats,
) {
    let skip_shred_version_counter = match message.protocol() {
        Protocol::PullResponse(..) => &stats.skip_pull_response_shred_version,
        Protocol::PushMessage(..) => &stats.skip_push_message_shred_version,
        // Shred-version on pull-request callers can be checked without a lock
        // on CRDS table and is so verified separately (by
        // check_pull_request_shred_version).
        // No CRDS values in Prune, Ping and Pong messages.
        Protocol::PullRequest(..)
        | Protocol::PruneMessage(..)
        | Protocol::PingMessage(_)
        | Protocol::PongMessage(_) => return,
    };
    let num_skipped = message.retain_crds_values(|value| match value.data() {
        CrdsData::ContactInfo(ci) => ci.shred_version() == self_shred_version,
        // for any other CRDS types we check if we store anything already
        // for this pubkey, if we do we allow more values in
        _ => crds.get_records(&value.pubkey()).next().is_some(),
    });
    if num_skipped != 0 {
        skip_shred_version_counter.add_relaxed(num_skipped as u64);
    }
}

#[inline]
fn get_node_addr(
    pubkey: Pubkey,
    query: impl Fn(&ContactInfo) -> Option<SocketAddr>,
    crds: &Crds,
    socket_addr_space: &SocketAddrSpace,
) -> Option<SocketAddr> {
    let node = crds.get::<&ContactInfo>(pubkey)?;
    query(node).filter(|addr| socket_addr_space.check(addr))
}

// If the CRDS value is an unstaked contact-info, verifies if
// it has responded to ping on its gossip socket address.
// Returns false if the CRDS value should be discarded.
#[must_use]
fn verify_gossip_addr<R: Rng + CryptoRng>(
    rng: &mut R,
    keypair: &Keypair,
    value: &CrdsValue,
    socket_addr_space: &SocketAddrSpace,
    ping_cache: &Mutex<PingCache>,
    pings: &mut Vec<(SocketAddr, Ping)>,
) -> bool {
    let (pubkey, addr) = match value.data() {
        CrdsData::ContactInfo(node) => (node.pubkey(), node.gossip()),
        _ => return true, // If not a contact-info, nothing to verify.
    };
    // Invalid addresses are not verifiable.
    let Some(addr) = addr.filter(|addr| socket_addr_space.check(addr)) else {
        return false;
    };
    let (out, ping) = {
        let node = (*pubkey, addr);
        let mut ping_cache = ping_cache.lock().unwrap();
        ping_cache.check(rng, keypair, Instant::now(), node)
    };
    if let Some(ping) = ping {
        pings.push((addr, ping));
    }
    out
}

fn send_gossip_packets<S: Borrow<SocketAddr>>(
    pkts: impl IntoIterator<Item = (S, Protocol), IntoIter: ExactSizeIterator>,
    recycler: &PacketBatchRecycler,
    sender: &impl ChannelSend<PacketBatch>,
    stats: &GossipStats,
) {
    let pkts = pkts.into_iter();
    if pkts.len() != 0 {
        let pkts = make_gossip_packet_batch(pkts, recycler, stats);
        if let Err(TrySendError::Full(pkts)) = sender.try_send(pkts.into()) {
            stats
                .gossip_packets_dropped_count
                .add_relaxed(pkts.len() as u64);
        }
    }
}

fn make_gossip_packet_batch<S: Borrow<SocketAddr>>(
    pkts: impl IntoIterator<Item = (S, Protocol), IntoIter: ExactSizeIterator>,
    recycler: &PacketBatchRecycler,
    stats: &GossipStats,
) -> RecycledPacketBatch {
    let pkts = pkts.into_iter();
    let mut batch =
        RecycledPacketBatch::new_with_recycler(recycler, pkts.len(), "gossip_packet_batch");
    for (addr, pkt) in pkts {
        let addr = addr.borrow();
        if !addr.ip().is_unspecified() && addr.port() != 0 {
            if let Some(packet) = make_gossip_packet(addr, &pkt, stats) {
                batch.push(packet);
            }
        } else {
            trace!("Dropping packet, as destination is unknown");
        }
    }
    batch
}

#[inline]
fn make_gossip_packet(
    addr: impl Borrow<SocketAddr>,
    pkt: &Protocol,
    stats: &GossipStats,
) -> Option<Packet> {
    let mut packet = Packet::default();
    let size = {
        let buffer = packet.buffer_mut();
        let initial_len = buffer.len();
        let mut writer: &mut [u8] = buffer;
        if let Err(err) = wincode::serialize_into(&mut writer, pkt) {
            error!("failed to write gossip packet: {err:?}");
            return None;
        }
        initial_len - writer.len()
    };
    let meta = packet.meta_mut();
    meta.size = size;
    meta.set_socket_addr(addr.borrow());
    stats.record_gossip_packet(pkt);
    Some(packet)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            crds_gossip_pull::tests::MIN_NUM_BLOOM_FILTERS,
            crds_value::{CrdsValue, CrdsValueLabel},
            duplicate_shred::tests::new_rand_shred,
            node::Node,
            protocol::tests::new_rand_remote_node,
            socketaddr,
        },
        itertools::izip,
        rand::{SeedableRng, rngs::StdRng},
        rayon::ThreadPoolBuilder,
        solana_keypair::Keypair,
        solana_ledger::shred::Shredder,
        solana_net_utils::sockets::localhost_port_range_for_tests,
        solana_sanitize::Sanitize,
        solana_signer::Signer,
        solana_streamer::quic::DEFAULT_QUIC_ENDPOINTS,
        solana_vote_program::{
            vote_instruction,
            vote_state::{MAX_LOCKOUT_HISTORY, Vote},
        },
        std::{
            iter::repeat_with,
            net::{IpAddr, Ipv4Addr},
            ops::Deref,
            panic,
            sync::Arc,
            thread::sleep,
        },
    };

    fn test_message(protocol: Protocol) -> ValidatedGossipMessage {
        ValidatedGossipMessage::new_unchecked("127.0.0.1:1234".parse().unwrap(), protocol)
    }

    const DEFAULT_NUM_QUIC_ENDPOINTS: NonZeroUsize =
        NonZeroUsize::new(DEFAULT_QUIC_ENDPOINTS).unwrap();

    impl ClusterInfo {
        // Wrapper for ClusterInfo.new_pull_requests replicating old return
        // type for legacy tests.
        #[allow(clippy::type_complexity)]
        fn old_pull_requests(
            &self,
            thread_pool: &ThreadPool,
            gossip_validators: Option<&HashSet<Pubkey>>,
            stakes: &HashMap<Pubkey, u64>,
        ) -> (
            Vec<(SocketAddr, Ping)>,     // Ping packets
            Vec<(SocketAddr, Protocol)>, // Pull requests
        ) {
            self.new_pull_requests(thread_pool, gossip_validators, stakes)
                .partition_map(|(addr, protocol)| {
                    if let Protocol::PingMessage(ping) = protocol {
                        Either::Left((addr, ping))
                    } else {
                        Either::Right((addr, protocol))
                    }
                })
        }
    }

    #[test]
    fn test_pull_request_scan_cost() {
        assert_eq!(pull_request_scan_cost(0, 0), 8);
        assert_eq!(pull_request_scan_cost(16, 8), 128);
        assert_eq!(pull_request_scan_cost(16, 12), 192);
        assert_eq!(pull_request_scan_cost(2, 8), 16);
    }

    #[test]
    fn test_pull_request_scan_budget_production_config() {
        assert_eq!(GOSSIP_PULL_SCAN_BUDGET_CAPACITY, 1_048_576);
        assert_eq!(GOSSIP_PULL_SCAN_BUDGET_REFILL_PER_SEC, 262_144);
    }

    #[test]
    fn test_pull_request_scan_budget_exhaustion_isolated_per_ip() {
        let keypair = Arc::new(Keypair::new());
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), timestamp());
        let mut cluster_info =
            ClusterInfo::new(contact_info, keypair, SocketAddrSpace::Unspecified);
        // Keep the test independent of wall-clock refill.
        cluster_info.pull_request_budget = KeyedRateLimiter::new(
            GOSSIP_PULL_SCAN_BUDGET_CACHE_CAPACITY,
            TokenBucket::new(
                GOSSIP_PULL_SCAN_BUDGET_CAPACITY,
                GOSSIP_PULL_SCAN_BUDGET_CAPACITY,
                f64::MIN_POSITIVE,
            ),
            GOSSIP_PULL_SCAN_BUDGET_SHARD_COUNT,
        );
        let request = PullRequest {
            pubkey: Pubkey::new_unique(),
            addr: SocketAddr::from(([127, 0, 0, 1], 12_345)),
            wallclock: timestamp(),
            filter: CrdsFilter::new_rand(
                crds_gossip_pull::MIN_NUM_BLOOM_ITEMS,
                solana_packet::PACKET_DATA_SIZE,
            ),
        };
        let second_ip_request = PullRequest {
            pubkey: request.pubkey,
            addr: SocketAddr::from(([127, 0, 0, 2], request.addr.port())),
            wallclock: request.wallclock,
            filter: request.filter.clone(),
        };
        request.filter.sanitize().unwrap();
        let crds_len = crds_gossip_pull::MIN_NUM_BLOOM_ITEMS;
        let cost = pull_request_scan_cost(crds_len, request.filter.bloom_hash_count());
        let successful_requests = GOSSIP_PULL_SCAN_BUDGET_CAPACITY / cost;

        assert!(cost <= GOSSIP_PULL_SCAN_BUDGET_CAPACITY);
        assert_eq!(
            cluster_info
                .stats
                .pull_request_scan_budget_exhausted
                .load_relaxed(),
            0,
        );
        for _ in 0..successful_requests {
            assert!(cluster_info.try_consume_pull_request_scan_budget(&request, crds_len));
        }
        assert!(!cluster_info.try_consume_pull_request_scan_budget(&request, crds_len));
        assert!(cluster_info.try_consume_pull_request_scan_budget(&second_ip_request, crds_len));
        assert_eq!(
            cluster_info
                .stats
                .pull_request_scan_budget_exhausted
                .load_relaxed(),
            1,
        );
    }

    #[test]
    fn test_gossip_node() {
        //check that a gossip nodes always show up as spies
        let (node, _, _) = ClusterInfo::spy_node(solana_pubkey::new_rand(), 0);
        assert!(ClusterInfo::is_spy_node(
            &node,
            &SocketAddrSpace::Unspecified
        ));
        let (node, _, _) =
            ClusterInfo::gossip_node(solana_pubkey::new_rand(), &"1.1.1.1:0".parse().unwrap(), 0);
        assert!(ClusterInfo::is_spy_node(
            &node,
            &SocketAddrSpace::Unspecified
        ));
    }

    #[test]
    fn test_handle_pull() {
        agave_logger::setup();
        let cluster_info = Arc::new({
            let keypair = Arc::new(Keypair::new());
            let node = Node::new_localhost_with_pubkey(&keypair.pubkey());
            ClusterInfo::new(node.info, keypair, SocketAddrSpace::Unspecified)
        });
        let stakes = HashMap::from([(Pubkey::new_unique(), 1u64)]);
        let timeouts = CrdsTimeouts::new(
            cluster_info.id(),
            CRDS_GOSSIP_PULL_CRDS_TIMEOUT_MS, // default_timeout
            CRDS_GOSSIP_PURGE_DURATION,
            &stakes,
        );

        // Generate CRDS value w/ expired timestamp.
        let entrypoint_pubkey = solana_pubkey::new_rand();
        let entrypoint_ci = ContactInfo::new_localhost(&entrypoint_pubkey, 0);
        let entrypoint_crdsvalue = CrdsValue::new_unsigned(CrdsData::from(entrypoint_ci));
        let data = vec![entrypoint_crdsvalue];

        // Should fail for expired timestamp.
        assert_eq!(
            (1, 1, 0),
            cluster_info.handle_pull_response(data, &timeouts)
        );

        // Expect pull response to be successfully inserted.
        let data = test_crds_values(entrypoint_pubkey);
        assert_eq!(
            (0, 0, 1),
            cluster_info.handle_pull_response(data.clone(), &timeouts)
        );

        // Should fail insertion because it was already inserted.
        assert_eq!(
            (1, 0, 0),
            cluster_info.handle_pull_response(data, &timeouts)
        );
    }

    #[test]
    fn test_handle_pong_messages() {
        let now = Instant::now();
        let mut rng = rand::rng();
        let this_node = Arc::new(Keypair::new());
        let cluster_info = ClusterInfo::new(
            ContactInfo::new_localhost(&this_node.pubkey(), timestamp()),
            this_node.clone(),
            SocketAddrSpace::Unspecified,
        );
        let remote_nodes: Vec<(Keypair, SocketAddr)> =
            repeat_with(|| new_rand_remote_node(&mut rng))
                .take(128)
                .collect();
        let pings: Vec<_> = {
            let mut ping_cache = cluster_info.ping_cache.lock().unwrap();
            remote_nodes
                .iter()
                .map(|(keypair, socket)| {
                    let node = (keypair.pubkey(), *socket);
                    let (check, ping) = ping_cache.check(&mut rng, &this_node, now, node);
                    // Assert that initially remote nodes will not pass the
                    // ping/pong check.
                    assert!(!check);
                    ping.unwrap()
                })
                .collect()
        };
        let pongs: Vec<(SocketAddr, Pong)> = pings
            .iter()
            .zip(&remote_nodes)
            .map(|(ping, (keypair, socket))| (*socket, Pong::new(ping, keypair)))
            .collect();
        let now = now + Duration::from_millis(1);
        cluster_info.handle_batch_pong_messages(pongs, now);
        // Assert that remote nodes now pass the ping/pong check.
        {
            let mut ping_cache = cluster_info.ping_cache.lock().unwrap();
            for (keypair, socket) in &remote_nodes {
                let node = (keypair.pubkey(), *socket);
                let (check, _) = ping_cache.check(&mut rng, &this_node, now, node);
                assert!(check);
            }
        }
        // Assert that a new random remote node still will not pass the check.
        {
            let mut ping_cache = cluster_info.ping_cache.lock().unwrap();
            let (keypair, socket) = new_rand_remote_node(&mut rng);
            let node = (keypair.pubkey(), socket);
            let (check, _) = ping_cache.check(&mut rng, &this_node, now, node);
            assert!(!check);
        }
    }

    #[test]
    fn test_handle_ping_messages() {
        let mut rng = rand::rng();
        let this_node = Arc::new(Keypair::new());
        let cluster_info = ClusterInfo::new(
            ContactInfo::new_localhost(&this_node.pubkey(), timestamp()),
            this_node.clone(),
            SocketAddrSpace::Unspecified,
        );
        let remote_nodes: Vec<(Keypair, SocketAddr)> =
            repeat_with(|| new_rand_remote_node(&mut rng))
                .filter(|(_, socket)| socket.port() != 0)
                .take(128)
                .collect();
        let pings: Vec<_> = remote_nodes
            .iter()
            .map(|(keypair, _)| Ping::new(rng.random(), keypair))
            .collect();
        let pongs: Vec<_> = pings
            .iter()
            .map(|ping| Pong::new(ping, &this_node))
            .collect();
        let recycler = PacketBatchRecycler::default();
        let packets = {
            let (sender, receiver) = crossbeam_channel::bounded(1024);
            cluster_info.handle_batch_ping_messages(
                remote_nodes.iter().map(|(_, socket)| socket).zip(pings),
                &recycler,
                &sender,
            );
            receiver.recv().unwrap()
        };
        assert_eq!(remote_nodes.len(), packets.len());
        for (packet, (_, socket), pong) in izip!(
            packets.into_iter(),
            remote_nodes.into_iter(),
            pongs.into_iter()
        ) {
            assert_eq!(packet.meta().socket_addr(), socket);
            let bytes = wincode::serialize(&pong).unwrap();
            match deserialize_protocol(packet.data(..).unwrap_or_default()).unwrap() {
                Protocol::PongMessage(pong) => {
                    let pong_bytes = wincode::serialize(&pong).unwrap();
                    assert_eq!(pong_bytes, bytes);
                }
                _ => panic!("invalid packet!"),
            }
        }
    }

    fn test_crds_values(pubkey: Pubkey) -> Vec<CrdsValue> {
        let entrypoint = ContactInfo::new_localhost(&pubkey, timestamp());
        let entrypoint_crdsvalue = CrdsValue::new_unsigned(CrdsData::from(entrypoint));
        vec![entrypoint_crdsvalue]
    }

    #[test]
    fn test_cluster_spy_gossip() {
        let thread_pool = ThreadPoolBuilder::new().build().unwrap();
        //check that gossip doesn't try to push to invalid addresses
        let (spy, _, _) = ClusterInfo::spy_node(solana_pubkey::new_rand(), 0);
        let cluster_info = Arc::new({
            let keypair = Arc::new(Keypair::new());
            let node = Node::new_localhost_with_pubkey(&keypair.pubkey());
            ClusterInfo::new(node.info, keypair, SocketAddrSpace::Unspecified)
        });
        cluster_info.insert_info(spy);
        cluster_info.gossip.refresh_push_active_set(
            &cluster_info.keypair(),
            cluster_info.my_shred_version(),
            &HashMap::new(), // stakes
            None,            // gossip validators
            &cluster_info.ping_cache,
            &mut Vec::new(), // pings
            &SocketAddrSpace::Unspecified,
        );
        let mut gossip_requests = cluster_info.build_gossip_requests(
            &thread_pool,
            None,            // gossip_validators
            &HashMap::new(), // stakes
            true,            // generate_pull_requests
        );
        //assert none of the addrs are invalid.
        assert!(gossip_requests.all(|(addr, _)| {
            ContactInfo::is_valid_address(&addr, &SocketAddrSpace::Unspecified)
        }));
    }

    #[test]
    fn test_cluster_info_new() {
        let keypair = Arc::new(Keypair::new());
        let d = ContactInfo::new_localhost(&keypair.pubkey(), timestamp());
        let cluster_info = ClusterInfo::new(d.clone(), keypair, SocketAddrSpace::Unspecified);
        assert_eq!(d.pubkey(), &cluster_info.id());
    }

    #[test]
    fn insert_info_test() {
        let keypair = Arc::new(Keypair::new());
        let d = ContactInfo::new_localhost(&keypair.pubkey(), timestamp());
        let cluster_info = ClusterInfo::new(d, keypair, SocketAddrSpace::Unspecified);
        let d = ContactInfo::new_localhost(&solana_pubkey::new_rand(), timestamp());
        let label = CrdsValueLabel::ContactInfo(*d.pubkey());
        cluster_info.insert_info(d);
        let gossip_crds = cluster_info.gossip.crds.read().unwrap();
        assert!(gossip_crds.get::<&CrdsValue>(&label).is_some());
    }

    #[test]
    fn test_save_restore_contact_info_round_trip() {
        let tmpdir = tempfile::tempdir().unwrap();

        let self_keypair = Arc::new(Keypair::new());
        let self_ci = ContactInfo::new_localhost(&self_keypair.pubkey(), timestamp());
        let mut cluster_info_a =
            ClusterInfo::new(self_ci, self_keypair, SocketAddrSpace::Unspecified);
        // Sets contact_info_path; no file to load yet.
        cluster_info_a.restore_contact_info(tmpdir.path(), 0);

        let peer1 = ContactInfo::new_localhost(&solana_pubkey::new_rand(), timestamp());
        let peer2 = ContactInfo::new_localhost(&solana_pubkey::new_rand(), timestamp());
        let peer1_pubkey = *peer1.pubkey();
        let peer2_pubkey = *peer2.pubkey();
        cluster_info_a.insert_info(peer1);
        cluster_info_a.insert_info(peer2);
        cluster_info_a.save_contact_info();

        let other_keypair = Arc::new(Keypair::new());
        let other_ci = ContactInfo::new_localhost(&other_keypair.pubkey(), timestamp());
        let mut cluster_info_b =
            ClusterInfo::new(other_ci, other_keypair, SocketAddrSpace::Unspecified);
        cluster_info_b.restore_contact_info(tmpdir.path(), 0);

        let gossip_crds = cluster_info_b.gossip.crds.read().unwrap();
        assert!(
            gossip_crds
                .get::<&CrdsValue>(&CrdsValueLabel::ContactInfo(peer1_pubkey))
                .is_some(),
            "peer1 missing after restore"
        );
        assert!(
            gossip_crds
                .get::<&CrdsValue>(&CrdsValueLabel::ContactInfo(peer2_pubkey))
                .is_some(),
            "peer2 missing after restore"
        );
    }

    fn assert_in_range(x: u16, range: (u16, u16)) {
        assert!(x >= range.0);
        assert!(x < range.1);
    }

    fn check_sockets<T>(sockets: &[T], ip: IpAddr, range: (u16, u16))
    where
        T: Borrow<UdpSocket>,
    {
        assert!(!sockets.is_empty());
        let port = sockets[0].borrow().local_addr().unwrap().port();
        for s in sockets {
            let s = s.borrow();
            let local_addr = s.local_addr().unwrap();
            assert_eq!(local_addr.ip(), ip);
            assert_in_range(local_addr.port(), range);
            assert_eq!(local_addr.port(), port);
        }
    }

    fn check_socket<T>(socket: &T, ip: IpAddr, range: (u16, u16))
    where
        T: Borrow<UdpSocket>,
    {
        check_sockets(std::slice::from_ref(socket), ip, range);
    }

    fn check_node_sockets(node: &Node, ip: IpAddr, range: (u16, u16)) {
        check_socket(&node.sockets.repair, ip, range);
        check_sockets(&node.sockets.votor_server, ip, range);
        check_sockets(&node.sockets.gossip, ip, range);
        check_sockets(&node.sockets.tvu, ip, range);
        check_sockets(&node.sockets.tpu_quic, ip, range);
    }

    #[test]
    fn new_with_external_ip_test_random() {
        let ip = Ipv4Addr::LOCALHOST;
        let port_range = localhost_port_range_for_tests();
        let config = NodeConfig {
            advertised_ip: IpAddr::V4(ip),
            gossip_port: 0,
            port_range,
            bind_ip_addrs: BindIpAddrs::new(vec![IpAddr::V4(ip)]).unwrap(),
            public_tpu_addr: None,
            public_tpu_forwards_addr: None,
            public_tvu_addr: None,
            num_tvu_receive_sockets: MINIMUM_NUM_TVU_RECEIVE_SOCKETS,
            num_tvu_retransmit_sockets: MINIMUM_NUM_TVU_RECEIVE_SOCKETS,
            num_quic_endpoints: DEFAULT_NUM_QUIC_ENDPOINTS,
            num_votor_quic_endpoints: DEFAULT_NUM_VOTOR_QUIC_ENDPOINTS,
        };

        let node = Node::new_with_external_ip(&solana_pubkey::new_rand(), config);

        check_node_sockets(&node, IpAddr::V4(ip), port_range);
    }

    #[test]
    fn new_with_external_ip_test_gossip() {
        // Can't use VALIDATOR_PORT_RANGE because if this test runs in parallel with others, the
        // port returned by `bind_in_range()` might be snatched up before `Node::new_with_external_ip()` runs
        let port_range = localhost_port_range_for_tests();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let port = port_range.0;
        let config = NodeConfig {
            advertised_ip: ip,
            gossip_port: port,
            port_range,
            bind_ip_addrs: BindIpAddrs::new(vec![ip]).unwrap(),
            public_tpu_addr: None,
            public_tpu_forwards_addr: None,
            public_tvu_addr: None,
            num_tvu_receive_sockets: MINIMUM_NUM_TVU_RECEIVE_SOCKETS,
            num_tvu_retransmit_sockets: MINIMUM_NUM_TVU_RECEIVE_SOCKETS,
            num_quic_endpoints: DEFAULT_NUM_QUIC_ENDPOINTS,
            num_votor_quic_endpoints: DEFAULT_NUM_VOTOR_QUIC_ENDPOINTS,
        };

        let node = Node::new_with_external_ip(&solana_pubkey::new_rand(), config);

        check_node_sockets(&node, ip, port_range);
        check_sockets(&node.sockets.gossip, ip, port_range);
    }

    //test that all cluster_info objects only generate signed messages
    //when constructed with keypairs
    #[test]
    fn test_gossip_signature_verification() {
        let thread_pool = ThreadPoolBuilder::new().build().unwrap();
        //create new cluster info, leader, and peer
        let keypair = Keypair::new();
        let peer_keypair = Keypair::new();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let peer = ContactInfo::new_localhost(&peer_keypair.pubkey(), 0);
        let cluster_info = ClusterInfo::new(
            contact_info,
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        );
        let stakes = HashMap::<Pubkey, u64>::default();
        cluster_info.ping_cache.lock().unwrap().mock_pong(
            *peer.pubkey(),
            peer.gossip().unwrap(),
            Instant::now(),
        );
        cluster_info.insert_info(peer);
        cluster_info.gossip.refresh_push_active_set(
            &cluster_info.keypair(),
            cluster_info.my_shred_version(),
            &stakes,
            None, // gossip validators
            &cluster_info.ping_cache,
            &mut Vec::new(), // pings
            &SocketAddrSpace::Unspecified,
        );
        //check that all types of gossip messages are signed correctly
        let (entries, push_messages, _) = cluster_info.gossip.new_push_messages(
            &cluster_info.id(),
            timestamp(),
            &stakes,
            |_| true, // should_retain_crds_value
        );
        let push_messages = push_messages
            .into_iter()
            .map(|(pubkey, indices)| {
                let values = indices.into_iter().map(|k| entries[k].clone()).collect();
                (pubkey, values)
            })
            .collect::<HashMap<_, Vec<_>>>();
        // there should be some pushes ready
        assert!(!push_messages.is_empty());
        let sigverify_cache = SigVerifyCache::new();
        push_messages.values().for_each(|v| {
            v.par_iter()
                .for_each(|v| assert!(v.verify_with_cache(&sigverify_cache)))
        });

        let mut pings = Vec::new();
        let _ = cluster_info
            .gossip
            .new_pull_request(
                &thread_pool,
                cluster_info.keypair().deref(),
                cluster_info.my_shred_version(),
                timestamp(),
                None,
                &HashMap::new(),
                992, // max_bloom_filter_bytes
                &cluster_info.ping_cache,
                &mut pings,
                &cluster_info.socket_addr_space,
            )
            .ok()
            .unwrap();
    }

    #[test]
    fn test_refresh_vote_eviction() {
        let keypair = Arc::new(Keypair::new());
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = ClusterInfo::new(contact_info, keypair, SocketAddrSpace::Unspecified);

        // Push MAX_LOCKOUT_HISTORY votes into gossip, one for each slot between
        // [lowest_vote_slot, lowest_vote_slot + MAX_LOCKOUT_HISTORY)
        let lowest_vote_slot = 1;
        let max_vote_slot = lowest_vote_slot + MAX_LOCKOUT_HISTORY as Slot;
        let mut first_vote = None;
        let mut prev_votes = vec![];
        for slot in lowest_vote_slot..max_vote_slot {
            prev_votes.push(slot);
            let unrefresh_vote = Vote::new(vec![slot], Hash::new_unique());
            let vote_ix = vote_instruction::vote(
                &Pubkey::new_unique(), // vote_pubkey
                &Pubkey::new_unique(), // authorized_voter_pubkey
                unrefresh_vote,
            );
            let vote_tx = Transaction::new_with_payer(
                &[vote_ix], // instructions
                None,       // payer
            );
            if first_vote.is_none() {
                first_vote = Some(vote_tx.clone());
            }
            cluster_info.push_vote(&prev_votes, vote_tx);
            // Sleep to avoid votes with same timestamp causing the insert to
            // fail when wallclocks collide (find_vote_index_to_evict breaks
            // ties by ms timestamp during eviction).
            // Since we only store MAX_VOTES, when conflict occurs we may end up evicting
            // the newer votes if all timestamps are the same.
            std::thread::sleep(Duration::from_millis(2));
        }

        // We should now have the MAX_VOTES most recent votes in CRDS, for slots
        // [max_vote_slot - MAX_VOTES, max_vote_slot)
        let initial_votes = cluster_info.get_votes(&mut Cursor::default());
        assert_eq!(initial_votes.len(), MAX_VOTES as usize);

        // Trying to refresh a vote less than all votes in gossip should do nothing
        let refresh_slot = lowest_vote_slot - 1;
        let refresh_vote = Vote::new(vec![refresh_slot], Hash::new_unique());
        let refresh_ix = vote_instruction::vote(
            &Pubkey::new_unique(), // vote_pubkey
            &Pubkey::new_unique(), // authorized_voter_pubkey
            refresh_vote,
        );
        let refresh_tx = Transaction::new_with_payer(
            &[refresh_ix], // instructions
            None,          // payer
        );
        cluster_info.refresh_vote(refresh_tx.clone(), refresh_slot);
        let current_votes = cluster_info.get_votes(&mut Cursor::default());
        assert_eq!(initial_votes, current_votes);
        assert!(
            !current_votes.contains(&refresh_tx),
            "Refresh on outdated vote should fail"
        );

        // Trying to refresh a vote should evict the first slot less than the refreshed vote slot
        let refresh_slot = max_vote_slot + 1;
        let refresh_vote = Vote::new(vec![refresh_slot], Hash::new_unique());
        let refresh_ix = vote_instruction::vote(
            &Pubkey::new_unique(), // vote_pubkey
            &Pubkey::new_unique(), // authorized_voter_pubkey
            refresh_vote,
        );
        let refresh_tx = Transaction::new_with_payer(
            &[refresh_ix], // instructions
            None,          // payer
        );
        cluster_info.refresh_vote(refresh_tx.clone(), refresh_slot);

        // This should evict the latest vote since it's for a slot less than refresh_slot
        let votes = cluster_info.get_votes(&mut Cursor::default());
        assert_eq!(votes.len(), MAX_VOTES as usize);
        assert!(votes.contains(&refresh_tx), "Refresh vote not found");
        assert!(!votes.contains(&first_vote.unwrap()));
    }

    #[test]
    fn test_refresh_vote() {
        let keypair = Arc::new(Keypair::new());
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = ClusterInfo::new(contact_info, keypair, SocketAddrSpace::Unspecified);

        // Construct and push a vote for some other slot
        let unrefresh_slot = 5;
        let unrefresh_tower = vec![1, 3, unrefresh_slot];
        let unrefresh_vote = Vote::new(unrefresh_tower.clone(), Hash::new_unique());
        let unrefresh_ix = vote_instruction::vote(
            &Pubkey::new_unique(), // vote_pubkey
            &Pubkey::new_unique(), // authorized_voter_pubkey
            unrefresh_vote,
        );
        let unrefresh_tx = Transaction::new_with_payer(
            &[unrefresh_ix], // instructions
            None,            // payer
        );
        cluster_info.push_vote(&unrefresh_tower, unrefresh_tx.clone());
        let mut cursor = Cursor::default();
        let votes = cluster_info.get_votes(&mut cursor);
        assert_eq!(votes, vec![unrefresh_tx.clone()]);

        // Now construct vote for the slot to be refreshed later.
        let refresh_slot = unrefresh_slot + 1;
        let refresh_tower = vec![1, 3, unrefresh_slot, refresh_slot];
        let refresh_vote = Vote::new(refresh_tower, Hash::new_unique());
        let refresh_ix = vote_instruction::vote(
            &Pubkey::new_unique(), // vote_pubkey
            &Pubkey::new_unique(), // authorized_voter_pubkey
            refresh_vote.clone(),
        );
        let refresh_tx = Transaction::new_with_payer(
            &[refresh_ix], // instructions
            None,          // payer
        );

        // Trying to refresh vote when it doesn't yet exist in gossip
        // should add the vote without eviction if there is room in the gossip table.
        cluster_info.refresh_vote(refresh_tx.clone(), refresh_slot);

        // Should be two votes in gossip
        cursor = Cursor::default();
        let votes = cluster_info.get_votes(&mut cursor);
        assert_eq!(votes.len(), 2);
        assert!(votes.contains(&unrefresh_tx));
        assert!(votes.contains(&refresh_tx));

        // Refresh a few times, we should only have the latest update
        let mut latest_refresh_tx = refresh_tx;
        for _ in 0..10 {
            let latest_refreshed_recent_blockhash = Hash::new_unique();
            let new_signer = Keypair::new();
            let refresh_ix = vote_instruction::vote(
                &new_signer.pubkey(), // vote_pubkey
                &new_signer.pubkey(), // authorized_voter_pubkey
                refresh_vote.clone(),
            );
            latest_refresh_tx = Transaction::new_signed_with_payer(
                &[refresh_ix],
                None,
                &[&new_signer],
                latest_refreshed_recent_blockhash,
            );
            cluster_info.refresh_vote(latest_refresh_tx.clone(), refresh_slot);
            // Sleep to avoid votes with same timestamp causing later vote to not override prior vote
            std::thread::sleep(Duration::from_millis(1));
        }
        // The diff since `max_ts` should only be the latest refreshed vote
        let votes = cluster_info.get_votes(&mut cursor);
        assert_eq!(votes.len(), 1);
        assert_eq!(votes[0], latest_refresh_tx);

        // Should still be two votes in gossip
        let votes = cluster_info.get_votes(&mut Cursor::default());
        assert_eq!(votes.len(), 2);
        assert!(votes.contains(&unrefresh_tx));
        assert!(votes.contains(&latest_refresh_tx));
    }

    #[test]
    fn test_push_vote() {
        let keypair = Arc::new(Keypair::new());
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info =
            ClusterInfo::new(contact_info, keypair.clone(), SocketAddrSpace::Unspecified);

        // make sure empty crds is handled correctly
        let mut cursor = Cursor::default();
        let votes = cluster_info.get_votes(&mut cursor);
        assert_eq!(votes, vec![]);

        // add a vote
        let vote = Vote::new(
            vec![1, 3, 7], // slots
            Hash::new_unique(),
        );
        let ix = vote_instruction::vote(
            &Pubkey::new_unique(), // vote_pubkey
            &Pubkey::new_unique(), // authorized_voter_pubkey
            vote,
        );
        let tx = Transaction::new_with_payer(
            &[ix], // instructions
            None,  // payer
        );
        let tower = vec![7]; // Last slot in the vote.
        cluster_info.push_vote(&tower, tx.clone());

        let (labels, votes) = cluster_info.get_votes_with_labels(&mut cursor);
        assert_eq!(votes, vec![tx]);
        assert_eq!(labels.len(), 1);
        match labels[0] {
            CrdsValueLabel::Vote(_, pubkey) => {
                assert_eq!(pubkey, keypair.pubkey());
            }

            _ => panic!("Bad match"),
        }
        // make sure timestamp filter works
        let votes = cluster_info.get_votes(&mut cursor);
        assert_eq!(votes, vec![]);
    }

    fn new_vote_transaction(slots: Vec<Slot>) -> Transaction {
        let vote = Vote::new(slots, Hash::new_unique());
        let ix = vote_instruction::vote(
            &Pubkey::new_unique(), // vote_pubkey
            &Pubkey::new_unique(), // authorized_voter_pubkey
            vote,
        );
        Transaction::new_with_payer(
            &[ix], // instructions
            None,  // payer
        )
    }

    #[test]
    fn test_push_votes_with_tower() {
        let get_vote_slots = |cluster_info: &ClusterInfo| -> Vec<Slot> {
            let (labels, _) = cluster_info.get_votes_with_labels(&mut Cursor::default());
            let gossip_crds = cluster_info.gossip.crds.read().unwrap();
            let mut vote_slots = HashSet::new();
            for label in labels {
                let CrdsData::Vote(_, vote) = &gossip_crds.get::<&CrdsData>(&label).unwrap() else {
                    panic!("this should not happen!");
                };
                assert!(vote_slots.insert(vote.slot().unwrap()));
            }
            vote_slots.into_iter().sorted().collect()
        };
        let keypair = Arc::new(Keypair::new());
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = ClusterInfo::new(contact_info, keypair, SocketAddrSpace::Unspecified);
        let mut tower = Vec::new();

        // Evict the oldest vote
        for k in 0..2 * MAX_LOCKOUT_HISTORY {
            let slot = k as Slot;
            tower.push(slot);
            let vote = new_vote_transaction(vec![slot]);
            cluster_info.push_vote(&tower, vote);

            let vote_slots = get_vote_slots(&cluster_info);
            let min_vote = k.saturating_sub(MAX_VOTES as usize - 1) as u64;
            assert!(vote_slots.into_iter().eq(min_vote..=(k as u64)));
            sleep(Duration::from_millis(1));
        }

        // Attempting to push an older vote will panic
        let slot = 30;
        tower.clear();
        tower.extend(0..=slot);
        let vote = new_vote_transaction(vec![slot]);
        // `KeyedRateLimiter` is not unwind-safe; this test only checks the panic.
        assert!(
            panic::catch_unwind(panic::AssertUnwindSafe(|| {
                cluster_info.push_vote(&tower, vote)
            }))
            .err()
            .and_then(|a| a
                .downcast_ref::<String>()
                .map(|s| { s.starts_with("Submitting old vote") }))
            .unwrap_or_default()
        );
    }

    #[test]
    fn test_push_epoch_slots() {
        let keypair = Arc::new(Keypair::new());
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = ClusterInfo::new(contact_info, keypair, SocketAddrSpace::Unspecified);
        let slots = cluster_info.get_epoch_slots(&mut Cursor::default());
        assert!(slots.is_empty());
        cluster_info.push_epoch_slots(&[0]);

        let mut cursor = Cursor::default();
        let slots = cluster_info.get_epoch_slots(&mut cursor);
        assert_eq!(slots.len(), 1);

        let slots = cluster_info.get_epoch_slots(&mut cursor);
        assert!(slots.is_empty());
    }

    #[test]
    fn test_push_epoch_slots_wraps() {
        let keypair = Arc::new(Keypair::new());
        let pubkey = keypair.pubkey();
        let contact_info = ContactInfo::new_localhost(&pubkey, 0);
        let cluster_info =
            ClusterInfo::new(contact_info, keypair.clone(), SocketAddrSpace::Unspecified);
        let mut rng = StdRng::seed_from_u64(0);
        let slots: Vec<Slot> = repeat_with(|| rng.random_range(1..32))
            .scan(0, |slot, step| {
                *slot += step;
                Some(*slot)
            })
            .take(32_000)
            .collect();
        let mut epoch_slots = EpochSlots::new(pubkey, timestamp());
        let num_slots = epoch_slots.fill(&slots, timestamp());
        assert!(num_slots < slots.len());
        let next_slot = slots[num_slots];
        assert_eq!(epoch_slots.clone().fill(&[next_slot], timestamp()), 0);

        let value = CrdsValue::new(
            CrdsData::EpochSlots(crds_data::MAX_EPOCH_SLOTS - 1, epoch_slots),
            keypair.as_ref(),
        );
        cluster_info
            .gossip
            .crds
            .write()
            .unwrap()
            .insert(value, timestamp(), GossipRoute::LocalMessage)
            .unwrap();
        cluster_info.push_epoch_slots(&[next_slot]);

        let crds = cluster_info.gossip.crds.read().unwrap();
        let label = CrdsValueLabel::EpochSlots(0, pubkey);
        let value = crds.get::<&CrdsValue>(&label).unwrap();
        assert!(value.sanitize().is_ok());
        assert_eq!(
            value.epoch_slots().unwrap().to_slots(next_slot).next(),
            Some(next_slot)
        );
        let invalid_label = CrdsValueLabel::EpochSlots(crds_data::MAX_EPOCH_SLOTS, pubkey);
        assert!(crds.get::<&CrdsValue>(&invalid_label).is_none());
    }

    #[test]
    fn test_append_entrypoint_to_pulls() {
        let thread_pool = ThreadPoolBuilder::new().build().unwrap();
        let node_keypair = Arc::new(Keypair::new());
        let cluster_info = ClusterInfo::new(
            ContactInfo::new_localhost(&node_keypair.pubkey(), timestamp()),
            node_keypair,
            SocketAddrSpace::Unspecified,
        );
        let entrypoint_pubkey = solana_pubkey::new_rand();
        let entrypoint = ContactInfo::new_localhost(&entrypoint_pubkey, timestamp());
        cluster_info.set_entrypoint(entrypoint.clone());
        let (pings, pulls) = cluster_info.old_pull_requests(&thread_pool, None, &HashMap::new());
        assert!(pings.is_empty());
        assert_eq!(pulls.len(), MIN_NUM_BLOOM_FILTERS);
        for (addr, msg) in pulls {
            assert_eq!(addr, entrypoint.gossip().unwrap());
            match msg {
                Protocol::PullRequest(_, value) => {
                    assert!(value.verify_with_cache(&SigVerifyCache::new()));
                    assert_eq!(value.pubkey(), cluster_info.id())
                }
                _ => panic!("wrong protocol"),
            }
        }
        // now add this message back to the table and make sure after the next pull, the entrypoint is unset
        let entrypoint_crdsvalue = CrdsValue::new_unsigned(CrdsData::from(&entrypoint));
        let cluster_info = Arc::new(cluster_info);
        let stakes = HashMap::from([(Pubkey::new_unique(), 1u64)]);
        let timeouts = cluster_info.gossip.make_timeouts(
            cluster_info.id(),
            &stakes,
            Duration::from_millis(cluster_info.gossip.pull.crds_timeout),
        );
        cluster_info.handle_pull_response(vec![entrypoint_crdsvalue], &timeouts);
        let (pings, pulls) = cluster_info.old_pull_requests(&thread_pool, None, &HashMap::new());
        assert_eq!(pings.len(), 1);
        assert_eq!(pulls.len(), MIN_NUM_BLOOM_FILTERS);
        assert_eq!(*cluster_info.entrypoints.read().unwrap(), vec![entrypoint]);
    }

    #[test]
    fn test_tvu_peers_and_stakes() {
        let keypair = Arc::new(Keypair::new());
        let d = ContactInfo::new_localhost(&keypair.pubkey(), timestamp());
        let cluster_info = ClusterInfo::new(d.clone(), keypair, SocketAddrSpace::Unspecified);
        let mut stakes = HashMap::new();

        // no stake
        let id = Pubkey::from([1u8; 32]);
        let contact_info = ContactInfo::new_localhost(&id, timestamp());
        cluster_info.insert_info(contact_info);

        // normal
        let id2 = Pubkey::from([2u8; 32]);
        let mut contact_info = ContactInfo::new_localhost(&id2, timestamp());
        cluster_info.insert_info(contact_info.clone());
        stakes.insert(id2, 10);

        // duplicate
        contact_info.set_wallclock(timestamp() + 1);
        cluster_info.insert_info(contact_info);

        // no tvu
        let id3 = Pubkey::from([3u8; 32]);
        let mut contact_info = ContactInfo::new_localhost(&id3, timestamp());
        contact_info.remove_tvu();
        cluster_info.insert_info(contact_info);
        stakes.insert(id3, 10);

        // normal but with different shred version
        let id4 = Pubkey::from([4u8; 32]);
        let mut contact_info = ContactInfo::new_localhost(&id4, timestamp());
        contact_info.set_shred_version(1);
        assert_ne!(contact_info.shred_version(), d.shred_version());
        cluster_info.insert_info(contact_info);
        stakes.insert(id4, 10);
    }

    #[test]
    fn test_pull_from_entrypoint_if_not_present() {
        let thread_pool = ThreadPoolBuilder::new().build().unwrap();
        let node_keypair = Arc::new(Keypair::new());
        let cluster_info = ClusterInfo::new(
            ContactInfo::new_localhost(&node_keypair.pubkey(), timestamp()),
            node_keypair,
            SocketAddrSpace::Unspecified,
        );
        let entrypoint_pubkey = solana_pubkey::new_rand();
        let mut entrypoint = ContactInfo::new_localhost(&entrypoint_pubkey, timestamp());
        entrypoint
            .set_gossip(socketaddr!("127.0.0.2:1234"))
            .unwrap();
        cluster_info.set_entrypoint(entrypoint.clone());

        let mut stakes = HashMap::new();

        let other_node_pubkey = solana_pubkey::new_rand();
        let other_node = ContactInfo::new_localhost(&other_node_pubkey, timestamp());
        assert_ne!(other_node.gossip().unwrap(), entrypoint.gossip().unwrap());
        cluster_info.ping_cache.lock().unwrap().mock_pong(
            *other_node.pubkey(),
            other_node.gossip().unwrap(),
            Instant::now(),
        );
        cluster_info.insert_info(other_node.clone());
        stakes.insert(other_node_pubkey, 10);

        // Pull request 1:  `other_node` is present but `entrypoint` was just added (so it has a
        // fresh timestamp).  There should only be one pull request to `other_node`
        let (pings, pulls) = cluster_info.old_pull_requests(&thread_pool, None, &stakes);
        assert!(pings.is_empty());
        assert_eq!(pulls.len(), MIN_NUM_BLOOM_FILTERS);
        assert!(
            pulls
                .into_iter()
                .all(|(addr, _)| addr == other_node.gossip().unwrap())
        );

        // Pull request 2: pretend it's been a while since we've pulled from `entrypoint`.  There should
        // now be two pull requests
        cluster_info.entrypoints.write().unwrap()[0].set_wallclock(0);
        let (pings, pulls) = cluster_info.old_pull_requests(&thread_pool, None, &stakes);
        assert!(pings.is_empty());
        assert_eq!(pulls.len(), 2 * MIN_NUM_BLOOM_FILTERS);
        for node in [&other_node, &entrypoint] {
            assert_eq!(
                pulls
                    .iter()
                    .filter(|(addr, _)| *addr == node.gossip().unwrap())
                    .count(),
                MIN_NUM_BLOOM_FILTERS
            );
        }
        // Pull request 3:  `other_node` is present and `entrypoint` was just pulled from.  There should
        // only be one pull request to `other_node`
        let (pings, pulls) = cluster_info.old_pull_requests(&thread_pool, None, &stakes);
        assert!(pings.is_empty());
        assert_eq!(pulls.len(), MIN_NUM_BLOOM_FILTERS);
        assert!(
            pulls
                .into_iter()
                .all(|(addr, _)| addr == other_node.gossip().unwrap())
        );
    }

    #[test]
    fn test_repair_peers() {
        let node_keypair = Arc::new(Keypair::new());
        let cluster_info = ClusterInfo::new(
            ContactInfo::new_localhost(&node_keypair.pubkey(), timestamp()),
            node_keypair,
            SocketAddrSpace::Unspecified,
        );
        for i in 0..10 {
            // make these invalid for the upcoming repair request
            let peer_lowest = if i >= 5 { 10 } else { 0 };
            let other_node_pubkey = solana_pubkey::new_rand();
            let other_node = ContactInfo::new_localhost(&other_node_pubkey, timestamp());
            cluster_info.insert_info(other_node.clone());
            let value = CrdsValue::new_unsigned(CrdsData::LowestSlot(
                0,
                LowestSlot::new(other_node_pubkey, peer_lowest, timestamp()),
            ));
            let mut gossip_crds = cluster_info.gossip.crds.write().unwrap();
            let _ = gossip_crds.insert(value, timestamp(), GossipRoute::LocalMessage);
        }
        // only half the visible peers should be eligible to serve this repair
        assert_eq!(cluster_info.repair_peers(5).len(), 5);
    }

    #[test]
    fn test_push_epoch_slots_large() {
        let node_keypair = Arc::new(Keypair::new());
        let cluster_info = ClusterInfo::new(
            ContactInfo::new_localhost(&node_keypair.pubkey(), timestamp()),
            node_keypair,
            SocketAddrSpace::Unspecified,
        );
        //random should be hard to compress
        let mut rng = rand::rng();
        let range: Vec<Slot> = repeat_with(|| rng.random_range(1..32))
            .scan(0, |slot, step| {
                *slot += step;
                Some(*slot)
            })
            .take(32000)
            .collect();
        cluster_info.push_epoch_slots(&range[..16000]);
        cluster_info.push_epoch_slots(&range[16000..]);
        let slots = cluster_info.get_epoch_slots(&mut Cursor::default());
        let slots: Vec<_> = slots.iter().flat_map(|x| x.to_slots(0)).collect();
        assert_eq!(slots, range);
    }

    #[test]
    fn test_process_entrypoint_without_adopt_shred_version() {
        let node_keypair = Arc::new(Keypair::new());
        let cluster_info = Arc::new(ClusterInfo::new(
            {
                let mut contact_info =
                    ContactInfo::new_localhost(&node_keypair.pubkey(), timestamp());
                contact_info.set_shred_version(2);
                contact_info
            },
            node_keypair,
            SocketAddrSpace::Unspecified,
        ));
        assert_eq!(cluster_info.my_shred_version(), 2);

        // Simulating starting up with default entrypoint, no known id, only a gossip
        // address
        let entrypoint_gossip_addr = socketaddr!("127.0.0.2:1234");
        let mut entrypoint = ContactInfo::new_localhost(&Pubkey::default(), timestamp());
        entrypoint.set_gossip(entrypoint_gossip_addr).unwrap();
        assert_eq!(entrypoint.shred_version(), 0);
        cluster_info.set_entrypoint(entrypoint);

        // Simulate getting entrypoint ContactInfo from gossip
        let mut gossiped_entrypoint_info =
            ContactInfo::new_localhost(&solana_pubkey::new_rand(), timestamp());
        gossiped_entrypoint_info
            .set_gossip(entrypoint_gossip_addr)
            .unwrap();
        gossiped_entrypoint_info.set_shred_version(1);
        cluster_info.insert_info(gossiped_entrypoint_info.clone());

        // Adopt the entrypoint's gossiped contact info and verify
        let entrypoints_processed = ClusterInfo::process_entrypoints(&cluster_info);
        assert_eq!(cluster_info.entrypoints.read().unwrap().len(), 1);
        assert_eq!(
            cluster_info.entrypoints.read().unwrap()[0],
            gossiped_entrypoint_info,
        );
        assert!(entrypoints_processed);
        assert_eq!(cluster_info.my_shred_version(), 2); // <--- No change to shred version
    }

    #[test]
    fn test_get_duplicate_shreds() {
        let host1_key = Arc::new(Keypair::new());
        let node = Node::new_localhost_with_pubkey(&host1_key.pubkey());
        let cluster_info = Arc::new(ClusterInfo::new(
            node.info,
            host1_key.clone(),
            SocketAddrSpace::Unspecified,
        ));
        let mut cursor = Cursor::default();
        assert!(cluster_info.get_duplicate_shreds(&mut cursor).is_empty());

        let mut rng = rand::rng();
        let (slot, parent_slot, reference_tick, version) = (53084024, 53084023, 0, 0);
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).unwrap();
        let next_shred_index = 353;
        let leader = Arc::new(Keypair::new());
        let shred1 = new_rand_shred(&mut rng, next_shred_index, &shredder, &leader);
        let shred2 = new_rand_shred(&mut rng, next_shred_index, &shredder, &leader);
        assert!(
            cluster_info
                .push_duplicate_shred(&shred1, shred2.payload())
                .is_ok()
        );
        let entries = cluster_info.get_duplicate_shreds(&mut cursor);
        // One duplicate shred proof is split into 3 chunks.
        assert_eq!(3, entries.len());
        for (i, shred_data) in entries.iter().enumerate() {
            assert_eq!(shred_data.from, host1_key.pubkey());
            assert_eq!(shred_data.slot, 53084024);
            assert_eq!(shred_data.chunk_index() as usize, i);
        }

        let slot = 53084025;
        let shredder = Shredder::new(slot, parent_slot, reference_tick, version).unwrap();
        let next_shred_index = 354;
        let shred3 = new_rand_shred(&mut rng, next_shred_index, &shredder, &leader);
        let shred4 = new_rand_shred(&mut rng, next_shred_index, &shredder, &leader);
        assert!(
            cluster_info
                .push_duplicate_shred(&shred3, shred4.payload())
                .is_ok()
        );
        let entries1 = cluster_info.get_duplicate_shreds(&mut cursor);
        // One duplicate shred proof is split into 3 chunks.
        assert_eq!(3, entries1.len());
        for (i, shred_data) in entries1.iter().enumerate() {
            assert_eq!(shred_data.from, host1_key.pubkey());
            assert_eq!(shred_data.slot, 53084025);
            assert_eq!(shred_data.chunk_index() as usize, i);
        }
    }

    #[test]
    fn test_contact_trace() {
        agave_logger::setup();
        let keypair44 = Arc::new(
            Keypair::try_from(
                [
                    66, 88, 3, 70, 228, 215, 125, 64, 130, 183, 180, 98, 22, 166, 201, 234, 89, 80,
                    135, 24, 228, 35, 20, 52, 105, 130, 50, 51, 46, 229, 244, 108, 70, 57, 45, 247,
                    57, 177, 39, 126, 190, 238, 50, 96, 186, 208, 28, 168, 148, 56, 9, 106, 92,
                    213, 63, 205, 252, 225, 244, 101, 77, 182, 4, 2,
                ]
                .as_ref(),
            )
            .unwrap(),
        );

        let cluster_info44 = Arc::new({
            let node = Node::new_localhost_with_pubkey(&keypair44.pubkey());
            ClusterInfo::new(node.info, keypair44.clone(), SocketAddrSpace::Unspecified)
        });

        fn check_table_layout(trace: String) {
            let line_lengths = trace.lines().map(|l| l.len()).collect::<Vec<_>>();
            for &ll in line_lengths.iter().take(3) {
                assert_eq!(
                    ll, line_lengths[0],
                    "Line length mismatch, check table layout!"
                );
            }
        }

        let trace = cluster_info44.contact_info_trace();
        info!("cluster:\n{trace}");
        check_table_layout(trace);

        let trace = cluster_info44.rpc_info_trace();
        info!("rpc:\n{trace}");
        check_table_layout(trace);
    }

    #[test]
    fn test_discard_different_shred_version_push_message() {
        let self_shred_version = 5555;
        let mut crds = Crds::default();
        let stats = GossipStats::default();
        let mut rng = rand::rng();
        let keypair = Keypair::new();

        // create contact info with matching shred version
        let contact_info = ContactInfo::new(
            keypair.pubkey(),
            /*wallclock:*/ 1234567890,
            self_shred_version,
        );
        let ci = CrdsValue::new(CrdsData::ContactInfo(contact_info), &keypair);

        // Test push message with matching shred version
        let mut msg = test_message(Protocol::PushMessage(keypair.pubkey(), vec![ci.clone()]));
        discard_different_shred_version(&mut msg, self_shred_version, &crds, &stats);
        if let Protocol::PushMessage(_, values) = msg.protocol() {
            assert_eq!(values.len(), 1);
        }

        let contact_info_wrong_shred_version =
            ContactInfo::new(keypair.pubkey(), /*wallclock:*/ 1234567890, 1);
        let ci_wrong_shred_version = CrdsValue::new(
            CrdsData::ContactInfo(contact_info_wrong_shred_version),
            &keypair,
        );

        // Test push message with non-matching shred version
        let mut msg = test_message(Protocol::PushMessage(
            keypair.pubkey(),
            vec![ci_wrong_shred_version],
        ));
        discard_different_shred_version(&mut msg, self_shred_version, &crds, &stats);
        if let Protocol::PushMessage(_, values) = msg.protocol() {
            assert_eq!(values.len(), 0);
        }

        // Test EpochSlot w/o previous CI with matching shred version/pubkey -> should be rejected
        let epoch_slots = EpochSlots::new_rand(&mut rng, Some(keypair.pubkey()));
        let es = CrdsValue::new_unsigned(CrdsData::EpochSlots(0, epoch_slots));
        let mut msg = test_message(Protocol::PushMessage(keypair.pubkey(), vec![es]));
        discard_different_shred_version(&mut msg, self_shred_version, &crds, &stats);
        if let Protocol::PushMessage(_, values) = msg.protocol() {
            assert_eq!(values.len(), 0);
        }

        // Insert ContactInfo with different pubkey than EpochSlot
        let keypair2 = Keypair::new();
        let ci_wrong_pubkey = CrdsValue::new(
            CrdsData::ContactInfo(ContactInfo::new(
                keypair2.pubkey(),
                /*wallclock:*/ 1234567890,
                self_shred_version,
            )),
            &keypair2,
        );
        assert!(
            crds.insert(ci_wrong_pubkey, /*now=*/ 0, GossipRoute::LocalMessage)
                .is_ok()
        );

        // Test insert EpochSlot w/ previous ContactInfo w/ matching shred version but different pubkey -> should be rejected
        let epoch_slots = EpochSlots::new_rand(&mut rng, Some(keypair.pubkey()));
        let es: CrdsValue = CrdsValue::new_unsigned(CrdsData::EpochSlots(0, epoch_slots));
        let mut msg = test_message(Protocol::PushMessage(keypair.pubkey(), vec![es.clone()]));
        discard_different_shred_version(&mut msg, self_shred_version, &crds, &stats);
        if let Protocol::PushMessage(_, values) = msg.protocol() {
            assert_eq!(values.len(), 0);
        }

        // Now insert ContactInfo with same pubkey as EpochSlot
        assert!(
            crds.insert(ci.clone(), /*now=*/ 0, GossipRoute::LocalMessage)
                .is_ok()
        );

        let mut msg = test_message(Protocol::PushMessage(keypair.pubkey(), vec![es]));
        discard_different_shred_version(&mut msg, self_shred_version, &crds, &stats);
        if let Protocol::PushMessage(_, values) = msg.protocol() {
            assert_eq!(values.len(), 1);
        }

        // Test multiple ContactInfo/EpochSlot with various shred versions. Crds table contains ContactInfo from `keypair`
        let keypair3 = Keypair::new();
        let keypair4 = Keypair::new();
        let entries = vec![
            CrdsValue::new(
                CrdsData::ContactInfo(ContactInfo::new(
                    keypair2.pubkey(),
                    /*wallclock:*/ 1234567890,
                    self_shred_version,
                )),
                &keypair2,
            ),
            CrdsValue::new(
                CrdsData::ContactInfo(ContactInfo::new(
                    keypair3.pubkey(),
                    /*wallclock:*/ 1234567890,
                    1,
                )),
                &keypair3,
            ),
            CrdsValue::new_unsigned(CrdsData::EpochSlots(
                0,
                EpochSlots::new_rand(&mut rng, Some(keypair.pubkey())),
            )),
            CrdsValue::new(
                CrdsData::ContactInfo(ContactInfo::new(
                    keypair.pubkey(),
                    /*wallclock:*/ 1234567890,
                    self_shred_version,
                )),
                &keypair4,
            ),
        ];
        let mut msg = test_message(Protocol::PushMessage(keypair.pubkey(), entries));
        discard_different_shred_version(&mut msg, self_shred_version, &crds, &stats);
        if let Protocol::PushMessage(_, values) = msg.protocol() {
            // Only reject ContactInfo with invalid shred version. EpochSlot with associated ContactInfo is already in the table
            assert_eq!(values.len(), 3);
        }

        // Remove ContactInfo with matching pubkey as EpochSlot
        crds.remove(&ci.label(), /* now */ 0);

        // Test multiple ContactInfo with various shred versions. Crds table is empty
        let entries = vec![
            CrdsValue::new(
                CrdsData::ContactInfo(ContactInfo::new(
                    keypair2.pubkey(),
                    /*wallclock:*/ 1234567890,
                    self_shred_version,
                )),
                &keypair2,
            ),
            CrdsValue::new(
                CrdsData::ContactInfo(ContactInfo::new(
                    keypair3.pubkey(),
                    /*wallclock:*/ 1234567890,
                    1,
                )),
                &keypair3,
            ),
            CrdsValue::new_unsigned(CrdsData::EpochSlots(
                0,
                EpochSlots::new_rand(&mut rng, Some(keypair.pubkey())),
            )),
            CrdsValue::new(
                CrdsData::ContactInfo(ContactInfo::new(
                    keypair.pubkey(),
                    /*wallclock:*/ 1234567890,
                    self_shred_version,
                )),
                &keypair,
            ),
        ];
        let mut msg = test_message(Protocol::PushMessage(keypair.pubkey(), entries));
        discard_different_shred_version(&mut msg, self_shred_version, &crds, &stats);
        if let Protocol::PushMessage(_, values) = msg.protocol() {
            // Reject ContactInfo with invalid shred version and EpochSlot with no associated ContactInfo in the table
            assert_eq!(values.len(), 2);
        }
    }
}
