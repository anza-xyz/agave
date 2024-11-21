// static account keys has max
use {
    agave_transaction_view::static_account_keys_frame::MAX_STATIC_ACCOUNTS_PER_PACKET as FILTER_SIZE,
    solana_builtins_default_costs::{is_builtin_program, MAYBE_BUILTIN_KEY},
    solana_sdk::pubkey::Pubkey,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ProgramKind {
    NotBuiltin,
    Builtin { is_compute_budget: bool },
}

pub(crate) struct ComputeBudgetProgramIdFilter {
    // array of slots for all possible static and sanitized program_id_index,
    // each slot indicates if a program_id_index has not been checked (eg, None),
    // or already checked with result (eg, Some(ProgramKind)) that can be reused.
    program_kind: [Option<ProgramKind>; FILTER_SIZE as usize],
}

impl ComputeBudgetProgramIdFilter {
    pub(crate) fn new() -> Self {
        ComputeBudgetProgramIdFilter {
            program_kind: [None; FILTER_SIZE as usize],
        }
    }

    #[inline]
    pub(crate) fn is_compute_budget_program(&mut self, index: usize, program_id: &Pubkey) -> bool {
        // Access the program kind at the given index, panic if index is invalid.
        match self
            .program_kind
            .get(index)
            .expect("program id index is sanitized")
        {
            // If the program kind is already set, check if it matches the target ProgramKind.
            Some(program_kind) => {
                *program_kind
                    == ProgramKind::Builtin {
                        is_compute_budget: true,
                    }
            }
            // If the program kind is not set, calculate it, store it, and then check the condition.
            None => {
                let is_compute_budget = Self::check_compute_bugdet_program_id(program_id);

                if is_compute_budget {
                    self.program_kind[index] = Some(ProgramKind::Builtin {
                        is_compute_budget: true,
                    });
                }

                is_compute_budget
            }
        }
    }

    #[inline]
    pub(crate) fn get_program_kind(&mut self, index: usize, program_id: &Pubkey) -> ProgramKind {
        *self
            .program_kind
            .get_mut(index)
            .expect("program id index is sanitized")
            .get_or_insert_with(|| Self::check_program_kind(program_id))
    }

    #[inline]
    fn check_compute_bugdet_program_id(program_id: &Pubkey) -> bool {
        if !MAYBE_BUILTIN_KEY[program_id.as_ref()[0] as usize] {
            return false;
        }

        solana_sdk::compute_budget::check_id(program_id)
    }

    #[inline]
    fn check_program_kind(program_id: &Pubkey) -> ProgramKind {
        if !MAYBE_BUILTIN_KEY[program_id.as_ref()[0] as usize] {
            return ProgramKind::NotBuiltin;
        }

        if is_builtin_program(program_id) {
            ProgramKind::Builtin {
                is_compute_budget: solana_sdk::compute_budget::check_id(program_id),
            }
        } else {
            ProgramKind::NotBuiltin
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const DUMMY_PROGRAM_ID: &str = "dummmy1111111111111111111111111111111111111";

    #[test]
    fn get_program_kind() {
        let mut test_store = ComputeBudgetProgramIdFilter::new();
        let mut index = 9;

        // initial state is Unchecked
        assert!(test_store.program_kind[index].is_none());

        // non builtin returns None
        assert_eq!(
            test_store.get_program_kind(index, &DUMMY_PROGRAM_ID.parse().unwrap()),
            ProgramKind::NotBuiltin
        );
        // but its state is now checked (eg, Some(...))
        assert_eq!(
            test_store.program_kind[index],
            Some(ProgramKind::NotBuiltin)
        );
        // lookup same `index` will return cached data, will not lookup `program_id`
        // again
        assert_eq!(
            test_store.get_program_kind(index, &solana_sdk::loader_v4::id()),
            ProgramKind::NotBuiltin
        );

        // not-migrating builtin
        index += 1;
        assert_eq!(
            test_store.get_program_kind(index, &solana_sdk::loader_v4::id()),
            ProgramKind::Builtin {
                is_compute_budget: false
            }
        );

        // compute-budget
        index += 1;
        assert_eq!(
            test_store.get_program_kind(index, &solana_sdk::compute_budget::id()),
            ProgramKind::Builtin {
                is_compute_budget: true
            }
        );
    }

    #[test]
    #[should_panic(expected = "program id index is sanitized")]
    fn test_get_program_kind_out_of_bound_index() {
        let mut test_store = ComputeBudgetProgramIdFilter::new();
        assert_eq!(
            test_store
                .get_program_kind(FILTER_SIZE as usize + 1, &DUMMY_PROGRAM_ID.parse().unwrap(),),
            ProgramKind::NotBuiltin
        );
    }
}
