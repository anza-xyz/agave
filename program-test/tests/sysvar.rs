use {
    core::{mem::size_of, slice::from_raw_parts_mut},
    solana_account_info::AccountInfo,
    solana_clock::Clock,
    solana_epoch_rewards::EpochRewards,
    solana_epoch_schedule::EpochSchedule,
    solana_instruction::Instruction,
    solana_msg::msg,
    solana_program_error::ProgramResult,
    solana_program_test::{ProgramTest, processor},
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_signer::Signer,
    solana_sysvar::Sysvar,
    solana_sysvar_id::SysvarId,
    solana_transaction::Transaction,
};

// Process instruction to invoke into another program
fn sysvar_getter_process_instruction(
    _program_id: &Pubkey,
    _accounts: &[AccountInfo],
    _input: &[u8],
) -> ProgramResult {
    msg!("sysvar_getter");

    let clock = Clock::get()?;
    assert_eq!(42, clock.slot);

    let epoch_schedule = EpochSchedule::get()?;
    assert_eq!(epoch_schedule, EpochSchedule::default());

    let rent = Rent::get()?;
    assert_eq!(rent.exemption_threshold, 1.0);

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

    // input[0] == 0 indicates the bank is not in reward period.
    // input[0] == 1 indicates the bank is in reward period.
    if input[0] == 0 {
        // epoch rewards sysvar should not exist for banks that are not in reward period
        let epoch_rewards = EpochRewards::get()?;
        assert!(!epoch_rewards.active);
    } else {
        let epoch_rewards = EpochRewards::get()?;
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

    let clock_get = Clock::get()?;

    let mut buffer = [0u64; size_of::<Clock>() / size_of::<u64>()];
    let length = size_of::<Clock>();
    let bytes = unsafe { from_raw_parts_mut(buffer.as_mut_ptr() as *mut u8, length) };

    solana_sysvar::get_sysvar(bytes, &Clock::id(), 0, length as u64)?;

    let clock_get_sysvar = unsafe { &*(buffer.as_ptr() as *const Clock) };

    assert_eq!(clock_get, *clock_get_sysvar);

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
