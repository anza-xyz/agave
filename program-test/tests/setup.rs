use {
    agave_feature_set::{bls_pubkey_management_in_vote_account, vote_state_v4},
    solana_banks_client::BanksClient,
    solana_keypair::Keypair,
    solana_program_test::ProgramTestContext,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
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
            self, create_bls_pubkey_and_proof_of_possession, VoteInit, VoteInitV2, VoteStateV4,
        },
    },
};

async fn is_feature_active(banks_client: &mut BanksClient, feature_id: Pubkey) -> bool {
    banks_client
        .get_account(feature_id)
        .await
        .unwrap()
        .and_then(|account| solana_feature_gate_interface::from_account(&account))
        .is_some_and(|feature| feature.activated_at.is_some())
}

async fn is_bls_pubkey_feature_enabled(banks_client: &mut BanksClient) -> bool {
    is_feature_active(banks_client, bls_pubkey_management_in_vote_account::id()).await
        && is_feature_active(banks_client, vote_state_v4::id()).await
}

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
    let vote_lamports = Rent::default().minimum_balance(VoteStateV4::size_of());
    let vote_keypair = Keypair::new();
    let user_keypair = Keypair::new();

    if is_bls_pubkey_feature_enabled(&mut context.banks_client).await {
        // Use V2 instruction with BLS pubkey.
        let (bls_pubkey, bls_proof_of_possession) =
            create_bls_pubkey_and_proof_of_possession(&vote_keypair.pubkey());
        instructions.append(&mut vote_instruction::create_account_with_config_v2(
            &context.payer.pubkey(),
            &vote_keypair.pubkey(),
            &VoteInitV2 {
                node_pubkey: validator_keypair.pubkey(),
                authorized_voter: user_keypair.pubkey(),
                authorized_voter_bls_pubkey: bls_pubkey,
                authorized_voter_bls_proof_of_possession: bls_proof_of_possession,
                ..Default::default()
            },
            vote_lamports,
            vote_instruction::CreateVoteAccountConfig {
                space: vote_state::VoteStateV4::size_of() as u64,
                ..vote_instruction::CreateVoteAccountConfig::default()
            },
        ));
    } else {
        // Use V1 instruction.
        instructions.append(&mut vote_instruction::create_account_with_config(
            &context.payer.pubkey(),
            &vote_keypair.pubkey(),
            &VoteInit {
                node_pubkey: validator_keypair.pubkey(),
                authorized_voter: user_keypair.pubkey(),
                ..VoteInit::default()
            },
            vote_lamports,
            vote_instruction::CreateVoteAccountConfig {
                space: vote_state::VoteStateV4::size_of() as u64,
                ..vote_instruction::CreateVoteAccountConfig::default()
            },
        ));
    }

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

    vote_keypair.pubkey()
}
