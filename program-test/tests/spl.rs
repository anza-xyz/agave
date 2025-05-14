use {
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
    let (banks_client, _, _) = ProgramTest::default().start().await;
    let rent = banks_client.get_rent().await.unwrap();
    let token_2022_id = spl_generic_token::token_2022::id();
    let (token_2022_programdata_id, _) =
        Pubkey::find_program_address(&[token_2022_id.as_ref()], &bpf_loader_upgradeable::id());

    for (program_id, _) in spl_programs(&rent) {
        let program_account = banks_client.get_account(program_id).await.unwrap().unwrap();
        if program_id == token_2022_id || program_id == token_2022_programdata_id {
            assert_eq!(program_account.owner, bpf_loader_upgradeable::id());
        } else {
            assert_eq!(program_account.owner, bpf_loader::id());
        }
    }
}

#[tokio::test]
async fn token_2022() {
    let (banks_client, payer, recent_blockhash) = ProgramTest::default().start().await;

    let token_2022_id = spl_generic_token::token_2022::id();
    let mint = Keypair::new();
    let rent = banks_client.get_rent().await.unwrap();
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
        recent_blockhash,
    );

    banks_client.process_transaction(transaction).await.unwrap();
}
