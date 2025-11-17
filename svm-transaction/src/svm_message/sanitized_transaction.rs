use {
    crate::{
        instruction::SVMInstruction,
        message_address_table_lookup::SVMMessageAddressTableLookup,
        svm_message::{SVMMessage, SVMStaticMessage},
    },
    solana_hash::Hash,
    solana_message::AccountKeys,
    solana_pubkey::Pubkey,
    solana_transaction::sanitized::SanitizedTransaction,
};

impl SVMStaticMessage for SanitizedTransaction {
    fn num_transaction_signatures(&self) -> u64 {
        SVMStaticMessage::num_transaction_signatures(SanitizedTransaction::message(self))
    }

    fn num_write_locks(&self) -> u64 {
        SVMStaticMessage::num_write_locks(SanitizedTransaction::message(self))
    }

    fn recent_blockhash(&self) -> &Hash {
        SVMStaticMessage::recent_blockhash(SanitizedTransaction::message(self))
    }

    fn num_instructions(&self) -> usize {
        SVMStaticMessage::num_instructions(SanitizedTransaction::message(self))
    }

    fn instructions_iter(&self) -> impl Iterator<Item = SVMInstruction<'_>> {
        SVMStaticMessage::instructions_iter(SanitizedTransaction::message(self))
    }

    fn program_instructions_iter(
        &self,
    ) -> impl Iterator<Item = (&Pubkey, SVMInstruction<'_>)> + Clone {
        SVMStaticMessage::program_instructions_iter(SanitizedTransaction::message(self))
    }

    fn static_account_keys(&self) -> &[Pubkey] {
        SVMStaticMessage::static_account_keys(SanitizedTransaction::message(self))
    }

    fn fee_payer(&self) -> &Pubkey {
        SVMStaticMessage::fee_payer(SanitizedTransaction::message(self))
    }

    fn num_lookup_tables(&self) -> usize {
        SVMStaticMessage::num_lookup_tables(SanitizedTransaction::message(self))
    }

    fn message_address_table_lookups(
        &self,
    ) -> impl Iterator<Item = SVMMessageAddressTableLookup<'_>> {
        SVMStaticMessage::message_address_table_lookups(SanitizedTransaction::message(self))
    }
}

impl SVMMessage for SanitizedTransaction {
    fn account_keys(&self) -> AccountKeys<'_> {
        SVMMessage::account_keys(SanitizedTransaction::message(self))
    }

    fn is_writable(&self, index: usize) -> bool {
        SVMMessage::is_writable(SanitizedTransaction::message(self), index)
    }

    fn is_signer(&self, index: usize) -> bool {
        SVMMessage::is_signer(SanitizedTransaction::message(self), index)
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        SVMMessage::is_invoked(SanitizedTransaction::message(self), key_index)
    }
}
