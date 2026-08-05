//! Transaction context (input).

use {
    crate::conformance::nonce_fields::NonceFields, agave_feature_set::FeatureSet,
    solana_account::Account, solana_message::SanitizedMessage,
    solana_program_runtime::execution_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
    solana_pubkey::Pubkey,
};
#[cfg(feature = "conformance")]
use {
    crate::conformance::{
        account_state::account_from_proto, feature_set::feature_set_from_proto,
        setup::sanitized_message_from_versioned_message,
        versioned_message::versioned_message_from_proto,
    },
    protosol::protos::TxnContext as ProtoTxnContext,
    solana_hash::Hash,
};

pub struct TxnContext {
    pub feature_set: FeatureSet,
    pub accounts: Vec<(Pubkey, Account)>,
    pub message: SanitizedMessage,
    pub nonce_fields: Option<NonceFields>,
    pub cu_avail: u64,
}

impl TxnContext {
    /// Create a new [`TxnContext`] with the default compute unit budget
    /// (200,000 CUs).
    pub fn new_with_default_budget(
        feature_set: FeatureSet,
        accounts: Vec<(Pubkey, Account)>,
        message: SanitizedMessage,
        nonce_fields: Option<NonceFields>,
    ) -> Self {
        Self {
            feature_set,
            accounts,
            message,
            nonce_fields,
            cu_avail: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
        }
    }
}

#[cfg(feature = "conformance")]
impl From<ProtoTxnContext> for TxnContext {
    fn from(value: ProtoTxnContext) -> Self {
        let bank = value.bank.as_ref();
        let accounts: Vec<_> = value
            .account_shared_data
            .into_iter()
            .map(account_from_proto)
            .collect();

        let tx = value.tx.expect("missing transaction");
        let proto_message = tx.message.expect("missing transaction message");
        let versioned_message = versioned_message_from_proto(&proto_message);
        let message =
            sanitized_message_from_versioned_message(versioned_message.clone(), &accounts);

        let feature_set = bank
            .and_then(|bank| bank.features.as_ref())
            .map(feature_set_from_proto)
            .unwrap_or_default();

        let nonce_fields = bank.map(|bank| {
            let blockhash = bank
                .blockhash_queue
                .iter()
                .map(|entry| {
                    <[u8; 32]>::try_from(entry.blockhash.as_slice())
                        .map(Hash::new_from_array)
                        .expect("invalid blockhash queue entry bytes")
                })
                .next_back()
                .unwrap_or(*versioned_message.recent_blockhash());
            let blockhash_lamports_per_signature = u64::from(bank.rbh_lamports_per_signature);

            NonceFields {
                blockhash,
                blockhash_lamports_per_signature,
            }
        });

        TxnContext::new_with_default_budget(feature_set, accounts, message, nonce_fields)
    }
}
