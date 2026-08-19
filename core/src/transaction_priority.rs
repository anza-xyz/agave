use {
    agave_transaction_view::transaction_view::SanitizedTransactionView,
    solana_cost_model::cost_model::CostModel,
    solana_runtime::bank::{Bank, CollectorFeeDetails},
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction,
        sanitize_config::sanitize_config,
        transaction_meta::{TransactionConfiguration, TransactionMeta},
    },
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_transaction::sanitized::MessageHash,
};

/// Block resource limits used to normalize transaction cost and serialized
/// bytes into a single priority score.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TransactionPriorityResourceLimits {
    block_cost: u64,
    block_bytes: u64,
    penalize_alt_lookups: bool,
}

impl TransactionPriorityResourceLimits {
    pub(crate) fn for_bank(bank: &Bank) -> Self {
        let block_cost = bank.read_cost_tracker().unwrap().get_block_limit();
        let block_bytes = bank.max_entry_bytes_per_slot();
        let penalize_alt_lookups = bank.feature_set.snapshot().enable_tx_v1;
        debug_assert!(block_cost > 0);
        debug_assert!(block_bytes > 0);
        Self {
            block_cost,
            block_bytes,
            penalize_alt_lookups,
        }
    }
}

const ALT_LOOKUP_PUBKEY_BYTE_PENALTY: u64 = 32;

fn prioritized_transaction_bytes<Tx: SVMStaticMessage>(
    transaction: &Tx,
    serialized_bytes: u64,
    penalize_alt_lookups: bool,
) -> u64 {
    if !penalize_alt_lookups {
        return serialized_bytes;
    }

    let num_looked_up_pubkeys =
        transaction
            .message_address_table_lookups()
            .fold(0u64, |num_looked_up_pubkeys, lookup| {
                num_looked_up_pubkeys
                    .saturating_add(lookup.writable_indexes.len() as u64)
                    .saturating_add(lookup.readonly_indexes.len() as u64)
            });
    serialized_bytes
        .saturating_add(num_looked_up_pubkeys.saturating_mul(ALT_LOOKUP_PUBKEY_BYTE_PENALTY))
}

/// Calculate transaction priority from its reward and consumption of the two
/// block-wide resources: cost-model units and serialized entry bytes.
///
/// Serialized bytes are converted to cost-model units using the ratio of the
/// block limits. Cross multiplication retains the fractional byte cost instead
/// of rounding it before calculating priority.
/// This affects transaction ordering only; hard byte accounting continues to
/// use the actual serialized transaction size.
fn calculate_priority(
    reward: u64,
    cost: u64,
    prioritized_bytes: u64,
    resource_limits: TransactionPriorityResourceLimits,
) -> u64 {
    const MULTIPLIER: u64 = 1_000_000;

    // This is equivalent to:
    // reward * MULTIPLIER / (cost + 1 + prioritized_bytes * block_cost / block_bytes)
    let denominator = u128::from(cost.saturating_add(1))
        .saturating_mul(u128::from(resource_limits.block_bytes))
        .saturating_add(
            u128::from(prioritized_bytes).saturating_mul(u128::from(resource_limits.block_cost)),
        );
    let priority = u128::from(reward)
        .saturating_mul(u128::from(MULTIPLIER))
        .saturating_mul(u128::from(resource_limits.block_bytes))
        .checked_div(denominator)
        .unwrap_or(0);

    priority.min(u128::from(u64::MAX)) as u64
}

pub(crate) fn calculate_priority_for_transaction<Tx: SVMStaticMessage>(
    transaction: &Tx,
    reward: u64,
    cost: u64,
    serialized_bytes: u64,
    resource_limits: TransactionPriorityResourceLimits,
) -> u64 {
    let prioritized_bytes = prioritized_transaction_bytes(
        transaction,
        serialized_bytes,
        resource_limits.penalize_alt_lookups,
    );
    calculate_priority(reward, cost, prioritized_bytes, resource_limits)
}

/// Calculate priority and cost for a transaction:
///
/// Cost is calculated through the `CostModel`,
/// and priority is calculated through a formula here that attempts to sell
/// blockspace to the highest bidder.
///
/// The priority is calculated as:
/// P = R / (1 + C + B * L_C / L_B)
/// where P is the priority, R is the reward,
/// C is the cost towards the block cost limit, B is the prioritized transaction
/// size, L_C is the block cost limit, and L_B is the block entry-bytes limit.
/// Once txv1 is enabled, B includes a 32-byte penalty for each pubkey loaded
/// through an address lookup table.
///
/// The +1 explicitly avoids division by zero. Giving the normalized resource
/// terms equal weight means that consuming the same fraction of either block
/// limit contributes the same amount to the denominator.
pub(crate) fn calculate_priority_and_cost<Tx: TransactionMeta + SVMStaticMessage>(
    bank: &Bank,
    transaction: &Tx,
    transaction_configuration: &TransactionConfiguration,
    transaction_bytes: u64,
    resource_limits: TransactionPriorityResourceLimits,
) -> (u64, u64) {
    let cost = CostModel::calculate_cost_for_executed_transaction(
        transaction,
        u64::from(transaction_configuration.compute_unit_limit),
        transaction_configuration.loaded_accounts_data_size_limit,
        &bank.feature_set,
    )
    .sum();
    let fee_details = solana_fee::calculate_fee_details(
        transaction,
        bank.fee_structure().lamports_per_signature,
        transaction_configuration.priority_fee_lamports,
        bank.fee_features(),
    );
    let reward = bank
        .calculate_reward_and_burn_fee_details(&CollectorFeeDetails::from(fee_details))
        .get_deposit();

    (
        calculate_priority_for_transaction(
            transaction,
            reward,
            cost,
            transaction_bytes,
            resource_limits,
        ),
        cost,
    )
}

/// Evaluate raw packet bytes against the pf-floor, returning the computed
/// priority.
///
/// Returns `None` if the bytes don't parse as a valid transaction, in which
/// case the caller should leave the packet to downstream stages to reject.
pub(crate) fn calculate_priority_from_bytes(
    bank: &Bank,
    data: &[u8],
    resource_limits: TransactionPriorityResourceLimits,
) -> Option<u64> {
    let view = SanitizedTransactionView::try_new_sanitized(data, &sanitize_config()).ok()?;
    let runtime_tx = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
        view,
        MessageHash::Compute,
        None,
    )
    .ok()?;
    let transaction_configuration = runtime_tx
        .transaction_configuration(&bank.feature_set)
        .ok()?;
    let (priority, _cost) = calculate_priority_and_cost(
        bank,
        &runtime_tx,
        &transaction_configuration,
        data.len() as u64,
        resource_limits,
    );

    Some(priority)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_transaction_view::resolved_transaction_view::ResolvedTransactionView,
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_hash::Hash,
        solana_instruction::{AccountMeta, Instruction},
        solana_keypair::Keypair,
        solana_ledger::genesis_utils::{GenesisConfigInfo, create_genesis_config},
        solana_message::{
            AddressLookupTableAccount, Message, VersionedMessage,
            v0::{self, LoadedAddresses},
        },
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
        wincode::serialize(&VersionedTransaction::from(tx)).unwrap()
    }

    fn make_v0_tx_bytes(mint: &Keypair, recent_blockhash: Hash) -> Vec<u8> {
        let writable = Pubkey::new_unique();
        let readonly = Pubkey::new_unique();
        let instruction = Instruction::new_with_bytes(
            Pubkey::new_unique(),
            &[],
            vec![
                AccountMeta::new(writable, false),
                AccountMeta::new_readonly(readonly, false),
            ],
        );
        let message = v0::Message::try_compile(
            &mint.pubkey(),
            &[instruction],
            &[AddressLookupTableAccount {
                key: Pubkey::new_unique(),
                addresses: vec![writable, readonly],
            }],
            recent_blockhash,
        )
        .unwrap();
        let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[mint]).unwrap();
        wincode::serialize(&tx).unwrap()
    }

    fn priority_from(bank: &Bank, bytes: &[u8]) -> u64 {
        calculate_priority_from_bytes(
            bank,
            bytes,
            TransactionPriorityResourceLimits::for_bank(bank),
        )
        .unwrap()
    }

    #[test]
    fn priority_from_bytes_returns_none_for_garbage() {
        let (bank, _) = test_bank();
        let resource_limits = TransactionPriorityResourceLimits::for_bank(&bank);
        assert!(calculate_priority_from_bytes(&bank, &[], resource_limits).is_none());
        assert!(calculate_priority_from_bytes(&bank, &[0u8; 32], resource_limits).is_none());
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

        let view =
            SanitizedTransactionView::try_new_sanitized(&bytes[..], &sanitize_config()).unwrap();
        let runtime_tx = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
            view,
            MessageHash::Compute,
            None,
        )
        .unwrap();
        let transaction_configuration = runtime_tx
            .transaction_configuration(&bank.feature_set)
            .unwrap();
        let (from_typed, _cost) = calculate_priority_and_cost(
            &bank,
            &runtime_tx,
            &transaction_configuration,
            bytes.len() as u64,
            TransactionPriorityResourceLimits::for_bank(&bank),
        );

        assert_eq!(from_bytes, from_typed);
    }

    #[test]
    fn zero_bytes_matches_previous_priority_formula() {
        let reward = 5_000;
        let cost = 200_000;
        let resource_limits = TransactionPriorityResourceLimits {
            block_cost: 100_000_000,
            block_bytes: 20 * 1024 * 1024,
            penalize_alt_lookups: false,
        };

        assert_eq!(
            calculate_priority(reward, cost, 0, resource_limits),
            reward
                .saturating_mul(1_000_000)
                .saturating_div(cost.saturating_add(1))
        );
    }

    #[test]
    fn smaller_transaction_has_higher_priority() {
        let resource_limits = TransactionPriorityResourceLimits {
            block_cost: 100_000_000,
            block_bytes: 20 * 1024 * 1024,
            penalize_alt_lookups: false,
        };
        let smaller = calculate_priority(5_000, 20_000, 500, resource_limits);
        let larger = calculate_priority(5_000, 20_000, 1_000, resource_limits);

        assert!(smaller > larger);
    }

    #[test]
    fn equal_resource_fractions_have_equal_weight() {
        let resource_limits = TransactionPriorityResourceLimits {
            block_cost: 1_000,
            block_bytes: 100,
            penalize_alt_lookups: false,
        };

        // cost + 1 consumes 1% of the cost limit, and one byte consumes 1%
        // of the byte limit, so adding the byte halves the priority.
        let cost_only = calculate_priority(1_000, 9, 0, resource_limits);
        let cost_and_bytes = calculate_priority(1_000, 9, 1, resource_limits);
        assert_eq!(cost_only, cost_and_bytes * 2);
    }

    #[test]
    fn alt_lookup_byte_penalty_requires_tx_v1_feature() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair: mint,
            ..
        } = create_genesis_config(u64::MAX);
        let mut bank = Bank::new_for_tests(&genesis_config);
        let bytes = make_v0_tx_bytes(&mint, bank.last_blockhash());
        let view =
            SanitizedTransactionView::try_new_sanitized(&bytes[..], &sanitize_config()).unwrap();
        let runtime_tx = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
            view,
            MessageHash::Compute,
            None,
        )
        .unwrap();
        let serialized_bytes = bytes.len() as u64;

        bank.deactivate_feature(&agave_feature_set::enable_tx_v1::id());
        let resource_limits = TransactionPriorityResourceLimits::for_bank(&bank);
        assert_eq!(
            prioritized_transaction_bytes(
                &runtime_tx,
                serialized_bytes,
                resource_limits.penalize_alt_lookups,
            ),
            serialized_bytes,
        );

        bank.activate_feature(&agave_feature_set::enable_tx_v1::id());
        let resource_limits = TransactionPriorityResourceLimits::for_bank(&bank);
        assert_eq!(
            prioritized_transaction_bytes(
                &runtime_tx,
                serialized_bytes,
                resource_limits.penalize_alt_lookups,
            ),
            serialized_bytes + 2 * ALT_LOOKUP_PUBKEY_BYTE_PENALTY,
        );

        let resolved_tx = RuntimeTransaction::<ResolvedTransactionView<_>>::try_new(
            runtime_tx,
            Some(LoadedAddresses {
                writable: vec![Pubkey::new_unique()],
                readonly: vec![Pubkey::new_unique()],
            }),
            bank.get_reserved_account_keys(),
        )
        .unwrap();
        assert_eq!(
            prioritized_transaction_bytes(
                &resolved_tx,
                serialized_bytes,
                resource_limits.penalize_alt_lookups,
            ),
            serialized_bytes + 2 * ALT_LOOKUP_PUBKEY_BYTE_PENALTY,
        );
    }
}
