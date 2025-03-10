//! defines block cost related limits
//!

// Cluster data, method of collecting at https://github.com/solana-labs/solana/issues/19627
// Dashboard: https://metrics.solana.com/d/monitor-edge/cluster-telemetry?orgId=1

/// Cluster averaged compute unit to micro-sec conversion rate
pub const COMPUTE_UNIT_TO_US_RATIO: u64 = 30;
/// Number of compute units for one signature verification.
pub const SIGNATURE_COST: u64 = COMPUTE_UNIT_TO_US_RATIO * 24;
/// Number of compute units for one secp256k1 signature verification.
pub const SECP256K1_VERIFY_COST: u64 = COMPUTE_UNIT_TO_US_RATIO * 223;
/// Number of compute units for one ed25519 signature verification.
pub const ED25519_VERIFY_COST: u64 = COMPUTE_UNIT_TO_US_RATIO * 76;
/// Number of compute units for one ed25519 strict signature verification.
pub const ED25519_VERIFY_STRICT_COST: u64 = COMPUTE_UNIT_TO_US_RATIO * 80;
/// Number of compute units for one secp256r1 signature verification.
pub const SECP256R1_VERIFY_COST: u64 = COMPUTE_UNIT_TO_US_RATIO * 160;
/// Number of compute units for one write lock
pub const WRITE_LOCK_UNITS: u64 = COMPUTE_UNIT_TO_US_RATIO * 10;
/// Number of data bytes per compute units
pub const INSTRUCTION_DATA_BYTES_COST: u64 = 140 /*bytes per us*/ / COMPUTE_UNIT_TO_US_RATIO;

/// Number of compute units that a block is allowed. A block's compute units are
/// accumulated by Transactions added to it; A transaction's compute units are
/// calculated by cost_model, based on transaction's signatures, write locks,
/// data size and built-in and SBF instructions.
pub const MAX_BLOCK_UNITS: u64 = 48_000_000;
pub const MAX_BLOCK_UNITS_SIMD_0207: u64 = 50_000_000;
pub const MAX_BLOCK_UNITS_SIMD_0256: u64 = 60_000_000;

/// Number of compute units that a writable account in a block is allowed. The
/// limit is to prevent too many transactions write to same account, therefore
/// reduce block's parallelism.
pub const MAX_WRITABLE_ACCOUNT_UNITS: u64 = 12_000_000;

/// Number of compute units that a block can have for vote transactions,
/// set to less than MAX_BLOCK_UNITS to leave room for non-vote transactions
pub const MAX_VOTE_UNITS: u64 = 36_000_000;

/// The maximum allowed size, in bytes, that accounts data can grow, per block.
/// This can also be thought of as the maximum size of new allocations per block.
pub const MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA: u64 = 100_000_000;

/// Return the block limits that will be used upon activation of SIMD-0207.
/// Returns as
/// (account_limit, block_limit, vote_limit)
// ^ Above order is used to be consistent with the order of
//   `CostTracker::set_limits`.
pub const fn simd_0207_block_limits() -> (u64, u64, u64) {
    (
        MAX_WRITABLE_ACCOUNT_UNITS,
        MAX_BLOCK_UNITS_SIMD_0207,
        MAX_VOTE_UNITS,
    )
}

/// Return the block limits that will be used upon activation of SIMD-0256.
/// Returns as
/// (account_limit, block_limit, vote_limit)
// ^ Above order is used to be consistent with the order of
//   `CostTracker::set_limits`.
pub const fn simd_0256_block_limits() -> (u64, u64, u64) {
    (
        MAX_WRITABLE_ACCOUNT_UNITS,
        MAX_BLOCK_UNITS_SIMD_0256,
        MAX_VOTE_UNITS,
    )
}
