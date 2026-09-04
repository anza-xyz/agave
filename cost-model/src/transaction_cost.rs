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
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{block_cost_limits, cost_model::CostModel},
        agave_feature_set::{FeatureSet, bls_pubkey_management_in_vote_account},
        agave_reserved_account_keys::ReservedAccountKeys,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::SimpleAddressLoader,
        solana_runtime_transaction::{
            runtime_transaction::RuntimeTransaction, transaction_meta::TransactionMeta,
        },
        solana_transaction::{sanitized::MessageHash, versioned::VersionedTransaction},
        solana_vote::vote_transaction,
        solana_vote_program::vote_state::TowerSync,
    };

    fn get_example_transaction() -> VersionedTransaction {
        let node_keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let auth_keypair = Keypair::new();
        let transaction = vote_transaction::new_tower_sync_transaction(
            TowerSync::default(),
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &auth_keypair,
            None,
        );

        VersionedTransaction::from(transaction)
    }

    #[test]
    fn test_vote_transaction_cost() {
        agave_logger::setup();

        use {
            crate::block_cost_limits::INSTRUCTION_DATA_BYTES_COST,
            solana_compute_budget::compute_budget_limits::{
                DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT, MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT,
                MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            },
        };

        // Create a sanitized vote transaction.
        let vote_transaction = RuntimeTransaction::try_create(
            get_example_transaction(),
            MessageHash::Compute,
            Some(true),
            SimpleAddressLoader::Disabled,
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        for simd_0387_enabled in [false, true] {
            let mut feature_set = FeatureSet::all_enabled();
            if !simd_0387_enabled {
                feature_set.deactivate(&bls_pubkey_management_in_vote_account::id());
            }

            let signature_cost = 2 * block_cost_limits::SIGNATURE_COST;
            let write_lock_cost = 2 * block_cost_limits::WRITE_LOCK_UNITS;
            let data_bytes_cost =
                vote_transaction.instruction_data_len() / (INSTRUCTION_DATA_BYTES_COST as u16);
            // Estimated execution cost depends on whether the Vote program is
            // tracked in cost modeling as a builtin.
            let programs_execution_cost = if simd_0387_enabled {
                // SIMD-0387: Vote program removed from builtin cost modeling.
                DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64
            } else {
                MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT as u64
            };
            // and it has default loaded_account_data_size
            let loaded_accounts_data_size_cost =
                CostModel::calculate_loaded_accounts_data_size_cost(
                    MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get(),
                    &feature_set,
                );
            let vote_program_cost = TransactionCost {
                signature_cost,
                write_lock_cost,
                data_bytes_cost,
                programs_execution_cost,
                loaded_accounts_data_size_cost,
                allocated_accounts_data_size: 0,
            };
            let expected_cost = vote_program_cost.sum();

            let vote_cost = CostModel::calculate_cost(&vote_transaction, &feature_set);
            assert_eq!(expected_cost, vote_cost.sum());
        }
    }

    #[test]
    fn test_non_vote_transaction_cost() {
        agave_logger::setup();

        // Create a sanitized non-vote transaction.
        let non_vote_transaction = RuntimeTransaction::try_create(
            get_example_transaction(),
            MessageHash::Compute,
            Some(false),
            SimpleAddressLoader::Disabled,
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        // Compute expected cost.
        let signature_cost = 1440;
        let write_lock_cost = 600;
        let data_bytes_cost = 19;
        let loaded_accounts_data_size_cost = 16384;

        let compute_expected_non_vote_cost = |programs_execution_cost: u64| {
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

            let expected_non_vote_cost = compute_expected_non_vote_cost(3_000);

            let non_vote_cost = CostModel::calculate_cost(&non_vote_transaction, &feature_set);
            assert_eq!(expected_non_vote_cost, non_vote_cost.sum());
        }

        // SIMD-0387 active.
        // Vote program removed from builtin cost modeling.
        // Uses the default non-builtin cost (200,000 CUs).
        {
            let feature_set = FeatureSet::all_enabled();

            let expected_non_vote_cost = compute_expected_non_vote_cost(200_000);

            let non_vote_cost = CostModel::calculate_cost(&non_vote_transaction, &feature_set);
            assert_eq!(expected_non_vote_cost, non_vote_cost.sum());
        }
    }
}
