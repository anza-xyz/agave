use {
    crate::{Instruction, MessageAddressTableLookup, MessageTrait},
    solana_sdk::{
        hash::Hash,
        message::{AccountKeys, SanitizedMessage},
        pubkey::Pubkey,
    },
};

// Implement for the "reference" `SanitizedMessage` type.
impl MessageTrait for SanitizedMessage {
    fn num_signatures(&self) -> u64 {
        SanitizedMessage::num_signatures(self)
    }

    fn num_write_locks(&self) -> u64 {
        SanitizedMessage::num_write_locks(self)
    }

    fn recent_blockhash(&self) -> &Hash {
        SanitizedMessage::recent_blockhash(self)
    }

    fn num_instructions(&self) -> usize {
        self.instructions().len()
    }

    fn instructions_iter(&self) -> impl Iterator<Item = Instruction> {
        self.instructions().iter().map(Instruction::from)
    }

    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, Instruction)> {
        SanitizedMessage::program_instructions_iter(self)
            .map(|(pubkey, ix)| (pubkey, Instruction::from(ix)))
    }

    fn account_keys(&self) -> AccountKeys {
        SanitizedMessage::account_keys(self)
    }

    fn fee_payer(&self) -> &Pubkey {
        SanitizedMessage::fee_payer(self)
    }

    fn is_writable(&self, index: usize) -> bool {
        SanitizedMessage::is_writable(self, index)
    }

    fn is_signer(&self, index: usize) -> bool {
        SanitizedMessage::is_signer(self, index)
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        SanitizedMessage::is_invoked(self, key_index)
    }

    fn has_duplicates(&self) -> bool {
        SanitizedMessage::has_duplicates(self)
    }

    fn num_lookup_tables(&self) -> usize {
        SanitizedMessage::message_address_table_lookups(self).len()
    }

    fn message_address_table_lookups(&self) -> impl Iterator<Item = MessageAddressTableLookup> {
        SanitizedMessage::message_address_table_lookups(self)
            .iter()
            .map(MessageAddressTableLookup::from)
    }
}
