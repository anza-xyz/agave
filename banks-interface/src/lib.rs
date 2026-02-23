#![cfg(feature = "agave-unstable-api")]
#![allow(deprecated)]

use {
    serde::{Deserialize, Serialize},
    solana_account::Account,
    solana_clock::Slot,
    solana_commitment_config::CommitmentLevel,
    solana_hash::Hash,
    solana_message::{Message, inner_instruction::InnerInstructions},
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_context::transaction::TransactionReturnData,
    solana_transaction_error::TransactionError,
};

mod transaction {
    pub use solana_transaction_error::TransactionResult as Result;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransactionConfirmationStatus {
    Processed,
    Confirmed,
    Finalized,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionStatus {
    pub slot: Slot,
    pub confirmations: Option<usize>, // None = rooted
    pub err: Option<TransactionError>,
    pub confirmation_status: Option<TransactionConfirmationStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionSimulationDetails {
    pub logs: Vec<String>,
    pub units_consumed: u64,
    pub loaded_accounts_data_size: u32,
    pub return_data: Option<TransactionReturnData>,
    pub inner_instructions: Option<Vec<InnerInstructions>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionMetadata {
    pub log_messages: Vec<String>,
    pub compute_units_consumed: u64,
    pub return_data: Option<TransactionReturnData>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BanksTransactionResultWithSimulation {
    pub result: Option<transaction::Result<()>>,
    pub simulation_details: Option<TransactionSimulationDetails>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BanksTransactionResultWithMetadata {
    pub result: transaction::Result<()>,
    pub metadata: Option<TransactionMetadata>,
}

#[tarpc::service]
pub trait Banks {
    async fn send_transaction_with_context(transaction: VersionedTransaction);
    async fn get_transaction_status_with_context(signature: Signature)
    -> Option<TransactionStatus>;
    async fn get_slot_with_context(commitment: CommitmentLevel) -> Slot;
    async fn get_block_height_with_context(commitment: CommitmentLevel) -> u64;
    async fn process_transaction_with_preflight_and_commitment_and_context(
        transaction: VersionedTransaction,
        commitment: CommitmentLevel,
    ) -> BanksTransactionResultWithSimulation;
    async fn process_transaction_with_commitment_and_context(
        transaction: VersionedTransaction,
        commitment: CommitmentLevel,
    ) -> Option<transaction::Result<()>>;
    async fn process_transaction_with_metadata_and_context(
        transaction: VersionedTransaction,
    ) -> BanksTransactionResultWithMetadata;
    async fn simulate_transaction_with_commitment_and_context(
        transaction: VersionedTransaction,
        commitment: CommitmentLevel,
    ) -> BanksTransactionResultWithSimulation;
    async fn get_account_with_commitment_and_context(
        address: Pubkey,
        commitment: CommitmentLevel,
    ) -> Option<Account>;
    async fn get_latest_blockhash_with_context() -> Hash;
    async fn get_latest_blockhash_with_commitment_and_context(
        commitment: CommitmentLevel,
    ) -> Option<(Hash, u64)>;
    async fn get_fee_for_message_with_commitment_and_context(
        message: Message,
        commitment: CommitmentLevel,
    ) -> Option<u64>;
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        tarpc::{client, transport},
    };

    #[test]
    fn test_banks_client_new() {
        let (client_transport, _server_transport) = transport::channel::unbounded();
        BanksClient::new(client::Config::default(), client_transport);
    }
}
