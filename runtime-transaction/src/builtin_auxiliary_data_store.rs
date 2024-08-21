// static account keys has max
use {
    agave_transaction_view::static_account_keys_meta::MAX_STATIC_ACCOUNTS_PER_PACKET as FILTER_SIZE,
    solana_builtins_default_costs::{BUILTIN_INSTRUCTION_COSTS, MAYBE_BUILTIN_KEY},
    solana_sdk::pubkey::Pubkey,
};

pub(crate) struct BuiltinAuxiliaryDataStore {
    // Array of auxiliary data for all possible static and sanitized program_id_index,
    // Each possible value of data indicates:
    //   None - un-checked
    //   Some<None> - checked, not builtin
    //   Some<Some<(bool, u32)>> - checked, is builtin and (is-compute-budget, default-cost)
    auxiliary_data: [Option<Option<(bool, u32)>>; FILTER_SIZE as usize],
}

impl BuiltinAuxiliaryDataStore {
    pub(crate) fn new() -> Self {
        BuiltinAuxiliaryDataStore {
            auxiliary_data: [None; FILTER_SIZE as usize],
        }
    }

    #[inline]
    pub(crate) fn get_auxiliary_data(
        &mut self,
        index: usize,
        program_id: &Pubkey,
    ) -> Option<(bool, u32)> {
        *self
            .auxiliary_data
            .get_mut(index)
            .expect("program id index is sanitized")
            .get_or_insert_with(|| {
                if !MAYBE_BUILTIN_KEY[program_id.as_ref()[0] as usize] {
                    return None;
                }

                BUILTIN_INSTRUCTION_COSTS.get(program_id).map(|cost| {
                    (
                        solana_sdk::compute_budget::check_id(program_id),
                        *cost as u32,
                    )
                })
            })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const DUMMY_PROGRAM_ID: &str = "dummmy1111111111111111111111111111111111111";

    #[test]
    fn test_get_auxiliary_data() {
        let mut test_store = BuiltinAuxiliaryDataStore::new();
        let mut index = 9;

        // initial state is Unchecked (eg, None)
        assert!(test_store.auxiliary_data[index].is_none());

        // non builtin returns None
        assert!(test_store
            .get_auxiliary_data(index, &DUMMY_PROGRAM_ID.parse().unwrap())
            .is_none());
        // but its state is now checked (eg, Some(...))
        assert_eq!(test_store.auxiliary_data[index], Some(None));
        // lookup same `index` will return cached auxiliary data, will *not* lookup `program_id`
        // again
        assert!(test_store
            .get_auxiliary_data(index, &solana_sdk::loader_v4::id())
            .is_none());

        // builtin return default cost
        index += 1;
        assert_eq!(
            test_store.get_auxiliary_data(index, &solana_sdk::loader_v4::id()),
            Some((
                false,
                solana_loader_v4_program::DEFAULT_COMPUTE_UNITS as u32
            ))
        );

        // compute-budget return default cost, and true flag
        index += 1;
        assert_eq!(
            test_store.get_auxiliary_data(index, &solana_sdk::compute_budget::id()),
            Some((
                true,
                solana_compute_budget_program::DEFAULT_COMPUTE_UNITS as u32
            ))
        );
    }

    #[test]
    #[should_panic(expected = "program id index is sanitized")]
    fn test_get_auxiliary_data_out_of_bound_index() {
        let mut test_store = BuiltinAuxiliaryDataStore::new();
        assert!(test_store
            .get_auxiliary_data(FILTER_SIZE as usize + 1, &DUMMY_PROGRAM_ID.parse().unwrap())
            .is_none());
    }
}
