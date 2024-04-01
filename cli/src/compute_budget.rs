use {
    solana_client::rpc_config::RpcSimulateTransactionConfig,
    solana_rpc_client::rpc_client::RpcClient,
    solana_sdk::{
        borsh1::try_from_slice_unchecked,
        compute_budget::{self, ComputeBudgetInstruction},
        transaction::Transaction,
    },
};

// This enum is equivalent to an Option but was added to self-document
// the ok variants and has the benefit of not forcing the caller to use
// the result if they don't care about it.
pub(crate) enum UpdateComputeUnitLimitResult {
    UpdatedInstructionIndex(usize),
    NoInstructionFound,
}

// Returns the index of the compute unit limit instruction
pub(crate) fn simulate_and_update_compute_unit_limit(
    rpc_client: &RpcClient,
    transaction: &mut Transaction,
) -> Result<UpdateComputeUnitLimitResult, Box<dyn std::error::Error>> {
    let Some(compute_unit_limit_ix_index) = transaction
        .message
        .instructions
        .iter()
        .enumerate()
        .find_map(|(ix_index, instruction)| {
            let ix_program_id = transaction.message.program_id(ix_index)?;
            if ix_program_id != &compute_budget::id() {
                return None;
            }

            matches!(
                try_from_slice_unchecked(&instruction.data),
                Ok(ComputeBudgetInstruction::SetComputeUnitLimit(_))
            )
            .then_some(ix_index)
        })
    else {
        return Ok(UpdateComputeUnitLimitResult::NoInstructionFound);
    };

    let simulate_result = rpc_client
        .simulate_transaction_with_config(
            transaction,
            RpcSimulateTransactionConfig {
                replace_recent_blockhash: true,
                commitment: Some(rpc_client.commitment()),
                ..RpcSimulateTransactionConfig::default()
            },
        )?
        .value;

    // Bail if the simulated transaction failed
    if let Some(err) = simulate_result.err {
        return Err(err.into());
    }

    let units_consumed = simulate_result
        .units_consumed
        .expect("compute units unavailable");

    // Overwrite the compute unit limit instruction with the actual units consumed
    let compute_unit_limit = u32::try_from(units_consumed)?;
    transaction.message.instructions[compute_unit_limit_ix_index].data =
        ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit).data;

    Ok(UpdateComputeUnitLimitResult::UpdatedInstructionIndex(
        compute_unit_limit_ix_index,
    ))
}
