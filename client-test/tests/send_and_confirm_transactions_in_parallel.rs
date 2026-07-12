use {
    async_trait::async_trait,
    solana_client::{
        nonblocking::tpu_client::TpuClient,
        rpc_config::RpcSendTransactionConfig,
        send_and_confirm_transactions_in_parallel::{
            SendAndConfirmConfigV2, SendAndConfirmConfigV3, SendTransport,
            send_and_confirm_transactions_in_parallel_blocking_v2,
            send_and_confirm_transactions_in_parallel_v3,
        },
    },
    solana_commitment_config::CommitmentConfig,
    solana_keypair::Keypair,
    solana_message::{Message, VersionedMessage},
    solana_native_token::LAMPORTS_PER_SOL,
    solana_net_utils::{SocketAddrSpace, sockets},
    solana_packet::PACKET_DATA_SIZE,
    solana_pubkey::Pubkey,
    solana_rpc_client::{
        nonblocking::rpc_client::RpcClient as NonblockingRpcClient, rpc_client::RpcClient,
    },
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_test_validator::TestValidator,
    solana_tpu_client_next::{
        client_builder::ClientBuilder,
        connection_workers_scheduler::{ConnectionWorkersSchedulerError, WorkersBroadcaster},
        leader_updater::create_pinned_leader_updater,
        transaction_batch::TransactionBatch,
        workers_cache::WorkersCache,
    },
    spl_memo_interface::{instruction::build_memo, v3 as memo_program},
    std::{
        net::SocketAddr,
        num::NonZeroUsize,
        sync::Arc,
        time::{Duration, Instant},
    },
    test_case::test_case,
    tokio::runtime::Runtime,
};

const NUM_TRANSACTIONS: usize = 1000;

fn create_messages(from: Pubkey, to: Pubkey) -> (Vec<Message>, u64) {
    let mut messages = vec![];
    let mut sum = 0u64;
    for i in 1..NUM_TRANSACTIONS {
        let amount_to_transfer = (i as u64).checked_mul(LAMPORTS_PER_SOL).unwrap();
        let ix = system_instruction::transfer(&from, &to, amount_to_transfer);
        let message = Message::new(&[ix], Some(&from));
        messages.push(message);
        sum = sum.checked_add(amount_to_transfer).unwrap();
    }
    (messages, sum)
}

#[test]
fn test_send_and_confirm_transactions_in_parallel_without_tpu_client() {
    agave_logger::setup();

    let alice = Keypair::new();
    let test_validator =
        TestValidator::start_with_config(alice.pubkey(), None, SocketAddrSpace::Unspecified);

    let bob_pubkey = solana_pubkey::new_rand();
    let alice_pubkey = alice.pubkey();

    let rpc_client = Arc::new(RpcClient::new(test_validator.rpc_url()));

    assert_eq!(
        rpc_client.get_version().unwrap().solana_core,
        solana_version::semver!()
    );

    let original_alice_balance = rpc_client.get_balance(&alice.pubkey()).unwrap();
    let (messages, sum) = create_messages(alice_pubkey, bob_pubkey);
    let mut fee_message = messages.first().unwrap().clone();
    fee_message.recent_blockhash = rpc_client.get_latest_blockhash().unwrap();
    let total_fees = rpc_client
        .get_fee_for_message(&fee_message)
        .unwrap()
        .saturating_mul(messages.len() as u64);

    let txs_errors = send_and_confirm_transactions_in_parallel_blocking_v2(
        rpc_client.clone(),
        None,
        &messages,
        &[&alice],
        SendAndConfirmConfigV2 {
            with_spinner: false,
            resign_txs_count: Some(5),
            rpc_send_transaction_config: RpcSendTransactionConfig {
                skip_preflight: false,
                preflight_commitment: Some(CommitmentConfig::confirmed().commitment),
                encoding: None,
                max_retries: None,
                min_context_slot: None,
            },
        },
    );
    assert!(txs_errors.is_ok());
    assert!(txs_errors.unwrap().iter().all(|x| x.is_none()));

    assert_eq!(
        rpc_client
            .get_balance_with_commitment(&bob_pubkey, CommitmentConfig::processed())
            .unwrap()
            .value,
        sum
    );
    assert_eq!(
        rpc_client
            .get_balance_with_commitment(&alice_pubkey, CommitmentConfig::processed())
            .unwrap()
            .value,
        original_alice_balance - sum - total_fees
    );
}

#[test]
fn test_send_and_confirm_transactions_in_parallel_with_tpu_client() {
    agave_logger::setup();

    let alice = Keypair::new();
    let test_validator =
        TestValidator::start_with_config(alice.pubkey(), None, SocketAddrSpace::Unspecified);

    let bob_pubkey = solana_pubkey::new_rand();
    let alice_pubkey = alice.pubkey();

    let rpc_client = Arc::new(RpcClient::new(test_validator.rpc_url()));

    assert_eq!(
        rpc_client.get_version().unwrap().solana_core,
        solana_version::semver!()
    );

    let original_alice_balance = rpc_client.get_balance(&alice.pubkey()).unwrap();
    let (messages, sum) = create_messages(alice_pubkey, bob_pubkey);
    let mut fee_message = messages.first().unwrap().clone();
    fee_message.recent_blockhash = rpc_client.get_latest_blockhash().unwrap();
    let total_fees = rpc_client
        .get_fee_for_message(&fee_message)
        .unwrap()
        .saturating_mul(messages.len() as u64);
    let ws_url = test_validator.rpc_pubsub_url();
    let tpu_client_fut = TpuClient::new(
        "temp",
        rpc_client.get_inner_client().clone(),
        ws_url.as_str(),
        solana_client::tpu_client::TpuClientConfig::default(),
    );
    let tpu_client = rpc_client.runtime().block_on(tpu_client_fut).unwrap();

    let txs_errors = send_and_confirm_transactions_in_parallel_blocking_v2(
        rpc_client.clone(),
        Some(tpu_client),
        &messages,
        &[&alice],
        SendAndConfirmConfigV2 {
            with_spinner: false,
            resign_txs_count: Some(5),
            rpc_send_transaction_config: RpcSendTransactionConfig {
                skip_preflight: false,
                preflight_commitment: Some(CommitmentConfig::confirmed().commitment),
                encoding: None,
                max_retries: None,
                min_context_slot: None,
            },
        },
    );
    assert!(txs_errors.is_ok());
    assert!(txs_errors.unwrap().iter().all(|x| x.is_none()));

    assert_eq!(
        rpc_client
            .get_balance_with_commitment(&bob_pubkey, CommitmentConfig::processed())
            .unwrap()
            .value,
        sum
    );
    assert_eq!(
        rpc_client
            .get_balance_with_commitment(&alice_pubkey, CommitmentConfig::processed())
            .unwrap()
            .value,
        original_alice_balance - sum - total_fees
    );
}

/// Builds a memo message with a `memo_data_len`-byte payload. `tag` is embedded
/// in the memo so that every message has a distinct signature (and is therefore
/// not deduped by the TPU). `memo_data_len` must be at least 8 bytes.
fn tagged_memo_message(payer: &Pubkey, memo_data_len: usize, tag: usize) -> Message {
    let mut memo = vec![b'a'; memo_data_len];
    // Zero-padded decimal keeps the memo valid UTF-8 while making it unique.
    let tag = format!("{tag:016}");
    memo[..tag.len()].copy_from_slice(tag.as_bytes());
    Message::new(&[build_memo(&memo_program::id(), &memo, &[])], Some(payer))
}

/// [`BackpressuredBroadcaster`] sends transactions to all the workers, awaiting
/// free capacity on each worker's channel instead of dropping the batch when
/// the channel is full (as the default `NonblockingBroadcaster` does).
///
/// Note: leaders in the fanout are served sequentially, so a particularly slow
/// leader delays delivery to the remaining leaders in the same batch.
struct BackpressuredBroadcaster;

#[async_trait]
impl WorkersBroadcaster for BackpressuredBroadcaster {
    async fn send_to_workers(
        &self,
        workers: &mut WorkersCache,
        leaders: &[SocketAddr],
        transaction_batch: TransactionBatch,
    ) -> Result<(), ConnectionWorkersSchedulerError> {
        for leader in leaders {
            // Unlike `try_send_transactions_to_address`, this awaits until the
            // worker channel has room, so a full channel never surfaces as an
            // error and the batch is not dropped.
            let send_res = workers
                .send_transactions_to_address(leader, transaction_batch.clone())
                .await;
            if let Err(err) = send_res {
                log::debug!("Failed to send transactions to {leader:?}, worker send error: {err}.");
                // `send_transactions_to_address` already evicts the worker on
                // `ReceiverDropped`; the remaining errors (`WorkerNotFound`,
                // `ShutdownError`) are transient/expected and non-fatal here.
            }
        }
        Ok(())
    }
}

/// Submits a large burst of maximum-size transactions through the tpu-client-next
/// transport of `send_and_confirm_transactions_in_parallel_v3` and asserts every
/// one lands.
#[allow(clippy::arithmetic_side_effects)]
#[test_case(true; "Use staked connection")]
#[test_case(false; "Use unstaked connection")]
fn test_send_and_confirm_transactions_in_parallel_v3(staked: bool) {
    /// Total wire bytes to push through the transport.
    const TARGET_TOTAL_BYTES: usize = 10 * 1024 * 1024;
    /// Each transaction is packet-sized, so this many of them ~= the target.
    const NUM_MAX_SIZE_MESSAGES: usize = TARGET_TOTAL_BYTES.div_ceil(PACKET_DATA_SIZE);
    /// Serialized size of the signatures for a legacy
    /// transaction with one signer (which is what we send here)
    const SIGNED_TX_SIGNATURES_LEN: usize = 1 + 64;
    agave_logger::setup();

    let signer = Keypair::new();
    let test_validator =
        TestValidator::start_with_config(signer.pubkey(), None, SocketAddrSpace::Unspecified);
    let signer = if staked {
        test_validator.cluster_info().keypair().insecure_clone()
    } else {
        signer
    };
    let fee_payer = signer.pubkey();

    let rpc_client = Arc::new(NonblockingRpcClient::new_with_commitment(
        test_validator.rpc_url(),
        CommitmentConfig::confirmed(),
    ));
    let runtime = Runtime::new().unwrap();

    // Figure out padding for memo so its transaction fills a packet
    let empty_memo = Message::new(
        &[build_memo(&memo_program::id(), &[], &[])],
        Some(&fee_payer),
    );
    let empty_memo_len = SIGNED_TX_SIGNATURES_LEN + empty_memo.serialize().len();
    let memo_padding_len = PACKET_DATA_SIZE - empty_memo_len - 1;

    let messages: Vec<VersionedMessage> = (0..NUM_MAX_SIZE_MESSAGES)
        .map(|tag| VersionedMessage::Legacy(tagged_memo_message(&fee_payer, memo_padding_len, tag)))
        .collect();
    let mut total_wire_bytes = 0usize;
    for message in &messages {
        let tx_len = SIGNED_TX_SIGNATURES_LEN + message.serialize().len();
        assert!(
            (PACKET_DATA_SIZE - 2..=PACKET_DATA_SIZE).contains(&tx_len),
            "each transaction must be packet-sized, got {tx_len} bytes"
        );
        total_wire_bytes += tx_len;
    }

    let bind_socket = sockets::bind_to_localhost_unique().unwrap();
    let leader_updater = create_pinned_leader_updater(*test_validator.tpu_quic());

    let (transaction_sender, client) = runtime.block_on(async {
        ClientBuilder::new(leader_updater)
            .bind_socket(bind_socket)
            .identity(&signer)
            // Apply backpressure instead of dropping batches on a
            // full worker channel.
            .broadcaster(BackpressuredBroadcaster)
            .build()
            .expect("Failed to build TPU client")
    });
    //std::thread::sleep(Duration::from_secs(5));
    log::info!(
        "sending {NUM_MAX_SIZE_MESSAGES} transactions ({:.2} MB wire) over tpu-client-next",
        total_wire_bytes as f64 / (1024.0 * 1024.0)
    );
    let started = Instant::now();
    let result = runtime.block_on(send_and_confirm_transactions_in_parallel_v3(
        rpc_client.clone(),
        // TPU-only: never fall back to RPC for sending.
        SendTransport::Tpu(transaction_sender),
        messages,
        &[&signer],
        SendAndConfirmConfigV3 {
            with_spinner: true,
            // Large bursts may outlive a single blockhash; allow re-signing.
            max_sign_attempts: NonZeroUsize::new(16).unwrap(),
            // Recheck landing / resend at most once a second.
            check_interval: Duration::from_secs(1),
            // Pace at ~333 TPS: a single write-locked account saturates at
            // around 250 TPS, we could send more and just retransmit but why?.
            send_interval: Duration::from_millis(3),
        },
    ));
    let elapsed = started.elapsed();
    runtime.block_on(async {
        let _ = client.shutdown().await;
    });

    let transaction_errors = result.expect("all transactions must land within the resign window");
    assert_eq!(transaction_errors.len(), NUM_MAX_SIZE_MESSAGES);
    log::info!(
        "landed {NUM_MAX_SIZE_MESSAGES} transactions ({:.2} MB) in {:.2}s ({:.2} MB/s, {:.0} tx/s)",
        total_wire_bytes as f64 / TARGET_TOTAL_BYTES as f64,
        elapsed.as_secs_f64(),
        total_wire_bytes as f64 / TARGET_TOTAL_BYTES as f64 / elapsed.as_secs_f64(),
        NUM_MAX_SIZE_MESSAGES as f64 / elapsed.as_secs_f64(),
    );
}
