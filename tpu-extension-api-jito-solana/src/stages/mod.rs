mod block_engine;
mod bundle;
mod sigverify;

use solana_pubkey::Pubkey;

pub use block_engine::{BlockEngineConfig, BlockEngineStage};
pub use bundle::BundleStage;
pub use sigverify::BundleSigverifyStage;

/// Inter-stage bundle payload (reference stand-in for the production gRPC bundle type).
///
/// Production Jito-Solana uses separate pre- and post-sigverify types; the
/// reference collapses both into one to keep the stub minimal.
pub(super) struct PacketBundle {
    /// Serialized transaction bytes.
    pub packets: Vec<Vec<u8>>,
    /// Writable accounts locked for the duration of bundle execution.
    pub write_locks: Vec<Pubkey>,
}
