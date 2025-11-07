use {
    agave_transaction_view::{
        transaction_data::TransactionData, transaction_view::TransactionView,
    },
    solana_pubkey::Pubkey,
    solana_svm_transaction::instruction::SVMInstruction,
};

pub trait StaticMessage {
    fn static_num_write_locks(&self) -> u64;
    fn static_program_instructions_iter(
        &self,
    ) -> impl Iterator<Item = (&Pubkey, SVMInstruction<'_>)> + Clone;
}

/*
impl<T> StaticMessage for T
where
    T: SVMMessage,
{
    fn static_num_write_locks(&self) -> u64 {
        self.num_write_locks()
    }

    fn static_program_instructions_iter(
        &self,
    ) -> impl Iterator<Item = (&Pubkey, SVMInstruction<'_>)> + Clone {
        self.program_instructions_iter()
    }
}
*/

impl<T> StaticMessage for TransactionView<true, T>
where
    T: TransactionData,
{
    fn static_num_write_locks(&self) -> u64 {
        self.num_requested_write_locks()
    }

    fn static_program_instructions_iter(
        &self,
    ) -> impl Iterator<Item = (&Pubkey, SVMInstruction<'_>)> + Clone {
        self.program_instructions_iter()
    }
}
