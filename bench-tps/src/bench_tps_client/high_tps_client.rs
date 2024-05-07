//! Client optimized for bench-tps: high mean TPS and moderate std
//! This is achieved by:
//! * optimizing the size of send batch
//! * sending batches using connection cache in nonblocking fashion
//! * use leader cache from TpuClient to minimize rpc calls
//! * using several staked connection caches (not implemented yet)
//!
use {
    crate::bench_tps_client::{BenchTpsClient, BenchTpsError, Result},
    rand::prelude::*,
    solana_client::{
        connection_cache::Protocol, nonblocking::tpu_client::LeaderTpuService,
        tpu_connection::TpuConnection,
    },
    solana_connection_cache::connection_cache::ConnectionCache as BackendConnectionCache,
    solana_quic_client::{QuicConfig, QuicConnectionManager, QuicPool},
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::config::RpcBlockConfig,
    solana_sdk::{
        account::Account, commitment_config::CommitmentConfig, epoch_info::EpochInfo, hash::Hash,
        message::Message, pubkey::Pubkey, signature::Signature, slot_history::Slot,
        transaction::Transaction,
    },
    solana_transaction_status::UiConfirmedBlock,
    std::{
        net::SocketAddr,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    },
};

#[derive(Debug, Clone)]
pub struct HighTpsClientConfig {
    pub send_batch_size: usize,
    pub fanout_slots: u64,
    pub pinned_tpu_address: Option<SocketAddr>,
}

type ConnectionCache = BackendConnectionCache<QuicPool, QuicConnectionManager, QuicConfig>;
pub struct HighTpsClient {
    leader_tpu_service: LeaderTpuService,
    exit: Arc<AtomicBool>,
    rpc_client: Arc<RpcClient>,
    connection_caches: Vec<Arc<ConnectionCache>>,
    config: HighTpsClientConfig,
}

impl Drop for HighTpsClient {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
    }
}

impl HighTpsClient {
    pub fn new(
        rpc_client: Arc<RpcClient>,
        websocket_url: &str,
        config: HighTpsClientConfig,
        connection_caches: Vec<Arc<ConnectionCache>>,
    ) -> Result<Self> {
        let exit = Arc::new(AtomicBool::new(false));
        let create_leader_tpu_service = LeaderTpuService::new(
            rpc_client.get_inner_client().clone(),
            websocket_url,
            Protocol::QUIC,
            exit.clone(),
        );
        let leader_tpu_service = tokio::task::block_in_place(|| {
            rpc_client.runtime().block_on(create_leader_tpu_service)
        })?;
        if config.pinned_tpu_address.is_some() {
            // TODO(klykov): for the sake of this PR, just stop service if use pinned address
            exit.store(true, Ordering::Relaxed);
        }

        Ok(Self {
            leader_tpu_service,
            exit,
            rpc_client,
            connection_caches,
            config,
        })
    }
}

impl BenchTpsClient for HighTpsClient {
    fn send_transaction(&self, transaction: Transaction) -> Result<Signature> {
        self.rpc_client
            .send_transaction(&transaction)
            .map_err(|err| err.into())
    }

    fn send_batch(&self, transactions: Vec<Transaction>) -> Result<()> {
        let wire_transactions = transactions
            .into_iter() //.into_par_iter() any effect of this?
            .map(|tx| bincode::serialize(&tx).expect("transaction should be valid."))
            .collect::<Vec<_>>();
        let mut rng = rand::thread_rng();
        let cc_index: usize = rng.gen_range(0..self.connection_caches.len());
        // TODO(klykov): rewrite this more elegantly later
        if let Some(pinned_tpu_address) = self.config.pinned_tpu_address {
            for c in wire_transactions.chunks(self.config.send_batch_size) {
                let conn = self.connection_caches[cc_index].get_connection(&pinned_tpu_address);
                let _ = conn.send_data_batch_async(c.to_vec());
            }
        } else {
            for c in wire_transactions.chunks(self.config.send_batch_size) {
                let tpu_addresses = self
                    .leader_tpu_service
                    .leader_tpu_sockets(self.config.fanout_slots);
                for tpu_address in &tpu_addresses {
                    let conn = self.connection_caches[cc_index].get_connection(tpu_address);
                    let _ = conn.send_data_batch_async(c.to_vec());
                }
            }
        }
        Ok(())
    }
    fn get_latest_blockhash(&self) -> Result<Hash> {
        self.rpc_client
            .get_latest_blockhash()
            .map_err(|err| err.into())
    }

    fn get_latest_blockhash_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> Result<(Hash, u64)> {
        self.rpc_client
            .get_latest_blockhash_with_commitment(commitment_config)
            .map_err(|err| err.into())
    }

    fn get_transaction_count(&self) -> Result<u64> {
        self.rpc_client
            .get_transaction_count()
            .map_err(|err| err.into())
    }

    fn get_transaction_count_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> Result<u64> {
        self.rpc_client
            .get_transaction_count_with_commitment(commitment_config)
            .map_err(|err| err.into())
    }

    fn get_epoch_info(&self) -> Result<EpochInfo> {
        self.rpc_client.get_epoch_info().map_err(|err| err.into())
    }

    fn get_balance(&self, pubkey: &Pubkey) -> Result<u64> {
        self.rpc_client
            .get_balance(pubkey)
            .map_err(|err| err.into())
    }

    fn get_balance_with_commitment(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> Result<u64> {
        self.rpc_client
            .get_balance_with_commitment(pubkey, commitment_config)
            .map(|res| res.value)
            .map_err(|err| err.into())
    }

    fn get_fee_for_message(&self, message: &Message) -> Result<u64> {
        self.rpc_client
            .get_fee_for_message(message)
            .map_err(|err| err.into())
    }

    fn get_minimum_balance_for_rent_exemption(&self, data_len: usize) -> Result<u64> {
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
    ) -> Result<Signature> {
        self.rpc_client
            .request_airdrop_with_blockhash(pubkey, lamports, recent_blockhash)
            .map_err(|err| err.into())
    }

    fn get_account(&self, pubkey: &Pubkey) -> Result<Account> {
        self.rpc_client
            .get_account(pubkey)
            .map_err(|err| err.into())
    }

    fn get_account_with_commitment(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> Result<Account> {
        self.rpc_client
            .get_account_with_commitment(pubkey, commitment_config)
            .map(|res| res.value)
            .map_err(|err| err.into())
            .and_then(|account| {
                account.ok_or_else(|| {
                    BenchTpsError::Custom(format!("AccountNotFound: pubkey={pubkey}"))
                })
            })
    }

    fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        self.rpc_client
            .get_multiple_accounts(pubkeys)
            .map_err(|err| err.into())
    }

    fn get_slot_with_commitment(&self, commitment_config: CommitmentConfig) -> Result<Slot> {
        self.rpc_client
            .get_slot_with_commitment(commitment_config)
            .map_err(|err| err.into())
    }

    fn get_blocks_with_commitment(
        &self,
        start_slot: Slot,
        end_slot: Option<Slot>,
        commitment_config: CommitmentConfig,
    ) -> Result<Vec<Slot>> {
        self.rpc_client
            .get_blocks_with_commitment(start_slot, end_slot, commitment_config)
            .map_err(|err| err.into())
    }

    fn get_block_with_config(
        &self,
        slot: Slot,
        rpc_block_config: RpcBlockConfig,
    ) -> Result<UiConfirmedBlock> {
        self.rpc_client
            .get_block_with_config(slot, rpc_block_config)
            .map_err(|err| err.into())
    }
}
