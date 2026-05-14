use {
    crate::{
        instruction::SVMInstruction,
        message_address_table_lookup::SVMMessageAddressTableLookup,
        svm_message::{SVMMessage, SVMStaticMessage},
        svm_transaction::SVMTransaction,
    },
    agave_transaction_view::{
        resolved_transaction_view::ResolvedTransactionView, transaction_data::TransactionData,
        transaction_view::TransactionView,
    },
    solana_hash::Hash,
    solana_message::AccountKeys,
    solana_pubkey::Pubkey,
    solana_signature::Signature,
    solana_transaction::versioned::TransactionVersion,
};

impl<D: TransactionData> SVMStaticMessage for TransactionView<true, D> {
    fn version(&self) -> TransactionVersion {
        self.version().into()
    }

    fn num_transaction_signatures(&self) -> u64 {
        u64::from(self.num_required_signatures())
    }

    fn num_write_locks(&self) -> u64 {
        self.num_requested_write_locks()
    }

    fn recent_blockhash(&self) -> &Hash {
        self.recent_blockhash()
    }

    fn num_instructions(&self) -> usize {
        usize::from(self.num_instructions())
    }

    fn instructions_iter(&self) -> impl Iterator<Item = SVMInstruction<'_>> {
        TransactionView::instructions_iter(self).map(SVMInstruction::from)
    }

    fn program_instructions_iter(
        &self,
    ) -> impl Iterator<Item = (&Pubkey, SVMInstruction<'_>)> + Clone {
        TransactionView::program_instructions_iter(self)
            .map(|(program_id, ix)| (program_id, SVMInstruction::from(ix)))
    }

    fn static_account_keys(&self) -> &[Pubkey] {
        self.static_account_keys()
    }

    fn fee_payer(&self) -> &Pubkey {
        &self.static_account_keys()[0]
    }

    fn num_lookup_tables(&self) -> usize {
        usize::from(self.num_address_table_lookups())
    }

    fn message_address_table_lookups(
        &self,
    ) -> impl Iterator<Item = SVMMessageAddressTableLookup<'_>> {
        self.address_table_lookup_iter()
            .map(SVMMessageAddressTableLookup::from)
    }
}

impl<D: TransactionData> SVMStaticMessage for &TransactionView<true, D> {
    fn version(&self) -> TransactionVersion {
        TransactionView::version(self).into()
    }

    fn num_transaction_signatures(&self) -> u64 {
        u64::from(TransactionView::num_required_signatures(self))
    }

    fn num_write_locks(&self) -> u64 {
        TransactionView::num_requested_write_locks(self)
    }

    fn recent_blockhash(&self) -> &Hash {
        TransactionView::recent_blockhash(self)
    }

    fn num_instructions(&self) -> usize {
        usize::from(TransactionView::num_instructions(self))
    }

    fn instructions_iter(&self) -> impl Iterator<Item = SVMInstruction<'_>> {
        TransactionView::instructions_iter(self).map(SVMInstruction::from)
    }

    fn program_instructions_iter(
        &self,
    ) -> impl Iterator<Item = (&Pubkey, SVMInstruction<'_>)> + Clone {
        TransactionView::program_instructions_iter(self)
            .map(|(program_id, ix)| (program_id, SVMInstruction::from(ix)))
    }

    fn static_account_keys(&self) -> &[Pubkey] {
        TransactionView::static_account_keys(self)
    }

    fn fee_payer(&self) -> &Pubkey {
        &TransactionView::static_account_keys(self)[0]
    }

    fn num_lookup_tables(&self) -> usize {
        usize::from(TransactionView::num_address_table_lookups(self))
    }

    fn message_address_table_lookups(
        &self,
    ) -> impl Iterator<Item = SVMMessageAddressTableLookup<'_>> {
        TransactionView::address_table_lookup_iter(self).map(SVMMessageAddressTableLookup::from)
    }
}

impl<D: TransactionData> SVMStaticMessage for ResolvedTransactionView<D> {
    fn version(&self) -> TransactionVersion {
        TransactionView::version(self).into()
    }

    fn num_transaction_signatures(&self) -> u64 {
        u64::from(TransactionView::num_required_signatures(self))
    }

    fn num_write_locks(&self) -> u64 {
        TransactionView::num_requested_write_locks(self)
    }

    fn recent_blockhash(&self) -> &Hash {
        TransactionView::recent_blockhash(self)
    }

    fn num_instructions(&self) -> usize {
        usize::from(TransactionView::num_instructions(self))
    }

    fn instructions_iter(&self) -> impl Iterator<Item = SVMInstruction<'_>> {
        TransactionView::instructions_iter(self).map(SVMInstruction::from)
    }

    fn program_instructions_iter(
        &self,
    ) -> impl Iterator<Item = (&Pubkey, SVMInstruction<'_>)> + Clone {
        TransactionView::program_instructions_iter(self)
            .map(|(program_id, ix)| (program_id, SVMInstruction::from(ix)))
    }

    fn static_account_keys(&self) -> &[Pubkey] {
        TransactionView::static_account_keys(self)
    }

    fn fee_payer(&self) -> &Pubkey {
        &TransactionView::static_account_keys(self)[0]
    }

    fn num_lookup_tables(&self) -> usize {
        usize::from(TransactionView::num_address_table_lookups(self))
    }

    fn message_address_table_lookups(
        &self,
    ) -> impl Iterator<Item = SVMMessageAddressTableLookup<'_>> {
        TransactionView::address_table_lookup_iter(self).map(SVMMessageAddressTableLookup::from)
    }
}

impl<D: TransactionData> SVMMessage for ResolvedTransactionView<D> {
    fn account_keys(&self) -> AccountKeys<'_> {
        AccountKeys::new(
            TransactionView::static_account_keys(self),
            self.loaded_addresses(),
        )
    }

    fn is_writable(&self, index: usize) -> bool {
        ResolvedTransactionView::is_writable(self, index)
    }

    fn is_signer(&self, index: usize) -> bool {
        index < usize::from(TransactionView::num_required_signatures(self))
    }

    fn is_invoked(&self, key_index: usize) -> bool {
        let Ok(index) = u8::try_from(key_index) else {
            return false;
        };
        TransactionView::instructions_iter(self).any(|ix| ix.program_id_index == index)
    }
}

impl<D: TransactionData> SVMTransaction for ResolvedTransactionView<D> {
    fn signature(&self) -> &Signature {
        &TransactionView::signatures(self)[0]
    }

    fn signatures(&self) -> &[Signature] {
        TransactionView::signatures(self)
    }
}
