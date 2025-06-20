use {
    solana_account::ReadableAccount,
    solana_account_info::AccountInfo,
    solana_instruction::Instruction,
    solana_keypair::Keypair,
    solana_msg::msg,
    solana_program_error::ProgramResult,
    solana_program_test::{processor, ProgramTest},
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_runtime::bank::Bank,
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_transaction::Transaction,
    std::sync::Arc,
};

fn process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _input: &[u8],
) -> ProgramResult {
    msg!("Processing instruction");
    Ok(())
}

#[test]
fn simulate_fuzz() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let program_id = Pubkey::new_unique();
    // Initialize and start the test network
    let program_test = ProgramTest::new(
        "program-test-fuzz",
        program_id,
        processor!(process_instruction),
    );

    let (bank, payer) = rt.block_on(program_test.start());

    // the honggfuzz `fuzz!` macro does not allow for async closures,
    // so we have to use the runtime directly to run async functions
    rt.block_on(run_fuzz_instructions(
        &[1, 2, 3, 4, 5],
        &bank,
        &payer,
        &program_id,
    ));
}

#[test]
fn simulate_fuzz_with_context() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let program_id = Pubkey::new_unique();
    // Initialize and start the test network
    let program_test = ProgramTest::new(
        "program-test-fuzz",
        program_id,
        processor!(process_instruction),
    );

    let context = rt.block_on(program_test.start_with_context());

    // the honggfuzz `fuzz!` macro does not allow for async closures,
    // so we have to use the runtime directly to run async functions
    rt.block_on(run_fuzz_instructions(
        &[1, 2, 3, 4, 5],
        &context.working_bank(),
        &context.payer,
        &program_id,
    ));
}

async fn run_fuzz_instructions(
    fuzz_instruction: &[u8],
    bank: &Arc<Bank>,
    payer: &Keypair,
    program_id: &Pubkey,
) {
    let mut instructions = vec![];
    let mut signer_keypairs = vec![];
    for &i in fuzz_instruction {
        let keypair = Keypair::new();
        let instruction = system_instruction::create_account(
            &payer.pubkey(),
            &keypair.pubkey(),
            Rent::default().minimum_balance(i as usize),
            i as u64,
            program_id,
        );
        instructions.push(instruction);
        instructions.push(Instruction::new_with_bincode(*program_id, &[0], vec![]));
        signer_keypairs.push(keypair);
    }
    // Process transaction on test network
    let mut transaction = Transaction::new_with_payer(&instructions, Some(&payer.pubkey()));
    let signers = [payer]
        .iter()
        .copied()
        .chain(signer_keypairs.iter())
        .collect::<Vec<&Keypair>>();
    transaction.partial_sign(&signers, bank.last_blockhash());

    bank.process_transaction(&transaction).unwrap();
    for keypair in signer_keypairs {
        let account = bank.get_account(&keypair.pubkey()).unwrap();
        assert!(account.lamports() > 0);
        assert!(!account.data().is_empty());
    }
}
