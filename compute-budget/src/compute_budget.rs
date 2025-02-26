use crate::compute_budget_limits::ComputeBudgetLimits;
pub use solana_program_runtime::execution_budget::SVMTransactionExecutionBudget as ComputeBudget;

impl From<ComputeBudgetLimits> for ComputeBudget {
    fn from(compute_budget_limits: ComputeBudgetLimits) -> Self {
        ComputeBudget {
            compute_unit_limit: u64::from(compute_budget_limits.compute_unit_limit),
            heap_size: compute_budget_limits.updated_heap_bytes,
            ..ComputeBudget::default()
        }
    }
}
