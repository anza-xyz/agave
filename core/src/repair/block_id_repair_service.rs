//! Service responsible for fetching alternate versions of blocks through informed repair.
//! Receives [`RepairEvent`]s from votor / replay and engages in a repair process to fetch
//! the block to the alternate blockstore column.
//!
//! Sends and processes [`BlockIdRepairType`] requests through a separate socket for block and
//! fec set metadata. Additionally sends [`ShredRepairType`] requests through the main socket
//! to fetch shreds.

mod stats;

use {
    super::{
        repair_service::OutstandingShredRepairs,
        serve_repair::{REPAIR_PEERS_CACHE_CAPACITY, RepairPeers, ServeRepair, ShredRepairType},
        standard_repair_handler::StandardRepairHandler,
    },
    crate::{
        repair::{
            outstanding_requests::OutstandingRequests,
            packet_threshold::DynamicPacketToProcessThreshold,
            repair_service::{REPAIR_MS, RepairInfo, RepairStats},
            serve_repair::{BlockIdRepairResponse, BlockIdRepairType, RepairProtocol},
        },
        shred_fetch_stage::SHRED_FETCH_CHANNEL_SIZE,
    },
    agave_votor::{
        common::DELTA,
        event::{RepairEvent, RepairEventReceiver},
    },
    agave_votor_messages::{consensus_message::Block, migration::MigrationStatus},
    crossbeam_channel::select,
    lazy_lru::LruCache,
    log::{debug, info},
    solana_clock::Slot,
    solana_gossip::ping_pong::{Ping, Pong},
    solana_hash::Hash,
    solana_keypair::signable::Signable,
    solana_ledger::{
        blockstore::{Blockstore, BlockstoreError, CompletedSlotsReceiver},
        blockstore_meta::BlockLocation,
        shred::DATA_SHREDS_PER_FEC_BLOCK,
    },
    solana_perf::{
        packet::{PacketBatch, PacketRef, deserialize_from_with_limit},
        recycler::Recycler,
    },
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::SharableBanks,
    solana_streamer::{
        evicting_sender::EvictingSender,
        sendmmsg::{SendPktsError, batch_send},
        streamer::{self, PacketBatchReceiver, StreamerReceiveStats},
    },
    solana_time_utils::timestamp,
    stats::{BlockIdRepairRequestsStats, BlockIdRepairResponsesStats},
    std::{
        collections::{BTreeSet, BinaryHeap, HashMap, HashSet},
        io::Cursor,
        net::{SocketAddr, UdpSocket},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

type OutstandingBlockIdRepairs = OutstandingRequests<BlockIdRepairType>;

const MAX_REPAIR_REQUESTS_PER_ITERATION: usize = 200;
const MAX_ALTERNATE_BLOCKS_PER_SLOT: usize = 11;
const MAX_PENDING_REPAIR_EVENTS: usize = 10_000;

/// Idle wake-up cadence for `run_repair_iteration`'s `select!`. Bounds the worst-case
/// latency between a `sent_requests` entry exceeding its `2 * DELTA` TTL and us
/// re-queueing it, while keeping idle CPU well below the busy-path `REPAIR_MS` rate.
const IDLE_TICK: Duration = Duration::from_millis(10);

/// Bound to 1/32th the size of shred fetch, as we roughly expect one message per FEC set
const RESPONSE_CHANNEL_SIZE: usize = SHRED_FETCH_CHANNEL_SIZE / DATA_SHREDS_PER_FEC_BLOCK;
/// Roughly 1/32th of shred sigverify, amortizes overhead without starving processing
const SOFT_RECEIVE_CAP: usize = 192;

/// The type of messages that this service will send
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum OutgoingMessage {
    /// Metadata requests
    Metadata(BlockIdRepairType),

    /// Shred request
    Shred(ShredRepairType),
}

impl OutgoingMessage {
    fn slot(&self) -> Slot {
        match self {
            OutgoingMessage::Metadata(block_id_repair_type) => block_id_repair_type.slot(),
            OutgoingMessage::Shred(shred_repair_type) => shred_repair_type.slot(),
        }
    }
}

/// We prioritize requests with lower slot #s,
/// and then prefer metadata requests before shred requests.
impl Ord for OutgoingMessage {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use {
            BlockIdRepairType::*, OutgoingMessage::*, ShredRepairType::ShredForBlockId,
            std::cmp::Ordering,
        };

        if self.slot() != other.slot() {
            // lower slot is higher priority
            return other.slot().cmp(&self.slot());
        }

        match (&self, &other) {
            // prioritize metadata requests
            (Metadata(_), Shred(_)) => Ordering::Greater,
            (Shred(_), Metadata(_)) => Ordering::Less,

            // prioritize top level metadata request
            (Metadata(ParentAndFecSetCount { .. }), _) => Ordering::Greater,
            (_, Metadata(ParentAndFecSetCount { .. })) => Ordering::Less,

            // prioritize lower shred indices
            (
                Metadata(FecSetRoot {
                    fec_set_index: a, ..
                }),
                Metadata(FecSetRoot {
                    fec_set_index: b, ..
                }),
            ) => b.cmp(a),
            (Shred(ShredForBlockId { index: a, .. }), Shred(ShredForBlockId { index: b, .. })) => {
                b.cmp(a)
            }

            _ => Ordering::Equal,
        }
    }
}

impl PartialOrd for OutgoingMessage {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Decision for what to do for a pending repair event
enum PendingRepairDecision {
    KeepPending,
    Drop,
    Act(RepairAction),
}

/// Action to perform as a result of a repair event
enum RepairAction {
    StartRepair { slot: Slot, block_id: Hash },
    QueueParent { slot: Slot, location: BlockLocation },
}

struct RepairState {
    /// Request builder
    serve_repair: ServeRepair,
    /// Repair peers cache
    peers_cache: LruCache<u64, RepairPeers>,

    /// Metadata requests sent to the cluster
    outstanding_requests: OutstandingBlockIdRepairs,
    /// Shred requests sent to the cluster
    outstanding_shred_requests: Arc<RwLock<OutstandingShredRepairs>>,

    /// Repair requests waiting to be sent to the cluster
    pending_repair_requests: BinaryHeap<OutgoingMessage>,
    /// Peers from which we will accept Ping challenges, keyed by sender and source socket.
    expected_ping_responses: HashMap<(Pubkey, SocketAddr), u64>,

    /// Repair events that are pending because Turbine/Eager repair hasn't completed yet.
    /// These are re-processed each iteration until Turbine/Eager repair completes or marks the slot dead.
    /// Only the lowest slots are retained if the queue reaches [`MAX_PENDING_REPAIR_EVENTS`].
    pending_repair_events: BTreeSet<RepairEvent>,

    /// Requests that have been sent, mapped to the timestamp they were sent.
    /// Used for retry logic - requests that exceed `2 * DELTA`
    /// are moved back to pending_repair_requests. We track this separately from the
    /// outstanding_requests maps as those are used for verifying response validity.
    sent_requests: HashMap<OutgoingMessage, u64>,

    /// Blocks we've previously requested. Used to avoid re-initiating repair for an in progress block.
    requested_blocks: HashSet<Block>,

    // Stats
    response_stats: BlockIdRepairResponsesStats,
    request_stats: BlockIdRepairRequestsStats,
}

impl RepairState {
    fn push_pending_repair_event(&mut self, event: RepairEvent) {
        self.pending_repair_events.insert(event);
        if self.pending_repair_events.len() > MAX_PENDING_REPAIR_EVENTS {
            self.pending_repair_events.pop_last();
        }
        debug_assert!(self.pending_repair_events.len() <= MAX_PENDING_REPAIR_EVENTS);
    }

    fn expect_ping_response(&mut self, sender: Pubkey, addr: SocketAddr, now: u64) {
        self.expected_ping_responses.insert((sender, addr), now);
    }

    fn is_expected_ping_response(&self, sender: Pubkey, addr: SocketAddr) -> bool {
        self.expected_ping_responses.contains_key(&(sender, addr))
    }

    fn remove_expected_ping_response(&mut self, sender: Pubkey, addr: SocketAddr) {
        self.expected_ping_responses.remove(&(sender, addr));
    }

    fn prune_expected_ping_responses(&mut self, now: u64) {
        let ttl_ms = u64::try_from(2 * DELTA.as_millis()).unwrap();
        self.expected_ping_responses
            .retain(|_, sent_at| now.saturating_sub(*sent_at) <= ttl_ms);
    }

    fn extend_pending_repair_events(&mut self, events: impl IntoIterator<Item = RepairEvent>) {
        events
            .into_iter()
            .for_each(|event| self.push_pending_repair_event(event));
    }
}

pub struct BlockIdRepairChannels {
    pub repair_event_receiver: RepairEventReceiver,
    pub completed_slots_receiver: CompletedSlotsReceiver,
}

struct BlockIdRepairSockets {
    block_id_repair_socket: Arc<UdpSocket>,
    repair_socket: Arc<UdpSocket>,
}

struct BlockIdRepairContext {
    exit: Arc<AtomicBool>,
    response_receiver: PacketBatchReceiver,
    channels: BlockIdRepairChannels,
    blockstore: Arc<Blockstore>,
    sockets: BlockIdRepairSockets,
    repair_info: RepairInfo,
    outstanding_shred_requests: Arc<RwLock<OutstandingShredRepairs>>,
}

pub struct BlockIdRepairService {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl BlockIdRepairService {
    pub fn new(
        exit: Arc<AtomicBool>,
        blockstore: Arc<Blockstore>,
        block_id_repair_socket: Arc<UdpSocket>,
        repair_socket: Arc<UdpSocket>,
        block_id_repair_channels: BlockIdRepairChannels,
        repair_info: RepairInfo,
        outstanding_shred_requests: Arc<RwLock<OutstandingShredRepairs>>,
    ) -> Self {
        let (response_sender, response_receiver) =
            EvictingSender::new_bounded(RESPONSE_CHANNEL_SIZE);

        // UDP receiver thread
        let t_receiver = streamer::receiver(
            "solRcvrBlockId".to_string(),
            block_id_repair_socket.clone(),
            exit.clone(),
            response_sender,
            Recycler::default(),
            Arc::new(StreamerReceiveStats::new(
                "block_id_repair_response_receiver",
            )),
            None,  // coalesce
            false, // use_pinned_memory
            false, // is_staked_service
        );

        let t_block_id_repair = Self::run(BlockIdRepairContext {
            exit,
            response_receiver,
            channels: block_id_repair_channels,
            blockstore,
            sockets: BlockIdRepairSockets {
                block_id_repair_socket,
                repair_socket,
            },
            repair_info,
            outstanding_shred_requests,
        });

        Self {
            thread_hdls: vec![t_receiver, t_block_id_repair],
        }
    }

    /// Main thread that processes responses and sends requests
    /// - Listens for responses to our block ID repair requests
    /// - Listens for new repair events from votor / replay
    /// - Generates new block id repair requests to send to the cluster
    fn run(context: BlockIdRepairContext) -> JoinHandle<()> {
        Builder::new()
            .name("solBlockIdRep".to_string())
            .spawn(move || {
                info!("BlockIdRepairService started");
                let sharable_banks = context
                    .repair_info
                    .bank_forks
                    .read()
                    .unwrap()
                    .sharable_banks();
                let mut state = RepairState {
                    // One day we'll actually split out the build request functionality from the full ServeRepair :'(
                    serve_repair: ServeRepair::new(
                        context.repair_info.cluster_info.clone(),
                        sharable_banks.clone(),
                        context.repair_info.repair_whitelist.clone(),
                        Box::new(StandardRepairHandler::new(context.blockstore.clone())),
                        // Doesn't matter this serve_repair isn't used to respond
                        Arc::new(MigrationStatus::default()),
                    ),
                    peers_cache: LruCache::new(REPAIR_PEERS_CACHE_CAPACITY),
                    outstanding_requests: OutstandingBlockIdRepairs::default(),
                    outstanding_shred_requests: context.outstanding_shred_requests.clone(),
                    pending_repair_requests: BinaryHeap::default(),
                    expected_ping_responses: HashMap::default(),
                    sent_requests: HashMap::default(),
                    requested_blocks: HashSet::default(),
                    pending_repair_events: BTreeSet::default(),
                    response_stats: BlockIdRepairResponsesStats::default(),
                    request_stats: BlockIdRepairRequestsStats::default(),
                };

                let mut last_stats_report = Instant::now();
                // throttle starts at 1024 responses => 1 second of compute
                let mut throttle = DynamicPacketToProcessThreshold::default();

                while !context.exit.load(Ordering::Relaxed) {
                    if !Self::run_repair_iteration(
                        &context,
                        &sharable_banks,
                        &mut state,
                        &mut last_stats_report,
                        &mut throttle,
                    )
                    .inspect_err(|e| error!("Blockstore error, exiting {e:?}"))
                    .unwrap_or(false)
                    {
                        break;
                    }
                }

                info!("BlockIdRepairService shutting down");
            })
            .unwrap()
    }

    fn run_repair_iteration(
        context: &BlockIdRepairContext,
        sharable_banks: &SharableBanks,
        state: &mut RepairState,
        last_stats_report: &mut Instant,
        throttle: &mut DynamicPacketToProcessThreshold,
    ) -> Result<bool, BlockstoreError> {
        if last_stats_report.elapsed().as_secs() >= 10 {
            state.response_stats.report();
            state.request_stats.report();
            *last_stats_report = Instant::now();
        }

        let mut pending_response = None;
        if !state.pending_repair_requests.is_empty() {
            // We have requests to send out, minimal sleep.
            std::thread::sleep(Duration::from_millis(REPAIR_MS));
        } else {
            // Otherwise wait for:
            // - a new block repair request from votor,
            // - turbine block full event from blockstore (for deferred events)
            // - a response to our repair request
            //
            // If none of those are received, we still continue on in order to retry any timed out requests
            select! {
                recv(&context.channels.completed_slots_receiver) -> result => match result {
                    Ok(_) => (),
                    Err(_) => return Ok(false),
                },
                recv(&context.channels.repair_event_receiver) -> result => match result {
                    Ok(event) => state.push_pending_repair_event(event),
                    Err(_) => return Ok(false),
                },
                recv(&context.response_receiver) -> result => match result {
                    Ok(response) => pending_response = Some(response),
                    Err(_) => return Ok(false),
                },
                default(IDLE_TICK) => ()
            }
        }

        let root = sharable_banks.root().slot();
        let my_pubkey = context.repair_info.cluster_info.id();
        let mut repair_actions = Vec::new();
        let mut first_error = None;

        // Clean up old request tracking
        state.requested_blocks.retain(|(slot, _)| *slot > root);
        state.prune_expected_ping_responses(timestamp());

        // Process responses, generate new requests / repair events
        Self::process_responses(
            &my_pubkey,
            pending_response,
            &context.response_receiver,
            state,
            throttle,
            &context.repair_info.cluster_info.keypair(),
            context.sockets.block_id_repair_socket.as_ref(),
        );

        // Receive new repair events from votor / replay
        state.extend_pending_repair_events(context.channels.repair_event_receiver.try_iter());

        // Filter out actionable repair events from the pending queue
        let requested_blocks = &state.requested_blocks;
        state.pending_repair_events.retain(|event| {
            if first_error.is_some() {
                return true;
            }

            match Self::decide_pending_repair_event(
                &my_pubkey,
                *event,
                root,
                context.blockstore.as_ref(),
                requested_blocks,
            ) {
                Ok(PendingRepairDecision::KeepPending) => true,
                Ok(PendingRepairDecision::Drop) => false,
                Ok(PendingRepairDecision::Act(repair_action)) => {
                    repair_actions.push(repair_action);
                    false
                }
                Err(err) => {
                    first_error = Some(err);
                    true
                }
            }
        });

        if let Some(err) = first_error {
            return Err(err);
        }

        // Generate repair requests for repair actions
        for action in repair_actions {
            Self::process_repair_decision(&my_pubkey, action, context.blockstore.as_ref(), state)?;
        }

        // Retry requests that have timed out
        Self::retry_timed_out_requests(context.blockstore.as_ref(), state, timestamp());

        // Send out new requests
        Self::send_requests(
            context.sockets.block_id_repair_socket.as_ref(),
            context.sockets.repair_socket.as_ref(),
            &context.repair_info,
            sharable_banks.root().slot(),
            state,
        );

        Ok(true)
    }

    /// Process any pending responses from the response receiver and generate any new requests
    fn process_responses(
        my_pubkey: &Pubkey,
        pending_response: Option<PacketBatch>,
        response_receiver: &PacketBatchReceiver,
        state: &mut RepairState,
        throttle: &mut DynamicPacketToProcessThreshold,
        keypair: &solana_keypair::Keypair,
        block_id_repair_socket: &UdpSocket,
    ) {
        let Some(packet_batch) = pending_response.or_else(|| response_receiver.try_recv().ok())
        else {
            return;
        };
        let mut packet_batches = vec![packet_batch];

        // Throttle
        let mut total_packets = packet_batches[0].len();
        let mut dropped_packets = 0;
        while total_packets < SOFT_RECEIVE_CAP {
            let Ok(batch) = response_receiver.try_recv() else {
                break;
            };
            total_packets += batch.len();
            if throttle.should_drop(total_packets) {
                dropped_packets += batch.len();
            } else {
                packet_batches.push(batch);
            }
        }
        state.response_stats.dropped_packets += dropped_packets;
        state.response_stats.total_packets += total_packets;

        // Process all responses, including ping challenges.
        let compute_timer = Instant::now();
        packet_batches
            .iter()
            .flat_map(|packet_batch| packet_batch.iter())
            .for_each(|packet| {
                Self::process_block_id_repair_response(
                    my_pubkey,
                    packet,
                    keypair,
                    block_id_repair_socket,
                    state,
                );
            });

        // adjust throttle based on actual compute time
        throttle.update(total_packets, compute_timer.elapsed());
    }

    /// Process a response:
    /// - Sanity checks on deserialization
    /// - Verify repair nonce
    /// - Queue more repair requests or events
    fn process_block_id_repair_response(
        my_pubkey: &Pubkey,
        packet: PacketRef<'_>,
        keypair: &solana_keypair::Keypair,
        block_id_repair_socket: &UdpSocket,
        state: &mut RepairState,
    ) {
        let Some(packet_data) = packet.data(..) else {
            state.response_stats.invalid_packets += 1;
            return;
        };
        let mut cursor = Cursor::new(packet_data);
        let Ok(response) = deserialize_from_with_limit::<_, BlockIdRepairResponse>(&mut cursor)
            .inspect_err(|e| {
                debug!("Failed to deserialize response: {e:?}");
            })
        else {
            state.response_stats.invalid_packets += 1;
            return;
        };

        if let BlockIdRepairResponse::Ping { ping } = &response {
            let addr = packet.meta().socket_addr();
            Self::process_block_id_repair_ping_response(
                my_pubkey,
                addr,
                ping,
                keypair,
                block_id_repair_socket,
                state,
            );
            return;
        }

        let nonce: u32 = match deserialize_from_with_limit(&mut cursor) {
            Ok(n) => n,
            Err(e) => {
                debug!("{my_pubkey}: Failed to deserialize nonce: {e:?}");
                state.response_stats.invalid_packets += 1;
                return;
            }
        };

        debug!("{my_pubkey}: Received response: {response:?}, nonce={nonce}");

        let Some(request) =
            // verify the response (and check merkle proof validity)
            state.outstanding_requests.register_response(
                nonce,
                &response,
                timestamp(),
                // If valid return the original request
                |block_id_request| *block_id_request,
            )
        else {
            debug!(
                "{my_pubkey}: Response with invalid nonce {nonce} or failed verification for {response:?}"
            );
            state.response_stats.invalid_packets += 1;
            return;
        };

        debug!("{my_pubkey}: Received valid response for request {request:?}");

        // Remove from sent_requests since we got a response
        state
            .sent_requests
            .remove(&OutgoingMessage::Metadata(request));

        let (slot, block_id) = request.block();

        match response {
            BlockIdRepairResponse::ParentFecSetCount {
                fec_set_count,
                parent_info: (p_slot, p_block_id),
                parent_proof: _,
            } => {
                // Queue a request to repair the parent (filtered out later if we already have the parent)
                state.push_pending_repair_event(RepairEvent::FetchBlock {
                    slot: p_slot,
                    block_id: p_block_id,
                });

                // Queue FecSetRoot requests
                state
                    .pending_repair_requests
                    .extend((0..fec_set_count).map(|i| {
                        let fec_set_index = i * DATA_SHREDS_PER_FEC_BLOCK as u32;
                        OutgoingMessage::Metadata(BlockIdRepairType::FecSetRoot {
                            slot,
                            block_id,
                            fec_set_index,
                        })
                    }));

                state.response_stats.parent_fec_set_count_responses += 1;
            }

            BlockIdRepairResponse::FecSetRoot {
                fec_set_root: fec_set_merkle_root,
                ..
            } => {
                let BlockIdRepairType::FecSetRoot { fec_set_index, .. } = request else {
                    panic!(
                        "{my_pubkey}: Programmer error, *verified* response was FecSetRoot but \
                         request was not"
                    );
                };
                let start_index = fec_set_index;
                let end_index = fec_set_index + DATA_SHREDS_PER_FEC_BLOCK as u32;

                // Queue ShredForBlockId requests
                state
                    .pending_repair_requests
                    .extend((start_index..end_index).map(|index| {
                        OutgoingMessage::Shred(ShredRepairType::ShredForBlockId {
                            slot,
                            index,
                            fec_set_merkle_root,
                            block_id,
                        })
                    }));

                state.response_stats.fec_set_root_responses += 1;
            }

            BlockIdRepairResponse::Ping { .. } => {
                unreachable!("Ping handled above")
            }
        }

        state.response_stats.processed += 1;
    }

    /// If this Ping request is expected (from a peer that we recently sent a repair request to),
    /// immediately verify and respond with a Pong. Otherwise ignore the Ping request.
    fn process_block_id_repair_ping_response<const N: usize>(
        my_pubkey: &Pubkey,
        addr: SocketAddr,
        ping: &Ping<N>,
        keypair: &solana_keypair::Keypair,
        block_id_repair_socket: &UdpSocket,
        state: &mut RepairState,
    ) {
        let sender = ping.pubkey();
        if !state.is_expected_ping_response(sender, addr) {
            debug!("{my_pubkey}: Received unexpected ping challenge from {sender} at {addr}");
            state.response_stats.unexpected_ping_responses += 1;
            return;
        }
        if !ping.verify() {
            debug!("{my_pubkey}: Received invalid ping challenge from {addr}, ignoring");
            state.response_stats.invalid_packets += 1;
            return;
        }
        let pong = RepairProtocol::Pong(Pong::new(ping, keypair));
        let pong_bytes = bincode::serialize(&pong).expect("Pong serialization should not fail");

        match block_id_repair_socket.send_to(&pong_bytes, addr) {
            Ok(bytes_sent) if bytes_sent == pong_bytes.len() => {
                debug!("{my_pubkey}: Received ping challenge from {addr}, sent pong");
                state.remove_expected_ping_response(sender, addr);
                state.response_stats.ping_responses += 1;
                state.response_stats.sent_pong_responses += 1;
            }
            Ok(bytes_sent) => {
                debug!(
                    "{my_pubkey}: Failed to send full pong response to {addr}, sent \
                     {bytes_sent}/{} bytes",
                    pong_bytes.len()
                );
                state.response_stats.dropped_pong_responses += 1;
            }
            Err(err) => {
                debug!("{my_pubkey}: Failed to send pong response to {addr}: {err:?}");
                state.response_stats.dropped_pong_responses += 1;
            }
        }
    }

    /// Decide if we should keep a pending repair event or if we should act
    fn decide_pending_repair_event(
        my_pubkey: &Pubkey,
        event: RepairEvent,
        root: Slot,
        blockstore: &Blockstore,
        requested_blocks: &HashSet<Block>,
    ) -> Result<PendingRepairDecision, BlockstoreError> {
        if event.slot() <= root {
            return Ok(PendingRepairDecision::Drop);
        }

        match event {
            RepairEvent::FetchBlock { slot, block_id } => {
                if requested_blocks.contains(&(slot, block_id)) {
                    return Ok(PendingRepairDecision::Drop);
                }

                // Check if we already have the block, if so queue fetching the parent
                // Note: when a block becomes full in blockstore -> we atomically calculate the DMR and populate location
                if let Some(location) = blockstore.get_block_location(slot, block_id)? {
                    return Ok(PendingRepairDecision::Act(RepairAction::QueueParent {
                        slot,
                        location,
                    }));
                }

                // We don't have the block. Check if turbine failed (dead)
                // Note: we require the invariant that Turbine + Eager repair will either:
                // - Eventually fill in all shreds for a slot (slot_meta.is_full()) resulting in the DMR calculation
                // - Mark the slot as dead
                if blockstore.is_dead(slot) {
                    info!(
                        "{my_pubkey}: FetchBlock: slot {slot} is dead, starting repair for \
                         block_id={block_id:?}"
                    );
                    return Ok(PendingRepairDecision::Act(RepairAction::StartRepair {
                        slot,
                        block_id,
                    }));
                }

                // Turbine did not fail, check the progress
                match blockstore.get_double_merkle_root(slot, BlockLocation::Original)? {
                    None => {
                        // Turbine has not completed, defer and check again later
                        debug!(
                            "{my_pubkey}: FetchBlock: Turbine not complete for slot {slot}, \
                             deferring"
                        );
                        Ok(PendingRepairDecision::KeepPending)
                    }
                    Some(turbine_block_id) if turbine_block_id != block_id => {
                        // Turbine has a different block
                        warn!(
                            "{my_pubkey}: FetchBlock: Turbine has different block \
                             {turbine_block_id:?} vs requested {block_id:?} for slot {slot}, \
                             starting repair"
                        );
                        Ok(PendingRepairDecision::Act(RepairAction::StartRepair {
                            slot,
                            block_id,
                        }))
                    }
                    Some(_) => {
                        // Turbine completed between when we checked for the block above and here
                        // Queue the parent
                        debug!(
                            "{my_pubkey}: FetchBlock: Turbine has correct block for slot {slot}, \
                             fetching parent"
                        );
                        Ok(PendingRepairDecision::Act(RepairAction::QueueParent {
                            slot,
                            location: BlockLocation::Original,
                        }))
                    }
                }
            }
        }
    }

    /// Process a repair decision and generate any requests
    fn process_repair_decision(
        my_pubkey: &Pubkey,
        action: RepairAction,
        blockstore: &Blockstore,
        state: &mut RepairState,
    ) -> Result<(), BlockstoreError> {
        match action {
            RepairAction::StartRepair { slot, block_id } => {
                if state.requested_blocks.contains(&(slot, block_id)) {
                    return Ok(());
                }

                // Sanity check: limit alternate blocks per slot.
                let alternate_blocks: Vec<_> = state
                    .requested_blocks
                    .iter()
                    .filter(|(s, _)| *s == slot)
                    .map(|(_, b)| *b)
                    .collect();

                if alternate_blocks.len() >= MAX_ALTERNATE_BLOCKS_PER_SLOT {
                    error!(
                        "{my_pubkey}: Too many alternate blocks for slot {slot}, ignoring request \
                         for {block_id:?}, requested_blocks: {alternate_blocks:?}"
                    );
                    datapoint_error!(
                        "block_id_repair_service-too_many_alternate_blocks",
                        ("slot", slot, i64),
                        ("block_id", block_id.to_string(), String),
                    );
                    return Ok(());
                }

                state
                    .pending_repair_requests
                    .push(OutgoingMessage::Metadata(
                        BlockIdRepairType::ParentAndFecSetCount { slot, block_id },
                    ));
                state.requested_blocks.insert((slot, block_id));
                Ok(())
            }
            RepairAction::QueueParent { slot, location } => {
                Self::queue_fetch_parent_block(blockstore, slot, location, state)
            }
        }
    }

    /// Helper to fetch the parent block for a slot we already have
    fn queue_fetch_parent_block(
        blockstore: &Blockstore,
        slot: Slot,
        location: BlockLocation,
        state: &mut RepairState,
    ) -> Result<(), BlockstoreError> {
        debug_assert!(
            blockstore
                .meta_from_location(slot, location)
                .unwrap()
                .unwrap()
                .is_full()
        );
        let meta = blockstore
            .meta_from_location(slot, location)?
            .expect("SlotMeta must be populated for full slots");

        state.push_pending_repair_event(RepairEvent::FetchBlock {
            slot: meta.parent_slot.expect("Parent must exist for full slots"),
            block_id: meta.parent_block_id,
        });

        Ok(())
    }

    /// Check for requests that have timed out and move them back to pending_repair_requests.
    /// For shred requests, we check if the shred has been received before retrying
    fn retry_timed_out_requests(blockstore: &Blockstore, state: &mut RepairState, now: u64) {
        state.sent_requests.retain(|request, sent_time| {
            if now.saturating_sub(*sent_time) >= u64::try_from(2 * DELTA.as_millis()).unwrap() {
                match request {
                    OutgoingMessage::Metadata(_) => {
                        // Metadata requests: always retry on timeout
                        state.pending_repair_requests.push(request.clone());
                    }
                    // Since shred responses are sent to a different socket, we need to check
                    // blockstore to see if this expired request is actually expired, or if the
                    // shred has already been ingested
                    OutgoingMessage::Shred(shred_request) => {
                        if !Self::has_received_shred(blockstore, shred_request) {
                            state.pending_repair_requests.push(request.clone());
                        }
                    }
                }
                false
            } else {
                true
            }
        });
    }

    /// Check if we have received a shred for a ShredForBlockId request.
    /// Returns true if the shred exists in the blockstore's alternate index.
    fn has_received_shred(blockstore: &Blockstore, request: &ShredRepairType) -> bool {
        let ShredRepairType::ShredForBlockId {
            slot,
            index,
            block_id,
            ..
        } = request
        else {
            return false;
        };

        let location = BlockLocation::Alternate {
            block_id: *block_id,
        };
        blockstore
            .get_index_from_location(*slot, location)
            .ok()
            .flatten()
            .map(|idx| idx.data().contains(*index as u64))
            .unwrap_or(false)
    }

    /// Drain the pending requests and send them out to the cluster
    fn send_requests(
        block_id_repair_socket: &UdpSocket,
        repair_socket: &UdpSocket,
        repair_info: &RepairInfo,
        root: Slot,
        state: &mut RepairState,
    ) {
        let pending_count = state.pending_repair_requests.len();
        let max_batch_len = pending_count.min(MAX_REPAIR_REQUESTS_PER_ITERATION);
        let mut block_id_socket_batch: Vec<(Vec<u8>, SocketAddr)> =
            Vec::with_capacity(max_batch_len);
        let mut shred_socket_batch = Vec::with_capacity(max_batch_len);

        let root_bank = repair_info.bank_forks.read().unwrap().root_bank();
        let staked_nodes = root_bank.current_epoch_staked_nodes();
        let now = timestamp();

        while block_id_socket_batch
            .len()
            .saturating_add(shred_socket_batch.len())
            < MAX_REPAIR_REQUESTS_PER_ITERATION
        {
            let Some(request) = state.pending_repair_requests.pop() else {
                break;
            };

            if request.slot() <= root {
                continue;
            }

            match request {
                OutgoingMessage::Metadata(block_id_repair_type) => {
                    let Ok((bytes, addr, peer_pubkey)) = state
                        .serve_repair
                        .block_id_repair_request(
                            &repair_info.repair_validators,
                            block_id_repair_type,
                            &mut state.peers_cache,
                            &mut state.outstanding_requests,
                            &repair_info.cluster_info.keypair(),
                            &staked_nodes,
                        )
                        .inspect_err(|e| {
                            error!(
                                "Unable to serialize block id repair request \
                                 {block_id_repair_type:?} ignoring: {e:?}"
                            );
                        })
                    else {
                        state.requested_blocks.remove(&block_id_repair_type.block());
                        continue;
                    };

                    block_id_socket_batch.push((bytes, addr));
                    state.sent_requests.insert(request, now);
                    state.expect_ping_response(peer_pubkey, addr, now);

                    // Update stats
                    state.request_stats.total_requests += 1;
                    match block_id_repair_type {
                        BlockIdRepairType::ParentAndFecSetCount { .. } => {
                            state.request_stats.parent_fec_set_count_requests += 1;
                        }
                        BlockIdRepairType::FecSetRoot { .. } => {
                            state.request_stats.fec_set_root_requests += 1;
                        }
                    }
                }
                OutgoingMessage::Shred(shred_request) => {
                    let Ok(Some((addr, bytes))) = state
                        .serve_repair
                        .repair_request(
                            &repair_info.cluster_slots,
                            shred_request,
                            &mut state.peers_cache,
                            &mut RepairStats::default(),
                            &repair_info.repair_validators,
                            &mut state.outstanding_shred_requests.write().unwrap(),
                            &repair_info.cluster_info.keypair(),
                        )
                        .inspect_err(|e| {
                            error!(
                                "Unable to serialize shred for block id repair request \
                                 {shred_request:?} ignoring: {e:?}"
                            );
                        })
                    else {
                        state
                            .requested_blocks
                            .remove(&(shred_request.slot(), shred_request.block_id().unwrap()));
                        continue;
                    };

                    shred_socket_batch.push((bytes, addr));
                    state.sent_requests.insert(request, now);

                    // Update stats
                    state.request_stats.total_requests += 1;
                    state.request_stats.shred_for_block_id_requests += 1;
                }
            }
        }

        if !block_id_socket_batch.is_empty() {
            let total = block_id_socket_batch.len();
            let _ = batch_send(
                block_id_repair_socket,
                block_id_socket_batch
                    .iter()
                    .map(|(bytes, addr)| (bytes, addr)),
            )
            .inspect_err(|SendPktsError::IoError(err, failed)| {
                error!(
                    "{}: failed to send block_id repair packets, packets failed {failed}/{total}: \
                     {err:?}",
                    repair_info.cluster_info.id(),
                )
            });
        }
        if !shred_socket_batch.is_empty() {
            let total = shred_socket_batch.len();
            let _ = batch_send(
                repair_socket,
                shred_socket_batch.iter().map(|(bytes, addr)| (bytes, addr)),
            )
            .inspect_err(|SendPktsError::IoError(err, failed)| {
                error!(
                    "{}: failed to send shred repair requests, packets failed {failed}/{total}: \
                     {err:?}",
                    repair_info.cluster_info.id(),
                )
            });
        }
    }

    pub fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        bincode::Options,
        solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo, ping_pong::Ping},
        solana_hash::Hash,
        solana_keypair::{Keypair, Signer},
        solana_ledger::{
            blockstore::Blockstore,
            get_tmp_ledger_path_auto_delete,
            shred::merkle_tree::{MerkleTree, SIZE_OF_MERKLE_PROOF_ENTRY},
        },
        solana_net_utils::SocketAddrSpace,
        solana_perf::packet::Packet,
        solana_runtime::{bank::Bank, bank_forks::BankForks, genesis_utils::create_genesis_config},
        solana_sha256_hasher::hashv,
        std::{io::Cursor, sync::RwLock},
    };

    /// Helper to build a merkle tree from leaf hashes and return the root and proofs
    fn build_merkle_tree(leaves: &[Hash]) -> (Hash, Vec<Vec<u8>>) {
        let tree =
            MerkleTree::try_new_with_len(leaves.iter().cloned().map(Ok), leaves.len()).unwrap();
        let root = *tree.root();
        let num_leaves = leaves.len();

        // Generate proofs for each leaf
        let proofs = (0..num_leaves)
            .map(|leaf_index| {
                tree.make_merkle_proof(leaf_index, num_leaves)
                    .flat_map(|entry| entry.unwrap().iter().copied())
                    .collect()
            })
            .collect();

        (root, proofs)
    }

    /// Serialize a response and nonce into packet format
    fn serialize_response(response: &BlockIdRepairResponse, nonce: u32) -> Vec<u8> {
        bincode::options()
            .with_fixint_encoding()
            .allow_trailing_bytes()
            .serialize(&(response, nonce))
            .unwrap()
    }

    /// Create a packet from serialized data
    fn make_packet(data: &[u8]) -> Packet {
        let mut packet = Packet::default();
        packet.buffer_mut()[..data.len()].copy_from_slice(data);
        packet.meta_mut().size = data.len();
        packet
    }

    fn new_test_cluster_info() -> ClusterInfo {
        let keypair = Arc::new(Keypair::new());
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), timestamp());
        ClusterInfo::new(contact_info, keypair, SocketAddrSpace::Unspecified)
    }

    fn test_udp_socket() -> UdpSocket {
        solana_net_utils::sockets::bind_to_localhost_unique().unwrap()
    }

    /// Create a RepairState for testing, along with bank_forks for tests that need it
    fn create_test_repair_state() -> (RepairState, Arc<RwLock<BankForks>>) {
        let genesis_config = create_genesis_config(100).genesis_config;
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);

        let cluster_info = Arc::new(new_test_cluster_info());
        let serve_repair = ServeRepair::new_for_test(
            cluster_info,
            bank_forks.clone(),
            Arc::new(RwLock::new(HashSet::default())),
        );

        let state = RepairState {
            serve_repair,
            peers_cache: LruCache::new(REPAIR_PEERS_CACHE_CAPACITY),
            outstanding_requests: OutstandingBlockIdRepairs::default(),
            outstanding_shred_requests: Arc::new(RwLock::new(OutstandingShredRepairs::default())),
            pending_repair_requests: BinaryHeap::default(),
            expected_ping_responses: HashMap::default(),
            sent_requests: HashMap::default(),
            requested_blocks: HashSet::default(),
            pending_repair_events: BTreeSet::default(),
            response_stats: BlockIdRepairResponsesStats::default(),
            request_stats: BlockIdRepairRequestsStats::default(),
        };

        (state, bank_forks)
    }

    fn process_repair_event_for_test(
        my_pubkey: Pubkey,
        event: RepairEvent,
        root: Slot,
        blockstore: &Blockstore,
        state: &mut RepairState,
    ) -> Result<(), BlockstoreError> {
        match BlockIdRepairService::decide_pending_repair_event(
            &my_pubkey,
            event,
            root,
            blockstore,
            &state.requested_blocks,
        )? {
            PendingRepairDecision::KeepPending => {
                state.push_pending_repair_event(event);
                Ok(())
            }
            PendingRepairDecision::Drop => Ok(()),
            PendingRepairDecision::Act(action) => {
                BlockIdRepairService::process_repair_decision(&my_pubkey, action, blockstore, state)
            }
        }
    }

    #[test]
    fn test_pending_repair_events_keeps_lowest_slots_at_capacity() {
        let mut state = create_test_repair_state().0;
        let base_slot = MAX_PENDING_REPAIR_EVENTS as Slot;

        for slot in base_slot..base_slot + MAX_PENDING_REPAIR_EVENTS as Slot {
            state.push_pending_repair_event(RepairEvent::FetchBlock {
                slot,
                block_id: Hash::new_unique(),
            });
            assert!(state.pending_repair_events.len() <= MAX_PENDING_REPAIR_EVENTS);
        }

        state.push_pending_repair_event(RepairEvent::FetchBlock {
            slot: 1,
            block_id: Hash::new_unique(),
        });
        state.push_pending_repair_event(RepairEvent::FetchBlock {
            slot: base_slot + MAX_PENDING_REPAIR_EVENTS as Slot,
            block_id: Hash::new_unique(),
        });

        assert_eq!(state.pending_repair_events.len(), MAX_PENDING_REPAIR_EVENTS);
        let slots: Vec<_> = state
            .pending_repair_events
            .iter()
            .map(RepairEvent::slot)
            .collect();

        assert_eq!(slots.len(), MAX_PENDING_REPAIR_EVENTS);
        assert_eq!(slots[0], 1);
        assert!(!slots.contains(&(base_slot + MAX_PENDING_REPAIR_EVENTS as Slot - 1)));
        assert!(!slots.contains(&(base_slot + MAX_PENDING_REPAIR_EVENTS as Slot)));
        assert!(slots.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn test_deserialize_parent_fec_set_count_response() {
        let fec_set_count = 3u32;
        let parent_slot = 99u64;
        let parent_block_id = Hash::new_unique();
        let parent_proof = vec![1u8; SIZE_OF_MERKLE_PROOF_ENTRY * 2];

        let response = BlockIdRepairResponse::ParentFecSetCount {
            fec_set_count,
            parent_info: (parent_slot, parent_block_id),
            parent_proof: parent_proof.clone(),
        };

        let data = bincode::serialize(&response).unwrap();
        let packet = make_packet(&data);
        let packet_data = packet.data(..).unwrap();

        let deser_response: BlockIdRepairResponse =
            deserialize_from_with_limit(&mut Cursor::new(packet_data)).unwrap();

        match deser_response {
            BlockIdRepairResponse::ParentFecSetCount {
                fec_set_count: fc,
                parent_info: (ps, pb),
                parent_proof: pp,
            } => {
                assert_eq!(fc, fec_set_count);
                assert_eq!(ps, parent_slot);
                assert_eq!(pb, parent_block_id);
                assert_eq!(pp, parent_proof);
            }
            _ => panic!("Expected ParentFecSetCount response"),
        }
    }

    #[test]
    fn test_deserialize_fec_set_root_response() {
        let fec_set_root = Hash::new_unique();
        let fec_set_proof = vec![2u8; SIZE_OF_MERKLE_PROOF_ENTRY * 3];

        let response = BlockIdRepairResponse::FecSetRoot {
            fec_set_root,
            fec_set_proof: fec_set_proof.clone(),
        };

        let data = bincode::serialize(&response).unwrap();
        let packet = make_packet(&data);
        let packet_data = packet.data(..).unwrap();

        let deser_response: BlockIdRepairResponse =
            deserialize_from_with_limit(&mut Cursor::new(packet_data)).unwrap();

        match deser_response {
            BlockIdRepairResponse::FecSetRoot {
                fec_set_root: fr,
                fec_set_proof: fp,
            } => {
                assert_eq!(fr, fec_set_root);
                assert_eq!(fp, fec_set_proof);
            }
            _ => panic!("Expected FecSetRoot response"),
        }
    }

    #[test]
    fn test_deserialize_invalid_response() {
        // Empty packet
        let packet = make_packet(&[]);
        let packet_data = packet.data(..).unwrap();
        assert!(
            deserialize_from_with_limit::<_, BlockIdRepairResponse>(&mut Cursor::new(packet_data))
                .is_err()
        );

        // Garbage data
        let packet = make_packet(&[0xff, 0xff, 0xff, 0xff]);
        let packet_data = packet.data(..).unwrap();
        assert!(
            deserialize_from_with_limit::<_, BlockIdRepairResponse>(&mut Cursor::new(packet_data))
                .is_err()
        );
    }

    #[test]
    fn test_retry_timed_out_requests() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (mut state, _bank_forks) = create_test_repair_state();

        let now = timestamp();
        let expired_time = now - (2 * DELTA.as_millis() as u64) - 100;

        // 1. Expired metadata request (ParentAndFecSetCount) - should retry
        let expired_metadata = OutgoingMessage::Metadata(BlockIdRepairType::ParentAndFecSetCount {
            slot: 100,
            block_id: Hash::new_unique(),
        });
        state
            .sent_requests
            .insert(expired_metadata.clone(), expired_time);

        // 2. Recent metadata request - should stay in sent_requests
        let recent_metadata = OutgoingMessage::Metadata(BlockIdRepairType::ParentAndFecSetCount {
            slot: 101,
            block_id: Hash::new_unique(),
        });
        state.sent_requests.insert(recent_metadata.clone(), now);

        // 3. Expired metadata request (FecSetRoot) - should retry
        let expired_fec_set_root = OutgoingMessage::Metadata(BlockIdRepairType::FecSetRoot {
            slot: 102,
            block_id: Hash::new_unique(),
            fec_set_index: 0,
        });
        state
            .sent_requests
            .insert(expired_fec_set_root.clone(), expired_time);

        // 4. Expired shred request, shred NOT in blockstore - should retry
        let expired_shred_not_received = OutgoingMessage::Shred(ShredRepairType::ShredForBlockId {
            slot: 103,
            index: 5,
            fec_set_merkle_root: Hash::new_unique(),
            block_id: Hash::new_unique(),
        });
        state
            .sent_requests
            .insert(expired_shred_not_received.clone(), expired_time);

        // 5. Expired shred request, shred IS in blockstore - should NOT retry
        let received_block_id = Hash::new_unique();
        let received_slot = 104u64;
        let received_shred_index = 10u32;
        blockstore
            .insert_shred_index_for_alternate_block(
                received_slot,
                received_block_id,
                received_shred_index,
            )
            .unwrap();
        let expired_shred_already_received =
            OutgoingMessage::Shred(ShredRepairType::ShredForBlockId {
                slot: received_slot,
                index: received_shred_index,
                fec_set_merkle_root: Hash::new_unique(),
                block_id: received_block_id,
            });
        state
            .sent_requests
            .insert(expired_shred_already_received.clone(), expired_time);

        // 6. Recent shred request - should stay in sent_requests
        let recent_shred = OutgoingMessage::Shred(ShredRepairType::ShredForBlockId {
            slot: 105,
            index: 15,
            fec_set_merkle_root: Hash::new_unique(),
            block_id: Hash::new_unique(),
        });
        state.sent_requests.insert(recent_shred.clone(), now);

        // Run the retry logic
        BlockIdRepairService::retry_timed_out_requests(&blockstore, &mut state, now);

        // Verify: only non-expired requests remain in sent_requests
        assert_eq!(state.sent_requests.len(), 2);
        assert!(state.sent_requests.contains_key(&recent_metadata));
        assert!(state.sent_requests.contains_key(&recent_shred));

        // Verify: 3 requests moved to pending (2 expired metadata + 1 expired shred not received)
        // The expired shred that was already received should NOT be in pending
        assert_eq!(state.pending_repair_requests.len(), 3);
        let pending: Vec<_> = std::iter::from_fn(|| state.pending_repair_requests.pop()).collect();
        assert!(pending.contains(&expired_metadata));
        assert!(pending.contains(&expired_fec_set_root));
        assert!(pending.contains(&expired_shred_not_received));
        assert!(!pending.contains(&expired_shred_already_received));
    }

    #[test]
    fn test_process_block_id_repair_response_parent_fec_set_count() {
        let (mut state, _bank_forks) = create_test_repair_state();
        let keypair = Keypair::new();
        let block_id_repair_socket = test_udp_socket();

        let slot = 100u64;
        let parent_slot = 99u64;
        let parent_block_id = Hash::new_unique();
        let fec_set_count = 2u32;
        let fec_set_count_usize = usize::try_from(fec_set_count).unwrap();

        // Create valid merkle tree for the response
        let fec_set_roots: Vec<Hash> = (0..fec_set_count).map(|_| Hash::new_unique()).collect();
        let parent_info_leaf = hashv(&[&parent_slot.to_le_bytes(), parent_block_id.as_ref()]);
        let mut leaves = fec_set_roots.clone();
        leaves.push(parent_info_leaf);
        let (block_id, proofs) = build_merkle_tree(&leaves);
        let parent_proof = proofs[fec_set_count_usize].clone();

        // Create the request that would have been sent
        let request = BlockIdRepairType::ParentAndFecSetCount { slot, block_id };

        // Register the request in outstanding_requests and get the nonce
        let nonce = state.outstanding_requests.add_request(request, timestamp());

        // Also track in sent_requests
        state
            .sent_requests
            .insert(OutgoingMessage::Metadata(request), timestamp());

        // Build the response
        let response = BlockIdRepairResponse::ParentFecSetCount {
            fec_set_count,
            parent_info: (parent_slot, parent_block_id),
            parent_proof,
        };

        // Serialize and create packet
        let data = serialize_response(&response, nonce);
        let packet = make_packet(&data);

        BlockIdRepairService::process_block_id_repair_response(
            &Pubkey::new_unique(),
            (&packet).into(),
            &keypair,
            &block_id_repair_socket,
            &mut state,
        );

        // Verify: FetchBlock event for parent was added to pending_repair_events
        assert_eq!(state.pending_repair_events.len(), 1);
        let RepairEvent::FetchBlock {
            slot: s,
            block_id: b,
        } = state.pending_repair_events.first().unwrap();
        assert_eq!(*s, parent_slot);
        assert_eq!(*b, parent_block_id);

        // Verify: FecSetRoot requests were added to pending
        assert_eq!(state.pending_repair_requests.len(), fec_set_count_usize);

        // Verify: request was removed from sent_requests
        assert!(
            !state
                .sent_requests
                .contains_key(&OutgoingMessage::Metadata(request))
        );

        // Verify: stats were updated
        assert_eq!(state.response_stats.parent_fec_set_count_responses, 1);
    }

    #[test]
    fn test_process_block_id_repair_response_fec_set_root() {
        let (mut state, _bank_forks) = create_test_repair_state();
        let keypair = Keypair::new();
        let block_id_repair_socket = test_udp_socket();

        let slot = 100u64;
        let fec_set_index = 32u32; // Second FEC set
        let fec_set_count = 3usize;

        // Create valid merkle tree - FEC set roots form the leaves, parent info is last leaf
        let fec_set_roots: Vec<Hash> = (0..fec_set_count).map(|_| Hash::new_unique()).collect();
        let parent_info_leaf = Hash::new_unique(); // Placeholder for parent info
        let mut leaves = fec_set_roots.clone();
        leaves.push(parent_info_leaf);
        let (block_id, proofs) = build_merkle_tree(&leaves);

        // The FEC set root for fec_set_index=32 corresponds to leaf index 1 (32/32=1)
        let fec_set_leaf_index = fec_set_index as usize / DATA_SHREDS_PER_FEC_BLOCK;
        let fec_set_root = fec_set_roots[fec_set_leaf_index];
        let fec_set_proof = proofs[fec_set_leaf_index].clone();

        // Create the request that would have been sent
        let request = BlockIdRepairType::FecSetRoot {
            slot,
            block_id,
            fec_set_index,
        };

        // Register the request in outstanding_requests and get the nonce
        let nonce = state.outstanding_requests.add_request(request, timestamp());

        // Also track in sent_requests
        state
            .sent_requests
            .insert(OutgoingMessage::Metadata(request), timestamp());

        // Build the response
        let response = BlockIdRepairResponse::FecSetRoot {
            fec_set_root,
            fec_set_proof,
        };

        // Serialize and create packet
        let data = serialize_response(&response, nonce);
        let packet = make_packet(&data);

        BlockIdRepairService::process_block_id_repair_response(
            &Pubkey::new_unique(),
            (&packet).into(),
            &keypair,
            &block_id_repair_socket,
            &mut state,
        );

        // Verify: No FetchBlock events (FecSetRoot doesn't generate those)
        assert!(state.pending_repair_events.is_empty());

        // Verify: ShredForBlockId requests were added to pending (one for each shred in FEC set)
        assert_eq!(
            state.pending_repair_requests.len(),
            DATA_SHREDS_PER_FEC_BLOCK
        );

        // Verify the shred requests have correct parameters
        while let Some(req) = state.pending_repair_requests.pop() {
            match req {
                OutgoingMessage::Shred(ShredRepairType::ShredForBlockId {
                    slot: s,
                    index,
                    fec_set_merkle_root,
                    block_id: b,
                }) => {
                    assert_eq!(s, slot);
                    assert!(
                        index >= fec_set_index
                            && index < fec_set_index + DATA_SHREDS_PER_FEC_BLOCK as u32
                    );
                    assert_eq!(fec_set_merkle_root, fec_set_root);
                    assert_eq!(b, block_id);
                }
                _ => panic!("Expected ShredForBlockId request"),
            }
        }

        // Verify: request was removed from sent_requests
        assert!(
            !state
                .sent_requests
                .contains_key(&OutgoingMessage::Metadata(request))
        );

        // Verify: stats were updated
        assert_eq!(state.response_stats.fec_set_root_responses, 1);
    }

    #[test]
    fn test_process_block_id_repair_response_invalid_nonce() {
        let (mut state, _bank_forks) = create_test_repair_state();
        let keypair = Keypair::new();
        let block_id_repair_socket = test_udp_socket();

        // Create a response with a nonce that wasn't registered
        let response = BlockIdRepairResponse::ParentFecSetCount {
            fec_set_count: 2,
            parent_info: (99, Hash::new_unique()),
            parent_proof: vec![0u8; SIZE_OF_MERKLE_PROOF_ENTRY * 2],
        };

        let invalid_nonce = 99999u32;
        let data = serialize_response(&response, invalid_nonce);
        let packet = make_packet(&data);

        BlockIdRepairService::process_block_id_repair_response(
            &Pubkey::new_unique(),
            (&packet).into(),
            &keypair,
            &block_id_repair_socket,
            &mut state,
        );

        // Verify: No events or requests generated
        assert!(state.pending_repair_events.is_empty());
        assert!(state.pending_repair_requests.is_empty());

        // Verify: invalid packet stat was incremented
        assert_eq!(state.response_stats.invalid_packets, 1);
    }

    #[test]
    fn test_process_block_id_repair_response_ping_sends_pong_and_clears_expected() {
        let (mut state, _bank_forks) = create_test_repair_state();
        let keypair = Keypair::new();
        let block_id_repair_socket = test_udp_socket();
        let pong_receiver = test_udp_socket();
        let ping_keypair = Keypair::new();
        let from_addr = pong_receiver.local_addr().unwrap();
        let ping = Ping::new([7u8; 32], &ping_keypair);
        state.expect_ping_response(ping_keypair.pubkey(), from_addr, timestamp());
        let response = BlockIdRepairResponse::Ping { ping };
        let data = bincode::serialize(&response).unwrap();
        let mut packet = make_packet(&data);
        packet.meta_mut().set_socket_addr(&from_addr);

        BlockIdRepairService::process_block_id_repair_response(
            &Pubkey::new_unique(),
            (&packet).into(),
            &keypair,
            &block_id_repair_socket,
            &mut state,
        );

        assert!(state.pending_repair_requests.is_empty());
        assert_eq!(state.response_stats.ping_responses, 1);
        assert_eq!(state.response_stats.sent_pong_responses, 1);
        assert!(!state.is_expected_ping_response(ping_keypair.pubkey(), from_addr));

        pong_receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut buffer = vec![0; 2048];
        let (size, _) = pong_receiver.recv_from(&mut buffer).unwrap();
        match bincode::deserialize(&buffer[..size]).unwrap() {
            RepairProtocol::Pong(pong) => assert!(pong.verify()),
            request => panic!("Expected Pong response, got {request:?}"),
        }
    }

    #[test]
    fn test_process_block_id_repair_response_ping_removed_after_send() {
        let (mut state, _bank_forks) = create_test_repair_state();
        let keypair = Keypair::new();
        let block_id_repair_socket = test_udp_socket();
        let pong_receiver = test_udp_socket();
        let ping_keypair = Keypair::new();
        let from_addr = pong_receiver.local_addr().unwrap();
        state.expect_ping_response(ping_keypair.pubkey(), from_addr, timestamp());

        let first_ping = Ping::new([1u8; 32], &ping_keypair);
        let response = BlockIdRepairResponse::Ping { ping: first_ping };
        let data = bincode::serialize(&response).unwrap();
        let mut packet = make_packet(&data);
        packet.meta_mut().set_socket_addr(&from_addr);
        BlockIdRepairService::process_block_id_repair_response(
            &Pubkey::new_unique(),
            (&packet).into(),
            &keypair,
            &block_id_repair_socket,
            &mut state,
        );

        let second_ping = Ping::new([2u8; 32], &ping_keypair);
        let response = BlockIdRepairResponse::Ping { ping: second_ping };
        let data = bincode::serialize(&response).unwrap();
        let mut packet = make_packet(&data);
        packet.meta_mut().set_socket_addr(&from_addr);
        BlockIdRepairService::process_block_id_repair_response(
            &Pubkey::new_unique(),
            (&packet).into(),
            &keypair,
            &block_id_repair_socket,
            &mut state,
        );

        assert_eq!(state.response_stats.ping_responses, 1);
        assert_eq!(state.response_stats.sent_pong_responses, 1);
        assert_eq!(state.response_stats.unexpected_ping_responses, 1);
    }

    #[test]
    fn test_process_block_id_repair_response_unexpected_ping_dropped() {
        let (mut state, _bank_forks) = create_test_repair_state();
        let keypair = Keypair::new();
        let block_id_repair_socket = test_udp_socket();
        let ping_keypair = Keypair::new();
        let from_addr = SocketAddr::from(([127, 0, 0, 1], 1234));
        let ping = Ping::new([7u8; 32], &ping_keypair);
        let response = BlockIdRepairResponse::Ping { ping };
        let data = bincode::serialize(&response).unwrap();
        let mut packet = make_packet(&data);
        packet.meta_mut().set_socket_addr(&from_addr);

        BlockIdRepairService::process_block_id_repair_response(
            &Pubkey::new_unique(),
            (&packet).into(),
            &keypair,
            &block_id_repair_socket,
            &mut state,
        );

        assert_eq!(state.response_stats.unexpected_ping_responses, 1);
        assert_eq!(state.response_stats.ping_responses, 0);
    }

    #[test]
    fn test_process_repair_event_dead_slot_triggers_repair() {
        // When Turbine has failed (slot is dead), repair should kick off immediately
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (mut state, _bank_forks) = create_test_repair_state();

        let slot = 100u64;
        let block_id = Hash::new_unique();

        // Mark the slot as dead (Turbine failed)
        blockstore.set_dead_slot(slot).unwrap();

        let event = RepairEvent::FetchBlock { slot, block_id };

        process_repair_event_for_test(Pubkey::new_unique(), event, 0, &blockstore, &mut state)
            .unwrap();

        // Verify: ParentAndFecSetCount request was added
        assert_eq!(state.pending_repair_requests.len(), 1);
        match state.pending_repair_requests.pop().unwrap() {
            OutgoingMessage::Metadata(BlockIdRepairType::ParentAndFecSetCount {
                slot: s,
                block_id: b,
            }) => {
                assert_eq!(s, slot);
                assert_eq!(b, block_id);
            }
            _ => panic!("Expected ParentAndFecSetCount request"),
        }

        // Verify: block was added to requested_blocks
        assert!(state.requested_blocks.contains(&(slot, block_id)));

        // Verify: no deferred events
        assert!(state.pending_repair_events.is_empty());
    }

    #[test]
    fn test_process_repair_event_deferred_when_turbine_not_complete() {
        // When Turbine hasn't completed (slot not dead, no DMR), event should be deferred
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (mut state, _bank_forks) = create_test_repair_state();

        let slot = 100u64;
        let block_id = Hash::new_unique();
        let event = RepairEvent::FetchBlock { slot, block_id };

        process_repair_event_for_test(Pubkey::new_unique(), event, 0, &blockstore, &mut state)
            .unwrap();

        // Verify: No repair request was added (event was deferred)
        assert!(state.pending_repair_requests.is_empty());

        // Verify: Event was deferred
        assert_eq!(state.pending_repair_events.len(), 1);
        let RepairEvent::FetchBlock {
            slot: s,
            block_id: b,
        } = state.pending_repair_events.first().unwrap();
        assert_eq!(*s, slot);
        assert_eq!(*b, block_id);

        // Verify: block was NOT added to requested_blocks (so it can be re-added when reprocessed)
        assert!(!state.requested_blocks.contains(&(slot, block_id)));
    }

    #[test]
    fn test_process_repair_event_turbine_got_different_block() {
        // When Turbine completed with a different block_id, repair should kick off
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (mut state, _bank_forks) = create_test_repair_state();

        let slot = 100u64;
        let requested_block_id = Hash::new_unique();
        let turbine_block_id = Hash::new_unique(); // Different block_id from Turbine

        // Set up blockstore to have a different block_id at Original location
        blockstore
            .set_double_merkle_root(slot, BlockLocation::Original, turbine_block_id)
            .unwrap();

        let event = RepairEvent::FetchBlock {
            slot,
            block_id: requested_block_id,
        };

        process_repair_event_for_test(Pubkey::new_unique(), event, 0, &blockstore, &mut state)
            .unwrap();

        // Verify: ParentAndFecSetCount request was added for the requested block
        assert_eq!(state.pending_repair_requests.len(), 1);
        match state.pending_repair_requests.pop().unwrap() {
            OutgoingMessage::Metadata(BlockIdRepairType::ParentAndFecSetCount {
                slot: s,
                block_id: b,
            }) => {
                assert_eq!(s, slot);
                assert_eq!(b, requested_block_id);
            }
            _ => panic!("Expected ParentAndFecSetCount request"),
        }

        // Verify: block was added to requested_blocks
        assert!(state.requested_blocks.contains(&(slot, requested_block_id)));

        // Verify: no deferred events
        assert!(state.pending_repair_events.is_empty());
    }

    #[test]
    fn test_process_repair_event_already_requested() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (mut state, _bank_forks) = create_test_repair_state();

        let slot = 100u64;
        let block_id = Hash::new_unique();

        // Pre-add block to requested_blocks
        state.requested_blocks.insert((slot, block_id));

        let event = RepairEvent::FetchBlock { slot, block_id };

        process_repair_event_for_test(Pubkey::new_unique(), event, 0, &blockstore, &mut state)
            .unwrap();

        // Verify: No new request was added (block already requested)
        assert!(state.pending_repair_requests.is_empty());
    }

    #[test]
    fn test_process_repair_event_at_root_ignored() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (mut state, _bank_forks) = create_test_repair_state();

        // Use slot 0 which is at root
        let slot = 0u64;
        let block_id = Hash::new_unique();
        let event = RepairEvent::FetchBlock { slot, block_id };

        process_repair_event_for_test(Pubkey::new_unique(), event, 0, &blockstore, &mut state)
            .unwrap();

        // Verify: No request was added (slot at root is ignored)
        assert!(state.pending_repair_requests.is_empty());
        assert!(!state.requested_blocks.contains(&(slot, block_id)));
    }

    #[test]
    fn test_process_repair_event_too_many_alternate_blocks() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (mut state, _bank_forks) = create_test_repair_state();

        let slot = 100u64;

        // Fill up requested_blocks with MAX_ALTERNATE_BLOCKS_PER_SLOT blocks for this slot
        for _ in 0..MAX_ALTERNATE_BLOCKS_PER_SLOT {
            state.requested_blocks.insert((slot, Hash::new_unique()));
        }

        let new_block_id = Hash::new_unique();
        let event = RepairEvent::FetchBlock {
            slot,
            block_id: new_block_id,
        };

        process_repair_event_for_test(Pubkey::new_unique(), event, 0, &blockstore, &mut state)
            .unwrap();

        // Verify: No new request was added
        assert!(state.pending_repair_requests.is_empty());
        // Verify: new block was NOT added to requested_blocks
        assert!(!state.requested_blocks.contains(&(slot, new_block_id)));
    }

    #[test]
    fn test_process_repair_decision_limits_batched_alternate_blocks() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let (mut state, _bank_forks) = create_test_repair_state();
        let my_pubkey = Pubkey::new_unique();

        let slot = 100u64;
        blockstore.set_dead_slot(slot).unwrap();

        let block_ids: Vec<_> = (0..=MAX_ALTERNATE_BLOCKS_PER_SLOT)
            .map(|_| Hash::new_unique())
            .collect();
        let actions: Vec<_> = block_ids
            .iter()
            .map(|block_id| {
                let event = RepairEvent::FetchBlock {
                    slot,
                    block_id: *block_id,
                };
                match BlockIdRepairService::decide_pending_repair_event(
                    &my_pubkey,
                    event,
                    0,
                    &blockstore,
                    &state.requested_blocks,
                )
                .unwrap()
                {
                    PendingRepairDecision::Act(action) => action,
                    _ => panic!("Expected StartRepair action"),
                }
            })
            .collect();

        for action in actions {
            BlockIdRepairService::process_repair_decision(
                &my_pubkey,
                action,
                &blockstore,
                &mut state,
            )
            .unwrap();
        }

        assert_eq!(
            state.pending_repair_requests.len(),
            MAX_ALTERNATE_BLOCKS_PER_SLOT
        );
        assert_eq!(state.requested_blocks.len(), MAX_ALTERNATE_BLOCKS_PER_SLOT);
        assert!(
            block_ids[..MAX_ALTERNATE_BLOCKS_PER_SLOT]
                .iter()
                .all(|block_id| state.requested_blocks.contains(&(slot, *block_id)))
        );
        assert!(
            !state
                .requested_blocks
                .contains(&(slot, block_ids[MAX_ALTERNATE_BLOCKS_PER_SLOT]))
        );
    }
}
