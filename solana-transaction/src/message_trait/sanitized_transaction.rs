use {
    crate::{Instruction, MessageAddressTableLookup, SolanaMessage},
    solana_sdk::{
        hash::Hash, message::AccountKeys, pubkey::Pubkey, transaction::SanitizedTransaction,
    },
};

impl SolanaMessage for SanitizedTransaction {
    fn num_signatures(&self) -> u64 {
        SolanaMessage::num_signatures(self.message())
    }

    fn num_write_locks(&self) -> u64 {
        SolanaMessage::num_write_locks(self.message())
    }

    fn recent_blockhash(&self) -> &Hash {
        SolanaMessage::recent_blockhash(self.message())
    }

    fn num_instructions(&self) -> usize {
        SolanaMessage::num_instructions(self.message())
    }

    fn instructions_iter(&self) -> impl Iterator<Item = Instruction> {
        SolanaMessage::instructions_iter(self.message())
    }

    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, Instruction)> {
        SolanaMessage::program_instructions_iter(self.message())
    }

    fn account_keys(&self) -> AccountKeys {
        SolanaMessage::account_keys(self.message())
    }

    fn fee_payer(&self) -> &Pubkey {
        SolanaMessage::fee_payer(self.message())
    }

    fn is_writable(&self, index: usize) -> bool {
        SolanaMessage::is_writable(self.message(), index)
    }

    fn is_signer(&self, index: usize) -> bool {
        SolanaMessage::is_signer(self.message(), index)
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        SolanaMessage::is_invoked(self.message(), key_index)
    }

    fn has_duplicates(&self) -> bool {
        SolanaMessage::has_duplicates(self.message())
    }

    fn num_lookup_tables(&self) -> usize {
        SolanaMessage::num_lookup_tables(self.message())
    }

    fn message_address_table_lookups(&self) -> impl Iterator<Item = MessageAddressTableLookup> {
        SolanaMessage::message_address_table_lookups(self.message())
    }
}
