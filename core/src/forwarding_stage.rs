//! `ForwardingStage` is a stage parallel to `BankingStage` that forwards
//! packets to a node that is or will be leader soon.

use {
    crate::{
        banking_stage::LikeClusterInfo,
        next_leader::{next_leader_tpu_vote, next_leaders},
    },
    agave_banking_stage_ingress_types::BankingPacketBatch,
    agave_transaction_view::transaction_view::SanitizedTransactionView,
    async_trait::async_trait,
    crossbeam_channel::{Receiver, RecvTimeoutError},
    packet_container::PacketContainer,
    solana_client::connection_cache::ConnectionCache,
    solana_connection_cache::client_connection::ClientConnection,
    solana_cost_model::cost_model::CostModel,
    solana_gossip::contact_info::Protocol,
    solana_net_utils::bind_to_unspecified,
    solana_perf::data_budget::DataBudget,
    solana_poh::poh_recorder::PohRecorder,
    solana_runtime::{bank::Bank, root_bank_cache::RootBankCache},
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_meta::StaticMeta,
    },
    solana_sdk::{
        fee::FeeBudgetLimits, packet, quic::NotifyKeyUpdate, signer::keypair::Keypair,
        transaction::MessageHash,
    },
    solana_streamer::sendmmsg::batch_send,
    solana_tpu_client_next::{
        connection_workers_scheduler::{ConnectionWorkersSchedulerConfig, Fanout},
        leader_updater::LeaderUpdater,
        transaction_batch::TransactionBatch,
        ConnectionWorkersScheduler, ConnectionWorkersSchedulerError,
    },
    std::{
        net::{Ipv4Addr, SocketAddr, UdpSocket},
        sync::{Arc, Mutex, RwLock},
        thread::{Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::{runtime::Handle as RuntimeHandle, sync::mpsc, task::JoinHandle as TokioJoinHandle},
    tokio_util::sync::CancellationToken,
};

mod packet_container;

/// Value chosen because it was used historically, at some point
/// was found to be optimal. If we need to improve performance
/// this should be evaluated with new stage.
const FORWARD_BATCH_SIZE: usize = 128;

/// TODO(klykov): add some documentation here
pub trait ForwardAddressGetter: Clone + Send + Sync + 'static {
    fn get_non_vote_forwarding_addresses(
        &self,
        max_count: u64,
        protocol: Protocol,
    ) -> Vec<Option<SocketAddr>>;
    fn get_vote_forwarding_addresses(&self) -> Option<SocketAddr>;
}

impl<T: LikeClusterInfo> ForwardAddressGetter for (T, Arc<RwLock<PohRecorder>>) {
    fn get_non_vote_forwarding_addresses(
        &self,
        max_count: u64,
        protocol: Protocol,
    ) -> Vec<Option<SocketAddr>> {
        next_leaders(&self.0, &self.1, max_count, |node| {
            node.tpu_forwards(protocol)
        })
    }
    fn get_vote_forwarding_addresses(&self) -> Option<SocketAddr> {
        next_leader_tpu_vote(&self.0, &self.1).map(|(_, s)| s)
    }
}

struct VoteClient<F: ForwardAddressGetter> {
    udp_socket: UdpSocket,
    forward_address_getter: F,
    current_address: Option<SocketAddr>,
}

impl<F: ForwardAddressGetter> VoteClient<F> {
    fn new(forward_address_getter: F) -> Self {
        Self {
            udp_socket: bind_to_unspecified().unwrap(),
            forward_address_getter,
            current_address: None,
        }
    }
}

impl<F: ForwardAddressGetter> ForwardingClient for VoteClient<F> {
    fn update_address(&mut self) -> bool {
        self.current_address = self.forward_address_getter.get_vote_forwarding_addresses();
        self.current_address.is_some()
    }

    fn send_batch(&self, input_batch: Vec<Vec<u8>>) {
        assert!(
            self.current_address.is_some(),
            "current_address should be updated before send_batch call."
        );
        let batch_with_addresses = input_batch
            .iter()
            .map(|bytes| (bytes, self.current_address.unwrap()));
        // TODO(klykov) Not really in favour of skipping these errors, need to think
        let _res = batch_send(&self.udp_socket, batch_with_addresses);
    }
}

/// [`ForwardingClientOption`] enum represents the available client types for TPU
/// communication:
/// * [`ConnectionCacheClient`]: Uses a shared [`ConnectionCache`] to manage
///       connections efficiently.
/// * [`TpuClientNextClient`]: Relies on the `tpu-client-next` crate and
///       requires a reference to a [`Keypair`].
pub enum ForwardingClientOption<'a> {
    ConnectionCache(Arc<ConnectionCache>),
    TpuClientNext((&'a Keypair, RuntimeHandle)),
}

pub trait ForwardingClient: Send + Sync + 'static {
    fn update_address(&mut self) -> bool;
    fn send_batch(&self, input_batch: Vec<Vec<u8>>);
}

#[derive(Clone)]
struct ConnectionCacheClient<F: ForwardAddressGetter> {
    connection_cache: Arc<ConnectionCache>,
    forward_address_getter: F,
    current_address: Option<SocketAddr>,
}

impl<F: ForwardAddressGetter> ConnectionCacheClient<F> {
    fn new(connection_cache: Arc<ConnectionCache>, forward_address_getter: F) -> Self {
        Self {
            connection_cache,
            forward_address_getter,
            current_address: None,
        }
    }
}

impl<F: ForwardAddressGetter> ForwardingClient for ConnectionCacheClient<F> {
    fn update_address(&mut self) -> bool {
        self.current_address = self
            .forward_address_getter
            .get_non_vote_forwarding_addresses(1, self.connection_cache.protocol())[0];
        self.current_address.is_some()
    }

    fn send_batch(&self, input_batch: Vec<Vec<u8>>) {
        assert!(
            self.current_address.is_some(),
            "current_address should be updated before send_batch call."
        );
        let conn = self
            .connection_cache
            .get_connection(&self.current_address.unwrap());
        let _res = conn.send_data_batch_async(input_batch);
    }
}

impl<F: ForwardAddressGetter> NotifyKeyUpdate for ConnectionCacheClient<F> {
    fn update_key(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        self.connection_cache.update_key(identity)
    }
}

struct ForwardingStageLeaderUpdater<F: ForwardAddressGetter> {
    pub forward_address_getter: F,
}

#[async_trait]
impl<F: ForwardAddressGetter> LeaderUpdater for ForwardingStageLeaderUpdater<F> {
    fn next_leaders(&mut self, lookahead_slots: usize) -> Vec<SocketAddr> {
        self.forward_address_getter
            .get_non_vote_forwarding_addresses(lookahead_slots as u64, Protocol::QUIC)
            .into_iter()
            .flatten()
            .collect()
    }

    async fn stop(&mut self) {}
}

type TpuClientJoinHandle =
    TokioJoinHandle<Result<ConnectionWorkersScheduler, ConnectionWorkersSchedulerError>>;

#[derive(Clone)]
struct TpuClientNextClient<F: ForwardAddressGetter> {
    runtime_handle: RuntimeHandle,
    sender: mpsc::Sender<TransactionBatch>,
    // This handle is needed to implement `NotifyKeyUpdate` trait. It's only
    // method takes &self and thus we need to wrap with Mutex.
    join_and_cancel: Arc<Mutex<(Option<TpuClientJoinHandle>, CancellationToken)>>,
    forward_address_getter: F,
}

const METRICS_REPORTING_INTERVAL: Duration = Duration::from_secs(3);

impl<F: ForwardAddressGetter> TpuClientNextClient<F> {
    fn new(
        runtime_handle: tokio::runtime::Handle,
        forward_address_getter: F,
        stake_identity: Option<&Keypair>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(16); // random number of now
        let cancel = CancellationToken::new();
        let leader_updater = ForwardingStageLeaderUpdater {
            forward_address_getter: forward_address_getter.clone(),
        };

        let config = Self::create_config(stake_identity, 1);
        let scheduler = ConnectionWorkersScheduler::new(Box::new(leader_updater), receiver);
        // leaking handle to this task, as it will run until the cancel signal is received
        runtime_handle.spawn(scheduler.get_stats().report_to_influxdb(
            "forwarding-stage-tpu-client",
            METRICS_REPORTING_INTERVAL,
            cancel.clone(),
        ));
        let handle = runtime_handle.spawn(scheduler.run(config, cancel.clone()));
        Self {
            runtime_handle,
            join_and_cancel: Arc::new(Mutex::new((Some(handle), cancel))),
            sender,
            forward_address_getter,
        }
    }

    fn create_config(
        stake_identity: Option<&Keypair>,
        leader_forward_count: usize,
    ) -> ConnectionWorkersSchedulerConfig {
        ConnectionWorkersSchedulerConfig {
            bind: SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0),
            stake_identity: stake_identity.map(Into::into),
            num_connections: 1,
            skip_check_transaction_age: true,
            worker_channel_size: 2,
            max_reconnect_attempts: 4,
            leaders_fanout: Fanout {
                send: leader_forward_count,
                connect: 4 * leader_forward_count,
            },
        }
    }

    async fn do_update_key(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let runtime_handle = self.runtime_handle.clone();
        let config = Self::create_config(Some(identity), 1);
        let handle = self.join_and_cancel.clone();

        let join_handle = {
            let Ok(mut lock) = handle.lock() else {
                return Err("TpuClientNext task panicked.".into());
            };
            let (handle, token) = std::mem::take(&mut *lock);
            token.cancel();
            handle
        };

        if let Some(join_handle) = join_handle {
            let Ok(result) = join_handle.await else {
                return Err("TpuClientNext task panicked.".into());
            };

            match result {
                Ok(scheduler) => {
                    let cancel = CancellationToken::new();
                    // leaking handle to this task, as it will run until the cancel signal is received
                    runtime_handle.spawn(scheduler.get_stats().report_to_influxdb(
                        "send-transaction-service-TPU-client",
                        METRICS_REPORTING_INTERVAL,
                        cancel.clone(),
                    ));
                    let join_handle = runtime_handle.spawn(scheduler.run(config, cancel.clone()));

                    let Ok(mut lock) = handle.lock() else {
                        return Err("TpuClientNext task panicked.".into());
                    };
                    *lock = (Some(join_handle), cancel);
                }
                Err(error) => {
                    return Err(Box::new(error));
                }
            }
        }
        Ok(())
    }
}

impl<F: ForwardAddressGetter> ForwardingClient for TpuClientNextClient<F> {
    fn update_address(&mut self) -> bool {
        // Updating the address is not strictly necessary because
        // `TpuClientNextClient` relies on `ForwardingStageLeaderUpdater`
        // to determine the correct leader for forwarding transactions.
        //
        // However, we still need to indicate that a next leader exists.
        // This is important because in `forward_buffered_packets`, packets
        // are only pop from the container and processed if `next_leader` is not `None`.
        self.forward_address_getter
            .get_non_vote_forwarding_addresses(1, Protocol::QUIC)
            .first()
            .is_some_and(|first| first.is_some())
    }

    fn send_batch(&self, input_batch: Vec<Vec<u8>>) {
        self.runtime_handle.spawn({
            let sender = self.sender.clone();
            async move {
                let res = sender.send(TransactionBatch::new(input_batch)).await;
                if res.is_err() {
                    warn!("Failed to send transaction to channel: it is closed.");
                }
            }
        });
    }
}

impl<F: ForwardAddressGetter> NotifyKeyUpdate for TpuClientNextClient<F> {
    fn update_key(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime_handle.block_on(self.do_update_key(identity))
    }
}

/// [`SpawnForwardingStageResult`] contains the result of spawning the
/// [`ForwardingStage`], including the background task handle and a shared
/// notifier for client address updates.
pub struct SpawnForwardingStageResult {
    pub join_handle: JoinHandle<()>,
    pub client_updater: Arc<dyn NotifyKeyUpdate + Send + Sync>,
}

pub fn spawn_forwarding_stage<F>(
    receiver: Receiver<(BankingPacketBatch, bool)>,
    client: ForwardingClientOption<'_>,
    root_bank_cache: RootBankCache,
    forward_address_getter: F,
    data_budget: DataBudget,
) -> SpawnForwardingStageResult
where
    F: ForwardAddressGetter,
{
    let vote_client = VoteClient::new(forward_address_getter.clone());
    match client {
        ForwardingClientOption::ConnectionCache(connection_cache) => {
            let non_vote_client =
                ConnectionCacheClient::new(connection_cache, forward_address_getter);
            let forwarding_stage: ForwardingStage<F, ConnectionCacheClient<F>> =
                ForwardingStage::new(
                    receiver,
                    vote_client,
                    non_vote_client.clone(),
                    root_bank_cache,
                    data_budget,
                );
            SpawnForwardingStageResult {
                join_handle: Builder::new()
                    .name("solFwdStage".to_string())
                    .spawn(move || forwarding_stage.run())
                    .unwrap(),
                client_updater: Arc::new(non_vote_client) as Arc<dyn NotifyKeyUpdate + Send + Sync>,
            }
        }
        ForwardingClientOption::TpuClientNext((stake_identity, runtime_handle)) => {
            let non_vote_client = TpuClientNextClient::new(
                runtime_handle,
                forward_address_getter,
                Some(stake_identity),
            );
            let forwarding_stage = ForwardingStage::new(
                receiver,
                vote_client,
                non_vote_client.clone(),
                root_bank_cache,
                data_budget,
            );
            SpawnForwardingStageResult {
                join_handle: Builder::new()
                    .name("solFwdStage".to_string())
                    .spawn(move || forwarding_stage.run())
                    .unwrap(),
                client_updater: Arc::new(non_vote_client) as Arc<dyn NotifyKeyUpdate + Send + Sync>,
            }
        }
    }
}

// TODO(klykov): If we remove ForwardAddressGetter and parametrize ForwardingStage with VoteClient,
// We can use in tests MockClient and MockVoteClient avoiding network at all.
// For that, you need to use ForwardingClient trait also for VoteClient.
pub struct ForwardingStage<F: ForwardAddressGetter, Client: ForwardingClient> {
    receiver: Receiver<(BankingPacketBatch, bool)>,
    packet_container: PacketContainer,

    root_bank_cache: RootBankCache,
    vote_client: VoteClient<F>,
    non_vote_client: Client,
    data_budget: DataBudget,

    metrics: ForwardingStageMetrics,
}

impl<F: ForwardAddressGetter, Client: ForwardingClient> ForwardingStage<F, Client> {
    fn new(
        receiver: Receiver<(BankingPacketBatch, bool)>,
        vote_client: VoteClient<F>,
        non_vote_client: Client,
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
                if vote_batch.len() == vote_batch.capacity() {
                    self.metrics.votes_forwarded += vote_batch.len();

                    let mut batch = Vec::with_capacity(FORWARD_BATCH_SIZE);
                    core::mem::swap(&mut batch, &mut vote_batch);
                    self.vote_client.send_batch(batch);
                }
            } else {
                non_vote_batch.push(packet_data_vec);
                if non_vote_batch.len() == non_vote_batch.capacity() {
                    self.metrics.non_votes_forwarded += non_vote_batch.len();

                    let mut batch = Vec::with_capacity(FORWARD_BATCH_SIZE);
                    core::mem::swap(&mut batch, &mut non_vote_batch);
                    self.non_vote_client.send_batch(batch);
                }
            }
        }

        // Send out remaining packets
        if !vote_batch.is_empty() {
            self.metrics.votes_forwarded += vote_batch.len();
            self.vote_client.send_batch(vote_batch);
        }
        if !non_vote_batch.is_empty() {
            self.metrics.non_votes_forwarded += non_vote_batch.len();
            self.non_vote_client.send_batch(non_vote_batch);
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
    let fee = signature_fee.saturating_add(prioritization_fee);

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
            .saturating_mul(fee)
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
        solana_net_utils::bind_to_unspecified,
        solana_perf::packet::{Packet, PacketBatch},
        solana_pubkey::Pubkey,
        solana_runtime::genesis_utils::create_genesis_config,
        solana_sdk::{hash::Hash, signature::Keypair, system_transaction},
    };

    /// Addresses to use for forwarding.
    #[derive(Clone)]
    pub struct ForwardingAddressesForTest {
        /// Forwarding address for TPU (non-votes)
        pub tpu: Option<SocketAddr>,
        /// Forwarding address for votes
        pub tpu_vote: Option<SocketAddr>,
    }

    impl ForwardAddressGetter for ForwardingAddressesForTest {
        fn get_non_vote_forwarding_addresses(
            &self,
            _max_count: u64,
            _protocol: Protocol,
        ) -> Vec<Option<SocketAddr>> {
            vec![self.tpu]
        }

        fn get_vote_forwarding_addresses(&self) -> Option<SocketAddr> {
            self.tpu_vote
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
        let vote_socket = bind_to_unspecified().unwrap();
        vote_socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let non_vote_socket = bind_to_unspecified().unwrap();
        non_vote_socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();

        let forwarding_addresses = ForwardingAddressesForTest {
            tpu_vote: Some(vote_socket.local_addr().unwrap()),
            tpu: Some(non_vote_socket.local_addr().unwrap()),
        };

        let (packet_batch_sender, packet_batch_receiver) = unbounded();
        let connection_cache = Arc::new(ConnectionCache::with_udp("connection_cache_test", 1));
        let non_vote_client =
            ConnectionCacheClient::new(connection_cache, forwarding_addresses.clone());
        let (_bank, bank_forks) =
            Bank::new_with_bank_forks_for_tests(&create_genesis_config(1).genesis_config);
        let root_bank_cache = RootBankCache::new(bank_forks);
        let vote_client = VoteClient::new(forwarding_addresses);
        let mut forwarding_stage = ForwardingStage::new(
            packet_batch_receiver,
            vote_client,
            non_vote_client,
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

        let recv_buffer = &mut [0; 1024];
        let (vote_packet_bytes, _) = vote_socket.recv_from(recv_buffer).unwrap();
        assert_eq!(
            &recv_buffer[..vote_packet_bytes],
            vote_packets[0][0].data(..).unwrap()
        );
        assert!(vote_socket.recv_from(recv_buffer).is_err());

        let (non_vote_packet_bytes, _) = non_vote_socket.recv_from(recv_buffer).unwrap();
        assert_eq!(
            &recv_buffer[..non_vote_packet_bytes],
            non_vote_packets[0][0].data(..).unwrap()
        );
        assert!(non_vote_socket.recv_from(recv_buffer).is_err());
    }
}
