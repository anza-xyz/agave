use {
    crate::{Instruction, MessageAddressTableLookup, MessageTrait},
    solana_sdk::{
        hash::Hash, message::AccountKeys, pubkey::Pubkey, transaction::SanitizedTransaction,
    },
};

impl MessageTrait for SanitizedTransaction {
    fn num_signatures(&self) -> u64 {
        MessageTrait::num_signatures(self.message())
    }

    fn num_write_locks(&self) -> u64 {
        MessageTrait::num_write_locks(self.message())
    }

    fn recent_blockhash(&self) -> &Hash {
        MessageTrait::recent_blockhash(self.message())
    }

    fn num_instructions(&self) -> usize {
        MessageTrait::num_instructions(self.message())
    }

    fn instructions_iter(&self) -> impl Iterator<Item = Instruction> {
        MessageTrait::instructions_iter(self.message())
    }

    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, Instruction)> {
        MessageTrait::program_instructions_iter(self.message())
    }

    fn account_keys(&self) -> AccountKeys {
        MessageTrait::account_keys(self.message())
    }

    fn fee_payer(&self) -> &Pubkey {
        MessageTrait::fee_payer(self.message())
    }

    fn is_writable(&self, index: usize) -> bool {
        MessageTrait::is_writable(self.message(), index)
    }

    fn is_signer(&self, index: usize) -> bool {
        MessageTrait::is_signer(self.message(), index)
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        MessageTrait::is_invoked(self.message(), key_index)
    }

    fn has_duplicates(&self) -> bool {
        MessageTrait::has_duplicates(self.message())
    }

    fn num_lookup_tables(&self) -> usize {
        MessageTrait::num_lookup_tables(self.message())
    }

    fn message_address_table_lookups(&self) -> impl Iterator<Item = MessageAddressTableLookup> {
        MessageTrait::message_address_table_lookups(self.message())
    }
}
