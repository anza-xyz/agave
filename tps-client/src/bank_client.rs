use {
    crate::{TpsClient, TpsClientError, TpsClientResult},
    solana_account::Account,
    solana_client_traits::{AsyncClient, SyncClient},
    solana_commitment_config::CommitmentConfig,
    solana_epoch_info::EpochInfo,
    solana_hash::Hash,
    solana_message::Message,
    solana_pubkey::Pubkey,
    solana_rpc_client_api::config::RpcBlockConfig,
    solana_runtime::bank_client::BankClient,
    solana_signature::Signature,
    solana_transaction::Transaction,
    solana_transaction_error::TransactionResult as Result,
    solana_transaction_status::UiConfirmedBlock,
};

impl TpsClient for BankClient {
    fn send_transaction(&self, transaction: Transaction) -> TpsClientResult<Signature> {
        AsyncClient::async_send_transaction(self, transaction).map_err(|err| err.into())
    }
    fn send_batch(&self, transactions: Vec<Transaction>) -> TpsClientResult<()> {
        AsyncClient::async_send_batch(self, transactions).map_err(|err| err.into())
    }
    fn get_latest_blockhash(&self) -> TpsClientResult<Hash> {
        SyncClient::get_latest_blockhash(self).map_err(|err| err.into())
    }

    fn get_latest_blockhash_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> TpsClientResult<(Hash, u64)> {
        SyncClient::get_latest_blockhash_with_commitment(self, commitment_config)
            .map_err(|err| err.into())
    }

    fn get_transaction_count(&self) -> TpsClientResult<u64> {
        SyncClient::get_transaction_count(self).map_err(|err| err.into())
    }

    fn get_signature_status(&self, signature: &Signature) -> TpsClientResult<Option<Result<()>>> {
        SyncClient::get_signature_status(self, signature).map_err(|err| err.into())
    }

    fn get_transaction_count_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> TpsClientResult<u64> {
        SyncClient::get_transaction_count_with_commitment(self, commitment_config)
            .map_err(|err| err.into())
    }

    fn get_epoch_info(&self) -> TpsClientResult<EpochInfo> {
        SyncClient::get_epoch_info(self).map_err(|err| err.into())
    }

    fn get_balance(&self, pubkey: &Pubkey) -> TpsClientResult<u64> {
        SyncClient::get_balance(self, pubkey).map_err(|err| err.into())
    }

    fn get_balance_with_commitment(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> TpsClientResult<u64> {
        SyncClient::get_balance_with_commitment(self, pubkey, commitment_config)
            .map_err(|err| err.into())
    }

    fn get_fee_for_message(&self, message: &Message) -> TpsClientResult<u64> {
        SyncClient::get_fee_for_message(self, message).map_err(|err| err.into())
    }

    fn get_minimum_balance_for_rent_exemption(&self, data_len: usize) -> TpsClientResult<u64> {
        SyncClient::get_minimum_balance_for_rent_exemption(self, data_len).map_err(|err| err.into())
    }

    fn addr(&self) -> String {
        "Local BankClient".to_string()
    }

    fn request_airdrop_with_blockhash(
        &self,
        _pubkey: &Pubkey,
        _lamports: u64,
        _recent_blockhash: &Hash,
    ) -> TpsClientResult<Signature> {
        // BankClient doesn't support airdrops
        Err(TpsClientError::AirdropFailure)
    }

    fn get_account(&self, pubkey: &Pubkey) -> TpsClientResult<Account> {
        SyncClient::get_account(self, pubkey)
            .map_err(|err| err.into())
            .and_then(|account| {
                account.ok_or_else(|| {
                    TpsClientError::Custom(format!("AccountNotFound: pubkey={pubkey}"))
                })
            })
    }

    fn get_account_with_commitment(
        &self,
        pubkey: &Pubkey,
        commitment_config: CommitmentConfig,
    ) -> TpsClientResult<Account> {
        SyncClient::get_account_with_commitment(self, pubkey, commitment_config)
            .map_err(|err| err.into())
            .and_then(|account| {
                account.ok_or_else(|| {
                    TpsClientError::Custom(format!("AccountNotFound: pubkey={pubkey}"))
                })
            })
    }

    fn get_multiple_accounts(&self, _pubkeys: &[Pubkey]) -> TpsClientResult<Vec<Option<Account>>> {
        unimplemented!("BankClient doesn't support get_multiple_accounts");
    }

    fn get_slot_with_commitment(
        &self,
        commitment_config: CommitmentConfig,
    ) -> TpsClientResult<u64> {
        SyncClient::get_slot_with_commitment(self, commitment_config).map_err(|err| err.into())
    }

    fn get_blocks_with_commitment(
        &self,
        _start_slot: u64,
        _end_slot: Option<u64>,
        _commitment_config: CommitmentConfig,
    ) -> TpsClientResult<Vec<u64>> {
        unimplemented!("BankClient doesn't support get_blocks");
    }

    fn get_block_with_config(
        &self,
        _slot: u64,
        _rpc_block_config: RpcBlockConfig,
    ) -> TpsClientResult<UiConfirmedBlock> {
        unimplemented!("BankClient doesn't support get_block_with_config");
    }
}
