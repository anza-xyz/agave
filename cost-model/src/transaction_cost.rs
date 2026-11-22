// Costs are stored in compute units.
#[derive(Debug)]
pub struct TransactionCost {
    pub signature_cost: u64,
    pub write_lock_cost: u64,
    pub data_bytes_cost: u16,
    pub programs_execution_cost: u64,
    pub loaded_accounts_data_size_cost: u64,
    pub allocated_accounts_data_size: u64,
}

impl TransactionCost {
    pub fn sum(&self) -> u64 {
        self.signature_cost
            .saturating_add(self.write_lock_cost)
            .saturating_add(u64::from(self.data_bytes_cost))
            .saturating_add(self.programs_execution_cost)
            .saturating_add(self.loaded_accounts_data_size_cost)
    }

    pub fn programs_execution_cost(&self) -> u64 {
        self.programs_execution_cost
    }

    pub fn data_bytes_cost(&self) -> u16 {
        self.data_bytes_cost
    }

    pub fn allocated_accounts_data_size(&self) -> u64 {
        self.allocated_accounts_data_size
    }

    pub fn loaded_accounts_data_size_cost(&self) -> u64 {
        self.loaded_accounts_data_size_cost
    }

    pub fn signature_cost(&self) -> u64 {
        self.signature_cost
    }

    pub fn write_lock_cost(&self) -> u64 {
        self.write_lock_cost
    }

    /// Returns the part of the cost that [`CostTracker`] limits.
    ///
    /// The returned value sums all cost components into one compute-unit total and keeps the
    /// allocated account data size. It leaves out the individual components, which are not needed
    /// to check the block limits and each account's limit.
    ///
    /// [`CostTracker`]: crate::cost_tracker::CostTracker
    pub fn tracked_cost(&self) -> TrackedCost {
        TrackedCost {
            cost: self.sum(),
            allocated_accounts_data_size: self.allocated_accounts_data_size,
        }
    }
}

/// Total cost and allocated account data size, a subset of [`TransactionCost`] data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrackedCost {
    /// Sum of all cost components, in compute units.
    pub cost: u64,
    /// Allocated account data bytes.
    pub allocated_accounts_data_size: u64,
}

#[cfg(test)]
mod tests {
    use {
        crate::cost_model::CostModel,
        agave_feature_set::{FeatureSet, bls_pubkey_management_in_vote_account},
        agave_reserved_account_keys::ReservedAccountKeys,
        solana_hash::Hash,
        solana_instruction::{AccountMeta, Instruction},
        solana_keypair::Keypair,
        solana_message::SimpleAddressLoader,
        solana_pubkey::Pubkey,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_transaction::{
            Transaction, sanitized::MessageHash, versioned::VersionedTransaction,
        },
    };

    fn get_example_transaction() -> VersionedTransaction {
        let payer = Keypair::new();
        let instruction = Instruction::new_with_bytes(
            solana_vote_program::id(),
            &[0; 76],
            vec![AccountMeta::new(Pubkey::new_unique(), false)],
        );
        let transaction = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&payer.pubkey()),
            &[&payer],
            Hash::default(),
        );

        VersionedTransaction::from(transaction)
    }

    #[test]
    fn test_vote_program_transaction_cost() {
        agave_logger::setup();

        let transaction = RuntimeTransaction::try_create(
            get_example_transaction(),
            MessageHash::Compute,
            SimpleAddressLoader::Disabled,
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        // Compute expected cost.
        let signature_cost = 720;
        let write_lock_cost = 600;
        let data_bytes_cost = 19;
        let loaded_accounts_data_size_cost = 16384;

        let compute_expected_cost = |programs_execution_cost: u64| {
            signature_cost
                + write_lock_cost
                + data_bytes_cost
                + programs_execution_cost
                + loaded_accounts_data_size_cost
        };

        // SIMD-0387 inactive.
        // Vote program uses builtin cost modeling (3,000 CUs).
        {
            let mut feature_set = FeatureSet::all_enabled();
            feature_set.deactivate(&bls_pubkey_management_in_vote_account::id());

            let expected_cost = compute_expected_cost(3_000);

            let cost = CostModel::calculate_cost(&transaction, &feature_set);
            assert_eq!(expected_cost, cost.sum());
        }

        // SIMD-0387 active.
        // Vote program removed from builtin cost modeling.
        // Uses the default non-builtin cost (200,000 CUs).
        {
            let feature_set = FeatureSet::all_enabled();

            let expected_cost = compute_expected_cost(200_000);

            let cost = CostModel::calculate_cost(&transaction, &feature_set);
            assert_eq!(expected_cost, cost.sum());
        }
    }
}
