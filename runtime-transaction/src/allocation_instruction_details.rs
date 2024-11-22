use {
    agave_transaction_view::static_account_keys_frame::MAX_STATIC_ACCOUNTS_PER_PACKET as FILTER_SIZE,
    solana_pubkey::Pubkey,
    solana_sdk::{
        program_utils::limited_deserialize,
        system_instruction::{
            SystemInstruction, MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION,
            MAX_PERMITTED_DATA_LENGTH,
        },
        system_program,
    },
    solana_svm_transaction::instruction::SVMInstruction,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct AllocationInstructionDetails {
    pub total_space: u64,
}

impl AllocationInstructionDetails {
    pub fn process_instructions<'a>(
        instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
    ) -> Option<Self> {
        let mut filter = SystemProgramIdFilter::default();
        let mut details = AllocationInstructionDetails::default();

        for (program_id, instruction) in instructions {
            if filter.is_system_program(usize::from(instruction.program_id_index), program_id) {
                let requested_space = match parse_system_instruction_allocation(instruction.data) {
                    SystemProgramAccountAllocation::Some(space) => space,
                    SystemProgramAccountAllocation::Failed => return None,
                    SystemProgramAccountAllocation::None => 0,
                };

                details.total_space = details.total_space.saturating_add(requested_space);
            }
        }

        // The runtime prevents transactions from allocating too much account
        // data so clamp the attempted allocation size to the max amount.
        //
        // Note that if there are any custom bpf instructions in the transaction
        // it's tricky to know whether a newly allocated account will be freed
        // or not during an intermediate instruction in the transaction so we
        // shouldn't assume that a large sum of allocations will necessarily
        // lead to transaction failure.
        details.total_space = (MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION as u64)
            .min(details.total_space);

        Some(details)
    }
}

struct SystemProgramIdFilter {
    // array of slots for all possible static and sanitized program_id_index,
    // each slot indicates if a program_id_index has not been checked (eg, None),
    // or already checked with result (eg, Some(result)) that can be reused.
    flags: [Option<bool>; FILTER_SIZE as usize],
}

impl Default for SystemProgramIdFilter {
    fn default() -> Self {
        SystemProgramIdFilter {
            flags: [None; FILTER_SIZE as usize],
        }
    }
}

impl SystemProgramIdFilter {
    #[inline]
    fn is_system_program(&mut self, index: usize, program_id: &Pubkey) -> bool {
        *self
            .flags
            .get_mut(index)
            .expect("program id index is sanitized")
            .get_or_insert_with(|| program_id == &system_program::ID)
    }
}

#[derive(Debug, PartialEq)]
enum SystemProgramAccountAllocation {
    None,
    Some(u64),
    Failed,
}

fn parse_system_instruction_allocation(data: &[u8]) -> SystemProgramAccountAllocation {
    let Ok(instruction) = limited_deserialize(data) else {
        return SystemProgramAccountAllocation::Failed;
    };

    match instruction {
        SystemInstruction::CreateAccount { space, .. }
        | SystemInstruction::CreateAccountWithSeed { space, .. }
        | SystemInstruction::Allocate { space }
        | SystemInstruction::AllocateWithSeed { space, .. } => {
            if space > MAX_PERMITTED_DATA_LENGTH {
                SystemProgramAccountAllocation::Failed
            } else {
                SystemProgramAccountAllocation::Some(space)
            }
        }
        _ => SystemProgramAccountAllocation::None,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sdk::{instruction::Instruction, system_instruction},
    };

    #[test]
    fn test_transfer() {
        let instruction =
            system_instruction::transfer(&Pubkey::new_unique(), &Pubkey::new_unique(), 1);
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::None
        );
    }

    #[test]
    fn test_create_account() {
        let mut instruction = system_instruction::create_account(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            1,
            57,
            &Pubkey::new_unique(),
        );
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Some(57)
        );
        instruction.data.push(0); // trailing byte is okay
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Some(57)
        );
        // remaining data must be valid even if we have enough to read `space`
        instruction.data.truncate(instruction.data.len() - 2);
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Failed
        );
    }

    #[test]
    fn test_create_account_with_seed() {
        let mut instruction = system_instruction::create_account_with_seed(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            "seed",
            1,
            57,
            &Pubkey::new_unique(),
        );
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Some(57)
        );
        instruction.data.push(0); // trailing byte is okay
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Some(57)
        );
        // remaining data must be valid even if we have enough to read `space`
        instruction.data.truncate(instruction.data.len() - 2);
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Failed
        );
    }

    #[test]
    fn test_allocate() {
        let mut instruction = system_instruction::allocate(&Pubkey::new_unique(), 57);
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Some(57)
        );
        instruction.data.push(0); // trailing byte is okay
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Some(57)
        );
        // remaining data must be valid even if we have enough to read `space`
        instruction.data.truncate(instruction.data.len() - 2);
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Failed
        );
    }

    #[test]
    fn test_allocate_with_seed() {
        let mut instruction = system_instruction::allocate_with_seed(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            "seed",
            57,
            &Pubkey::new_unique(),
        );
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Some(57)
        );
        instruction.data.push(0); // trailing byte is okay
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Some(57)
        );
        // remaining data must be valid even if we have enough to read `space`
        instruction.data.truncate(instruction.data.len() - 2);
        assert_eq!(
            parse_system_instruction_allocation(&instruction.data),
            SystemProgramAccountAllocation::Failed
        );
    }

    #[test]
    fn test_multiple_allocations() {
        fn allocate_space(space: u64) -> Instruction {
            system_instruction::allocate(&Pubkey::new_unique(), space)
        }
        fn map_instructions(
            instructions: &[Instruction],
        ) -> impl Iterator<Item = (&Pubkey, SVMInstruction)> + '_ {
            instructions.iter().map(|i| {
                (
                    &system_program::ID,
                    SVMInstruction {
                        program_id_index: 0,
                        accounts: &[], // we don't need to copy this
                        data: &i.data,
                    },
                )
            })
        }

        // Valid allocations - sum
        let size_1 = 100;
        let size_2 = 200;
        let instructions = [allocate_space(size_1), allocate_space(size_2)];

        let allocation_details =
            AllocationInstructionDetails::process_instructions(map_instructions(&instructions))
                .unwrap();
        assert_eq!(allocation_details.total_space, size_1 + size_2);

        // max permitted allocations
        let instructions = [
            allocate_space(MAX_PERMITTED_DATA_LENGTH),
            allocate_space(MAX_PERMITTED_DATA_LENGTH),
            allocate_space(100),
        ];
        let allocation_details =
            AllocationInstructionDetails::process_instructions(map_instructions(&instructions))
                .unwrap();
        assert_eq!(
            allocation_details.total_space,
            MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION as u64
        );

        // invalid allocations
        let instructions = [
            allocate_space(100),
            allocate_space(MAX_PERMITTED_DATA_LENGTH + 1),
            allocate_space(200),
        ];

        assert!(
            AllocationInstructionDetails::process_instructions(map_instructions(&instructions))
                .is_none()
        );

        // overflow
        let instructions = [allocate_space(100), allocate_space(u64::MAX)];
        assert!(
            AllocationInstructionDetails::process_instructions(map_instructions(&instructions))
                .is_none()
        );
    }
}
