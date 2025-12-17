//! Transaction context (input).

use {
    agave_feature_set::FeatureSet,
    solana_account::Account,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_transaction::{sanitized::SanitizedTransaction, Transaction},
};

/// Transaction context fixture.
pub struct TxnContext {
    pub feature_set: FeatureSet,
    pub blockhash_queue: Vec<Hash>,
    pub slot: u64,
    pub accounts: Vec<(Pubkey, Account)>,
    pub transaction: SanitizedTransaction,
}

impl TxnContext {
    pub fn new(
        feature_set: FeatureSet,
        blockhash_queue: Vec<Hash>,
        slot: u64,
        accounts: Vec<(Pubkey, Account)>,
        transaction: Transaction,
    ) -> Self {
        let transaction = SanitizedTransaction::from_transaction_for_tests(transaction);
        Self {
            feature_set,
            blockhash_queue,
            slot,
            accounts,
            transaction,
        }
    }
}

#[cfg(feature = "fuzz")]
use {
    super::{
        error::FixtureError, message::build_sanitized_message, proto::TxnContext as ProtoTxnContext,
    },
    solana_signature::Signature,
};

#[cfg(feature = "fuzz")]
impl TryFrom<ProtoTxnContext> for TxnContext {
    type Error = FixtureError;

    fn try_from(value: ProtoTxnContext) -> Result<Self, Self::Error> {
        let feature_set: FeatureSet = value
            .epoch_ctx
            .as_ref()
            .and_then(|epoch_ctx| epoch_ctx.features.as_ref())
            .map(|fs| fs.into())
            .unwrap_or_default();

        let blockhash_queue: Vec<Hash> = if value.blockhash_queue.is_empty() {
            vec![Hash::default()]
        } else {
            value
                .blockhash_queue
                .into_iter()
                .map(|hash_bytes| {
                    let hash_array: [u8; 32] = hash_bytes
                        .try_into()
                        .map_err(FixtureError::InvalidHashBytes)?;
                    Ok(Hash::new_from_array(hash_array))
                })
                .collect::<Result<Vec<_>, FixtureError>>()?
        };

        let slot = value
            .slot_ctx
            .as_ref()
            .map(|slot_ctx| slot_ctx.slot)
            .unwrap_or_default();

        let accounts: Vec<(Pubkey, Account)> = value
            .account_shared_data
            .into_iter()
            .map(|acct_state| acct_state.try_into())
            .collect::<Result<Vec<_>, _>>()?;

        let proto_tx = value.tx.ok_or(FixtureError::InvalidFixtureInput)?;
        let proto_message = proto_tx.message.ok_or(FixtureError::InvalidFixtureInput)?;

        let message = build_sanitized_message(&proto_message, &accounts)?;
        let message_hash: Hash = if proto_tx.message_hash.is_empty() {
            Hash::default()
        } else {
            let hash_array: [u8; 32] = proto_tx
                .message_hash
                .try_into()
                .map_err(FixtureError::InvalidHashBytes)?;
            Hash::new_from_array(hash_array)
        };

        let mut signatures: Vec<Signature> = proto_tx
            .signatures
            .iter()
            .map(|sig_bytes| {
                let sig_array: [u8; 64] = sig_bytes
                    .clone()
                    .try_into()
                    .map_err(|_| FixtureError::InvalidFixtureInput)?;
                Ok(Signature::from(sig_array))
            })
            .collect::<Result<Vec<_>, FixtureError>>()?;

        if signatures.is_empty() {
            signatures.push(Signature::default());
        }

        let transaction = SanitizedTransaction::try_new_from_fields(
            message,
            message_hash,
            /* is_simple_vote_tx */ false,
            signatures,
        )
        .map_err(|_| FixtureError::InvalidFixtureInput)?;

        Ok(Self {
            feature_set,
            blockhash_queue,
            slot,
            accounts,
            transaction,
        })
    }
}
