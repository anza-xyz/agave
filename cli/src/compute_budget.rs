use {
    solana_borsh::v1::try_from_slice_unchecked,
    solana_clap_utils::compute_budget::ComputeUnitLimit,
    solana_compute_budget_interface::{self as compute_budget, ComputeBudgetInstruction},
    solana_instruction::Instruction,
    solana_keypair::Signature,
    solana_message::{Message, VersionedMessage},
    solana_program_runtime::execution_budget::MAX_COMPUTE_UNIT_LIMIT,
    solana_pubkey::Pubkey,
    solana_rpc_client::{nonblocking::rpc_client::RpcClient, rpc_client::SerializableTransaction},
    solana_rpc_client_api::config::RpcSimulateTransactionConfig,
    solana_transaction::{CompiledInstruction, Transaction, versioned::VersionedTransaction},
};

/// Enum capturing the possible results of updating a message based on the
/// compute unit limits consumed during simulation.
pub(crate) enum UpdateComputeUnitLimitResult {
    UpdatedInstructionIndex(usize),
    UpdatedTransactionConfig,
    NoComputeUnitLimitFound,
    SimulationNotConfigured,
}

fn get_compute_unit_limit_instruction_index_legacy_v0(
    instructions: &[CompiledInstruction],
    account_keys: &[Pubkey],
) -> Option<usize> {
    instructions
        .iter()
        .enumerate()
        .find_map(|(ix_index, instruction)| {
            let ix_program_id = account_keys.get(instruction.program_id_index as usize)?;
            if ix_program_id != &compute_budget::id() {
                return None;
            }

            matches!(
                try_from_slice_unchecked(&instruction.data),
                Ok(ComputeBudgetInstruction::SetComputeUnitLimit(_))
            )
            .then_some(ix_index)
        })
}

/// Like `simulate_for_compute_unit_limit`, but does not check that the message
/// contains a compute unit limit instruction.
async fn simulate_for_compute_unit_limit_unchecked(
    rpc_client: &RpcClient,
    transaction: &impl SerializableTransaction,
) -> Result<u32, Box<dyn std::error::Error>> {
    let simulate_result = rpc_client
        .simulate_transaction_with_config(
            transaction,
            RpcSimulateTransactionConfig {
                replace_recent_blockhash: true,
                commitment: Some(rpc_client.commitment()),
                ..RpcSimulateTransactionConfig::default()
            },
        )
        .await?
        .value;

    // Bail if the simulated transaction failed
    if let Some(err) = simulate_result.err {
        return Err(err.into());
    }

    let units_consumed = simulate_result
        .units_consumed
        .ok_or("compute units unavailable")?;

    u32::try_from(units_consumed).map_err(Into::into)
}

/// Simulates a message and returns the index of the compute unit limit
/// instruction
///
/// If the message does not contain a compute unit limit instruction, or if
/// simulation was not configured, then the function will not simulate the
/// message.
///
/// Prefer [`simulate_and_update_compute_unit_limit_versioned`] for new callers.
pub(crate) async fn simulate_and_update_compute_unit_limit(
    compute_unit_limit: &ComputeUnitLimit,
    rpc_client: &RpcClient,
    message: &mut Message,
) -> Result<UpdateComputeUnitLimitResult, Box<dyn std::error::Error>> {
    let Some(compute_unit_limit_ix_index) = get_compute_unit_limit_instruction_index_legacy_v0(
        &message.instructions,
        &message.account_keys,
    ) else {
        return Ok(UpdateComputeUnitLimitResult::NoComputeUnitLimitFound);
    };

    match compute_unit_limit {
        ComputeUnitLimit::Simulated | ComputeUnitLimit::SimulatedWithExtraPercentage(_) => {
            let transaction = Transaction::new_unsigned(message.clone());
            let base_compute_unit_limit =
                simulate_for_compute_unit_limit_unchecked(rpc_client, &transaction).await?;

            let compute_unit_limit =
                if let ComputeUnitLimit::SimulatedWithExtraPercentage(n) = compute_unit_limit {
                    (base_compute_unit_limit as u64)
                        .saturating_mul(100_u64.saturating_add(*n as u64))
                        .saturating_div(100) as u32
                } else {
                    base_compute_unit_limit
                };

            // Overwrite the compute unit limit instruction with the actual units consumed
            message.instructions[compute_unit_limit_ix_index].data =
                ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit).data;

            Ok(UpdateComputeUnitLimitResult::UpdatedInstructionIndex(
                compute_unit_limit_ix_index,
            ))
        }
        ComputeUnitLimit::Static(_) | ComputeUnitLimit::Default => {
            Ok(UpdateComputeUnitLimitResult::SimulationNotConfigured)
        }
    }
}

pub(crate) struct ComputeUnitConfig {
    pub(crate) compute_unit_price: Option<u64>,
    pub(crate) compute_unit_limit: ComputeUnitLimit,
}

pub(crate) trait WithComputeUnitConfig {
    fn with_compute_unit_config(self, config: &ComputeUnitConfig) -> Self;
}

impl WithComputeUnitConfig for Vec<Instruction> {
    fn with_compute_unit_config(mut self, config: &ComputeUnitConfig) -> Self {
        if let Some(compute_unit_price) = config.compute_unit_price {
            self.push(ComputeBudgetInstruction::set_compute_unit_price(
                compute_unit_price,
            ));
            match config.compute_unit_limit {
                ComputeUnitLimit::Default => {}
                ComputeUnitLimit::Static(compute_unit_limit) => {
                    self.push(ComputeBudgetInstruction::set_compute_unit_limit(
                        compute_unit_limit,
                    ));
                }
                ComputeUnitLimit::Simulated | ComputeUnitLimit::SimulatedWithExtraPercentage(_) => {
                    // Default to the max compute unit limit because later transactions will be
                    // simulated to get the exact compute units consumed.
                    self.push(ComputeBudgetInstruction::set_compute_unit_limit(
                        MAX_COMPUTE_UNIT_LIMIT,
                    ));
                }
            }
        }
        self
    }
}

/// Simulates a message and updates the compute unit limit instruction (Legacy/V0)
/// or transaction configuration (V1).
///
/// If the message does not contain an explicit compute unit limit, or if
/// simulation was not configured, then the function will not simulate the
/// message.
pub(crate) async fn simulate_and_update_compute_unit_limit_versioned(
    compute_unit_limit: &ComputeUnitLimit,
    rpc_client: &RpcClient,
    message: &mut VersionedMessage,
) -> Result<UpdateComputeUnitLimitResult, Box<dyn std::error::Error>> {
    let compute_unit_limit_ix_index = match message {
        VersionedMessage::V1(message) => {
            if message.config.compute_unit_limit.is_none() {
                return Ok(UpdateComputeUnitLimitResult::NoComputeUnitLimitFound);
            }
            // V1 has a configured limit, but no instruction index.
            None
        }
        _ => {
            let Some(index) = get_compute_unit_limit_instruction_index_legacy_v0(
                message.instructions(),
                message.static_account_keys(),
            ) else {
                return Ok(UpdateComputeUnitLimitResult::NoComputeUnitLimitFound);
            };
            Some(index)
        }
    };

    match compute_unit_limit {
        ComputeUnitLimit::Simulated | ComputeUnitLimit::SimulatedWithExtraPercentage(_) => {
            let transaction = VersionedTransaction {
                signatures: vec![
                    Signature::default();
                    message.header().num_required_signatures as usize
                ],
                message: message.clone(),
            };
            let base_compute_unit_limit =
                simulate_for_compute_unit_limit_unchecked(rpc_client, &transaction).await?;

            let compute_unit_limit =
                if let ComputeUnitLimit::SimulatedWithExtraPercentage(n) = compute_unit_limit {
                    (base_compute_unit_limit as u64)
                        .saturating_mul(100_u64.saturating_add(*n as u64))
                        .saturating_div(100) as u32
                } else {
                    base_compute_unit_limit
                };

            let instructions = match message {
                VersionedMessage::Legacy(message) => &mut message.instructions,
                VersionedMessage::V0(message) => &mut message.instructions,
                VersionedMessage::V1(message) => {
                    message.config.compute_unit_limit = Some(compute_unit_limit);
                    return Ok(UpdateComputeUnitLimitResult::UpdatedTransactionConfig);
                }
            };
            let compute_unit_limit_ix_index = compute_unit_limit_ix_index
                .expect("Legacy and V0 instruction index was checked above");

            // Overwrite the compute unit limit instruction with the actual units consumed
            instructions[compute_unit_limit_ix_index].data =
                ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit).data;

            Ok(UpdateComputeUnitLimitResult::UpdatedInstructionIndex(
                compute_unit_limit_ix_index,
            ))
        }
        ComputeUnitLimit::Static(_) | ComputeUnitLimit::Default => {
            Ok(UpdateComputeUnitLimitResult::SimulationNotConfigured)
        }
    }
}
