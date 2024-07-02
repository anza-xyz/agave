use {
    crate::{Instruction, MessageAddressTableLookup, SolanaMessage},
    solana_sdk::{hash::Hash, message::AccountKeys, pubkey::Pubkey},
};

// References to `MessageTrait` are also `MessageTrait`.
impl<T: SolanaMessage> SolanaMessage for &T {
    fn num_signatures(&self) -> u64 {
        SolanaMessage::num_signatures(*self)
    }

    fn num_write_locks(&self) -> u64 {
        SolanaMessage::num_write_locks(*self)
    }

    fn recent_blockhash(&self) -> &Hash {
        SolanaMessage::recent_blockhash(*self)
    }

    fn num_instructions(&self) -> usize {
        SolanaMessage::num_instructions(*self)
    }

    fn instructions_iter(&self) -> impl Iterator<Item = Instruction> {
        SolanaMessage::instructions_iter(*self)
    }

    fn program_instructions_iter(&self) -> impl Iterator<Item = (&Pubkey, Instruction)> {
        SolanaMessage::program_instructions_iter(*self)
    }

    fn account_keys(&self) -> AccountKeys {
        SolanaMessage::account_keys(*self)
    }

    fn fee_payer(&self) -> &Pubkey {
        SolanaMessage::fee_payer(*self)
    }

    fn is_writable(&self, index: usize) -> bool {
        SolanaMessage::is_writable(*self, index)
    }

    fn is_signer(&self, index: usize) -> bool {
        SolanaMessage::is_signer(*self, index)
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        SolanaMessage::is_invoked(*self, key_index)
    }

    fn num_lookup_tables(&self) -> usize {
        SolanaMessage::num_lookup_tables(*self)
    }

    fn message_address_table_lookups(&self) -> impl Iterator<Item = MessageAddressTableLookup> {
        SolanaMessage::message_address_table_lookups(*self)
    }
}
