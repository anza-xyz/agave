#[cfg(test)]
use {
    crate::repair::standard_repair_handler::StandardRepairHandler,
    solana_ledger::{blockstore::Blockstore, get_tmp_ledger_path_auto_delete},
    solana_runtime::bank_forks::BankForks,
};
use {
    crate::{
        cluster_slots_service::cluster_slots::ClusterSlots,
        repair::{
            duplicate_repair_status::get_ancestor_hash_repair_sample_size,
            outstanding_requests::OutstandingRequests,
            repair_handler::RepairHandler,
            repair_service::{OutstandingShredRepairs, REPAIR_MS, RepairStats},
            request_response::RequestResponse,
            result::{Error, RepairVerifyError, Result},
        },
    },
    agave_votor_messages::{consensus_message::Block, migration::MigrationStatus},
    bincode::{Options, serialize},
    crossbeam_channel::{Receiver, RecvTimeoutError},
    lazy_lru::LruCache,
    rand::{
        Rng,
        distr::{
            Distribution,
            weighted::{Error as WeightedError, WeightedIndex},
        },
    },
    serde::{Deserialize, Serialize},
    solana_clock::Slot,
    solana_gossip::{
        cluster_info::{ClusterInfo, ClusterInfoError},
        contact_info::{ContactInfo, Protocol},
        ping_pong::{self, Pong},
        weighted_shuffle::WeightedShuffle,
    },
    solana_hash::{HASH_BYTES, Hash},
    solana_keypair::{Keypair, signable::Signable},
    solana_ledger::{
        blockstore_meta::BlockLocation,
        shred::{
            self, DATA_SHREDS_PER_FEC_BLOCK, MAX_FEC_SETS_PER_SLOT, Nonce, SIZE_OF_NONCE,
            ShredFetchStats, ShredType, layout::get_merkle_root, merkle_tree,
        },
    },
    solana_net_utils::{SocketAddrSpace, token_bucket::TokenBucket},
    solana_packet::PACKET_DATA_SIZE,
    solana_perf::packet::{
        BytesPacket, Packet, PacketBatch, PacketBatchRecycler, PacketRef, RecycledPacketBatch,
    },
    solana_poh::poh_recorder::SharedLeaderState,
    solana_pubkey::{PUBKEY_BYTES, Pubkey},
    solana_runtime::bank_forks::SharableBanks,
    solana_sha256_hasher::hashv,
    solana_signature::{SIGNATURE_BYTES, Signature},
    solana_signer::Signer,
    solana_streamer::{
        sendmmsg::{SendPktsError, batch_send},
        streamer::PacketBatchSender,
    },
    solana_time_utils::timestamp,
    std::{
        cmp::Reverse,
        collections::{HashMap, HashSet},
        net::{SocketAddr, UdpSocket},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

/// the number of slots to respond with when responding to `Orphan` requests
pub const MAX_ORPHAN_REPAIR_RESPONSES: usize = 11;
// Number of slots to cache their respective repair peers and sampling weights.
pub(crate) const REPAIR_PEERS_CACHE_CAPACITY: usize = 128;
// Limit cache entries ttl in order to avoid re-using outdated data.
const REPAIR_PEERS_CACHE_TTL: Duration = Duration::from_secs(10);

#[cfg(test)]
static_assertions::const_assert_eq!(MAX_ANCESTOR_BYTES_IN_PACKET, 1220);
pub const MAX_ANCESTOR_BYTES_IN_PACKET: usize =
    PACKET_DATA_SIZE -
    SIZE_OF_NONCE -
    4 /*(response version enum discriminator)*/ -
    4 /*slot_hash length*/;

pub const MAX_ANCESTOR_RESPONSES: usize =
    MAX_ANCESTOR_BYTES_IN_PACKET / std::mem::size_of::<(Slot, Hash)>();
/// Number of bytes in the randomly generated token sent with ping messages.
const REPAIR_PING_TOKEN_SIZE: usize = HASH_BYTES;
pub const REPAIR_PING_CACHE_CAPACITY: usize = 65536;
pub const REPAIR_PING_CACHE_TTL: Duration = Duration::from_secs(1280);
const REPAIR_PING_CACHE_RATE_LIMIT_DELAY: Duration = Duration::from_secs(2);
pub(crate) const REPAIR_RESPONSE_SERIALIZED_PING_BYTES: usize =
    4 /*enum discriminator*/ + PUBKEY_BYTES + REPAIR_PING_TOKEN_SIZE + SIGNATURE_BYTES;
const SIGNED_REPAIR_TIME_WINDOW: Duration = Duration::from_secs(60 * 10); // 10 min

#[cfg(test)]
static_assertions::const_assert_eq!(MAX_ANCESTOR_RESPONSES, 30);

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum ShredRepairType {
    /// Requesting `MAX_ORPHAN_REPAIR_RESPONSES ` parent shreds
    Orphan(Slot),
    /// Requesting any shred with index greater than or equal to the particular index
    HighestShred(Slot, u64),
    /// Requesting the missing shred at a particular index
    Shred(Slot, u64),
    /// Requesting the missing shred at a particular index for a specific block ID
    ShredForBlockId {
        slot: Slot,
        index: u32,
        fec_set_merkle_root: Hash,
        // Double merkle block id
        block_id: Hash,
    },
}

impl ShredRepairType {
    pub fn slot(&self) -> Slot {
        match self {
            ShredRepairType::Orphan(slot)
            | ShredRepairType::HighestShred(slot, _)
            | ShredRepairType::Shred(slot, _) => *slot,
            ShredRepairType::ShredForBlockId { slot, .. } => *slot,
        }
    }

    pub fn block_id(&self) -> Option<Hash> {
        match self {
            ShredRepairType::ShredForBlockId { block_id, .. } => Some(*block_id),
            ShredRepairType::Orphan(_)
            | ShredRepairType::HighestShred(_, _)
            | ShredRepairType::Shred(_, _) => None,
        }
    }
}

impl RequestResponse for ShredRepairType {
    type Response = [u8]; // shred's payload
    fn num_expected_responses(&self) -> u32 {
        match self {
            ShredRepairType::Orphan(_) => MAX_ORPHAN_REPAIR_RESPONSES as u32,
            ShredRepairType::Shred(_, _)
            | ShredRepairType::HighestShred(_, _)
            | ShredRepairType::ShredForBlockId { .. } => 1,
        }
    }
    fn verify_response(&self, shred: &Self::Response) -> bool {
        #[inline]
        fn get_shred_index(shred: &[u8]) -> Option<u64> {
            shred::layout::get_index(shred).map(u64::from)
        }
        let Some(shred_slot) = shred::layout::get_slot(shred) else {
            return false;
        };
        match self {
            ShredRepairType::Orphan(slot) => shred_slot <= *slot,
            ShredRepairType::HighestShred(slot, index) => {
                shred_slot == *slot && get_shred_index(shred) >= Some(*index)
            }
            ShredRepairType::Shred(slot, index) => {
                shred_slot == *slot && get_shred_index(shred) == Some(*index)
            }
            ShredRepairType::ShredForBlockId {
                slot,
                index,
                fec_set_merkle_root,
                ..
            } => {
                shred_slot == *slot
                    && matches!(shred::layout::get_shred_type(shred), Ok(ShredType::Data))
                    && shred::layout::get_index(shred) == Some(*index)
                    && get_merkle_root(shred) == Some(*fec_set_merkle_root)
            }
        }
    }
}

#[derive(Copy, Clone)]
pub struct AncestorHashesRepairType(pub Slot);
impl AncestorHashesRepairType {
    pub fn slot(&self) -> Slot {
        self.0
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub enum AncestorHashesResponse {
    Hashes(Vec<(Slot, Hash)>),
    Ping(Ping),
}

impl RequestResponse for AncestorHashesRepairType {
    type Response = AncestorHashesResponse;
    fn num_expected_responses(&self) -> u32 {
        1
    }
    fn verify_response(&self, response: &AncestorHashesResponse) -> bool {
        match response {
            AncestorHashesResponse::Hashes(hashes) => hashes.len() <= MAX_ANCESTOR_RESPONSES,
            AncestorHashesResponse::Ping(ping) => ping.verify(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BlockIdRepairType {
    ParentAndFecSetCount {
        slot: Slot,
        block_id: Hash,
    },

    FecSetRoot {
        slot: Slot,
        block_id: Hash,
        fec_set_index: u32,
    },
}

#[allow(dead_code)]
impl BlockIdRepairType {
    pub(crate) fn block(&self) -> Block {
        match self {
            BlockIdRepairType::ParentAndFecSetCount { slot, block_id } => (*slot, *block_id),
            BlockIdRepairType::FecSetRoot { slot, block_id, .. } => (*slot, *block_id),
        }
    }

    pub(crate) fn slot(&self) -> Slot {
        self.block().0
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub enum BlockIdRepairResponse {
    ParentFecSetCount {
        fec_set_count: u32,
        parent_info: (Slot, Hash),
        parent_proof: Vec<u8>,
    },

    FecSetRoot {
        fec_set_root: Hash,
        fec_set_proof: Vec<u8>,
    },

    Ping {
        ping: Ping,
    },
}

impl RequestResponse for BlockIdRepairType {
    type Response = BlockIdRepairResponse;

    fn num_expected_responses(&self) -> u32 {
        // The only variable in the response are the merkle proofs.
        // Even assuming an extreme of u64::MAX fec sets, that corresponds to
        // 63 * 20 = 1260 bytes for a proof which fits in one packet.
        1
    }

    fn verify_response(&self, response: &Self::Response) -> bool {
        match (self, response) {
            (_, Self::Response::Ping { ping }) => ping.verify(),
            (
                Self::ParentAndFecSetCount {
                    slot: _slot,
                    block_id,
                },
                Self::Response::ParentFecSetCount {
                    fec_set_count,
                    parent_info: (parent_slot, parent_block_id),
                    parent_proof,
                },
            ) => {
                if *fec_set_count > MAX_FEC_SETS_PER_SLOT {
                    return false;
                }

                // + 1 here to account for the parent info which is the final leaf of the tree
                let proof_size = merkle_tree::get_proof_size(*fec_set_count as usize + 1);
                if parent_proof.len()
                    != proof_size as usize * merkle_tree::SIZE_OF_MERKLE_PROOF_ENTRY
                {
                    return false;
                }

                let parent_info_leaf =
                    hashv(&[&parent_slot.to_le_bytes(), parent_block_id.as_ref()]);
                merkle_tree::verify_merkle_proof(
                    parent_info_leaf,
                    *fec_set_count as usize,
                    parent_proof,
                    *block_id,
                )
                .is_ok()
            }

            (
                Self::FecSetRoot {
                    slot: _slot,
                    block_id,
                    fec_set_index,
                },
                Self::Response::FecSetRoot {
                    fec_set_root,
                    fec_set_proof,
                },
            ) => {
                debug_assert_eq!(*fec_set_index as usize % DATA_SHREDS_PER_FEC_BLOCK, 0);
                // Convert from shred-space to leaf-index
                let leaf_index = *fec_set_index as usize / DATA_SHREDS_PER_FEC_BLOCK;
                merkle_tree::verify_merkle_proof(
                    *fec_set_root,
                    leaf_index,
                    fec_set_proof,
                    *block_id,
                )
                .is_ok()
            }

            (Self::ParentAndFecSetCount { .. }, _) | (Self::FecSetRoot { .. }, _) => false,
        }
    }
}

#[derive(Default)]
struct ServeRepairStats {
    total_requests: usize,
    dropped_requests_outbound_bandwidth: usize,
    dropped_requests_load_shed: usize,
    dropped_requests_load_shed_sigverify: usize,
    dropped_requests_low_stake: usize,
    whitelisted_requests: usize,
    total_dropped_response_packets: usize,
    total_response_packets: usize,
    total_response_bytes_staked: usize,
    total_response_bytes_unstaked: usize,
    processed: usize,
    window_index: usize,
    highest_window_index: usize,
    orphan: usize,
    pong: usize,
    ancestor_hashes: usize,
    parent: usize,
    fec_set_root: usize,
    window_index_for_block_id: usize,
    window_index_misses: usize,
    parent_misses: usize,
    fec_set_root_misses: usize,
    window_index_for_block_id_misses: usize,
    ping_cache_check_failed: usize,
    pings_sent: usize,
    decode_time_us: u64,
    handle_requests_time_us: u64,
    handle_requests_staked: usize,
    handle_requests_unstaked: usize,
    err_self_repair: usize,
    err_time_skew: usize,
    err_malformed: usize,
    err_sig_verify: usize,
    err_unsigned: usize,
    err_id_mismatch: usize,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample, StableAbi))]
#[derive(Debug, Deserialize, Serialize)]
pub struct RepairRequestHeader {
    signature: Signature,
    sender: Pubkey,
    recipient: Pubkey,
    timestamp: u64,
    nonce: Nonce,
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<RepairRequestHeader>
    for solana_frozen_abi::rand::distr::StandardUniform
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> RepairRequestHeader {
        RepairRequestHeader {
            signature: Signature::from(rng.random::<[u8; SIGNATURE_BYTES]>()),
            sender: Pubkey::new_from_array(rng.random()),
            recipient: Pubkey::new_from_array(rng.random()),
            timestamp: rng.random(),
            nonce: rng.random(),
        }
    }
}

impl RepairRequestHeader {
    pub fn new(sender: Pubkey, recipient: Pubkey, timestamp: u64, nonce: Nonce) -> Self {
        Self {
            signature: Signature::default(),
            sender,
            recipient,
            timestamp,
            nonce,
        }
    }

    /// Returns the nonce for this repair request.
    pub fn nonce(&self) -> Nonce {
        self.nonce
    }
}

type Ping = ping_pong::Ping<REPAIR_PING_TOKEN_SIZE>;
type PingCache = ping_pong::PingCache<REPAIR_PING_TOKEN_SIZE>;

/// Window protocol messages
/// Appending new messages here is safe as long as it is feature gated.
/// Changing the format of an existing message is possible but not advised.
/// Removing a message is possible by first removing the sender and feature gating the response.
/// The message can then be removed once the feature gate is active and there are no responders.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiEnumVisitor, AbiExample, StableAbi),
    frozen_abi(
        api_digest = "2wGmauKxLKD81QzmBo7CtW1tKVr9P5WgMs7RzJA9kvpd",
        abi_digest = "7DpV7t5vZAWSZEshJ4hAVTHnR37jzLwUMYhs1fGrX3G5"
    )
)]
#[derive(Debug, Deserialize, Serialize)]
pub enum RepairProtocol {
    LegacyWindowIndex,
    LegacyHighestWindowIndex,
    LegacyOrphan,
    LegacyWindowIndexWithNonce,
    LegacyHighestWindowIndexWithNonce,
    LegacyOrphanWithNonce,
    LegacyAncestorHashes,
    Pong(ping_pong::Pong),
    WindowIndex {
        header: RepairRequestHeader,
        slot: Slot,
        shred_index: u64,
    },
    HighestWindowIndex {
        header: RepairRequestHeader,
        slot: Slot,
        shred_index: u64,
    },
    Orphan {
        header: RepairRequestHeader,
        slot: Slot,
    },
    AncestorHashes {
        header: RepairRequestHeader,
        slot: Slot,
    },
    ParentAndFecSetCount {
        header: RepairRequestHeader,
        slot: Slot,
        block_id: Hash,
    },
    FecSetRoot {
        header: RepairRequestHeader,
        slot: Slot,
        block_id: Hash,
        fec_set_index: u32,
    },
    WindowIndexForBlockId {
        header: RepairRequestHeader,
        slot: Slot,
        shred_index: u32,
        fec_set_merkle_root: Hash,
        block_id: Hash,
    },
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::rand::prelude::Distribution<RepairProtocol>
    for solana_frozen_abi::rand::distr::StandardUniform
{
    fn sample<R: solana_frozen_abi::rand::Rng + ?Sized>(&self, rng: &mut R) -> RepairProtocol {
        use ping_pong::{Ping, Pong};
        let variant = rng.random_range(7..=11);
        match variant {
            // we never actually use any of the Legacy_ variants
            // so we don't need to sample them here
            7 => {
                let mut token = [0u8; 8];
                rng.fill_bytes(&mut token);
                let keypair = Keypair::new_from_array(rng.random());
                let ping = Ping::new(token, &keypair);
                RepairProtocol::Pong(Pong::new(&ping, &keypair))
            }
            8 => RepairProtocol::WindowIndex {
                header: rng.random(),
                slot: rng.random(),
                shred_index: rng.random(),
            },
            9 => RepairProtocol::HighestWindowIndex {
                header: rng.random(),
                slot: rng.random(),
                shred_index: rng.random(),
            },
            10 => RepairProtocol::Orphan {
                header: rng.random(),
                slot: rng.random(),
            },
            11 => RepairProtocol::AncestorHashes {
                header: rng.random(),
                slot: rng.random(),
            },
            _ => unreachable!(),
        }
    }
}

const REPAIR_REQUEST_PONG_SERIALIZED_BYTES: usize = PUBKEY_BYTES + HASH_BYTES + SIGNATURE_BYTES;
const REPAIR_REQUEST_MIN_BYTES: usize = REPAIR_REQUEST_PONG_SERIALIZED_BYTES;

fn is_well_formed_repair_request(packet: &PacketRef, stats: &mut ServeRepairStats) -> bool {
    let well_formed = packet
        .data(..)
        .is_some_and(|data| data.len() >= REPAIR_REQUEST_MIN_BYTES);
    if !well_formed {
        stats.err_malformed += 1;
    }
    well_formed
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum RepairResponse {
    Ping(Ping),
}

impl RepairProtocol {
    fn sender(&self) -> Option<&Pubkey> {
        match self {
            Self::LegacyWindowIndex
            | Self::LegacyHighestWindowIndex
            | Self::LegacyOrphan
            | Self::LegacyWindowIndexWithNonce
            | Self::LegacyHighestWindowIndexWithNonce
            | Self::LegacyOrphanWithNonce
            | Self::LegacyAncestorHashes => None,
            Self::Pong(pong) => Some(pong.from()),
            Self::WindowIndex { header, .. }
            | Self::HighestWindowIndex { header, .. }
            | Self::Orphan { header, .. }
            | Self::AncestorHashes { header, .. }
            | Self::ParentAndFecSetCount { header, .. }
            | Self::FecSetRoot { header, .. }
            | Self::WindowIndexForBlockId { header, .. } => Some(&header.sender),
        }
    }

    fn supports_signature(&self) -> bool {
        match self {
            Self::LegacyWindowIndex
            | Self::LegacyHighestWindowIndex
            | Self::LegacyOrphan
            | Self::LegacyWindowIndexWithNonce
            | Self::LegacyHighestWindowIndexWithNonce
            | Self::LegacyOrphanWithNonce
            | Self::LegacyAncestorHashes => false,
            Self::Pong(_)
            | Self::WindowIndex { .. }
            | Self::HighestWindowIndex { .. }
            | Self::Orphan { .. }
            | Self::AncestorHashes { .. }
            | Self::ParentAndFecSetCount { .. }
            | Self::FecSetRoot { .. }
            | Self::WindowIndexForBlockId { .. } => true,
        }
    }

    fn max_response_packets(&self) -> usize {
        match self {
            RepairProtocol::WindowIndex { .. }
            | RepairProtocol::HighestWindowIndex { .. }
            | RepairProtocol::AncestorHashes { .. }
            | RepairProtocol::ParentAndFecSetCount { .. }
            | RepairProtocol::FecSetRoot { .. }
            | RepairProtocol::WindowIndexForBlockId { .. } => 1,
            RepairProtocol::Orphan { .. } => MAX_ORPHAN_REPAIR_RESPONSES,
            RepairProtocol::Pong(_) => 0, // no response
            RepairProtocol::LegacyWindowIndex
            | RepairProtocol::LegacyHighestWindowIndex
            | RepairProtocol::LegacyOrphan
            | RepairProtocol::LegacyWindowIndexWithNonce
            | RepairProtocol::LegacyHighestWindowIndexWithNonce
            | RepairProtocol::LegacyOrphanWithNonce
            | RepairProtocol::LegacyAncestorHashes => 0, // unsupported
        }
    }

    fn max_response_bytes(&self) -> usize {
        self.max_response_packets() * PACKET_DATA_SIZE
    }
}

pub struct ServeRepair {
    cluster_info: Arc<ClusterInfo>,
    sharable_banks: SharableBanks,
    repair_whitelist: Arc<RwLock<HashSet<Pubkey>>>,
    repair_handler: Box<dyn RepairHandler + Send + Sync>,
    leader_state: Option<SharedLeaderState>,
    migration_status: Arc<MigrationStatus>,
}

// Cache entry for repair peers for a slot.
pub(crate) struct RepairPeers {
    asof: Instant,
    peers: Vec<Node>,
    weighted_index: WeightedIndex<u64>,
}

struct Node {
    pubkey: Pubkey,
    serve_repair: SocketAddr,
}

impl RepairPeers {
    fn new(asof: Instant, peers: &[ContactInfo], weights: &[u64]) -> Result<Self> {
        if peers.len() != weights.len() {
            return Err(Error::from(WeightedError::InvalidWeight));
        }
        let (peers, weights): (Vec<_>, Vec<u64>) = peers
            .iter()
            .zip(weights)
            .filter_map(|(peer, &weight)| {
                let node = Node {
                    pubkey: *peer.pubkey(),
                    serve_repair: peer.serve_repair(Protocol::UDP)?,
                };
                Some((node, weight))
            })
            .unzip();
        if peers.is_empty() {
            return Err(Error::from(ClusterInfoError::NoPeers));
        }
        let weighted_index = WeightedIndex::new(weights)?;
        Ok(Self {
            asof,
            peers,
            weighted_index,
        })
    }

    fn sample<R: Rng>(&self, rng: &mut R) -> &Node {
        let index = self.weighted_index.sample(rng);
        &self.peers[index]
    }
}

struct RepairRequestWithMeta {
    request: RepairProtocol,
    from_addr: SocketAddr,
    stake: u64,
    whitelisted: bool,
}

impl ServeRepair {
    pub fn new(
        cluster_info: Arc<ClusterInfo>,
        sharable_banks: SharableBanks,
        repair_whitelist: Arc<RwLock<HashSet<Pubkey>>>,
        repair_handler: Box<dyn RepairHandler + Send + Sync>,
        migration_status: Arc<MigrationStatus>,
    ) -> Self {
        Self {
            cluster_info,
            sharable_banks,
            repair_whitelist,
            repair_handler,
            leader_state: None,
            migration_status,
        }
    }

    pub fn new_with_leader_state(
        cluster_info: Arc<ClusterInfo>,
        sharable_banks: SharableBanks,
        repair_whitelist: Arc<RwLock<HashSet<Pubkey>>>,
        repair_handler: Box<dyn RepairHandler + Send + Sync>,
        leader_state: SharedLeaderState,
        migration_status: Arc<MigrationStatus>,
    ) -> Self {
        Self {
            cluster_info,
            sharable_banks,
            repair_whitelist,
            repair_handler,
            leader_state: Some(leader_state),
            migration_status,
        }
    }

    #[cfg(test)]
    pub fn new_for_test(
        cluster_info: Arc<ClusterInfo>,
        bank_forks: Arc<RwLock<BankForks>>,
        repair_whitelist: Arc<RwLock<HashSet<Pubkey>>>,
    ) -> Self {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let repair_handler = Box::new(StandardRepairHandler::new(blockstore));
        let bank_forks_r = bank_forks.read().unwrap();
        Self::new(
            cluster_info,
            bank_forks_r.sharable_banks(),
            repair_whitelist,
            repair_handler,
            bank_forks_r.migration_status(),
        )
    }

    #[cfg(test)]
    pub(crate) fn my_id(&self) -> Pubkey {
        self.cluster_info.id()
    }

    fn handle_repair(
        &self,
        recycler: &PacketBatchRecycler,
        from_addr: &SocketAddr,
        request: RepairProtocol,
        stats: &mut ServeRepairStats,
        ping_cache: &mut PingCache,
    ) -> Option<PacketBatch> {
        let now = Instant::now();
        let (res, label) = {
            match &request {
                RepairProtocol::WindowIndex {
                    header: RepairRequestHeader { nonce, .. },
                    slot,
                    shred_index,
                } => {
                    stats.window_index += 1;
                    let batch = self.repair_handler.run_window_request(
                        recycler,
                        from_addr,
                        *slot,
                        *shred_index,
                        *nonce,
                    );
                    if batch.is_none() {
                        stats.window_index_misses += 1;
                    }
                    (batch, "WindowIndexWithNonce")
                }
                RepairProtocol::HighestWindowIndex {
                    header: RepairRequestHeader { nonce, .. },
                    slot,
                    shred_index: highest_index,
                } => {
                    stats.highest_window_index += 1;
                    (
                        self.repair_handler.run_highest_window_request(
                            recycler,
                            from_addr,
                            *slot,
                            *highest_index,
                            *nonce,
                        ),
                        "HighestWindowIndexWithNonce",
                    )
                }
                RepairProtocol::Orphan {
                    header: RepairRequestHeader { nonce, .. },
                    slot,
                } => {
                    stats.orphan += 1;
                    (
                        self.repair_handler.run_orphan(
                            recycler,
                            from_addr,
                            *slot,
                            MAX_ORPHAN_REPAIR_RESPONSES,
                            *nonce,
                        ),
                        "OrphanWithNonce",
                    )
                }
                RepairProtocol::AncestorHashes {
                    header: RepairRequestHeader { nonce, .. },
                    slot,
                } => {
                    stats.ancestor_hashes += 1;
                    if self
                        .migration_status
                        .should_respond_to_ancestor_hashes_requests(*slot)
                    {
                        (
                            self.repair_handler
                                .run_ancestor_hashes(recycler, from_addr, *slot, *nonce),
                            "AncestorHashes",
                        )
                    } else {
                        (None, "AncestorHashes")
                    }
                }
                RepairProtocol::Pong(pong) => {
                    stats.pong += 1;
                    ping_cache.add(pong, *from_addr, Instant::now());
                    (None, "Pong")
                }
                RepairProtocol::ParentAndFecSetCount {
                    header: RepairRequestHeader { nonce, .. },
                    slot,
                    block_id,
                } => {
                    stats.parent += 1;
                    let response = if self.migration_status.should_allow_block_markers(*slot) {
                        let response = self.repair_handler.run_parent_fec_set_count(
                            recycler, from_addr, *slot, *block_id, *nonce,
                        );
                        if response.is_none() {
                            stats.parent_misses += 1;
                        }
                        response
                    } else {
                        None
                    };
                    (response, "Parent")
                }
                RepairProtocol::FecSetRoot {
                    header: RepairRequestHeader { nonce, .. },
                    slot,
                    block_id,
                    fec_set_index,
                } => {
                    stats.fec_set_root += 1;
                    let response = if self.migration_status.should_allow_block_markers(*slot) {
                        let response = self.repair_handler.run_fec_set_root(
                            recycler,
                            from_addr,
                            *slot,
                            *block_id,
                            *fec_set_index,
                            *nonce,
                        );
                        if response.is_none() {
                            stats.fec_set_root_misses += 1;
                        }
                        response
                    } else {
                        None
                    };
                    (response, "FecSetRoot")
                }
                RepairProtocol::WindowIndexForBlockId {
                    header: RepairRequestHeader { nonce, .. },
                    slot,
                    shred_index,
                    fec_set_merkle_root: _,
                    block_id,
                } => {
                    stats.window_index_for_block_id += 1;
                    let response = if self.migration_status.should_allow_block_markers(*slot) {
                        let batch = self.repair_handler.run_window_request_for_block_id(
                            recycler,
                            from_addr,
                            *slot,
                            u64::from(*shred_index),
                            *block_id,
                            *nonce,
                        );
                        if batch.is_none() {
                            stats.window_index_for_block_id_misses += 1;
                        }
                        batch
                    } else {
                        None
                    };
                    (response, "WindowIndexForBlockIdWithNonce")
                }
                RepairProtocol::LegacyWindowIndex
                | RepairProtocol::LegacyWindowIndexWithNonce
                | RepairProtocol::LegacyHighestWindowIndex
                | RepairProtocol::LegacyHighestWindowIndexWithNonce
                | RepairProtocol::LegacyOrphan
                | RepairProtocol::LegacyOrphanWithNonce
                | RepairProtocol::LegacyAncestorHashes => {
                    error!("Unexpected legacy request: {request:?}");
                    debug_assert!(
                        false,
                        "Legacy requests should have been filtered out during signature \
                         verification. {request:?}"
                    );
                    (None, "Legacy")
                }
            }
        };
        Self::report_time_spent(label, &now.elapsed(), "");
        res
    }

    fn report_time_spent(label: &str, time: &Duration, extra: &str) {
        let count = time.as_millis();
        if count > 5 {
            info!("{label} took: {count} ms {extra}");
        }
    }

    fn decode_request(
        remote_request: BytesPacket,
        epoch_staked_nodes: &Option<Arc<HashMap<Pubkey, u64>>>,
        whitelist: &HashSet<Pubkey>,
        my_id: &Pubkey,
        socket_addr_space: &SocketAddrSpace,
    ) -> Result<RepairRequestWithMeta> {
        let Ok(request) = deserialize_request::<RepairProtocol>(&remote_request) else {
            return Err(Error::from(RepairVerifyError::Malformed));
        };
        let from_addr = remote_request.meta().socket_addr();
        if !ContactInfo::is_valid_address(&from_addr, socket_addr_space) {
            return Err(Error::from(RepairVerifyError::Malformed));
        }
        Self::verify_signed_packet(my_id, remote_request.buffer(), &request)?;
        if request.sender() == Some(my_id) {
            error!("self repair: from_addr={from_addr} my_id={my_id} request={request:?}");
            return Err(Error::from(RepairVerifyError::SelfRepair));
        }
        let stake = *epoch_staked_nodes
            .as_ref()
            .and_then(|stakes| stakes.get(request.sender()?))
            .unwrap_or(&0);

        let whitelisted = request
            .sender()
            .map(|pubkey| whitelist.contains(pubkey))
            .unwrap_or_default();

        Ok(RepairRequestWithMeta {
            request,
            from_addr,
            stake,
            whitelisted,
        })
    }

    fn record_request_decode_error(error: &Error, stats: &mut ServeRepairStats) {
        match error {
            Error::RepairVerify(RepairVerifyError::IdMismatch) => {
                stats.err_id_mismatch += 1;
            }
            Error::RepairVerify(RepairVerifyError::Malformed) => {
                stats.err_malformed += 1;
            }
            Error::RepairVerify(RepairVerifyError::SelfRepair) => {
                stats.err_self_repair += 1;
            }
            Error::RepairVerify(RepairVerifyError::SigVerify) => {
                stats.err_sig_verify += 1;
            }
            Error::RepairVerify(RepairVerifyError::TimeSkew) => {
                stats.err_time_skew += 1;
            }
            Error::RepairVerify(RepairVerifyError::Unsigned) => {
                stats.err_unsigned += 1;
            }
            _ => {
                debug_assert!(false, "unhandled error {error:?}");
            }
        }
    }

    fn decode_requests(
        requests: Vec<BytesPacket>,
        epoch_staked_nodes: &Option<Arc<HashMap<Pubkey, u64>>>,
        whitelist: &HashSet<Pubkey>,
        my_id: &Pubkey,
        socket_addr_space: &SocketAddrSpace,
        mut remaining_budget_estimate: usize,
        stats: &mut ServeRepairStats,
    ) -> Vec<RepairRequestWithMeta> {
        const MIN_RESPONSE_SIZE: usize = PACKET_DATA_SIZE + SIZE_OF_NONCE;
        let decode_request = |request| {
            if remaining_budget_estimate < MIN_RESPONSE_SIZE {
                stats.dropped_requests_load_shed_sigverify += 1;
                return None;
            }
            let result = Self::decode_request(
                request,
                epoch_staked_nodes,
                whitelist,
                my_id,
                socket_addr_space,
            );
            match &result {
                Ok(req) => {
                    if req.stake == 0 {
                        stats.handle_requests_unstaked += 1;
                    } else {
                        stats.handle_requests_staked += 1;
                    }
                    // assuming we will reply to the request, we need to update the budget estimate
                    // some responses may be larger, but we have to be conservative here
                    remaining_budget_estimate -= MIN_RESPONSE_SIZE;
                }
                Err(e) => {
                    Self::record_request_decode_error(e, stats);
                }
            }
            result.ok()
        };
        requests.into_iter().filter_map(decode_request).collect()
    }

    /// Process messages from the network
    fn run_listen(
        &mut self,
        ping_cache: &mut PingCache,
        recycler: &PacketBatchRecycler,
        requests_receiver: &Receiver<PacketBatch>,
        response_sender: &PacketBatchSender,
        stats: &mut ServeRepairStats,
        data_budget: &TokenBucket,
    ) -> std::result::Result<(), RecvTimeoutError> {
        /// How much more expensive it is to serve bytes if we are a leader
        const LEADER_BYTE_COST_MULTIPLIER: usize = 10;
        const TIMEOUT: Duration = Duration::from_secs(1);
        let initial_batch = requests_receiver.recv_timeout(TIMEOUT)?;

        const MAX_REQUESTS_PER_ITERATION: usize = 1024;
        let mut total_requests = initial_batch.len();

        let socket_addr_space = *self.cluster_info.socket_addr_space();
        let root_bank = self.sharable_banks.root();
        let epoch_staked_nodes = root_bank.epoch_staked_nodes(root_bank.epoch());
        let identity_keypair = self.cluster_info.keypair();
        let my_id = identity_keypair.pubkey();

        let target_max_buffered_packets = if !self.repair_whitelist.read().unwrap().is_empty() {
            4 * MAX_REQUESTS_PER_ITERATION
        } else {
            2 * MAX_REQUESTS_PER_ITERATION
        };

        let mut requests = Vec::<BytesPacket>::with_capacity(64);
        for packet in initial_batch.iter() {
            if is_well_formed_repair_request(&packet, stats) {
                requests.push(packet.to_bytes_packet());
            }
        }
        while let Ok(batch) = requests_receiver.try_recv() {
            total_requests += batch.len();
            for packet in batch.into_iter() {
                if is_well_formed_repair_request(&packet, stats) {
                    requests.push(packet.to_bytes_packet());
                }
            }

            if requests.len() > target_max_buffered_packets {
                // Already exceeded max_buffered_packets. We must be under extreme load.
                // Don't waste time on stale requests and eradicate all buffered packets.
                let drained: usize = requests_receiver.try_iter().map(|batch| batch.len()).sum();
                total_requests += drained;
                stats.dropped_requests_load_shed += drained;
                break;
            }
        }

        stats.total_requests += total_requests;

        // Check if we are currently a leader, so we can limit the service rate
        // If information is not available, assume we are not a leader.
        let is_leader = self
            .leader_state
            .as_ref()
            .map(|ls| ls.load().working_bank().is_some())
            .unwrap_or(false);
        // assign extra cost to served bytes if we are a leader
        let byte_cost_multiplier = if is_leader {
            LEADER_BYTE_COST_MULTIPLIER
        } else {
            1
        };

        let decode_start = Instant::now();
        let mut decoded_requests = {
            // Estimate how much data budget we have left, 2x margin to prioritize staked,
            // apply byte cost multiplier here so we can operate in bytes inside decode_requests
            let effective_data_budget_estimate =
                (data_budget.current_tokens() as usize).saturating_mul(2) / byte_cost_multiplier;
            // staked requests (as those get filtered after sigverify)
            let whitelist = self.repair_whitelist.read().unwrap();
            Self::decode_requests(
                requests,
                &epoch_staked_nodes,
                &whitelist,
                &my_id,
                &socket_addr_space,
                effective_data_budget_estimate,
                stats,
            )
        };
        let whitelisted_request_count = decoded_requests.iter().filter(|r| r.whitelisted).count();
        stats.decode_time_us += decode_start.elapsed().as_micros() as u64;
        stats.whitelisted_requests += whitelisted_request_count.min(MAX_REQUESTS_PER_ITERATION);

        if decoded_requests.len() > MAX_REQUESTS_PER_ITERATION {
            stats.dropped_requests_low_stake += decoded_requests.len() - MAX_REQUESTS_PER_ITERATION;
            decoded_requests.sort_unstable_by_key(|r| Reverse((r.whitelisted, r.stake)));
            decoded_requests.truncate(MAX_REQUESTS_PER_ITERATION);
        }

        let handle_requests_start = Instant::now();
        self.handle_requests(
            ping_cache,
            recycler,
            decoded_requests,
            response_sender,
            stats,
            data_budget,
            byte_cost_multiplier,
        );
        stats.handle_requests_time_us += handle_requests_start.elapsed().as_micros() as u64;

        Ok(())
    }

    fn report_reset_stats(&self, stats: &mut ServeRepairStats) {
        if stats.err_self_repair > 0 {
            let my_id = self.cluster_info.id();
            warn!(
                "{}: Ignored received repair requests from ME: {}",
                my_id, stats.err_self_repair,
            );
        }

        datapoint_info!(
            "serve_repair-requests_received",
            ("total_requests", stats.total_requests, i64),
            (
                "dropped_requests_outbound_bandwidth",
                stats.dropped_requests_outbound_bandwidth,
                i64
            ),
            (
                "dropped_requests_load_shed",
                stats.dropped_requests_load_shed,
                i64
            ),
            (
                "dropped_requests_load_shed_sigverify",
                stats.dropped_requests_load_shed_sigverify,
                i64
            ),
            (
                "dropped_requests_low_stake",
                stats.dropped_requests_low_stake,
                i64
            ),
            ("whitelisted_requests", stats.whitelisted_requests, i64),
            (
                "total_dropped_response_packets",
                stats.total_dropped_response_packets,
                i64
            ),
            ("handle_requests_staked", stats.handle_requests_staked, i64),
            (
                "handle_requests_unstaked",
                stats.handle_requests_unstaked,
                i64
            ),
            ("processed", stats.processed, i64),
            ("total_response_packets", stats.total_response_packets, i64),
            (
                "total_response_bytes_staked",
                stats.total_response_bytes_staked,
                i64
            ),
            (
                "total_response_bytes_unstaked",
                stats.total_response_bytes_unstaked,
                i64
            ),
            ("self_repair", stats.err_self_repair, i64),
            ("window_index", stats.window_index, i64),
            ("parent", stats.parent, i64),
            ("fec_set_root", stats.fec_set_root, i64),
            (
                "window_index_for_block_id",
                stats.window_index_for_block_id,
                i64
            ),
            (
                "request-highest-window-index",
                stats.highest_window_index,
                i64
            ),
            ("orphan", stats.orphan, i64),
            (
                "serve_repair-request-ancestor-hashes",
                stats.ancestor_hashes,
                i64
            ),
            ("pong", stats.pong, i64),
            ("window_index_misses", stats.window_index_misses, i64),
            ("parent_misses", stats.parent_misses, i64),
            ("fec_set_root_misses", stats.fec_set_root_misses, i64),
            (
                "window_index_for_block_id_misses",
                stats.window_index_for_block_id_misses,
                i64
            ),
            (
                "ping_cache_check_failed",
                stats.ping_cache_check_failed,
                i64
            ),
            ("pings_sent", stats.pings_sent, i64),
            ("decode_time_us", stats.decode_time_us, i64),
            (
                "handle_requests_time_us",
                stats.handle_requests_time_us,
                i64
            ),
            ("err_time_skew", stats.err_time_skew, i64),
            ("err_malformed", stats.err_malformed, i64),
            ("err_sig_verify", stats.err_sig_verify, i64),
            ("err_unsigned", stats.err_unsigned, i64),
            ("err_id_mismatch", stats.err_id_mismatch, i64),
        );

        *stats = ServeRepairStats::default();
    }

    pub(crate) fn listen(
        mut self,
        requests_receiver: Receiver<PacketBatch>,
        response_sender: PacketBatchSender,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        const MAX_BYTES_PER_SECOND: u64 = 12_000_000;

        // rate limit delay should be greater than the repair request iteration delay
        assert!(REPAIR_PING_CACHE_RATE_LIMIT_DELAY > Duration::from_millis(REPAIR_MS));

        let mut ping_cache = PingCache::new(
            &mut rand::rng(),
            Instant::now(),
            REPAIR_PING_CACHE_TTL,
            REPAIR_PING_CACHE_RATE_LIMIT_DELAY,
            REPAIR_PING_CACHE_CAPACITY,
        );

        let recycler = PacketBatchRecycler::default();
        Builder::new()
            .name("solRepairListen".to_string())
            .spawn(move || {
                let mut last_print = Instant::now();
                let mut stats = ServeRepairStats::default();
                let data_budget = TokenBucket::new(
                    MAX_BYTES_PER_SECOND,
                    MAX_BYTES_PER_SECOND,
                    MAX_BYTES_PER_SECOND as f64,
                );
                while !exit.load(Ordering::Relaxed) {
                    let result = self.run_listen(
                        &mut ping_cache,
                        &recycler,
                        &requests_receiver,
                        &response_sender,
                        &mut stats,
                        &data_budget,
                    );
                    match result {
                        Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            info!("repair listener disconnected");
                            return;
                        }
                    };

                    const REPORT_INTERVAL: Duration = Duration::from_secs(2);
                    if last_print.elapsed() > REPORT_INTERVAL {
                        self.report_reset_stats(&mut stats);
                        last_print = Instant::now();
                    }
                }
            })
            .unwrap()
    }

    fn verify_signed_packet(my_id: &Pubkey, bytes: &[u8], request: &RepairProtocol) -> Result<()> {
        match request {
            RepairProtocol::LegacyWindowIndex
            | RepairProtocol::LegacyHighestWindowIndex
            | RepairProtocol::LegacyOrphan
            | RepairProtocol::LegacyWindowIndexWithNonce
            | RepairProtocol::LegacyHighestWindowIndexWithNonce
            | RepairProtocol::LegacyOrphanWithNonce
            | RepairProtocol::LegacyAncestorHashes => {
                return Err(Error::from(RepairVerifyError::Unsigned));
            }
            RepairProtocol::Pong(pong) => {
                if !pong.verify() {
                    return Err(Error::from(RepairVerifyError::SigVerify));
                }
            }
            RepairProtocol::WindowIndex { header, .. }
            | RepairProtocol::HighestWindowIndex { header, .. }
            | RepairProtocol::Orphan { header, .. }
            | RepairProtocol::AncestorHashes { header, .. }
            | RepairProtocol::ParentAndFecSetCount { header, .. }
            | RepairProtocol::FecSetRoot { header, .. }
            | RepairProtocol::WindowIndexForBlockId { header, .. } => {
                if &header.recipient != my_id {
                    return Err(Error::from(RepairVerifyError::IdMismatch));
                }
                let time_diff_ms = timestamp().abs_diff(header.timestamp);
                if u128::from(time_diff_ms) > SIGNED_REPAIR_TIME_WINDOW.as_millis() {
                    return Err(Error::from(RepairVerifyError::TimeSkew));
                }
                let Some(leading_buf) = bytes.get(..4) else {
                    debug_assert!(
                        false,
                        "request should have failed deserialization: {request:?}",
                    );
                    return Err(Error::from(RepairVerifyError::Malformed));
                };
                let Some(trailing_buf) = bytes.get(4 + SIGNATURE_BYTES..) else {
                    debug_assert!(
                        false,
                        "request should have failed deserialization: {request:?}",
                    );
                    return Err(Error::from(RepairVerifyError::Malformed));
                };
                let Some(from_id) = request.sender() else {
                    return Err(Error::from(RepairVerifyError::SigVerify));
                };
                let signed_data = [leading_buf, trailing_buf].concat();
                if !header.signature.verify(from_id.as_ref(), &signed_data) {
                    return Err(Error::from(RepairVerifyError::SigVerify));
                }
            }
        }
        Ok(())
    }

    fn check_ping_cache(
        ping_cache: &mut PingCache,
        request: &RepairProtocol,
        from_addr: &SocketAddr,
        identity_keypair: &Keypair,
    ) -> (bool, Option<Packet>) {
        let mut rng = rand::rng();
        let (check, ping) = request
            .sender()
            .map(|&sender| {
                ping_cache.check(
                    &mut rng,
                    identity_keypair,
                    Instant::now(),
                    (sender, *from_addr),
                )
            })
            .unwrap_or_default();
        let ping_pkt = if let Some(ping) = ping {
            match request {
                RepairProtocol::WindowIndex { .. }
                | RepairProtocol::HighestWindowIndex { .. }
                | RepairProtocol::Orphan { .. }
                | RepairProtocol::WindowIndexForBlockId { .. } => {
                    let ping = RepairResponse::Ping(ping);
                    Packet::from_data(Some(from_addr), ping).ok()
                }
                RepairProtocol::ParentAndFecSetCount { .. } | RepairProtocol::FecSetRoot { .. } => {
                    let ping = BlockIdRepairResponse::Ping { ping };
                    Packet::from_data(Some(from_addr), ping).ok()
                }
                RepairProtocol::AncestorHashes { .. } => {
                    let ping = AncestorHashesResponse::Ping(ping);
                    Packet::from_data(Some(from_addr), ping).ok()
                }
                RepairProtocol::Pong(_) => None,
                RepairProtocol::LegacyWindowIndex
                | RepairProtocol::LegacyHighestWindowIndex
                | RepairProtocol::LegacyOrphan
                | RepairProtocol::LegacyWindowIndexWithNonce
                | RepairProtocol::LegacyHighestWindowIndexWithNonce
                | RepairProtocol::LegacyOrphanWithNonce
                | RepairProtocol::LegacyAncestorHashes => {
                    error!("Unexpected legacy request: {request:?}");
                    debug_assert!(
                        false,
                        "Legacy requests should have been filtered out during signature \
                         verification. {request:?}"
                    );
                    None
                }
            }
        } else {
            None
        };
        (check, ping_pkt)
    }

    fn handle_requests(
        &self,
        ping_cache: &mut PingCache,
        recycler: &PacketBatchRecycler,
        requests: Vec<RepairRequestWithMeta>,
        packet_batch_sender: &PacketBatchSender,
        stats: &mut ServeRepairStats,
        data_budget: &TokenBucket,
        byte_cost_multiplier: usize,
    ) {
        let identity_keypair = self.cluster_info.keypair();
        let mut pending_pings = Vec::default();

        for RepairRequestWithMeta {
            request,
            from_addr,
            stake,
            whitelisted: _,
        } in requests.into_iter()
        {
            // we deliberately consume early assuming that request succeeds,
            // if it does we will refund the unused tokens
            let max_response_cost = request.max_response_bytes() * byte_cost_multiplier;
            if data_budget
                .consume_tokens(max_response_cost as u64)
                .is_err()
            {
                stats.dropped_requests_outbound_bandwidth += 1;
                continue;
            }
            if !matches!(&request, RepairProtocol::Pong(_)) {
                let (check, ping_pkt) =
                    Self::check_ping_cache(ping_cache, &request, &from_addr, &identity_keypair);
                if let Some(ping_pkt) = ping_pkt {
                    pending_pings.push(ping_pkt);
                }
                if !check {
                    // return all borrowed tokens
                    data_budget.add_tokens(max_response_cost as u64);
                    stats.ping_cache_check_failed += 1;
                    continue;
                }
            }
            stats.processed += 1;
            let Some(rsp) = self.handle_repair(recycler, &from_addr, request, stats, ping_cache)
            else {
                data_budget.add_tokens(max_response_cost as u64);
                continue;
            };
            let num_response_packets = rsp.len();
            let num_response_bytes: usize = rsp.iter().map(|p| p.meta().size).sum();
            // refund unused tokens if we can only serve the request partially
            let actually_used_cost = num_response_bytes * byte_cost_multiplier;
            debug_assert!(max_response_cost >= actually_used_cost);
            // We try to refund all tokens we have not used to actually serve request here.
            // We can theoretically still drop responses in outbound channel, but such drops
            // are unlikely unless we are severely overloaded, and thus not accounted for here.
            data_budget.add_tokens(max_response_cost.saturating_sub(actually_used_cost) as u64);

            // send the responses to the socket
            if packet_batch_sender.try_send(rsp).is_ok() {
                stats.total_response_packets += num_response_packets;
                match stake > 0 {
                    true => stats.total_response_bytes_staked += num_response_bytes,
                    false => stats.total_response_bytes_unstaked += num_response_bytes,
                }
            } else {
                stats.dropped_requests_outbound_bandwidth += 1;
                stats.total_dropped_response_packets += num_response_packets;
            }
        }

        if !pending_pings.is_empty() {
            let num_pings_to_send = pending_pings.len();
            let batch = RecycledPacketBatch::new(pending_pings);
            if packet_batch_sender.try_send(batch.into()).is_ok() {
                stats.pings_sent += num_pings_to_send;
            } else {
                stats.total_dropped_response_packets += num_pings_to_send;
            }
        }
    }

    pub fn ancestor_repair_request_bytes(
        &self,
        keypair: &Keypair,
        repair_peer_id: &Pubkey,
        request_slot: Slot,
        nonce: Nonce,
    ) -> Result<Vec<u8>> {
        let header = RepairRequestHeader {
            signature: Signature::default(),
            sender: keypair.pubkey(),
            recipient: *repair_peer_id,
            timestamp: timestamp(),
            nonce,
        };
        let request = RepairProtocol::AncestorHashes {
            header,
            slot: request_slot,
        };
        Self::repair_proto_to_bytes(&request, keypair)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn repair_request(
        &self,
        cluster_slots: &ClusterSlots,
        repair_request: ShredRepairType,
        peers_cache: &mut LruCache<Slot, RepairPeers>,
        repair_stats: &mut RepairStats,
        repair_validators: &Option<HashSet<Pubkey>>,
        outstanding_requests: &mut OutstandingShredRepairs,
        identity_keypair: &Keypair,
    ) -> Result<Option<(SocketAddr, Vec<u8>)>> {
        // find a peer that appears to be accepting replication and has the desired slot, as indicated
        // by a valid tvu port location
        let slot = repair_request.slot();
        let repair_peers = match peers_cache.get(&slot) {
            Some(entry) if entry.asof.elapsed() < REPAIR_PEERS_CACHE_TTL => entry,
            _ => {
                peers_cache.pop(&slot);
                let repair_peers =
                    self.repair_peers(repair_validators, slot, &identity_keypair.pubkey());
                let weights = cluster_slots.compute_weights(slot, &repair_peers);
                let repair_peers = RepairPeers::new(Instant::now(), &repair_peers, &weights)?;
                peers_cache.put(slot, repair_peers);
                peers_cache.get(&slot).unwrap()
            }
        };
        let peer = repair_peers.sample(&mut rand::rng());
        let location = repair_request
            .block_id()
            // Eager repair uses the Original blockstore column,
            // however block id based repair shreds must be inserted in the
            // Alternate column
            .map_or(BlockLocation::Original, |block_id| {
                BlockLocation::Alternate { block_id }
            });
        let nonce = outstanding_requests.add_request_with_metadata(
            repair_request,
            timestamp(),
            Some(location),
        );
        let out = self.map_repair_request(
            &repair_request,
            &peer.pubkey,
            repair_stats,
            nonce,
            identity_keypair,
        )?;
        debug!(
            "Sending repair request from {} to {} for {:#?}",
            identity_keypair.pubkey(),
            peer.pubkey,
            repair_request
        );
        Ok(Some((peer.serve_repair, out)))
    }

    /// Similar to [`Self::repair_request`] but for [`BlockIdRepairType`] requests.
    /// Uses stake-weighted peer selection rather than cluster_slots weights.
    #[allow(dead_code)]
    pub(crate) fn block_id_repair_request(
        &self,
        repair_validators: &Option<HashSet<Pubkey>>,
        repair_request: BlockIdRepairType,
        peers_cache: &mut LruCache<Slot, RepairPeers>,
        outstanding_requests: &mut OutstandingRequests<BlockIdRepairType>,
        identity_keypair: &Keypair,
        staked_nodes: &HashMap<Pubkey, u64>,
    ) -> Result<(Vec<u8>, SocketAddr)> {
        let slot = repair_request.slot();
        let repair_peers = match peers_cache.get(&slot) {
            Some(entry) if entry.asof.elapsed() < REPAIR_PEERS_CACHE_TTL => entry,
            _ => {
                peers_cache.pop(&slot);
                let repair_peers =
                    self.repair_peers(repair_validators, slot, &identity_keypair.pubkey());
                let weights: Vec<u64> = repair_peers
                    .iter()
                    .map(|peer| staked_nodes.get(peer.pubkey()).copied().unwrap_or(0))
                    .collect();
                let repair_peers = RepairPeers::new(Instant::now(), &repair_peers, &weights)?;
                peers_cache.put(slot, repair_peers);
                peers_cache.get(&slot).unwrap()
            }
        };
        let peer = repair_peers.sample(&mut rand::rng());
        let nonce = outstanding_requests.add_request(repair_request, timestamp());

        let out = self.map_block_id_repair_request(
            &repair_request,
            &peer.pubkey,
            nonce,
            identity_keypair,
        )?;
        debug!(
            "Sending block_id repair request from {} to {} for {:#?}",
            identity_keypair.pubkey(),
            peer.pubkey,
            repair_request
        );
        Ok((out, peer.serve_repair))
    }

    pub(crate) fn repair_request_ancestor_hashes_sample_peers(
        &self,
        slot: Slot,
        cluster_slots: &ClusterSlots,
        repair_validators: &Option<HashSet<Pubkey>>,
        repair_protocol: Protocol,
        my_pubkey: &Pubkey,
    ) -> Result<Vec<(Pubkey, SocketAddr)>> {
        let repair_peers: Vec<_> = self.repair_peers(repair_validators, slot, my_pubkey);
        if repair_peers.is_empty() {
            return Err(ClusterInfoError::NoPeers.into());
        }
        let (weights, index) = cluster_slots.compute_weights_exclude_nonfrozen(slot, &repair_peers);
        let peers = WeightedShuffle::new("repair_request_ancestor_hashes", weights)
            .shuffle(&mut rand::rng())
            .map(|i| index[i])
            .filter_map(|i| {
                let addr = repair_peers[i].serve_repair(repair_protocol)?;
                Some((*repair_peers[i].pubkey(), addr))
            })
            .take(get_ancestor_hash_repair_sample_size())
            .collect();
        Ok(peers)
    }

    #[cfg(test)]
    pub(crate) fn repair_request_duplicate_compute_best_peer(
        &self,
        slot: Slot,
        cluster_slots: &ClusterSlots,
        repair_validators: &Option<HashSet<Pubkey>>,
        my_pubkey: &Pubkey,
    ) -> Option<(Pubkey, SocketAddr)> {
        let repair_peers: Vec<_> = self.repair_peers(repair_validators, slot, my_pubkey);
        if repair_peers.is_empty() {
            return None;
        }
        let (weights, index) = cluster_slots.compute_weights_exclude_nonfrozen(slot, &repair_peers);
        let k = WeightedIndex::new(weights).ok()?.sample(&mut rand::rng());
        let n = index[k];
        Some((
            *repair_peers[n].pubkey(),
            repair_peers[n].serve_repair(Protocol::UDP)?,
        ))
    }

    pub(crate) fn map_repair_request(
        &self,
        repair_request: &ShredRepairType,
        repair_peer_id: &Pubkey,
        repair_stats: &mut RepairStats,
        nonce: Nonce,
        identity_keypair: &Keypair,
    ) -> Result<Vec<u8>> {
        let header = RepairRequestHeader {
            signature: Signature::default(),
            sender: identity_keypair.pubkey(),
            recipient: *repair_peer_id,
            timestamp: timestamp(),
            nonce,
        };
        let request_proto = match repair_request {
            ShredRepairType::Shred(slot, shred_index) => {
                repair_stats
                    .shred
                    .update(repair_peer_id, *slot, *shred_index);
                RepairProtocol::WindowIndex {
                    header,
                    slot: *slot,
                    shred_index: *shred_index,
                }
            }
            ShredRepairType::HighestShred(slot, shred_index) => {
                repair_stats
                    .highest_shred
                    .update(repair_peer_id, *slot, *shred_index);
                RepairProtocol::HighestWindowIndex {
                    header,
                    slot: *slot,
                    shred_index: *shred_index,
                }
            }
            ShredRepairType::Orphan(slot) => {
                repair_stats.orphan.update(repair_peer_id, *slot, 0);
                RepairProtocol::Orphan {
                    header,
                    slot: *slot,
                }
            }
            ShredRepairType::ShredForBlockId {
                slot,
                index,
                fec_set_merkle_root,
                block_id,
            } => RepairProtocol::WindowIndexForBlockId {
                header,
                slot: *slot,
                shred_index: *index,
                fec_set_merkle_root: *fec_set_merkle_root,
                block_id: *block_id,
            },
        };
        Self::repair_proto_to_bytes(&request_proto, identity_keypair)
    }

    /// Transforms a [`BlockIdRepairType`] into a signed repair protocol message.
    #[allow(dead_code)]
    pub(crate) fn map_block_id_repair_request(
        &self,
        repair_request: &BlockIdRepairType,
        repair_peer_id: &Pubkey,
        nonce: Nonce,
        identity_keypair: &Keypair,
    ) -> Result<Vec<u8>> {
        let header = RepairRequestHeader {
            signature: Signature::default(),
            sender: identity_keypair.pubkey(),
            recipient: *repair_peer_id,
            timestamp: timestamp(),
            nonce,
        };
        let request_proto = match repair_request {
            BlockIdRepairType::ParentAndFecSetCount { slot, block_id } => {
                RepairProtocol::ParentAndFecSetCount {
                    header,
                    slot: *slot,
                    block_id: *block_id,
                }
            }
            BlockIdRepairType::FecSetRoot {
                slot,
                block_id,
                fec_set_index,
            } => RepairProtocol::FecSetRoot {
                header,
                slot: *slot,
                block_id: *block_id,
                fec_set_index: *fec_set_index,
            },
        };
        Self::repair_proto_to_bytes(&request_proto, identity_keypair)
    }

    /// Distinguish and process `RepairResponse` ping packets ignoring other
    /// packets in the batch.
    pub(crate) fn handle_repair_response_pings(
        repair_socket: &UdpSocket,
        keypair: &Keypair,
        packet_batch: &mut PacketBatch,
        stats: &mut ShredFetchStats,
    ) {
        let mut pending_pongs = Vec::default();
        for mut packet in packet_batch.iter_mut() {
            if packet.meta().size != REPAIR_RESPONSE_SERIALIZED_PING_BYTES {
                continue;
            }
            if let Ok(RepairResponse::Ping(ping)) = packet.deserialize_slice(..) {
                if !ping.verify() {
                    // Do _not_ set `discard` to allow shred processing to attempt to
                    // handle the packet.
                    // Ping error count may include false posities for shreds of size
                    // `REPAIR_RESPONSE_SERIALIZED_PING_BYTES` whose first 4 bytes
                    // match `RepairResponse` discriminator (these 4 bytes overlap
                    // with the shred signature field).
                    stats.ping_err_verify_count += 1;
                    continue;
                }
                packet.meta_mut().set_discard(true);
                stats.ping_count += 1;
                let pong = RepairProtocol::Pong(Pong::new(&ping, keypair));
                if let Ok(pong) = bincode::serialize(&pong) {
                    let from_addr = packet.meta().socket_addr();
                    pending_pongs.push((pong, from_addr));
                }
            }
        }
        if !pending_pongs.is_empty() {
            let num_pkts = pending_pongs.len();
            let pending_pongs = pending_pongs.iter().map(|(bytes, addr)| (bytes, addr));
            match batch_send(repair_socket, pending_pongs) {
                Ok(()) => (),
                Err(SendPktsError::IoError(err, num_failed)) => {
                    warn!(
                        "batch_send failed to send {num_failed}/{num_pkts} packets. First error: \
                         {err:?}"
                    );
                }
            }
        }
    }

    pub fn repair_proto_to_bytes(request: &RepairProtocol, keypair: &Keypair) -> Result<Vec<u8>> {
        debug_assert!(request.supports_signature());
        let mut payload = serialize(&request)?;
        let signable_data = [&payload[..4], &payload[4 + SIGNATURE_BYTES..]].concat();
        let signature = keypair.sign_message(&signable_data[..]);
        payload[4..4 + SIGNATURE_BYTES].copy_from_slice(signature.as_ref());
        Ok(payload)
    }

    fn repair_peers(
        &self,
        repair_validators: &Option<HashSet<Pubkey>>,
        slot: Slot,
        my_pubkey: &Pubkey,
    ) -> Vec<ContactInfo> {
        if let Some(repair_validators) = repair_validators {
            repair_validators
                .iter()
                .filter_map(|key| {
                    if key != my_pubkey {
                        self.cluster_info.lookup_contact_info(key, |ci| ci.clone())
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            self.cluster_info.repair_peers(slot)
        }
    }
}

pub(crate) fn deserialize_request<T>(
    request: &BytesPacket,
) -> std::result::Result<T, bincode::Error>
where
    T: serde::de::DeserializeOwned,
{
    bincode::options()
        .with_limit(request.buffer().len() as u64)
        .with_fixint_encoding()
        .reject_trailing_bytes()
        .deserialize(request.buffer())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::repair::repair_response,
        agave_feature_set::FeatureSet,
        solana_gossip::{contact_info::ContactInfo, socketaddr, socketaddr_any},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::{Blockstore, make_many_slot_entries},
            blockstore_processor::fill_blockstore_slot_with_ticks,
            genesis_utils::{GenesisConfigInfo, create_genesis_config},
            get_tmp_ledger_path_auto_delete,
            shred::{
                ProcessShredsStats, ReedSolomonCache, Shred, Shredder, max_ticks_per_n_shreds,
            },
        },
        solana_net_utils::SocketAddrSpace,
        solana_perf::packet::{Packet, PacketFlags, PacketRef, deserialize_from_with_limit},
        solana_pubkey::Pubkey,
        solana_runtime::bank::Bank,
        solana_time_utils::timestamp,
        std::{io::Cursor, net::Ipv4Addr},
    };

    fn discard_malformed_repair_requests(
        requests: &mut Vec<BytesPacket>,
        stats: &mut ServeRepairStats,
    ) -> usize {
        requests.retain(|request| is_well_formed_repair_request(&PacketRef::from(request), stats));
        requests.len()
    }

    #[test]
    fn test_serialized_ping_size() {
        let mut rng = rand::rng();
        let keypair = Keypair::new();
        let ping = Ping::new(rng.random(), &keypair);
        let ping = RepairResponse::Ping(ping);
        let pkt = Packet::from_data(None, ping).unwrap();
        assert_eq!(pkt.meta().size, REPAIR_RESPONSE_SERIALIZED_PING_BYTES);
    }

    #[test]
    fn test_deserialize_shred_as_ping() {
        let keypair = Keypair::new();
        let shred = Shredder::single_shred_for_tests(123, &keypair);
        let mut pkt = Packet::default();
        shred.copy_to_packet(&mut pkt);
        pkt.meta_mut().size = REPAIR_RESPONSE_SERIALIZED_PING_BYTES;
        let res = pkt.deserialize_slice::<RepairResponse, _>(..);
        if let Ok(RepairResponse::Ping(ping)) = res {
            assert!(!ping.verify());
        } else {
            assert!(res.is_err());
        }
    }

    fn repair_request_header_for_tests() -> RepairRequestHeader {
        RepairRequestHeader {
            signature: Signature::default(),
            sender: Pubkey::default(),
            recipient: Pubkey::default(),
            timestamp: timestamp(),
            nonce: Nonce::default(),
        }
    }

    fn make_remote_request(packet: &Packet) -> BytesPacket {
        PacketRef::from(packet).to_bytes_packet()
    }

    #[test]
    fn test_check_well_formed_repair_request() {
        let mut rng = rand::rng();
        let keypair = Keypair::new();
        let ping = Ping::new(rng.random(), &keypair);
        let pong = Pong::new(&ping, &keypair);
        let request = RepairProtocol::Pong(pong);
        let mut pkt = Packet::from_data(None, request).unwrap();
        let mut batch = vec![make_remote_request(&pkt)];
        let mut stats = ServeRepairStats::default();
        let num_well_formed = discard_malformed_repair_requests(&mut batch, &mut stats);
        assert_eq!(num_well_formed, 1);
        pkt.meta_mut().size = 5;
        let mut batch = vec![make_remote_request(&pkt)];
        let mut stats = ServeRepairStats::default();
        let num_well_formed = discard_malformed_repair_requests(&mut batch, &mut stats);
        assert_eq!(num_well_formed, 0);
        assert_eq!(stats.err_malformed, 1);

        let request = RepairProtocol::WindowIndex {
            header: repair_request_header_for_tests(),
            slot: 123,
            shred_index: 456,
        };
        let mut pkt = Packet::from_data(None, request).unwrap();
        let mut batch = vec![make_remote_request(&pkt)];
        let mut stats = ServeRepairStats::default();
        let num_well_formed = discard_malformed_repair_requests(&mut batch, &mut stats);
        assert_eq!(num_well_formed, 1);
        pkt.meta_mut().size = 8;
        let mut batch = vec![make_remote_request(&pkt)];
        let mut stats = ServeRepairStats::default();
        let num_well_formed = discard_malformed_repair_requests(&mut batch, &mut stats);
        assert_eq!(num_well_formed, 0);
        assert_eq!(stats.err_malformed, 1);

        let request = RepairProtocol::AncestorHashes {
            header: repair_request_header_for_tests(),
            slot: 123,
        };
        let mut pkt = Packet::from_data(None, request).unwrap();
        let mut batch = vec![make_remote_request(&pkt)];
        let mut stats = ServeRepairStats::default();
        let num_well_formed = discard_malformed_repair_requests(&mut batch, &mut stats);
        assert_eq!(num_well_formed, 1);
        pkt.meta_mut().size = 1;
        let mut batch = vec![make_remote_request(&pkt)];
        let mut stats = ServeRepairStats::default();
        let num_well_formed = discard_malformed_repair_requests(&mut batch, &mut stats);
        assert_eq!(num_well_formed, 0);
        assert_eq!(stats.err_malformed, 1);

        let request = RepairProtocol::Orphan {
            header: repair_request_header_for_tests(),
            slot: 262_547_696,
        };
        let mut pkt = Packet::from_data(None, request).unwrap();
        let mut batch = vec![make_remote_request(&pkt)];
        let mut stats = ServeRepairStats::default();
        let num_well_formed = discard_malformed_repair_requests(&mut batch, &mut stats);
        assert_eq!(num_well_formed, 1);
        pkt.meta_mut().size = 3;
        let mut batch = vec![make_remote_request(&pkt)];
        let mut stats = ServeRepairStats::default();
        let num_well_formed = discard_malformed_repair_requests(&mut batch, &mut stats);
        assert_eq!(num_well_formed, 0);
        assert_eq!(stats.err_malformed, 1);
    }

    #[test]
    fn test_serialize_deserialize_signed_request() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let cluster_info = Arc::new(new_test_cluster_info());
        let serve_repair = ServeRepair::new_for_test(
            cluster_info.clone(),
            bank_forks,
            Arc::new(RwLock::new(HashSet::default())),
        );
        let keypair = cluster_info.keypair();
        let repair_peer_id = solana_pubkey::new_rand();
        let repair_request = ShredRepairType::Orphan(123);

        let rsp = serve_repair
            .map_repair_request(
                &repair_request,
                &repair_peer_id,
                &mut RepairStats::default(),
                456,
                &keypair,
            )
            .unwrap();

        let mut cursor = Cursor::new(&rsp[..]);
        let deserialized_request: RepairProtocol =
            deserialize_from_with_limit(&mut cursor).unwrap();
        assert_eq!(cursor.position(), rsp.len() as u64);
        if let RepairProtocol::Orphan { header, slot } = deserialized_request {
            assert_eq!(slot, 123);
            assert_eq!(header.nonce, 456);
            assert_eq!(&header.sender, &serve_repair.my_id());
            assert_eq!(&header.recipient, &repair_peer_id);
            let signed_data = [&rsp[..4], &rsp[4 + SIGNATURE_BYTES..]].concat();
            assert!(
                header
                    .signature
                    .verify(keypair.pubkey().as_ref(), &signed_data)
            );
        } else {
            panic!("unexpected request type {:?}", &deserialized_request);
        }
    }

    #[test]
    fn test_serialize_deserialize_ancestor_hashes_request() {
        let slot: Slot = 50;
        let nonce = 70;
        let cluster_info = Arc::new(new_test_cluster_info());
        let repair_peer_id = solana_pubkey::new_rand();
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let keypair = cluster_info.keypair();

        let mut bank = Bank::new_for_tests(&genesis_config);
        bank.feature_set = Arc::new(FeatureSet::all_enabled());
        let bank_forks = BankForks::new_rw_arc(bank);
        let serve_repair = ServeRepair::new_for_test(
            cluster_info,
            bank_forks,
            Arc::new(RwLock::new(HashSet::default())),
        );

        let request_bytes = serve_repair
            .ancestor_repair_request_bytes(&keypair, &repair_peer_id, slot, nonce)
            .unwrap();
        let mut cursor = Cursor::new(&request_bytes[..]);
        let deserialized_request: RepairProtocol =
            deserialize_from_with_limit(&mut cursor).unwrap();
        assert_eq!(cursor.position(), request_bytes.len() as u64);
        if let RepairProtocol::AncestorHashes {
            header,
            slot: deserialized_slot,
        } = deserialized_request
        {
            assert_eq!(deserialized_slot, slot);
            assert_eq!(header.nonce, nonce);
            assert_eq!(&header.sender, &serve_repair.my_id());
            assert_eq!(&header.recipient, &repair_peer_id);
            let signed_data = [&request_bytes[..4], &request_bytes[4 + SIGNATURE_BYTES..]].concat();
            assert!(
                header
                    .signature
                    .verify(keypair.pubkey().as_ref(), &signed_data)
            );
        } else {
            panic!("unexpected request type {:?}", &deserialized_request);
        }
    }

    #[test]
    fn test_map_requests_signed() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let cluster_info = Arc::new(new_test_cluster_info());
        let serve_repair = ServeRepair::new_for_test(
            cluster_info.clone(),
            bank_forks,
            Arc::new(RwLock::new(HashSet::default())),
        );
        let keypair = cluster_info.keypair();
        let repair_peer_id = solana_pubkey::new_rand();

        let slot = 50;
        let shred_index = 60;
        let nonce = 70;

        let request = ShredRepairType::Shred(slot, shred_index);
        let request_bytes = serve_repair
            .map_repair_request(
                &request,
                &repair_peer_id,
                &mut RepairStats::default(),
                nonce,
                &keypair,
            )
            .unwrap();

        let mut cursor = Cursor::new(&request_bytes[..]);
        let deserialized_request: RepairProtocol =
            deserialize_from_with_limit(&mut cursor).unwrap();
        assert_eq!(cursor.position(), request_bytes.len() as u64);
        if let RepairProtocol::WindowIndex {
            header,
            slot: deserialized_slot,
            shred_index: deserialized_shred_index,
        } = deserialized_request
        {
            assert_eq!(deserialized_slot, slot);
            assert_eq!(deserialized_shred_index, shred_index);
            assert_eq!(header.nonce, nonce);
            assert_eq!(&header.sender, &serve_repair.my_id());
            assert_eq!(&header.recipient, &repair_peer_id);
            let signed_data = [&request_bytes[..4], &request_bytes[4 + SIGNATURE_BYTES..]].concat();
            assert!(
                header
                    .signature
                    .verify(keypair.pubkey().as_ref(), &signed_data)
            );
        } else {
            panic!("unexpected request type {:?}", &deserialized_request);
        }

        let request = ShredRepairType::HighestShred(slot, shred_index);
        let request_bytes = serve_repair
            .map_repair_request(
                &request,
                &repair_peer_id,
                &mut RepairStats::default(),
                nonce,
                &keypair,
            )
            .unwrap();

        let mut cursor = Cursor::new(&request_bytes[..]);
        let deserialized_request: RepairProtocol =
            deserialize_from_with_limit(&mut cursor).unwrap();
        assert_eq!(cursor.position(), request_bytes.len() as u64);
        if let RepairProtocol::HighestWindowIndex {
            header,
            slot: deserialized_slot,
            shred_index: deserialized_shred_index,
        } = deserialized_request
        {
            assert_eq!(deserialized_slot, slot);
            assert_eq!(deserialized_shred_index, shred_index);
            assert_eq!(header.nonce, nonce);
            assert_eq!(&header.sender, &serve_repair.my_id());
            assert_eq!(&header.recipient, &repair_peer_id);
            let signed_data = [&request_bytes[..4], &request_bytes[4 + SIGNATURE_BYTES..]].concat();
            assert!(
                header
                    .signature
                    .verify(keypair.pubkey().as_ref(), &signed_data)
            );
        } else {
            panic!("unexpected request type {:?}", &deserialized_request);
        }
    }

    #[test]
    fn test_verify_signed_packet() {
        let my_keypair = Keypair::new();
        let other_keypair = Keypair::new();

        fn sign_packet(packet: &mut Packet, keypair: &Keypair) {
            let signable_data = [
                packet.data(..4).unwrap(),
                packet.data(4 + SIGNATURE_BYTES..).unwrap(),
            ]
            .concat();
            let signature = keypair.sign_message(&signable_data[..]);
            packet.buffer_mut()[4..4 + SIGNATURE_BYTES].copy_from_slice(signature.as_ref());
        }

        // well formed packet
        let packet = {
            let header = RepairRequestHeader::new(
                my_keypair.pubkey(),
                other_keypair.pubkey(),
                timestamp(),
                678,
            );
            let slot = 239847;
            let request = RepairProtocol::Orphan { header, slot };
            let mut packet = Packet::from_data(None, request).unwrap();
            sign_packet(&mut packet, &my_keypair);
            packet
        };
        let request: RepairProtocol = packet.deserialize_slice(..).unwrap();
        assert_matches!(
            ServeRepair::verify_signed_packet(
                &other_keypair.pubkey(),
                packet.data(..).unwrap(),
                &request
            ),
            Ok(())
        );

        // recipient mismatch
        let packet = {
            let header = RepairRequestHeader::new(
                my_keypair.pubkey(),
                other_keypair.pubkey(),
                timestamp(),
                678,
            );
            let slot = 239847;
            let request = RepairProtocol::Orphan { header, slot };
            let mut packet = Packet::from_data(None, request).unwrap();
            sign_packet(&mut packet, &my_keypair);
            packet
        };
        let request: RepairProtocol = packet.deserialize_slice(..).unwrap();
        assert_matches!(
            ServeRepair::verify_signed_packet(
                &my_keypair.pubkey(),
                packet.data(..).unwrap(),
                &request
            ),
            Err(Error::RepairVerify(RepairVerifyError::IdMismatch))
        );

        // outside time window
        let packet = {
            let time_diff_ms = u64::try_from(SIGNED_REPAIR_TIME_WINDOW.as_millis() * 2).unwrap();
            let old_timestamp = timestamp().saturating_sub(time_diff_ms);
            let header = RepairRequestHeader::new(
                my_keypair.pubkey(),
                other_keypair.pubkey(),
                old_timestamp,
                678,
            );
            let slot = 239847;
            let request = RepairProtocol::Orphan { header, slot };
            let mut packet = Packet::from_data(None, request).unwrap();
            sign_packet(&mut packet, &my_keypair);
            packet
        };
        let request: RepairProtocol = packet.deserialize_slice(..).unwrap();
        assert_matches!(
            ServeRepair::verify_signed_packet(
                &other_keypair.pubkey(),
                packet.data(..).unwrap(),
                &request
            ),
            Err(Error::RepairVerify(RepairVerifyError::TimeSkew))
        );

        // bad signature
        let packet = {
            let header = RepairRequestHeader::new(
                my_keypair.pubkey(),
                other_keypair.pubkey(),
                timestamp(),
                678,
            );
            let slot = 239847;
            let request = RepairProtocol::Orphan { header, slot };
            let mut packet = Packet::from_data(None, request).unwrap();
            sign_packet(&mut packet, &other_keypair);
            packet
        };
        let request: RepairProtocol = packet.deserialize_slice(..).unwrap();
        assert_matches!(
            ServeRepair::verify_signed_packet(
                &other_keypair.pubkey(),
                packet.data(..).unwrap(),
                &request
            ),
            Err(Error::RepairVerify(RepairVerifyError::SigVerify))
        );
    }

    #[test]
    fn test_run_highest_window_request() {
        run_highest_window_request(5, 3, 9);
    }

    /// test run_window_request responds with the right shred, and do not overrun
    pub fn run_highest_window_request(slot: Slot, num_slots: u64, nonce: Nonce) {
        let recycler = PacketBatchRecycler::default();
        agave_logger::setup();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let handler = StandardRepairHandler::new(blockstore.clone());
        let rv = handler.run_highest_window_request(&recycler, &socketaddr_any!(), 0, 0, nonce);
        assert!(rv.is_none());

        let _ = fill_blockstore_slot_with_ticks(
            &blockstore,
            max_ticks_per_n_shreds(1, None) + 1,
            slot,
            slot - num_slots + 1,
            Hash::default(),
        );

        let index = 1;
        let mut rv = handler
            .run_highest_window_request(&recycler, &socketaddr_any!(), slot, index, nonce)
            .expect("packets");
        let request = ShredRepairType::HighestShred(slot, index);
        verify_responses(&request, rv.iter());

        let rv: Vec<Shred> = rv
            .iter_mut()
            .map(|mut packet| {
                packet.meta_mut().flags |= PacketFlags::REPAIR;
                let (shred, repair_nonce) =
                    shred::layout::get_shred_and_repair_nonce(packet.as_ref()).unwrap();
                assert_eq!(repair_nonce.unwrap(), nonce);
                Shred::new_from_serialized_shred(shred.to_vec()).unwrap()
            })
            .collect();
        assert!(!rv.is_empty());
        let index = blockstore.meta(slot).unwrap().unwrap().received - 1;
        assert_eq!(rv[0].index(), index as u32);
        assert_eq!(rv[0].slot(), slot);

        let rv = handler.run_highest_window_request(
            &recycler,
            &socketaddr_any!(),
            slot,
            index + 1,
            nonce,
        );
        assert!(rv.is_none());
    }

    #[test]
    /// test window requests respond with the right shred, and do not overrun
    fn test_run_window_request() {
        let slot = 2;
        let nonce = 9;
        let recycler = PacketBatchRecycler::default();
        agave_logger::setup();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let handler = StandardRepairHandler::new(blockstore.clone());
        let rv = handler.run_window_request(&recycler, &socketaddr_any!(), slot, 0, nonce);
        assert!(rv.is_none());
        let shredder = Shredder::new(slot, slot - 1, 0, 2).unwrap();
        let keypair = Keypair::new();
        let reed_solomon_cache = ReedSolomonCache::default();
        let index = 1;
        let (mut shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
            &keypair,
            &[],
            true,
            Hash::default(),
            index as u32,
            index as u32,
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        );
        shreds.truncate(1);

        blockstore
            .insert_shreds(shreds, None, false)
            .expect("Expect successful ledger write");

        let mut rv = handler
            .run_window_request(&recycler, &socketaddr_any!(), slot, index, nonce)
            .expect("packets");
        let request = ShredRepairType::Shred(slot, index);
        verify_responses(&request, rv.iter());
        let rv: Vec<Shred> = rv
            .iter_mut()
            .map(|mut packet| {
                packet.meta_mut().flags |= PacketFlags::REPAIR;
                let (shred, repair_nonce) =
                    shred::layout::get_shred_and_repair_nonce(packet.as_ref()).unwrap();
                assert_eq!(repair_nonce.unwrap(), nonce);
                Shred::new_from_serialized_shred(shred.to_vec()).unwrap()
            })
            .collect();
        assert_eq!(rv[0].index(), 1);
        assert_eq!(rv[0].slot(), slot);
    }

    fn new_test_cluster_info() -> ClusterInfo {
        let keypair = Arc::new(Keypair::new());
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), timestamp());
        ClusterInfo::new(contact_info, keypair, SocketAddrSpace::Unspecified)
    }

    #[test]
    fn window_index_request() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let cluster_slots = ClusterSlots::default_for_tests();
        let cluster_info = Arc::new(new_test_cluster_info());
        let serve_repair = ServeRepair::new_for_test(
            cluster_info.clone(),
            bank_forks,
            Arc::new(RwLock::new(HashSet::default())),
        );
        let identity_keypair = cluster_info.keypair();
        let mut outstanding_requests = OutstandingShredRepairs::default();
        let rv = serve_repair.repair_request(
            &cluster_slots,
            ShredRepairType::Shred(0, 0),
            &mut LruCache::new(100),
            &mut RepairStats::default(),
            &None,
            &mut outstanding_requests,
            &identity_keypair,
        );
        assert_matches!(rv, Err(Error::ClusterInfo(ClusterInfoError::NoPeers)));

        let serve_repair_addr = socketaddr!(Ipv4Addr::LOCALHOST, 1243);
        let mut nxt = ContactInfo::new(
            solana_pubkey::new_rand(),
            timestamp(), // wallclock
            0u16,        // shred_version
        );
        nxt.set_gossip((Ipv4Addr::LOCALHOST, 1234)).unwrap();
        nxt.set_tvu(Protocol::UDP, (Ipv4Addr::LOCALHOST, 1235))
            .unwrap();
        nxt.set_rpc((Ipv4Addr::LOCALHOST, 1241)).unwrap();
        nxt.set_rpc_pubsub((Ipv4Addr::LOCALHOST, 1242)).unwrap();
        nxt.set_serve_repair(Protocol::UDP, serve_repair_addr)
            .unwrap();
        cluster_info.insert_info(nxt.clone());
        let rv = serve_repair
            .repair_request(
                &cluster_slots,
                ShredRepairType::Shred(0, 0),
                &mut LruCache::new(100),
                &mut RepairStats::default(),
                &None,
                &mut outstanding_requests,
                &identity_keypair,
            )
            .unwrap()
            .unwrap();
        assert_eq!(nxt.serve_repair(Protocol::UDP).unwrap(), serve_repair_addr);
        assert_eq!(rv.0, nxt.serve_repair(Protocol::UDP).unwrap());

        let serve_repair_addr2 = socketaddr!([127, 0, 0, 2], 1243);
        let mut nxt = ContactInfo::new(
            solana_pubkey::new_rand(),
            timestamp(), // wallclock
            0u16,        // shred_version
        );
        nxt.set_gossip((Ipv4Addr::LOCALHOST, 1234)).unwrap();
        nxt.set_tvu(Protocol::UDP, (Ipv4Addr::LOCALHOST, 1235))
            .unwrap();
        nxt.set_rpc((Ipv4Addr::LOCALHOST, 1241)).unwrap();
        nxt.set_rpc_pubsub((Ipv4Addr::LOCALHOST, 1242)).unwrap();
        nxt.set_serve_repair(Protocol::UDP, serve_repair_addr2)
            .unwrap();
        cluster_info.insert_info(nxt);
        let mut one = false;
        let mut two = false;
        while !one || !two {
            //this randomly picks an option, so eventually it should pick both
            let rv = serve_repair
                .repair_request(
                    &cluster_slots,
                    ShredRepairType::Shred(0, 0),
                    &mut LruCache::new(100),
                    &mut RepairStats::default(),
                    &None,
                    &mut outstanding_requests,
                    &identity_keypair,
                )
                .unwrap()
                .unwrap();
            if rv.0 == serve_repair_addr {
                one = true;
            }
            if rv.0 == serve_repair_addr2 {
                two = true;
            }
        }
        assert!(one && two);
    }

    #[test]
    fn test_run_orphan() {
        run_orphan(2, 3, 9);
    }

    pub fn run_orphan(slot: Slot, num_slots: u64, nonce: Nonce) {
        agave_logger::setup();
        let recycler = PacketBatchRecycler::default();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let handler = StandardRepairHandler::new(blockstore.clone());
        let rv = handler.run_orphan(&recycler, &socketaddr_any!(), slot, 5, nonce);
        assert!(rv.is_none());

        // Create slots [slot, slot + num_slots) with 5 shreds apiece
        let (shreds, _) = make_many_slot_entries(slot, num_slots, 5);

        blockstore
            .insert_shreds(shreds, None, false)
            .expect("Expect successful ledger write");

        // We don't have slot `slot + num_slots`, so we don't know how to service this request
        let rv = handler.run_orphan(&recycler, &socketaddr_any!(), slot + num_slots, 5, nonce);
        assert!(rv.is_none());

        // For a orphan request for `slot + num_slots - 1`, we should return the highest shreds
        // from slots in the range [slot, slot + num_slots - 1]
        let rv = handler
            .run_orphan(
                &recycler,
                &socketaddr_any!(),
                slot + num_slots - 1,
                5,
                nonce,
            )
            .expect("run_orphan packets");

        // Verify responses
        let request = ShredRepairType::Orphan(slot + num_slots - 1);
        verify_responses(&request, rv.iter());

        let expected: Vec<_> = (slot..slot + num_slots)
            .rev()
            .filter_map(|slot| {
                let index = blockstore.meta(slot).unwrap().unwrap().received - 1;
                repair_response::repair_response_packet(
                    &blockstore,
                    slot,
                    index,
                    &socketaddr_any!(),
                    nonce,
                )
            })
            .collect();
        let expected = PacketBatch::Pinned(RecycledPacketBatch::new(expected));
        assert_eq!(rv, expected);
    }

    #[test]
    fn run_orphan_corrupted_shred_size() {
        agave_logger::setup();
        let recycler = PacketBatchRecycler::default();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        // Create slots [1, 2] with 1 shred apiece
        let (mut shreds, _) = make_many_slot_entries(1, 2, 1);

        assert_eq!(shreds[0].slot(), 1);
        assert_eq!(shreds[0].index(), 0);
        // TODO: The test previously relied on corrupting shred payload
        // size which we no longer want to expose. Current test no longer
        // covers packet size check in repair_response_packet_from_bytes.
        shreds.retain(|shred| shred.slot() != 1);
        blockstore
            .insert_shreds(shreds, None, false)
            .expect("Expect successful ledger write");
        let nonce = 42;
        // Make sure repair response is corrupted
        assert!(
            repair_response::repair_response_packet(&blockstore, 1, 0, &socketaddr_any!(), nonce,)
                .is_none()
        );

        // Orphan request for slot 2 should only return slot 1 since
        // calling `repair_response_packet` on slot 1's shred will
        // be corrupted
        let handler = StandardRepairHandler::new(blockstore.clone());
        let rv = handler
            .run_orphan(&recycler, &socketaddr_any!(), 2, 5, nonce)
            .expect("run_orphan packets");

        // Verify responses
        let expected = RecycledPacketBatch::new(vec![
            repair_response::repair_response_packet(
                &blockstore,
                2,
                31, // shred_index
                &socketaddr_any!(),
                nonce,
            )
            .unwrap(),
        ])
        .into();
        assert_eq!(rv, expected);
    }

    #[test]
    fn test_run_ancestor_hashes() {
        fn deserialize_ancestor_hashes_response(packet: PacketRef) -> AncestorHashesResponse {
            packet
                .deserialize_slice(..packet.meta().size - SIZE_OF_NONCE)
                .unwrap()
        }

        agave_logger::setup();
        let recycler = PacketBatchRecycler::default();
        let ledger_path = get_tmp_ledger_path_auto_delete!();

        let slot = 0;
        let num_slots = MAX_ANCESTOR_RESPONSES as u64;
        let nonce = 10;

        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());

        // Create slots [slot, slot + num_slots) with 5 shreds apiece
        let (shreds, _) = make_many_slot_entries(slot, num_slots, 5);

        blockstore
            .insert_shreds(shreds, None, false)
            .expect("Expect successful ledger write");

        // We don't have slot `slot + num_slots`, so we return empty
        let handler = StandardRepairHandler::new(blockstore.clone());
        let rv = handler
            .run_ancestor_hashes(&recycler, &socketaddr_any!(), slot + num_slots, nonce)
            .expect("run_ancestor_hashes packets");
        assert_eq!(rv.len(), 1);
        let packet = rv.first().unwrap();
        let ancestor_hashes_response = deserialize_ancestor_hashes_response(packet);
        match ancestor_hashes_response {
            AncestorHashesResponse::Hashes(hashes) => {
                assert!(hashes.is_empty());
            }
            _ => {
                panic!("unexpected response: {:?}", &ancestor_hashes_response);
            }
        }

        // `slot + num_slots - 1` is not marked duplicate confirmed so nothing should return
        // empty
        let rv = handler
            .run_ancestor_hashes(&recycler, &socketaddr_any!(), slot + num_slots - 1, nonce)
            .expect("run_ancestor_hashes packets");
        assert_eq!(rv.len(), 1);
        let packet = rv.first().unwrap();
        let ancestor_hashes_response = deserialize_ancestor_hashes_response(packet);
        match ancestor_hashes_response {
            AncestorHashesResponse::Hashes(hashes) => {
                assert!(hashes.is_empty());
            }
            _ => {
                panic!("unexpected response: {:?}", &ancestor_hashes_response);
            }
        }

        // Set duplicate confirmed
        let mut expected_ancestors = Vec::with_capacity(num_slots as usize);
        expected_ancestors.resize(num_slots as usize, (0, Hash::default()));
        for (i, duplicate_confirmed_slot) in (slot..slot + num_slots).enumerate() {
            let frozen_hash = Hash::new_unique();
            expected_ancestors[num_slots as usize - i - 1] =
                (duplicate_confirmed_slot, frozen_hash);
            blockstore.insert_bank_hash(duplicate_confirmed_slot, frozen_hash, true);
        }
        let rv = handler
            .run_ancestor_hashes(&recycler, &socketaddr_any!(), slot + num_slots - 1, nonce)
            .expect("run_ancestor_hashes packets");
        assert_eq!(rv.len(), 1);
        let packet = rv.first().unwrap();
        let ancestor_hashes_response = deserialize_ancestor_hashes_response(packet);
        match ancestor_hashes_response {
            AncestorHashesResponse::Hashes(hashes) => {
                assert_eq!(hashes, expected_ancestors);
            }
            _ => {
                panic!("unexpected response: {:?}", &ancestor_hashes_response);
            }
        }
    }

    #[test]
    fn test_repair_with_repair_validators() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let cluster_slots = ClusterSlots::default_for_tests();
        let cluster_info = Arc::new(new_test_cluster_info());
        let me = cluster_info.my_contact_info();
        // Insert two peers on the network
        let contact_info2 = ContactInfo::new_localhost(&solana_pubkey::new_rand(), timestamp());
        let contact_info3 = ContactInfo::new_localhost(&solana_pubkey::new_rand(), timestamp());
        cluster_info.insert_info(contact_info2.clone());
        cluster_info.insert_info(contact_info3.clone());
        let identity_keypair = cluster_info.keypair();
        let serve_repair = ServeRepair::new_for_test(
            cluster_info,
            bank_forks,
            Arc::new(RwLock::new(HashSet::default())),
        );

        // If:
        // 1) repair validator set doesn't exist in gossip
        // 2) repair validator set only includes our own id
        // then no repairs should be generated
        for pubkey in &[solana_pubkey::new_rand(), *me.pubkey()] {
            let known_validators = Some(vec![*pubkey].into_iter().collect());
            assert!(
                serve_repair
                    .repair_peers(&known_validators, 1, &identity_keypair.pubkey())
                    .is_empty()
            );
            assert_matches!(
                serve_repair.repair_request(
                    &cluster_slots,
                    ShredRepairType::Shred(0, 0),
                    &mut LruCache::new(100),
                    &mut RepairStats::default(),
                    &known_validators,
                    &mut OutstandingShredRepairs::default(),
                    &identity_keypair,
                ),
                Err(Error::ClusterInfo(ClusterInfoError::NoPeers))
            );
        }

        // If known validator exists in gossip, should return repair successfully
        let known_validators = Some(vec![*contact_info2.pubkey()].into_iter().collect());
        let repair_peers =
            serve_repair.repair_peers(&known_validators, 1, &identity_keypair.pubkey());
        assert_eq!(repair_peers.len(), 1);
        assert_eq!(repair_peers[0].pubkey(), contact_info2.pubkey());
        assert_matches!(
            serve_repair.repair_request(
                &cluster_slots,
                ShredRepairType::Shred(0, 0),
                &mut LruCache::new(100),
                &mut RepairStats::default(),
                &known_validators,
                &mut OutstandingShredRepairs::default(),
                &identity_keypair,
            ),
            Ok(Some(_))
        );

        // Using no known validators should default to all
        // validator's available in gossip, excluding myself
        let repair_peers: HashSet<Pubkey> = serve_repair
            .repair_peers(&None, 1, &identity_keypair.pubkey())
            .into_iter()
            .map(|node| *node.pubkey())
            .collect();
        assert_eq!(repair_peers.len(), 2);
        assert!(repair_peers.contains(contact_info2.pubkey()));
        assert!(repair_peers.contains(contact_info3.pubkey()));
        assert_matches!(
            serve_repair.repair_request(
                &cluster_slots,
                ShredRepairType::Shred(0, 0),
                &mut LruCache::new(100),
                &mut RepairStats::default(),
                &None,
                &mut OutstandingShredRepairs::default(),
                &identity_keypair,
            ),
            Ok(Some(_))
        );
    }

    #[test]
    fn test_verify_shred_response() {
        fn new_test_data_shred(slot: Slot, index: u32) -> Shred {
            let shredder = Shredder::new(slot, slot.saturating_sub(1), 0, 0).unwrap();
            let keypair = Keypair::new();
            let reed_solomon_cache = ReedSolomonCache::default();
            let (mut shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
                &keypair,
                &[],
                true,
                Hash::default(),
                0,
                0,
                &reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            );
            shreds.remove(index as usize)
        }
        fn new_test_coding_shred(slot: Slot, index: u32) -> Shred {
            let shredder = Shredder::new(slot, slot.saturating_sub(1), 0, 0).unwrap();
            let keypair = Keypair::new();
            let reed_solomon_cache = ReedSolomonCache::default();
            let (_, mut shreds) = shredder.entries_to_merkle_shreds_for_tests(
                &keypair,
                &[],
                true,
                Hash::default(),
                0,
                0,
                &reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            );
            shreds.remove(index as usize)
        }

        let repair = ShredRepairType::Orphan(9);
        match repair {
            ShredRepairType::Orphan(_)
            | ShredRepairType::HighestShred(_, _)
            | ShredRepairType::Shred(_, _)
            | ShredRepairType::ShredForBlockId { .. } => (),
            // Ensure new options are added below to this test
        };

        let slot = 9;
        let index = 5;

        // Orphan
        let shred = new_test_data_shred(slot, 0);
        let request = ShredRepairType::Orphan(slot);
        assert!(request.verify_response(shred.payload()));
        let shred = new_test_data_shred(slot - 1, 0);
        assert!(request.verify_response(shred.payload()));
        let shred = new_test_data_shred(slot + 1, 0);
        assert!(!request.verify_response(shred.payload()));

        // HighestShred
        let shred = new_test_data_shred(slot, index);
        let request = ShredRepairType::HighestShred(slot, index as u64);
        assert!(request.verify_response(shred.payload()));
        let shred = new_test_data_shred(slot, index + 1);
        assert!(request.verify_response(shred.payload()));
        let shred = new_test_data_shred(slot, index - 1);
        assert!(!request.verify_response(shred.payload()));
        let shred = new_test_data_shred(slot - 1, index);
        assert!(!request.verify_response(shred.payload()));
        let shred = new_test_data_shred(slot + 1, index);
        assert!(!request.verify_response(shred.payload()));

        // Shred
        let shred = new_test_data_shred(slot, index);
        let request = ShredRepairType::Shred(slot, index as u64);
        assert!(request.verify_response(shred.payload()));
        let shred = new_test_data_shred(slot, index + 1);
        assert!(!request.verify_response(shred.payload()));
        let shred = new_test_data_shred(slot + 1, index);
        assert!(!request.verify_response(shred.payload()));

        // ShredForBlockId
        let shred = new_test_data_shred(slot, index);
        let merkle_root = shred.merkle_root().expect("No more legacy shreds");
        let request = ShredRepairType::ShredForBlockId {
            slot,
            index,
            fec_set_merkle_root: merkle_root,
            block_id: Hash::new_unique(),
        };
        assert!(request.verify_response(shred.payload()));
        // bad fec set root
        let request = ShredRepairType::ShredForBlockId {
            slot,
            index,
            fec_set_merkle_root: Hash::new_unique(),
            block_id: Hash::new_unique(),
        };
        assert!(!request.verify_response(shred.payload()));
        // coding shred
        let shred = new_test_coding_shred(slot, index);
        assert!(!request.verify_response(shred.payload()));
        // bad index
        let shred = new_test_data_shred(slot, index + 1);
        assert!(!request.verify_response(shred.payload()));
        // bad slot
        let shred = new_test_data_shred(slot + 1, index);
        assert!(!request.verify_response(shred.payload()));
    }

    fn verify_responses<'a>(
        request: &ShredRepairType,
        packets: impl Iterator<Item = PacketRef<'a>>,
    ) {
        for packet in packets {
            let shred = shred::layout::get_shred(packet).unwrap();
            assert!(request.verify_response(shred));
        }
    }

    #[test]
    fn test_verify_ancestor_response() {
        let request_slot = MAX_ANCESTOR_RESPONSES as Slot;
        let repair = AncestorHashesRepairType(request_slot);
        let mut response: Vec<(Slot, Hash)> = (0..request_slot)
            .map(|slot| (slot, Hash::new_unique()))
            .collect();
        assert!(repair.verify_response(&AncestorHashesResponse::Hashes(response.clone())));

        // over the allowed limit, should fail
        response.push((request_slot, Hash::new_unique()));
        assert!(!repair.verify_response(&AncestorHashesResponse::Hashes(response)));
    }
}
