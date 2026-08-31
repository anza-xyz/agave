/// BG Chain protocol configuration.
///
/// This crate contains protocol-level constants only.
/// Consensus-critical values should eventually be activated
/// through the BG feature-set mechanism rather than changed
/// silently at runtime.

/// BG network identifier.
pub const BG_NETWORK_ID: &str = "bg-devnet-1";

/// Native token symbol.
pub const BG_TOKEN_SYMBOL: &str = "BG";

/// Native token decimals.
pub const BG_TOKEN_DECIMALS: u8 = 9;

/// Maximum transactions permitted in a block.
pub const BG_MAX_TRANSACTIONS_PER_BLOCK: usize = 50_000;

/// Minimum normal transaction fee in USD-equivalent target terms.
///
/// 1/256 of one cent = $0.0000390625.
pub const BG_MIN_FEE_USD: f64 = 0.0000390625;

/// Maximum normal transaction fee in USD-equivalent target terms.
///
/// 1/44 of one cent = $0.0002272727...
pub const BG_MAX_FEE_USD: f64 = 1.0 / 4400.0;

/// Maximum optional priority fee target.
pub const BG_MAX_PRIORITY_FEE_USD: f64 = 0.25;

/// Target light-load confirmation time.
pub const BG_LIGHT_LOAD_TARGET_MS: u64 = 2_000;

/// Target normal-load confirmation time.
pub const BG_NORMAL_LOAD_TARGET_MS: u64 = 5_000;

/// Maximum normal-load confirmation target.
pub const BG_HEAVY_LOAD_TARGET_MS: u64 = 10_000;

/// Target cross-chain gas budget.
pub const BG_CROSS_CHAIN_GAS_MIN_USD: f64 = 0.10;

/// Maximum cross-chain gas budget target.
pub const BG_CROSS_CHAIN_GAS_MAX_USD: f64 = 0.25;

/// Transactions at or below this serialized size use the normal
/// double-hash path.
///
/// This is deliberately a protocol placeholder for Devnet and
/// should be benchmarked before becoming consensus-critical.
pub const BG_LARGE_TRANSACTION_THRESHOLD_BYTES: usize = 1_024;

/// Number of hash rounds for normal transactions.
pub const BG_NORMAL_HASH_ROUNDS: u8 = 2;

/// Number of hash rounds for large transactions.
pub const BG_LARGE_HASH_ROUNDS: u8 = 4;
