pub(crate) mod error;
mod source_buffer;
mod target_bpf_v2;
mod target_builtin;
mod target_core_bpf;

use {
    crate::bank::{builtins::core_bpf_migration::target_bpf_v2::TargetBpfV2, Bank},
    error::CoreBpfMigrationError,
    num_traits::{CheckedAdd, CheckedSub},
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_builtins::core_bpf_migration::CoreBpfMigrationConfig,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_hash::Hash,
    solana_instruction::error::InstructionError,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_program_runtime::{
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::ProgramCacheForTxBatch,
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::bpf_loader_upgradeable,
    solana_svm_callback::InvokeContextCallback,
    solana_transaction_context::TransactionContext,
    source_buffer::SourceBuffer,
    std::{cmp::Ordering, sync::atomic::Ordering::Relaxed},
    target_builtin::TargetBuiltin,
    target_core_bpf::TargetCoreBpf,
};

fn checked_add<T: CheckedAdd>(a: T, b: T) -> Result<T, CoreBpfMigrationError> {
    a.checked_add(&b)
        .ok_or(CoreBpfMigrationError::ArithmeticOverflow)
}

fn checked_sub<T: CheckedSub>(a: T, b: T) -> Result<T, CoreBpfMigrationError> {
    a.checked_sub(&b)
        .ok_or(CoreBpfMigrationError::ArithmeticOverflow)
}

impl Bank {
    /// Create an `AccountSharedData` with data initialized to
    /// `UpgradeableLoaderState::Program` populated with the target's new data
    /// account address.
    fn new_target_program_account(
        &self,
        program_data_address: &Pubkey,
    ) -> Result<AccountSharedData, CoreBpfMigrationError> {
        let state = UpgradeableLoaderState::Program {
            programdata_address: *program_data_address,
        };
        let lamports =
            self.get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program());
        let mut account =
            AccountSharedData::new_data(lamports, &state, &bpf_loader_upgradeable::id())?;
        account.set_executable(true);
        Ok(account)
    }

    /// Create an `AccountSharedData` with data initialized to
    /// `UpgradeableLoaderState::ProgramData` populated with the current slot, as
    /// well as the source program data account's upgrade authority and ELF.
    ///
    /// This function accepts a provided upgrade authority address, which comes
    /// from the migration configuration. If the provided upgrade authority
    /// address is different from the source buffer account's upgrade authority
    /// address, the migration will fail. If the provided upgrade authority
    /// address is `None`, the migration will ignore the source buffer account's
    /// upgrade authority and set the new program data account's upgrade
    /// authority to `None`.
    fn new_target_program_data_account(
        &self,
        source: &SourceBuffer,
        upgrade_authority_address: Option<Pubkey>,
    ) -> Result<AccountSharedData, CoreBpfMigrationError> {
        let buffer_metadata_size = UpgradeableLoaderState::size_of_buffer_metadata();
        if let UpgradeableLoaderState::Buffer {
            authority_address: buffer_authority,
        } = bincode::deserialize(&source.buffer_account.data()[..buffer_metadata_size])?
        {
            if let Some(provided_authority) = upgrade_authority_address {
                if upgrade_authority_address != buffer_authority {
                    return Err(CoreBpfMigrationError::UpgradeAuthorityMismatch(
                        provided_authority,
                        buffer_authority,
                    ));
                }
            }

            let elf = &source.buffer_account.data()[buffer_metadata_size..];

            let programdata_metadata_size = UpgradeableLoaderState::size_of_programdata_metadata();
            let space = programdata_metadata_size + elf.len();
            let lamports = self.get_minimum_balance_for_rent_exemption(space);
            let owner = &bpf_loader_upgradeable::id();

            let programdata_metadata = UpgradeableLoaderState::ProgramData {
                slot: self.slot,
                upgrade_authority_address,
            };

            let mut account = AccountSharedData::new_data_with_space(
                lamports,
                &programdata_metadata,
                space,
                owner,
            )?;
            account.data_as_mut_slice()[programdata_metadata_size..].copy_from_slice(elf);

            Ok(account)
        } else {
            Err(CoreBpfMigrationError::InvalidBufferAccount(
                source.buffer_address,
            ))
        }
    }

    /// In order to properly update a newly migrated or upgraded Core BPF
    /// program in the program cache, the runtime must directly invoke the BPF
    /// Upgradeable Loader's deployment functionality for validating the ELF
    /// bytes against the current environment, as well as updating the program
    /// cache.
    ///
    /// Invoking the loader's `direct_deploy_program` function will update the
    /// program cache in the currently executing context, but the runtime must
    /// also propagate those updates to the currently active cache.
    fn directly_invoke_loader_v3_deploy(
        &self,
        program_id: &Pubkey,
        programdata: &[u8],
    ) -> Result<(), InstructionError> {
        let data_len = programdata.len();
        let progradata_metadata_size = UpgradeableLoaderState::size_of_programdata_metadata();
        let elf = &programdata[progradata_metadata_size..];
        // Set up the two `LoadedProgramsForTxBatch` instances, as if
        // processing a new transaction batch.
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::new(self.slot);
        let program_runtime_environments = self
            .transaction_processor
            .get_environments_for_epoch(self.epoch);

        // Configure a dummy `InvokeContext` from the runtime's current
        // environment, as well as the two `ProgramCacheForTxBatch`
        // instances configured above, then invoke the loader.
        {
            let compute_budget = self
                .compute_budget()
                .unwrap_or(ComputeBudget::new_with_defaults(
                    /* simd_0268_active */ false, /* simd_0339_active */ false,
                ));
            let mut sysvar_cache = SysvarCache::default();
            sysvar_cache.fill_missing_entries(|pubkey, set_sysvar| {
                if let Some(account) = self.get_account(pubkey) {
                    set_sysvar(account.data());
                }
            });

            let mut dummy_transaction_context = TransactionContext::new(
                vec![],
                self.rent_collector.rent.clone(),
                compute_budget.max_instruction_stack_depth,
                compute_budget.max_instruction_trace_length,
            );

            struct MockCallback {}
            impl InvokeContextCallback for MockCallback {}
            let feature_set = self.feature_set.runtime_features();
            let mut dummy_invoke_context = InvokeContext::new(
                &mut dummy_transaction_context,
                &mut program_cache_for_tx_batch,
                EnvironmentConfig::new(
                    Hash::default(),
                    0,
                    &MockCallback {},
                    &feature_set,
                    &program_runtime_environments,
                    &program_runtime_environments,
                    &sysvar_cache,
                ),
                None,
                compute_budget.to_budget(),
                compute_budget.to_cost(),
            );

            let load_program_metrics = solana_bpf_loader_program::deploy_program(
                dummy_invoke_context.get_log_collector(),
                dummy_invoke_context.program_cache_for_tx_batch,
                program_runtime_environments.program_runtime_v1.clone(),
                program_id,
                &bpf_loader_upgradeable::id(),
                // The size of the program cache entry is the size of the program account
                // + size of the program data account.
                UpgradeableLoaderState::size_of_program().saturating_add(data_len),
                elf,
                self.slot,
            )?;
            load_program_metrics.submit_datapoint(&mut dummy_invoke_context.timings);
        }

        // Update the program cache by merging with `programs_modified`, which
        // should have been updated by the deploy function.
        self.transaction_processor
            .global_program_cache
            .write()
            .unwrap()
            .merge(
                &self.transaction_processor.environments,
                &program_cache_for_tx_batch.drain_modified_entries(),
            );

        Ok(())
    }

    pub(crate) fn migrate_builtin_to_core_bpf(
        &mut self,
        builtin_program_id: &Pubkey,
        config: &CoreBpfMigrationConfig,
    ) -> Result<(), CoreBpfMigrationError> {
        datapoint_info!(config.datapoint_name, ("slot", self.slot, i64));

        let target =
            TargetBuiltin::new_checked(self, builtin_program_id, &config.migration_target)?;
        let source = if let Some(expected_hash) = config.verified_build_hash {
            SourceBuffer::new_checked_with_verified_build_hash(
                self,
                &config.source_buffer_address,
                expected_hash,
            )?
        } else {
            SourceBuffer::new_checked(self, &config.source_buffer_address)?
        };

        // Attempt serialization first before modifying the bank.
        let new_target_program_account =
            self.new_target_program_account(&target.program_data_address)?;
        let new_target_program_data_account =
            self.new_target_program_data_account(&source, config.upgrade_authority_address)?;

        // Gather old and new account data sizes, for updating the bank's
        // accounts data size delta off-chain.
        // The old data size is the total size of all original accounts
        // involved.
        // The new data size is the total size of all the new program accounts.
        let old_data_size = checked_add(
            target.program_account.data().len(),
            source.buffer_account.data().len(),
        )?;
        let new_data_size = checked_add(
            new_target_program_account.data().len(),
            new_target_program_data_account.data().len(),
        )?;

        // Deploy the new target Core BPF program.
        // This step will validate the program ELF against the current runtime
        // environment, as well as update the program cache.
        self.directly_invoke_loader_v3_deploy(
            &target.program_address,
            new_target_program_data_account.data(),
        )?;

        // Calculate the lamports to burn.
        // The target program account will be replaced, so burn its lamports.
        // The target program data account might have lamports if it existed,
        // so burn its lamports if any.
        // The source buffer account will be cleared, so burn its lamports.
        // The two new program accounts will need to be funded.
        let lamports_to_burn = checked_add(
            target.program_account.lamports(),
            source.buffer_account.lamports(),
        )
        .and_then(|v| checked_add(v, target.program_data_account_lamports))?;
        let lamports_to_fund = checked_add(
            new_target_program_account.lamports(),
            new_target_program_data_account.lamports(),
        )?;
        self.update_captalization(lamports_to_burn, lamports_to_fund)?;

        // Store the new program accounts and clear the source buffer account.
        self.store_account(&target.program_address, &new_target_program_account);
        self.store_account(
            &target.program_data_address,
            &new_target_program_data_account,
        );
        self.store_account(&source.buffer_address, &AccountSharedData::default());

        // Remove the built-in program from the bank's list of built-ins.
        self.transaction_processor
            .builtin_program_ids
            .write()
            .unwrap()
            .remove(&target.program_address);

        // Update the account data size delta.
        self.calculate_and_update_accounts_data_size_delta_off_chain(old_data_size, new_data_size);

        Ok(())
    }

    /// Upgrade a Core BPF program.
    /// To use this function, add a feature-gated callsite to bank's
    /// `apply_new_feature_activations` function, similar to below.
    ///
    /// ```ignore
    /// if new_feature_activations.contains(&agave_feature_set::test_upgrade_program::id()) {
    ///     self.upgrade_core_bpf_program(
    ///        &core_bpf_program_address,
    ///        &source_buffer_address,
    ///        "test_upgrade_core_bpf_program",
    ///     );
    /// }
    /// ```
    #[allow(dead_code)] // Only used when an upgrade is configured.
    pub(crate) fn upgrade_core_bpf_program(
        &mut self,
        core_bpf_program_address: &Pubkey,
        source_buffer_address: &Pubkey,
        datapoint_name: &'static str,
    ) -> Result<(), CoreBpfMigrationError> {
        datapoint_info!(datapoint_name, ("slot", self.slot, i64));

        let target = TargetCoreBpf::new_checked(self, core_bpf_program_address)?;
        let source = SourceBuffer::new_checked(self, source_buffer_address)?;

        // Attempt serialization first before modifying the bank.
        let new_target_program_data_account =
            self.new_target_program_data_account(&source, target.upgrade_authority_address)?;

        // Gather old and new account data sizes, for updating the bank's
        // accounts data size delta off-chain.
        // Since the program account is not replaced, only the program data
        // account and the source buffer account are involved.
        let old_data_size = checked_add(
            target.program_data_account.data().len(),
            source.buffer_account.data().len(),
        )?;
        let new_data_size = new_target_program_data_account.data().len();

        // Deploy the new target Core BPF program.
        // This step will validate the program ELF against the current runtime
        // environment, as well as update the program cache.
        self.directly_invoke_loader_v3_deploy(
            &target.program_address,
            new_target_program_data_account.data(),
        )?;

        // Calculate the lamports to burn.
        // Since the program account is not replaced, only the program data
        // account and the source buffer account are involved.
        // The target program data account will be replaced, so burn its
        // lamports.
        // The source buffer account will be cleared, so burn its lamports.
        // The new program data account will need to be funded.
        let lamports_to_burn = checked_add(
            target.program_data_account.lamports(),
            source.buffer_account.lamports(),
        )?;
        let lamports_to_fund = new_target_program_data_account.lamports();
        self.update_captalization(lamports_to_burn, lamports_to_fund)?;

        // Store the new program data account and clear the source buffer account.
        self.store_account(
            &target.program_data_address,
            &new_target_program_data_account,
        );
        self.store_account(&source.buffer_address, &AccountSharedData::default());

        // Update the account data size delta.
        self.calculate_and_update_accounts_data_size_delta_off_chain(old_data_size, new_data_size);

        Ok(())
    }

    /// Upgrade a Loader v2 BPF program to a Loader v3 BPF program.
    ///
    /// To use this function, add a feature-gated callsite to bank's
    /// `apply_feature_activations` function, similar to below.
    ///
    /// ```ignore
    /// if new_feature_activations.contains(&agave_feature_set::test_upgrade_program::id()) {
    ///     self.upgrade_loader_v2_program_with_loader_v3_program(
    ///        &bpf_loader_v2_program_address,
    ///        &source_buffer_address,
    ///        "test_upgrade_loader_v2_program_with_loader_v3_program",
    ///     );
    /// }
    /// ```
    /// The `source_buffer_address` must point to a Loader v3 buffer account
    /// (state equal to [`UpgradeableLoaderState::Buffer`]).
    #[allow(dead_code)] // Only used when an upgrade is configured.
    pub(crate) fn upgrade_loader_v2_program_with_loader_v3_program(
        &mut self,
        loader_v2_bpf_program_address: &Pubkey,
        source_buffer_address: &Pubkey,
        datapoint_name: &'static str,
    ) -> Result<(), CoreBpfMigrationError> {
        datapoint_info!(datapoint_name, ("slot", self.slot, i64));

        let target = TargetBpfV2::new_checked(self, loader_v2_bpf_program_address)?;
        let source = SourceBuffer::new_checked(self, source_buffer_address)?;

        // Attempt serialization first before modifying the bank.
        let new_target_program_account =
            self.new_target_program_account(&target.program_data_address)?;
        // Loader v2 programs do not have an upgrade authority, so pass `None` when
        // creating the new program data account.
        let new_target_program_data_account =
            self.new_target_program_data_account(&source, None)?;

        // Gather old and new account data sizes, for updating the bank's
        // accounts data size delta off-chain.
        // The old data size is the total size of all original accounts
        // involved.
        // The new data size is the total size of all the new program accounts.
        let old_data_size = checked_add(
            target.program_account.data().len(),
            source.buffer_account.data().len(),
        )?;
        let new_data_size = checked_add(
            new_target_program_account.data().len(),
            new_target_program_data_account.data().len(),
        )?;

        // Deploy the new Loader v3 program.
        // This step will validate the program ELF against the current runtime
        // environment, as well as update the program cache.
        self.directly_invoke_loader_v3_deploy(
            &target.program_address,
            new_target_program_data_account.data(),
        )?;

        // Calculate the lamports to burn.
        // The target program account will be replaced, so burn its lamports.
        // The target program data account might have lamports if it existed,
        // so burn its lamports if any.
        // The source buffer account will be cleared, so burn its lamports.
        // The two new program accounts will need to be funded.
        let lamports_to_burn = checked_add(
            target.program_account.lamports(),
            source.buffer_account.lamports(),
        )
        .and_then(|v| checked_add(v, target.program_data_account_lamports))?;
        let lamports_to_fund = checked_add(
            new_target_program_account.lamports(),
            new_target_program_data_account.lamports(),
        )?;
        self.update_captalization(lamports_to_burn, lamports_to_fund)?;

        // Store the new program accounts and clear the source buffer account.
        self.store_account(&target.program_address, &new_target_program_account);
        self.store_account(
            &target.program_data_address,
            &new_target_program_data_account,
        );
        self.store_account(&source.buffer_address, &AccountSharedData::default());

        // Update the account data size delta.
        self.calculate_and_update_accounts_data_size_delta_off_chain(old_data_size, new_data_size);

        Ok(())
    }

    fn update_captalization(
        &mut self,
        lamports_to_burn: u64,
        lamports_to_fund: u64,
    ) -> Result<(), CoreBpfMigrationError> {
        match lamports_to_burn.cmp(&lamports_to_fund) {
            Ordering::Greater => {
                self.capitalization
                    .fetch_sub(checked_sub(lamports_to_burn, lamports_to_fund)?, Relaxed);
            }
            Ordering::Less => {
                self.capitalization
                    .fetch_add(checked_sub(lamports_to_fund, lamports_to_burn)?, Relaxed);
            }
            Ordering::Equal => (),
        };

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        crate::{
            bank::{
                test_utils::goto_end_of_slot,
                tests::{create_genesis_config, create_simple_test_bank},
                Bank,
            },
            runtime_config::RuntimeConfig,
            snapshot_bank_utils::{bank_from_snapshot_archives, bank_to_full_snapshot_archive},
            snapshot_utils::create_tmp_accounts_dir_for_tests,
        },
        agave_feature_set::FeatureSet,
        agave_snapshots::snapshot_config::SnapshotConfig,
        assert_matches::assert_matches,
        solana_account::{
            state_traits::StateMut, AccountSharedData, ReadableAccount, WritableAccount,
        },
        solana_accounts_db::accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING,
        solana_builtins::{
            core_bpf_migration::{CoreBpfMigrationConfig, CoreBpfMigrationTargetType},
            prototype::{BuiltinPrototype, StatelessBuiltinPrototype},
            BUILTINS,
        },
        solana_clock::Slot,
        solana_epoch_schedule::EpochSchedule,
        solana_feature_gate_interface::{self as feature, Feature},
        solana_instruction::{AccountMeta, Instruction},
        solana_keypair::Keypair,
        solana_loader_v3_interface::{get_program_data_address, state::UpgradeableLoaderState},
        solana_message::Message,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_program_runtime::loaded_programs::{ProgramCacheEntry, ProgramCacheEntryType},
        solana_pubkey::Pubkey,
        solana_sdk_ids::{bpf_loader, bpf_loader_upgradeable, native_loader, system_program},
        solana_signer::Signer,
        solana_transaction::Transaction,
        std::{fs::File, io::Read, sync::Arc},
        test_case::test_case,
    };

    fn test_elf() -> Vec<u8> {
        let mut elf = Vec::new();
        File::open("../programs/bpf_loader/test_elfs/out/noop_aligned.so")
            .unwrap()
            .read_to_end(&mut elf)
            .unwrap();
        elf
    }

    pub(crate) struct TestContext {
        target_program_address: Pubkey,
        source_buffer_address: Pubkey,
        upgrade_authority_address: Option<Pubkey>,
        elf: Vec<u8>,
    }
    impl TestContext {
        // Initialize some test values and set up the source buffer account in
        // the bank.
        pub(crate) fn new(
            bank: &Bank,
            target_program_address: &Pubkey,
            source_buffer_address: &Pubkey,
            upgrade_authority_address: Option<Pubkey>,
        ) -> Self {
            let elf = test_elf();

            let source_buffer_account = {
                // BPF Loader always writes ELF bytes after
                // `UpgradeableLoaderState::size_of_buffer_metadata()`.
                let buffer_metadata_size = UpgradeableLoaderState::size_of_buffer_metadata();
                let space = buffer_metadata_size + elf.len();
                let lamports = bank.get_minimum_balance_for_rent_exemption(space);
                let owner = &bpf_loader_upgradeable::id();

                let buffer_metadata = UpgradeableLoaderState::Buffer {
                    authority_address: upgrade_authority_address,
                };

                let mut account = AccountSharedData::new_data_with_space(
                    lamports,
                    &buffer_metadata,
                    space,
                    owner,
                )
                .unwrap();
                account.data_as_mut_slice()[buffer_metadata_size..].copy_from_slice(&elf);
                account
            };

            bank.store_account_and_update_capitalization(
                source_buffer_address,
                &source_buffer_account,
            );

            Self {
                target_program_address: *target_program_address,
                source_buffer_address: *source_buffer_address,
                upgrade_authority_address,
                elf,
            }
        }

        // Given a bank, calculate the expected capitalization and accounts data
        // size delta off-chain after a migration, using the values stored in
        // the test context.
        pub(crate) fn calculate_post_migration_capitalization_and_accounts_data_size_delta_off_chain(
            &self,
            bank: &Bank,
        ) -> (u64, i64) {
            let builtin_account = bank
                .get_account(&self.target_program_address)
                .unwrap_or_default();
            let source_buffer_account = bank.get_account(&self.source_buffer_address).unwrap();
            let resulting_program_data_len = UpgradeableLoaderState::size_of_program();
            let resulting_programdata_data_len =
                UpgradeableLoaderState::size_of_programdata_metadata() + self.elf.len();
            let expected_post_migration_capitalization = bank.capitalization()
                - builtin_account.lamports()
                - source_buffer_account.lamports()
                + bank.get_minimum_balance_for_rent_exemption(resulting_program_data_len)
                + bank.get_minimum_balance_for_rent_exemption(resulting_programdata_data_len);
            let expected_post_migration_accounts_data_size_delta_off_chain =
                bank.accounts_data_size_delta_off_chain.load(Relaxed)
                    + resulting_program_data_len as i64
                    + resulting_programdata_data_len as i64
                    - builtin_account.data().len() as i64
                    - source_buffer_account.data().len() as i64;
            (
                expected_post_migration_capitalization,
                expected_post_migration_accounts_data_size_delta_off_chain,
            )
        }

        // Given a bank, calculate the expected capitalization and accounts data
        // size delta off-chain after an upgrade, using the values stored in
        // the test context.
        fn calculate_post_upgrade_capitalization_and_accounts_data_size_delta_off_chain(
            &self,
            bank: &Bank,
        ) -> (u64, i64) {
            let program_data_account = bank
                .get_account(&get_program_data_address(&self.target_program_address))
                .unwrap_or_default();
            let source_buffer_account = bank.get_account(&self.source_buffer_address).unwrap();
            let resulting_programdata_data_len =
                UpgradeableLoaderState::size_of_programdata_metadata() + self.elf.len();
            let expected_post_migration_capitalization = bank.capitalization()
                - program_data_account.lamports()
                - source_buffer_account.lamports()
                + bank.get_minimum_balance_for_rent_exemption(resulting_programdata_data_len);
            let expected_post_migration_accounts_data_size_delta_off_chain =
                bank.accounts_data_size_delta_off_chain.load(Relaxed)
                    + resulting_programdata_data_len as i64
                    - program_data_account.data().len() as i64
                    - source_buffer_account.data().len() as i64;
            (
                expected_post_migration_capitalization,
                expected_post_migration_accounts_data_size_delta_off_chain,
            )
        }

        // Evaluate the account state of the target and source.
        // After either a migration or upgrade:
        // * The target program is a BPF upgradeable program with a pointer to
        //   its program data address.
        // * The source buffer account is cleared.
        // * The bank's builtin IDs do not contain the target program address.
        // * The cache contains the target program, and the entry is updated.
        pub(crate) fn run_program_checks(&self, bank: &Bank, migration_or_upgrade_slot: Slot) {
            // Verify the source buffer account has been cleared.
            assert!(bank.get_account(&self.source_buffer_address).is_none());

            let program_account = bank.get_account(&self.target_program_address).unwrap();
            let program_data_address = get_program_data_address(&self.target_program_address);

            // Program account is owned by the upgradeable loader.
            assert_eq!(program_account.owner(), &bpf_loader_upgradeable::id());

            // Program account is executable.
            assert!(program_account.executable());

            // Program account has the correct state, with a pointer to its program
            // data address.
            let program_account_state: UpgradeableLoaderState = program_account.state().unwrap();
            assert_eq!(
                program_account_state,
                UpgradeableLoaderState::Program {
                    programdata_address: program_data_address
                }
            );

            let program_data_account = bank.get_account(&program_data_address).unwrap();

            // Program data account is owned by the upgradeable loader.
            assert_eq!(program_data_account.owner(), &bpf_loader_upgradeable::id());

            // Program data account has the correct state.
            // It should have the same update authority and ELF as the source
            // buffer account.
            // The slot should be the slot it was migrated at.
            let programdata_metadata_size = UpgradeableLoaderState::size_of_programdata_metadata();
            let program_data_account_state_metadata: UpgradeableLoaderState =
                bincode::deserialize(&program_data_account.data()[..programdata_metadata_size])
                    .unwrap();
            assert_eq!(
                program_data_account_state_metadata,
                UpgradeableLoaderState::ProgramData {
                    slot: migration_or_upgrade_slot,
                    upgrade_authority_address: self.upgrade_authority_address // Preserved
                },
            );
            assert_eq!(
                &program_data_account.data()[programdata_metadata_size..],
                &self.elf,
            );

            // The bank's builtins should not contain the target program
            // address.
            assert!(!bank
                .transaction_processor
                .builtin_program_ids
                .read()
                .unwrap()
                .contains(&self.target_program_address));

            // The cache should contain the target program.
            let program_cache = bank
                .transaction_processor
                .global_program_cache
                .read()
                .unwrap();
            let entries = program_cache.get_flattened_entries(true, true);
            let target_entry = entries
                .iter()
                .find(|(program_id, _)| program_id == &self.target_program_address)
                .map(|(_, entry)| entry)
                .unwrap();

            // The target program entry should be updated.
            assert_eq!(
                target_entry.account_size,
                program_account.data().len() + program_data_account.data().len()
            );
            assert_eq!(target_entry.deployment_slot, migration_or_upgrade_slot);
            assert_eq!(target_entry.effective_slot, migration_or_upgrade_slot + 1);

            // The target program entry should be a BPF program.
            assert_matches!(target_entry.program, ProgramCacheEntryType::Loaded(..));
        }
    }

    #[test_case(Some(Pubkey::new_unique()); "with_upgrade_authority")]
    #[test_case(None; "without_upgrade_authority")]
    fn test_migrate_builtin(upgrade_authority_address: Option<Pubkey>) {
        let mut bank = create_simple_test_bank(0);

        let builtin_id = Pubkey::new_unique();
        let source_buffer_address = Pubkey::new_unique();

        // This will be checked by `TargetBuiltinProgram::new_checked`, but set
        // up the mock builtin and ensure it exists as configured.
        let builtin_account = {
            let builtin_name = String::from("test_builtin");
            let account =
                AccountSharedData::new_data(1, &builtin_name, &native_loader::id()).unwrap();
            bank.store_account_and_update_capitalization(&builtin_id, &account);
            bank.add_builtin(
                builtin_id,
                builtin_name.as_str(),
                ProgramCacheEntry::new_builtin(
                    0,
                    builtin_name.len(),
                    |_invoke_context, _param0, _param1, _param2, _param3, _param4| {},
                ),
            );
            account
        };
        assert_eq!(&bank.get_account(&builtin_id).unwrap(), &builtin_account);

        let test_context = TestContext::new(
            &bank,
            &builtin_id,
            &source_buffer_address,
            upgrade_authority_address,
        );
        let TestContext {
            target_program_address: builtin_id,
            source_buffer_address,
            ..
        } = test_context;

        let (
            expected_post_migration_capitalization,
            expected_post_migration_accounts_data_size_delta_off_chain,
        ) = test_context
            .calculate_post_migration_capitalization_and_accounts_data_size_delta_off_chain(&bank);

        let core_bpf_migration_config = CoreBpfMigrationConfig {
            source_buffer_address,
            upgrade_authority_address,
            feature_id: Pubkey::new_unique(),
            migration_target: CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "test_migrate_builtin",
        };

        // Perform the migration.
        let migration_slot = bank.slot();
        bank.migrate_builtin_to_core_bpf(&builtin_id, &core_bpf_migration_config)
            .unwrap();

        // Run the post-migration program checks.
        test_context.run_program_checks(&bank, migration_slot);

        // Check the bank's capitalization.
        assert_eq!(
            bank.capitalization(),
            expected_post_migration_capitalization
        );

        // Check the bank's accounts data size delta off-chain.
        assert_eq!(
            bank.accounts_data_size_delta_off_chain.load(Relaxed),
            expected_post_migration_accounts_data_size_delta_off_chain
        );
    }

    #[test_case(Some(Pubkey::new_unique()); "with_upgrade_authority")]
    #[test_case(None; "without_upgrade_authority")]
    fn test_migrate_stateless_builtin(upgrade_authority_address: Option<Pubkey>) {
        let mut bank = create_simple_test_bank(0);

        let builtin_id = Pubkey::new_unique();
        let source_buffer_address = Pubkey::new_unique();

        let test_context = TestContext::new(
            &bank,
            &builtin_id,
            &source_buffer_address,
            upgrade_authority_address,
        );
        let TestContext {
            target_program_address: builtin_id,
            source_buffer_address,
            ..
        } = test_context;

        // This will be checked by `TargetBuiltinProgram::new_checked`, but
        // assert the stateless builtin account doesn't exist.
        assert!(bank.get_account(&builtin_id).is_none());

        let (
            expected_post_migration_capitalization,
            expected_post_migration_accounts_data_size_delta_off_chain,
        ) = test_context
            .calculate_post_migration_capitalization_and_accounts_data_size_delta_off_chain(&bank);

        let expected_hash = {
            let data = test_elf();
            let end_offset = data.iter().rposition(|&x| x != 0).map_or(0, |i| i + 1);
            solana_sha256_hasher::hash(&data[..end_offset])
        };
        let core_bpf_migration_config = CoreBpfMigrationConfig {
            source_buffer_address,
            upgrade_authority_address,
            feature_id: Pubkey::new_unique(),
            verified_build_hash: Some(expected_hash),
            migration_target: CoreBpfMigrationTargetType::Stateless,
            datapoint_name: "test_migrate_stateless_builtin",
        };

        // Perform the migration.
        let migration_slot = bank.slot();
        bank.migrate_builtin_to_core_bpf(&builtin_id, &core_bpf_migration_config)
            .unwrap();

        // Run the post-migration program checks.
        test_context.run_program_checks(&bank, migration_slot);

        // Check the bank's capitalization.
        assert_eq!(
            bank.capitalization(),
            expected_post_migration_capitalization
        );

        // Check the bank's accounts data size delta off-chain.
        assert_eq!(
            bank.accounts_data_size_delta_off_chain.load(Relaxed),
            expected_post_migration_accounts_data_size_delta_off_chain
        );
    }

    #[test]
    fn test_migrate_fail_authority_mismatch() {
        let mut bank = create_simple_test_bank(0);

        let builtin_id = Pubkey::new_unique();
        let source_buffer_address = Pubkey::new_unique();

        let upgrade_authority_address = Some(Pubkey::new_unique());

        {
            let builtin_name = String::from("test_builtin");
            let account =
                AccountSharedData::new_data(1, &builtin_name, &native_loader::id()).unwrap();
            bank.store_account_and_update_capitalization(&builtin_id, &account);
            bank.add_builtin(
                builtin_id,
                builtin_name.as_str(),
                ProgramCacheEntry::new_builtin(
                    0,
                    builtin_name.len(),
                    |_invoke_context, _param0, _param1, _param2, _param3, _param4| {},
                ),
            );
            account
        };

        let test_context = TestContext::new(
            &bank,
            &builtin_id,
            &source_buffer_address,
            upgrade_authority_address,
        );
        let TestContext {
            target_program_address: builtin_id,
            source_buffer_address,
            ..
        } = test_context;

        let core_bpf_migration_config = CoreBpfMigrationConfig {
            source_buffer_address,
            upgrade_authority_address: Some(Pubkey::new_unique()), // Mismatch.
            feature_id: Pubkey::new_unique(),
            migration_target: CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "test_migrate_builtin",
        };

        assert_matches!(
            bank.migrate_builtin_to_core_bpf(&builtin_id, &core_bpf_migration_config)
                .unwrap_err(),
            CoreBpfMigrationError::UpgradeAuthorityMismatch(_, _)
        )
    }

    #[test]
    fn test_migrate_fail_verified_build_mismatch() {
        let mut bank = create_simple_test_bank(0);

        let builtin_id = Pubkey::new_unique();
        let source_buffer_address = Pubkey::new_unique();

        let upgrade_authority_address = Some(Pubkey::new_unique());

        {
            let builtin_name = String::from("test_builtin");
            let account =
                AccountSharedData::new_data(1, &builtin_name, &native_loader::id()).unwrap();
            bank.store_account_and_update_capitalization(&builtin_id, &account);
            bank.add_builtin(
                builtin_id,
                builtin_name.as_str(),
                ProgramCacheEntry::new_builtin(
                    0,
                    builtin_name.len(),
                    |_invoke_context, _param0, _param1, _param2, _param3, _param4| {},
                ),
            );
            account
        };

        let test_context = TestContext::new(
            &bank,
            &builtin_id,
            &source_buffer_address,
            upgrade_authority_address,
        );
        let TestContext {
            target_program_address: builtin_id,
            source_buffer_address,
            ..
        } = test_context;

        let core_bpf_migration_config = CoreBpfMigrationConfig {
            source_buffer_address,
            upgrade_authority_address: None,
            feature_id: Pubkey::new_unique(),
            migration_target: CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: Some(Hash::default()),
            datapoint_name: "test_migrate_builtin",
        };

        assert_matches!(
            bank.migrate_builtin_to_core_bpf(&builtin_id, &core_bpf_migration_config)
                .unwrap_err(),
            CoreBpfMigrationError::BuildHashMismatch(_, _)
        )
    }

    #[test]
    fn test_migrate_none_authority_with_some_buffer_authority() {
        let mut bank = create_simple_test_bank(0);

        let builtin_id = Pubkey::new_unique();
        let source_buffer_address = Pubkey::new_unique();

        let upgrade_authority_address = Some(Pubkey::new_unique());

        {
            let builtin_name = String::from("test_builtin");
            let account =
                AccountSharedData::new_data(1, &builtin_name, &native_loader::id()).unwrap();
            bank.store_account_and_update_capitalization(&builtin_id, &account);
            bank.add_builtin(
                builtin_id,
                builtin_name.as_str(),
                ProgramCacheEntry::new_builtin(
                    0,
                    builtin_name.len(),
                    |_invoke_context, _param0, _param1, _param2, _param3, _param4| {},
                ),
            );
            account
        };

        // Set up the source buffer with a valid authority, but the migration
        // config will define the upgrade authority to be `None`.
        {
            let elf = test_elf();
            let buffer_metadata_size = UpgradeableLoaderState::size_of_buffer_metadata();
            let space = buffer_metadata_size + elf.len();
            let lamports = bank.get_minimum_balance_for_rent_exemption(space);
            let owner = &bpf_loader_upgradeable::id();

            let buffer_metadata = UpgradeableLoaderState::Buffer {
                authority_address: upgrade_authority_address,
            };

            let mut account =
                AccountSharedData::new_data_with_space(lamports, &buffer_metadata, space, owner)
                    .unwrap();
            account.data_as_mut_slice()[buffer_metadata_size..].copy_from_slice(&elf);

            bank.store_account_and_update_capitalization(&source_buffer_address, &account);
        }

        let core_bpf_migration_config = CoreBpfMigrationConfig {
            source_buffer_address,
            upgrade_authority_address: None, // None.
            feature_id: Pubkey::new_unique(),
            migration_target: CoreBpfMigrationTargetType::Builtin,
            verified_build_hash: None,
            datapoint_name: "test_migrate_builtin",
        };

        bank.migrate_builtin_to_core_bpf(&builtin_id, &core_bpf_migration_config)
            .unwrap();

        let program_data_address = get_program_data_address(&builtin_id);
        let program_data_account = bank.get_account(&program_data_address).unwrap();
        let program_data_account_state: UpgradeableLoaderState =
            program_data_account.state().unwrap();
        assert_eq!(
            program_data_account_state,
            UpgradeableLoaderState::ProgramData {
                upgrade_authority_address: None,
                slot: bank.slot(),
            },
        );
    }

    fn set_up_test_core_bpf_program(
        bank: &mut Bank,
        program_address: &Pubkey,
        upgrade_authority_address: Option<Pubkey>,
    ) {
        // This will be checked by `TargetCoreBpf::new_checked`, but set
        // up the mock Core BPF program and ensure it exists as configured.
        let programdata_address = get_program_data_address(program_address);
        let program_account = {
            let data = bincode::serialize(&UpgradeableLoaderState::Program {
                programdata_address,
            })
            .unwrap();
            let space = data.len();
            let lamports = bank.get_minimum_balance_for_rent_exemption(space);
            let owner = &bpf_loader_upgradeable::id();

            let mut account = AccountSharedData::new(lamports, space, owner);
            account.set_executable(true);
            account.data_as_mut_slice().copy_from_slice(&data);
            bank.store_account_and_update_capitalization(program_address, &account);
            account
        };
        let program_data_account = {
            let elf = [4u8; 20]; // Mock ELF to start.
            let programdata_metadata_size = UpgradeableLoaderState::size_of_programdata_metadata();
            let space = programdata_metadata_size + elf.len();
            let lamports = bank.get_minimum_balance_for_rent_exemption(space);
            let owner = &bpf_loader_upgradeable::id();

            let programdata_metadata = UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address,
            };

            let mut account = AccountSharedData::new_data_with_space(
                lamports,
                &programdata_metadata,
                space,
                owner,
            )
            .unwrap();
            account.data_as_mut_slice()[programdata_metadata_size..].copy_from_slice(&elf);
            bank.store_account_and_update_capitalization(&programdata_address, &account);
            account
        };
        assert_eq!(
            &bank.get_account(program_address).unwrap(),
            &program_account
        );
        assert_eq!(
            &bank.get_account(&programdata_address).unwrap(),
            &program_data_account
        );
    }

    #[test_case(Some(Pubkey::new_unique()); "with_upgrade_authority")]
    #[test_case(None; "without_upgrade_authority")]
    fn test_upgrade_core_bpf_program(upgrade_authority_address: Option<Pubkey>) {
        let mut bank = create_simple_test_bank(0);

        let core_bpf_program_address = Pubkey::new_unique();
        let source_buffer_address = Pubkey::new_unique();

        set_up_test_core_bpf_program(
            &mut bank,
            &core_bpf_program_address,
            upgrade_authority_address,
        );

        let test_context = TestContext::new(
            &bank,
            &core_bpf_program_address,
            &source_buffer_address,
            upgrade_authority_address,
        );
        let TestContext {
            source_buffer_address,
            ..
        } = test_context;

        let (
            expected_post_upgrade_capitalization,
            expected_post_upgrade_accounts_data_size_delta_off_chain,
        ) = test_context
            .calculate_post_upgrade_capitalization_and_accounts_data_size_delta_off_chain(&bank);

        // Perform the upgrade.
        let upgrade_slot = bank.slot();
        bank.upgrade_core_bpf_program(
            &core_bpf_program_address,
            &source_buffer_address,
            "test_upgrade_core_bpf_program",
        )
        .unwrap();

        // Run the post-upgrade program checks.
        test_context.run_program_checks(&bank, upgrade_slot);

        // Check the bank's capitalization.
        assert_eq!(bank.capitalization(), expected_post_upgrade_capitalization);

        // Check the bank's accounts data size delta off-chain.
        assert_eq!(
            bank.accounts_data_size_delta_off_chain.load(Relaxed),
            expected_post_upgrade_accounts_data_size_delta_off_chain
        );
    }

    #[test]
    fn test_upgrade_fail_authority_mismatch() {
        let mut bank = create_simple_test_bank(0);

        let program_address = Pubkey::new_unique();
        let source_buffer_address = Pubkey::new_unique();

        let upgrade_authority_address = Some(Pubkey::new_unique());

        set_up_test_core_bpf_program(&mut bank, &program_address, upgrade_authority_address);

        let _test_context = TestContext::new(
            &bank,
            &program_address,
            &source_buffer_address,
            Some(Pubkey::new_unique()), // Mismatch.
        );

        assert_matches!(
            bank.upgrade_core_bpf_program(
                &program_address,
                &source_buffer_address,
                "test_upgrade_core_bpf_program"
            )
            .unwrap_err(),
            CoreBpfMigrationError::UpgradeAuthorityMismatch(_, _)
        )
    }

    #[test]
    fn test_upgrade_none_authority_with_some_buffer_authority() {
        let mut bank = create_simple_test_bank(0);

        let program_address = Pubkey::new_unique();
        let source_buffer_address = Pubkey::new_unique();

        set_up_test_core_bpf_program(&mut bank, &program_address, None);

        let _test_context = TestContext::new(
            &bank,
            &program_address,
            &source_buffer_address,
            Some(Pubkey::new_unique()), // Not `None`.
        );

        bank.upgrade_core_bpf_program(
            &program_address,
            &source_buffer_address,
            "test_upgrade_core_bpf_program",
        )
        .unwrap();

        let program_data_address = get_program_data_address(&program_address);
        let program_data_account = bank.get_account(&program_data_address).unwrap();
        let program_data_account_state: UpgradeableLoaderState =
            program_data_account.state().unwrap();
        assert_eq!(
            program_data_account_state,
            UpgradeableLoaderState::ProgramData {
                upgrade_authority_address: None,
                slot: bank.slot(),
            },
        );
    }

    // CPI mockup to test CPI to newly migrated programs.
    mod cpi_mockup {
        use {
            solana_instruction::Instruction, solana_program_runtime::declare_process_instruction,
        };

        declare_process_instruction!(Entrypoint, 0, |invoke_context| {
            let transaction_context = &invoke_context.transaction_context;
            let instruction_context = transaction_context.get_current_instruction_context()?;

            let target_program_id = instruction_context.get_key_of_instruction_account(0)?;

            let instruction = Instruction::new_with_bytes(*target_program_id, &[], Vec::new());

            invoke_context.native_invoke(instruction, &[])
        });
    }

    enum TestPrototype<'a> {
        Builtin(&'a BuiltinPrototype),
        #[allow(unused)]
        // We aren't migrating any stateless builtins right now. Uncomment if needed.
        Stateless(&'a StatelessBuiltinPrototype),
    }
    impl<'a> TestPrototype<'a> {
        fn deconstruct(&'a self) -> (&'a Pubkey, &'a CoreBpfMigrationConfig) {
            match self {
                Self::Builtin(prototype) => (
                    &prototype.program_id,
                    prototype.core_bpf_migration_config.as_ref().unwrap(),
                ),
                Self::Stateless(prototype) => (
                    &prototype.program_id,
                    prototype.core_bpf_migration_config.as_ref().unwrap(),
                ),
            }
        }
    }

    /// Activate a feature and run checks on the test context.
    fn activate_feature_and_run_checks(
        root_bank: Bank,
        test_context: &TestContext,
        program_id: &Pubkey,
        feature_id: &Pubkey,
        source_buffer_address: &Pubkey,
        mint_keypair: &Keypair,
        slots_per_epoch: u64,
        cpi_program_id: &Pubkey,
    ) {
        let (bank, bank_forks) = root_bank.wrap_with_bank_forks_for_tests();

        // Advance to the next epoch without activating the feature.
        let mut first_slot_in_next_epoch = slots_per_epoch + 1;
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank,
            &Pubkey::default(),
            first_slot_in_next_epoch,
        );

        // Assert the feature was not activated and the program was not
        // migrated.
        assert!(!bank.feature_set.is_active(feature_id));
        assert!(bank.get_account(source_buffer_address).is_some());

        // Store the account to activate the feature.
        bank.store_account_and_update_capitalization(
            feature_id,
            &feature::create_account(&Feature::default(), 42),
        );

        // Advance the bank to cross the epoch boundary and activate the
        // feature.
        goto_end_of_slot(bank.clone());
        first_slot_in_next_epoch += slots_per_epoch;
        let migration_slot = first_slot_in_next_epoch;
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank,
            &Pubkey::default(),
            first_slot_in_next_epoch,
        );

        // Run the post-migration program checks.
        assert!(bank.feature_set.is_active(feature_id));
        test_context.run_program_checks(&bank, migration_slot);

        // Advance one slot so that the new BPF loader v3 program becomes
        // effective in the program cache.
        goto_end_of_slot(bank.clone());
        let next_slot = bank.slot() + 1;
        let bank =
            Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), next_slot);

        // Successfully invoke the new BPF loader v3 program.
        bank.process_transaction(&Transaction::new(
            &vec![&mint_keypair],
            Message::new(
                &[Instruction::new_with_bytes(*program_id, &[], Vec::new())],
                Some(&mint_keypair.pubkey()),
            ),
            bank.last_blockhash(),
        ))
        .unwrap();

        // Successfully invoke the new BPF loader v3 program via CPI.
        bank.process_transaction(&Transaction::new(
            &vec![&mint_keypair],
            Message::new(
                &[Instruction::new_with_bytes(
                    *cpi_program_id,
                    &[],
                    vec![AccountMeta::new_readonly(*program_id, false)],
                )],
                Some(&mint_keypair.pubkey()),
            ),
            bank.last_blockhash(),
        ))
        .unwrap();

        // Simulate crossing another epoch boundary for a new bank.
        goto_end_of_slot(bank.clone());
        first_slot_in_next_epoch += slots_per_epoch;
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank,
            &Pubkey::default(),
            first_slot_in_next_epoch,
        );

        // Run the post-migration program checks again.
        assert!(bank.feature_set.is_active(feature_id));
        test_context.run_program_checks(&bank, migration_slot);

        // Again, successfully invoke the new BPF loader v3 program.
        bank.process_transaction(&Transaction::new(
            &vec![&mint_keypair],
            Message::new(
                &[Instruction::new_with_bytes(*program_id, &[], Vec::new())],
                Some(&mint_keypair.pubkey()),
            ),
            bank.last_blockhash(),
        ))
        .unwrap();

        // Again, successfully invoke the new BPF loader v3 program via CPI.
        bank.process_transaction(&Transaction::new(
            &vec![&mint_keypair],
            Message::new(
                &[Instruction::new_with_bytes(
                    *cpi_program_id,
                    &[],
                    vec![AccountMeta::new_readonly(*program_id, false)],
                )],
                Some(&mint_keypair.pubkey()),
            ),
            bank.last_blockhash(),
        ))
        .unwrap();
    }

    // This test can't be used to the `compute_budget` program, unless a valid
    // `compute_budget` program is provided as the replacement (source).
    // See program_runtime::compute_budget_processor::process_compute_budget_instructions`.`
    // It also can't test the `bpf_loader_upgradeable` program, as it's used in
    // the SVM's loader to invoke programs.
    // See `solana_svm::account_loader::load_transaction_accounts`.
    #[test_case(TestPrototype::Builtin(&BUILTINS[0]); "system")]
    #[test_case(TestPrototype::Builtin(&BUILTINS[1]); "vote")]
    #[test_case(TestPrototype::Builtin(&BUILTINS[2]); "bpf_loader_deprecated")]
    #[test_case(TestPrototype::Builtin(&BUILTINS[3]); "bpf_loader")]
    fn test_migrate_builtin_e2e(prototype: TestPrototype) {
        let (mut genesis_config, mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let slots_per_epoch = 32;
        genesis_config.epoch_schedule =
            EpochSchedule::custom(slots_per_epoch, slots_per_epoch, false);

        let mut root_bank = Bank::new_for_tests(&genesis_config);

        // Set up the CPI mockup to test CPI'ing to the migrated program.
        let cpi_program_id = Pubkey::new_unique();
        let cpi_program_name = "mock_cpi_program";
        root_bank.add_builtin(
            cpi_program_id,
            cpi_program_name,
            ProgramCacheEntry::new_builtin(0, cpi_program_name.len(), cpi_mockup::Entrypoint::vm),
        );

        let (builtin_id, config) = prototype.deconstruct();
        let feature_id = &config.feature_id;
        let source_buffer_address = &config.source_buffer_address;
        let upgrade_authority_address = config.upgrade_authority_address;

        // Add the feature to the bank's inactive feature set.
        // Note this will add the feature ID if it doesn't exist.
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(feature_id);
        root_bank.feature_set = Arc::new(feature_set);

        // Initialize the source buffer account.
        let test_context = TestContext::new(
            &root_bank,
            builtin_id,
            source_buffer_address,
            upgrade_authority_address,
        );

        // Activate the feature and run the migration checks.
        activate_feature_and_run_checks(
            root_bank,
            &test_context,
            builtin_id,
            feature_id,
            source_buffer_address,
            &mint_keypair,
            slots_per_epoch,
            &cpi_program_id,
        );
    }

    // Simulate a failure to migrate the program.
    // Here we want to see that the bank handles the failure gracefully and
    // advances to the next epoch without issue.
    #[test]
    fn test_migrate_builtin_e2e_failure() {
        let (genesis_config, _mint_keypair) = create_genesis_config(0);
        let mut root_bank = Bank::new_for_tests(&genesis_config);

        let test_prototype = TestPrototype::Builtin(&BUILTINS[0]); // System program
        let (builtin_id, config) = test_prototype.deconstruct();
        let feature_id = &config.feature_id;
        let source_buffer_address = &config.source_buffer_address;
        let upgrade_authority_address = Some(Pubkey::new_unique());

        // Add the feature to the bank's inactive feature set.
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.inactive_mut().insert(*feature_id);
        root_bank.feature_set = Arc::new(feature_set);

        // Initialize the source buffer account.
        let _test_context = TestContext::new(
            &root_bank,
            builtin_id,
            source_buffer_address,
            upgrade_authority_address,
        );

        let (bank, bank_forks) = root_bank.wrap_with_bank_forks_for_tests();

        // Intentionally nuke the source buffer account to force the migration
        // to fail.
        bank.store_account_and_update_capitalization(
            source_buffer_address,
            &AccountSharedData::default(),
        );

        // Activate the feature.
        bank.store_account_and_update_capitalization(
            feature_id,
            &feature::create_account(&Feature::default(), 42),
        );

        // Advance the bank to cross the epoch boundary and activate the
        // feature.
        goto_end_of_slot(bank.clone());
        let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 33);

        // Assert the feature _was_ activated but the program was not migrated.
        assert!(bank.feature_set.is_active(feature_id));
        assert!(bank
            .transaction_processor
            .builtin_program_ids
            .read()
            .unwrap()
            .contains(builtin_id));
        assert_eq!(
            bank.get_account(builtin_id).unwrap().owner(),
            &native_loader::id()
        );

        // Simulate crossing an epoch boundary again.
        goto_end_of_slot(bank.clone());
        let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 96);

        // Again, assert the feature is still active and the program still was
        // not migrated.
        assert!(bank.feature_set.is_active(feature_id));
        assert!(bank
            .transaction_processor
            .builtin_program_ids
            .read()
            .unwrap()
            .contains(builtin_id));
        assert_eq!(
            bank.get_account(builtin_id).unwrap().owner(),
            &native_loader::id()
        );
    }

    // Simulate creating a bank from a snapshot after a migration feature was
    // activated, but the migration failed.
    // Here we want to see that the bank recognizes the failed migration and
    // adds the original builtin to the new bank.
    #[test]
    fn test_migrate_builtin_e2e_init_after_failed_migration() {
        let (genesis_config, _mint_keypair) = create_genesis_config(0);

        let test_prototype = TestPrototype::Builtin(&BUILTINS[0]); // System program
        let (builtin_id, config) = test_prototype.deconstruct();
        let feature_id = &config.feature_id;

        // Since the test feature IDs aren't included in the SDK, the only way
        // to simulate loading from snapshot with this feature active is to
        // create a bank, overwrite the feature set with the feature active,
        // then re-run the `finish_init` method.
        let mut bank = Bank::new_for_tests(&genesis_config);

        // Set up the feature set with the migration feature marked as active.
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.active_mut().insert(*feature_id, 0);
        bank.feature_set = Arc::new(feature_set);
        bank.store_account_and_update_capitalization(
            feature_id,
            &feature::create_account(
                &Feature {
                    activated_at: Some(0),
                },
                42,
            ),
        );

        // Run `compute_and_apply_features_after_snapshot_restore` to simulate
        // starting up from a snapshot. Clear all builtins to simulate a fresh
        // bank init.
        bank.transaction_processor
            .global_program_cache
            .write()
            .unwrap()
            .remove_programs(
                bank.transaction_processor
                    .builtin_program_ids
                    .read()
                    .unwrap()
                    .clone()
                    .into_iter(),
            );
        bank.transaction_processor
            .builtin_program_ids
            .write()
            .unwrap()
            .clear();
        bank.compute_and_apply_features_after_snapshot_restore();

        // Assert the feature is active and the bank still added the builtin.
        assert!(bank.feature_set.is_active(feature_id));
        assert!(bank
            .transaction_processor
            .builtin_program_ids
            .read()
            .unwrap()
            .contains(builtin_id));
        assert_eq!(
            bank.get_account(builtin_id).unwrap().owner(),
            &native_loader::id()
        );

        // Simulate crossing an epoch boundary for a new bank.
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        goto_end_of_slot(bank.clone());
        let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 33);

        // Assert the feature is active but the builtin was not migrated.
        assert!(bank.feature_set.is_active(feature_id));
        assert!(bank
            .transaction_processor
            .builtin_program_ids
            .read()
            .unwrap()
            .contains(builtin_id));
        assert_eq!(
            bank.get_account(builtin_id).unwrap().owner(),
            &native_loader::id()
        );
    }

    // Simulate creating a bank from a snapshot after a migration feature was
    // activated and the migration was successful.
    // Here we want to see that the bank recognizes the migration and
    // _does not_ add the original builtin to the new bank.
    #[test]
    fn test_migrate_builtin_e2e_init_after_successful_migration() {
        let (mut genesis_config, _mint_keypair) = create_genesis_config(0);

        let test_prototype = TestPrototype::Builtin(&BUILTINS[0]); // System program
        let (builtin_id, config) = test_prototype.deconstruct();
        let feature_id = &config.feature_id;

        let upgrade_authority_address = Some(Pubkey::new_unique());
        let elf = test_elf();
        let program_data_metadata_size = UpgradeableLoaderState::size_of_programdata_metadata();
        let program_data_size = program_data_metadata_size + elf.len();

        // Set up a post-migration builtin.
        let builtin_program_data_address = get_program_data_address(builtin_id);
        let builtin_program_account = AccountSharedData::new_data(
            100_000,
            &UpgradeableLoaderState::Program {
                programdata_address: builtin_program_data_address,
            },
            &bpf_loader_upgradeable::id(),
        )
        .unwrap();
        let mut builtin_program_data_account = AccountSharedData::new_data_with_space(
            100_000,
            &UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address,
            },
            program_data_size,
            &bpf_loader_upgradeable::id(),
        )
        .unwrap();
        builtin_program_data_account.data_as_mut_slice()[program_data_metadata_size..]
            .copy_from_slice(&elf);
        genesis_config
            .accounts
            .insert(*builtin_id, builtin_program_account.into());
        genesis_config.accounts.insert(
            builtin_program_data_address,
            builtin_program_data_account.into(),
        );

        // Use this closure to run checks on the builtin.
        let check_builtin_is_bpf = |bank: &Bank| {
            // The bank's transaction processor should not contain the builtin
            // in its list of builtin program IDs.
            assert!(!bank
                .transaction_processor
                .builtin_program_ids
                .read()
                .unwrap()
                .contains(builtin_id));
            // The builtin should be owned by the upgradeable loader and have
            // the correct state.
            let fetched_builtin_program_account = bank.get_account(builtin_id).unwrap();
            assert_eq!(
                fetched_builtin_program_account.owner(),
                &bpf_loader_upgradeable::id()
            );
            assert_eq!(
                bincode::deserialize::<UpgradeableLoaderState>(
                    fetched_builtin_program_account.data()
                )
                .unwrap(),
                UpgradeableLoaderState::Program {
                    programdata_address: builtin_program_data_address
                }
            );
            // The builtin's program data should be owned by the upgradeable
            // loader and have the correct state.
            let fetched_builtin_program_data_account =
                bank.get_account(&builtin_program_data_address).unwrap();
            assert_eq!(
                fetched_builtin_program_data_account.owner(),
                &bpf_loader_upgradeable::id()
            );
            assert_eq!(
                bincode::deserialize::<UpgradeableLoaderState>(
                    &fetched_builtin_program_data_account.data()[..program_data_metadata_size]
                )
                .unwrap(),
                UpgradeableLoaderState::ProgramData {
                    slot: 0,
                    upgrade_authority_address
                }
            );
            assert_eq!(
                &fetched_builtin_program_data_account.data()[program_data_metadata_size..],
                elf,
            );
        };

        // Create a new bank.
        let mut bank = Bank::new_for_tests(&genesis_config);
        check_builtin_is_bpf(&bank);

        // Now, add the feature ID as active, and run `finish_init` again to
        // make sure the feature is idempotent.
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.active_mut().insert(*feature_id, 0);
        bank.feature_set = Arc::new(feature_set);
        bank.store_account_and_update_capitalization(
            feature_id,
            &feature::create_account(
                &Feature {
                    activated_at: Some(0),
                },
                42,
            ),
        );

        // Run `compute_and_apply_features_after_snapshot_restore` to simulate
        // starting up from a snapshot. Clear all builtins to simulate a fresh
        // bank init.
        bank.transaction_processor
            .global_program_cache
            .write()
            .unwrap()
            .remove_programs(
                bank.transaction_processor
                    .builtin_program_ids
                    .read()
                    .unwrap()
                    .clone()
                    .into_iter(),
            );
        bank.transaction_processor
            .builtin_program_ids
            .write()
            .unwrap()
            .clear();
        bank.compute_and_apply_features_after_snapshot_restore();

        check_builtin_is_bpf(&bank);
    }

    #[test]
    fn test_upgrade_loader_v2_program_with_loader_v3_program() {
        let mut bank = create_simple_test_bank(0);

        let bpf_loader_v2_program_address = Pubkey::new_unique();
        let source_buffer_address = Pubkey::new_unique();

        {
            let program_account = {
                let elf = [4u8; 200]; // Mock ELF to start.
                let space = elf.len();
                let lamports = bank.get_minimum_balance_for_rent_exemption(space);
                let owner = &bpf_loader::id();

                let mut account = AccountSharedData::new(lamports, space, owner);
                account.set_executable(true);
                account.data_as_mut_slice().copy_from_slice(&elf);
                bank.store_account_and_update_capitalization(
                    &bpf_loader_v2_program_address,
                    &account,
                );
                account
            };

            assert_eq!(
                &bank.get_account(&bpf_loader_v2_program_address).unwrap(),
                &program_account
            );
        };

        let test_context = TestContext::new(
            &bank,
            &bpf_loader_v2_program_address,
            &source_buffer_address,
            None,
        );
        let TestContext {
            source_buffer_address,
            ..
        } = test_context;

        let (
            expected_post_upgrade_capitalization,
            expected_post_upgrade_accounts_data_size_delta_off_chain,
        ) = test_context
            .calculate_post_migration_capitalization_and_accounts_data_size_delta_off_chain(&bank);

        // Perform the upgrade.
        let upgrade_slot = bank.slot();
        bank.upgrade_loader_v2_program_with_loader_v3_program(
            &bpf_loader_v2_program_address,
            &source_buffer_address,
            "test_upgrade_loader_v2_program_with_loader_v3_program",
        )
        .unwrap();

        // Run the post-upgrade program checks.
        test_context.run_program_checks(&bank, upgrade_slot);

        // Check the bank's capitalization.
        assert_eq!(bank.capitalization(), expected_post_upgrade_capitalization);

        // Check the bank's accounts data size delta off-chain.
        assert_eq!(
            bank.accounts_data_size_delta_off_chain.load(Relaxed),
            expected_post_upgrade_accounts_data_size_delta_off_chain
        );

        // Check the migrated program account is now owned by the upgradeable loader.
        let migrated_program_account = bank.get_account(&bpf_loader_v2_program_address).unwrap();
        assert_eq!(
            migrated_program_account.owner(),
            &bpf_loader_upgradeable::id()
        );
    }

    #[test]
    fn test_upgrade_loader_v2_program_with_loader_v3_program_fail_invalid_buffer() {
        let mut bank = create_simple_test_bank(0);

        let bpf_loader_v2_program_address = Pubkey::new_unique();
        let source_buffer_address = Pubkey::new_unique();

        {
            let program_account = {
                let elf = [4u8; 200]; // Mock ELF to start.
                let space = elf.len();
                let lamports = bank.get_minimum_balance_for_rent_exemption(space);
                let owner = &bpf_loader::id();

                let mut account = AccountSharedData::new(lamports, space, owner);
                account.set_executable(true);
                account.data_as_mut_slice().copy_from_slice(&elf);
                bank.store_account_and_update_capitalization(
                    &bpf_loader_v2_program_address,
                    &account,
                );
                account
            };

            assert_eq!(
                &bank.get_account(&bpf_loader_v2_program_address).unwrap(),
                &program_account
            );
        };

        // Set up the source buffer with a valid authority, but the migration
        // config will define the upgrade authority to be `None`.
        {
            let elf = test_elf();
            let buffer_metadata_size = UpgradeableLoaderState::size_of_buffer_metadata();
            let space = buffer_metadata_size + elf.len();
            let lamports = bank.get_minimum_balance_for_rent_exemption(space);
            let owner = &bpf_loader_upgradeable::id();

            let buffer_metadata = UpgradeableLoaderState::Program {
                programdata_address: Pubkey::new_unique(),
            };

            let mut account =
                AccountSharedData::new_data_with_space(lamports, &buffer_metadata, space, owner)
                    .unwrap();
            account.data_as_mut_slice()[buffer_metadata_size..].copy_from_slice(&elf);

            bank.store_account_and_update_capitalization(&source_buffer_address, &account);
        }

        // Try to perform the upgrade.
        assert_matches!(
            bank.upgrade_loader_v2_program_with_loader_v3_program(
                &bpf_loader_v2_program_address,
                &source_buffer_address,
                "test_upgrade_loader_v2_program_with_loader_v3_program",
            )
            .unwrap_err(),
            CoreBpfMigrationError::InvalidBufferAccount(_)
        )
    }

    /// Mock BPF loader v2 program for testing.
    fn mock_bpf_loader_v2_program(bank: &Bank) -> AccountSharedData {
        let elf = [4u8; 200]; // Mock ELF to start.
        let space = elf.len();
        let lamports = bank.get_minimum_balance_for_rent_exemption(space);
        let owner = &bpf_loader::id();

        let mut account = AccountSharedData::new(lamports, space, owner);
        account.set_executable(true);
        account.data_as_mut_slice().copy_from_slice(&elf);

        account
    }

    #[test]
    fn test_replace_spl_token_with_p_token_e2e() {
        let (mut genesis_config, mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let slots_per_epoch = 32;
        genesis_config.epoch_schedule =
            EpochSchedule::custom(slots_per_epoch, slots_per_epoch, false);

        let mut root_bank = Bank::new_for_tests(&genesis_config);

        let feature_id = agave_feature_set::replace_spl_token_with_p_token::id();
        let program_id = agave_feature_set::replace_spl_token_with_p_token::SPL_TOKEN_PROGRAM_ID;
        let source_buffer_address =
            agave_feature_set::replace_spl_token_with_p_token::PTOKEN_PROGRAM_BUFFER;

        // Set up a mock BPF loader v2 program.
        {
            let program_account = mock_bpf_loader_v2_program(&root_bank);
            root_bank.store_account_and_update_capitalization(&program_id, &program_account);
            assert_eq!(
                &root_bank.get_account(&program_id).unwrap(),
                &program_account
            );
        };

        // Set up the CPI mockup to test CPI'ing to the migrated program.
        let cpi_program_id = Pubkey::new_unique();
        let cpi_program_name = "mock_cpi_program";
        root_bank.add_builtin(
            cpi_program_id,
            cpi_program_name,
            ProgramCacheEntry::new_builtin(0, cpi_program_name.len(), cpi_mockup::Entrypoint::vm),
        );

        // Add the feature to the bank's inactive feature set.
        // Note this will add the feature ID if it doesn't exist.
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(&feature_id);
        root_bank.feature_set = Arc::new(feature_set);

        // Initialize the source buffer account.
        let test_context = TestContext::new(&root_bank, &program_id, &source_buffer_address, None);

        // Activate the feature and run the necessary checks.
        activate_feature_and_run_checks(
            root_bank,
            &test_context,
            &program_id,
            &feature_id,
            &source_buffer_address,
            &mint_keypair,
            slots_per_epoch,
            &cpi_program_id,
        );
    }

    // Simulate a failure to migrate the program.
    // Here we want to see that the bank handles the failure gracefully and
    // advances to the next epoch without issue.
    #[test]
    fn test_replace_spl_token_with_p_token_e2e_failure() {
        let (genesis_config, _mint_keypair) = create_genesis_config(0);
        let mut root_bank = Bank::new_for_tests(&genesis_config);

        let feature_id = &agave_feature_set::replace_spl_token_with_p_token::id();
        let program_id = &agave_feature_set::replace_spl_token_with_p_token::SPL_TOKEN_PROGRAM_ID;
        let source_buffer_address =
            &agave_feature_set::replace_spl_token_with_p_token::PTOKEN_PROGRAM_BUFFER;

        // Set up a mock BPF loader v2 program.
        {
            let program_account = mock_bpf_loader_v2_program(&root_bank);
            root_bank.store_account_and_update_capitalization(program_id, &program_account);
            assert_eq!(
                &root_bank.get_account(program_id).unwrap(),
                &program_account
            );
        };

        // Add the feature to the bank's inactive feature set.
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.inactive_mut().insert(*feature_id);
        root_bank.feature_set = Arc::new(feature_set);

        // Initialize the source buffer account.
        let _test_context = TestContext::new(&root_bank, program_id, source_buffer_address, None);

        let (bank, bank_forks) = root_bank.wrap_with_bank_forks_for_tests();

        // Intentionally nuke the source buffer account to force the migration
        // to fail.
        bank.store_account_and_update_capitalization(
            source_buffer_address,
            &AccountSharedData::default(),
        );

        // Activate the feature.
        bank.store_account_and_update_capitalization(
            feature_id,
            &feature::create_account(&Feature::default(), 42),
        );

        // Advance the bank to cross the epoch boundary and activate the
        // feature.
        goto_end_of_slot(bank.clone());
        let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 33);

        // Assert the feature _was_ activated but the program was not migrated.
        assert!(bank.feature_set.is_active(feature_id));
        assert_eq!(
            bank.get_account(program_id).unwrap().owner(),
            &bpf_loader::id()
        );

        // Simulate crossing an epoch boundary again.
        goto_end_of_slot(bank.clone());
        let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 96);

        // Again, assert the feature is still active and the program still was
        // not migrated.
        assert!(bank.feature_set.is_active(feature_id));
        assert_eq!(
            bank.get_account(program_id).unwrap().owner(),
            &bpf_loader::id()
        );
    }

    // Simulate creating a bank from a snapshot after p-token migration feature was
    // activated and the migration was successful.
    #[test]
    fn test_startup_from_snapshot_after_replace_spl_token_with_p_token() {
        let (genesis_config, _mint_keypair) = create_genesis_config(0);
        let mut bank = Bank::new_for_tests(&genesis_config);

        let bpf_loader_v2_program_address = Pubkey::new_unique();
        let source_buffer_address = Pubkey::new_unique();

        {
            let program_account = {
                let elf = [4u8; 200]; // Mock ELF to start.
                let space = elf.len();
                let lamports = bank.get_minimum_balance_for_rent_exemption(space);
                let owner = &bpf_loader::id();

                let mut account = AccountSharedData::new(lamports, space, owner);
                account.set_executable(true);
                account.data_as_mut_slice().copy_from_slice(&elf);
                bank.store_account_and_update_capitalization(
                    &bpf_loader_v2_program_address,
                    &account,
                );
                account
            };

            assert_eq!(
                &bank.get_account(&bpf_loader_v2_program_address).unwrap(),
                &program_account
            );
        };

        let test_context = TestContext::new(
            &bank,
            &bpf_loader_v2_program_address,
            &source_buffer_address,
            None,
        );
        let TestContext {
            source_buffer_address,
            ..
        } = test_context;

        // Perform the upgrade.
        let upgrade_slot = bank.slot();
        bank.upgrade_loader_v2_program_with_loader_v3_program(
            &bpf_loader_v2_program_address,
            &source_buffer_address,
            "test_upgrade_loader_v2_program_with_loader_v3_program",
        )
        .unwrap();

        // Run the post-upgrade program checks.
        test_context.run_program_checks(&bank, upgrade_slot);

        bank.fill_bank_with_ticks_for_tests();
        // Force flush the bank to create the account storage entry
        bank.squash();
        bank.force_flush_accounts_cache();

        // Create a snapshot of the bank.
        let (_tmp_dir, accounts_dir) = create_tmp_accounts_dir_for_tests();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archive_format = SnapshotConfig::default().archive_format;

        let full_snapshot_archive_info = bank_to_full_snapshot_archive(
            bank_snapshots_dir.path(),
            &bank,
            None,
            snapshot_archives_dir.path(),
            snapshot_archives_dir.path(),
            snapshot_archive_format,
        )
        .unwrap();

        // Restore the bank from the snapshot and run checks.
        let roundtrip_bank = bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            None,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();

        // Load the migrated program to the cache and run checks.
        let entry = roundtrip_bank
            .load_program(&bpf_loader_v2_program_address, false, upgrade_slot)
            .unwrap();

        let mut program_cache = roundtrip_bank
            .transaction_processor
            .global_program_cache
            .write()
            .unwrap();

        program_cache.assign_program(
            &roundtrip_bank.transaction_processor.environments,
            bpf_loader_v2_program_address,
            entry,
        );
        // Release the lock on the program cache.
        drop(program_cache);

        // Run the post-upgrade program checks on the restored bank.
        test_context.run_program_checks(&roundtrip_bank, upgrade_slot);
        assert_eq!(bank, roundtrip_bank);
    }

    #[test]
    fn test_replace_spl_token_with_p_token_and_funded_program_data_account_e2e() {
        let (mut genesis_config, mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let slots_per_epoch = 32;
        genesis_config.epoch_schedule =
            EpochSchedule::custom(slots_per_epoch, slots_per_epoch, false);

        let mut root_bank = Bank::new_for_tests(&genesis_config);

        let feature_id = agave_feature_set::replace_spl_token_with_p_token::id();
        let program_id = agave_feature_set::replace_spl_token_with_p_token::SPL_TOKEN_PROGRAM_ID;
        let source_buffer_address =
            agave_feature_set::replace_spl_token_with_p_token::PTOKEN_PROGRAM_BUFFER;

        // Set up a mock BPF loader v2 program.
        {
            let program_account = mock_bpf_loader_v2_program(&root_bank);
            root_bank.store_account_and_update_capitalization(&program_id, &program_account);
            assert_eq!(
                &root_bank.get_account(&program_id).unwrap(),
                &program_account
            );
        };

        // Set up the CPI mockup to test CPI'ing to the migrated program.
        let cpi_program_id = Pubkey::new_unique();
        let cpi_program_name = "mock_cpi_program";
        root_bank.add_builtin(
            cpi_program_id,
            cpi_program_name,
            ProgramCacheEntry::new_builtin(0, cpi_program_name.len(), cpi_mockup::Entrypoint::vm),
        );

        // Add the feature to the bank's inactive feature set.
        // Note this will add the feature ID if it doesn't exist.
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(&feature_id);
        root_bank.feature_set = Arc::new(feature_set);

        // Initialize the source buffer account.
        let test_context = TestContext::new(&root_bank, &program_id, &source_buffer_address, None);

        // Fund the program data account so it will appear as an existing account.
        let program_data_account = AccountSharedData::new(1_000_000_000, 0, &system_program::ID);
        root_bank.store_account_and_update_capitalization(
            &get_program_data_address(&program_id),
            &program_data_account,
        );

        // Activate the feature and run the necessary checks.
        activate_feature_and_run_checks(
            root_bank,
            &test_context,
            &program_id,
            &feature_id,
            &source_buffer_address,
            &mint_keypair,
            slots_per_epoch,
            &cpi_program_id,
        );
    }

    #[test]
    fn test_replace_spl_token_with_p_token_and_existing_program_data_account_failure() {
        let (mut genesis_config, _mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let slots_per_epoch = 32;
        genesis_config.epoch_schedule =
            EpochSchedule::custom(slots_per_epoch, slots_per_epoch, false);

        let mut root_bank = Bank::new_for_tests(&genesis_config);

        let feature_id = agave_feature_set::replace_spl_token_with_p_token::id();
        let program_id = agave_feature_set::replace_spl_token_with_p_token::SPL_TOKEN_PROGRAM_ID;
        let source_buffer_address =
            agave_feature_set::replace_spl_token_with_p_token::PTOKEN_PROGRAM_BUFFER;

        // Set up a mock BPF loader v2 program.
        let program_account = mock_bpf_loader_v2_program(&root_bank);
        root_bank.store_account_and_update_capitalization(&program_id, &program_account);
        assert_eq!(
            &root_bank.get_account(&program_id).unwrap(),
            &program_account
        );

        // Set up the CPI mockup to test CPI'ing to the migrated program.
        let cpi_program_id = Pubkey::new_unique();
        let cpi_program_name = "mock_cpi_program";
        root_bank.add_builtin(
            cpi_program_id,
            cpi_program_name,
            ProgramCacheEntry::new_builtin(0, cpi_program_name.len(), cpi_mockup::Entrypoint::vm),
        );

        // Add the feature to the bank's inactive feature set.
        // Note this will add the feature ID if it doesn't exist.
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(&feature_id);
        root_bank.feature_set = Arc::new(feature_set);

        // Initialize the source buffer account.
        let _test_context = TestContext::new(&root_bank, &program_id, &source_buffer_address, None);

        // Create the program data account to simulate existing account owned by
        // the upgradeable loader.
        let program_data_account =
            AccountSharedData::new(1_000_000_000, 0, &bpf_loader_upgradeable::ID);
        root_bank.store_account_and_update_capitalization(
            &get_program_data_address(&program_id),
            &program_data_account,
        );

        // Activate the feature and run the necessary checks.
        let (bank, bank_forks) = root_bank.wrap_with_bank_forks_for_tests();

        // Advance to the next epoch without activating the feature.
        let mut first_slot_in_next_epoch = slots_per_epoch + 1;
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank,
            &Pubkey::default(),
            first_slot_in_next_epoch,
        );

        // Assert the feature was not activated and the program was not
        // migrated.
        assert!(!bank.feature_set.is_active(&feature_id));
        assert!(bank.get_account(&source_buffer_address).is_some());

        // Store the account to activate the feature.
        bank.store_account_and_update_capitalization(
            &feature_id,
            &feature::create_account(&Feature::default(), 42),
        );

        // Advance the bank to cross the epoch boundary and activate the
        // feature.
        goto_end_of_slot(bank.clone());
        first_slot_in_next_epoch += slots_per_epoch;
        let _migration_slot = first_slot_in_next_epoch;
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank,
            &Pubkey::default(),
            first_slot_in_next_epoch,
        );

        // Check that the feature was activated.
        assert!(bank.feature_set.is_active(&feature_id));
        // The program should still be owned by the bpf loader v2.
        let program_account = bank.get_account(&program_id).unwrap();
        assert_eq!(program_account.owner(), &bpf_loader::id());
        // The program data should have zero data and still have
        // 1_000_000_000 lamports.
        let program_data_account = bank
            .get_account(&get_program_data_address(&program_id))
            .unwrap();
        assert_eq!(program_data_account.data().len(), 0);
        assert_eq!(program_data_account.lamports(), 1_000_000_000);
    }
}
