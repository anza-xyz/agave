//! Configurations for handling cost modeling of builtin programs.

/// Configuration for cost modeling of a builtin program.
#[derive(Debug)]
pub enum CostModelingConfig {
    /// The builtin program is cost modeled.
    CostModeled {
        /// The default cost of the builtin program.
        default_cost: u64,
    },
    /// The builtin program is not cost modeled.
    NotCostModeled,
}
