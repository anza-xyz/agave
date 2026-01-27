use {
    super::error::CoreBpfMigrationError,
    crate::bank::Bank,
    solana_account::{AccountSharedData, ReadableAccount},
    solana_builtins::core_bpf_migration::CoreBpfMigrationTargetType,
    solana_loader_v3_interface::get_program_data_address,
    solana_pubkey::Pubkey,
    solana_sdk_ids::{
        native_loader::ID as NATIVE_LOADER_ID, system_program::ID as SYSTEM_PROGRAM_ID,
    },
};

/// The account details of a built-in program to be migrated to Core BPF.
#[derive(Debug)]
pub(crate) struct TargetBuiltin {
    pub program_address: Pubkey,
    pub program_account: AccountSharedData,
    pub program_data_address: Pubkey,
    pub program_data_account_lamports: u64,
}

impl TargetBuiltin {
    /// Collects the details of a built-in program and verifies it is properly
    /// configured
    pub(crate) fn new_checked(
        bank: &Bank,
        program_address: &Pubkey,
        migration_target: &CoreBpfMigrationTargetType,
        allow_prefunded: bool,
    ) -> Result<Self, CoreBpfMigrationError> {
        let program_account = match migration_target {
            CoreBpfMigrationTargetType::Builtin => {
                // The program account should exist.
                let program_account = bank
                    .get_account_with_fixed_root(program_address)
                    .ok_or(CoreBpfMigrationError::AccountNotFound(*program_address))?;

                // The program account should be owned by the native loader.
                if program_account.owner() != &NATIVE_LOADER_ID {
                    return Err(CoreBpfMigrationError::IncorrectOwner(*program_address));
                }

                program_account
            }
            CoreBpfMigrationTargetType::Stateless => {
                // The program account should _not_ exist.
                if bank.get_account_with_fixed_root(program_address).is_some() {
                    return Err(CoreBpfMigrationError::AccountExists(*program_address));
                }

                AccountSharedData::default()
            }
        };

        let program_data_address = get_program_data_address(program_address);

        let program_data_account_lamports = if allow_prefunded {
            // The program data account should not exist, but a system account with funded
            // lamports is acceptable.
            if let Some(account) = bank.get_account_with_fixed_root(&program_data_address) {
                if account.owner() != &SYSTEM_PROGRAM_ID {
                    return Err(CoreBpfMigrationError::ProgramHasDataAccount(
                        *program_address,
                    ));
                }
                account.lamports()
            } else {
                0
            }
        } else {
            // The program data account should not exist and have zero lamports.
            if bank
                .get_account_with_fixed_root(&program_data_address)
                .is_some()
            {
                return Err(CoreBpfMigrationError::ProgramHasDataAccount(
                    *program_address,
                ));
            }

            0
        };

        Ok(Self {
            program_address: *program_address,
            program_account,
            program_data_address,
            program_data_account_lamports,
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::bank::tests::create_simple_test_bank, agave_feature_set as feature_set,
        assert_matches::assert_matches, solana_account::Account,
        solana_feature_gate_interface as feature,
        solana_loader_v3_interface::state::UpgradeableLoaderState,
        solana_sdk_ids::bpf_loader_upgradeable::ID as BPF_LOADER_UPGRADEABLE_ID,
        test_case::test_matrix,
    };

    fn store_account<T: serde::Serialize>(
        bank: &Bank,
        address: &Pubkey,
        data: &T,
        executable: bool,
        owner: &Pubkey,
    ) {
        let data = bincode::serialize(data).unwrap();
        let data_len = data.len();
        let lamports = bank.get_minimum_balance_for_rent_exemption(data_len);
        let account = AccountSharedData::from(Account {
            data,
            executable,
            lamports,
            owner: *owner,
            ..Account::default()
        });
        bank.store_account_and_update_capitalization(address, &account);
    }

    #[test_matrix(
        [
            (solana_sdk_ids::bpf_loader::id(), None),
            (solana_sdk_ids::bpf_loader_deprecated::id(), None),
            (solana_sdk_ids::bpf_loader_upgradeable::id(), None),
            (solana_compute_budget_interface::id(), None),
            (solana_system_interface::program::id(), None),
            (solana_vote_interface::program::id(), None),
            (
                solana_sdk_ids::loader_v4::id(),
                Some(feature_set::enable_loader_v4::id())
            ),
            (
                solana_sdk_ids::zk_token_proof_program::id(),
                Some(feature_set::zk_token_sdk_enabled::id())
            ),
            (
                solana_sdk_ids::zk_elgamal_proof_program::id(),
                Some(feature_set::zk_elgamal_proof_program_enabled::id())
            ),
        ],
        [false, true])
    ]
    fn test_target_program_builtin(
        program_address_and_activation_feature: (Pubkey, Option<Pubkey>),
        allow_prefund: bool,
    ) {
        let (program_address, activation_feature) = program_address_and_activation_feature;
        let migration_target = CoreBpfMigrationTargetType::Builtin;
        let mut bank = create_simple_test_bank(0);

        if let Some(feature_id) = activation_feature {
            // Activate the feature to enable the built-in program
            bank.store_account(
                &feature_id,
                &feature::create_account(
                    &feature::Feature { activated_at: None },
                    bank.get_minimum_balance_for_rent_exemption(feature::Feature::size_of()),
                ),
            );
            bank.compute_and_apply_new_feature_activations();
        }

        let program_account = bank.get_account_with_fixed_root(&program_address).unwrap();
        let program_data_address = get_program_data_address(&program_address);

        // Success
        let target_builtin =
            TargetBuiltin::new_checked(&bank, &program_address, &migration_target, allow_prefund)
                .unwrap();
        assert_eq!(target_builtin.program_address, program_address);
        assert_eq!(target_builtin.program_account, program_account);
        assert_eq!(target_builtin.program_data_address, program_data_address);

        // Fail if the program account is not owned by the native loader
        store_account(
            &bank,
            &program_address,
            &String::from("some built-in program"),
            true,
            &Pubkey::new_unique(), // Not the native loader
        );
        assert_matches!(
            TargetBuiltin::new_checked(&bank, &program_address, &migration_target, allow_prefund)
                .unwrap_err(),
            CoreBpfMigrationError::IncorrectOwner(..)
        );

        // Fail if the program data account exists
        store_account(
            &bank,
            &program_address,
            &program_account.data(),
            program_account.executable(),
            program_account.owner(),
        );
        store_account(
            &bank,
            &program_data_address,
            &UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(Pubkey::new_unique()),
            },
            false,
            &BPF_LOADER_UPGRADEABLE_ID,
        );
        assert_matches!(
            TargetBuiltin::new_checked(&bank, &program_address, &migration_target, allow_prefund)
                .unwrap_err(),
            CoreBpfMigrationError::ProgramHasDataAccount(..)
        );

        // Allow some lamports in the program data account owned by the system program
        store_account(
            &bank,
            &program_data_address,
            &vec![0u8; 100],
            false,
            &SYSTEM_PROGRAM_ID,
        );

        if allow_prefund {
            // Succeed if prefund is allowed
            assert!(
                TargetBuiltin::new_checked(
                    &bank,
                    &program_address,
                    &migration_target,
                    allow_prefund,
                )
                .is_ok()
            );
        } else {
            // Fail if prefund is not allowed
            assert_matches!(
                TargetBuiltin::new_checked(
                    &bank,
                    &program_address,
                    &migration_target,
                    allow_prefund
                )
                .unwrap_err(),
                CoreBpfMigrationError::ProgramHasDataAccount(..)
            );
        }

        // Clean up the program data account lamports for zero-lamport test
        bank.store_account_and_update_capitalization(
            &program_data_address,
            &AccountSharedData::default(),
        );

        // Success
        let target_builtin =
            TargetBuiltin::new_checked(&bank, &program_address, &migration_target, allow_prefund)
                .unwrap();

        assert_eq!(target_builtin.program_address, program_address);
        assert_eq!(target_builtin.program_data_address, program_data_address);

        // Fail if the program account does not exist
        bank.store_account_and_update_capitalization(
            &program_address,
            &AccountSharedData::default(),
        );
        assert_matches!(
            TargetBuiltin::new_checked(&bank, &program_address, &migration_target, allow_prefund)
                .unwrap_err(),
            CoreBpfMigrationError::AccountNotFound(..)
        );
    }

    #[test_matrix(
        [solana_feature_gate_interface::id(), solana_sdk_ids::native_loader::id()],
        [false, true]
    )]
    fn test_target_program_stateless_builtin(program_address: Pubkey, allow_prefund: bool) {
        let migration_target = CoreBpfMigrationTargetType::Stateless;
        let bank = create_simple_test_bank(0);

        let program_account = AccountSharedData::default();
        let program_data_address = get_program_data_address(&program_address);

        // Success
        let target_builtin =
            TargetBuiltin::new_checked(&bank, &program_address, &migration_target, allow_prefund)
                .unwrap();
        assert_eq!(target_builtin.program_address, program_address);
        assert_eq!(target_builtin.program_account, program_account);
        assert_eq!(target_builtin.program_data_address, program_data_address);

        // Fail if the program data account exists
        store_account(
            &bank,
            &program_data_address,
            &UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(Pubkey::new_unique()),
            },
            false,
            &BPF_LOADER_UPGRADEABLE_ID,
        );
        assert_matches!(
            TargetBuiltin::new_checked(&bank, &program_address, &migration_target, allow_prefund)
                .unwrap_err(),
            CoreBpfMigrationError::ProgramHasDataAccount(..)
        );

        // Fail if the program account exists
        store_account(
            &bank,
            &program_address,
            &String::from("some built-in program"),
            true,
            &NATIVE_LOADER_ID,
        );
        assert_matches!(
            TargetBuiltin::new_checked(&bank, &program_address, &migration_target, allow_prefund)
                .unwrap_err(),
            CoreBpfMigrationError::AccountExists(..)
        );
    }
}
