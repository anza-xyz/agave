use {
    crate::{
        builtin_programs_filter::{BuiltinProgramsFilter, ProgramKind},
        compute_budget_program_id_filter::ComputeBudgetProgramIdFilter,
    },
    agave_feature_set::{self as feature_set, FeatureSet},
    solana_borsh::v1::try_from_slice_unchecked,
    solana_builtins_default_costs::{get_migration_feature_id, MIGRATING_BUILTINS_COSTS},
    solana_compute_budget::compute_budget_limits::*,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_svm_transaction::instruction::SVMInstruction,
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    std::num::{NonZeroU32, Saturating},
};

#[cfg_attr(test, derive(Eq, PartialEq))]
#[cfg_attr(feature = "dev-context-only-utils", derive(Clone))]
#[derive(Debug)]
struct MigrationBuiltinFeatureCounter {
    // The vector of counters, matching the size of the static vector MIGRATION_FEATURE_IDS,
    // each counter representing the number of times its corresponding feature ID is
    // referenced in this transaction.
    migrating_builtin: [Saturating<u16>; MIGRATING_BUILTINS_COSTS.len()],
}

impl Default for MigrationBuiltinFeatureCounter {
    fn default() -> Self {
        Self {
            migrating_builtin: [Saturating(0); MIGRATING_BUILTINS_COSTS.len()],
        }
    }
}

#[cfg_attr(test, derive(Eq, PartialEq))]
#[cfg_attr(feature = "dev-context-only-utils", derive(Clone))]
#[derive(Default, Debug)]
pub struct ComputeBudgetInstructionDetails {
    // compute-budget instruction details:
    // the first field in tuple is instruction index, second field is the unsanitized value set by user
    requested_compute_unit_limit: Option<(u8, u32)>,
    requested_compute_unit_price: Option<(u8, u64)>,
    requested_heap_size: Option<(u8, u32)>,
    requested_loaded_accounts_data_size_limit: Option<(u8, u32)>,
    num_non_compute_budget_instructions: Saturating<u16>,
    // Additional builtin program counters
    num_non_migratable_builtin_instructions: Saturating<u16>,
    num_non_builtin_instructions: Saturating<u16>,
    migrating_builtin_feature_counters: MigrationBuiltinFeatureCounter,
}

impl ComputeBudgetInstructionDetails {
    pub fn try_from<'a>(
        instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)> + Clone,
    ) -> Result<Self> {
        let mut filter = ComputeBudgetProgramIdFilter::new();
        let mut compute_budget_instruction_details = ComputeBudgetInstructionDetails::default();

        for (i, (program_id, instruction)) in instructions.clone().enumerate() {
            if filter.is_compute_budget_program(instruction.program_id_index as usize, program_id) {
                compute_budget_instruction_details.process_instruction(i as u8, &instruction)?;
            } else {
                compute_budget_instruction_details.num_non_compute_budget_instructions += 1;
            }
        }

        if compute_budget_instruction_details
            .requested_compute_unit_limit
            .is_none()
        {
            let mut filter = BuiltinProgramsFilter::new();
            // reiterate to collect builtin details
            for (program_id, instruction) in instructions {
                match filter.get_program_kind(instruction.program_id_index as usize, program_id) {
                    ProgramKind::Builtin => {
                        compute_budget_instruction_details
                            .num_non_migratable_builtin_instructions += 1;
                    }
                    ProgramKind::NotBuiltin => {
                        compute_budget_instruction_details.num_non_builtin_instructions += 1;
                    }
                    ProgramKind::MigratingBuiltin {
                        core_bpf_migration_feature_index,
                    } => {
                        *compute_budget_instruction_details
                            .migrating_builtin_feature_counters
                            .migrating_builtin
                            .get_mut(core_bpf_migration_feature_index)
                            .expect(
                                "migrating feature index within range of MIGRATION_FEATURE_IDS",
                            ) += 1;
                    }
                }
            }
        }

        Ok(compute_budget_instruction_details)
    }

    pub fn sanitize_and_convert_to_compute_budget_limits(
        &self,
        feature_set: &FeatureSet,
    ) -> Result<ComputeBudgetLimits> {
        // Sanitize requested heap size
        let updated_heap_bytes =
            if let Some((index, requested_heap_size)) = self.requested_heap_size {
                if Self::sanitize_requested_heap_size(requested_heap_size) {
                    requested_heap_size
                } else {
                    return Err(TransactionError::InstructionError(
                        index,
                        InstructionError::InvalidInstructionData,
                    ));
                }
            } else {
                MIN_HEAP_FRAME_BYTES
            }
            .min(MAX_HEAP_FRAME_BYTES);

        // Calculate compute unit limit
        let compute_unit_limit = self
            .requested_compute_unit_limit
            .map_or_else(
                || self.calculate_default_compute_unit_limit(feature_set),
                |(_index, requested_compute_unit_limit)| requested_compute_unit_limit,
            )
            .min(MAX_COMPUTE_UNIT_LIMIT);

        let compute_unit_price = self
            .requested_compute_unit_price
            .map_or(0, |(_index, requested_compute_unit_price)| {
                requested_compute_unit_price
            });

        let loaded_accounts_bytes =
            if let Some((_index, requested_loaded_accounts_data_size_limit)) =
                self.requested_loaded_accounts_data_size_limit
            {
                NonZeroU32::new(requested_loaded_accounts_data_size_limit)
                    .ok_or(TransactionError::InvalidLoadedAccountsDataSizeLimit)?
            } else {
                MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES
            }
            .min(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES);

        Ok(ComputeBudgetLimits {
            updated_heap_bytes,
            compute_unit_limit,
            compute_unit_price,
            loaded_accounts_bytes,
        })
    }

    fn process_instruction(&mut self, index: u8, instruction: &SVMInstruction) -> Result<()> {
        let invalid_instruction_data_error =
            TransactionError::InstructionError(index, InstructionError::InvalidInstructionData);
        let duplicate_instruction_error = TransactionError::DuplicateInstruction(index);

        match try_from_slice_unchecked(instruction.data) {
            Ok(ComputeBudgetInstruction::RequestHeapFrame(bytes)) => {
                if self.requested_heap_size.is_some() {
                    return Err(duplicate_instruction_error);
                }
                self.requested_heap_size = Some((index, bytes));
            }
            Ok(ComputeBudgetInstruction::SetComputeUnitLimit(compute_unit_limit)) => {
                if self.requested_compute_unit_limit.is_some() {
                    return Err(duplicate_instruction_error);
                }
                self.requested_compute_unit_limit = Some((index, compute_unit_limit));
            }
            Ok(ComputeBudgetInstruction::SetComputeUnitPrice(micro_lamports)) => {
                if self.requested_compute_unit_price.is_some() {
                    return Err(duplicate_instruction_error);
                }
                self.requested_compute_unit_price = Some((index, micro_lamports));
            }
            Ok(ComputeBudgetInstruction::SetLoadedAccountsDataSizeLimit(bytes)) => {
                if self.requested_loaded_accounts_data_size_limit.is_some() {
                    return Err(duplicate_instruction_error);
                }
                self.requested_loaded_accounts_data_size_limit = Some((index, bytes));
            }
            _ => return Err(invalid_instruction_data_error),
        }

        Ok(())
    }

    #[inline]
    fn sanitize_requested_heap_size(bytes: u32) -> bool {
        (MIN_HEAP_FRAME_BYTES..=MAX_HEAP_FRAME_BYTES).contains(&bytes) && bytes % 1024 == 0
    }

    fn calculate_default_compute_unit_limit(&self, feature_set: &FeatureSet) -> u32 {
        if feature_set.is_active(&feature_set::reserve_minimal_cus_for_builtin_instructions::id()) {
            // evaluate if any builtin has migrated with feature_set
            let (num_migrated, num_not_migrated) = self
                .migrating_builtin_feature_counters
                .migrating_builtin
                .iter()
                .enumerate()
                .fold((0, 0), |(migrated, not_migrated), (index, count)| {
                    if count.0 > 0 && feature_set.is_active(get_migration_feature_id(index)) {
                        (migrated + count.0, not_migrated)
                    } else {
                        (migrated, not_migrated + count.0)
                    }
                });

            u32::from(self.num_non_migratable_builtin_instructions.0)
                .saturating_add(u32::from(num_not_migrated))
                .saturating_mul(MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT)
                .saturating_add(
                    u32::from(self.num_non_builtin_instructions.0)
                        .saturating_add(u32::from(num_migrated))
                        .saturating_mul(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT),
                )
        } else {
            u32::from(self.num_non_compute_budget_instructions.0)
                .saturating_mul(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT)
        }
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_builtins_default_costs::get_migration_feature_position,
        solana_instruction::Instruction,
        solana_keypair::Keypair,
        solana_message::Message,
        solana_pubkey::Pubkey,
        solana_signer::Signer,
        solana_svm_transaction::svm_message::SVMMessage,
        solana_transaction::{sanitized::SanitizedTransaction, Transaction},
    };

    fn build_sanitized_transaction(instructions: &[Instruction]) -> SanitizedTransaction {
        let payer_keypair = Keypair::new();
        SanitizedTransaction::from_transaction_for_tests(Transaction::new_unsigned(Message::new(
            instructions,
            Some(&payer_keypair.pubkey()),
        )))
    }

    #[test]
    fn test_try_from_request_heap() {
        let tx = build_sanitized_transaction(&[
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
            ComputeBudgetInstruction::request_heap_frame(40 * 1024),
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
        ]);
        let expected_details = Ok(ComputeBudgetInstructionDetails {
            requested_heap_size: Some((1, 40 * 1024)),
            num_non_compute_budget_instructions: Saturating(2),
            num_non_migratable_builtin_instructions: Saturating(1),
            num_non_builtin_instructions: Saturating(2),
            ..ComputeBudgetInstructionDetails::default()
        });
        assert_eq!(
            ComputeBudgetInstructionDetails::try_from(SVMMessage::program_instructions_iter(&tx),),
            expected_details
        );

        let tx = build_sanitized_transaction(&[
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
            ComputeBudgetInstruction::request_heap_frame(40 * 1024),
            ComputeBudgetInstruction::request_heap_frame(41 * 1024),
        ]);
        assert_eq!(
            ComputeBudgetInstructionDetails::try_from(SVMMessage::program_instructions_iter(&tx),),
            Err(TransactionError::DuplicateInstruction(2))
        );
    }

    #[test]
    fn test_try_from_compute_unit_limit() {
        let tx = build_sanitized_transaction(&[
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
            ComputeBudgetInstruction::set_compute_unit_limit(u32::MAX),
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
        ]);
        let expected_details = Ok(ComputeBudgetInstructionDetails {
            requested_compute_unit_limit: Some((1, u32::MAX)),
            num_non_compute_budget_instructions: Saturating(2),
            ..ComputeBudgetInstructionDetails::default()
        });
        assert_eq!(
            ComputeBudgetInstructionDetails::try_from(SVMMessage::program_instructions_iter(&tx),),
            expected_details
        );

        let tx = build_sanitized_transaction(&[
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
            ComputeBudgetInstruction::set_compute_unit_limit(0),
            ComputeBudgetInstruction::set_compute_unit_limit(u32::MAX),
        ]);
        assert_eq!(
            ComputeBudgetInstructionDetails::try_from(SVMMessage::program_instructions_iter(&tx),),
            Err(TransactionError::DuplicateInstruction(2))
        );
    }

    #[test]
    fn test_try_from_compute_unit_price() {
        let tx = build_sanitized_transaction(&[
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
            ComputeBudgetInstruction::set_compute_unit_price(u64::MAX),
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
        ]);
        let expected_details = Ok(ComputeBudgetInstructionDetails {
            requested_compute_unit_price: Some((1, u64::MAX)),
            num_non_compute_budget_instructions: Saturating(2),
            num_non_migratable_builtin_instructions: Saturating(1),
            num_non_builtin_instructions: Saturating(2),
            ..ComputeBudgetInstructionDetails::default()
        });
        assert_eq!(
            ComputeBudgetInstructionDetails::try_from(SVMMessage::program_instructions_iter(&tx),),
            expected_details
        );

        let tx = build_sanitized_transaction(&[
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
            ComputeBudgetInstruction::set_compute_unit_price(0),
            ComputeBudgetInstruction::set_compute_unit_price(u64::MAX),
        ]);
        assert_eq!(
            ComputeBudgetInstructionDetails::try_from(SVMMessage::program_instructions_iter(&tx),),
            Err(TransactionError::DuplicateInstruction(2))
        );
    }

    #[test]
    fn test_try_from_loaded_accounts_data_size_limit() {
        let tx = build_sanitized_transaction(&[
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(u32::MAX),
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
        ]);
        let expected_details = Ok(ComputeBudgetInstructionDetails {
            requested_loaded_accounts_data_size_limit: Some((1, u32::MAX)),
            num_non_compute_budget_instructions: Saturating(2),
            num_non_migratable_builtin_instructions: Saturating(1),
            num_non_builtin_instructions: Saturating(2),
            ..ComputeBudgetInstructionDetails::default()
        });
        assert_eq!(
            ComputeBudgetInstructionDetails::try_from(SVMMessage::program_instructions_iter(&tx),),
            expected_details
        );

        let tx = build_sanitized_transaction(&[
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(0),
            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(u32::MAX),
        ]);
        assert_eq!(
            ComputeBudgetInstructionDetails::try_from(SVMMessage::program_instructions_iter(&tx),),
            Err(TransactionError::DuplicateInstruction(2))
        );
    }

    fn prep_feature_minimial_cus_for_builtin_instructions(
        is_active: bool,
        instruction_details: &ComputeBudgetInstructionDetails,
    ) -> (FeatureSet, u32) {
        let mut feature_set = FeatureSet::default();
        let ComputeBudgetInstructionDetails {
            num_non_compute_budget_instructions,
            num_non_migratable_builtin_instructions,
            num_non_builtin_instructions,
            ..
        } = *instruction_details;
        let expected_cu_limit = if is_active {
            feature_set.activate(
                &feature_set::reserve_minimal_cus_for_builtin_instructions::id(),
                0,
            );
            u32::from(num_non_builtin_instructions.0) * DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT
                + u32::from(num_non_migratable_builtin_instructions.0)
                    * MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT
        } else {
            u32::from(num_non_compute_budget_instructions.0)
                * DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT
        };

        (feature_set, expected_cu_limit)
    }

    #[test]
    fn test_sanitize_and_convert_to_compute_budget_limits() {
        // empty details, default ComputeBudgetLimits with 0 compute_unit_limits
        let instruction_details = ComputeBudgetInstructionDetails::default();
        assert_eq!(
            instruction_details
                .sanitize_and_convert_to_compute_budget_limits(&FeatureSet::default()),
            Ok(ComputeBudgetLimits {
                compute_unit_limit: 0,
                ..ComputeBudgetLimits::default()
            })
        );

        // no compute-budget instructions, all default ComputeBudgetLimits except cu-limit
        let instruction_details = ComputeBudgetInstructionDetails {
            num_non_compute_budget_instructions: Saturating(4),
            num_non_migratable_builtin_instructions: Saturating(1),
            num_non_builtin_instructions: Saturating(3),
            ..ComputeBudgetInstructionDetails::default()
        };
        for is_active in [true, false] {
            let (feature_set, expected_compute_unit_limit) =
                prep_feature_minimial_cus_for_builtin_instructions(is_active, &instruction_details);
            assert_eq!(
                instruction_details.sanitize_and_convert_to_compute_budget_limits(&feature_set),
                Ok(ComputeBudgetLimits {
                    compute_unit_limit: expected_compute_unit_limit,
                    ..ComputeBudgetLimits::default()
                })
            );
        }

        let expected_heap_size_err = Err(TransactionError::InstructionError(
            3,
            InstructionError::InvalidInstructionData,
        ));
        // invalid: requested_heap_size can't be zero
        let instruction_details = ComputeBudgetInstructionDetails {
            requested_compute_unit_limit: Some((1, 0)),
            requested_compute_unit_price: Some((2, 0)),
            requested_heap_size: Some((3, 0)),
            requested_loaded_accounts_data_size_limit: Some((4, 1024)),
            ..ComputeBudgetInstructionDetails::default()
        };
        for is_active in [true, false] {
            let (feature_set, _expected_compute_unit_limit) =
                prep_feature_minimial_cus_for_builtin_instructions(is_active, &instruction_details);
            assert_eq!(
                instruction_details.sanitize_and_convert_to_compute_budget_limits(&feature_set),
                expected_heap_size_err
            );
        }

        // invalid: requested_heap_size can't be less than MIN_HEAP_FRAME_BYTES
        let instruction_details = ComputeBudgetInstructionDetails {
            requested_compute_unit_limit: Some((1, 0)),
            requested_compute_unit_price: Some((2, 0)),
            requested_heap_size: Some((3, MIN_HEAP_FRAME_BYTES - 1)),
            requested_loaded_accounts_data_size_limit: Some((4, 1024)),
            ..ComputeBudgetInstructionDetails::default()
        };
        for is_active in [true, false] {
            let (feature_set, _expected_compute_unit_limit) =
                prep_feature_minimial_cus_for_builtin_instructions(is_active, &instruction_details);
            assert_eq!(
                instruction_details.sanitize_and_convert_to_compute_budget_limits(&feature_set),
                expected_heap_size_err
            );
        }

        // invalid: requested_heap_size can't be more than MAX_HEAP_FRAME_BYTES
        let instruction_details = ComputeBudgetInstructionDetails {
            requested_compute_unit_limit: Some((1, 0)),
            requested_compute_unit_price: Some((2, 0)),
            requested_heap_size: Some((3, MAX_HEAP_FRAME_BYTES + 1)),
            requested_loaded_accounts_data_size_limit: Some((4, 1024)),
            ..ComputeBudgetInstructionDetails::default()
        };
        for is_active in [true, false] {
            let (feature_set, _expected_compute_unit_limit) =
                prep_feature_minimial_cus_for_builtin_instructions(is_active, &instruction_details);
            assert_eq!(
                instruction_details.sanitize_and_convert_to_compute_budget_limits(&feature_set),
                expected_heap_size_err
            );
        }

        // invalid: requested_heap_size must be round by 1024
        let instruction_details = ComputeBudgetInstructionDetails {
            requested_compute_unit_limit: Some((1, 0)),
            requested_compute_unit_price: Some((2, 0)),
            requested_heap_size: Some((3, MIN_HEAP_FRAME_BYTES + 1024 + 1)),
            requested_loaded_accounts_data_size_limit: Some((4, 1024)),
            ..ComputeBudgetInstructionDetails::default()
        };
        for is_active in [true, false] {
            let (feature_set, _expected_compute_unit_limit) =
                prep_feature_minimial_cus_for_builtin_instructions(is_active, &instruction_details);
            assert_eq!(
                instruction_details.sanitize_and_convert_to_compute_budget_limits(&feature_set),
                expected_heap_size_err
            );
        }

        // invalid: loaded_account_data_size can't be zero
        let instruction_details = ComputeBudgetInstructionDetails {
            requested_compute_unit_limit: Some((1, 0)),
            requested_compute_unit_price: Some((2, 0)),
            requested_heap_size: Some((3, 40 * 1024)),
            requested_loaded_accounts_data_size_limit: Some((4, 0)),
            ..ComputeBudgetInstructionDetails::default()
        };
        for is_active in [true, false] {
            let (feature_set, _expected_compute_unit_limit) =
                prep_feature_minimial_cus_for_builtin_instructions(is_active, &instruction_details);
            assert_eq!(
                instruction_details.sanitize_and_convert_to_compute_budget_limits(&feature_set),
                Err(TransactionError::InvalidLoadedAccountsDataSizeLimit)
            );
        }

        // valid: acceptable MAX
        let instruction_details = ComputeBudgetInstructionDetails {
            requested_compute_unit_limit: Some((1, u32::MAX)),
            requested_compute_unit_price: Some((2, u64::MAX)),
            requested_heap_size: Some((3, MAX_HEAP_FRAME_BYTES)),
            requested_loaded_accounts_data_size_limit: Some((4, u32::MAX)),
            num_non_compute_budget_instructions: Saturating(4),
            ..ComputeBudgetInstructionDetails::default()
        };
        for is_active in [true, false] {
            let (feature_set, _expected_compute_unit_limit) =
                prep_feature_minimial_cus_for_builtin_instructions(is_active, &instruction_details);
            assert_eq!(
                instruction_details.sanitize_and_convert_to_compute_budget_limits(&feature_set),
                Ok(ComputeBudgetLimits {
                    updated_heap_bytes: MAX_HEAP_FRAME_BYTES,
                    compute_unit_limit: MAX_COMPUTE_UNIT_LIMIT,
                    compute_unit_price: u64::MAX,
                    loaded_accounts_bytes: MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
                })
            );
        }

        // valid
        let val: u32 = 1024 * 40;
        let instruction_details = ComputeBudgetInstructionDetails {
            requested_compute_unit_limit: Some((1, val)),
            requested_compute_unit_price: Some((2, val as u64)),
            requested_heap_size: Some((3, val)),
            requested_loaded_accounts_data_size_limit: Some((4, val)),
            ..ComputeBudgetInstructionDetails::default()
        };
        for is_active in [true, false] {
            let (feature_set, _expected_compute_unit_limit) =
                prep_feature_minimial_cus_for_builtin_instructions(is_active, &instruction_details);
            assert_eq!(
                instruction_details.sanitize_and_convert_to_compute_budget_limits(&feature_set),
                Ok(ComputeBudgetLimits {
                    updated_heap_bytes: val,
                    compute_unit_limit: val,
                    compute_unit_price: val as u64,
                    loaded_accounts_bytes: NonZeroU32::new(val).unwrap(),
                })
            );
        }
    }

    #[test]
    fn test_builtin_program_migration() {
        let tx = build_sanitized_transaction(&[
            Instruction::new_with_bincode(Pubkey::new_unique(), &(), vec![]),
            solana_program::stake::instruction::delegate_stake(
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
            ),
        ]);
        let feature_id_index =
            get_migration_feature_position(&feature_set::migrate_stake_program_to_core_bpf::id());
        let mut expected_details = ComputeBudgetInstructionDetails {
            num_non_compute_budget_instructions: Saturating(2),
            num_non_builtin_instructions: Saturating(1),
            ..ComputeBudgetInstructionDetails::default()
        };
        expected_details
            .migrating_builtin_feature_counters
            .migrating_builtin[feature_id_index] = Saturating(1);
        let expected_details = Ok(expected_details);
        let details =
            ComputeBudgetInstructionDetails::try_from(SVMMessage::program_instructions_iter(&tx));
        assert_eq!(details, expected_details);
        let details = details.unwrap();

        // reserve_minimal_cus_for_builtin_instructions: false;
        // migrate_stake_program_to_core_bpf: false;
        // expect: 1 bpf ix, 1 non-compute-budget builtin, cu-limit = 2 * 200K
        let mut feature_set = FeatureSet::default();
        let cu_limits = details.sanitize_and_convert_to_compute_budget_limits(&feature_set);
        assert_eq!(
            cu_limits,
            Ok(ComputeBudgetLimits {
                compute_unit_limit: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT * 2,
                ..ComputeBudgetLimits::default()
            })
        );

        // reserve_minimal_cus_for_builtin_instructions: true;
        // migrate_stake_program_to_core_bpf: false;
        // expect: 1 bpf ix, 1 non-compute-budget builtin, cu-limit = 200K + 3K
        feature_set.activate(
            &feature_set::reserve_minimal_cus_for_builtin_instructions::id(),
            0,
        );
        let cu_limits = details.sanitize_and_convert_to_compute_budget_limits(&feature_set);
        assert_eq!(
            cu_limits,
            Ok(ComputeBudgetLimits {
                compute_unit_limit: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT
                    + MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT,
                ..ComputeBudgetLimits::default()
            })
        );

        // reserve_minimal_cus_for_builtin_instructions: true;
        // migrate_stake_program_to_core_bpf: true;
        // expect: 2 bpf ix, cu-limit = 2 * 200K
        feature_set.activate(&feature_set::migrate_stake_program_to_core_bpf::id(), 0);
        let cu_limits = details.sanitize_and_convert_to_compute_budget_limits(&feature_set);
        assert_eq!(
            cu_limits,
            Ok(ComputeBudgetLimits {
                compute_unit_limit: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT * 2,
                ..ComputeBudgetLimits::default()
            })
        );

        // reserve_minimal_cus_for_builtin_instructions: false;
        // migrate_stake_program_to_core_bpf: false;
        // expect: 1 bpf ix, 1 non-compute-budget builtin, cu-limit = 2 * 200K
        feature_set.deactivate(&feature_set::reserve_minimal_cus_for_builtin_instructions::id());
        let cu_limits = details.sanitize_and_convert_to_compute_budget_limits(&feature_set);
        assert_eq!(
            cu_limits,
            Ok(ComputeBudgetLimits {
                compute_unit_limit: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT * 2,
                ..ComputeBudgetLimits::default()
            })
        );
    }
}
