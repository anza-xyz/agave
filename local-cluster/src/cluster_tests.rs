/// Cluster independent integration tests
///
/// All tests must start from an entry point and a funding keypair and
/// discover the rest of the network.
use log::*;
use {
    crate::local_cluster::LocalCluster,
    agave_votor_messages::{
        consensus_message::VoteMessage, unverified_vote_message::DecodedWireConsensusMessage,
        wire::VersionedWireConsensusMessage,
    },
    crossbeam_channel::bounded,
    rand::{Rng, rng},
    rayon::{ThreadPool, prelude::*},
    solana_clock::{self as clock, Slot},
    solana_commitment_config::CommitmentConfig,
    solana_core::consensus::tower_storage::{
        FileTowerStorage, SavedTower, SavedTowerVersions, TowerStorage,
    },
    solana_entry::entry::{self, Entry, EntrySlice},
    solana_epoch_schedule::MINIMUM_SLOTS_PER_EPOCH,
    solana_gossip::{
        cluster_info::{self, ClusterInfo},
        contact_info::ContactInfo,
        crds::Cursor,
        crds_data::{self, CrdsData},
        crds_value::{CrdsValue, CrdsValueLabel},
        gossip_error::GossipError,
        gossip_service::{self, GossipService, discover_validators},
    },
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::blockstore::Blockstore,
    solana_net_utils::{SocketAddrSpace, sockets::bind_to_localhost_unique},
    solana_perf::packet::{PacketRef, packet_config},
    solana_poh_config::PohConfig,
    solana_pubkey::Pubkey,
    solana_rpc_client::rpc_client::RpcClient,
    solana_runtime::bank_forks::BankForks,
    solana_signer::{Signer, signers::Signers},
    solana_streamer::{
        nonblocking::simple_qos::SimpleQosConfig,
        quic::{QuicStreamerConfig, spawn_simple_qos_server},
        streamer::StakedNodes,
    },
    solana_system_transaction as system_transaction,
    solana_time_utils::timestamp,
    solana_tpu_client_next::{
        client_builder::{ClientBuilder, TransactionSender},
        leader_updater::create_pinned_leader_updater,
    },
    solana_transaction::Transaction,
    solana_transaction_error::TransportError,
    solana_validator_exit::Exit,
    solana_vote::vote_transaction::{self, VoteTransaction},
    solana_vote_program::vote_state::TowerSync,
    std::{
        collections::{HashMap, HashSet, VecDeque},
        net::{SocketAddr, TcpListener, UdpSocket},
        path::{Path, PathBuf},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{JoinHandle, sleep},
        time::{Duration, Instant},
    },
    tokio_util::sync::CancellationToken,
    wincode,
};

/// Packages a multi-threaded tokio runtime with a tpu-client-next sender, providing
/// a synchronous interface for sending transactions in local-cluster tests.
#[derive(Clone)]
pub struct TpuSender {
    runtime: Arc<tokio::runtime::Runtime>,
}

impl TpuSender {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            runtime: Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .enable_all()
                    .build()
                    .expect("TpuSender: should create tokio runtime"),
            ),
        }
    }

    /// Open a QUIC connection to `tpu_addr`, run `f` with the live sender, then shut down.
    ///
    /// Keeps the tpu-client-next scheduler alive for the duration of `f` so that any
    /// transactions enqueued via [`send_wire_transaction`] are not dropped before wire
    /// transmission. Shutdown happens synchronously after `f` returns.
    pub fn with_connection<F, R>(&self, tpu_addr: SocketAddr, f: F) -> R
    where
        F: FnOnce(&TransactionSender) -> R,
    {
        let bind_socket = bind_to_localhost_unique().expect("TpuSender: should bind UDP socket");
        let (sender, client) = self
            .runtime
            .block_on(async {
                ClientBuilder::new(create_pinned_leader_updater(tpu_addr))
                    .bind_socket(bind_socket)
                    .build()
            })
            .expect("TpuSender: should build tpu-client-next client");
        let result = f(&sender);
        self.runtime
            .block_on(async { client.shutdown().await })
            .ok();
        result
    }

    /// Send a pre-serialized transaction wire frame through an open `sender`.
    pub fn send_wire_transaction(&self, sender: &TransactionSender, wire_tx: Vec<u8>) {
        self.runtime
            .block_on(async { sender.send_transactions_in_batch(vec![wire_tx]).await })
            .expect("TpuSender: should send transactions in batch");
    }

    /// Send a pre-serialized wire frame, logging any error rather than panicking.
    ///
    /// Use this in tests that tolerate transient send failures (e.g. partition tests).
    pub fn try_send_wire_transaction(&self, sender: &TransactionSender, wire_tx: Vec<u8>) {
        if let Err(e) = self
            .runtime
            .block_on(async { sender.send_transactions_in_batch(vec![wire_tx]).await })
        {
            debug!("TpuSender: send_wire_transaction failed: {e:?}");
        }
    }

    /// Send and confirm `transaction` with retries via `sender`, using `rpc_client` for
    /// confirmation and blockhash refresh.
    fn send_and_confirm_transaction<T: Signers + ?Sized>(
        &self,
        sender: &TransactionSender,
        rpc_client: &RpcClient,
        signers: &T,
        transaction: &mut Transaction,
        max_attempts: usize,
    ) -> Result<(), TransportError> {
        for attempt in 1..=max_attempts {
            let wire_tx = wincode::serialize(transaction)
                .expect("send_and_confirm_transaction: should serialize transaction");
            self.send_wire_transaction(sender, wire_tx);
            if LocalCluster::poll_for_successfully_processed_transaction(rpc_client, transaction)?
                .is_some()
            {
                return Ok(());
            }
            let (blockhash, _) =
                rpc_client.get_latest_blockhash_with_commitment(CommitmentConfig::processed())?;
            transaction.sign(signers, blockhash);
            warn!("send_and_confirm_transaction: attempt {attempt} failed, retrying");
        }
        Err(std::io::Error::other("failed to confirm transaction after max retries").into())
    }

    /// Open a QUIC connection to `tpu_addr` and send-and-confirm `transaction` with retries.
    ///
    /// Uses `rpc_client` to poll for confirmation and refresh the blockhash between attempts.
    pub fn send_transaction_with_retries<T: Signers + ?Sized>(
        &self,
        tpu_addr: SocketAddr,
        rpc_client: &RpcClient,
        signers: &T,
        transaction: &mut Transaction,
        attempts: usize,
    ) -> Result<(), TransportError> {
        let sender_clone = self.clone();
        self.with_connection(tpu_addr, move |sender| {
            sender_clone.send_and_confirm_transaction(
                sender,
                rpc_client,
                signers,
                transaction,
                attempts,
            )
        })
    }
}

/// Resolve the current slot leader and its QUIC TPU address via RPC.
///
/// Returns `None` when the leader cannot yet be mapped to a QUIC TPU address.
fn current_leader(rpc_client: &RpcClient) -> Option<(Pubkey, SocketAddr)> {
    let leader_pubkey = rpc_client
        .get_slot_leader()
        .expect("current_leader: get_slot_leader");
    let tpu = rpc_client
        .get_cluster_nodes()
        .expect("current_leader: get_cluster_nodes")
        .into_iter()
        .find(|n| n.pubkey == leader_pubkey.to_string())
        .and_then(|n| n.tpu_quic)?;
    Some((leader_pubkey, tpu))
}

/// Resolve the QUIC TPU address of the current slot leader via RPC.
///
/// Returns `None` when the leader cannot yet be mapped to a QUIC TPU address
fn current_leader_tpu(rpc_client: &RpcClient) -> Option<SocketAddr> {
    current_leader(rpc_client).map(|(_, tpu)| tpu)
}

/// Poll until `transaction` is observed as processed by `rpc_client`, or `timeout` elapses.
///
/// Returns `true` once the transaction is processed. Returns `false` if timeout expires.
fn poll_processed_until_timeout(
    rpc_client: &RpcClient,
    transaction: &Transaction,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = rpc_client.get_signature_status_with_commitment(
            &transaction.signatures[0],
            CommitmentConfig::processed(),
        ) {
            // Surface a genuine execution error rather than silently retrying it as if the
            // transaction had never arrived.
            assert!(
                status.is_ok(),
                "poll_processed_within: transaction failed on-chain: {status:?}"
            );
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(200));
    }
}

/// Spend and verify from every node in the network
pub fn spend_and_verify_all_nodes<S: ::std::hash::BuildHasher + Sync + Send>(
    entry_point_info: &ContactInfo,
    funding_keypair: &Keypair,
    nodes: usize,
    ignore_nodes: HashSet<Pubkey, S>,
    socket_addr_space: SocketAddrSpace,
    tpu_sender: &TpuSender,
) {
    let cluster_nodes = discover_validators(
        &entry_point_info.gossip().unwrap(),
        nodes,
        entry_point_info.shred_version(),
        socket_addr_space,
    )
    .unwrap();
    assert_eq!(cluster_nodes.len(), nodes);
    let ignore_nodes = Arc::new(ignore_nodes);
    cluster_nodes.par_iter().for_each(|ingress_node| {
        if ignore_nodes.contains(ingress_node.pubkey()) {
            return;
        }
        let random_keypair = Keypair::new();
        let rpc_client = RpcClient::new(format!("http://{}", ingress_node.rpc().unwrap()));
        let bal = rpc_client
            .poll_get_balance_with_commitment(
                &funding_keypair.pubkey(),
                CommitmentConfig::processed(),
            )
            .expect("balance in source");
        assert!(bal > 0);
        let (blockhash, _) = rpc_client
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
            .unwrap();
        let mut transaction =
            system_transaction::transfer(funding_keypair, &random_keypair.pubkey(), 1, blockhash);
        // find target TPU address from RPC
        let tpu_addr = current_leader_tpu(&rpc_client)
            .expect("spend_and_verify_all_nodes: no current leader TPU");
        tpu_sender.with_connection(tpu_addr, |sender| {
            tpu_sender
                .send_and_confirm_transaction(
                    sender,
                    &rpc_client,
                    &[funding_keypair],
                    &mut transaction,
                    10,
                )
                .unwrap();
        });
        for validator in &cluster_nodes {
            if ignore_nodes.contains(validator.pubkey()) {
                continue;
            }
            LocalCluster::poll_for_successfully_processed_transaction(&rpc_client, &transaction)
                .unwrap()
                .unwrap();
        }
    });
}

pub fn verify_balances<S: ::std::hash::BuildHasher>(
    expected_balances: HashMap<Pubkey, u64, S>,
    node: &ContactInfo,
) {
    let rpc_client = RpcClient::new(format!("http://{}", node.rpc().unwrap()));
    for (pk, b) in expected_balances {
        let bal = rpc_client
            .poll_get_balance_with_commitment(&pk, CommitmentConfig::processed())
            .expect("balance in source");
        assert_eq!(bal, b);
    }
}

pub fn send_many_transactions(
    node: &ContactInfo,
    funding_keypair: &Keypair,
    tpu_sender: &TpuSender,
    max_tokens_per_transfer: u64,
    num_txs: u64,
) -> HashMap<Pubkey, u64> {
    let rpc_client = RpcClient::new(format!("http://{}", node.rpc().unwrap()));
    // Ask RPC who the leader is
    let tpu_addr =
        current_leader_tpu(&rpc_client).expect("send_many_transactions: no current leader TPU");
    // One persistent QUIC connection for all transactions to avoid per-tx handshake overhead.
    tpu_sender.with_connection(tpu_addr, |sender| {
        let mut expected_balances = HashMap::new();
        for _ in 0..num_txs {
            let random_keypair = Keypair::new();
            let bal = rpc_client
                .poll_get_balance_with_commitment(
                    &funding_keypair.pubkey(),
                    CommitmentConfig::processed(),
                )
                .expect("balance in source");
            assert!(bal > 0);
            let (blockhash, _) = rpc_client
                .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
                .unwrap();
            let transfer_amount = rng().random_range(1..max_tokens_per_transfer);
            let mut transaction = system_transaction::transfer(
                funding_keypair,
                &random_keypair.pubkey(),
                transfer_amount,
                blockhash,
            );
            tpu_sender
                .send_and_confirm_transaction(
                    sender,
                    &rpc_client,
                    &[funding_keypair],
                    &mut transaction,
                    5,
                )
                .unwrap();
            expected_balances.insert(random_keypair.pubkey(), transfer_amount);
        }
        expected_balances
    })
}

pub fn verify_ledger_ticks(ledger_path: &Path, ticks_per_slot: usize) {
    let ledger = Blockstore::open(ledger_path).unwrap();
    let thread_pool = entry::thread_pool_for_tests();

    let zeroth_slot = ledger.get_slot_entries(0, 0).unwrap();
    let last_id = zeroth_slot.last().unwrap().hash;
    let next_slots = ledger.get_slots_since(&[0]).unwrap().remove(&0).unwrap();
    let mut pending_slots: Vec<_> = next_slots
        .into_iter()
        .map(|slot| (slot, 0, last_id))
        .collect();
    while let Some((slot, parent_slot, last_id)) = pending_slots.pop() {
        let next_slots = ledger
            .get_slots_since(&[slot])
            .unwrap()
            .remove(&slot)
            .unwrap();

        // If you're not the last slot, you should have a full set of ticks
        let should_verify_ticks = if !next_slots.is_empty() {
            Some((slot - parent_slot) as usize * ticks_per_slot)
        } else {
            None
        };

        let last_id = verify_slot_ticks(&ledger, &thread_pool, slot, &last_id, should_verify_ticks);
        pending_slots.extend(
            next_slots
                .into_iter()
                .map(|child_slot| (child_slot, slot, last_id)),
        );
    }
}

pub fn sleep_n_epochs(
    num_epochs: f64,
    config: &PohConfig,
    ticks_per_slot: u64,
    slots_per_epoch: u64,
) {
    let num_ticks_per_second = config.target_tick_duration.as_secs_f64().recip();
    let num_ticks_to_sleep = num_epochs * ticks_per_slot as f64 * slots_per_epoch as f64;
    let secs = ((num_ticks_to_sleep + num_ticks_per_second - 1.0) / num_ticks_per_second) as u64;
    warn!("sleep_n_epochs: {secs} seconds");
    sleep(Duration::from_secs(secs));
}

pub fn kill_entry_and_spend_and_verify_rest(
    entry_point_info: &ContactInfo,
    entry_point_validator_exit: &Arc<RwLock<Exit>>,
    funding_keypair: &Keypair,
    tpu_sender: &TpuSender,
    nodes: usize,
    slot_millis: u64,
    socket_addr_space: SocketAddrSpace,
) {
    info!("kill_entry_and_spend_and_verify_rest...");

    // Ensure all nodes have spun up and are funded.
    let cluster_nodes = discover_validators(
        &entry_point_info.gossip().unwrap(),
        nodes,
        entry_point_info.shred_version(),
        socket_addr_space,
    )
    .unwrap();
    assert!(cluster_nodes.len() >= nodes);
    let ep_rpc = RpcClient::new(format!("http://{}", entry_point_info.rpc().unwrap()));
    for ingress_node in &cluster_nodes {
        ep_rpc
            .poll_get_balance_with_commitment(ingress_node.pubkey(), CommitmentConfig::processed())
            .unwrap_or_else(|err| panic!("Node {} has no balance: {}", ingress_node.pubkey(), err));
    }

    // Kill the entry point node and wait for it to die.
    info!("killing entry point: {}", entry_point_info.pubkey());
    entry_point_validator_exit.write().unwrap().exit();
    info!("sleeping for some time to let entry point exit and partitions to resolve...");
    sleep(Duration::from_millis(slot_millis * MINIMUM_SLOTS_PER_EPOCH));
    info!("done sleeping");

    // Ensure all other nodes are still alive and able to ingest and confirm
    // transactions.
    for ingress_node in &cluster_nodes {
        if ingress_node.pubkey() == entry_point_info.pubkey() {
            info!("ingress_node.id == entry_point_info.id, continuing...");
            continue;
        }

        // Ensure the current ingress node is still funded.
        let rpc_client = RpcClient::new(format!("http://{}", ingress_node.rpc().unwrap()));
        let balance = rpc_client
            .poll_get_balance_with_commitment(
                &funding_keypair.pubkey(),
                CommitmentConfig::processed(),
            )
            .expect("balance in source");
        assert_ne!(balance, 0);

        // After the bootstrap leader is killed it stays in the (fixed) leader schedule for the
        // rest of the epoch, so `get_slot_leader` points at the dead node for ~1/4 of slots. It
        // accepts no QUIC connection, so targeting it wastes a whole poll window; skip it and wait
        // one slot for rotation to a live leader instead. For live leaders, resend with a fresh
        // blockhash each attempt and give the send a bounded window to land.
        let killed_leader = *entry_point_info.pubkey();
        const MAX_SEND_ATTEMPTS: usize = 20;
        // Generous enough for a live leader to land a transaction on a loaded CI runner, yet far
        // below the blockhash lifetime so a wrong/stuck target is abandoned in seconds.
        let poll_window = Duration::from_millis(slot_millis * 20).max(Duration::from_secs(8));
        let mut confirmed = false;
        for attempt in 1..=MAX_SEND_ATTEMPTS {
            let Some((leader_pubkey, tpu_addr)) = current_leader(&rpc_client) else {
                // Leader not resolvable, back off a slot
                sleep(Duration::from_millis(slot_millis));
                continue;
            };
            if leader_pubkey == killed_leader {
                // Scheduled leader is the dead bootstrap node; wait for rotation rather than
                // burning a poll window on a node that will never accept the transaction.
                sleep(Duration::from_millis(slot_millis));
                continue;
            }
            let random_keypair = Keypair::new();
            let (blockhash, _) = rpc_client
                .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
                .unwrap();
            let transaction = system_transaction::transfer(
                funding_keypair,
                &random_keypair.pubkey(),
                1,
                blockhash,
            );
            let wire_tx = wincode::serialize(&transaction)
                .expect("kill_entry_and_spend_and_verify_rest: should serialize transaction");
            // Poll *inside* the connection: tpu-client-next only enqueues the transaction, so the
            // QUIC client must stay alive long enough to flush it onto the wire (and retransmit)
            // before we tear it down.
            let landed = tpu_sender.with_connection(tpu_addr, |sender| {
                tpu_sender.send_wire_transaction(sender, wire_tx);
                poll_processed_until_timeout(&rpc_client, &transaction, poll_window)
            });
            if !landed {
                warn!(
                    "kill_entry_and_spend_and_verify_rest: attempt {attempt} to {tpu_addr} did \
                     not land within {poll_window:?}, re-resolving leader"
                );
                continue;
            }

            // The transaction landed at the targeted node, ensure every other node
            // observes it too. If it landed on a fork that loses, resend.
            info!("poll_all_nodes_for_signature()");
            match poll_all_nodes_for_signature(entry_point_info, &cluster_nodes, &transaction) {
                Ok(()) => {
                    info!("poll_all_nodes_for_signature() succeeded, done.");
                    confirmed = true;
                    break;
                }
                Err(e) => info!("poll_all_nodes_for_signature() failed {e:?}, resending"),
            }
        }
        assert!(
            confirmed,
            "kill_entry_and_spend_and_verify_rest: transaction never confirmed on all surviving \
             nodes after {MAX_SEND_ATTEMPTS} attempts"
        );
    }
}

pub fn apply_votes_to_tower(node_keypair: &Keypair, votes: Vec<(Slot, Hash)>, tower_path: PathBuf) {
    let tower_storage = FileTowerStorage::new(tower_path);
    let mut tower = tower_storage.load(&node_keypair.pubkey()).unwrap();
    for (slot, hash) in votes {
        tower.record_vote(slot, hash);
    }
    let saved_tower = SavedTowerVersions::from(SavedTower::new(&tower, node_keypair).unwrap());
    tower_storage.store(&saved_tower).unwrap();
}

pub fn check_min_slot_is_rooted(min_slot: Slot, contact_infos: &[ContactInfo], test_name: &str) {
    let mut last_print = Instant::now();
    let loop_start = Instant::now();
    let loop_timeout = Duration::from_secs(180);
    for ingress_node in contact_infos.iter() {
        let rpc_client = RpcClient::new(format!("http://{}", ingress_node.rpc().unwrap()));
        loop {
            let root_slot = rpc_client
                .get_slot_with_commitment(CommitmentConfig::finalized())
                .unwrap_or(0);
            if root_slot >= min_slot || last_print.elapsed().as_secs() > 3 {
                info!(
                    "{} waiting for node {} to see root >= {}.. observed latest root: {}",
                    test_name,
                    ingress_node.pubkey(),
                    min_slot,
                    root_slot
                );
                last_print = Instant::now();
                if root_slot >= min_slot {
                    break;
                }
            }
            sleep(Duration::from_millis(clock::DEFAULT_MS_PER_SLOT / 2));
            assert!(loop_start.elapsed() < loop_timeout);
        }
    }
}

fn check_for_new_slots_with_commitment(
    num_new_slots: usize,
    contact_infos: &[ContactInfo],
    test_name: &str,
    commitment: CommitmentConfig,
) {
    let mut slots = vec![HashSet::new(); contact_infos.len()];
    let mut done = false;
    let mut last_print = Instant::now();
    let loop_start = Instant::now();
    let loop_timeout = Duration::from_secs(180);
    let mut num_slots_map = HashMap::new();
    while !done {
        assert!(loop_start.elapsed() < loop_timeout);

        for (i, ingress_node) in contact_infos.iter().enumerate() {
            let rpc_client = RpcClient::new(format!("http://{}", ingress_node.rpc().unwrap()));
            let slot = rpc_client.get_slot_with_commitment(commitment).unwrap_or(0);
            slots[i].insert(slot);
            num_slots_map.insert(*ingress_node.pubkey(), slots[i].len());
            let num_slots = slots.iter().map(|r| r.len()).min().unwrap();
            done = num_slots >= num_new_slots;
            if done || last_print.elapsed().as_secs() > 3 {
                info!(
                    "{test_name} waiting for {num_new_slots} new {} slots .. observed: \
                     {num_slots_map:?}",
                    commitment.commitment,
                );
                last_print = Instant::now();
            }
        }
        sleep(Duration::from_millis(clock::DEFAULT_MS_PER_SLOT / 2));
    }
}

pub fn check_for_new_roots(num_new_roots: usize, contact_infos: &[ContactInfo], test_name: &str) {
    check_for_new_slots_with_commitment(
        num_new_roots,
        contact_infos,
        test_name,
        CommitmentConfig::finalized(),
    );
}

pub fn check_for_new_processed(
    num_new_processed: usize,
    contact_infos: &[ContactInfo],
    test_name: &str,
) {
    check_for_new_slots_with_commitment(
        num_new_processed,
        contact_infos,
        test_name,
        CommitmentConfig::processed(),
    );
}

/// Start a QUIC streamer to listen for votes and certificates.
/// Returns a cancellation token, the server thread handle, and a receiver for packet batches.
pub fn start_quic_streamer_to_listen_for_votes_and_certs(
    vote_listener_socket: UdpSocket,
    validator_keys: &[Arc<Keypair>],
    node_stakes: &[u64],
) -> (
    CancellationToken,
    JoinHandle<()>,
    crossbeam_channel::Receiver<solana_streamer::packet::PacketBatch>,
) {
    let (sender, receiver) = bounded(1024);
    let cancel = CancellationToken::new();
    let stakes = validator_keys
        .iter()
        .zip(node_stakes)
        .map(|(keypair, stake)| (keypair.pubkey(), *stake))
        .collect();
    let staked_nodes: Arc<RwLock<StakedNodes>> = Arc::new(RwLock::new(StakedNodes::new(
        Arc::new(stakes),
        HashMap::<Pubkey, u64>::default(), // overrides
    )));
    let (result, _banlist) = spawn_simple_qos_server(
        "solAlpenglowTest",
        "alpenglow_local_cluster_test",
        [vote_listener_socket.into()],
        &Keypair::new(),
        sender,
        staked_nodes,
        QuicStreamerConfig::default(),
        SimpleQosConfig::default(),
        cancel.clone(),
    )
    .unwrap();
    (cancel, result.thread, receiver)
}

fn convert_packet_to_vote_message(
    bank_forks: &RwLock<BankForks>,
    packet: PacketRef,
    my_shred_version: u16,
) -> Option<VoteMessage> {
    let sender = packet.meta().remote_pubkey()?;
    let Ok(msg) = VersionedWireConsensusMessage::deserialize_with_expected_shred_version(
        packet.data(..).unwrap_or_default(),
        packet_config(),
        my_shred_version,
    ) else {
        return None;
    };
    let DecodedWireConsensusMessage::Vote(vote_msg) = DecodedWireConsensusMessage::new(msg) else {
        return None;
    };
    let bank = bank_forks.read().unwrap().root_bank();
    let rank_map = bank.get_rank_map(vote_msg.vote.slot())?;
    let sender_entry = rank_map.node_pubkey_to_stake_entry(&sender)?;
    let rank = *rank_map.get_rank(&sender_entry.bls_pubkey)?;
    Some(VoteMessage {
        vote: vote_msg.vote,
        signature: vote_msg.signature,
        rank,
    })
}

/// Check that all nodes in the cluster are producing notarized votes.
pub fn check_for_new_notarized_votes(
    my_shred_version: u16,
    num_new_votes: usize,
    contact_infos: &[ContactInfo],
    test_name: &str,
    vote_listener_socket: UdpSocket,
    validator_node_keypairs: &[Arc<Keypair>],
    node_stakes: &[u64],
    bank_forks: Arc<RwLock<BankForks>>,
) {
    let loop_start = Instant::now();
    let loop_timeout = Duration::from_secs(180);
    // First get the current max processed slot.
    let Some(current_slot) = contact_infos
        .iter()
        .map(|ingress_node| {
            RpcClient::new(format!("http://{}", ingress_node.rpc().unwrap()))
                .get_slot_with_commitment(CommitmentConfig::processed())
                .unwrap_or(0)
        })
        .max()
    else {
        panic!("No nodes found to get current slot");
    };

    // Clone data for thread
    let contact_infos_owned: Vec<ContactInfo> = contact_infos.to_vec();
    let test_name_owned = test_name.to_string();

    let (cancel, quic_server_thread, receiver) = start_quic_streamer_to_listen_for_votes_and_certs(
        vote_listener_socket,
        validator_node_keypairs,
        node_stakes,
    );

    // Now start vote listener and wait for new notarized votes.
    std::thread::scope(|s| {
        s.spawn(move || {
            let mut num_new_notarized_votes =
                contact_infos_owned.iter().map(|_| 0).collect::<Vec<_>>();
            let mut last_notarized = contact_infos_owned
                .iter()
                .map(|_| current_slot)
                .collect::<Vec<_>>();
            let mut last_print = Instant::now();
            let mut done = false;

            while !done {
                assert!(loop_start.elapsed() < loop_timeout);
                let Ok(packet_batch) = receiver.recv_timeout(Duration::from_millis(100)) else {
                    continue;
                };
                for packet in packet_batch.iter() {
                    let Some(vote_message) =
                        convert_packet_to_vote_message(&bank_forks, packet, my_shred_version)
                    else {
                        continue;
                    };
                    let vote = vote_message.vote;
                    if !vote.is_notarization() {
                        continue;
                    }
                    let rank = vote_message.rank;
                    if rank >= contact_infos_owned.len() as u16 {
                        warn!(
                            "Received vote with rank {} which is greater than number of nodes {}",
                            rank,
                            contact_infos_owned.len()
                        );
                        continue;
                    }
                    let slot = vote.slot();
                    if slot <= last_notarized[rank as usize] {
                        continue;
                    }
                    last_notarized[rank as usize] = slot;
                    num_new_notarized_votes[rank as usize] += 1;
                    done = num_new_notarized_votes.iter().all(|&x| x > num_new_votes);
                    if done || last_print.elapsed().as_secs() > 3 {
                        info!(
                            "{test_name_owned} waiting for {num_new_votes} new notarized votes.. \
                             observed: {num_new_notarized_votes:?}"
                        );
                        last_print = Instant::now();
                    }
                }
                if done {
                    cancel.cancel();
                }
            }
        });
    });
    quic_server_thread
        .join()
        .expect("QUIC server thread panicked");
}

pub fn check_no_new_roots(
    num_slots_to_wait: usize,
    contact_infos: &[&ContactInfo],
    test_name: &str,
) {
    assert!(!contact_infos.is_empty());
    let mut roots = vec![0; contact_infos.len()];
    let max_slot = contact_infos
        .iter()
        .enumerate()
        .map(|(i, ingress_node)| {
            let rpc_client = RpcClient::new(format!("http://{}", ingress_node.rpc().unwrap()));
            let initial_root = rpc_client
                .get_slot()
                .unwrap_or_else(|_| panic!("get_slot for {} failed", ingress_node.pubkey()));
            roots[i] = initial_root;
            rpc_client
                .get_slot_with_commitment(CommitmentConfig::processed())
                .unwrap_or_else(|_| panic!("get_slot for {} failed", ingress_node.pubkey()))
        })
        .max()
        .unwrap();

    let end_slot = max_slot + num_slots_to_wait as u64;
    let mut current_slot;
    let mut last_print = Instant::now();
    let mut reached_end_slot = false;
    loop {
        for contact_info in contact_infos {
            let rpc_client = RpcClient::new(format!("http://{}", contact_info.rpc().unwrap()));
            current_slot = rpc_client
                .get_slot_with_commitment(CommitmentConfig::processed())
                .unwrap_or_else(|_| panic!("get_slot for {} failed", contact_infos[0].pubkey()));
            if current_slot > end_slot {
                reached_end_slot = true;
                break;
            }
            if last_print.elapsed().as_secs() > 3 {
                info!(
                    "{} current slot: {} on validator: {}, waiting for any validator with slot: {}",
                    test_name,
                    current_slot,
                    contact_info.pubkey(),
                    end_slot
                );
                last_print = Instant::now();
            }
        }
        if reached_end_slot {
            break;
        }
    }

    for (i, ingress_node) in contact_infos.iter().enumerate() {
        let rpc_client = RpcClient::new(format!("http://{}", ingress_node.rpc().unwrap()));
        assert_eq!(
            rpc_client
                .get_slot()
                .unwrap_or_else(|_| panic!("get_slot for {} failed", ingress_node.pubkey())),
            roots[i]
        );
    }
}

fn poll_all_nodes_for_signature(
    entry_point_info: &ContactInfo,
    cluster_nodes: &[ContactInfo],
    transaction: &Transaction,
) -> Result<(), TransportError> {
    for validator in cluster_nodes {
        if validator.pubkey() == entry_point_info.pubkey() {
            continue;
        }
        let rpc_client = RpcClient::new(format!("http://{}", validator.rpc().unwrap()));
        LocalCluster::poll_for_successfully_processed_transaction(&rpc_client, transaction)?
            .unwrap();
    }

    Ok(())
}

pub struct GossipVoter {
    pub gossip_service: GossipService,
    pub tcp_listener: Option<TcpListener>,
    pub cluster_info: Arc<ClusterInfo>,
    pub t_voter: JoinHandle<()>,
    pub exit: Arc<AtomicBool>,
}

impl GossipVoter {
    pub fn close(self) {
        self.exit.store(true, Ordering::Relaxed);
        self.t_voter.join().unwrap();
        self.gossip_service.join().unwrap();
    }
}

/// Reads votes from gossip and runs them through `vote_filter` to filter votes that then
/// get passed to `generate_vote_tx` to create votes that are then pushed into gossip as if
/// sent by a node with identity `node_keypair`.
pub fn start_gossip_voter(
    gossip_addr: &SocketAddr,
    node_keypair: &Keypair,
    vote_filter: impl Fn((CrdsValueLabel, Transaction)) -> Option<(VoteTransaction, Transaction)>
    + std::marker::Send
    + 'static,
    mut process_vote_tx: impl FnMut(Slot, &Transaction, &VoteTransaction, &ClusterInfo)
    + std::marker::Send
    + 'static,
    sleep_ms: u64,
    num_expected_peers: usize,
    refresh_ms: u64,
    max_votes_to_refresh: usize,
    shred_version: u16,
) -> GossipVoter {
    let exit = Arc::new(AtomicBool::new(false));
    let (gossip_service, tcp_listener, cluster_info) = gossip_service::make_node(
        // Need to use our validator's keypair to gossip EpochSlots and votes for our
        // node later.
        node_keypair.insecure_clone(),
        &[*gossip_addr],
        exit.clone(),
        None,
        shred_version,
        false,
        SocketAddrSpace::Unspecified,
    );

    // Wait for peer discovery
    while cluster_info.gossip_peers().len() < num_expected_peers {
        sleep(Duration::from_millis(sleep_ms));
    }

    let mut latest_voted_slot = 0;
    let mut refreshable_votes: VecDeque<(Transaction, VoteTransaction)> = VecDeque::new();
    let mut latest_push_attempt = Instant::now();

    let t_voter = {
        let exit = exit.clone();
        let cluster_info = cluster_info.clone();
        std::thread::spawn(move || {
            let mut cursor = Cursor::default();
            loop {
                if exit.load(Ordering::Relaxed) {
                    return;
                }

                let (labels, votes) = cluster_info.get_votes_with_labels(&mut cursor);
                if labels.is_empty() {
                    if latest_push_attempt.elapsed() > Duration::from_millis(refresh_ms) {
                        for (leader_vote_tx, parsed_vote) in refreshable_votes.iter().rev() {
                            let vote_slot = parsed_vote.last_voted_slot().unwrap();
                            info!("gossip voter refreshing vote {vote_slot}");
                            process_vote_tx(vote_slot, leader_vote_tx, parsed_vote, &cluster_info);
                            latest_push_attempt = Instant::now();
                        }
                    }
                    sleep(Duration::from_millis(sleep_ms));
                    continue;
                }
                let mut parsed_vote_iter: Vec<_> = labels
                    .into_iter()
                    .zip(votes)
                    .filter_map(&vote_filter)
                    .collect();

                parsed_vote_iter.sort_by(|(vote, _), (vote2, _)| {
                    vote.last_voted_slot()
                        .unwrap()
                        .cmp(&vote2.last_voted_slot().unwrap())
                });

                for (parsed_vote, leader_vote_tx) in &parsed_vote_iter {
                    if let Some(vote_slot) = parsed_vote.last_voted_slot() {
                        info!("received vote for {vote_slot}");
                        if vote_slot > latest_voted_slot {
                            latest_voted_slot = vote_slot;
                            refreshable_votes
                                .push_front((leader_vote_tx.clone(), parsed_vote.clone()));
                            refreshable_votes.truncate(max_votes_to_refresh);
                        }
                        process_vote_tx(vote_slot, leader_vote_tx, parsed_vote, &cluster_info);
                        latest_push_attempt = Instant::now();
                    }
                    // Give vote some time to propagate
                    sleep(Duration::from_millis(sleep_ms));
                }
            }
        })
    };

    GossipVoter {
        gossip_service,
        tcp_listener,
        cluster_info,
        t_voter,
        exit,
    }
}

fn get_and_verify_slot_entries(
    blockstore: &Blockstore,
    thread_pool: &ThreadPool,
    slot: Slot,
    last_entry: &Hash,
) -> Vec<Entry> {
    let entries = blockstore.get_slot_entries(slot, 0).unwrap();
    assert!(entries.verify(last_entry, thread_pool).status());
    entries
}

fn verify_slot_ticks(
    blockstore: &Blockstore,
    thread_pool: &ThreadPool,
    slot: Slot,
    last_entry: &Hash,
    expected_num_ticks: Option<usize>,
) -> Hash {
    let entries = get_and_verify_slot_entries(blockstore, thread_pool, slot, last_entry);
    let num_ticks: usize = entries.iter().map(|entry| entry.is_tick() as usize).sum();
    if let Some(expected_num_ticks) = expected_num_ticks {
        assert_eq!(num_ticks, expected_num_ticks);
    }
    entries.last().unwrap().hash
}

pub fn submit_vote_to_cluster_gossip(
    node_keypair: &Keypair,
    vote_keypair: &Keypair,
    vote_slot: Slot,
    vote_hash: Hash,
    blockhash: Hash,
    gossip_addr: SocketAddr,
    socket_addr_space: &SocketAddrSpace,
) -> Result<(), GossipError> {
    let tower_sync = TowerSync::new_from_slots(vec![vote_slot], vote_hash, None);
    let vote_tx = vote_transaction::new_tower_sync_transaction(
        tower_sync,
        blockhash,
        node_keypair,
        vote_keypair,
        vote_keypair,
        None,
    );

    cluster_info::push_messages_to_peer_for_tests(
        vec![CrdsValue::new(
            CrdsData::Vote(
                0,
                crds_data::Vote::new(node_keypair.pubkey(), vote_tx, timestamp()).unwrap(),
            ),
            node_keypair,
        )],
        node_keypair.pubkey(),
        gossip_addr,
        socket_addr_space,
    )
}
