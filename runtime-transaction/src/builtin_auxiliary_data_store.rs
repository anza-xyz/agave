// static account keys has max
use {
    agave_transaction_view::static_account_keys_meta::MAX_STATIC_ACCOUNTS_PER_PACKET as FILTER_SIZE,
    solana_builtins_default_costs::{BUILTIN_INSTRUCTION_COSTS, MAYBE_BUILTIN_KEY},
    solana_sdk::pubkey::Pubkey,
};

#[derive(Default, PartialEq)]
enum BuiltinCheckStatus {
    #[default]
    Unchecked,
    NotBuiltin,
    Builtin {
        is_compute_budget: bool,
        default_cost: u32,
    },
}

pub(crate) struct BuiltinAuxiliaryDataStore {
    auxiliary_data: [BuiltinCheckStatus; FILTER_SIZE as usize],
}

impl BuiltinAuxiliaryDataStore {
    pub(crate) fn new() -> Self {
        BuiltinAuxiliaryDataStore {
            auxiliary_data: core::array::from_fn(|_| BuiltinCheckStatus::default()),
        }
    }

    #[inline]
    pub(crate) fn get_auxiliary_data(
        &mut self,
        index: usize,
        program_id: &Pubkey,
    ) -> Option<(bool, u32)> {
        let stat = self
            .auxiliary_data
            .get_mut(index)
            .expect("program id index is sanitized");
        if stat == &BuiltinCheckStatus::Unchecked {
            *stat = Self::check_status(program_id)
        }

        match stat {
            BuiltinCheckStatus::NotBuiltin => None,
            BuiltinCheckStatus::Builtin {
                is_compute_budget,
                default_cost,
            } => Some((*is_compute_budget, *default_cost)),
            _ => unreachable!("already checked"),
        }
    }

    #[inline]
    fn check_status(program_id: &Pubkey) -> BuiltinCheckStatus {
        if !MAYBE_BUILTIN_KEY[program_id.as_ref()[0] as usize] {
            return BuiltinCheckStatus::NotBuiltin;
        }

        BUILTIN_INSTRUCTION_COSTS
            .get(program_id)
            .map_or(BuiltinCheckStatus::NotBuiltin, |cost| {
                BuiltinCheckStatus::Builtin {
                    is_compute_budget: solana_sdk::compute_budget::check_id(program_id),
                    default_cost: *cost as u32,
                }
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

        // initial state is Unchecked
        assert!(test_store.auxiliary_data[index] == BuiltinCheckStatus::Unchecked);

        // non builtin returns None
        assert!(test_store
            .get_auxiliary_data(index, &DUMMY_PROGRAM_ID.parse().unwrap())
            .is_none());
        // but its state is now checked (eg, Some(...))
        assert!(test_store.auxiliary_data[index] == BuiltinCheckStatus::NotBuiltin);
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
