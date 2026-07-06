use {
    crate::{
        byzzfuzz::schedule::message_slot, integration_tests::ValidatorKeys,
        local_cluster::LocalCluster,
    },
    agave_votor::voting_service::{AlpenglowPortOverride, VotingServiceOverride},
    agave_votor_messages::{
        certificate::Certificate,
        consensus_message::{ConsensusMessage, VoteMessage},
        unverified_vote_message::DecodedWireConsensusMessage,
        wire::VersionedWireConsensusMessage,
    },
    crossbeam_channel::bounded,
    log::{debug, warn},
    rand::rngs::StdRng,
    solana_client::connection_cache::ConnectionCache,
    solana_clock::Slot,
    solana_connection_cache::client_connection::ClientConnection,
    solana_keypair::Keypair,
    solana_net_utils::sockets::bind_to_localhost_unique,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_streamer::{
        nonblocking::simple_qos::SimpleQosConfig,
        quic::{QuicStreamerConfig, SpawnServerResult, spawn_simple_qos_server},
        streamer::{PacketBatchReceiver, StakedNodes},
    },
    std::{
        collections::HashMap,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            Arc, Mutex, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
    tokio_util::sync::CancellationToken,
};

pub type AlpenglowRng = StdRng;

#[derive(Clone, Debug, Default)]
pub struct AlpenglowInterceptorState {
    pub validator_count: usize,
    pub votes: Vec<(Pubkey, Pubkey, VoteMessage)>,
    pub certificates: Vec<(Pubkey, Pubkey, Certificate)>,
}

pub struct AlpenglowInterceptedMessage {
    pub source: Pubkey,
    pub destination: Pubkey,
    pub message: ConsensusMessage,
    pub shred_version: u16,
    // Logical time: the highest slot the interceptor has observed so far.  Used
    // for temporal faults (partitions) that depend on how far consensus has
    // progressed, not on which slot a given message is about.
    pub current_slot: Slot,
}

pub enum AlpenglowInterceptAction {
    Forward,
    Drop,
    Duplicate,
    DuplicateToAll,
    DelayMessages(usize),
    Replace(Box<ConsensusMessage>),
}

type InterceptPolicy =
    Arc<dyn Fn(AlpenglowInterceptedMessage) -> AlpenglowInterceptAction + Send + Sync>;

struct DestinationService {
    cancel: CancellationToken,
    streamer_thread: Option<JoinHandle<()>>,
    processor_thread: Option<JoinHandle<()>>,
}

struct DelayedMessage {
    // Released once the interceptor observes a message at or past this slot.
    release_at: Slot,
    source: Pubkey,
    destination: Pubkey,
    message: ConsensusMessage,
    shred_version: u16,
}

pub struct AlpenglowInterceptor {
    pub state: Arc<Mutex<AlpenglowInterceptorState>>,
    port_override: AlpenglowPortOverride,
    destinations: Arc<RwLock<HashMap<Pubkey, SocketAddr>>>,
    exit: Arc<AtomicBool>,
    services: Vec<DestinationService>,
}

impl AlpenglowInterceptor {
    pub fn new(
        validator_keys: &[ValidatorKeys],
        policy: impl Fn(AlpenglowInterceptedMessage) -> AlpenglowInterceptAction + Send + Sync + 'static,
    ) -> Self {
        let source_clients = Arc::new(Self::source_clients(validator_keys));
        let staked_nodes = Self::staked_nodes(validator_keys);
        let destinations = Arc::new(RwLock::new(HashMap::new()));
        let exit = Arc::new(AtomicBool::new(false));
        let all_destinations = Arc::new(
            validator_keys
                .iter()
                .map(|keys| keys.node_keypair.pubkey())
                .collect::<Vec<_>>(),
        );
        let delayed_messages = Arc::new(Mutex::new(Vec::new()));
        // Highest slot observed across all processor threads — shared logical
        // clock driving temporal faults and delayed-message release.
        let current_slot = Arc::new(AtomicU64::new(0));
        let state = Arc::new(Mutex::new(AlpenglowInterceptorState {
            validator_count: validator_keys.len(),
            ..AlpenglowInterceptorState::default()
        }));
        let policy: InterceptPolicy = Arc::new(policy);
        let port_override = AlpenglowPortOverride::default();
        let mut override_map = HashMap::new();
        let mut services = Vec::with_capacity(validator_keys.len());

        for keys in validator_keys {
            let destination = keys.node_keypair.pubkey();
            let socket =
                bind_to_localhost_unique().expect("AlpenglowInterceptor: bind listener socket");
            let listener_addr = socket
                .local_addr()
                .expect("AlpenglowInterceptor: get listener address");
            override_map.insert(destination, listener_addr);

            let (packet_sender, packet_receiver) = bounded(1024);
            let cancel = CancellationToken::new();
            let listener_keypair = Keypair::new();
            let (
                SpawnServerResult {
                    thread: streamer_thread,
                    ..
                },
                _banlist,
            ) = spawn_simple_qos_server(
                "solAlpIntercept",
                "alpenglow_interceptor",
                [socket.into()],
                &listener_keypair,
                packet_sender,
                staked_nodes.clone(),
                QuicStreamerConfig::default(),
                SimpleQosConfig::default(),
                cancel.clone(),
            )
            .expect("AlpenglowInterceptor: spawn QUIC listener");

            let processor_thread = Self::spawn_processor(
                destination,
                packet_receiver,
                source_clients.clone(),
                all_destinations.clone(),
                destinations.clone(),
                policy.clone(),
                delayed_messages.clone(),
                current_slot.clone(),
                state.clone(),
                exit.clone(),
            );

            services.push(DestinationService {
                cancel,
                streamer_thread: Some(streamer_thread),
                processor_thread: Some(processor_thread),
            });
        }

        port_override.update_override(override_map);
        Self {
            state,
            port_override,
            destinations,
            exit,
            services,
        }
    }

    pub fn voting_service_override(&self) -> VotingServiceOverride {
        VotingServiceOverride {
            additional_listeners: Vec::new(),
            alpenglow_port_override: self.port_override.clone(),
        }
    }

    pub fn set_destinations_from_cluster(&self, cluster: &LocalCluster) {
        self.set_destinations(cluster.validators.values().filter_map(|validator| {
            let contact_info = &validator.info.contact_info;
            contact_info
                .alpenglow()
                .map(|addr| (*contact_info.pubkey(), addr))
        }));
    }

    pub fn set_destinations(&self, destinations: impl IntoIterator<Item = (Pubkey, SocketAddr)>) {
        let mut write = self.destinations.write().unwrap();
        for (pubkey, addr) in destinations {
            write.insert(pubkey, addr);
        }
    }

    fn source_clients(validator_keys: &[ValidatorKeys]) -> HashMap<Pubkey, Arc<ConnectionCache>> {
        let staked_nodes = Self::staked_nodes(validator_keys);
        let max_connections = validator_keys.len().saturating_mul(2).max(1);
        validator_keys
            .iter()
            .map(|keys| {
                let source = keys.node_keypair.pubkey();
                let cache = ConnectionCache::new_with_max_connections(
                    "alpenglow_interceptor_forward",
                    1,
                    max_connections,
                    Some(
                        bind_to_localhost_unique()
                            .expect("AlpenglowInterceptor: bind forward socket"),
                    ),
                    Some((&keys.node_keypair, IpAddr::V4(Ipv4Addr::UNSPECIFIED))),
                    Some((&staked_nodes, &source)),
                );
                (source, Arc::new(cache))
            })
            .collect()
    }

    fn staked_nodes(validator_keys: &[ValidatorKeys]) -> Arc<RwLock<StakedNodes>> {
        let stakes = validator_keys
            .iter()
            .map(|keys| (keys.node_keypair.pubkey(), 1))
            .collect::<HashMap<_, _>>();
        Arc::new(RwLock::new(StakedNodes::new(
            Arc::new(stakes),
            HashMap::new(),
        )))
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_processor(
        destination: Pubkey,
        packet_receiver: PacketBatchReceiver,
        source_clients: Arc<HashMap<Pubkey, Arc<ConnectionCache>>>,
        all_destinations: Arc<Vec<Pubkey>>,
        destinations: Arc<RwLock<HashMap<Pubkey, SocketAddr>>>,
        policy: InterceptPolicy,
        delayed_messages: Arc<Mutex<Vec<DelayedMessage>>>,
        current_slot: Arc<AtomicU64>,
        state: Arc<Mutex<AlpenglowInterceptorState>>,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        Builder::new()
            .name("solAlpInterceptProc".to_string())
            .spawn(move || {
                while let Ok(batch) = packet_receiver.recv() {
                    for packet in batch.iter() {
                        if packet.meta().discard() {
                            continue;
                        }
                        let Some(source) = packet.meta().remote_pubkey() else {
                            warn!("AlpenglowInterceptor: packet missing source identity");
                            continue;
                        };
                        let Some(bytes) = packet.data(..) else {
                            warn!("AlpenglowInterceptor: packet missing data");
                            continue;
                        };
                        let Ok(wire_message) =
                            wincode::deserialize::<VersionedWireConsensusMessage>(bytes)
                        else {
                            warn!("AlpenglowInterceptor: malformed consensus message");
                            continue;
                        };
                        // Preserve the wire shred version so forwarded messages are
                        // accepted by the destination validator's sigverifier.
                        let shred_version = wire_message.shred_version();
                        // Passing the message's own shred version makes the decode's
                        // shred-version filter a no-op; the interceptor relays rather
                        // than validates.
                        let Some(decoded) =
                            DecodedWireConsensusMessage::try_new(wire_message, shred_version)
                        else {
                            warn!("AlpenglowInterceptor: malformed consensus message");
                            continue;
                        };
                        let message = match decoded {
                            DecodedWireConsensusMessage::Vote(vote) => {
                                ConsensusMessage::new_vote(vote.vote, vote.signature, vote.rank)
                            }
                            DecodedWireConsensusMessage::Certificate(cert) => {
                                ConsensusMessage::new_certificate(
                                    cert.cert_type,
                                    cert.bitmap,
                                    cert.signature,
                                )
                            }
                        };
                        // Advance the shared logical clock; `now` is monotonic.
                        let now = current_slot
                            .fetch_max(message_slot(&message), Ordering::Relaxed)
                            .max(message_slot(&message));

                        // Record original traffic before policy decisions mutate it.
                        Self::record_message(&state, source, destination, &message);
                        let action = policy(AlpenglowInterceptedMessage {
                            source,
                            destination,
                            message: message.clone(),
                            shred_version,
                            current_slot: now,
                        });
                        match action {
                            AlpenglowInterceptAction::Forward => Self::forward(
                                source,
                                destination,
                                message,
                                shred_version,
                                source_clients.clone(),
                                destinations.clone(),
                                exit.clone(),
                            ),
                            AlpenglowInterceptAction::Drop => {
                                debug!("AlpenglowInterceptor: dropped {source} -> {destination}");
                            }
                            AlpenglowInterceptAction::Duplicate => {
                                Self::forward(
                                    source,
                                    destination,
                                    message.clone(),
                                    shred_version,
                                    source_clients.clone(),
                                    destinations.clone(),
                                    exit.clone(),
                                );
                                Self::forward(
                                    source,
                                    destination,
                                    message,
                                    shred_version,
                                    source_clients.clone(),
                                    destinations.clone(),
                                    exit.clone(),
                                );
                            }
                            AlpenglowInterceptAction::DuplicateToAll => {
                                Self::forward_to_all(
                                    source,
                                    message,
                                    shred_version,
                                    all_destinations.clone(),
                                    source_clients.clone(),
                                    destinations.clone(),
                                    exit.clone(),
                                );
                            }
                            AlpenglowInterceptAction::DelayMessages(delay) => {
                                Self::delay_message(
                                    &delayed_messages,
                                    now,
                                    delay,
                                    source,
                                    destination,
                                    message,
                                    shred_version,
                                );
                            }
                            AlpenglowInterceptAction::Replace(message) => Self::forward(
                                source,
                                destination,
                                *message,
                                shred_version,
                                source_clients.clone(),
                                destinations.clone(),
                                exit.clone(),
                            ),
                        }
                        Self::release_due_messages(
                            &delayed_messages,
                            now,
                            source_clients.clone(),
                            destinations.clone(),
                            exit.clone(),
                        );
                    }
                }
            })
            .expect("AlpenglowInterceptor: spawn processor")
    }

    fn record_message(
        state: &Arc<Mutex<AlpenglowInterceptorState>>,
        source: Pubkey,
        destination: Pubkey,
        message: &ConsensusMessage,
    ) {
        let mut state = state.lock().unwrap();
        match message {
            ConsensusMessage::Vote(vote) => state.votes.push((source, destination, vote.clone())),
            ConsensusMessage::Certificate(certificate) => {
                state
                    .certificates
                    .push((source, destination, certificate.clone()))
            }
        }
    }

    fn delay_message(
        delayed_messages: &Mutex<Vec<DelayedMessage>>,
        slot: Slot,
        delay: usize,
        source: Pubkey,
        destination: Pubkey,
        message: ConsensusMessage,
        shred_version: u16,
    ) {
        // Hold the message until the protocol advances `delay` slots past it.
        let release_at = slot.saturating_add(delay.clamp(1, 16) as u64);
        delayed_messages.lock().unwrap().push(DelayedMessage {
            release_at,
            source,
            destination,
            message,
            shred_version,
        });
    }

    fn release_due_messages(
        delayed_messages: &Mutex<Vec<DelayedMessage>>,
        current_slot: Slot,
        source_clients: Arc<HashMap<Pubkey, Arc<ConnectionCache>>>,
        destinations: Arc<RwLock<HashMap<Pubkey, SocketAddr>>>,
        exit: Arc<AtomicBool>,
    ) {
        let due = {
            let mut delayed_messages = delayed_messages.lock().unwrap();
            let mut due = Vec::new();
            let mut index = 0;
            while index < delayed_messages.len() {
                if delayed_messages[index].release_at <= current_slot {
                    due.push(delayed_messages.remove(index));
                } else {
                    index += 1;
                }
            }
            due
        };
        for delayed in due {
            Self::forward(
                delayed.source,
                delayed.destination,
                delayed.message,
                delayed.shred_version,
                source_clients.clone(),
                destinations.clone(),
                exit.clone(),
            );
        }
    }

    fn forward_to_all(
        source: Pubkey,
        message: ConsensusMessage,
        shred_version: u16,
        all_destinations: Arc<Vec<Pubkey>>,
        source_clients: Arc<HashMap<Pubkey, Arc<ConnectionCache>>>,
        destinations: Arc<RwLock<HashMap<Pubkey, SocketAddr>>>,
        exit: Arc<AtomicBool>,
    ) {
        for destination in all_destinations.iter().copied() {
            Self::forward(
                source,
                destination,
                message.clone(),
                shred_version,
                source_clients.clone(),
                destinations.clone(),
                exit.clone(),
            );
        }
    }

    fn forward(
        source: Pubkey,
        destination: Pubkey,
        message: ConsensusMessage,
        shred_version: u16,
        source_clients: Arc<HashMap<Pubkey, Arc<ConnectionCache>>>,
        destinations: Arc<RwLock<HashMap<Pubkey, SocketAddr>>>,
        exit: Arc<AtomicBool>,
    ) {
        let Some(destination_addr) = Self::wait_for_destination(destination, &destinations, &exit)
        else {
            return;
        };
        let Some(source_client) = source_clients.get(&source) else {
            warn!("AlpenglowInterceptor: unknown source {source}");
            return;
        };
        // Re-wrap into the wire format so the destination validator's
        // sigverifier can decode it; preserve the original shred version.
        let wire_message = VersionedWireConsensusMessage::new(message, shred_version);
        let Ok(buf) = wincode::serialize(&wire_message) else {
            unreachable!("AlpenglowInterceptor: failed to serialize message");
        };
        let client = source_client.get_connection(&destination_addr);
        if let Err(err) = client.send_data_async(Arc::new(buf)) {
            unreachable!(
                "AlpenglowInterceptor: failed to forward {source} -> {destination}: {err:?}"
            );
        }
    }

    fn wait_for_destination(
        destination: Pubkey,
        destinations: &RwLock<HashMap<Pubkey, SocketAddr>>,
        exit: &AtomicBool,
    ) -> Option<SocketAddr> {
        while !exit.load(Ordering::Relaxed) {
            if let Some(addr) = destinations.read().unwrap().get(&destination).copied() {
                return Some(addr);
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for AlpenglowInterceptor {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        self.port_override.clear();
        for service in &self.services {
            service.cancel.cancel();
        }
        for service in &mut self.services {
            if let Some(thread) = service.streamer_thread.take() {
                thread.join().ok();
            }
            if let Some(thread) = service.processor_thread.take() {
                thread.join().ok();
            }
        }
    }
}
