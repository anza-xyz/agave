use {
    solana_borsh::v1::try_from_slice_unchecked,
    solana_clap_utils::compute_budget::ComputeUnitLimit,
    solana_compute_budget_interface::{self as compute_budget, ComputeBudgetInstruction},
    solana_instruction::Instruction,
    solana_message::{VersionedMessage, compiled_instruction::CompiledInstruction},
    solana_program_runtime::execution_budget::MAX_COMPUTE_UNIT_LIMIT,
    solana_pubkey::Pubkey,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::config::RpcSimulateTransactionConfig,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
};

fn get_compute_unit_limit_instruction_index(
    instructions: &[CompiledInstruction],
    account_keys: &[Pubkey],
) -> Option<usize> {
    instructions.iter().position(|instruction| {
        account_keys.get(usize::from(instruction.program_id_index)) == Some(&compute_budget::id())
            && matches!(
                try_from_slice_unchecked(&instruction.data),
                Ok(ComputeBudgetInstruction::SetComputeUnitLimit(_))
            )
    })
}

/// Simulate a message without checking whether its CU limit needs updating.
async fn simulate_for_compute_unit_limit_unchecked(
    rpc_client: &RpcClient,
    message: VersionedMessage,
) -> Result<u32, Box<dyn std::error::Error>> {
    let transaction = VersionedTransaction {
        signatures: vec![
            Signature::default();
            usize::from(message.header().num_required_signatures)
        ],
        message,
    };
    let simulate_result = rpc_client
        .simulate_transaction_with_config(
            &transaction,
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
        .expect("compute units unavailable");

    u32::try_from(units_consumed).map_err(Into::into)
}

/// Simulate and apply the compute unit limit, returning it for reuse in other messages.
pub(crate) async fn simulate_and_update_compute_unit_limit(
    compute_unit_limit: &ComputeUnitLimit,
    rpc_client: &RpcClient,
    message: &mut VersionedMessage,
) -> Result<Option<u32>, Box<dyn std::error::Error>> {
    if !matches!(
        compute_unit_limit,
        ComputeUnitLimit::Simulated | ComputeUnitLimit::SimulatedWithExtraPercentage(_)
    ) {
        return Ok(None);
    }
    if !matches!(message, VersionedMessage::V1(_))
        && get_compute_unit_limit_instruction_index(
            message.instructions(),
            message.static_account_keys(),
        )
        .is_none()
    {
        return Ok(None);
    }
    let base_compute_unit_limit =
        simulate_for_compute_unit_limit_unchecked(rpc_client, message.clone()).await?;
    let compute_unit_limit =
        if let ComputeUnitLimit::SimulatedWithExtraPercentage(n) = compute_unit_limit {
            (base_compute_unit_limit as u64)
                .saturating_mul(100_u64.saturating_add(*n as u64))
                .saturating_div(100) as u32
        } else {
            base_compute_unit_limit
        };
    set_compute_unit_limit(message, compute_unit_limit);
    Ok(Some(compute_unit_limit))
}

/// Apply a simulated limit, preserving V1's explicit priority fee.
/// Changing the limit can change Legacy/V0 priority fees.
pub(crate) fn set_compute_unit_limit(message: &mut VersionedMessage, limit: u32) {
    let (instructions, account_keys) = match message {
        VersionedMessage::Legacy(message) => (&mut message.instructions, &message.account_keys),
        VersionedMessage::V0(message) => (&mut message.instructions, &message.account_keys),
        VersionedMessage::V1(message) => {
            message.config.compute_unit_limit = Some(limit);
            return;
        }
    };
    let index = get_compute_unit_limit_instruction_index(instructions, account_keys)
        .expect("simulated message has a compute unit limit instruction");
    instructions[index].data = ComputeBudgetInstruction::set_compute_unit_limit(limit).data;
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
