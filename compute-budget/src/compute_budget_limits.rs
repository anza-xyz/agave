#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
pub use solana_program_runtime::execution_budget::{
    DEFAULT_HEAP_COST, DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
    MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT, MAX_COMPUTE_UNIT_LIMIT, MAX_HEAP_FRAME_BYTES,
    MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES, MIN_HEAP_FRAME_BYTES,
};
use {
    solana_fee_structure::{FeeBudgetLimits, FeeDetails},
    solana_program_runtime::execution_budget::{
        SVMTransactionExecutionAndFeeBudgetLimits, SVMTransactionExecutionBudget,
    },
    std::num::NonZeroU32,
};

type MicroLamports = u128;

/// There are 10^6 micro-lamports in one lamport
const MICRO_LAMPORTS_PER_LAMPORT: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComputeBudgetLimits {
    pub updated_heap_bytes: u32,
    pub compute_unit_limit: u32,
    pub compute_unit_price: u64,
    pub loaded_accounts_bytes: NonZeroU32,
}

impl Default for ComputeBudgetLimits {
    fn default() -> Self {
        ComputeBudgetLimits {
            updated_heap_bytes: MIN_HEAP_FRAME_BYTES,
            compute_unit_limit: MAX_COMPUTE_UNIT_LIMIT,
            compute_unit_price: 0,
            loaded_accounts_bytes: MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
        }
    }
}

impl ComputeBudgetLimits {
    pub fn get_compute_budget_and_limits(
        &self,
        loaded_accounts_data_size_limit: NonZeroU32,
        fee_details: FeeDetails,
    ) -> SVMTransactionExecutionAndFeeBudgetLimits {
        SVMTransactionExecutionAndFeeBudgetLimits {
            budget: SVMTransactionExecutionBudget {
                compute_unit_limit: u64::from(self.compute_unit_limit),
                heap_size: self.updated_heap_bytes,
                ..SVMTransactionExecutionBudget::default()
            },
            loaded_accounts_data_size_limit,
            fee_details,
        }
    }

    pub fn get_prioritization_fee(&self) -> u64 {
        get_prioritization_fee(self.compute_unit_price, u64::from(self.compute_unit_limit))
    }
}

fn get_prioritization_fee(compute_unit_price: u64, compute_unit_limit: u64) -> u64 {
    let micro_lamport_fee: MicroLamports =
        (compute_unit_price as u128).saturating_mul(compute_unit_limit as u128);
    micro_lamport_fee
        .saturating_add(MICRO_LAMPORTS_PER_LAMPORT.saturating_sub(1) as u128)
        .checked_div(MICRO_LAMPORTS_PER_LAMPORT as u128)
        .and_then(|fee| u64::try_from(fee).ok())
        .unwrap_or(u64::MAX)
}

impl From<ComputeBudgetLimits> for FeeBudgetLimits {
    fn from(val: ComputeBudgetLimits) -> Self {
        let prioritization_fee =
            get_prioritization_fee(val.compute_unit_price, u64::from(val.compute_unit_limit));

        FeeBudgetLimits {
            loaded_accounts_data_size_limit: val.loaded_accounts_bytes,
            heap_cost: DEFAULT_HEAP_COST,
            compute_unit_limit: u64::from(val.compute_unit_limit),
            prioritization_fee,
        }
    }
}

impl From<&ComputeBudgetLimits> for FeeBudgetLimits {
    fn from(val: &ComputeBudgetLimits) -> Self {
        let prioritization_fee =
            get_prioritization_fee(val.compute_unit_price, u64::from(val.compute_unit_limit));

        FeeBudgetLimits {
            loaded_accounts_data_size_limit: val.loaded_accounts_bytes,
            heap_cost: DEFAULT_HEAP_COST,
            compute_unit_limit: u64::from(val.compute_unit_limit),
            prioritization_fee,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_new_with_no_fee() {
        for compute_units in [0, 1, MICRO_LAMPORTS_PER_LAMPORT, u64::MAX] {
            assert_eq!(get_prioritization_fee(0, compute_units), 0);
        }
    }

    #[test]
    fn test_new_with_compute_unit_price() {
        assert_eq!(
            get_prioritization_fee(MICRO_LAMPORTS_PER_LAMPORT - 1, 1),
            1,
            "should round up (<1.0) lamport fee to 1 lamport"
        );

        assert_eq!(get_prioritization_fee(MICRO_LAMPORTS_PER_LAMPORT, 1), 1);

        assert_eq!(
            get_prioritization_fee(MICRO_LAMPORTS_PER_LAMPORT + 1, 1),
            2,
            "should round up (>1.0) lamport fee to 2 lamports"
        );

        assert_eq!(get_prioritization_fee(200, 100_000), 20);

        assert_eq!(
            get_prioritization_fee(MICRO_LAMPORTS_PER_LAMPORT, u64::MAX),
            u64::MAX
        );

        assert_eq!(get_prioritization_fee(u64::MAX, u64::MAX), u64::MAX);
    }
}
