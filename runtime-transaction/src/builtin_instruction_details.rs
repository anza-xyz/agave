use {
    agave_transaction_view::static_account_keys_frame::MAX_STATIC_ACCOUNTS_PER_PACKET as FILTER_SIZE,
    solana_builtins_default_costs::BUILTIN_INSTRUCTION_COSTS,
    solana_compute_budget::compute_budget_limits::{
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT, MAX_COMPUTE_UNIT_LIMIT,
    },
    solana_pubkey::Pubkey,
    solana_svm_transaction::instruction::SVMInstruction,
};

#[cfg_attr(feature = "dev-context-only-utils", derive(Clone))]
#[derive(Debug, Default)]
pub struct BuiltinInstructionDetails {
    /// Whether the transaction has user space instructions.
    pub has_user_space_instructions: bool,
    /// Sum of all instruction execution costs.
    /// This does not consider if the a CU request is present.
    pub instruction_execution_costs: u64,
    /// Sum of instruction data length.
    pub data_bytes_len_total: u64,
}

impl BuiltinInstructionDetails {
    pub fn process_instructions<'a>(
        instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
    ) -> Self {
        let mut filter = BuiltinInstructionDetailsFilter::default();
        let mut details = BuiltinInstructionDetails::default();

        for (program_id, instruction) in instructions {
            let program_id_index = instruction.program_id_index as usize;
            let kind = filter.check_program_id(program_id_index, program_id);

            match kind {
                InstructionKind::Builtin(cost) => {
                    details.instruction_execution_costs =
                        details.instruction_execution_costs.saturating_add(cost);
                }
                InstructionKind::UserSpace => {
                    details.has_user_space_instructions = true;
                    details.instruction_execution_costs = details
                        .instruction_execution_costs
                        .saturating_add(u64::from(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT));
                }
            }

            details.data_bytes_len_total = details
                .data_bytes_len_total
                .saturating_add(instruction.data.len() as u64);
        }

        details.instruction_execution_costs = details
            .instruction_execution_costs
            .min(u64::from(MAX_COMPUTE_UNIT_LIMIT));

        details
    }
}

#[derive(Debug, Clone, Copy)]
enum InstructionKind {
    /// Builtin instruction with a fixed cost.
    Builtin(u64),
    /// User-space instruction with a default cost.
    UserSpace,
}

struct BuiltinInstructionDetailsFilter {
    // array of slots for all possible static and sanitized program_id_index,
    // each slot indicates if a program_id_index has not been checked (eg, None),
    // or already checked with result (eg, Some(result)) that can be reused.
    flags: [Option<InstructionKind>; FILTER_SIZE as usize],
}

impl Default for BuiltinInstructionDetailsFilter {
    fn default() -> Self {
        BuiltinInstructionDetailsFilter {
            flags: [None; FILTER_SIZE as usize],
        }
    }
}

impl BuiltinInstructionDetailsFilter {
    fn check_program_id(&mut self, index: usize, program_id: &Pubkey) -> InstructionKind {
        *self
            .flags
            .get_mut(index)
            .expect("program id index is sanitized")
            .get_or_insert_with(|| {
                BUILTIN_INSTRUCTION_COSTS
                    .get(program_id)
                    .map(|cost| InstructionKind::Builtin(*cost))
                    .unwrap_or(InstructionKind::UserSpace)
            })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sdk::{compute_budget, system_program},
    };

    #[test]
    fn test_builtin_instruction_details_empty() {
        let instructions = [].into_iter();
        let details = BuiltinInstructionDetails::process_instructions(instructions);
        assert!(!details.has_user_space_instructions);
        assert_eq!(details.instruction_execution_costs, 0);
        assert_eq!(details.data_bytes_len_total, 0);
    }

    #[test]
    fn test_builtin_instruction_details_non_builtin() {
        let user_program_id = Pubkey::new_unique();
        let instructions = [(
            &user_program_id,
            SVMInstruction {
                program_id_index: 0,
                accounts: &[],
                data: &[1, 2, 3],
            },
        )]
        .into_iter();
        let details = BuiltinInstructionDetails::process_instructions(instructions);

        assert!(details.has_user_space_instructions);
        assert_eq!(
            details.instruction_execution_costs,
            u64::from(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT)
        );
        assert_eq!(details.data_bytes_len_total, 3);
    }

    #[test]
    fn test_builtin_instruction_details_builtin_only() {
        let instructions = [
            (
                &system_program::ID,
                SVMInstruction {
                    program_id_index: 0,
                    accounts: &[],
                    data: &[1, 2, 3],
                },
            ),
            (
                &compute_budget::ID,
                SVMInstruction {
                    program_id_index: 1,
                    accounts: &[],
                    data: &[4, 5],
                },
            ),
        ]
        .into_iter();
        let details = BuiltinInstructionDetails::process_instructions(instructions);

        assert!(!details.has_user_space_instructions);
        assert_eq!(
            details.instruction_execution_costs,
            BUILTIN_INSTRUCTION_COSTS
                .get(&system_program::ID)
                .unwrap()
                .saturating_add(*BUILTIN_INSTRUCTION_COSTS.get(&compute_budget::ID).unwrap())
        );
        assert_eq!(details.data_bytes_len_total, 5);
    }

    #[test]
    fn test_builtin_instruction_details_mixed() {
        let user_program_id = Pubkey::new_unique();
        let instructions = [
            (
                &system_program::ID,
                SVMInstruction {
                    program_id_index: 0,
                    accounts: &[],
                    data: &[1, 2, 3],
                },
            ),
            (
                &user_program_id,
                SVMInstruction {
                    program_id_index: 1,
                    accounts: &[],
                    data: &[4, 5],
                },
            ),
        ]
        .into_iter();
        let details = BuiltinInstructionDetails::process_instructions(instructions);

        assert!(details.has_user_space_instructions);
        assert_eq!(
            details.instruction_execution_costs,
            u64::from(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT)
                .saturating_add(*BUILTIN_INSTRUCTION_COSTS.get(&system_program::ID).unwrap())
        );
        assert_eq!(details.data_bytes_len_total, 5);
    }
}
