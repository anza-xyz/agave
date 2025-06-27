use {
    solana_account::ReadableAccount,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_program_test::{programs::spl_programs, ProgramTest},
    solana_pubkey::Pubkey,
    solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable},
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_sysvar::rent,
    solana_transaction::Transaction,
};

#[tokio::test]
async fn programs_present() {
    let (bank, _) = ProgramTest::default().start().await;
    let rent = &bank.rent_collector().rent;
    let token_2022_id = spl_generic_token::token_2022::id();
    let (token_2022_programdata_id, _) =
        Pubkey::find_program_address(&[token_2022_id.as_ref()], &bpf_loader_upgradeable::id());

    for (program_id, _) in spl_programs(rent) {
        let program_account = bank.get_account(&program_id).unwrap();
        if program_id == token_2022_id || program_id == token_2022_programdata_id {
            assert_eq!(program_account.owner(), &bpf_loader_upgradeable::id());
        } else {
            assert_eq!(program_account.owner(), &bpf_loader::id());
        }
    }
}

#[tokio::test]
async fn token_2022() {
    let (bank, payer) = ProgramTest::default().start().await;

    let token_2022_id = spl_generic_token::token_2022::id();
    let mint = Keypair::new();
    let rent = &bank.rent_collector().rent;
    let space = 82;
    let transaction = Transaction::new_signed_with_payer(
        &[
            system_instruction::create_account(
                &payer.pubkey(),
                &mint.pubkey(),
                rent.minimum_balance(space),
                space as u64,
                &token_2022_id,
            ),
            Instruction::new_with_bytes(
                token_2022_id,
                &[0; 35], // initialize mint
                vec![
                    AccountMeta::new(mint.pubkey(), false),
                    AccountMeta::new_readonly(rent::id(), false),
                ],
            ),
        ],
        Some(&payer.pubkey()),
        &[&payer, &mint],
        bank.last_blockhash(),
    );

    bank.process_transaction(&transaction).unwrap();
}
