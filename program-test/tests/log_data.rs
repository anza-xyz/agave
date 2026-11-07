use {
    solana_account_info::AccountInfo,
    solana_instruction::Instruction,
    solana_program::log::sol_log_data,
    solana_program_error::ProgramResult,
    solana_program_test::{ProgramTest, processor},
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_transaction::Transaction,
};

fn process_log_data(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    sol_log_data(&[b"hello", b"world"]);
    Ok(())
}

#[tokio::test]
async fn test_native_sol_log_data_recorded_in_transaction_logs() {
    let program_id = Pubkey::new_unique();

    let program_test = ProgramTest::new("log_data", program_id, processor!(process_log_data));

    let context = program_test.start_with_context().await;

    let instruction = Instruction::new_with_bytes(program_id, &[], vec![]);

    let transaction = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );

    let result = context
        .banks_client
        .process_transaction_with_metadata(transaction)
        .await
        .unwrap();

    assert!(result.result.is_ok());

    let logs = result.metadata.unwrap().log_messages;

    assert!(
        logs.iter()
            .any(|log| log == "Program data: aGVsbG8= d29ybGQ="),
        "Expected sol_log_data output in transaction logs, got: {logs:?}"
    );
}
