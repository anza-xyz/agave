use {
    crate::tpu_info::TpuInfo,
    async_trait::async_trait,
    crossbeam_channel::{Receiver, RecvTimeoutError},
    itertools::Itertools,
    log::{warn, *},
    solana_client::connection_cache::{ConnectionCache, Protocol},
    solana_connection_cache::client_connection::ClientConnection as TpuConnection,
    solana_keypair::Keypair,
    solana_measure::measure::Measure,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_sdk::{
        hash::Hash, nonce_account, pubkey::Pubkey, quic::NotifyKeyUpdate, saturating_add_assign,
        signature::Signature, timing::AtomicInterval,
    },
    solana_tpu_client_next::{
        connection_workers_scheduler::{
            ConnectionWorkersSchedulerConfig, Fanout, TransactionStatsAndReceiver,
        },
        leader_updater::LeaderUpdater,
        transaction_batch::TransactionBatch,
        ConnectionWorkersScheduler, ConnectionWorkersSchedulerError,
    },
    std::{
        collections::{
            hash_map::{Entry, HashMap},
            HashSet,
        },
        net::{Ipv4Addr, SocketAddr},
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, Mutex, RwLock,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
    tokio::{
        runtime::Handle,
        sync::mpsc::{self},
        task::JoinHandle as TokioJoinHandle,
    },
    tokio_util::sync::CancellationToken,
};

/// Maximum size of the transaction retry pool
const MAX_TRANSACTION_RETRY_POOL_SIZE: usize = 10_000; // This seems like a lot but maybe it needs to be bigger one day

/// Default retry interval
const DEFAULT_RETRY_RATE_MS: u64 = 2_000;

/// Default number of leaders to forward transactions to
const DEFAULT_LEADER_FORWARD_COUNT: u64 = 2;
/// Default max number of time the service will retry broadcast
const DEFAULT_SERVICE_MAX_RETRIES: usize = usize::MAX;

/// Default batch size for sending transaction in batch
/// When this size is reached, send out the transactions.
const DEFAULT_TRANSACTION_BATCH_SIZE: usize = 1;

// The maximum transaction batch size
pub const MAX_TRANSACTION_BATCH_SIZE: usize = 10_000;

/// Maximum transaction sends per second
pub const MAX_TRANSACTION_SENDS_PER_SECOND: u64 = 1_000;

/// Default maximum batch waiting time in ms. If this time is reached,
/// whatever transactions are cached will be sent.
const DEFAULT_BATCH_SEND_RATE_MS: u64 = 1;

// The maximum transaction batch send rate in MS
pub const MAX_BATCH_SEND_RATE_MS: usize = 100_000;

// Alias trait to shorten function definitions.
pub trait TpuInfoWithSendStatic: TpuInfo + std::marker::Send + 'static {}
impl<T> TpuInfoWithSendStatic for T where T: TpuInfo + std::marker::Send + 'static {}

pub struct SendTransactionService {
    receive_txn_thread: JoinHandle<()>,
    retry_thread: JoinHandle<()>,
    exit: Arc<AtomicBool>,
}

pub struct TransactionInfo {
    pub signature: Signature,
    pub wire_transaction: Vec<u8>,
    pub last_valid_block_height: u64,
    pub durable_nonce_info: Option<(Pubkey, Hash)>,
    pub max_retries: Option<usize>,
    retries: usize,
    /// Last time the transaction was sent
    last_sent_time: Option<Instant>,
}

impl TransactionInfo {
    pub fn new(
        signature: Signature,
        wire_transaction: Vec<u8>,
        last_valid_block_height: u64,
        durable_nonce_info: Option<(Pubkey, Hash)>,
        max_retries: Option<usize>,
        last_sent_time: Option<Instant>,
    ) -> Self {
        Self {
            signature,
            wire_transaction,
            last_valid_block_height,
            durable_nonce_info,
            max_retries,
            retries: 0,
            last_sent_time,
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
struct ProcessTransactionsResult {
    rooted: u64,
    expired: u64,
    retried: u64,
    max_retries_elapsed: u64,
    failed: u64,
    retained: u64,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub retry_rate_ms: u64,
    pub leader_forward_count: u64,
    pub default_max_retries: Option<usize>,
    pub service_max_retries: usize,
    /// The batch size for sending transactions in batches
    pub batch_size: usize,
    /// How frequently batches are sent
    pub batch_send_rate_ms: u64,
    /// When the retry pool exceeds this max size, new transactions are dropped after their first broadcast attempt
    pub retry_pool_max_size: usize,
    pub tpu_peers: Option<Vec<SocketAddr>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            retry_rate_ms: DEFAULT_RETRY_RATE_MS,
            leader_forward_count: DEFAULT_LEADER_FORWARD_COUNT,
            default_max_retries: None,
            service_max_retries: DEFAULT_SERVICE_MAX_RETRIES,
            batch_size: DEFAULT_TRANSACTION_BATCH_SIZE,
            batch_send_rate_ms: DEFAULT_BATCH_SEND_RATE_MS,
            retry_pool_max_size: MAX_TRANSACTION_RETRY_POOL_SIZE,
            tpu_peers: None,
        }
    }
}

/// The maximum duration the retry thread may be configured to sleep before
/// processing the transactions that need to be retried.
pub const MAX_RETRY_SLEEP_MS: u64 = 1000;

/// The leader info refresh rate.
pub const LEADER_INFO_REFRESH_RATE_MS: u64 = 1000;

/// A struct responsible for holding up-to-date leader information
/// used for sending transactions.
#[derive(Clone)]
pub struct CurrentLeaderInfo<T>
where
    T: TpuInfoWithSendStatic,
{
    /// The last time the leader info was refreshed
    last_leader_refresh: Option<Instant>,

    /// The leader info
    leader_info: Option<T>,

    /// How often to refresh the leader info
    refresh_rate: Duration,
}

impl<T> CurrentLeaderInfo<T>
where
    T: TpuInfoWithSendStatic,
{
    /// Get the leader info, refresh if expired
    pub fn get_leader_info(&mut self) -> Option<&T> {
        if let Some(leader_info) = self.leader_info.as_mut() {
            let now = Instant::now();
            let need_refresh = self
                .last_leader_refresh
                .map(|last| now.duration_since(last) >= self.refresh_rate)
                .unwrap_or(true);

            if need_refresh {
                leader_info.refresh_recent_peers();
                self.last_leader_refresh = Some(now);
            }
        }
        self.leader_info.as_ref()
    }

    pub fn new(leader_info: Option<T>) -> Self {
        Self {
            last_leader_refresh: None,
            leader_info,
            refresh_rate: Duration::from_millis(LEADER_INFO_REFRESH_RATE_MS),
        }
    }
}

/// Metrics of the send-transaction-service.
#[derive(Default)]
pub struct SendTransactionServiceStats {
    /// Count of the received transactions
    pub received_transactions: AtomicU64,

    /// Count of the received duplicate transactions
    pub received_duplicate_transactions: AtomicU64,

    /// Count of transactions sent in batch
    pub sent_transactions: AtomicU64,

    /// Count of transactions not being added to retry queue
    /// due to queue size limit
    pub retry_queue_overflow: AtomicU64,

    /// retry queue size
    pub retry_queue_size: AtomicU64,

    /// The count of calls of sending transactions which can be in batch or single.
    pub send_attempt_count: AtomicU64,

    /// Time spent on transactions in micro seconds
    pub send_us: AtomicU64,

    /// Send failure count
    pub send_failure_count: AtomicU64,

    /// Count of nonced transactions
    pub nonced_transactions: AtomicU64,

    /// Count of rooted transactions
    pub rooted_transactions: AtomicU64,

    /// Count of expired transactions
    pub expired_transactions: AtomicU64,

    /// Count of transactions exceeding max retries
    pub transactions_exceeding_max_retries: AtomicU64,

    /// Count of retries of transactions
    pub retries: AtomicU64,

    /// Count of transactions failed
    pub failed_transactions: AtomicU64,
}

#[derive(Default)]
pub(crate) struct SendTransactionServiceStatsReport {
    pub stats: SendTransactionServiceStats,
    last_report: AtomicInterval,
}

impl SendTransactionServiceStatsReport {
    /// report metrics of the send transaction service
    pub fn report(&self) {
        if self
            .last_report
            .should_update(SEND_TRANSACTION_METRICS_REPORT_RATE_MS)
        {
            datapoint_info!(
                "send_transaction_service",
                (
                    "recv-tx",
                    self.stats.received_transactions.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "recv-duplicate",
                    self.stats
                        .received_duplicate_transactions
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "sent-tx",
                    self.stats.sent_transactions.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "retry-queue-overflow",
                    self.stats.retry_queue_overflow.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "retry-queue-size",
                    self.stats.retry_queue_size.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "send-us",
                    self.stats.send_us.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "send-attempt-count",
                    self.stats.send_attempt_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "send-failure-count",
                    self.stats.send_failure_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "nonced-tx",
                    self.stats.nonced_transactions.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "rooted-tx",
                    self.stats.rooted_transactions.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "expired-tx",
                    self.stats.expired_transactions.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "max-retries-exceeded-tx",
                    self.stats
                        .transactions_exceeding_max_retries
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "retries",
                    self.stats.retries.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "failed-tx",
                    self.stats.failed_transactions.swap(0, Ordering::Relaxed),
                    i64
                )
            );
        }
    }
}

/// Report the send transaction metrics for every 5 seconds.
const SEND_TRANSACTION_METRICS_REPORT_RATE_MS: u64 = 5000;

impl SendTransactionService {
    pub fn new<T: TpuInfoWithSendStatic>(
        tpu_address: SocketAddr,
        bank_forks: &Arc<RwLock<BankForks>>,
        leader_info: Option<T>,
        receiver: Receiver<TransactionInfo>,
        connection_cache: &Arc<ConnectionCache>,
        retry_rate_ms: u64,
        leader_forward_count: u64,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let config = Config {
            retry_rate_ms,
            leader_forward_count,
            ..Config::default()
        };
        Self::new_with_config(
            tpu_address,
            bank_forks,
            leader_info,
            receiver,
            connection_cache,
            config,
            exit,
        )
    }

    pub fn new_with_config<T: TpuInfoWithSendStatic>(
        tpu_address: SocketAddr,
        bank_forks: &Arc<RwLock<BankForks>>,
        leader_info: Option<T>,
        receiver: Receiver<TransactionInfo>,
        connection_cache: &Arc<ConnectionCache>,
        config: Config,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let client = ConnectionCacheClient::new(
            connection_cache.clone(),
            tpu_address,
            config.tpu_peers.clone(),
            leader_info,
            config.leader_forward_count,
        );

        Self::new_with_client(bank_forks, receiver, client, config, exit)
    }

    pub fn new_with_client<Client: TransactionClient + Clone + std::marker::Send + 'static>(
        bank_forks: &Arc<RwLock<BankForks>>,
        receiver: Receiver<TransactionInfo>,
        client: Client,
        config: Config,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let stats_report = Arc::new(SendTransactionServiceStatsReport::default());

        let retry_transactions = Arc::new(Mutex::new(HashMap::new()));

        let receive_txn_thread = Self::receive_txn_thread(
            receiver,
            client.clone(),
            retry_transactions.clone(),
            config.clone(),
            stats_report.clone(),
            exit.clone(),
        );

        let retry_thread = Self::retry_thread(
            bank_forks.clone(),
            client,
            retry_transactions,
            config,
            stats_report,
            exit.clone(),
        );

        Self {
            receive_txn_thread,
            retry_thread,
            exit,
        }
    }

    /// Thread responsible for receiving transactions from RPC clients.
    fn receive_txn_thread<Client: TransactionClient + std::marker::Send + 'static>(
        receiver: Receiver<TransactionInfo>,
        client: Client,
        retry_transactions: Arc<Mutex<HashMap<Signature, TransactionInfo>>>,
        Config {
            batch_send_rate_ms,
            batch_size,
            retry_pool_max_size,
            ..
        }: Config,
        stats_report: Arc<SendTransactionServiceStatsReport>,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        let mut last_batch_sent = Instant::now();
        let mut transactions = HashMap::new();

        debug!("Starting send-transaction-service::receive_txn_thread");
        Builder::new()
            .name("solStxReceive".to_string())
            .spawn(move || loop {
                let stats = &stats_report.stats;
                let recv_result = receiver.recv_timeout(Duration::from_millis(batch_send_rate_ms));
                if exit.load(Ordering::Relaxed) {
                    break;
                }
                match recv_result {
                    Err(RecvTimeoutError::Disconnected) => {
                        info!("Terminating send-transaction-service.");
                        exit.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Ok(transaction_info) => {
                        stats.received_transactions.fetch_add(1, Ordering::Relaxed);
                        let entry = transactions.entry(transaction_info.signature);
                        let mut new_transaction = false;
                        if let Entry::Vacant(_) = entry {
                            if !retry_transactions
                                .lock()
                                .unwrap()
                                .contains_key(&transaction_info.signature)
                            {
                                entry.or_insert(transaction_info);
                                new_transaction = true;
                            }
                        }
                        if !new_transaction {
                            stats
                                .received_duplicate_transactions
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }

                if (!transactions.is_empty()
                    && last_batch_sent.elapsed().as_millis() as u64 >= batch_send_rate_ms)
                    || transactions.len() >= batch_size
                {
                    stats
                        .sent_transactions
                        .fetch_add(transactions.len() as u64, Ordering::Relaxed);
                    let wire_transactions = transactions
                        .values()
                        .map(|transaction_info| transaction_info.wire_transaction.clone())
                        .collect::<Vec<Vec<u8>>>();
                    client.send_transactions_in_batch(wire_transactions, stats);
                    let last_sent_time = Instant::now();
                    {
                        // take a lock of retry_transactions and move the batch to the retry set.
                        let mut retry_transactions = retry_transactions.lock().unwrap();
                        let transactions_to_retry = transactions.len();
                        let mut transactions_added_to_retry: usize = 0;
                        for (signature, mut transaction_info) in transactions.drain() {
                            let retry_len = retry_transactions.len();
                            let entry = retry_transactions.entry(signature);
                            if let Entry::Vacant(_) = entry {
                                if retry_len >= retry_pool_max_size {
                                    break;
                                } else {
                                    transaction_info.last_sent_time = Some(last_sent_time);
                                    saturating_add_assign!(transactions_added_to_retry, 1);
                                    entry.or_insert(transaction_info);
                                }
                            }
                        }
                        stats.retry_queue_overflow.fetch_add(
                            transactions_to_retry.saturating_sub(transactions_added_to_retry)
                                as u64,
                            Ordering::Relaxed,
                        );
                        stats
                            .retry_queue_size
                            .store(retry_transactions.len() as u64, Ordering::Relaxed);
                    }
                    last_batch_sent = Instant::now();
                }
                stats_report.report();
            })
            .unwrap()
    }

    /// Thread responsible for retrying transactions
    fn retry_thread<Client: TransactionClient + std::marker::Send + 'static>(
        bank_forks: Arc<RwLock<BankForks>>,
        client: Client,
        retry_transactions: Arc<Mutex<HashMap<Signature, TransactionInfo>>>,
        config: Config,
        stats_report: Arc<SendTransactionServiceStatsReport>,
        exit: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        debug!("Starting send-transaction-service::retry_thread.");
        Builder::new()
            .name("solStxRetry".to_string())
            .spawn(move || loop {
                let retry_interval_ms = config.retry_rate_ms;
                let stats = &stats_report.stats;
                sleep(Duration::from_millis(
                    MAX_RETRY_SLEEP_MS.min(retry_interval_ms),
                ));
                if exit.load(Ordering::Relaxed) {
                    break;
                }
                let mut transactions = retry_transactions.lock().unwrap();
                if !transactions.is_empty() {
                    stats
                        .retry_queue_size
                        .store(transactions.len() as u64, Ordering::Relaxed);
                    let (root_bank, working_bank) = {
                        let bank_forks = bank_forks.read().unwrap();
                        (bank_forks.root_bank(), bank_forks.working_bank())
                    };

                    let _result = Self::process_transactions(
                        &working_bank,
                        &root_bank,
                        &mut transactions,
                        &client,
                        &config,
                        stats,
                    );
                    stats_report.report();
                }
            })
            .unwrap()
    }

    /// Retry transactions sent before.
    fn process_transactions<Client: TransactionClient + std::marker::Send + 'static>(
        working_bank: &Bank,
        root_bank: &Bank,
        transactions: &mut HashMap<Signature, TransactionInfo>,
        client: &Client,
        &Config {
            retry_rate_ms,
            service_max_retries,
            default_max_retries,
            batch_size,
            ..
        }: &Config,
        stats: &SendTransactionServiceStats,
    ) -> ProcessTransactionsResult {
        let mut result = ProcessTransactionsResult::default();

        let mut batched_transactions = HashSet::new();
        let retry_rate = Duration::from_millis(retry_rate_ms);

        transactions.retain(|signature, transaction_info| {
            if transaction_info.durable_nonce_info.is_some() {
                stats.nonced_transactions.fetch_add(1, Ordering::Relaxed);
            }
            if root_bank.has_signature(signature) {
                info!("Transaction is rooted: {}", signature);
                result.rooted += 1;
                stats.rooted_transactions.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            let signature_status = working_bank.get_signature_status_slot(signature);
            if let Some((nonce_pubkey, durable_nonce)) = transaction_info.durable_nonce_info {
                let nonce_account = working_bank.get_account(&nonce_pubkey).unwrap_or_default();
                let now = Instant::now();
                let expired = transaction_info
                    .last_sent_time
                    .map(|last| now.duration_since(last) >= retry_rate)
                    .unwrap_or(false);
                let verify_nonce_account =
                    nonce_account::verify_nonce_account(&nonce_account, &durable_nonce);
                if verify_nonce_account.is_none() && signature_status.is_none() && expired {
                    info!("Dropping expired durable-nonce transaction: {}", signature);
                    result.expired += 1;
                    stats.expired_transactions.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }
            if transaction_info.last_valid_block_height < root_bank.block_height() {
                info!("Dropping expired transaction: {}", signature);
                result.expired += 1;
                stats.expired_transactions.fetch_add(1, Ordering::Relaxed);
                return false;
            }

            let max_retries = transaction_info
                .max_retries
                .or(default_max_retries)
                .map(|max_retries| max_retries.min(service_max_retries));

            if let Some(max_retries) = max_retries {
                if transaction_info.retries >= max_retries {
                    info!("Dropping transaction due to max retries: {}", signature);
                    result.max_retries_elapsed += 1;
                    stats
                        .transactions_exceeding_max_retries
                        .fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }

            match signature_status {
                None => {
                    let now = Instant::now();
                    let need_send = transaction_info
                        .last_sent_time
                        .map(|last| now.duration_since(last) >= retry_rate)
                        .unwrap_or(true);
                    if need_send {
                        if transaction_info.last_sent_time.is_some() {
                            // Transaction sent before is unknown to the working bank, it might have been
                            // dropped or landed in another fork.  Re-send it

                            info!("Retrying transaction: {}", signature);
                            result.retried += 1;
                            transaction_info.retries += 1;
                            stats.retries.fetch_add(1, Ordering::Relaxed);
                        }

                        batched_transactions.insert(*signature);
                        transaction_info.last_sent_time = Some(now);
                    }
                    true
                }
                Some((_slot, status)) => {
                    if status.is_err() {
                        info!("Dropping failed transaction: {}", signature);
                        result.failed += 1;
                        stats.failed_transactions.fetch_add(1, Ordering::Relaxed);
                        false
                    } else {
                        result.retained += 1;
                        true
                    }
                }
            }
        });

        if !batched_transactions.is_empty() {
            // Processing the transactions in batch
            let wire_transactions = transactions
                .iter()
                .filter(|(signature, _)| batched_transactions.contains(signature))
                .map(|(_, transaction_info)| transaction_info.wire_transaction.clone());

            let iter = wire_transactions.chunks(batch_size);
            for chunk in &iter {
                let chunk = chunk.collect();
                client.send_transactions_in_batch(chunk, stats);
            }
        }
        result
    }

    pub fn join(self) -> thread::Result<()> {
        self.receive_txn_thread.join()?;
        self.exit.store(true, Ordering::Relaxed);
        self.retry_thread.join()
    }
}

pub trait TransactionClient {
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    );

    fn protocol(&self) -> Protocol;

    fn exit(&self);
}

pub struct ConnectionCacheClient<T: TpuInfoWithSendStatic> {
    connection_cache: Arc<ConnectionCache>,
    tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
    leader_info_provider: Arc<Mutex<CurrentLeaderInfo<T>>>,
    leader_forward_count: u64,
}

// Manual implementation of Clone without requiring T to be Clone
impl<T> Clone for ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    fn clone(&self) -> Self {
        Self {
            connection_cache: Arc::clone(&self.connection_cache),
            tpu_address: self.tpu_address,
            tpu_peers: self.tpu_peers.clone(),
            leader_info_provider: Arc::clone(&self.leader_info_provider),
            leader_forward_count: self.leader_forward_count,
        }
    }
}

impl<T> ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    pub fn new(
        connection_cache: Arc<ConnectionCache>,
        tpu_address: SocketAddr,
        tpu_peers: Option<Vec<SocketAddr>>,
        leader_info: Option<T>,
        leader_forward_count: u64,
    ) -> Self {
        let leader_info_provider = Arc::new(Mutex::new(CurrentLeaderInfo::new(leader_info)));
        Self {
            connection_cache,
            tpu_address,
            tpu_peers,
            leader_info_provider,
            leader_forward_count,
        }
    }

    fn get_tpu_addresses<'a>(&'a self, leader_info: Option<&'a T>) -> Vec<&'a SocketAddr> {
        leader_info
            .map(|leader_info| {
                leader_info
                    .get_leader_tpus(self.leader_forward_count, self.connection_cache.protocol())
            })
            .filter(|addresses| !addresses.is_empty())
            .unwrap_or_else(|| vec![&self.tpu_address])
    }

    fn send_transactions(
        &self,
        peer: &SocketAddr,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    ) {
        let mut measure = Measure::start("send-us");
        let conn = self.connection_cache.get_connection(peer);
        let result = conn.send_data_batch_async(wire_transactions);

        if let Err(err) = result {
            warn!(
                "Failed to send transaction transaction to {}: {:?}",
                self.tpu_address, err
            );
            stats.send_failure_count.fetch_add(1, Ordering::Relaxed);
        }

        measure.stop();
        stats.send_us.fetch_add(measure.as_us(), Ordering::Relaxed);
        stats.send_attempt_count.fetch_add(1, Ordering::Relaxed);
    }
}

impl<T> TransactionClient for ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    ) {
        // Processing the transactions in batch
        let mut addresses = self
            .tpu_peers
            .as_ref()
            .map(|addrs| addrs.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut leader_info_provider = self.leader_info_provider.lock().unwrap();
        let leader_info = leader_info_provider.get_leader_info();
        let leader_addresses = self.get_tpu_addresses(leader_info);
        addresses.extend(leader_addresses);

        for address in &addresses {
            self.send_transactions(address, wire_transactions.clone(), stats);
        }
    }

    fn protocol(&self) -> Protocol {
        self.connection_cache.protocol()
    }

    fn exit(&self) {}
}

impl<T> NotifyKeyUpdate for ConnectionCacheClient<T>
where
    T: TpuInfoWithSendStatic,
{
    fn update_key(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        self.connection_cache.update_key(identity)
    }
}

/// `TpuClientNextClient` provides an interface for managing the
/// [`ConnectionWorkersScheduler`].
///
/// It allows:
/// * Create and initializes the scheduler with runtime configurations,
/// * Send transactions to the connection scheduler,
/// * Update the validator identity keypair and propagate the changes to the
///   scheduler. Most of the complexity of this structure arises from this
///   functionality.
#[derive(Clone)]
pub struct TpuClientNextClient<T>
where
    T: TpuInfoWithSendStatic + Clone,
{
    runtime_handle: Handle,
    sender: mpsc::Sender<TransactionBatch>,
    // This handle is needed to implement `NotifyKeyUpdate` trait. It's only
    // method takes &self and thus we need to wrap with Mutex.
    join_and_cancel: Arc<Mutex<(Option<TpuClientJoinHandle>, CancellationToken)>>,
    leader_updater: SendTransactionServiceLeaderUpdater<T>,
    leader_forward_count: u64,
}

type TpuClientJoinHandle =
    TokioJoinHandle<Result<TransactionStatsAndReceiver, ConnectionWorkersSchedulerError>>;

impl<T> TpuClientNextClient<T>
where
    T: TpuInfoWithSendStatic + Clone,
{
    pub fn new(
        runtime_handle: Handle,
        my_tpu_address: SocketAddr,
        tpu_peers: Option<Vec<SocketAddr>>,
        leader_info: Option<T>,
        leader_forward_count: u64,
        identity: Option<Keypair>,
    ) -> Self
    where
        T: TpuInfoWithSendStatic + Clone,
    {
        // The channel size represents 8s worth of transactions at a rate of
        // 1000 tps, assuming batch size is 64.
        let (sender, receiver) = mpsc::channel(128);

        let cancel = CancellationToken::new();

        let leader_info_provider = CurrentLeaderInfo::new(leader_info);
        let leader_updater: SendTransactionServiceLeaderUpdater<T> =
            SendTransactionServiceLeaderUpdater {
                leader_info_provider,
                my_tpu_address,
                tpu_peers,
            };
        let config = Self::create_config(identity, leader_forward_count as usize);
        let handle = runtime_handle.spawn(ConnectionWorkersScheduler::run(
            config,
            Box::new(leader_updater.clone()),
            receiver,
            cancel.clone(),
        ));

        Self {
            runtime_handle,
            join_and_cancel: Arc::new(Mutex::new((Some(handle), cancel))),
            sender,
            leader_updater,
            leader_forward_count,
        }
    }

    fn create_config(
        stake_identity: Option<Keypair>,
        leader_forward_count: usize,
    ) -> ConnectionWorkersSchedulerConfig {
        ConnectionWorkersSchedulerConfig {
            bind: SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), 0),
            stake_identity,
            // to match MAX_CONNECTIONS from ConnectionCache
            num_connections: 1024,
            skip_check_transaction_age: true,
            // experimentally found parameter values
            worker_channel_size: 64,
            max_reconnect_attempts: 4,
            leaders_fanout: Fanout {
                connect: leader_forward_count,
                send: leader_forward_count,
            },
        }
    }

    #[cfg(any(test, feature = "dev-context-only-utils"))]
    pub fn cancel(&self) -> Result<(), Box<dyn std::error::Error>> {
        let Ok(lock) = self.join_and_cancel.lock() else {
            return Err("Failed to stop scheduler: TpuClientNext task panicked.".into());
        };
        lock.1.cancel();
        Ok(())
    }

    async fn do_update_key(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let runtime_handle = self.runtime_handle.clone();
        let config = Self::create_config(
            Some(identity.insecure_clone()),
            self.leader_forward_count as usize,
        );
        let leader_updater = self.leader_updater.clone();
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
                Ok((_stats, receiver)) => {
                    let cancel = CancellationToken::new();
                    let join_handle = runtime_handle.spawn(ConnectionWorkersScheduler::run(
                        config,
                        Box::new(leader_updater),
                        receiver,
                        cancel.clone(),
                    ));

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

impl<T> NotifyKeyUpdate for TpuClientNextClient<T>
where
    T: TpuInfoWithSendStatic + Clone,
{
    fn update_key(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime_handle.block_on(self.do_update_key(identity))
    }
}

impl<T> TransactionClient for TpuClientNextClient<T>
where
    T: TpuInfoWithSendStatic + Clone,
{
    fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
        stats: &SendTransactionServiceStats,
    ) {
        let mut measure = Measure::start("send-us");
        self.runtime_handle.spawn({
            let sender = self.sender.clone();
            async move {
                let res = sender.send(TransactionBatch::new(wire_transactions)).await;
                if res.is_err() {
                    warn!("Failed to send transaction to channel: it is closed.");
                }
            }
        });

        measure.stop();
        stats.send_us.fetch_add(measure.as_us(), Ordering::Relaxed);
        stats.send_attempt_count.fetch_add(1, Ordering::Relaxed);
    }

    fn protocol(&self) -> Protocol {
        Protocol::QUIC
    }

    fn exit(&self) {
        let Ok(mut lock) = self.join_and_cancel.lock() else {
            error!("Failed to stop scheduler: TpuClientNext task panicked.");
            return;
        };
        let (handle, token) = std::mem::take(&mut *lock);
        token.cancel();
        if let Some(handle) = handle {
            match self.runtime_handle.block_on(handle) {
                Ok(result) => match result {
                    Ok(stats) => {
                        debug!("tpu-client-next statistics over all the connections: {stats:?}");
                    }
                    Err(error) => error!("tpu-client-next exits with error {error}."),
                },
                Err(error) => error!("Failed to join task {error}."),
            }
        }
    }
}

#[derive(Clone)]
pub struct SendTransactionServiceLeaderUpdater<T: TpuInfoWithSendStatic> {
    leader_info_provider: CurrentLeaderInfo<T>,
    my_tpu_address: SocketAddr,
    tpu_peers: Option<Vec<SocketAddr>>,
}

#[async_trait]
impl<T> LeaderUpdater for SendTransactionServiceLeaderUpdater<T>
where
    T: TpuInfoWithSendStatic,
{
    fn next_leaders(&mut self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        let discovered_peers = self
            .leader_info_provider
            .get_leader_info()
            .map(|leader_info| {
                leader_info.get_leader_tpus(lookahead_leaders as u64, Protocol::QUIC)
            })
            .filter(|addresses| !addresses.is_empty())
            .unwrap_or_else(|| vec![&self.my_tpu_address]);
        let mut all_peers = self.tpu_peers.clone().unwrap_or_default();
        all_peers.extend(discovered_peers.into_iter().cloned());
        all_peers
    }
    async fn stop(&mut self) {}
}

#[cfg(any(test, feature = "dev-context-only-utils"))]
mod test_utils {
    use {
        super::{
            ConnectionCacheClient, TpuClientNextClient, TpuInfoWithSendStatic, TransactionClient,
        },
        crate::tpu_info::NullTpuInfo,
        solana_client::connection_cache::ConnectionCache,
        std::{net::SocketAddr, sync::Arc},
        tokio::runtime::Handle,
    };

    // `maybe_runtime` argument is introduced to be able to use runtime from test
    // for the TpuClientNext, while ConnectionCache uses runtime created internally
    // in the quic-client module and it is impossible to pass test runtime there.
    pub trait CreateClient: TransactionClient {
        fn create_client(
            maybe_runtime: Option<Handle>,
            my_tpu_address: SocketAddr,
            tpu_peers: Option<Vec<SocketAddr>>,
            leader_forward_count: u64,
        ) -> Self;
    }

    impl CreateClient for ConnectionCacheClient<NullTpuInfo> {
        fn create_client(
            maybe_runtime: Option<Handle>,
            my_tpu_address: SocketAddr,
            tpu_peers: Option<Vec<SocketAddr>>,
            leader_forward_count: u64,
        ) -> Self {
            assert!(maybe_runtime.is_none());
            let connection_cache = Arc::new(ConnectionCache::new("connection_cache_test"));
            ConnectionCacheClient::new(
                connection_cache,
                my_tpu_address,
                tpu_peers,
                None,
                leader_forward_count,
            )
        }
    }

    impl CreateClient for TpuClientNextClient<NullTpuInfo> {
        fn create_client(
            maybe_runtime: Option<Handle>,
            my_tpu_address: SocketAddr,
            tpu_peers: Option<Vec<SocketAddr>>,
            leader_forward_count: u64,
        ) -> Self {
            let runtime_handle =
                maybe_runtime.expect("Runtime should be provided for the TpuClientNextClient.");
            Self::new(
                runtime_handle,
                my_tpu_address,
                tpu_peers,
                None,
                leader_forward_count,
                None,
            )
        }
    }

    pub trait Cancelable {
        fn cancel(&self);
    }

    impl<T> Cancelable for ConnectionCacheClient<T>
    where
        T: TpuInfoWithSendStatic,
    {
        fn cancel(&self) {}
    }

    impl<T> Cancelable for TpuClientNextClient<T>
    where
        T: TpuInfoWithSendStatic + Clone,
    {
        fn cancel(&self) {
            self.cancel().unwrap();
        }
    }

    // Define type alias to simplify definition of test functions.
    pub trait ClientWithCreator:
        CreateClient + TransactionClient + Cancelable + Send + Clone + 'static
    {
    }
    impl<T> ClientWithCreator for T where
        T: CreateClient + TransactionClient + Cancelable + Send + Clone + 'static
    {
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::{send_transaction_service::test_utils::ClientWithCreator, tpu_info::NullTpuInfo},
        crossbeam_channel::{bounded, unbounded},
        solana_sdk::{
            account::AccountSharedData,
            genesis_config::create_genesis_config,
            nonce::{self, state::DurableNonce},
            pubkey::Pubkey,
            signature::Signer,
            system_program, system_transaction,
        },
        std::ops::Sub,
        tokio::runtime::Handle,
    };

    fn service_exit<C: ClientWithCreator>(maybe_runtime: Option<Handle>) {
        let bank = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank);
        let (sender, receiver) = unbounded();

        let client = C::create_client(maybe_runtime, "127.0.0.1:0".parse().unwrap(), None, 1);

        let send_transaction_service = SendTransactionService::new_with_client(
            &bank_forks,
            receiver,
            client.clone(),
            Config {
                retry_rate_ms: 1000,
                ..Config::default()
            },
            Arc::new(AtomicBool::new(false)),
        );

        drop(sender);
        send_transaction_service.join().unwrap();
        client.cancel();
    }

    #[test]
    fn service_exit_with_connection_cache() {
        service_exit::<ConnectionCacheClient<NullTpuInfo>>(None);
    }

    #[tokio::test(flavor = "multi_thread")]

    async fn service_exit_with_tpu_client_next() {
        service_exit::<TpuClientNextClient<NullTpuInfo>>(Some(Handle::current()));
    }

    fn validator_exit<C: ClientWithCreator>(maybe_runtime: Option<Handle>) {
        let bank = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank);
        let (sender, receiver) = bounded(0);

        let dummy_tx_info = || TransactionInfo {
            signature: Signature::default(),
            wire_transaction: vec![0; 128],
            last_valid_block_height: 0,
            durable_nonce_info: None,
            max_retries: None,
            retries: 0,
            last_sent_time: None,
        };

        let exit = Arc::new(AtomicBool::new(false));
        let client = C::create_client(maybe_runtime, "127.0.0.1:0".parse().unwrap(), None, 1);
        let _send_transaction_service = SendTransactionService::new_with_client(
            &bank_forks,
            receiver,
            client.clone(),
            Config {
                retry_rate_ms: 1000,
                ..Config::default()
            },
            exit.clone(),
        );

        sender.send(dummy_tx_info()).unwrap();

        thread::spawn(move || {
            exit.store(true, Ordering::Relaxed);
            client.cancel();
        });

        let mut option = Ok(());
        while option.is_ok() {
            option = sender.send(dummy_tx_info());
        }
    }

    #[test]
    fn validator_exit_with_connection_cache() {
        validator_exit::<ConnectionCacheClient<NullTpuInfo>>(None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validator_exit_with_tpu_client_next() {
        validator_exit::<TpuClientNextClient<NullTpuInfo>>(Some(Handle::current()));
    }

    fn process_transactions<C: ClientWithCreator>(maybe_runtime: Option<Handle>) {
        solana_logger::setup();

        let (mut genesis_config, mint_keypair) = create_genesis_config(4);
        genesis_config.fee_rate_governor = solana_sdk::fee_calculator::FeeRateGovernor::new(0, 0);
        let (_, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let leader_forward_count = 1;
        let config = Config::default();

        let root_bank = Bank::new_from_parent(
            bank_forks.read().unwrap().working_bank(),
            &Pubkey::default(),
            1,
        );
        let root_bank = bank_forks
            .write()
            .unwrap()
            .insert(root_bank)
            .clone_without_scheduler();

        let rooted_signature = root_bank
            .transfer(1, &mint_keypair, &mint_keypair.pubkey())
            .unwrap();

        let working_bank = bank_forks
            .write()
            .unwrap()
            .insert(Bank::new_from_parent(
                root_bank.clone(),
                &Pubkey::default(),
                2,
            ))
            .clone_without_scheduler();

        let non_rooted_signature = working_bank
            .transfer(2, &mint_keypair, &mint_keypair.pubkey())
            .unwrap();

        let failed_signature = {
            let blockhash = working_bank.last_blockhash();
            let transaction =
                system_transaction::transfer(&mint_keypair, &Pubkey::default(), 1, blockhash);
            let signature = transaction.signatures[0];
            working_bank.process_transaction(&transaction).unwrap_err();
            signature
        };

        let mut transactions = HashMap::new();

        info!("Expired transactions are dropped...");
        let stats = SendTransactionServiceStats::default();
        transactions.insert(
            Signature::default(),
            TransactionInfo::new(
                Signature::default(),
                vec![],
                root_bank.block_height() - 1,
                None,
                None,
                Some(Instant::now()),
            ),
        );

        let client = C::create_client(
            maybe_runtime,
            "127.0.0.1:0".parse().unwrap(),
            config.tpu_peers.clone(),
            leader_forward_count,
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                expired: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        info!("Rooted transactions are dropped...");
        transactions.insert(
            rooted_signature,
            TransactionInfo::new(
                rooted_signature,
                vec![],
                working_bank.block_height(),
                None,
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                rooted: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        info!("Failed transactions are dropped...");
        transactions.insert(
            failed_signature,
            TransactionInfo::new(
                failed_signature,
                vec![],
                working_bank.block_height(),
                None,
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                failed: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        info!("Non-rooted transactions are kept...");
        transactions.insert(
            non_rooted_signature,
            TransactionInfo::new(
                non_rooted_signature,
                vec![],
                working_bank.block_height(),
                None,
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert_eq!(transactions.len(), 1);
        assert_eq!(
            result,
            ProcessTransactionsResult {
                retained: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        transactions.clear();

        info!("Unknown transactions are retried...");
        transactions.insert(
            Signature::default(),
            TransactionInfo::new(
                Signature::default(),
                vec![],
                working_bank.block_height(),
                None,
                None,
                Some(Instant::now().sub(Duration::from_millis(4000))),
            ),
        );

        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert_eq!(transactions.len(), 1);
        assert_eq!(
            result,
            ProcessTransactionsResult {
                retried: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        transactions.clear();

        info!("Transactions are only retried until max_retries");
        transactions.insert(
            Signature::from([1; 64]),
            TransactionInfo::new(
                Signature::default(),
                vec![],
                working_bank.block_height(),
                None,
                Some(0),
                Some(Instant::now()),
            ),
        );
        transactions.insert(
            Signature::from([2; 64]),
            TransactionInfo::new(
                Signature::default(),
                vec![],
                working_bank.block_height(),
                None,
                Some(1),
                Some(Instant::now().sub(Duration::from_millis(4000))),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert_eq!(transactions.len(), 1);
        assert_eq!(
            result,
            ProcessTransactionsResult {
                retried: 1,
                max_retries_elapsed: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                retried: 1,
                max_retries_elapsed: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        client.cancel();
    }

    #[test]
    fn process_transactions_with_connection_cache() {
        process_transactions::<ConnectionCacheClient<NullTpuInfo>>(None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn process_transactions_with_tpu_client_next() {
        process_transactions::<TpuClientNextClient<NullTpuInfo>>(Some(Handle::current()));
    }

    fn retry_durable_nonce_transactions<C: ClientWithCreator>(maybe_runtime: Option<Handle>) {
        solana_logger::setup();

        let (mut genesis_config, mint_keypair) = create_genesis_config(4);
        genesis_config.fee_rate_governor = solana_sdk::fee_calculator::FeeRateGovernor::new(0, 0);
        let (_, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let leader_forward_count = 1;
        let config = Config::default();

        let root_bank = Bank::new_from_parent(
            bank_forks.read().unwrap().working_bank(),
            &Pubkey::default(),
            1,
        );
        let root_bank = bank_forks
            .write()
            .unwrap()
            .insert(root_bank)
            .clone_without_scheduler();

        let rooted_signature = root_bank
            .transfer(1, &mint_keypair, &mint_keypair.pubkey())
            .unwrap();

        let nonce_address = Pubkey::new_unique();
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let nonce_state = nonce::state::Versions::new(nonce::State::Initialized(
            nonce::state::Data::new(Pubkey::default(), durable_nonce, 42),
        ));
        let nonce_account =
            AccountSharedData::new_data(43, &nonce_state, &system_program::id()).unwrap();
        root_bank.store_account(&nonce_address, &nonce_account);

        let working_bank = bank_forks
            .write()
            .unwrap()
            .insert(Bank::new_from_parent(
                root_bank.clone(),
                &Pubkey::default(),
                2,
            ))
            .clone_without_scheduler();
        let non_rooted_signature = working_bank
            .transfer(2, &mint_keypair, &mint_keypair.pubkey())
            .unwrap();

        let last_valid_block_height = working_bank.block_height() + 300;

        let failed_signature = {
            let blockhash = working_bank.last_blockhash();
            let transaction =
                system_transaction::transfer(&mint_keypair, &Pubkey::default(), 1, blockhash);
            let signature = transaction.signatures[0];
            working_bank.process_transaction(&transaction).unwrap_err();
            signature
        };

        let mut transactions = HashMap::new();

        info!("Rooted durable-nonce transactions are dropped...");
        transactions.insert(
            rooted_signature,
            TransactionInfo::new(
                rooted_signature,
                vec![],
                last_valid_block_height,
                Some((nonce_address, *durable_nonce.as_hash())),
                None,
                Some(Instant::now()),
            ),
        );
        let stats = SendTransactionServiceStats::default();
        let client = C::create_client(
            maybe_runtime,
            "127.0.0.1:0".parse().unwrap(),
            config.tpu_peers.clone(),
            leader_forward_count,
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                rooted: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        // Nonce expired case
        transactions.insert(
            rooted_signature,
            TransactionInfo::new(
                rooted_signature,
                vec![],
                last_valid_block_height,
                Some((nonce_address, Hash::new_unique())),
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                rooted: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        // Expired durable-nonce transactions are dropped; nonce has advanced...
        info!("Expired durable-nonce transactions are dropped...");
        transactions.insert(
            Signature::default(),
            TransactionInfo::new(
                Signature::default(),
                vec![],
                last_valid_block_height,
                Some((nonce_address, Hash::new_unique())),
                None,
                Some(Instant::now().sub(Duration::from_millis(4000))),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                expired: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        // ... or last_valid_block_height timeout has passed
        transactions.insert(
            Signature::default(),
            TransactionInfo::new(
                Signature::default(),
                vec![],
                root_bank.block_height() - 1,
                Some((nonce_address, *durable_nonce.as_hash())),
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                expired: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        info!("Failed durable-nonce transactions are dropped...");
        transactions.insert(
            failed_signature,
            TransactionInfo::new(
                failed_signature,
                vec![],
                last_valid_block_height,
                Some((nonce_address, Hash::new_unique())), // runtime should advance nonce on failed transactions
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert!(transactions.is_empty());
        assert_eq!(
            result,
            ProcessTransactionsResult {
                failed: 1,
                ..ProcessTransactionsResult::default()
            }
        );

        info!("Non-rooted durable-nonce transactions are kept...");
        transactions.insert(
            non_rooted_signature,
            TransactionInfo::new(
                non_rooted_signature,
                vec![],
                last_valid_block_height,
                Some((nonce_address, Hash::new_unique())), // runtime advances nonce when transaction lands
                None,
                Some(Instant::now()),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert_eq!(transactions.len(), 1);
        assert_eq!(
            result,
            ProcessTransactionsResult {
                retained: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        transactions.clear();

        info!("Unknown durable-nonce transactions are retried until nonce advances...");
        // simulate there was a nonce transaction sent 4 seconds ago (> the retry rate which is 2 seconds)
        transactions.insert(
            Signature::default(),
            TransactionInfo::new(
                Signature::default(),
                vec![],
                last_valid_block_height,
                Some((nonce_address, *durable_nonce.as_hash())),
                None,
                Some(Instant::now().sub(Duration::from_millis(4000))),
            ),
        );
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert_eq!(transactions.len(), 1);
        assert_eq!(
            result,
            ProcessTransactionsResult {
                retried: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        // Advance nonce, simulate the transaction was again last sent 4 seconds ago.
        // This time the transaction should have been dropped.
        for transaction in transactions.values_mut() {
            transaction.last_sent_time = Some(Instant::now().sub(Duration::from_millis(4000)));
        }
        let new_durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let new_nonce_state = nonce::state::Versions::new(nonce::State::Initialized(
            nonce::state::Data::new(Pubkey::default(), new_durable_nonce, 42),
        ));
        let nonce_account =
            AccountSharedData::new_data(43, &new_nonce_state, &system_program::id()).unwrap();
        working_bank.store_account(&nonce_address, &nonce_account);
        let result = SendTransactionService::process_transactions(
            &working_bank,
            &root_bank,
            &mut transactions,
            &client,
            &config,
            &stats,
        );
        assert_eq!(transactions.len(), 0);
        assert_eq!(
            result,
            ProcessTransactionsResult {
                expired: 1,
                ..ProcessTransactionsResult::default()
            }
        );
        client.cancel();
    }

    #[test]
    fn retry_durable_nonce_transactions_with_connection_cache() {
        retry_durable_nonce_transactions::<ConnectionCacheClient<NullTpuInfo>>(None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retry_durable_nonce_transactions_with_tpu_client_next() {
        retry_durable_nonce_transactions::<TpuClientNextClient<NullTpuInfo>>(Some(
            Handle::current(),
        ));
    }
}
