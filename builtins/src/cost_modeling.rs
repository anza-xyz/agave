//! Configurations for handling cost modeling of builtin programs.

use solana_pubkey::Pubkey;

/// Configuration for cost modeling of a builtin program.
#[derive(Debug)]
pub struct CostModelingConfig {
    /// The default cost of the builtin program.
    /// If None, this builtin program is not cost modeled.
    pub default_cost: Option<u64>,
    /// Configuration for updating the cost of the builtin program.
    /// If None, the cost is never updated.
    pub new_cost_config: Option<NewCostModelingConfig>,
}

/// Configuration for updating the cost of a builtin program.
#[derive(Debug)]
pub struct NewCostModelingConfig {
    /// The new cost of the builtin program.
    pub new_cost: u64,
    /// The feature gate to trigger the new cost.
    pub feature_id: Pubkey,
}
