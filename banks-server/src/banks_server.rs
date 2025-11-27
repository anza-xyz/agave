use {
    bincode::{deserialize, serialize},
    crossbeam_channel::{unbounded, Receiver, Sender},
    futures::{future, prelude::stream::StreamExt},
    solana_account::Account,
    solana_banks_interface::{
        Banks, BanksRequest, BanksResponse, BanksTransactionResultWithMetadata,
        BanksTransactionResultWithSimulation, TransactionConfirmationStatus, TransactionMetadata,
        TransactionSimulationDetails, TransactionStatus,
    },
    solana_clock::Slot,
    solana_commitment_config::CommitmentLevel,
    solana_hash::Hash,
    solana_message::{Message, SanitizedMessage},
    solana_net_utils::sockets::{bind_to, localhost_port_range_for_tests},
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, TransactionSimulationResult},
        bank_forks::BankForks,
        commitment::BlockCommitmentCache,
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_send_transaction_service::{
        send_transaction_service::{Config, SendTransactionService, TransactionInfo},
        tpu_info::NullTpuInfo,
        transaction_client::TpuClientNextClient,
    },
    solana_signature::Signature,
    solana_transaction::{
        sanitized::{MessageHash, SanitizedTransaction},
        versioned::VersionedTransaction,
    },
    std::{
        io,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::Builder,
        time::Duration,
    },
    tarpc::{
        context::Context,
        serde_transport::tcp,
        server::{self, incoming::Incoming, Channel},
        transport::{self, channel::UnboundedChannel},
        ClientMessage, Response,
    },
    tokio::{runtime::Handle, time::sleep},
    tokio_serde::formats::Bincode,
    tokio_util::sync::CancellationToken,
};

mod transaction {
    pub use solana_transaction_error::TransactionResult as Result;
}

#[derive(Clone)]
struct BanksServer {
    bank_forks: Arc<RwLock<BankForks>>,
    block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
    transaction_sender: Sender<TransactionInfo>,
    poll_signature_status_sleep_duration: Duration,
}

impl BanksServer {
    /// Return a BanksServer that forwards transactions to the
    /// given sender. If unit-testing, those transactions can go to
    /// a bank in the given BankForks. Otherwise, the receiver should
    /// forward them to a validator in the leader schedule.
    fn new(
        bank_forks: Arc<RwLock<BankForks>>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        transaction_sender: Sender<TransactionInfo>,
        poll_signature_status_sleep_duration: Duration,
    ) -> Self {
        Self {
            bank_forks,
            block_commitment_cache,
            transaction_sender,
            poll_signature_status_sleep_duration,
        }
    }

    fn run(bank_forks: Arc<RwLock<BankForks>>, transaction_receiver: Receiver<TransactionInfo>) {
        while let Ok(info) = transaction_receiver.recv() {
            let mut transaction_infos = vec![info];
            while let Ok(info) = transaction_receiver.try_recv() {
                transaction_infos.push(info);
            }
            let transactions: Vec<_> = transaction_infos
                .into_iter()
                .map(|info| deserialize(&info.wire_transaction).unwrap())
                .collect();
            loop {
                let bank = bank_forks.read().unwrap().working_bank();
                // bank forks lock released, now verify bank hasn't been frozen yet
                // in the mean-time the bank can not be frozen until this tx batch
                // has been processed
                let lock = bank.freeze_lock();
                if *lock == Hash::default() {
                    let _ = bank.try_process_entry_transactions(transactions);
                    // break out of inner loop and release bank freeze lock
                    break;
                }
            }
        }
    }

    /// Useful for unit-testing
    fn new_loopback(
        bank_forks: Arc<RwLock<BankForks>>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        poll_signature_status_sleep_duration: Duration,
    ) -> Self {
        let (transaction_sender, transaction_receiver) = unbounded();
        let bank = bank_forks.read().unwrap().working_bank();
        let slot = bank.slot();
        {
            // ensure that the commitment cache and bank are synced
            let mut w_block_commitment_cache = block_commitment_cache.write().unwrap();
            w_block_commitment_cache.set_all_slots(slot, slot);
        }
        let server_bank_forks = bank_forks.clone();
        Builder::new()
            .name("solBankForksCli".to_string())
            .spawn(move || Self::run(server_bank_forks, transaction_receiver))
            .unwrap();
        Self::new(
            bank_forks,
            block_commitment_cache,
            transaction_sender,
            poll_signature_status_sleep_duration,
        )
    }

    fn slot(&self, commitment: CommitmentLevel) -> Slot {
        self.block_commitment_cache
            .read()
            .unwrap()
            .slot_with_commitment(commitment)
    }

    fn bank(&self, commitment: CommitmentLevel) -> Arc<Bank> {
        self.bank_forks.read().unwrap()[self.slot(commitment)].clone()
    }

    async fn poll_signature_status(
        self,
        signature: &Signature,
        blockhash: &Hash,
        last_valid_block_height: u64,
        commitment: CommitmentLevel,
    ) -> Option<transaction::Result<()>> {
        let mut status = self
            .bank(commitment)
            .get_signature_status_with_blockhash(signature, blockhash);
        while status.is_none() {
            sleep(self.poll_signature_status_sleep_duration).await;
            let bank = self.bank(commitment);
            if bank.block_height() > last_valid_block_height {
                break;
            }
            status = bank.get_signature_status_with_blockhash(signature, blockhash);
        }
        status
    }
}

fn simulate_transaction(
    bank: &Bank,
    transaction: VersionedTransaction,
) -> BanksTransactionResultWithSimulation {
    let sanitized_transaction = match RuntimeTransaction::try_create(
        transaction,
        MessageHash::Compute,
        Some(false), // is_simple_vote_tx
        bank,
        bank.get_reserved_account_keys(),
        bank.feature_set
            .is_active(&agave_feature_set::static_instruction_limit::id()),
    ) {
        Err(err) => {
            return BanksTransactionResultWithSimulation {
                result: Some(Err(err)),
                simulation_details: None,
            };
        }
        Ok(tx) => tx,
    };
    let TransactionSimulationResult {
        result,
        logs,
        post_simulation_accounts: _,
        units_consumed,
        loaded_accounts_data_size,
        return_data,
        inner_instructions,
        fee: _,
        pre_balances: _,
        post_balances: _,
        pre_token_balances: _,
        post_token_balances: _,
    } = bank.simulate_transaction_unchecked(&sanitized_transaction, true);

    let simulation_details = TransactionSimulationDetails {
        logs,
        units_consumed,
        loaded_accounts_data_size,
        return_data,
        inner_instructions,
    };
    BanksTransactionResultWithSimulation {
        result: Some(result),
        simulation_details: Some(simulation_details),
    }
}

#[tarpc::server]
impl Banks for BanksServer {
    async fn send_transaction_with_context(self, _: Context, transaction: VersionedTransaction) {
        let message_hash = transaction.message.hash();
        let blockhash = transaction.message.recent_blockhash();
        let last_valid_block_height = self
            .bank_forks
            .read()
            .unwrap()
            .root_bank()
            .get_blockhash_last_valid_block_height(blockhash)
            .unwrap();
        let signature = transaction.signatures.first().cloned().unwrap_or_default();
        let info = TransactionInfo::new(
            message_hash,
            signature,
            *blockhash,
            serialize(&transaction).unwrap(),
            last_valid_block_height,
            None,
            None,
            None,
        );
        self.transaction_sender.send(info).unwrap();
    }

    async fn get_transaction_status_with_context(
        self,
        _: Context,
        signature: Signature,
    ) -> Option<TransactionStatus> {
        let bank = self.bank(CommitmentLevel::Processed);
        let (slot, status) = bank.get_signature_status_slot(&signature)?;
        let r_block_commitment_cache = self.block_commitment_cache.read().unwrap();

        let optimistically_confirmed_bank = self.bank(CommitmentLevel::Confirmed);
        let optimistically_confirmed =
            optimistically_confirmed_bank.get_signature_status_slot(&signature);

        let confirmations = if r_block_commitment_cache.root() >= slot
            && r_block_commitment_cache.highest_super_majority_root() >= slot
        {
            None
        } else {
            r_block_commitment_cache
                .get_confirmation_count(slot)
                .or(Some(0))
        };
        Some(TransactionStatus {
            slot,
            confirmations,
            err: status.err(),
            confirmation_status: if confirmations.is_none() {
                Some(TransactionConfirmationStatus::Finalized)
            } else if optimistically_confirmed.is_some() {
                Some(TransactionConfirmationStatus::Confirmed)
            } else {
                Some(TransactionConfirmationStatus::Processed)
            },
        })
    }

    async fn get_slot_with_context(self, _: Context, commitment: CommitmentLevel) -> Slot {
        self.slot(commitment)
    }

    async fn get_block_height_with_context(self, _: Context, commitment: CommitmentLevel) -> u64 {
        self.bank(commitment).block_height()
    }

    async fn process_transaction_with_preflight_and_commitment_and_context(
        self,
        ctx: Context,
        transaction: VersionedTransaction,
        commitment: CommitmentLevel,
    ) -> BanksTransactionResultWithSimulation {
        let mut simulation_result =
            simulate_transaction(&self.bank(commitment), transaction.clone());
        // Simulation was ok, so process the real transaction and replace the
        // simulation's result with the real transaction result
        if let Some(Ok(_)) = simulation_result.result {
            simulation_result.result = self
                .process_transaction_with_commitment_and_context(ctx, transaction, commitment)
                .await;
        }
        simulation_result
    }

    async fn simulate_transaction_with_commitment_and_context(
        self,
        _: Context,
        transaction: VersionedTransaction,
        commitment: CommitmentLevel,
    ) -> BanksTransactionResultWithSimulation {
        simulate_transaction(&self.bank(commitment), transaction)
    }

    async fn process_transaction_with_commitment_and_context(
        self,
        _: Context,
        transaction: VersionedTransaction,
        commitment: CommitmentLevel,
    ) -> Option<transaction::Result<()>> {
        let bank = self.bank(commitment);
        let sanitized_transaction = match SanitizedTransaction::try_create(
            transaction.clone(),
            MessageHash::Compute,
            Some(false), // is_simple_vote_tx
            bank.as_ref(),
            bank.get_reserved_account_keys(),
        ) {
            Ok(tx) => tx,
            Err(err) => return Some(Err(err)),
        };

        if let Err(err) = sanitized_transaction.verify() {
            return Some(Err(err));
        }

        let message_hash = sanitized_transaction.message_hash();
        let blockhash = transaction.message.recent_blockhash();
        let last_valid_block_height = self
            .bank(commitment)
            .get_blockhash_last_valid_block_height(blockhash)
            .unwrap();
        let signature = sanitized_transaction.signature();
        let info = TransactionInfo::new(
            *message_hash,
            *signature,
            *blockhash,
            serialize(&transaction).unwrap(),
            last_valid_block_height,
            None,
            None,
            None,
        );
        self.transaction_sender.send(info).unwrap();
        self.poll_signature_status(signature, blockhash, last_valid_block_height, commitment)
            .await
    }

    async fn process_transaction_with_metadata_and_context(
        self,
        _: Context,
        transaction: VersionedTransaction,
    ) -> BanksTransactionResultWithMetadata {
        let bank = self.bank_forks.read().unwrap().working_bank();
        match bank.process_transaction_with_metadata(transaction) {
            Err(error) => BanksTransactionResultWithMetadata {
                result: Err(error),
                metadata: None,
            },
            Ok(details) => BanksTransactionResultWithMetadata {
                result: details.status,
                metadata: Some(TransactionMetadata {
                    compute_units_consumed: details.executed_units,
                    log_messages: details.log_messages.unwrap_or_default(),
                    return_data: details.return_data,
                }),
            },
        }
    }

    async fn get_account_with_commitment_and_context(
        self,
        _: Context,
        address: Pubkey,
        commitment: CommitmentLevel,
    ) -> Option<Account> {
        let bank = self.bank(commitment);
        bank.get_account(&address).map(Account::from)
    }

    async fn get_latest_blockhash_with_context(self, _: Context) -> Hash {
        let bank = self.bank(CommitmentLevel::default());
        bank.last_blockhash()
    }

    async fn get_latest_blockhash_with_commitment_and_context(
        self,
        _: Context,
        commitment: CommitmentLevel,
    ) -> Option<(Hash, u64)> {
        let bank = self.bank(commitment);
        let blockhash = bank.last_blockhash();
        let last_valid_block_height = bank.get_blockhash_last_valid_block_height(&blockhash)?;
        Some((blockhash, last_valid_block_height))
    }

    async fn get_fee_for_message_with_commitment_and_context(
        self,
        _: Context,
        message: Message,
        commitment: CommitmentLevel,
    ) -> Option<u64> {
        let bank = self.bank(commitment);
        let sanitized_message =
            SanitizedMessage::try_from_legacy_message(message, bank.get_reserved_account_keys())
                .ok()?;
        bank.get_fee_for_message(&sanitized_message)
    }
}

pub async fn start_local_server(
    bank_forks: Arc<RwLock<BankForks>>,
    block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
    poll_signature_status_sleep_duration: Duration,
) -> UnboundedChannel<Response<BanksResponse>, ClientMessage<BanksRequest>> {
    let banks_server = BanksServer::new_loopback(
        bank_forks,
        block_commitment_cache,
        poll_signature_status_sleep_duration,
    );
    let (client_transport, server_transport) = transport::channel::unbounded();
    let server = server::BaseChannel::with_defaults(server_transport).execute(banks_server.serve());
    tokio::spawn(server);
    client_transport
}
fn create_client(
    maybe_runtime: Option<Handle>,
    my_tpu_address: SocketAddr,
    exit: Arc<AtomicBool>,
) -> TpuClientNextClient {
    let runtime_handle = maybe_runtime.unwrap_or_else(|| {
        Handle::try_current().expect("runtime handle not provided, and not inside Tokio runtime")
    });
    let port_range = localhost_port_range_for_tests();
    let bind_socket = bind_to(IpAddr::V4(Ipv4Addr::LOCALHOST), port_range.0)
        .expect("Should be able to open UdpSocket for tests.");

    let cancel = CancellationToken::new();
    runtime_handle.spawn({
        let exit = Arc::clone(&exit);
        let cancel = cancel.clone();

        async move {
            loop {
                if exit.load(Ordering::Relaxed) {
                    cancel.cancel();
                    break;
                }

                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    });

    let leader_forward_count = 0;
    TpuClientNextClient::new::<NullTpuInfo>(
        runtime_handle,
        my_tpu_address,
        None,
        None,
        leader_forward_count,
        None,
        bind_socket,
        cancel,
    )
}

pub async fn start_tcp_server(
    listen_addr: SocketAddr,
    tpu_addr: SocketAddr,
    bank_forks: Arc<RwLock<BankForks>>,
    block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
    exit: Arc<AtomicBool>,
) -> io::Result<()> {
    // Note: These settings are copied straight from the tarpc example.
    let server = tcp::listen(listen_addr, Bincode::default)
        .await?
        // Ignore accept errors.
        .filter_map(|r| future::ready(r.ok()))
        .map(server::BaseChannel::with_defaults)
        // Limit channels to 1 per IP.
        .max_channels_per_key(1, |t| {
            t.as_ref()
                .peer_addr()
                .map(|x| x.ip())
                .unwrap_or_else(|_| Ipv4Addr::UNSPECIFIED.into())
        })
        // serve is generated by the service attribute. It takes as input any type implementing
        // the generated Banks trait.
        .map(move |chan| {
            let (sender, receiver) = unbounded();

            let client = create_client(None, tpu_addr, exit.clone());

            SendTransactionService::new(
                &bank_forks,
                receiver,
                client,
                Config {
                    retry_rate_ms: 5_000,
                    ..Config::default()
                },
                exit.clone(),
            );

            let server = BanksServer::new(
                bank_forks.clone(),
                block_commitment_cache.clone(),
                sender,
                Duration::from_millis(200),
            );
            chan.execute(server.serve())
        })
        // Max 10 channels.
        .buffer_unordered(10)
        .for_each(|_| async {});

    server.await;
    Ok(())
}
