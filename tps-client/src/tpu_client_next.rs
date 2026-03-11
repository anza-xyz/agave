use {
    crate::{TpsClient, TpsClientError, TpsClientResult},
    bincode::serialize,
    solana_account::Account,
    solana_commitment_config::CommitmentConfig,
    solana_epoch_info::EpochInfo,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_rpc_client::{nonblocking::rpc_client::RpcClient as NonblockingRpcClient, rpc_client::RpcClient},
    solana_rpc_client_api::config::RpcBlockConfig,
    solana_signature::Signature,
    solana_tpu_client_next::{
        Client, ClientBuilder, TransactionSender,
        node_address_service::LeaderTpuCacheServiceConfig,
        websocket_node_address_service::WebsocketNodeAddressService,
    },
    solana_transaction::Transaction,
    solana_transaction_error::TransactionResult as Result,
    solana_transaction_status::UiConfirmedBlock,
    std::{
        net::{IpAddr, SocketAddr, UdpSocket},
        sync::Arc,
    },
    tokio::runtime::{Builder, Runtime},
    tokio_util::sync::CancellationToken,
};

pub struct TpuClientNextClient {
    rpc_client: Arc<RpcClient>,
    transaction_sender: TransactionSender,
    client: Option<Client>,
    runtime: Runtime,
}

impl TpuClientNextClient {
    pub fn new(
        json_rpc_url: &str,
        websocket_url: &str,
        commitment_config: CommitmentConfig,
        bind_address: IpAddr,
        identity: Option<&Keypair>,
    ) -> TpsClientResult<Self> {
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            json_rpc_url.to_string(),
            commitment_config,
        ));
        let async_rpc_client = Arc::new(NonblockingRpcClient::new_with_commitment(
            json_rpc_url.to_string(),
            commitment_config,
        ));
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(TpsClientError::from)?;
        let bind_socket = UdpSocket::bind(SocketAddr::new(bind_address, 0))
            .map_err(TpsClientError::from)?;
        let cancel = CancellationToken::new();
        let runtime_handle = runtime.handle().clone();
        let (transaction_sender, client) = runtime.block_on(async move {
            let leader_updater = WebsocketNodeAddressService::run(
                async_rpc_client,
                websocket_url.to_string(),
                LeaderTpuCacheServiceConfig::default(),
                cancel.clone(),
            )
            .await
            .map_err(|err| TpsClientError::Custom(err.to_string()))?;

            let mut builder = ClientBuilder::new(Box::new(leader_updater))
                .runtime_handle(runtime_handle)
                .bind_socket(bind_socket)
                .cancel_token(cancel);
            if let Some(identity) = identity {
                builder = builder.identity(identity);
            }
            builder
                .build()
                .map_err(|err| TpsClientError::Custom(err.to_string()))
        })?;

        Ok(Self {
            rpc_client,
            transaction_sender,
            client: Some(client),
            runtime,
        })
    }
}

impl Drop for TpuClientNextClient {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let _ = self.runtime.block_on(client.shutdown());
        }
    }
}

impl TpsClient for TpuClientNextClient {
    fn send_transaction(&self, transaction: Transaction) -> TpsClientResult<Signature> {
        let signature = transaction.signatures[0];
        let wire_transaction =
            serialize(&transaction).map_err(|err| TpsClientError::Custom(err.to_string()))?;
        self.transaction_sender
            .try_send_transactions_in_batch(vec![wire_transaction])
            .map_err(|err| TpsClientError::Custom(err.to_string()))?;
        Ok(signature)
    }

    fn send_batch(&self, transactions: Vec<Transaction>) -> TpsClientResult<()> {
        let wire_transactions = transactions
            .iter()
            .map(|transaction| {
                serialize(transaction).map_err(|err| TpsClientError::Custom(err.to_string()))
            })
            .collect::<TpsClientResult<Vec<_>>>()?;
        self.transaction_sender
            .try_send_transactions_in_batch(wire_transactions)
            .map_err(|err| TpsClientError::Custom(err.to_string()))?;
        Ok(())
    }

    fn get_latest_blockhash(&self) -> TpsClientResult<Hash> {
        self.rpc_client
            .get_latest_blockhash()
            .map_err(|err| err.into())
    }

    fn get_latest_blockhash_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> TpsClientResult<(Hash, u64)> {
        self.rpc_client
            .get_latest_blockhash_with_commitment(commitment_config)
            .map_err(|err| err.into())
    }

    fn get_new_latest_blockhash(&self, blockhash: &Hash) -> TpsClientResult<Hash> {
        self.rpc_client
            .get_new_latest_blockhash(blockhash)
            .map_err(|err| err.into())
    }

    fn get_signature_status(&self, signature: &Signature) -> TpsClientResult<Option<Result<()>>> {
        self.rpc_client
            .get_signature_status(signature)
            .map_err(|err| err.into())
    }

    fn get_transaction_count(&self) -> TpsClientResult<u64> {
        self.rpc_client
            .get_transaction_count()
            .map_err(|err| err.into())
    }

    fn get_transaction_count_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> TpsClientResult<u64> {
        self.rpc_client
            .get_transaction_count_with_commitment(commitment_config)
            .map_err(|err| err.into())
    }

    fn get_epoch_info(&self) -> TpsClientResult<EpochInfo> {
        self.rpc_client.get_epoch_info().map_err(|err| err.into())
    }

    fn get_balance(&self, pubkey: &Pubkey) -> TpsClientResult<u64> {
        self.rpc_client.get_balance(pubkey).map_err(|err| err.into())
    }

    fn get_balance_with_commitment(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> TpsClientResult<u64> {
        self.rpc_client
            .get_balance_with_commitment(pubkey, commitment_config)
            .map(|response| response.value)
            .map_err(|err| err.into())
    }

    fn get_fee_for_message(&self, message: &Message) -> TpsClientResult<u64> {
        self.rpc_client
            .get_fee_for_message(message)
            .map_err(|err| err.into())
    }

    fn get_minimum_balance_for_rent_exemption(&self, data_len: usize) -> TpsClientResult<u64> {
        self.rpc_client
            .get_minimum_balance_for_rent_exemption(data_len)
            .map_err(|err| err.into())
    }

    fn addr(&self) -> String {
        self.rpc_client.url()
    }

    fn request_airdrop_with_blockhash(
        &self,
        pubkey: &Pubkey,
        lamports: u64,
        recent_blockhash: &Hash,
    ) -> TpsClientResult<Signature> {
        self.rpc_client
            .request_airdrop_with_blockhash(pubkey, lamports, recent_blockhash)
            .map_err(|err| err.into())
    }

    fn get_account(&self, pubkey: &Pubkey) -> TpsClientResult<Account> {
        self.rpc_client.get_account(pubkey).map_err(|err| err.into())
    }

    fn get_account_with_commitment(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> TpsClientResult<Account> {
        self.rpc_client
            .get_account_with_commitment(pubkey, commitment_config)
            .map(|response| response.value)
            .map_err(|err| err.into())
            .and_then(|account| {
                account.ok_or_else(|| {
                    TpsClientError::Custom(format!("AccountNotFound: pubkey={pubkey}"))
                })
            })
    }

    fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> TpsClientResult<Vec<Option<Account>>> {
        self.rpc_client
            .get_multiple_accounts(pubkeys)
            .map_err(|err| err.into())
    }

    fn get_slot_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> TpsClientResult<u64> {
        self.rpc_client
            .get_slot_with_commitment(commitment_config)
            .map_err(|err| err.into())
    }

    fn get_blocks_with_commitment(
        &self,
        start_slot: u64,
        end_slot: Option<u64>,
        commitment_config: CommitmentConfig,
    ) -> TpsClientResult<Vec<u64>> {
        self.rpc_client
            .get_blocks_with_commitment(start_slot, end_slot, commitment_config)
            .map_err(|err| err.into())
    }

    fn get_block_with_config(
        &self,
        slot: u64,
        rpc_block_config: RpcBlockConfig,
    ) -> TpsClientResult<UiConfirmedBlock> {
        self.rpc_client
            .get_block_with_config(slot, rpc_block_config)
            .map_err(|err| err.into())
    }
}
