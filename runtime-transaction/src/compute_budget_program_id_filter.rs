use {solana_builtins_default_costs::MAYBE_BUILTIN_KEY, solana_sdk::pubkey::Pubkey};

pub(crate) struct ComputeBudgetProgramIdFilter {
    // array of 256 slots for all possible program_id_index (as u8),
    // each slot indicates if a program_id_index has not been checked (eg, None),
    // or already checked with result (eg, Some(result)) that can be reused.
    flags: [Option<bool>; 256],
}

impl ComputeBudgetProgramIdFilter {
    pub(crate) fn new() -> Self {
        ComputeBudgetProgramIdFilter { flags: [None; 256] }
    }

    #[inline]
    pub(crate) fn is_compute_budget_program(&mut self, index: usize, program_id: &Pubkey) -> bool {
        *self.flags[index].get_or_insert_with(|| Self::check_program_id(program_id))
    }

    #[inline]
    fn check_program_id(program_id: &Pubkey) -> bool {
        if !MAYBE_BUILTIN_KEY[program_id.as_ref()[0] as usize] {
            return false;
        }
        solana_sdk::compute_budget::check_id(program_id)
    }
}
