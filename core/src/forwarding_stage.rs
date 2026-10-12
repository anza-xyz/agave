//! `ForwardingStage` is a stage parallel to `BankingStage` that forwards
//! packets to a node that is or will be leader soon.

use {
    agave_banking_stage_ingress_types::BankingPacketBatch,
    agave_transaction_view::transaction_view::SanitizedTransactionView,
    agave_votor::slot_clock::SharedAlpenglowSlotClock,
    crossbeam_channel::{Receiver, RecvTimeoutError},
    packet_container::PacketContainer,
    solana_clock::FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET,
    solana_cost_model::cost_model::CostModel,
    solana_fee_structure::FeeDetails,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::Protocol, node::NodeMultihoming},
    solana_keypair::Keypair,
    solana_leader_schedule::NUM_CONSECUTIVE_LEADER_SLOTS,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_net_utils::{multihomed_sockets::BindIpAddrs, token_bucket::TokenBucket},
    solana_packet as packet,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, CollectorFeeDetails},
        bank_forks::SharableBanks,
    },
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, sanitize_config::sanitize_config,
        transaction_meta::TransactionMeta,
    },
    solana_tls_utils::NotifyKeyUpdate,
    solana_tpu_client_next::{
        ConnectionWorkersScheduler, WireTransaction,
        connection_workers_scheduler::{
            BindTarget, ConnectionWorkersSchedulerConfig, Fanout, StakeIdentity,
        },
        leader_updater::LeaderUpdater,
    },
    solana_transaction::sanitized::MessageHash,
    solana_transaction_error::TransportError,
    std::{
        net::{SocketAddr, UdpSocket},
        num::NonZeroUsize,
        sync::Arc,
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::{
        runtime::Handle as RuntimeHandle,
        sync::{mpsc, watch},
    },
    tokio_util::sync::CancellationToken,
};

mod packet_container;

/// [`ForwardingClientConfig`] is the config for `tpu-client-next` instance.
pub struct ForwardingClientConfig<'a> {
    pub stake_identity: &'a Keypair,
    pub tpu_client_sockets: Box<[UdpSocket]>,
    pub runtime_handle: RuntimeHandle,
    pub cancel: CancellationToken,
    pub node_multihoming: Arc<NodeMultihoming>,
}

/// Maximum forwarding rate in bytes per second.
const MAX_BYTES_PER_SECOND: u64 = 12_000_000;

/// Maximum number of transactions collected before forwarding a batch.
///
/// Value chosen because it was used historically, at some point
/// was found to be optimal. If we need to improve performance
/// this should be evaluated with new stage.
const FORWARD_BATCH_SIZE: usize = 128;

/// Scheduler channel capacity in transactions.
const SCHEDULER_CHANNEL_CAPACITY: usize = FORWARD_BATCH_SIZE;

/// Worker channel capacity in transactions.
const WORKER_CHANNEL_CAPACITY: usize = FORWARD_BATCH_SIZE;

/// [`ForwardAddressGetter`] provides helper methods for retrieving transaction
/// forwarding addresses.
#[derive(Clone)]
pub(crate) struct ForwardAddressGetter {
    cluster_info: Arc<ClusterInfo>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    sharable_banks: SharableBanks,
    alpenglow_slot_clock: SharedAlpenglowSlotClock,
}

impl ForwardAddressGetter {
    pub fn new(
        cluster_info: Arc<ClusterInfo>,
        leader_schedule_cache: Arc<LeaderScheduleCache>,
        sharable_banks: SharableBanks,
        alpenglow_slot_clock: SharedAlpenglowSlotClock,
    ) -> Self {
        Self {
            cluster_info,
            leader_schedule_cache,
            sharable_banks,
            alpenglow_slot_clock,
        }
    }

    fn leader_pubkeys(&self, max_count: u64) -> impl Iterator<Item = Pubkey> + '_ {
        let current_slot = self.alpenglow_slot_clock.current_slot();
        let working_bank = self.sharable_banks.working();
        (0..max_count).filter_map(move |i| {
            let target_slot = current_slot?
                .checked_add(FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET)?
                .checked_add(i.saturating_mul(NUM_CONSECUTIVE_LEADER_SLOTS.get() as u64))?;
            self.leader_schedule_cache
                .slot_leader_at(target_slot, Some(&working_bank))
                .map(|leader| leader.id)
        })
    }

    /// Appends transaction forwarding addresses to `leaders`.
    fn transaction_forwarding_addresses(&self, max_count: u64, leaders: &mut Vec<SocketAddr>) {
        leaders.extend(self.leader_pubkeys(max_count).filter_map(|leader_pubkey| {
            self.cluster_info
                .lookup_contact_info(&leader_pubkey, |node| node.tpu_forwards(Protocol::QUIC))?
                .filter(|addr| self.cluster_info.socket_addr_space().check(addr))
        }));
    }
}

/// [`SpawnForwardingStageResult`] contains the result of spawning the
/// [`ForwardingStage`], including the background task handle and a shared
/// notifier for client address updates.
pub(crate) struct SpawnForwardingStageResult {
    pub join_handle: JoinHandle<()>,
    pub client_updater: Arc<dyn NotifyKeyUpdate + Send + Sync>,
}

pub(crate) fn spawn_forwarding_stage(
    receiver: Receiver<BankingPacketBatch>,
    tpu_forwarding_client_config: ForwardingClientConfig<'_>,
    sharable_banks: SharableBanks,
    forward_address_getter: ForwardAddressGetter,
) -> SpawnForwardingStageResult {
    let ForwardingClientConfig {
        stake_identity,
        tpu_client_sockets,
        runtime_handle,
        cancel,
        node_multihoming,
    } = tpu_forwarding_client_config;

    // Create TPU clients for each socket provided.
    // Number of clients is same as number of bind IP addresses.
    let transaction_clients: Box<[TpuClientNextClient]> = tpu_client_sockets
        .into_vec()
        .into_iter()
        .map(|socket| {
            TpuClientNextClient::new(
                runtime_handle.clone(),
                forward_address_getter.clone(),
                Some(stake_identity),
                socket,
                cancel.clone(),
            )
        })
        .collect();
    let forwarding_stage = ForwardingStage::new(
        receiver,
        transaction_clients.clone(),
        sharable_banks,
        Some(node_multihoming.bind_ip_addrs.clone()),
    );
    SpawnForwardingStageResult {
        join_handle: Builder::new()
            .name("solFwdStage".to_string())
            .spawn(move || forwarding_stage.run())
            .unwrap(),
        client_updater: Arc::new(UpdateHandles(transaction_clients))
            as Arc<dyn NotifyKeyUpdate + Send + Sync>,
    }
}

/// Local struct to be able to update keys on all clients at once
struct UpdateHandles(Box<[TpuClientNextClient]>);
impl NotifyKeyUpdate for UpdateHandles {
    fn update_key(&self, key: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        self.0.iter().try_for_each(|client| client.update_key(key))
    }
}

struct ForwardingStage<Client: ForwardingClient> {
    receiver: Receiver<BankingPacketBatch>,
    packet_container: PacketContainer,
    sharable_banks: SharableBanks,
    clients: Box<[Client]>,
    data_budget: TokenBucket,
    metrics: ForwardingStageMetrics,
    bind_ip_addrs: Option<Arc<BindIpAddrs>>,
}

impl<Client: ForwardingClient> ForwardingStage<Client> {
    fn new(
        receiver: Receiver<BankingPacketBatch>,
        clients: Box<[Client]>,
        sharable_banks: SharableBanks,
        bind_ip_addrs: Option<Arc<BindIpAddrs>>,
    ) -> Self {
        let data_budget = TokenBucket::new(
            MAX_BYTES_PER_SECOND,
            MAX_BYTES_PER_SECOND,
            MAX_BYTES_PER_SECOND as f64,
        );
        Self {
            receiver,
            packet_container: PacketContainer::with_capacity(4 * 4096),
            sharable_banks,
            clients,
            data_budget,
            metrics: ForwardingStageMetrics::default(),
            bind_ip_addrs,
        }
    }

    /// Runs `ForwardingStage`'s main loop, to receive, order, and forward packets.
    fn run(mut self) {
        loop {
            let root_bank = self.sharable_banks.root();
            if !self.receive_and_buffer(&root_bank) {
                break;
            }
            self.forward_buffered_packets();
            self.metrics.maybe_report();
        }
    }

    /// Receive packets from previous stage and insert them into the buffer.
    fn receive_and_buffer(&mut self, bank: &Bank) -> bool {
        // Timeout is long enough to receive packets but not too long that we
        // forward infrequently.
        const TIMEOUT: Duration = Duration::from_millis(10);

        let now = Instant::now();
        match self.receiver.recv_timeout(TIMEOUT) {
            Ok(packet_batches) => {
                self.metrics.did_something = true;
                self.buffer_packet_batches(packet_batches, bank);

                // Drain the channel up to timeout
                while now.elapsed() < TIMEOUT {
                    match self.receiver.try_recv() {
                        Ok(packet_batches) => self.buffer_packet_batches(packet_batches, bank),
                        Err(_) => break,
                    }
                }

                true
            }
            Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => false,
        }
    }

    /// Insert received packets into the packet container.
    fn buffer_packet_batches(&mut self, packet_batch: BankingPacketBatch, bank: &Bank) {
        let sanitize_config = sanitize_config();
        for packet in packet_batch
            .iter()
            .filter(|p| initial_packet_meta_filter(p.meta()))
        {
            let Some(packet_data) = packet.data(..) else {
                unreachable!(
                    "packet.meta().discard() was already checked. If not discarded, packet MUST \
                     have data"
                );
            };

            self.metrics.packets_received += 1;

            // Perform basic sanitization checks and calculate priority.
            // If any steps fail, drop the packet.
            let Some(priority) =
                SanitizedTransactionView::try_new_sanitized(packet_data, &sanitize_config)
                    .map_err(|_| ())
                    .and_then(|transaction| {
                        RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
                            transaction,
                            MessageHash::Compute,
                        )
                        .map_err(|_| ())
                    })
                    .ok()
                    .and_then(|transaction| calculate_priority(&transaction, bank))
            else {
                self.metrics.packets_dropped_on_receive += 1;
                continue;
            };

            // If at capacity, check lowest priority item.
            if self.packet_container.is_full() {
                let min_priority = self.packet_container.min_priority().expect("not empty");
                // If priority of current packet is not higher than the min
                // drop the current packet.
                if min_priority >= priority {
                    self.metrics.packets_dropped_on_capacity += 1;
                    continue;
                }

                self.packet_container.pop_min().expect("not empty");
                self.metrics.packets_dropped_on_capacity += 1;
            }

            self.packet_container.insert(packet.clone(), priority);
        }
    }

    /// Forwards packets that have been buffered. This will loop through all
    /// packets. If the data budget is exceeded then remaining packets are
    /// dropped.
    fn forward_buffered_packets(&mut self) {
        self.metrics.did_something |= !self.packet_container.is_empty();

        let mut batch = Vec::with_capacity(FORWARD_BATCH_SIZE);

        // determine the client to use for next batch based on current active interface
        // use primary interface bind (index 0) if not in multihoming context.
        let active_client = {
            let active_index = self
                .bind_ip_addrs
                .as_ref()
                .map(|binds| binds.active_index())
                .unwrap_or(0);
            &self.clients[active_index]
        };
        // Loop through packets creating batches of packets to forward.
        while let Some(packet) = self.packet_container.pop_max() {
            // If it exceeds our data-budget, drop.
            if self
                .data_budget
                .consume_tokens(packet.meta().size as u64)
                .is_err()
            {
                self.metrics.packets_dropped_on_data_budget += 1;
                continue;
            }

            let packet_data_vec = packet.data(..).expect("packet has data").to_vec();

            batch.push(packet_data_vec);
            send_batch_if_full(
                &mut batch,
                active_client,
                &mut self.metrics.packets_forwarded,
                &mut self.metrics.packets_dropped_on_send,
            );
        }

        // Send out remaining packets
        if !batch.is_empty() {
            let num_packets = batch.len();
            self.metrics.packets_forwarded += num_packets;
            if active_client.send_transactions_in_batch(batch).is_err() {
                self.metrics.packets_dropped_on_send += num_packets;
            }
        }
    }
}

/// [`ForwardingClientError`] enum represents failure when sending transactions
/// over the network.
#[derive(Debug)]
enum ForwardingClientError {
    /// Failed to send transaction to the provided host.
    Failed,
}

impl From<TransportError> for ForwardingClientError {
    fn from(_err: TransportError) -> Self {
        ForwardingClientError::Failed
    }
}

/// [`ForwardingClient`] trait defines a generic interface for clients that can
/// forward transactions to other validators.
trait ForwardingClient: Send + Sync + 'static {
    /// Sends a batch of serialized transactions to the currently configured
    /// address.
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
    ) -> Result<(), ForwardingClientError>;
}

impl LeaderUpdater for ForwardAddressGetter {
    fn next_leaders(&mut self, lookahead_slots: usize, leaders: &mut Vec<SocketAddr>) {
        self.transaction_forwarding_addresses(lookahead_slots as u64, leaders);
    }
}

#[derive(Clone)]
struct TpuClientNextClient {
    sender: mpsc::Sender<WireTransaction>,
    update_certificate_sender: watch::Sender<Option<StakeIdentity>>,
}

const METRICS_REPORTING_INTERVAL: Duration = Duration::from_secs(3);

impl TpuClientNextClient {
    fn new(
        runtime_handle: tokio::runtime::Handle,
        forward_address_getter: ForwardAddressGetter,
        stake_identity: Option<&Keypair>,
        bind_socket: UdpSocket,
        cancel: CancellationToken,
    ) -> Self {
        // For now use large channel, the more suitable size to be found later.
        let (sender, receiver) = mpsc::channel(SCHEDULER_CHANNEL_CAPACITY);
        let leader_updater = forward_address_getter;

        let config = Self::create_config(bind_socket, stake_identity);
        let (update_certificate_sender, update_certificate_receiver) = watch::channel(None);
        let scheduler: ConnectionWorkersScheduler = ConnectionWorkersScheduler::new(
            Box::new(leader_updater),
            receiver,
            update_certificate_receiver,
            cancel.clone(),
        );
        // leaking handle to this task, as it will run until the cancel signal is received
        runtime_handle.spawn(scheduler.get_stats().report_to_influxdb(
            "forwarding-stage-tpu-client",
            METRICS_REPORTING_INTERVAL,
            cancel,
        ));
        let _handle = runtime_handle.spawn(scheduler.run(config));
        Self {
            sender,
            update_certificate_sender,
        }
    }

    fn create_config(
        bind_socket: UdpSocket,
        stake_identity: Option<&Keypair>,
    ) -> ConnectionWorkersSchedulerConfig {
        ConnectionWorkersSchedulerConfig {
            bind: BindTarget::Socket(bind_socket),
            stake_identity: stake_identity.map(StakeIdentity::new),
            // Cache size of 128 covers all nodes above the P90 slot count threshold,
            // which together account for ~75% of total slots in the epoch.
            num_connections: NonZeroUsize::new(128).unwrap(),
            worker_channel_size: WORKER_CHANNEL_CAPACITY,
            max_reconnect_attempts: 4,
            // Send to the next leader only, but verify that connections exist
            // for the leaders of the next `4 * NUM_CONSECUTIVE_SLOTS`.
            leaders_fanout: Fanout {
                send: 1,
                connect: 4,
            },
            override_initial_congestion_window: None,
        }
    }
}

impl ForwardingClient for TpuClientNextClient {
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
    ) -> Result<(), ForwardingClientError> {
        let permits = self
            .sender
            .try_reserve_many(wire_transactions.len())
            .map_err(|_err| ForwardingClientError::Failed)?;
        for (permit, wire_transaction) in permits.zip(wire_transactions) {
            permit.send(wire_transaction.into());
        }
        Ok(())
    }
}

impl NotifyKeyUpdate for TpuClientNextClient {
    fn update_key(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let stake_identity = StakeIdentity::new(identity);
        self.update_certificate_sender
            .send(Some(stake_identity))
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
    }
}

/// Calculate priority for a transaction:
///
/// The priority is calculated as:
/// P = R / (1 + C)
/// where P is the priority, R is the reward,
/// and C is the cost towards block-limits.
///
/// Current minimum costs are on the order of several hundred,
/// so the denominator is effectively C, and the +1 is simply
/// to avoid any division by zero due to a bug - these costs
/// are estimate by the cost-model and are not direct
/// from user input. They should never be zero.
/// Any difference in the prioritization is negligible for
/// the current transaction costs.
fn calculate_priority(
    transaction: &RuntimeTransaction<SanitizedTransactionView<&[u8]>>,
    bank: &Bank,
) -> Option<u64> {
    let transaction_configuration = transaction
        .transaction_configuration(&bank.feature_set)
        .ok()?;

    // Manually estimate fee here since currently interface doesn't allow a on SVM type.
    // Doesn't need to be 100% accurate so long as close and consistent.
    let prioritization_fee = transaction_configuration.priority_fee_lamports;
    let signature_details = transaction.signature_details();
    let signature_fee = signature_details
        .total_signatures()
        .saturating_mul(bank.fee_structure().lamports_per_signature);
    let fee_details = FeeDetails::new(signature_fee, prioritization_fee);

    let reward = bank
        .calculate_reward_and_burn_fee_details(&CollectorFeeDetails::from(fee_details))
        .get_deposit();

    let cost = CostModel::estimate_cost(
        transaction,
        transaction.program_instructions_iter(),
        transaction.num_requested_write_locks(),
        &bank.feature_set,
    );

    // We need a multiplier here to avoid rounding down too aggressively.
    // For many transactions, the cost will be greater than the fees in terms of raw lamports.
    // For the purposes of calculating prioritization, we multiply the fees by a large number so that
    // the cost is a small fraction.
    // An offset of 1 is used in the denominator to explicitly avoid division by zero.
    const MULTIPLIER: u64 = 1_000_000;
    Some(
        MULTIPLIER
            .saturating_mul(reward)
            .wrapping_div(cost.sum().saturating_add(1)),
    )
}

fn send_batch_if_full(
    batch: &mut Vec<Vec<u8>>,
    client: &impl ForwardingClient,
    forwarded_counter: &mut usize,
    dropped_counter: &mut usize,
) {
    if batch.len() == FORWARD_BATCH_SIZE {
        *forwarded_counter += batch.len();

        let mut swap_batch = Vec::with_capacity(FORWARD_BATCH_SIZE);
        std::mem::swap(batch, &mut swap_batch);

        if client.send_transactions_in_batch(swap_batch).is_err() {
            *dropped_counter += FORWARD_BATCH_SIZE;
        }
    }
}

struct ForwardingStageMetrics {
    last_reported: Instant,
    did_something: bool,
    packets_received: usize,
    packets_dropped_on_receive: usize,
    packets_dropped_on_capacity: usize,
    packets_dropped_on_data_budget: usize,
    packets_forwarded: usize,
    packets_dropped_on_send: usize,
}

impl ForwardingStageMetrics {
    fn maybe_report(&mut self) {
        const REPORTING_INTERVAL: Duration = Duration::from_secs(1);

        if self.last_reported.elapsed() > REPORTING_INTERVAL {
            // Reset time and all counts.
            let metrics = core::mem::take(self);

            // Only report if something happened.
            if !metrics.did_something {
                return;
            }

            datapoint_info!(
                "forwarding_stage",
                ("packets_received", metrics.packets_received, i64),
                (
                    "packets_dropped_on_receive",
                    metrics.packets_dropped_on_receive,
                    i64
                ),
                (
                    "packets_dropped_on_capacity",
                    metrics.packets_dropped_on_capacity,
                    i64
                ),
                (
                    "packets_dropped_on_data_budget",
                    metrics.packets_dropped_on_data_budget,
                    i64
                ),
                ("packets_forwarded", metrics.packets_forwarded, i64),
                (
                    "packets_dropped_on_send",
                    metrics.packets_dropped_on_send,
                    i64
                ),
            );
        }
    }
}

impl Default for ForwardingStageMetrics {
    fn default() -> Self {
        Self {
            last_reported: Instant::now(),
            did_something: false,
            packets_received: 0,
            packets_dropped_on_receive: 0,
            packets_dropped_on_capacity: 0,
            packets_dropped_on_data_budget: 0,
            packets_forwarded: 0,
            packets_dropped_on_send: 0,
        }
    }
}

fn initial_packet_meta_filter(meta: &packet::Meta) -> bool {
    !meta.discard() && !meta.forwarded() && meta.is_from_staked_node()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crossbeam_channel::bounded,
        packet::PacketFlags,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_perf::packet::{BytesPacket, PacketBatch},
        solana_pubkey::Pubkey,
        solana_runtime::genesis_utils::create_genesis_config,
        solana_system_transaction as system_transaction,
        std::sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    pub struct MockClient {
        packets: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl MockClient {
        pub fn new() -> Self {
            Self {
                packets: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn get_packets(&self) -> Vec<Vec<u8>> {
            self.packets.lock().unwrap().clone()
        }
    }

    impl ForwardingClient for MockClient {
        fn send_transactions_in_batch(
            &self,
            wire_transactions: Vec<Vec<u8>>,
        ) -> Result<(), ForwardingClientError> {
            self.packets.lock().unwrap().extend(wire_transactions);
            Ok(())
        }
    }

    fn meta_with_flags(packet_flags: PacketFlags) -> packet::Meta {
        let mut meta = packet::Meta::default();
        meta.flags = packet_flags;
        meta
    }

    fn simple_transfer_with_flags(packet_flags: PacketFlags) -> BytesPacket {
        let transaction = system_transaction::transfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            Hash::default(),
        );
        let mut packet = BytesPacket::from_data(&transaction).unwrap();
        packet.meta_mut().flags = packet_flags;
        packet
    }

    #[test]
    fn test_initial_packet_meta_filter() {
        assert!(!initial_packet_meta_filter(&meta_with_flags(
            PacketFlags::empty()
        )));
        assert!(initial_packet_meta_filter(&meta_with_flags(
            PacketFlags::FROM_STAKED_NODE
        )));
        assert!(!initial_packet_meta_filter(&meta_with_flags(
            PacketFlags::DISCARD
        )));
        assert!(!initial_packet_meta_filter(&meta_with_flags(
            PacketFlags::FORWARDED
        )));
        assert!(!initial_packet_meta_filter(&meta_with_flags(
            PacketFlags::FROM_STAKED_NODE | PacketFlags::DISCARD
        )));
    }

    #[test]
    fn test_forwarding() {
        let (packet_batch_sender, packet_batch_receiver) = bounded(1024);

        let (_bank, bank_forks) =
            Bank::new_with_bank_forks_for_tests(&create_genesis_config(1).genesis_config);
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let mock_client = MockClient::new();
        let mut forwarding_stage = ForwardingStage::new(
            packet_batch_receiver,
            Box::new([mock_client.clone()]),
            sharable_banks,
            None,
        );

        // Send packet batches.
        let packets = BankingPacketBatch::new(PacketBatch::from(vec![
            simple_transfer_with_flags(PacketFlags::FROM_STAKED_NODE),
            simple_transfer_with_flags(PacketFlags::FROM_STAKED_NODE | PacketFlags::DISCARD),
            simple_transfer_with_flags(PacketFlags::FROM_STAKED_NODE | PacketFlags::FORWARDED),
        ]));
        packet_batch_sender.send(packets.clone()).unwrap();

        let bank = forwarding_stage.sharable_banks.root();
        forwarding_stage.receive_and_buffer(&bank);
        if !packet_batch_sender.is_empty() {
            forwarding_stage.receive_and_buffer(&bank);
        }
        forwarding_stage.forward_buffered_packets();

        assert_eq!(forwarding_stage.metrics.packets_forwarded, 1);

        let wired_txs = mock_client.get_packets();
        assert_eq!(wired_txs.len(), 1);
        assert_eq!(wired_txs[0], packets.first().unwrap().data(..).unwrap());
    }
}
