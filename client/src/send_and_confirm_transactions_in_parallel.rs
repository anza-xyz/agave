use {
    crate::{
        nonblocking::{rpc_client::RpcClient, tpu_client::TpuClient},
        rpc_client::RpcClient as BlockingRpcClient,
    },
    bincode::serialize,
    dashmap::DashMap,
    futures_util::future::join_all,
    solana_hash::Hash,
    solana_message::{Message, VersionedMessage},
    solana_quic_client::{QuicConfig, QuicConnectionManager, QuicPool},
    solana_rpc_client::spinner::{self, SendTransactionProgress},
    solana_rpc_client_api::{
        client_error::ErrorKind,
        config::RpcSendTransactionConfig,
        request::{MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS, RpcError, RpcResponseErrorData},
        response::RpcSimulateTransactionResult,
    },
    solana_signature::Signature,
    solana_signer::{SignerError, signers::Signers},
    solana_tpu_client::tpu_client::{Result, TpuSenderError},
    solana_tpu_client_next::client_builder::TransactionSender,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_error::TransactionError,
    std::{
        future::Future,
        num::NonZeroUsize,
        sync::{
            Arc,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
        time::Duration,
    },
    tokio::{
        sync::{Notify, RwLock},
        task::JoinHandle,
        time::sleep,
    },
};

const BLOCKHASH_REFRESH_RATE: Duration = Duration::from_secs(5);
const SEND_INTERVAL: Duration = Duration::from_millis(10);
// This is a "reasonable" constant for how long it should
// take to fan the transactions out, taken from
// `solana_tpu_client::nonblocking::tpu_client::send_wire_transaction_futures`
const SEND_TIMEOUT_INTERVAL: Duration = Duration::from_secs(5);

type QuicTpuClient = TpuClient<QuicPool, QuicConnectionManager, QuicConfig>;

/// Abstracts sending a single serialized transaction to the current leader's
/// TPU, so the send & confirm engine can drive either the legacy
/// connection-cache [`QuicTpuClient`] or the [`TransactionSender`] from
/// `tpu-client-next`.
trait WireTransactionSender: Send + Sync {
    /// Returns `true` if the transaction was handed off to the transport.
    fn send(&self, wire_transaction: Vec<u8>) -> impl Future<Output = bool> + Send;
}

impl WireTransactionSender for QuicTpuClient {
    fn send(&self, wire_transaction: Vec<u8>) -> impl Future<Output = bool> + Send {
        let fut = self.send_wire_transaction(wire_transaction);
        async move {
            tokio::time::timeout(SEND_TIMEOUT_INTERVAL, fut)
                .await
                .unwrap_or(false)
        }
    }
}

impl WireTransactionSender for TransactionSender {
    // tpu-client-next is fire-and-forget: a successful local enqueue here means nothing.
    // The future returned here will only resolve to false if tpu-client-next
    // instance is dropped.
    fn send(&self, wire_transaction: Vec<u8>) -> impl Future<Output = bool> + Send {
        let sender = self.clone();
        async move {
            sender
                .send_transactions_in_batch(vec![wire_transaction])
                .await
                .is_ok()
        }
    }
}

#[derive(Clone, Debug)]
struct TransactionData {
    last_valid_block_height: u64,
    message: VersionedMessage,
    index: usize,
    serialized_transaction: Vec<u8>,
}

#[derive(Clone, Debug, Copy)]
struct BlockHashData {
    pub blockhash: Hash,
    pub last_valid_block_height: u64,
}

// New struct with RpcSendTransactionConfig for non-breaking change
#[derive(Clone, Debug, Copy)]
pub struct SendAndConfirmConfigV2 {
    pub with_spinner: bool,
    pub resign_txs_count: Option<usize>,
    pub rpc_send_transaction_config: RpcSendTransactionConfig,
}

pub enum SendTransport {
    /// Send directly to the leader through tpu-client-next.
    Tpu(TransactionSender),
    /// Send through RPC.
    Rpc(RpcSendTransactionConfig),
}

#[derive(Clone, Debug, Copy)]
pub struct SendAndConfirmConfigV3 {
    /// Shows a spinner with progress in the console while sending.
    pub with_spinner: bool,
    /// Maximum number of signing passes to make before giving up. A new
    /// signing pass is made whenever blockhash expires.
    pub max_sign_attempts: NonZeroUsize,
    /// How long to wait between checking whether transactions have landed (and
    /// resending those that have not). Should be at least a slot.
    pub check_interval: Duration,
    /// Delay between consecutive transactions within a send pass. Transaction
    /// `n` is delayed by `n * send_interval`, capping the per-pass send rate at
    /// `1 / send_interval`.
    pub send_interval: Duration,
}
const DEFAULT_MAX_SIGN_ATTEMPTS: NonZeroUsize = NonZeroUsize::new(1).unwrap();
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(1);

impl Default for SendAndConfirmConfigV3 {
    fn default() -> Self {
        Self {
            with_spinner: false,
            max_sign_attempts: DEFAULT_MAX_SIGN_ATTEMPTS,
            check_interval: DEFAULT_CHECK_INTERVAL,
            send_interval: SEND_INTERVAL,
        }
    }
}

/// Internal configuration shared by the v2 and v3 entry points.
struct ParallelSendConfig {
    with_spinner: bool,
    max_sign_attempts: NonZeroUsize,
    rpc_send_transaction_config: Option<RpcSendTransactionConfig>,
    // only the legacy v2 API sets this.
    dedupe_signers: bool,
    send_interval: Duration,
    check_interval: Duration,
}

impl From<SendAndConfirmConfigV2> for ParallelSendConfig {
    fn from(config: SendAndConfirmConfigV2) -> Self {
        Self {
            with_spinner: config.with_spinner,
            max_sign_attempts: config
                .resign_txs_count
                .and_then(NonZeroUsize::new)
                .unwrap_or(DEFAULT_MAX_SIGN_ATTEMPTS),
            // v2 always sends over RPC when the TPU transport is absent or fails.
            rpc_send_transaction_config: Some(config.rpc_send_transaction_config),
            dedupe_signers: true,
            send_interval: SEND_INTERVAL,
            check_interval: DEFAULT_CHECK_INTERVAL,
        }
    }
}

/// Sends and confirms transactions concurrently in a sync context
pub fn send_and_confirm_transactions_in_parallel_blocking_v2<T: Signers + ?Sized>(
    rpc_client: Arc<BlockingRpcClient>,
    tpu_client: Option<QuicTpuClient>,
    messages: &[Message],
    signers: &T,
    config: SendAndConfirmConfigV2,
) -> Result<Vec<Option<TransactionError>>> {
    let fut = send_and_confirm_transactions_in_parallel_impl(
        rpc_client.get_inner_client().clone(),
        tpu_client,
        messages.iter().cloned().map(VersionedMessage::Legacy),
        signers,
        config.into(),
    );
    tokio::task::block_in_place(|| rpc_client.runtime().block_on(fut))
}

fn create_blockhash_data_updating_task(
    rpc_client: Arc<RpcClient>,
    blockhash_data_rw: Arc<RwLock<BlockHashData>>,
    current_block_height: Arc<AtomicU64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Ok((blockhash, last_valid_block_height)) = rpc_client
                .get_latest_blockhash_with_commitment(rpc_client.commitment())
                .await
            {
                *blockhash_data_rw.write().await = BlockHashData {
                    blockhash,
                    last_valid_block_height,
                };
            }

            if let Ok(block_height) = rpc_client.get_block_height().await {
                current_block_height.store(block_height, Ordering::Relaxed);
            }
            tokio::time::sleep(BLOCKHASH_REFRESH_RATE).await;
        }
    })
}

fn create_transaction_confirmation_task(
    rpc_client: Arc<RpcClient>,
    current_block_height: Arc<AtomicU64>,
    unconfirmed_transaction_map: Arc<DashMap<Signature, TransactionData>>,
    errors_map: Arc<DashMap<usize, TransactionError>>,
    num_confirmed_transactions: Arc<AtomicUsize>,
    check_interval: Duration,
    confirmation_signal: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // check transactions that are not expired or have just expired between two checks
        let mut last_block_height = current_block_height.load(Ordering::Relaxed);

        loop {
            // we sleep before first check to give sender a chance
            tokio::time::sleep(check_interval).await;
            if !unconfirmed_transaction_map.is_empty() {
                let current_block_height = current_block_height.load(Ordering::Relaxed);
                let transactions_to_verify: Vec<Signature> = unconfirmed_transaction_map
                    .iter()
                    .filter(|x| {
                        let is_not_expired = current_block_height <= x.last_valid_block_height;
                        // transaction expired between last and current check
                        let is_recently_expired = last_block_height <= x.last_valid_block_height
                            && current_block_height > x.last_valid_block_height;
                        is_not_expired || is_recently_expired
                    })
                    .map(|x| *x.key())
                    .collect();
                for signatures in
                    transactions_to_verify.chunks(MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS)
                {
                    if let Ok(result) = rpc_client.get_signature_statuses(signatures).await {
                        let statuses = result.value;
                        for (signature, status) in signatures.iter().zip(statuses) {
                            if let Some((status, data)) = status
                                .filter(|status| {
                                    status.satisfies_commitment(rpc_client.commitment())
                                })
                                .and_then(|status| {
                                    unconfirmed_transaction_map
                                        .remove(signature)
                                        .map(|(_, data)| (status, data))
                                })
                            {
                                num_confirmed_transactions.fetch_add(1, Ordering::Relaxed);
                                match status.err {
                                    Some(TransactionError::AlreadyProcessed) | None => {}
                                    Some(error) => {
                                        errors_map.insert(data.index, error);
                                    }
                                }
                            };
                        }
                    }
                }

                last_block_height = current_block_height;
            }
            // Wake the resend loop with a fresh view of what has landed.
            confirmation_signal.notify_one();
        }
    })
}

#[derive(Clone, Debug)]
struct SendingContext {
    unconfirmed_transaction_map: Arc<DashMap<Signature, TransactionData>>,
    error_map: Arc<DashMap<usize, TransactionError>>,
    blockhash_data_rw: Arc<RwLock<BlockHashData>>,
    num_confirmed_transactions: Arc<AtomicUsize>,
    total_transactions: usize,
    current_block_height: Arc<AtomicU64>,
    /// Delay between consecutive transactions within a send pass. Transaction
    /// `n` is delayed by `n * send_interval`, so this caps the per-pass send
    /// rate at `1 / send_interval`.
    send_interval: Duration,
    /// Liveness fallback for the resend loop when the confirmation task is not
    /// signaling (e.g. RPC stalled). The signal below is the primary driver.
    check_interval: Duration,
    /// Signaled by the confirmation task after each poll, so the resend loop
    /// only resends once it has a fresh view of what has landed.
    confirmation_signal: Arc<Notify>,
}
fn progress_from_context_and_block_height(
    context: &SendingContext,
    last_valid_block_height: u64,
) -> SendTransactionProgress {
    SendTransactionProgress {
        confirmed_transactions: context
            .num_confirmed_transactions
            .load(std::sync::atomic::Ordering::Relaxed),
        total_transactions: context.total_transactions,
        block_height: context
            .current_block_height
            .load(std::sync::atomic::Ordering::Relaxed),
        last_valid_block_height,
    }
}

async fn send_transaction_with_rpc_fallback<S: WireTransactionSender>(
    rpc_client: &RpcClient,
    tpu_client: &Option<S>,
    transaction: VersionedTransaction,
    serialized_transaction: Vec<u8>,
    context: &SendingContext,
    index: usize,
    rpc_send_transaction_config: Option<RpcSendTransactionConfig>,
) -> Result<()> {
    // Prefer to send directly to the leader when possible.
    let handed_off_over_tpu = match tpu_client {
        Some(tpu_client) => tpu_client.send(serialized_transaction).await,
        None => false,
    };
    if handed_off_over_tpu {
        return Ok(());
    }
    // The hand-off failed or there is no TPU transport.
    // The expect can only panic in v3 version of the API where RPC send config is mutually
    // exclusive with TPU send config.
    let rpc_send_transaction_config = rpc_send_transaction_config
        .expect("TPU client must outlive the transaction sending process.");

    if let Err(e) = rpc_client
        .send_transaction_with_config(
            &transaction,
            RpcSendTransactionConfig {
                preflight_commitment: Some(rpc_client.commitment().commitment),
                ..rpc_send_transaction_config
            },
        )
        .await
    {
        match e.kind() {
            ErrorKind::Io(_) | ErrorKind::Reqwest(_) => {
                // fall through on io error, we will retry the transaction
            }
            ErrorKind::TransactionError(TransactionError::BlockhashNotFound) => {
                // fall through so that we will resend with another blockhash
            }
            ErrorKind::TransactionError(transaction_error) => {
                // if we get other than blockhash not found error the transaction is invalid
                context.error_map.insert(index, transaction_error.clone());
            }
            ErrorKind::RpcError(RpcError::RpcResponseError {
                data:
                    RpcResponseErrorData::SendTransactionPreflightFailure(
                        RpcSimulateTransactionResult {
                            err: Some(ui_transaction_error),
                            ..
                        },
                    ),
                ..
            }) => {
                match TransactionError::from(ui_transaction_error.clone()) {
                    TransactionError::BlockhashNotFound => {
                        // fall through so that we will resend with another blockhash
                    }
                    err => {
                        // if we get other than blockhash not found error the transaction is invalid
                        context.error_map.insert(index, err);
                    }
                }
            }
            _ => {
                return Err(TpuSenderError::from(e));
            }
        }
    }
    Ok(())
}

/// Signs `message` with `signers`.
///
/// When `dedupe_signers` is set, a signer that appears more than once (e.g. the
/// same key used as both fee payer and authority) is tolerated by matching each
/// required signer against the `signers` entry with that pubkey. The
/// legacy [`solana_transaction::Transaction::try_sign`] deduplicated signers
/// this way, but [`VersionedTransaction::try_new`] rejects the redundant entry
/// as `TooManySigners`. This flag preserves the old behavior for the deprecated
/// v2 API; v3 callers get the strict `try_new` semantics.
fn sign_versioned_message<T: Signers + ?Sized>(
    message: VersionedMessage,
    signers: &T,
    dedupe_signers: bool,
) -> std::result::Result<VersionedTransaction, SignerError> {
    // Once send_and_confirm_transactions_in_parallel_v2 is retired, this fn should be retired,
    // and the segment below inlined into callers.
    if !dedupe_signers {
        return VersionedTransaction::try_new(message, signers);
    }
    let signer_keys = signers.try_pubkeys()?;
    let unordered_signatures = signers.try_sign_message(&message.serialize())?;
    let account_keys = message.static_account_keys();
    let num_required_signatures = message.header().num_required_signatures as usize;
    let signatures = account_keys
        .get(..num_required_signatures)
        .ok_or_else(|| SignerError::InvalidInput("invalid message".to_string()))?
        .iter()
        .map(|required_key| {
            signer_keys
                .iter()
                .position(|key| key == required_key)
                .and_then(|i| unordered_signatures.get(i).copied())
                .ok_or(SignerError::NotEnoughSigners)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(VersionedTransaction {
        signatures,
        message,
    })
}

async fn sign_all_messages_and_send<T: Signers + ?Sized, S: WireTransactionSender>(
    progress_bar: &Option<indicatif::ProgressBar>,
    rpc_client: &RpcClient,
    tpu_client: &Option<S>,
    messages_with_index: Vec<(usize, VersionedMessage)>,
    signers: &T,
    dedupe_signers: bool,
    context: &SendingContext,
    rpc_send_transaction_config: Option<RpcSendTransactionConfig>,
) -> Result<()> {
    let current_transaction_count = messages_with_index.len();
    let mut futures = vec![];
    // send all the transaction messages
    for (counter, (index, message)) in messages_with_index.into_iter().enumerate() {
        futures.push(async move {
            sleep(context.send_interval.saturating_mul(counter as u32)).await;
            let blockhashdata = *context.blockhash_data_rw.read().await;

            let mut message = message;
            message.set_recent_blockhash(blockhashdata.blockhash);
            // we have already checked if all transactions are signable.
            let transaction = sign_versioned_message(message.clone(), signers, dedupe_signers)
                .expect("Transaction should be signable");
            let serialized_transaction =
                serialize(&transaction).expect("Transaction should serialize");
            let signature = transaction.signatures[0];

            // send to confirm the transaction
            context.unconfirmed_transaction_map.insert(
                signature,
                TransactionData {
                    index,
                    serialized_transaction: serialized_transaction.clone(),
                    last_valid_block_height: blockhashdata.last_valid_block_height,
                    message,
                },
            );
            if let Some(progress_bar) = progress_bar {
                let progress = progress_from_context_and_block_height(
                    context,
                    blockhashdata.last_valid_block_height,
                );
                progress.set_message_for_confirmed_transactions(
                    progress_bar,
                    &format!(
                        "Sending {}/{} transactions",
                        counter + 1,
                        current_transaction_count,
                    ),
                );
            }
            send_transaction_with_rpc_fallback(
                rpc_client,
                tpu_client,
                transaction,
                serialized_transaction,
                context,
                index,
                rpc_send_transaction_config,
            )
            .await
        });
    }
    // collect to convert Vec<Result<_>> to Result<Vec<_>>
    join_all(futures)
        .await
        .into_iter()
        .collect::<Result<Vec<()>>>()?;
    Ok(())
}

async fn confirm_transactions_till_block_height_and_resend_unexpired_transaction_over_tpu<
    S: WireTransactionSender,
>(
    progress_bar: &Option<indicatif::ProgressBar>,
    tpu_client: &Option<S>,
    context: &SendingContext,
) {
    let unconfirmed_transaction_map = context.unconfirmed_transaction_map.clone();
    let current_block_height = context.current_block_height.clone();

    let transactions_to_confirm = unconfirmed_transaction_map.len();
    let max_valid_block_height = unconfirmed_transaction_map
        .iter()
        .map(|x| x.last_valid_block_height)
        .max();

    if let Some(mut max_valid_block_height) = max_valid_block_height {
        if let Some(progress_bar) = progress_bar {
            let progress = progress_from_context_and_block_height(context, max_valid_block_height);
            progress.set_message_for_confirmed_transactions(
                progress_bar,
                &format!(
                    "Waiting for next block, {transactions_to_confirm} transactions pending..."
                ),
            );
        }

        // wait till all transactions are confirmed or we have surpassed max processing age for the last sent transaction
        while !unconfirmed_transaction_map.is_empty()
            && current_block_height.load(Ordering::Relaxed) <= max_valid_block_height
        {
            // Resend only after the confirmation task reports a fresh view of
            // what has landed. The timeout is a liveness fallback in case the
            // confirmation task stalls, so the loop can still re-check its exit
            // condition (e.g. blockhash expiry).
            tokio::select! {
                () = context.confirmation_signal.notified() => {}
                () = tokio::time::sleep(context.check_interval.saturating_mul(2)) => {}
            }
            let block_height = current_block_height.load(Ordering::Relaxed);

            if let Some(tpu_client) = tpu_client {
                // retry sending transaction only over TPU port
                // any transactions sent over RPC will be automatically rebroadcast by the RPC server
                let txs_to_resend_over_tpu = unconfirmed_transaction_map
                    .iter()
                    .filter(|x| block_height < x.last_valid_block_height)
                    .map(|x| x.serialized_transaction.clone())
                    .collect::<Vec<_>>();
                send_staggered_transactions(
                    progress_bar,
                    tpu_client,
                    txs_to_resend_over_tpu,
                    max_valid_block_height,
                    context,
                )
                .await;
            }
            if let Some(max_valid_block_height_in_remaining_transaction) =
                unconfirmed_transaction_map
                    .iter()
                    .map(|x| x.last_valid_block_height)
                    .max()
            {
                max_valid_block_height = max_valid_block_height_in_remaining_transaction;
            }
        }
    }
}

async fn send_staggered_transactions<S: WireTransactionSender>(
    progress_bar: &Option<indicatif::ProgressBar>,
    tpu_client: &S,
    wire_transactions: Vec<Vec<u8>>,
    last_valid_block_height: u64,
    context: &SendingContext,
) {
    let current_transaction_count = wire_transactions.len();
    let futures = wire_transactions
        .into_iter()
        .enumerate()
        .map(|(counter, transaction)| async move {
            tokio::time::sleep(context.send_interval.saturating_mul(counter as u32)).await;
            if let Some(progress_bar) = progress_bar {
                let progress =
                    progress_from_context_and_block_height(context, last_valid_block_height);
                progress.set_message_for_confirmed_transactions(
                    progress_bar,
                    &format!(
                        "Resending {}/{} transactions",
                        counter + 1,
                        current_transaction_count,
                    ),
                );
            }
            tpu_client.send(transaction).await
        })
        .collect::<Vec<_>>();
    join_all(futures).await;
}

/// Sends and confirms transactions concurrently using the legacy
/// connection-cache [`QuicTpuClient`] as the TPU transport.
///
/// The sending and confirmation of transactions is done in parallel tasks
/// The method signs transactions just before sending so that blockhash does not
/// expire.
pub async fn send_and_confirm_transactions_in_parallel_v2<T: Signers + ?Sized>(
    rpc_client: Arc<RpcClient>,
    tpu_client: Option<QuicTpuClient>,
    messages: &[Message],
    signers: &T,
    config: SendAndConfirmConfigV2,
) -> Result<Vec<Option<TransactionError>>> {
    send_and_confirm_transactions_in_parallel_impl(
        rpc_client,
        tpu_client,
        messages.iter().cloned().map(VersionedMessage::Legacy),
        signers,
        config.into(),
    )
    .await
}

/// Sends and confirms transactions.
///
/// `rpc_client` is used to target the leader and confirm landing.
/// `transport` selects how transactions are sent.
/// `signers` should provide unique private keys to sign transactions.
///
/// Transactions are signed with a fresh blockhash if necessary to guarantee
/// eventual landing.
///
/// With TPU transport, set the RPC client commitment to Confirmed to minimize
/// retransmit latency.
pub async fn send_and_confirm_transactions_in_parallel_v3<T, M>(
    rpc_client: Arc<RpcClient>,
    transport: SendTransport,
    messages: M,
    signers: &T,
    config: SendAndConfirmConfigV3,
) -> Result<Vec<Option<TransactionError>>>
where
    T: Signers + ?Sized,
    M: IntoIterator<Item = VersionedMessage>,
{
    let (tpu_transaction_sender, rpc_send_transaction_config) = match transport {
        SendTransport::Tpu(sender) => (Some(sender), None),
        SendTransport::Rpc(rpc_config) => (None, Some(rpc_config)),
    };
    send_and_confirm_transactions_in_parallel_impl(
        rpc_client,
        tpu_transaction_sender,
        messages,
        signers,
        ParallelSendConfig {
            with_spinner: config.with_spinner,
            max_sign_attempts: config.max_sign_attempts,
            rpc_send_transaction_config,
            dedupe_signers: false,
            // Pace sends client-side: the transport backpressures to its own
            // capacity, but the leader still drops transactions it cannot ingest
            // fast enough, so full-rate sending just fuels resends.
            send_interval: config.send_interval,
            check_interval: config.check_interval,
        },
    )
    .await
}

async fn send_and_confirm_transactions_in_parallel_impl<T, S, M>(
    rpc_client: Arc<RpcClient>,
    tpu_transaction_sender: Option<S>,
    messages: M,
    signers: &T,
    config: ParallelSendConfig,
) -> Result<Vec<Option<TransactionError>>>
where
    T: Signers + ?Sized,
    S: WireTransactionSender,
    M: IntoIterator<Item = VersionedMessage>,
{
    let messages: Vec<VersionedMessage> = messages.into_iter().collect();

    // get current blockhash and corresponding last valid block height
    let (blockhash, last_valid_block_height) = rpc_client
        .get_latest_blockhash_with_commitment(rpc_client.commitment())
        .await?;
    let blockhash_data_rw = Arc::new(RwLock::new(BlockHashData {
        blockhash,
        last_valid_block_height,
    }));

    // check if all the messages are signable by the signers
    messages
        .iter()
        .map(|x| {
            let mut message = x.clone();
            message.set_recent_blockhash(blockhash);
            sign_versioned_message(message, signers, config.dedupe_signers).map(|_| ())
        })
        .collect::<std::result::Result<Vec<()>, SignerError>>()?;

    // get current block height
    let block_height = rpc_client.get_block_height().await?;
    let current_block_height = Arc::new(AtomicU64::new(block_height));

    let progress_bar = config.with_spinner.then(|| {
        let progress_bar = spinner::new_progress_bar();
        progress_bar.set_message("Setting up...");
        progress_bar
    });

    // blockhash and block height update task
    let block_data_task = create_blockhash_data_updating_task(
        rpc_client.clone(),
        blockhash_data_rw.clone(),
        current_block_height.clone(),
    );

    let unconfirmed_transasction_map = Arc::new(DashMap::<Signature, TransactionData>::new());
    let error_map = Arc::new(DashMap::new());
    let num_confirmed_transactions = Arc::new(AtomicUsize::new(0));
    let confirmation_signal = Arc::new(Notify::new());
    // tasks which confirms the transactions that were sent
    let transaction_confirming_task = create_transaction_confirmation_task(
        rpc_client.clone(),
        current_block_height.clone(),
        unconfirmed_transasction_map.clone(),
        error_map.clone(),
        num_confirmed_transactions.clone(),
        config.check_interval,
        confirmation_signal.clone(),
    );

    // transaction sender task
    let total_transactions = messages.len();
    let mut initial = true;
    let context = SendingContext {
        unconfirmed_transaction_map: unconfirmed_transasction_map.clone(),
        blockhash_data_rw: blockhash_data_rw.clone(),
        num_confirmed_transactions: num_confirmed_transactions.clone(),
        current_block_height: current_block_height.clone(),
        error_map: error_map.clone(),
        total_transactions,
        send_interval: config.send_interval,
        check_interval: config.check_interval,
        confirmation_signal,
    };

    for expired_blockhash_retries in (0..config.max_sign_attempts.get()).rev() {
        // only send messages which have not been confirmed
        let messages_with_index: Vec<(usize, VersionedMessage)> = if initial {
            initial = false;
            messages.iter().cloned().enumerate().collect()
        } else {
            // remove all the confirmed transactions
            unconfirmed_transasction_map
                .iter()
                .map(|x| (x.index, x.message.clone()))
                .collect()
        };

        if messages_with_index.is_empty() {
            break;
        }

        // clear the map so that we can start resending
        unconfirmed_transasction_map.clear();

        sign_all_messages_and_send(
            &progress_bar,
            &rpc_client,
            &tpu_transaction_sender,
            messages_with_index,
            signers,
            config.dedupe_signers,
            &context,
            config.rpc_send_transaction_config,
        )
        .await?;
        confirm_transactions_till_block_height_and_resend_unexpired_transaction_over_tpu(
            &progress_bar,
            &tpu_transaction_sender,
            &context,
        )
        .await;

        if unconfirmed_transasction_map.is_empty() {
            break;
        }

        if let Some(progress_bar) = &progress_bar {
            progress_bar.println(format!(
                "Blockhash expired. {expired_blockhash_retries} retries remaining"
            ));
        }
    }

    block_data_task.abort();
    transaction_confirming_task.abort();
    if unconfirmed_transasction_map.is_empty() {
        let mut transaction_errors = vec![None; messages.len()];
        for iterator in error_map.iter() {
            transaction_errors[*iterator.key()] = Some(iterator.value().clone());
        }
        Ok(transaction_errors)
    } else {
        Err(TpuSenderError::Custom("Max retries exceeded".into()))
    }
}
