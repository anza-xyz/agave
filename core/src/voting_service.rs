use {
    crate::{
        consensus::tower_storage::{SavedTowerVersions, TowerStorage},
        next_leader::upcoming_leader_tpu_vote_sockets,
        staked_validators_cache::StakedValidatorsCache,
    },
    bincode::serialize,
    crossbeam_channel::{select, Receiver},
    solana_client::connection_cache::ConnectionCache,
    solana_clock::{Slot, FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET},
    solana_connection_cache::client_connection::ClientConnection,
    solana_gossip::cluster_info::ClusterInfo,
    solana_measure::measure::Measure,
    solana_poh::poh_recorder::PohRecorder,
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::BankForks,
    solana_transaction::Transaction,
    solana_transaction_error::TransportError,
    solana_votor::{vote_history_storage::VoteHistoryStorage, voting_utils::BLSOp},
    solana_votor_messages::bls_message::BLSMessage,
    std::{
        collections::HashMap,
        net::SocketAddr,
        sync::{Arc, RwLock},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    thiserror::Error,
};

const STAKED_VALIDATORS_CACHE_TTL_S: u64 = 5;
const STAKED_VALIDATORS_CACHE_NUM_EPOCH_CAP: usize = 5;

pub enum VoteOp {
    PushVote {
        tx: Transaction,
        tower_slots: Vec<Slot>,
        saved_tower: SavedTowerVersions,
    },
    RefreshVote {
        tx: Transaction,
        last_voted_slot: Slot,
    },
}

#[derive(Debug, Error)]
enum SendVoteError {
    #[error(transparent)]
    BincodeError(#[from] bincode::Error),
    #[error("Invalid TPU address")]
    InvalidTpuAddress,
    #[error(transparent)]
    TransportError(#[from] TransportError),
}

fn send_message(
    buf: Vec<u8>,
    socket: &SocketAddr,
    connection_cache: &Arc<ConnectionCache>,
) -> Result<(), TransportError> {
    let client = connection_cache.get_connection(socket);

    client.send_data_async(buf)
}

fn send_vote_transaction(
    cluster_info: &ClusterInfo,
    transaction: &Transaction,
    tpu: Option<SocketAddr>,
    connection_cache: &Arc<ConnectionCache>,
) -> Result<(), SendVoteError> {
    let tpu = tpu
        .or_else(|| {
            cluster_info
                .my_contact_info()
                .tpu(connection_cache.protocol())
        })
        .ok_or(SendVoteError::InvalidTpuAddress)?;
    let buf = serialize(transaction)?;
    let client = connection_cache.get_connection(&tpu);

    client.send_data_async(buf).map_err(|err| {
        trace!("Ran into an error when sending vote: {err:?} to {tpu:?}");
        SendVoteError::from(err)
    })
}

pub struct VotingService {
    thread_hdl: JoinHandle<()>,
}

/// Override for Alpenglow ports to allow testing with different ports
/// The last_modified is used to determine if the override has changed so
/// StakedValidatorsCache can refresh its cache.
/// Inside the map, the key is the validator's vote pubkey and the value
/// is the overridden socket address.
/// For example, if you want validator A to send messages for validator B's
/// Alpenglow port to a new_address, you would insert an entry into the A's
/// map like this: (B will not get the message as a result):
/// `override_map.insert(validator_b_pubkey, new_address);`
#[derive(Clone, Default)]
pub struct AlpenglowPortOverride {
    inner: Arc<RwLock<AlpenglowPortOverrideInner>>,
}

#[derive(Clone)]
struct AlpenglowPortOverrideInner {
    override_map: HashMap<Pubkey, SocketAddr>,
    last_modified: Instant,
}

impl Default for AlpenglowPortOverrideInner {
    fn default() -> Self {
        Self {
            override_map: HashMap::new(),
            last_modified: Instant::now(),
        }
    }
}

impl AlpenglowPortOverride {
    pub fn update_override(&self, new_override: HashMap<Pubkey, SocketAddr>) {
        let mut inner = self.inner.write().unwrap();
        inner.override_map = new_override;
        inner.last_modified = Instant::now();
    }

    pub fn has_new_override(&self, previous: Instant) -> bool {
        self.inner.read().unwrap().last_modified != previous
    }

    pub fn last_modified(&self) -> Instant {
        self.inner.read().unwrap().last_modified
    }

    pub fn clear(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.override_map.clear();
        inner.last_modified = Instant::now();
    }

    pub fn get_override_map(&self) -> HashMap<Pubkey, SocketAddr> {
        self.inner.read().unwrap().override_map.clone()
    }
}

#[derive(Clone)]
pub struct VotingServiceOverride {
    pub additional_listeners: Vec<SocketAddr>,
    pub alpenglow_port_override: AlpenglowPortOverride,
}

impl VotingService {
    pub fn new(
        vote_receiver: Receiver<VoteOp>,
        bls_receiver: Receiver<BLSOp>,
        cluster_info: Arc<ClusterInfo>,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        tower_storage: Arc<dyn TowerStorage>,
        vote_history_storage: Arc<dyn VoteHistoryStorage>,
        connection_cache: Arc<ConnectionCache>,
        bank_forks: Arc<RwLock<BankForks>>,
        test_override: Option<VotingServiceOverride>,
    ) -> Self {
        let (additional_listeners, alpenglow_port_override) = test_override
            .map(|test_override| {
                (
                    Some(test_override.additional_listeners),
                    Some(test_override.alpenglow_port_override),
                )
            })
            .unwrap_or((None, None));
        let thread_hdl = Builder::new()
            .name("solVoteService".to_string())
            .spawn(move || {
                let mut staked_validators_cache = StakedValidatorsCache::new(
                    bank_forks.clone(),
                    connection_cache.protocol(),
                    Duration::from_secs(STAKED_VALIDATORS_CACHE_TTL_S),
                    STAKED_VALIDATORS_CACHE_NUM_EPOCH_CAP,
                    false,
                    alpenglow_port_override,
                );

                loop {
                    select! {
                        recv(vote_receiver) -> vote_op => {
                            match vote_op {
                                Ok(vote_op) => {
                                    Self::handle_vote(
                                        &cluster_info,
                                        &poh_recorder,
                                        tower_storage.as_ref(),
                                        vote_op,
                                        connection_cache.clone(),
                                    );
                                }
                                Err(_) => {
                                    break;
                                }
                            }
                        }
                        recv(bls_receiver) -> bls_op => {
                            match bls_op {
                                Ok(bls_op) => {
                                    Self::handle_bls_vote(
                                        &cluster_info,
                                        vote_history_storage.as_ref(),
                                        bls_op,
                                        connection_cache.clone(),
                                        additional_listeners.as_ref(),
                                        &mut staked_validators_cache,
                                    );
                                }
                                Err(_) => {
                                    break;
                                }
                            }
                        }
                    }
                }
            })
            .unwrap();
        Self { thread_hdl }
    }

    fn broadcast_tower_vote(
        cluster_info: &ClusterInfo,
        poh_recorder: &RwLock<PohRecorder>,
        tx: &Transaction,
        connection_cache: &Arc<ConnectionCache>,
    ) {
        // Attempt to send our vote transaction to the leaders for the next few
        // slots. From the current slot to the forwarding slot offset
        // (inclusive).
        const UPCOMING_LEADER_FANOUT_SLOTS: u64 =
            FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET.saturating_add(1);
        #[cfg(test)]
        static_assertions::const_assert_eq!(UPCOMING_LEADER_FANOUT_SLOTS, 3);

        let leader_fanout = UPCOMING_LEADER_FANOUT_SLOTS;

        let upcoming_leader_sockets = upcoming_leader_tpu_vote_sockets(
            cluster_info,
            poh_recorder,
            leader_fanout,
            connection_cache.protocol(),
        );

        if !upcoming_leader_sockets.is_empty() {
            for tpu_vote_socket in upcoming_leader_sockets {
                let _ = send_vote_transaction(
                    cluster_info,
                    tx,
                    Some(tpu_vote_socket),
                    connection_cache,
                );
            }
        } else {
            // Send to our own tpu vote socket if we cannot find a leader to send to
            let _ = send_vote_transaction(cluster_info, tx, None, connection_cache);
        }
    }

    fn broadcast_alpenglow_message(
        slot: Slot,
        cluster_info: &ClusterInfo,
        bls_message: &BLSMessage,
        connection_cache: Arc<ConnectionCache>,
        additional_listeners: Option<&Vec<SocketAddr>>,
        staked_validators_cache: &mut StakedValidatorsCache,
    ) {
        let (staked_validator_alpenglow_sockets, _) = staked_validators_cache
            .get_staked_validators_by_slot_with_alpenglow_ports(slot, cluster_info, Instant::now());

        let sockets = additional_listeners
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .chain(staked_validator_alpenglow_sockets.iter());
        let buf = match serialize(bls_message) {
            Ok(buf) => buf,
            Err(err) => {
                error!("Failed to serialize alpenglow message: {:?}", err);
                return;
            }
        };

        // We use send_message in a loop right now because we worry that sending packets too fast
        // will cause a packet spike and overwhelm the network. If we later find out that this is
        // not an issue, we can optimize this by using multi_targret_send or similar methods.
        for alpenglow_socket in sockets {
            if let Err(e) = send_message(buf.clone(), alpenglow_socket, &connection_cache) {
                warn!(
                    "Failed to send alpenglow message to {}: {:?}",
                    alpenglow_socket, e
                );
            }
        }
    }

    pub fn handle_bls_vote(
        cluster_info: &ClusterInfo,
        vote_history_storage: &dyn VoteHistoryStorage,
        bls_op: BLSOp,
        connection_cache: Arc<ConnectionCache>,
        additional_listeners: Option<&Vec<SocketAddr>>,
        staked_validators_cache: &mut StakedValidatorsCache,
    ) {
        match bls_op {
            BLSOp::PushVote {
                bls_message,
                slot,
                saved_vote_history,
            } => {
                let mut measure = Measure::start("alpenglow vote history save");
                if let Err(err) = vote_history_storage.store(&saved_vote_history) {
                    error!("Unable to save vote history to storage: {:?}", err);
                    std::process::exit(1);
                }
                measure.stop();
                trace!("{measure}");

                Self::broadcast_alpenglow_message(
                    slot,
                    cluster_info,
                    &bls_message,
                    connection_cache,
                    additional_listeners,
                    staked_validators_cache,
                );
            }
            BLSOp::PushCertificate { certificate } => {
                let vote_slot = certificate.certificate.slot();
                let bls_message = BLSMessage::Certificate((*certificate).clone());
                Self::broadcast_alpenglow_message(
                    vote_slot,
                    cluster_info,
                    &bls_message,
                    connection_cache,
                    additional_listeners,
                    staked_validators_cache,
                );
            }
        }
    }

    pub fn handle_vote(
        cluster_info: &ClusterInfo,
        poh_recorder: &RwLock<PohRecorder>,
        tower_storage: &dyn TowerStorage,
        vote_op: VoteOp,
        connection_cache: Arc<ConnectionCache>,
    ) {
        match vote_op {
            VoteOp::PushVote {
                tx,
                tower_slots,
                saved_tower,
            } => {
                let mut measure = Measure::start("tower storage save");
                if let Err(err) = tower_storage.store(&saved_tower) {
                    error!("Unable to save tower to storage: {:?}", err);
                    std::process::exit(1);
                }
                measure.stop();
                trace!("{measure}");

                Self::broadcast_tower_vote(cluster_info, poh_recorder, &tx, &connection_cache);

                cluster_info.push_vote(&tower_slots, tx);
            }
            VoteOp::RefreshVote {
                tx,
                last_voted_slot,
            } => {
                cluster_info.refresh_vote(tx, last_voted_slot);
            }
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::consensus::tower_storage::NullTowerStorage,
        bitvec::prelude::*,
        solana_bls_signatures::Signature as BLSSignature,
        solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::Blockstore, get_tmp_ledger_path_auto_delete,
            leader_schedule_cache::LeaderScheduleCache,
        },
        solana_poh_config::PohConfig,
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                create_genesis_config_with_alpenglow_vote_accounts_no_program,
                ValidatorVoteKeypairs,
            },
        },
        solana_signer::Signer,
        solana_streamer::{packet::Packet, recvmmsg::recv_mmsg, socket::SocketAddrSpace},
        solana_votor::vote_history_storage::{
            NullVoteHistoryStorage, SavedVoteHistory, SavedVoteHistoryVersions,
        },
        solana_votor_messages::{
            bls_message::{
                BLSMessage, Certificate, CertificateMessage, CertificateType, VoteMessage,
            },
            vote::Vote,
        },
        std::{
            net::SocketAddr,
            sync::{atomic::AtomicBool, Arc, RwLock},
        },
        test_case::test_case,
    };

    fn create_voting_service(
        vote_receiver: Receiver<VoteOp>,
        bls_receiver: Receiver<BLSOp>,
        listener: SocketAddr,
    ) -> VotingService {
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts_no_program(
            1_000_000_000,
            &validator_keypairs,
            vec![100; validator_keypairs.len()],
        );
        let bank0 = Bank::new_for_tests(&genesis.genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank0);
        let keypair = Keypair::new();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = ClusterInfo::new(
            contact_info,
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        );
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let working_bank = bank_forks.read().unwrap().working_bank();
        let poh_recorder = PohRecorder::new(
            working_bank.tick_height(),
            working_bank.last_blockhash(),
            working_bank.clone(),
            None,
            working_bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&working_bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
        .0;

        VotingService::new(
            vote_receiver,
            bls_receiver,
            Arc::new(cluster_info),
            Arc::new(RwLock::new(poh_recorder)),
            Arc::new(NullTowerStorage::default()),
            Arc::new(NullVoteHistoryStorage::default()),
            Arc::new(ConnectionCache::with_udp("TestConnectionCache", 10)),
            bank_forks,
            Some(VotingServiceOverride {
                additional_listeners: vec![listener],
                alpenglow_port_override: AlpenglowPortOverride::default(),
            }),
        )
    }

    #[test_case(BLSOp::PushVote {
        bls_message: Arc::new(BLSMessage::Vote(VoteMessage {
            vote: Vote::new_skip_vote(5),
            signature: BLSSignature::default(),
            rank: 1,
        })),
        slot: 5,
        saved_vote_history: SavedVoteHistoryVersions::Current(SavedVoteHistory::default()),
    }, BLSMessage::Vote(VoteMessage {
        vote: Vote::new_skip_vote(5),
        signature: BLSSignature::default(),
        rank: 1,
    }))]
    #[test_case(BLSOp::PushCertificate {
        certificate: Arc::new(CertificateMessage {
            certificate: Certificate::new(CertificateType::Skip, 5, None),
            signature: BLSSignature::default(),
            bitmap: BitVec::new(),
        }),
    }, BLSMessage::Certificate(CertificateMessage {
        certificate: Certificate::new(CertificateType::Skip, 5, None),
        signature: BLSSignature::default(),
        bitmap: BitVec::new(),
    }))]
    fn test_send_bls_message(bls_op: BLSOp, expected_bls_message: BLSMessage) {
        solana_logger::setup();
        let (_vote_sender, vote_receiver) = crossbeam_channel::unbounded();
        let (bls_sender, bls_receiver) = crossbeam_channel::unbounded();
        // Create listener thread on a random port we allocated and return SocketAddr to create VotingService

        // Bind to a random UDP port
        let socket = solana_net_utils::bind_to_localhost().unwrap();
        let listener_addr = socket.local_addr().unwrap();

        // Create VotingService with the listener address
        let _ = create_voting_service(vote_receiver, bls_receiver, listener_addr);

        // Send a BLS message via the VotingService
        assert!(bls_sender.send(bls_op).is_ok());

        // Wait for the listener to receive the message
        let mut packets = vec![Packet::default(); 1];
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert!(recv_mmsg(&socket, &mut packets[..]).is_ok());
        let packet = packets.first().expect("No packets received");
        let received_bls_message = packet
            .deserialize_slice::<BLSMessage, _>(..)
            .unwrap_or_else(|err| {
                panic!(
                    "Failed to deserialize BLSMessage: {:?} {:?}",
                    size_of::<BLSMessage>(),
                    err
                )
            });
        assert_eq!(received_bls_message, expected_bls_message);
    }
}
