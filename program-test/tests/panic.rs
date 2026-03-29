use {
    solana_account_info::AccountInfo,
    solana_instruction::{Instruction, error::InstructionError},
    solana_program_error::ProgramResult,
    solana_program_test::{ProgramTest, processor},
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_transaction::Transaction,
    solana_transaction_error::TransactionError,
};

fn panic(_program_id: &Pubkey, _accounts: &[AccountInfo], _input: &[u8]) -> ProgramResult {
    panic!("I panicked");
}

#[tokio::test]
async fn panic_test() {
    let program_id = Pubkey::new_unique();

    let program_test = ProgramTest::new("panic", program_id, processor!(panic));

    let context = program_test.start_with_context().await;

    let instruction = Instruction::new_with_bytes(program_id, &[], vec![]);

    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );
    assert_eq!(
        context
            .banks_client
            .process_transaction(transaction)
            .await
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
    );
}
