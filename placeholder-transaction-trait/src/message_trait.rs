use {
    crate::{Instruction, MessageAddressTableLookup},
    core::fmt::Debug,
    solana_sdk::{
        feature_set::FeatureSet,
        hash::Hash,
        message::{AccountKeys, TransactionSignatureDetails},
        nonce::NONCED_TX_MARKER_IX_INDEX,
        precompiles::{get_precompiles, is_precompile},
        pubkey::Pubkey,
        sysvar::instructions::{BorrowedAccountMeta, BorrowedInstruction},
        transaction::TransactionError,
    },
};

mod sanitized_message;

// - Clone to support possible cloning from holding in Vec
// - Debug to support legacy logging
pub trait MessageTrait: Clone + Debug {
    /// Return the number of signatures in the message.
    fn num_signatures(&self) -> u64;

    /// Returns the number of requested write-locks in this message.
    /// This does not consider if write-locks are demoted.
    fn num_write_locks(&self) -> u64;

    /// Return the recent blockhash.
    fn recent_blockhash(&self) -> &Hash;

    /// Return the number of instructions in the message.
    fn num_instructions(&self) -> usize;

    /// Return an iterator over the instructions in the message.
    fn instructions_iter(&self) -> impl Iterator<Item = Instruction>;

    /// Return an iterator over the instructions in the message, paired with
    /// the pubkey of the program.
    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, Instruction)>;

    /// Return the account keys.
    fn account_keys(&self) -> AccountKeys;

    /// Return the fee-payer
    fn fee_payer(&self) -> &Pubkey;

    /// Returns `true` if the account at `index` is writable.
    fn is_writable(&self, index: usize) -> bool;

    /// Returns `true` if the account at `index` is signer.
    fn is_signer(&self, index: usize) -> bool;

    /// Returns true if the account at the specified index is invoked as a
    /// program in top-level instructions of this message.
    fn is_invoked(&self, key_index: usize) -> bool;

    /// Returns true if the account at the specified index is an input to some
    /// program instruction in this message.
    fn is_instruction_account(&self, key_index: usize) -> bool {
        if let Ok(key_index) = u8::try_from(key_index) {
            self.instructions_iter()
                .any(|ix| ix.accounts.contains(&key_index))
        } else {
            false
        }
    }

    /// Return signature details.
    fn get_signature_details(&self) -> TransactionSignatureDetails {
        let mut transaction_signature_details = TransactionSignatureDetails {
            num_transaction_signatures: self.num_signatures(),
            ..TransactionSignatureDetails::default()
        };

        // counting the number of pre-processor operations separately
        for (program_id, instruction) in self.program_instructions_iter() {
            if solana_sdk::secp256k1_program::check_id(program_id) {
                if let Some(num_verifies) = instruction.data.first() {
                    transaction_signature_details.num_secp256k1_instruction_signatures =
                        transaction_signature_details
                            .num_secp256k1_instruction_signatures
                            .saturating_add(u64::from(*num_verifies));
                }
            } else if solana_sdk::ed25519_program::check_id(program_id) {
                if let Some(num_verifies) = instruction.data.first() {
                    transaction_signature_details.num_ed25519_instruction_signatures =
                        transaction_signature_details
                            .num_ed25519_instruction_signatures
                            .saturating_add(u64::from(*num_verifies));
                }
            }
        }

        transaction_signature_details
    }

    /// Return the durable nonce for the message if it exists
    fn get_durable_nonce(&self) -> Option<&Pubkey> {
        self.instructions_iter()
            .nth(NONCED_TX_MARKER_IX_INDEX as usize)
            .filter(
                |ix| match self.account_keys().get(ix.program_id_index as usize) {
                    Some(program_id) => solana_sdk::system_program::check_id(program_id),
                    _ => false,
                },
            )
            .filter(|ix| {
                matches!(
                    solana_program::program_utils::limited_deserialize(
                        ix.data, 4 /* serialized size of AdvanceNonceAccount */
                    ),
                    Ok(solana_sdk::system_instruction::SystemInstruction::AdvanceNonceAccount)
                )
            })
            .and_then(|ix| {
                ix.accounts.first().and_then(|idx| {
                    let idx = *idx as usize;
                    if !self.is_writable(idx) {
                        None
                    } else {
                        self.account_keys().get(idx)
                    }
                })
            })
    }

    /// Return the signers for the instruction at the given index.
    fn get_ix_signers(&self, index: usize) -> impl Iterator<Item = &Pubkey> {
        self.instructions_iter()
            .nth(index)
            .into_iter()
            .flat_map(|ix| {
                ix.accounts
                    .iter()
                    .copied()
                    .map(usize::from)
                    .filter(|index| self.is_signer(*index))
                    .filter_map(|signer_index| self.account_keys().get(signer_index))
            })
    }

    /// Checks for duplicate accounts in the message
    fn has_duplicates(&self) -> bool;

    /// Decompile message instructions without cloning account keys
    /// TODO: Remove this - there's an allocation!
    fn decompile_instructions(&self) -> Vec<BorrowedInstruction> {
        let account_keys = self.account_keys();
        self.program_instructions_iter()
            .map(|(program_id, instruction)| {
                let accounts = instruction
                    .accounts
                    .iter()
                    .map(|account_index| {
                        let account_index = *account_index as usize;
                        BorrowedAccountMeta {
                            is_signer: self.is_signer(account_index),
                            is_writable: self.is_writable(account_index),
                            pubkey: account_keys.get(account_index).unwrap(),
                        }
                    })
                    .collect();

                BorrowedInstruction {
                    accounts,
                    data: instruction.data,
                    program_id,
                }
            })
            .collect()
    }

    /// Validate a transaction message against locked accounts
    fn validate_account_locks(&self, tx_account_lock_limit: usize) -> Result<(), TransactionError> {
        if self.has_duplicates() {
            Err(TransactionError::AccountLoadedTwice)
        } else if self.account_keys().len() > tx_account_lock_limit {
            Err(TransactionError::TooManyAccountLocks)
        } else {
            Ok(())
        }
    }

    /// Get the number of lookup tables.
    fn num_lookup_tables(&self) -> usize;

    /// Get message address table lookups used in the message
    fn message_address_table_lookups(&self) -> impl Iterator<Item = MessageAddressTableLookup>;

    /// Verify precompiles in the message
    fn verify_precompiles(&self, feature_set: &FeatureSet) -> Result<(), TransactionError> {
        let is_enabled = |feature_id: &Pubkey| feature_set.is_active(feature_id);
        let has_precompiles = self
            .program_instructions_iter()
            .any(|(program_id, _)| is_precompile(program_id, is_enabled));

        if has_precompiles {
            let instructions_data: Vec<_> = self
                .instructions_iter()
                .map(|instruction| instruction.data)
                .collect();
            for (program_id, instruction) in self.program_instructions_iter() {
                if let Some(precompile) = get_precompiles()
                    .iter()
                    .find(|precompile| precompile.check_id(program_id, is_enabled))
                {
                    precompile
                        .verify(instruction.data, &instructions_data, feature_set)
                        .map_err(|_| TransactionError::InvalidAccountIndex)?;
                }
            }
        }
        Ok(())
    }
}
