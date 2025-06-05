use {
    solana_hash::Hash,
    solana_message::{
        v0::LoadedAddresses, AccountKeys, AddressLoader, SanitizedMessage, SimpleAddressLoader,
        VersionedMessage,
    },
    solana_pubkey::Pubkey,
    solana_svm_transaction::{
        instruction::SVMInstruction, message_address_table_lookup::SVMMessageAddressTableLookup,
        svm_message::SVMMessage,
    },
    solana_transaction::{
        sanitized::SanitizedTransaction, versioned::sanitized::SanitizedVersionedTransaction,
    },
    solana_transaction_error::{TransactionError, TransactionResult},
    std::{collections::HashSet, ops::Deref},
};

#[cfg_attr(feature = "dev-context-only-utils", derive(Clone))]
#[derive(Debug)]
pub struct ResolvedTransaction {
    pub transaction: SanitizedTransaction,
    pub address_resolution_err: Option<TransactionError>,
}

impl Deref for ResolvedTransaction {
    type Target = SanitizedTransaction;
    fn deref(&self) -> &SanitizedTransaction {
        &self.transaction
    }
}

impl ResolvedTransaction {
    pub fn try_new(
        tx: SanitizedVersionedTransaction,
        message_hash: Hash,
        is_simple_vote_tx: bool,
        address_loader: impl AddressLoader,
        reserved_account_keys: &HashSet<Pubkey>,
    ) -> TransactionResult<Self> {
        let loaded_addresses_result = match &tx.get_message().message {
            VersionedMessage::Legacy(_) => Ok(LoadedAddresses::default()),
            VersionedMessage::V0(message) => {
                address_loader.load_addresses(&message.address_table_lookups)
            }
        };

        // hardcoded for now, but will be configurable via feature gate in the
        // future.
        let remove_address_resolution_constraint = false;
        let (address_loader, address_resolution_err) = match loaded_addresses_result {
            Ok(loaded_addresses) => (SimpleAddressLoader::Enabled(loaded_addresses), None),
            Err(err) => {
                let address_loader = if remove_address_resolution_constraint {
                    SimpleAddressLoader::Enabled(LoadedAddresses::default())
                } else {
                    SimpleAddressLoader::Disabled
                };
                (address_loader, Some(err.into()))
            }
        };

        Ok(Self {
            transaction: SanitizedTransaction::try_new(
                tx,
                message_hash,
                is_simple_vote_tx,
                address_loader,
                reserved_account_keys,
            )?,
            address_resolution_err,
        })
    }

    pub fn message(&self) -> &SanitizedMessage {
        self.transaction.message()
    }
}

impl SVMMessage for ResolvedTransaction {
    fn num_transaction_signatures(&self) -> u64 {
        self.transaction.num_transaction_signatures()
    }

    fn num_ed25519_signatures(&self) -> u64 {
        self.transaction.num_ed25519_signatures()
    }

    fn num_secp256k1_signatures(&self) -> u64 {
        self.transaction.num_secp256k1_signatures()
    }

    fn num_secp256r1_signatures(&self) -> u64 {
        self.transaction.num_secp256r1_signatures()
    }

    fn num_write_locks(&self) -> u64 {
        self.transaction.num_write_locks()
    }

    fn recent_blockhash(&self) -> &Hash {
        self.transaction.recent_blockhash()
    }

    fn num_instructions(&self) -> usize {
        self.transaction.num_instructions()
    }

    fn instructions_iter(&self) -> impl Iterator<Item = SVMInstruction> {
        self.transaction.instructions_iter()
    }

    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, SVMInstruction)> + Clone {
        self.transaction.program_instructions_iter()
    }

    fn static_account_keys(&self) -> &[Pubkey] {
        self.transaction.static_account_keys()
    }

    fn account_keys(&self) -> AccountKeys {
        self.transaction.account_keys()
    }

    fn fee_payer(&self) -> &Pubkey {
        self.transaction.fee_payer()
    }

    fn is_writable(&self, index: usize) -> bool {
        self.transaction.is_writable(index)
    }

    fn is_signer(&self, index: usize) -> bool {
        self.transaction.is_signer(index)
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        self.transaction.is_invoked(key_index)
    }

    fn num_lookup_tables(&self) -> usize {
        self.transaction.num_lookup_tables()
    }

    fn message_address_table_lookups(&self) -> impl Iterator<Item = SVMMessageAddressTableLookup> {
        self.transaction.message_address_table_lookups()
    }
}
