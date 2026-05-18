use {
    agave_transaction_view::transaction_view::SanitizedTransactionView,
    solana_cost_model::cost_model::CostModel,
    solana_fee::FeeFeatures,
    solana_runtime::bank::{Bank, CollectorFeeDetails},
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction,
        transaction_meta::{TransactionConfiguration, TransactionMeta},
    },
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_transaction::sanitized::MessageHash,
};

/// Calculate priority and cost for a transaction:
///
/// Cost is calculated through the `CostModel`,
/// and priority is calculated through a formula here that attempts to sell
/// blockspace to the highest bidder.
///
/// The priority is calculated as:
/// P = R / (1 + C)
/// where P is the priority, R is the reward,
/// and C is the cost towards block-limits.
///
/// Current minimum costs are on the order of several hundred,
/// so the denominator is effectively C, and the +1 is simply
/// to avoid any division by zero due to a bug - these costs
/// are calculated by the cost-model and are not direct
/// from user input. They should never be zero.
/// Any difference in the prioritization is negligible for
/// the current transaction costs.
pub(crate) fn calculate_priority_and_cost<Tx: TransactionMeta + SVMStaticMessage>(
    bank: &Bank,
    transaction: &Tx,
    transaction_configuration: &TransactionConfiguration,
) -> (u64, u64) {
    let cost = CostModel::calculate_cost(transaction, &bank.feature_set).sum();
    let fee_details = solana_fee::calculate_fee_details(
        transaction,
        bank.fee_structure().lamports_per_signature,
        transaction_configuration.priority_fee_lamports,
        FeeFeatures::from(bank.feature_set.as_ref()),
    );
    let reward = bank
        .calculate_reward_and_burn_fee_details(&CollectorFeeDetails::from(fee_details))
        .get_deposit();

    // We need a multiplier here to avoid rounding down too aggressively.
    // For many transactions, the cost will be greater than the fees in terms of raw lamports.
    // For the purposes of calculating prioritization, we multiply the fees by a large number so that
    // the cost is a small fraction.
    // An offset of 1 is used in the denominator to explicitly avoid division by zero.
    const MULTIPLIER: u64 = 1_000_000;
    (
        reward
            .saturating_mul(MULTIPLIER)
            .saturating_div(cost.saturating_add(1)),
        cost,
    )
}

/// Evaluate raw packet bytes against the pf-floor, returning the computed
/// priority.
///
/// Returns `None` if the bytes don't parse as a valid transaction, in which
/// case the caller should leave the packet to downstream stages to reject.
pub(crate) fn calculate_priority_from_bytes(bank: &Bank, data: &[u8]) -> Option<u64> {
    let enable_instruction_accounts_limit = bank.feature_set.snapshot().limit_instruction_accounts;
    let view = SanitizedTransactionView::try_new_sanitized(data, enable_instruction_accounts_limit)
        .ok()?;
    let runtime_tx = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
        view,
        MessageHash::Compute,
        None,
    )
    .ok()?;
    let transaction_configuration = runtime_tx
        .transaction_configuration(&bank.feature_set)
        .ok()?;
    let (priority, _cost) =
        calculate_priority_and_cost(bank, &runtime_tx, &transaction_configuration);

    Some(priority)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::genesis_utils::{GenesisConfigInfo, create_genesis_config},
        solana_message::Message,
        solana_pubkey::Pubkey,
        solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::{Transaction, versioned::VersionedTransaction},
        std::sync::Arc,
    };

    fn test_bank_with_lamports_per_signature(lamports_per_signature: u64) -> (Arc<Bank>, Keypair) {
        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(u64::MAX);
        if lamports_per_signature > 0 {
            genesis_config.fee_rate_governor =
                solana_fee_calculator::FeeRateGovernor::new(lamports_per_signature, 0);
        }
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        (bank, mint_keypair)
    }

    fn test_bank() -> (Arc<Bank>, Keypair) {
        test_bank_with_lamports_per_signature(0)
    }

    fn make_tx_bytes(mint: &Keypair, recent_blockhash: Hash, compute_unit_price: u64) -> Vec<u8> {
        let to = Pubkey::new_unique();
        let transfer = system_instruction::transfer(&mint.pubkey(), &to, 1);
        let prioritization = ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price);
        let message = Message::new(&[transfer, prioritization], Some(&mint.pubkey()));
        let tx = Transaction::new(&[mint], message, recent_blockhash);
        bincode::serialize(&VersionedTransaction::from(tx)).unwrap()
    }

    fn priority_from(bank: &Bank, bytes: &[u8]) -> u64 {
        calculate_priority_from_bytes(bank, bytes).unwrap()
    }

    #[test]
    fn priority_from_bytes_returns_none_for_garbage() {
        let (bank, _) = test_bank();
        assert!(calculate_priority_from_bytes(&bank, &[]).is_none());
        assert!(calculate_priority_from_bytes(&bank, &[0u8; 32]).is_none());
    }

    #[test]
    fn priority_is_zero_when_base_and_priority_fees_are_zero() {
        // Test bank has lamports_per_signature = 0, so base fee is 0.
        // With compute_unit_price = 0, priority fee is also 0 → reward 0 → priority 0.
        let (bank, mint) = test_bank();
        assert_eq!(bank.fee_structure().lamports_per_signature, 0);
        let bytes = make_tx_bytes(&mint, bank.last_blockhash(), 0);
        assert_eq!(priority_from(&bank, &bytes), 0);
    }

    #[test]
    fn higher_compute_unit_price_yields_higher_priority() {
        // Need non-zero base fee, otherwise the reward short-circuits to 0
        // and all priorities collapse regardless of compute_unit_price.
        let (bank, mint) = test_bank_with_lamports_per_signature(5_000);
        let low = priority_from(&bank, &make_tx_bytes(&mint, bank.last_blockhash(), 1));
        let high = priority_from(
            &bank,
            &make_tx_bytes(&mint, bank.last_blockhash(), 1_000_000),
        );
        assert!(high > low, "expected high {high} > low {low}");
    }

    #[test]
    fn floor_priority_from_bytes_matches_typed_path() {
        // The bytes-path and the typed-path must agree on the same packet,
        // since the scheduler-side queue priority is computed via the typed
        // path and the sigverify-side floor check via the bytes path.
        let (bank, mint) = test_bank();
        let bytes = make_tx_bytes(&mint, bank.last_blockhash(), 100);

        let from_bytes = priority_from(&bank, &bytes);

        let view = SanitizedTransactionView::try_new_sanitized(
            &bytes[..],
            bank.feature_set.snapshot().limit_instruction_accounts,
        )
        .unwrap();
        let runtime_tx = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
            view,
            MessageHash::Compute,
            None,
        )
        .unwrap();
        let transaction_configuration = runtime_tx
            .transaction_configuration(&bank.feature_set)
            .unwrap();
        let (from_typed, _cost) =
            calculate_priority_and_cost(&bank, &runtime_tx, &transaction_configuration);

        assert_eq!(from_bytes, from_typed);
    }
}
