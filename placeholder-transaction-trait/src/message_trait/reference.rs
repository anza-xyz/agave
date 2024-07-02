use {
    crate::{Instruction, MessageAddressTableLookup, MessageTrait},
    solana_sdk::{hash::Hash, message::AccountKeys, pubkey::Pubkey},
};

// References to `MessageTrait` are also `MessageTrait`.
impl<T: MessageTrait> MessageTrait for &T {
    fn num_signatures(&self) -> u64 {
        MessageTrait::num_signatures(*self)
    }

    fn num_write_locks(&self) -> u64 {
        MessageTrait::num_write_locks(*self)
    }

    fn recent_blockhash(&self) -> &Hash {
        MessageTrait::recent_blockhash(*self)
    }

    fn num_instructions(&self) -> usize {
        MessageTrait::num_instructions(*self)
    }

    fn instructions_iter(&self) -> impl Iterator<Item = Instruction> {
        MessageTrait::instructions_iter(*self)
    }

    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, Instruction)> {
        MessageTrait::program_instructions_iter(*self)
    }

    fn account_keys(&self) -> AccountKeys {
        MessageTrait::account_keys(*self)
    }

    fn fee_payer(&self) -> &Pubkey {
        MessageTrait::fee_payer(*self)
    }

    fn is_writable(&self, index: usize) -> bool {
        MessageTrait::is_writable(*self, index)
    }

    fn is_signer(&self, index: usize) -> bool {
        MessageTrait::is_signer(*self, index)
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        MessageTrait::is_invoked(*self, key_index)
    }

    fn has_duplicates(&self) -> bool {
        MessageTrait::has_duplicates(*self)
    }

    fn num_lookup_tables(&self) -> usize {
        MessageTrait::num_lookup_tables(*self)
    }

    fn message_address_table_lookups(&self) -> impl Iterator<Item = MessageAddressTableLookup> {
        MessageTrait::message_address_table_lookups(*self)
    }
}
