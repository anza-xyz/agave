use {
    solana_keypair::Keypair,
    solana_program_test::ProgramTestContext,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_runtime::genesis_utils::minimum_vote_account_balance_for_vat,
    solana_signer::Signer,
    solana_stake_interface::{
        instruction as stake_instruction,
        state::{Authorized, Lockup},
    },
    solana_system_interface::{instruction as system_instruction, program as system_program},
    solana_transaction::Transaction,
    solana_vote_program::{
        vote_instruction,
        vote_state::{
            self, VoteAuthorize, VoteInit, VoteStateV4, VoterWithBLSArgs,
            create_bls_pubkey_and_proof_of_possession,
        },
    },
};

pub async fn setup_stake(
    context: &mut ProgramTestContext,
    user: &Keypair,
    vote_address: &Pubkey,
    stake_lamports: u64,
) -> Pubkey {
    let stake_keypair = Keypair::new();
    let transaction = Transaction::new_signed_with_payer(
        &stake_instruction::create_account_and_delegate_stake(
            &context.payer.pubkey(),
            &stake_keypair.pubkey(),
            vote_address,
            &Authorized::auto(&user.pubkey()),
            &Lockup::default(),
            stake_lamports,
        ),
        Some(&context.payer.pubkey()),
        &vec![&context.payer, &stake_keypair, user],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();
    stake_keypair.pubkey()
}

pub async fn setup_vote(context: &mut ProgramTestContext) -> Pubkey {
    let mut instructions = vec![];
    let validator_keypair = Keypair::new();
    instructions.push(system_instruction::create_account(
        &context.payer.pubkey(),
        &validator_keypair.pubkey(),
        Rent::default().minimum_balance(0),
        0,
        &system_program::id(),
    ));
    let vote_lamports = if context.is_active(&agave_feature_set::validator_admission_ticket::id()) {
        // Fund vote accounts above VAT admission threshold
        minimum_vote_account_balance_for_vat(10)
    } else {
        Rent::default().minimum_balance(VoteStateV4::size_of())
    };

    let vote_keypair = Keypair::new();
    instructions.append(&mut vote_instruction::create_account_with_config(
        &context.payer.pubkey(),
        &vote_keypair.pubkey(),
        &VoteInit {
            node_pubkey: validator_keypair.pubkey(),
            authorized_voter: vote_keypair.pubkey(),
            ..VoteInit::default()
        },
        vote_lamports,
        vote_instruction::CreateVoteAccountConfig {
            space: vote_state::VoteStateV4::size_of() as u64,
            ..vote_instruction::CreateVoteAccountConfig::default()
        },
    ));

    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&context.payer.pubkey()),
        &vec![&context.payer, &validator_keypair, &vote_keypair],
        context.last_blockhash,
    );
    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();

    if context.is_active(&agave_feature_set::bls_pubkey_management_in_vote_account::id()) {
        let (bls_pubkey, bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_keypair.pubkey());
        let bls_transaction = Transaction::new_signed_with_payer(
            &[vote_instruction::authorize(
                &vote_keypair.pubkey(),
                &vote_keypair.pubkey(),
                &vote_keypair.pubkey(),
                VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
                    bls_pubkey,
                    bls_proof_of_possession,
                }),
            )],
            Some(&context.payer.pubkey()),
            &vec![&context.payer, &vote_keypair],
            context.last_blockhash,
        );
        context
            .banks_client
            .process_transaction(bls_transaction)
            .await
            .unwrap();
    }

    vote_keypair.pubkey()
}
