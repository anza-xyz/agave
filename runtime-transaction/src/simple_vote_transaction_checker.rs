use {solana_pubkey::Pubkey, solana_svm_transaction::instruction::SVMInstruction};

/// Simple vote transaction meets these conditions:
/// 1. has 1 or 2 signatures;
/// 2. is a legacy message;
/// 3. has 1 to 3 instructions;
/// 4. first instruction is a vote instruction;
/// 5. optional second instruction is `SetComputeUnitLimit`;
/// 6. optional third instruction is `SetLoadedAccountsDataSizeLimit`.
///
/// Static instruction layout note:
///
/// Other compute-budget instruction orders may be valid runtime transactions,
/// but they are not marked `SIMPLE_VOTE_TX` here. Keeping this classifier narrow
/// avoids compute-budget deserialization and keeps the SigVerify hot path
/// branch-light and easy to audit before AG is activated.
pub fn is_simple_vote_transaction<'a>(
    num_signatures: usize,
    is_legacy_message: bool,
    mut program_instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
) -> bool {
    if num_signatures > 2 || !is_legacy_message {
        return false;
    }

    // has 1 to 3 instructions...
    let Some((program_id, _instruction)) = program_instructions.next() else {
        return false;
    };
    if *program_id != solana_sdk_ids::vote::id() {
        return false;
    }

    let Some((program_id, instruction)) = program_instructions.next() else {
        return true;
    };
    if !is_compute_budget_instruction(
        program_id,
        &instruction,
        SET_COMPUTE_UNIT_LIMIT_DISCRIMINATOR,
    ) {
        return false;
    }

    let Some((program_id, instruction)) = program_instructions.next() else {
        return true;
    };
    is_compute_budget_instruction(
        program_id,
        &instruction,
        SET_LOADED_ACCOUNTS_DATA_SIZE_LIMIT_DISCRIMINATOR,
    ) && program_instructions.next().is_none()
}

// Local wire-format discriminators for the compute-budget instructions accepted
// by the temporary SigVerify simple-vote fast path. These mirror
// `solana_compute_budget_interface::ComputeBudgetInstruction` serialization.
const SET_COMPUTE_UNIT_LIMIT_DISCRIMINATOR: u8 = 2;
const SET_LOADED_ACCOUNTS_DATA_SIZE_LIMIT_DISCRIMINATOR: u8 = 4;

fn is_compute_budget_instruction(
    program_id: &Pubkey,
    instruction: &SVMInstruction,
    discriminator: u8,
) -> bool {
    // Both SetComputeUnitLimit and SetLoadedAccountsDataSizeLimit have `u32` data.
    const COMPUTE_BUDGET_INSTRUCTION_DATA_LEN: usize = 1 + core::mem::size_of::<u32>();

    *program_id == solana_sdk_ids::compute_budget::id()
        && instruction.data.len() == COMPUTE_BUDGET_INSTRUCTION_DATA_LEN
        && instruction.data[0] == discriminator
}
