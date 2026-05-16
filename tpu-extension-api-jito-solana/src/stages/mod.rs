mod block_engine;
mod bundle;
mod sigverify;

use solana_pubkey::Pubkey;

pub use block_engine::{BlockEngineConfig, BlockEngineStage};
pub use bundle::BundleStage;
pub use sigverify::BundleSigverifyStage;

/// Reference-only inter-stage bundle payload.
///
/// This type is the channel message between all three pipeline stages in this
/// reference implementation. Production Jito-Solana derives an equivalent type
/// from its block-engine protobuf definition.
///
/// In a production pipeline the pre- and post-sigverify types would differ:
/// `BlockEngineStage` produces raw serialized packets; `BundleSigverifyStage`
/// would produce `SanitizedTransaction` objects. The reference collapses both
/// into a single type to keep the stub minimal.
///
/// `write_locks` lists the writable accounts declared by the bundle's
/// transactions. `BundleStage` uses this list to acquire [`BundleExternalLocks`]
/// before executing and to signal [`BundleSchedulerGate`] so the packet
/// scheduler pauses for the duration. In production these are extracted from
/// the transactions' writable account keys during sigverify.
///
/// [`BundleExternalLocks`]: crate::hooks::BundleExternalLocks
/// [`BundleSchedulerGate`]: crate::hooks::BundleSchedulerGate
pub(super) struct PacketBundle {
    /// Serialized transaction bytes (pre-sigverify raw form in this reference).
    pub packets: Vec<Vec<u8>>,
    /// Writable accounts locked for the duration of bundle execution.
    pub write_locks: Vec<Pubkey>,
}
