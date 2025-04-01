//! `ForwardingStage` is a stage parallel to `BankingStage` that forwards
//! packets to a node that is or will be leader soon.

use {
    crate::next_leader::next_leaders,
    agave_banking_stage_ingress_types::BankingPacketBatch,
    agave_transaction_view::transaction_view::SanitizedTransactionView,
    crossbeam_channel::{Receiver, RecvTimeoutError},
    packet_container::PacketContainer,
    solana_client::connection_cache::ConnectionCache,
    solana_connection_cache::client_connection::ClientConnection,
    solana_cost_model::cost_model::CostModel,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::Protocol},
    solana_net_utils::bind_to_unspecified,
    solana_perf::data_budget::DataBudget,
    solana_poh::poh_recorder::PohRecorder,
    solana_runtime::{
        bank::{Bank, CollectorFeeDetails},
        root_bank_cache::RootBankCache,
    },
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_meta::StaticMeta,
    },
    solana_sdk::{
        fee::{FeeBudgetLimits, FeeDetails},
        packet,
        transaction::MessageHash,
    },
    solana_streamer::sendmmsg::batch_send,
    std::{
        net::{SocketAddr, UdpSocket},
        sync::{Arc, RwLock},
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

mod packet_container;

/// Value chosen because it was used historically, at some point
/// was found to be optimal. If we need to improve performance
/// this should be evaluated with new stage.
const FORWARD_BATCH_SIZE: usize = 128;

/// How far ahead to look in the leader schedule when determining forwarding
/// addresses. The unit is `NUM_CONSECUTIVE_LEADER_SLOTS`.
///
/// This lookahead is needed because the immediate next leader might not have
/// shared their forwarding ports. In such cases, we skip them and attempt to
/// forward to the next available leader (up to this limit).
///
/// The value is chosen to ensure that the likelihood of the same leader occupying
/// all lookahead slots is negligible.
const NUM_LOOKAHEAD_LEADERS: u64 = 3;

/// [`ForwardAddressGetter`] provides helper methods for retrieving forwarding
/// addresses for both vote and non-vote transactions.
#[derive(Clone)]
pub(crate) struct ForwardAddressGetter {
    cluster_info: Arc<ClusterInfo>,
    poh_recorder: Arc<RwLock<PohRecorder>>,
}

impl ForwardAddressGetter {
    pub fn new(cluster_info: Arc<ClusterInfo>, poh_recorder: Arc<RwLock<PohRecorder>>) -> Self {
        Self {
            cluster_info,
            poh_recorder,
        }
    }

    /// Returns a list of forwarding addresses for non-vote transactions.
    fn get_non_vote_forwarding_addresses(
        &self,
        max_count: u64,
        protocol: Protocol,
    ) -> Vec<SocketAddr> {
        next_leaders(&self.cluster_info, &self.poh_recorder, max_count, |node| {
            node.tpu_forwards(protocol)
        })
    }

    /// Returns the TPU vote forwarding address of the next leader, if
    /// available.
    fn get_vote_forwarding_addresses(&self, max_count: u64) -> Vec<SocketAddr> {
        next_leaders(&self.cluster_info, &self.poh_recorder, max_count, |node| {
            node.tpu_vote(Protocol::UDP)
        })
    }
}

/// [`ForwardingClient`] trait defines a generic interface for clients that can
/// forward transactions to other validators.
trait ForwardingClient: Send + Sync + 'static {
    /// Updates the internal forwarding address, if needed.
    ///
    /// Returns `true` if a valid address was set, or `false` if no forwarding
    /// address is currently available.
    fn update_address(&mut self) -> bool;

    /// Sends a batch of serialized transactions to the currently configured
    /// address.
    fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>);
}

struct VoteClient {
    udp_socket: UdpSocket,
    forward_address_getter: ForwardAddressGetter,
    current_address: Option<SocketAddr>,
}

impl VoteClient {
    fn new(forward_address_getter: ForwardAddressGetter) -> Self {
        Self {
            udp_socket: bind_to_unspecified().unwrap(),
            forward_address_getter,
            current_address: None,
        }
    }
}

impl ForwardingClient for VoteClient {
    fn update_address(&mut self) -> bool {
        let node_addresses = self
            .forward_address_getter
            .get_vote_forwarding_addresses(NUM_LOOKAHEAD_LEADERS);
        if node_addresses.is_empty() {
            self.current_address = None;
        } else {
            self.current_address = Some(node_addresses[0]);
        }
        self.current_address.is_some()
    }

    fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>) {
        let Some(current_address) = self.current_address else {
            panic!("current_address should be updated before send_batch call");
        };
        let batch_with_addresses = wire_transactions
            .iter()
            .map(|bytes| (bytes, current_address));
        let _res = batch_send(&self.udp_socket, batch_with_addresses);
    }
}

#[derive(Clone)]
struct ConnectionCacheClient {
    connection_cache: Arc<ConnectionCache>,
    forward_address_getter: ForwardAddressGetter,
    current_address: Option<SocketAddr>,
}

impl ConnectionCacheClient {
    fn new(
        connection_cache: Arc<ConnectionCache>,
        forward_address_getter: ForwardAddressGetter,
    ) -> Self {
        Self {
            connection_cache,
            forward_address_getter,
            current_address: None,
        }
    }
}

impl ForwardingClient for ConnectionCacheClient {
    fn update_address(&mut self) -> bool {
        let node_addresses = self
            .forward_address_getter
            .get_non_vote_forwarding_addresses(
                NUM_LOOKAHEAD_LEADERS,
                self.connection_cache.protocol(),
            );
        if node_addresses.is_empty() {
            self.current_address = None;
        } else {
            self.current_address = Some(node_addresses[0]);
        }
        self.current_address.is_some()
    }

    fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>) {
        assert!(
            self.current_address.is_some(),
            "current_address should be updated before send_batch call."
        );
        let conn = self
            .connection_cache
            .get_connection(&self.current_address.unwrap());
        let _res = conn.send_data_batch_async(wire_transactions);
    }
}

pub(crate) fn spawn_forwarding_stage(
    receiver: Receiver<(BankingPacketBatch, bool)>,
    connection_cache: Arc<ConnectionCache>,
    root_bank_cache: RootBankCache,
    forward_address_getter: ForwardAddressGetter,
    data_budget: DataBudget,
) -> JoinHandle<()> {
    let vote_client = VoteClient::new(forward_address_getter.clone());

    let non_vote_client = ConnectionCacheClient::new(connection_cache, forward_address_getter);
    let forwarding_stage = ForwardingStage::new(
        receiver,
        vote_client,
        non_vote_client.clone(),
        root_bank_cache,
        data_budget,
    );
    Builder::new()
        .name("solFwdStage".to_string())
        .spawn(move || forwarding_stage.run())
        .unwrap()
}

/// Forwards packets to current/next leader.
struct ForwardingStage<VoteClient: ForwardingClient, NonVoteClient: ForwardingClient> {
    receiver: Receiver<(BankingPacketBatch, bool)>,
    packet_container: PacketContainer,
    root_bank_cache: RootBankCache,
    vote_client: VoteClient,
    non_vote_client: NonVoteClient,
    data_budget: DataBudget,
    metrics: ForwardingStageMetrics,
}

impl<VoteClient: ForwardingClient, NonVoteClient: ForwardingClient>
    ForwardingStage<VoteClient, NonVoteClient>
{
    fn new(
        receiver: Receiver<(BankingPacketBatch, bool)>,
        vote_client: VoteClient,
        non_vote_client: NonVoteClient,
        root_bank_cache: RootBankCache,
        data_budget: DataBudget,
    ) -> Self {
        Self {
            receiver,
            packet_container: PacketContainer::with_capacity(4 * 4096),
            root_bank_cache,
            non_vote_client,
            vote_client,
            data_budget,
            metrics: ForwardingStageMetrics::default(),
        }
    }

    /// Runs `ForwardingStage`'s main loop, to receive, order, and forward packets.
    fn run(mut self) {
        loop {
            let root_bank = self.root_bank_cache.root_bank();
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
            Ok((packet_batches, tpu_vote_batch)) => {
                self.metrics.did_something = true;
                self.buffer_packet_batches(packet_batches, tpu_vote_batch, bank);

                // Drain the channel up to timeout
                let timed_out = loop {
                    if now.elapsed() >= TIMEOUT {
                        break true;
                    }
                    match self.receiver.try_recv() {
                        Ok((packet_batches, tpu_vote_batch)) => {
                            self.buffer_packet_batches(packet_batches, tpu_vote_batch, bank)
                        }
                        Err(_) => break false,
                    }
                };

                // If timeout was reached, prevent backup by draining all
                // packets in the channel.
                if timed_out {
                    warn!("ForwardingStage is backed up, dropping packets");
                    while let Ok((packet_batch, _)) = self.receiver.try_recv() {
                        self.metrics.dropped_on_timeout +=
                            packet_batch.iter().map(|b| b.len()).sum::<usize>();
                    }
                }

                true
            }
            Err(RecvTimeoutError::Timeout) => true,
            Err(RecvTimeoutError::Disconnected) => false,
        }
    }

    /// Insert received packets into the packet container.
    fn buffer_packet_batches(
        &mut self,
        packet_batches: BankingPacketBatch,
        is_tpu_vote_batch: bool,
        bank: &Bank,
    ) {
        for batch in packet_batches.iter() {
            for packet in batch
                .iter()
                .filter(|p| initial_packet_meta_filter(p.meta()))
            {
                let Some(packet_data) = packet.data(..) else {
                    unreachable!(
                        "packet.meta().discard() was already checked. \
                         If not discarded, packet MUST have data"
                    );
                };

                let vote_count = usize::from(is_tpu_vote_batch);
                let non_vote_count = usize::from(!is_tpu_vote_batch);

                self.metrics.votes_received += vote_count;
                self.metrics.non_votes_received += non_vote_count;

                // Perform basic sanitization checks and calculate priority.
                // If any steps fail, drop the packet.
                let Some(priority) = SanitizedTransactionView::try_new_sanitized(packet_data)
                    .map_err(|_| ())
                    .and_then(|transaction| {
                        RuntimeTransaction::<SanitizedTransactionView<_>>::try_from(
                            transaction,
                            MessageHash::Compute,
                            Some(packet.meta().is_simple_vote_tx()),
                        )
                        .map_err(|_| ())
                    })
                    .ok()
                    .and_then(|transaction| calculate_priority(&transaction, bank))
                else {
                    self.metrics.votes_dropped_on_receive += vote_count;
                    self.metrics.non_votes_dropped_on_receive += non_vote_count;
                    continue;
                };

                // If at capacity, check lowest priority item.
                if self.packet_container.is_full() {
                    let min_priority = self.packet_container.min_priority().expect("not empty");
                    // If priority of current packet is not higher than the min
                    // drop the current packet.
                    if min_priority >= priority {
                        self.metrics.votes_dropped_on_capacity += vote_count;
                        self.metrics.non_votes_dropped_on_capacity += non_vote_count;
                        continue;
                    }

                    let dropped_packet = self
                        .packet_container
                        .pop_and_remove_min()
                        .expect("not empty");
                    self.metrics.votes_dropped_on_capacity +=
                        usize::from(dropped_packet.meta().is_simple_vote_tx());
                    self.metrics.non_votes_dropped_on_capacity +=
                        usize::from(!dropped_packet.meta().is_simple_vote_tx());
                }

                self.packet_container.insert(packet.clone(), priority);
            }
        }
    }

    /// Forwards packets that have been buffered. This will loop through all
    /// packets. If the data budget is exceeded then remaining packets are
    /// dropped.
    fn forward_buffered_packets(&mut self) {
        self.metrics.did_something |= !self.packet_container.is_empty();
        self.refresh_data_budget();

        // Get forwarding addresses otherwise return now.
        if !self.vote_client.update_address() || !self.non_vote_client.update_address() {
            return;
        };

        let mut non_vote_batch = Vec::with_capacity(FORWARD_BATCH_SIZE);
        let mut vote_batch = Vec::with_capacity(FORWARD_BATCH_SIZE);

        // Loop through packets creating batches of packets to forward.
        while let Some(packet) = self.packet_container.pop_and_remove_max() {
            // If it exceeds our data-budget, drop.
            if !self.data_budget.take(packet.meta().size) {
                self.metrics.votes_dropped_on_data_budget +=
                    usize::from(packet.meta().is_simple_vote_tx());
                self.metrics.non_votes_dropped_on_data_budget +=
                    usize::from(!packet.meta().is_simple_vote_tx());
                continue;
            }

            let packet_data_vec = packet.data(..).expect("packet has data").to_vec();

            if packet.meta().is_simple_vote_tx() {
                vote_batch.push(packet_data_vec);
                if vote_batch.len() == FORWARD_BATCH_SIZE {
                    self.metrics.votes_forwarded += vote_batch.len();

                    let mut batch = Vec::with_capacity(FORWARD_BATCH_SIZE);
                    core::mem::swap(&mut batch, &mut vote_batch);
                    self.vote_client.send_transactions_in_batch(batch);
                }
            } else {
                non_vote_batch.push(packet_data_vec);
                if non_vote_batch.len() == FORWARD_BATCH_SIZE {
                    self.metrics.non_votes_forwarded += non_vote_batch.len();

                    let mut batch = Vec::with_capacity(FORWARD_BATCH_SIZE);
                    core::mem::swap(&mut batch, &mut non_vote_batch);
                    self.non_vote_client.send_transactions_in_batch(batch);
                }
            }
        }

        // Send out remaining packets
        if !vote_batch.is_empty() {
            self.metrics.votes_forwarded += vote_batch.len();
            self.vote_client.send_transactions_in_batch(vote_batch);
        }
        if !non_vote_batch.is_empty() {
            self.metrics.non_votes_forwarded += non_vote_batch.len();
            self.non_vote_client
                .send_transactions_in_batch(non_vote_batch);
        }
    }

    /// Re-fill the data budget if enough time has passed
    fn refresh_data_budget(&self) {
        const INTERVAL_MS: u64 = 100;
        // 12 MB outbound limit per second
        const MAX_BYTES_PER_SECOND: usize = 12_000_000;
        const MAX_BYTES_PER_INTERVAL: usize = MAX_BYTES_PER_SECOND * INTERVAL_MS as usize / 1000;
        const MAX_BYTES_BUDGET: usize = MAX_BYTES_PER_INTERVAL * 5;
        self.data_budget.update(INTERVAL_MS, |bytes| {
            std::cmp::min(
                bytes.saturating_add(MAX_BYTES_PER_INTERVAL),
                MAX_BYTES_BUDGET,
            )
        });
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
    let compute_budget_limits = transaction
        .compute_budget_instruction_details()
        .sanitize_and_convert_to_compute_budget_limits(&bank.feature_set)
        .ok()?;
    let fee_budget_limits = FeeBudgetLimits::from(compute_budget_limits);

    // Manually estimate fee here since currently interface doesn't allow a on SVM type.
    // Doesn't need to be 100% accurate so long as close and consistent.
    let prioritization_fee = fee_budget_limits.prioritization_fee;
    let signature_details = transaction.signature_details();
    let signature_fee = signature_details
        .total_signatures()
        .saturating_mul(bank.fee_structure().lamports_per_signature);
    let fee_details = FeeDetails::new(signature_fee, prioritization_fee);

    let (reward, _burn) =
        bank.calculate_reward_and_burn_fee_details(&CollectorFeeDetails::from(fee_details));

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

struct ForwardingStageMetrics {
    last_reported: Instant,
    did_something: bool,

    votes_received: usize,
    votes_dropped_on_receive: usize,
    votes_dropped_on_capacity: usize,
    votes_dropped_on_data_budget: usize,
    votes_forwarded: usize,

    non_votes_received: usize,
    non_votes_dropped_on_receive: usize,
    non_votes_dropped_on_capacity: usize,
    non_votes_dropped_on_data_budget: usize,
    non_votes_forwarded: usize,

    dropped_on_timeout: usize,
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
                ("votes_received", metrics.votes_received, i64),
                (
                    "votes_dropped_on_receive",
                    metrics.votes_dropped_on_receive,
                    i64
                ),
                (
                    "votes_dropped_on_data_budget",
                    metrics.votes_dropped_on_data_budget,
                    i64
                ),
                ("votes_forwarded", metrics.votes_forwarded, i64),
                ("non_votes_received", metrics.non_votes_received, i64),
                (
                    "votes_dropped_on_receive",
                    metrics.votes_dropped_on_receive,
                    i64
                ),
                (
                    "votes_dropped_on_data_budget",
                    metrics.votes_dropped_on_data_budget,
                    i64
                ),
                ("votes_forwarded", metrics.votes_forwarded, i64),
            );
        }
    }
}

impl Default for ForwardingStageMetrics {
    fn default() -> Self {
        Self {
            last_reported: Instant::now(),
            did_something: false,
            votes_received: 0,
            votes_dropped_on_receive: 0,
            votes_dropped_on_capacity: 0,
            votes_dropped_on_data_budget: 0,
            votes_forwarded: 0,
            non_votes_received: 0,
            non_votes_dropped_on_receive: 0,
            non_votes_dropped_on_capacity: 0,
            non_votes_dropped_on_data_budget: 0,
            non_votes_forwarded: 0,
            dropped_on_timeout: 0,
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
        crossbeam_channel::unbounded,
        packet::PacketFlags,
        solana_perf::packet::{Packet, PacketBatch},
        solana_pubkey::Pubkey,
        solana_runtime::genesis_utils::create_genesis_config,
        solana_sdk::{hash::Hash, signature::Keypair, system_transaction},
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
        fn update_address(&mut self) -> bool {
            true
        }

        fn send_transactions_in_batch(&self, wire_transactions: Vec<Vec<u8>>) {
            self.packets.lock().unwrap().extend(wire_transactions);
        }
    }

    fn meta_with_flags(packet_flags: PacketFlags) -> packet::Meta {
        packet::Meta {
            flags: packet_flags,
            ..packet::Meta::default()
        }
    }

    fn simple_transfer_with_flags(packet_flags: PacketFlags) -> Packet {
        let transaction = system_transaction::transfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            Hash::default(),
        );
        let mut packet = Packet::from_data(None, &transaction).unwrap();
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
        let (packet_batch_sender, packet_batch_receiver) = unbounded();

        let (_bank, bank_forks) =
            Bank::new_with_bank_forks_for_tests(&create_genesis_config(1).genesis_config);
        let root_bank_cache = RootBankCache::new(bank_forks);
        let vote_mock_client = MockClient::new();
        let non_vote_mock_client = MockClient::new();
        let mut forwarding_stage = ForwardingStage::new(
            packet_batch_receiver,
            vote_mock_client.clone(),
            non_vote_mock_client.clone(),
            root_bank_cache,
            DataBudget::default(),
        );

        // Send packet batches.
        let non_vote_packets = BankingPacketBatch::new(vec![PacketBatch::new(vec![
            simple_transfer_with_flags(PacketFlags::FROM_STAKED_NODE),
            simple_transfer_with_flags(PacketFlags::FROM_STAKED_NODE | PacketFlags::DISCARD),
            simple_transfer_with_flags(PacketFlags::FROM_STAKED_NODE | PacketFlags::FORWARDED),
        ])]);
        let vote_packets = BankingPacketBatch::new(vec![PacketBatch::new(vec![
            simple_transfer_with_flags(PacketFlags::SIMPLE_VOTE_TX | PacketFlags::FROM_STAKED_NODE),
            simple_transfer_with_flags(
                PacketFlags::SIMPLE_VOTE_TX | PacketFlags::FROM_STAKED_NODE | PacketFlags::DISCARD,
            ),
            simple_transfer_with_flags(
                PacketFlags::SIMPLE_VOTE_TX
                    | PacketFlags::FROM_STAKED_NODE
                    | PacketFlags::FORWARDED,
            ),
        ])]);

        packet_batch_sender
            .send((non_vote_packets.clone(), false))
            .unwrap();
        packet_batch_sender
            .send((vote_packets.clone(), true))
            .unwrap();

        let bank = forwarding_stage.root_bank_cache.root_bank();
        forwarding_stage.receive_and_buffer(&bank);
        if !packet_batch_sender.is_empty() {
            forwarding_stage.receive_and_buffer(&bank);
        }
        forwarding_stage.forward_buffered_packets();

        assert_eq!(forwarding_stage.metrics.non_votes_forwarded, 1);
        assert_eq!(forwarding_stage.metrics.votes_forwarded, 1);

        let vote_wired_txs = vote_mock_client.get_packets();
        assert_eq!(vote_wired_txs.len(), 1);
        assert_eq!(vote_wired_txs[0], vote_packets[0][0].data(..).unwrap());

        let non_vote_wired_txs = non_vote_mock_client.get_packets();
        assert_eq!(non_vote_wired_txs.len(), 1);
        assert_eq!(
            non_vote_wired_txs[0],
            non_vote_packets[0][0].data(..).unwrap()
        );
    }
}
