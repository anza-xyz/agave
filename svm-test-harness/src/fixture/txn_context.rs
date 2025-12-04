//! Transaction context (input).

use {
    super::{epoch_context::EpochContext, slot_context::SlotContext},
    solana_account::Account,
    solana_hash::Hash,
    solana_pubkey::Pubkey,
    solana_transaction::versioned::VersionedTransaction,
};

/// Transaction context fixture.
pub struct TxnContext {
    pub transaction: VersionedTransaction,
    pub accounts: Vec<(Pubkey, Account)>,
    pub blockhash_queue: Vec<Hash>,
    pub epoch_context: EpochContext,
    pub slot_context: SlotContext,
}

#[cfg(feature = "fuzz")]
use {
    super::{error::FixtureError, proto::TxnContext as ProtoTxnContext},
    crate::message::build_versioned_message,
    solana_signature::Signature,
};

#[cfg(feature = "fuzz")]
impl TryFrom<ProtoTxnContext> for TxnContext {
    type Error = FixtureError;

    fn try_from(value: ProtoTxnContext) -> Result<Self, Self::Error> {
        let proto_tx = value.tx.ok_or(FixtureError::InvalidFixtureInput)?;
        let proto_message = proto_tx.message.ok_or(FixtureError::InvalidFixtureInput)?;

        let message = build_versioned_message(&proto_message);

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

        let transaction = VersionedTransaction {
            message,
            signatures,
        };

        let accounts: Vec<(Pubkey, Account)> = value
            .account_shared_data
            .into_iter()
            .map(|acct_state| acct_state.try_into())
            .collect::<Result<Vec<_>, _>>()?;

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

        let epoch_context = value
            .epoch_ctx
            .as_ref()
            .map(EpochContext::from)
            .unwrap_or_default();

        let slot_context = value
            .slot_ctx
            .as_ref()
            .map(SlotContext::from)
            .unwrap_or_default();

        Ok(Self {
            transaction,
            accounts,
            blockhash_queue,
            epoch_context,
            slot_context,
        })
    }
}
