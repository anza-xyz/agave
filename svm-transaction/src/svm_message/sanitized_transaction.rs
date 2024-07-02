use {
    crate::{Instruction, MessageAddressTableLookup, SVMMessage},
    solana_sdk::{
        hash::Hash, message::AccountKeys, pubkey::Pubkey, transaction::SanitizedTransaction,
    },
};

impl SVMMessage for SanitizedTransaction {
    fn num_signatures(&self) -> u64 {
        SVMMessage::num_signatures(self.message())
    }

    fn num_write_locks(&self) -> u64 {
        SVMMessage::num_write_locks(self.message())
    }

    fn recent_blockhash(&self) -> &Hash {
        SVMMessage::recent_blockhash(self.message())
    }

    fn num_instructions(&self) -> usize {
        SVMMessage::num_instructions(self.message())
    }

    fn instructions_iter(&self) -> impl Iterator<Item = Instruction> {
        SVMMessage::instructions_iter(self.message())
    }

    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, Instruction)> {
        SVMMessage::program_instructions_iter(self.message())
    }

    fn account_keys(&self) -> AccountKeys {
        SVMMessage::account_keys(self.message())
    }

    fn fee_payer(&self) -> &Pubkey {
        SVMMessage::fee_payer(self.message())
    }

    fn is_writable(&self, index: usize) -> bool {
        SVMMessage::is_writable(self.message(), index)
    }

    fn is_signer(&self, index: usize) -> bool {
        SVMMessage::is_signer(self.message(), index)
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        SVMMessage::is_invoked(self.message(), key_index)
    }

    fn num_lookup_tables(&self) -> usize {
        SVMMessage::num_lookup_tables(self.message())
    }

    fn message_address_table_lookups(&self) -> impl Iterator<Item = MessageAddressTableLookup> {
        SVMMessage::message_address_table_lookups(self.message())
    }
}
