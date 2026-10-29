use {
    crate::message_builder::priority_fee,
    solana_borsh::v1::try_from_slice_unchecked,
    solana_clap_utils::compute_budget::ComputeUnitLimit,
    solana_compute_budget_interface::{self as compute_budget, ComputeBudgetInstruction},
    solana_message::{VersionedMessage, compiled_instruction::CompiledInstruction},
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
        .ok_or("compute units unavailable")?;

    u32::try_from(units_consumed).map_err(Into::into)
}

/// Simulate and apply the compute unit limit, returning it for reuse in other messages.
pub(crate) async fn simulate_and_update_compute_unit_limit(
    compute_unit_limit: &ComputeUnitLimit,
    compute_unit_price: Option<u64>,
    rpc_client: &RpcClient,
    message: &mut VersionedMessage,
) -> Result<Option<u32>, Box<dyn std::error::Error>> {
    if !matches!(
        compute_unit_limit,
        ComputeUnitLimit::Simulated | ComputeUnitLimit::SimulatedWithExtraPercentage(_)
    ) {
        return Ok(None);
    }
    let has_compute_unit_limit = match message {
        VersionedMessage::V1(message) => {
            message.config.compute_unit_limit.is_some()
                && (compute_unit_price.is_some() || message.config.priority_fee.is_some())
        }
        VersionedMessage::Legacy(message) => {
            get_compute_unit_limit_instruction_index(&message.instructions, &message.account_keys)
                .is_some()
        }
        VersionedMessage::V0(message) => {
            get_compute_unit_limit_instruction_index(&message.instructions, &message.account_keys)
                .is_some()
        }
    };
    if !has_compute_unit_limit {
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
    set_compute_unit_limit(message, compute_unit_limit, compute_unit_price);
    Ok(Some(compute_unit_limit))
}

/// Apply a simulated limit, preserving V1's explicit priority fee.
/// Changing the limit can change Legacy/V0 priority fees.
pub(crate) fn set_compute_unit_limit(
    message: &mut VersionedMessage,
    limit: u32,
    compute_unit_price: Option<u64>,
) {
    let (instructions, account_keys) = match message {
        VersionedMessage::Legacy(message) => (&mut message.instructions, &message.account_keys),
        VersionedMessage::V0(message) => (&mut message.instructions, &message.account_keys),
        VersionedMessage::V1(message) => {
            message.config.compute_unit_limit = Some(limit);
            if let Some(price) = compute_unit_price {
                message.config.priority_fee = Some(priority_fee(price, limit));
            }
            return;
        }
    };
    let index = get_compute_unit_limit_instruction_index(instructions, account_keys)
        .expect("simulated message has a compute unit limit instruction");
    instructions[index].data = ComputeBudgetInstruction::set_compute_unit_limit(limit).data;
}

pub struct ComputeUnitConfig {
    pub compute_unit_price: Option<u64>,
    pub compute_unit_limit: ComputeUnitLimit,
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_hash::Hash,
        solana_message::{Message, v0, v1},
    };

    #[test]
    fn simulated_limit_updates_all_message_versions() {
        let payer = Pubkey::new_unique();
        let instructions = [ComputeBudgetInstruction::set_compute_unit_limit(100)];
        let mut legacy = VersionedMessage::Legacy(Message::new(&instructions, Some(&payer)));
        let mut v0 = VersionedMessage::V0(
            v0::Message::try_compile(&payer, &instructions, &[], Hash::default()).unwrap(),
        );
        for message in [&mut legacy, &mut v0] {
            set_compute_unit_limit(message, 40, None);
            let (instructions, account_keys) = match message {
                VersionedMessage::Legacy(message) => (&message.instructions, &message.account_keys),
                VersionedMessage::V0(message) => (&message.instructions, &message.account_keys),
                VersionedMessage::V1(_) => unreachable!(),
            };
            let index = get_compute_unit_limit_instruction_index(instructions, account_keys)
                .expect("compute unit limit instruction");
            assert!(matches!(
                try_from_slice_unchecked(&instructions[index].data),
                Ok(ComputeBudgetInstruction::SetComputeUnitLimit(40))
            ));
        }

        let mut v1 = VersionedMessage::V1(
            v1::Message::try_compile_with_config(
                &payer,
                &[],
                Hash::default(),
                v1::TransactionConfig::empty()
                    .with_compute_unit_limit(100)
                    .with_priority_fee(7),
            )
            .unwrap(),
        );
        set_compute_unit_limit(&mut v1, 40, None);
        let VersionedMessage::V1(message) = &v1 else {
            unreachable!()
        };
        assert_eq!(message.config.priority_fee, Some(7));
        set_compute_unit_limit(&mut v1, 40, Some(1_000_000));
        let VersionedMessage::V1(message) = v1 else {
            unreachable!()
        };
        assert_eq!(message.config.compute_unit_limit, Some(40));
        assert_eq!(message.config.priority_fee, Some(40));
    }
}
