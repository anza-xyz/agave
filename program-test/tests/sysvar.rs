use {
    solana_account_info::{AccountInfo, next_account_info},
    solana_clock::Clock,
    solana_epoch_rewards::EpochRewards,
    solana_epoch_schedule::EpochSchedule,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_msg::msg,
    solana_program::program::invoke,
    solana_program_entrypoint::SUCCESS,
    solana_program_error::{ProgramError, ProgramResult},
    solana_program_test::{ProgramTest, processor},
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::sysvar::{clock, epoch_schedule, rent},
    solana_signer::Signer,
    solana_system_interface::instruction as system_instruction,
    solana_sysvar::{Sysvar, SysvarSerialize},
    solana_transaction::Transaction,
};

// Process instruction to invoke into another program
fn sysvar_getter_process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _input: &[u8],
) -> ProgramResult {
    msg!("sysvar_getter");

    let mut clock = Clock::default();
    assert_eq!(
        solana_program_test::sol_get_clock_sysvar(&mut clock as *mut _ as *mut u8),
        SUCCESS,
    );
    assert_eq!(42, clock.slot);

    let mut epoch_schedule = EpochSchedule::default();
    assert_eq!(
        solana_program_test::sol_get_epoch_schedule_sysvar(
            &mut epoch_schedule as *mut _ as *mut u8,
        ),
        SUCCESS,
    );
    assert_eq!(epoch_schedule, EpochSchedule::default());

    let mut rent = Rent::default();
    assert_eq!(
        solana_program_test::sol_get_rent_sysvar(&mut rent as *mut _ as *mut u8),
        SUCCESS,
    );
    assert_eq!(
        rent.lamports_per_byte,
        solana_rent::DEFAULT_LAMPORTS_PER_BYTE
    );

    Ok(())
}

#[tokio::test]
async fn get_sysvar() {
    let program_id = Pubkey::new_unique();
    let program_test = ProgramTest::new(
        "sysvar_getter",
        program_id,
        processor!(sysvar_getter_process_instruction),
    );

    let mut context = program_test.start_with_context().await;
    context.warp_to_slot(42).unwrap();
    let instructions = vec![Instruction::new_with_bincode(program_id, &(), vec![])];

    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );

    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();
}

fn epoch_reward_sysvar_getter_process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    input: &[u8],
) -> ProgramResult {
    msg!("epoch_reward_sysvar_getter");

    let mut epoch_rewards = EpochRewards::default();
    assert_eq!(
        solana_program_test::sol_get_epoch_rewards_sysvar(&mut epoch_rewards as *mut _ as *mut u8),
        SUCCESS,
    );

    // input[0] == 0 indicates the bank is not in reward period.
    // input[0] == 1 indicates the bank is in reward period.
    if input[0] == 0 {
        // epoch rewards sysvar should not exist for banks that are not in reward period
        assert!(!epoch_rewards.active);
    } else {
        assert!(epoch_rewards.active);
    }

    Ok(())
}

#[tokio::test]
async fn get_epoch_rewards_sysvar() {
    let program_id = Pubkey::new_unique();
    let program_test = ProgramTest::new(
        "epoch_reward_sysvar_getter",
        program_id,
        processor!(epoch_reward_sysvar_getter_process_instruction),
    );

    let mut context = program_test.start_with_context().await;

    // wrap to 1st slot before next epoch (outside reward interval)
    let first_normal_slot = context.genesis_config().epoch_schedule.first_normal_slot;
    let slots_per_epoch = context.genesis_config().epoch_schedule.slots_per_epoch;
    let last_slot_before_new_epoch = first_normal_slot
        .saturating_add(slots_per_epoch)
        .saturating_sub(1);
    context.warp_to_slot(last_slot_before_new_epoch).unwrap();

    // outside of reward interval, set input[0] == 0, so that the program assert that epoch_rewards sysvar doesn't exist.
    let instructions = vec![Instruction::new_with_bincode(program_id, &[0u8], vec![])];
    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );

    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();

    // wrap to 1st slot of next epoch (inside reward interval)
    let first_slot_in_new_epoch = first_normal_slot.saturating_add(slots_per_epoch);
    context.warp_to_slot(first_slot_in_new_epoch).unwrap();

    // inside of reward interval, set input[0] == 1, so that the program assert that epoch_rewards sysvar exist.
    let instructions = vec![Instruction::new_with_bincode(program_id, &[1u8], vec![])];
    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );

    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();
}

// Process instruction to get the clock sysvar
fn clock_sol_get_sysvar_process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _input: &[u8],
) -> ProgramResult {
    msg!("clock_sol_get_sysvar");

    let mut clock = Clock::default();
    assert_eq!(
        solana_program_test::sol_get_clock_sysvar(&mut clock as *mut _ as *mut u8),
        SUCCESS,
    );
    assert_eq!(42, clock.slot);

    Ok(())
}

#[tokio::test]
async fn clock_sol_get_sysvar() {
    let program_id = Pubkey::new_unique();
    let program_test = ProgramTest::new(
        "clock_sol_get_sysvar",
        program_id,
        processor!(clock_sol_get_sysvar_process_instruction),
    );

    let mut context = program_test.start_with_context().await;
    context.warp_to_slot(42).unwrap();
    let instructions = vec![Instruction::new_with_bincode(program_id, &(), vec![])];

    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );

    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();
}

async fn process_transaction(
    program_test: ProgramTest,
    program_id: Pubkey,
    accounts: Vec<AccountMeta>,
) {
    let mut context = program_test.start_with_context().await;
    context.warp_to_slot(42).unwrap();

    let transaction = Transaction::new_signed_with_payer(
        &[Instruction::new_with_bytes(program_id, &[], accounts)],
        Some(&context.payer.pubkey()),
        &[&context.payer],
        context.last_blockhash,
    );

    context
        .banks_client
        .process_transaction(transaction)
        .await
        .unwrap();
}

fn get_sysvar_process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _input: &[u8],
) -> ProgramResult {
    assert_eq!(Clock::get(), Err(ProgramError::UnsupportedSysvar));
    assert_eq!(EpochRewards::get(), Err(ProgramError::UnsupportedSysvar));
    assert_eq!(EpochSchedule::get(), Err(ProgramError::UnsupportedSysvar));
    assert_eq!(Rent::get(), Err(ProgramError::UnsupportedSysvar));
    Ok(())
}

#[tokio::test]
async fn get_sysvar_returns_unsupported_in_native_program_test() {
    let program_id = Pubkey::new_unique();
    let program_test = ProgramTest::new(
        "get_sysvar",
        program_id,
        processor!(get_sysvar_process_instruction),
    );
    process_transaction(program_test, program_id, vec![]).await;
}

fn from_account_info_process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    _input: &[u8],
) -> ProgramResult {
    msg!("from_account_info");

    let account_info_iter = &mut accounts.iter();
    let clock_info = next_account_info(account_info_iter)?;
    let epoch_schedule_info = next_account_info(account_info_iter)?;
    let rent_info = next_account_info(account_info_iter)?;

    let clock = Clock::from_account_info(clock_info)?;
    assert_eq!(42, clock.slot);

    let epoch_schedule = EpochSchedule::from_account_info(epoch_schedule_info)?;
    assert_eq!(epoch_schedule, EpochSchedule::default());

    let rent = Rent::from_account_info(rent_info)?;
    assert_eq!(
        rent.lamports_per_byte,
        solana_rent::DEFAULT_LAMPORTS_PER_BYTE
    );

    Ok(())
}

#[tokio::test]
async fn sysvar_from_account_info_works_in_native_program_test() {
    let program_id = Pubkey::new_unique();
    let program_test = ProgramTest::new(
        "from_account_info",
        program_id,
        processor!(from_account_info_process_instruction),
    );
    process_transaction(
        program_test,
        program_id,
        vec![
            AccountMeta::new_readonly(clock::id(), false),
            AccountMeta::new_readonly(epoch_schedule::id(), false),
            AccountMeta::new_readonly(rent::id(), false),
        ],
    )
    .await;
}

// Sysvar getters fail in native processors but work in a BPF callee reached via CPI.
// InitializeAccount3 reads rent through the getter, so it proves the callee gets the real syscall.
fn cpi_initialize_account3_process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    input: &[u8],
) -> ProgramResult {
    let owner = Pubkey::try_from(input).map_err(|_| ProgramError::InvalidInstructionData)?;

    let account_info_iter = &mut accounts.iter();
    let token_program_info = next_account_info(account_info_iter)?;
    let token_account_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;

    // The native caller has no syscall-backed sysvar access
    assert_eq!(Rent::get(), Err(ProgramError::UnsupportedSysvar));

    // but the BPF callee does, one CPI level down
    let mut data = vec![18]; // InitializeAccount3 tag
    data.extend_from_slice(owner.as_ref());
    invoke(
        &Instruction {
            program_id: *token_program_info.key,
            accounts: vec![
                AccountMeta::new(*token_account_info.key, false),
                AccountMeta::new_readonly(*mint_info.key, false),
            ],
            data,
        },
        &[
            token_account_info.clone(),
            mint_info.clone(),
            token_program_info.clone(),
        ],
    )
}

#[tokio::test]
async fn get_sysvar_works_in_bpf_program_invoked_from_native_program_test() {
    let proxy_program_id = Pubkey::new_unique();
    let token_program_id = spl_generic_token::token::id();
    let program_test = ProgramTest::new(
        "cpi_initialize_account3",
        proxy_program_id,
        processor!(cpi_initialize_account3_process_instruction),
    );
    let (banks_client, payer, recent_blockhash) = program_test.start().await;

    let rent_sysvar = banks_client.get_rent().await.unwrap();
    let mint = Keypair::new();
    let token_account = Keypair::new();
    let owner = Pubkey::new_unique();
    let mint_space = 82;
    let token_account_space = 165;

    let mut initialize_mint_data = vec![
        0, // InitializeMint instruction tag
        0, // decimals
    ];
    initialize_mint_data.extend_from_slice(payer.pubkey().as_ref()); // mint authority
    initialize_mint_data.push(0); // no freeze authority

    let transaction = Transaction::new_signed_with_payer(
        &[
            system_instruction::create_account(
                &payer.pubkey(),
                &mint.pubkey(),
                rent_sysvar.minimum_balance(mint_space),
                mint_space as u64,
                &token_program_id,
            ),
            Instruction {
                program_id: token_program_id,
                accounts: vec![
                    AccountMeta::new(mint.pubkey(), false),
                    AccountMeta::new_readonly(rent::id(), false),
                ],
                data: initialize_mint_data,
            },
            system_instruction::create_account(
                &payer.pubkey(),
                &token_account.pubkey(),
                rent_sysvar.minimum_balance(token_account_space),
                token_account_space as u64,
                &token_program_id,
            ),
            Instruction {
                program_id: proxy_program_id,
                accounts: vec![
                    AccountMeta::new_readonly(token_program_id, false),
                    AccountMeta::new(token_account.pubkey(), false),
                    AccountMeta::new_readonly(mint.pubkey(), false),
                ],
                data: owner.to_bytes().to_vec(),
            },
        ],
        Some(&payer.pubkey()),
        &[&payer, &mint, &token_account],
        recent_blockhash,
    );

    banks_client.process_transaction(transaction).await.unwrap();

    let account = banks_client
        .get_account(token_account.pubkey())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&account.data[32..64], owner.as_ref());
}
