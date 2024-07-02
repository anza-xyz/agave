use {
    crate::{Instruction, MessageAddressTableLookup, SVMMessage},
    solana_sdk::{hash::Hash, message::AccountKeys, pubkey::Pubkey},
};

// References to `MessageTrait` are also `MessageTrait`.
impl<T: SVMMessage> SVMMessage for &T {
    fn num_signatures(&self) -> u64 {
        SVMMessage::num_signatures(*self)
    }

    fn num_write_locks(&self) -> u64 {
        SVMMessage::num_write_locks(*self)
    }

    fn recent_blockhash(&self) -> &Hash {
        SVMMessage::recent_blockhash(*self)
    }

    fn num_instructions(&self) -> usize {
        SVMMessage::num_instructions(*self)
    }

    fn instructions_iter(&self) -> impl Iterator<Item = Instruction> {
        SVMMessage::instructions_iter(*self)
    }

    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, Instruction)> {
        SVMMessage::program_instructions_iter(*self)
    }

    fn account_keys(&self) -> AccountKeys {
        SVMMessage::account_keys(*self)
    }

    fn fee_payer(&self) -> &Pubkey {
        SVMMessage::fee_payer(*self)
    }

    fn is_writable(&self, index: usize) -> bool {
        SVMMessage::is_writable(*self, index)
    }

    fn is_signer(&self, index: usize) -> bool {
        SVMMessage::is_signer(*self, index)
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        SVMMessage::is_invoked(*self, key_index)
    }

    fn num_lookup_tables(&self) -> usize {
        SVMMessage::num_lookup_tables(*self)
    }

    fn message_address_table_lookups(&self) -> impl Iterator<Item = MessageAddressTableLookup> {
        SVMMessage::message_address_table_lookups(*self)
    }
}
